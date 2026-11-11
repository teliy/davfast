//! Parallel transfer pipeline: recursive upload / download / mirror sync for
//! trees with thousands of small files.
//!
//! Key ideas:
//! - every operation is bounded by a concurrency limit (`buffer_unordered`);
//! - one listing request per directory (not per file) — a whole tree is
//!   compared in a handful of round-trips;
//! - unchanged files are skipped using server tokens (ETag / mail.ru hash)
//!   remembered in a small local cache file (`.davfast-cache.json`).

use anyhow::{Context, Result};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;

use crate::dav::{DavClient, DavItem};
use crate::mailru::MailRuClient;
use crate::util::normalize_remote;

pub const CACHE_FILE: &str = ".davfast-cache.json";

// ---------------------------------------------------------------- backend ---

/// Storage backend for the pipeline: a generic WebDAV server or the
/// cloud.mail.ru web API.
#[derive(Clone)]
pub enum Backend {
    Dav(DavClient),
    MailRu(Arc<MailRuClient>),
}

impl Backend {
    /// Children of a directory (self included for DAV — filtered by caller).
    pub async fn list_dir(&self, path: &str) -> Result<Vec<DavItem>> {
        match self {
            Backend::Dav(c) => c.propfind(path, 1).await,
            Backend::MailRu(c) => {
                let items = c.list(path).await?;
                Ok(items.iter().map(MailRuClient::to_dav_item).collect())
            }
        }
    }

    /// Upload content to `path` (full remote path incl. file name).
    /// Returns a sync token (mail.ru hash) when the backend provides one.
    pub async fn put_bytes(&self, path: &str, data: Vec<u8>, speedup: bool) -> Result<Option<String>> {
        match self {
            Backend::Dav(c) => {
                c.put_bytes(path, data).await?;
                Ok(None)
            }
            Backend::MailRu(c) => c.put_file(path, data, speedup).await.map(Some),
        }
    }

    /// Open a download stream for a remote file.
    pub async fn get_stream(&self, path: &str) -> Result<reqwest::Response> {
        match self {
            Backend::Dav(c) => c.get_stream(path).await,
            Backend::MailRu(c) => c.download(path).await,
        }
    }

    /// Delete a file or folder tree.
    pub async fn delete(&self, path: &str) -> Result<()> {
        match self {
            Backend::Dav(c) => c.delete(path).await,
            Backend::MailRu(c) => c.remove(path).await,
        }
    }
}

// ------------------------------------------------------------------ options --

#[derive(Debug, Clone)]
pub struct TransferOpts {
    pub concurrency: usize,
    pub skip_existing: bool,
    pub delete_extras: bool,
    pub dry_run: bool,
    pub progress: bool,
    /// try "add by hash" before transferring bytes (mail.ru dedup)
    pub speedup_hash: bool,
    /// keep `.davfast-cache.json` next to the local root for precise sync
    pub use_cache: bool,
}

impl Default for TransferOpts {
    fn default() -> Self {
        Self {
            concurrency: 16,
            skip_existing: false,
            delete_extras: false,
            dry_run: false,
            progress: false,
            speedup_hash: true,
            use_cache: false,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub uploaded: u64,
    pub downloaded: u64,
    pub skipped: u64,
    pub deleted: u64,
    pub failed: u64,
    pub bytes: u64,
    pub elapsed_secs: f64,
}

impl Stats {
    #[allow(dead_code)]
    pub fn summary(&self, verb: &str) -> String {
        format!(
            "{}: {} file(s), {}, skipped {}, failed {} in {:.1}s",
            verb,
            self.uploaded + self.downloaded,
            human_bytes(self.bytes),
            self.skipped,
            self.failed,
            self.elapsed_secs
        )
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", n, UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

// ------------------------------------------------------------------- cache --

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
struct CacheEntry {
    token: String,
    size: u64,
    mtime: i64,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Cache {
    #[serde(default)]
    entries: HashMap<String, CacheEntry>,
}

impl Cache {
    fn load(local_root: &Path) -> Self {
        let p = local_root.join(CACHE_FILE);
        match std::fs::read(&p) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }
    fn save(&self, local_root: &Path) {
        let p = local_root.join(CACHE_FILE);
        let tmp = local_root.join(format!("{}.tmp", CACHE_FILE));
        if let Ok(data) = serde_json::to_vec_pretty(self) {
            if std::fs::write(&tmp, &data).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
    fn prune_missing(&mut self, keep: &HashSet<String>) {
        self.entries.retain(|k, _| keep.contains(k));
    }
}

fn local_mtime(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ------------------------------------------------------------- local walk ---

pub struct LocalTree {
    pub dirs: Vec<String>,
    pub files: BTreeMap<String, u64>,
}

/// Recursively collect a local tree. Symlinks are skipped (dangling / loops).
pub async fn walk_local(root: &Path) -> Result<LocalTree> {
    let mut tree = LocalTree {
        dirs: Vec::new(),
        files: BTreeMap::new(),
    };
    walk_dir(root, "", &mut tree).await?;
    tree.dirs.sort_by_key(|d| d.matches('/').count());
    Ok(tree)
}

fn walk_dir<'a>(
    dir: &'a Path,
    rel: &'a str,
    tree: &'a mut LocalTree,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let mut rd = tokio::fs::read_dir(dir).await.with_context(|| format!("read_dir {}", dir.display()))?;
        while let Some(entry) = rd.next_entry().await? {
            let ft = match entry.file_type().await {
                Ok(t) => t,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_string();
            if ft.is_symlink() {
                tracing::debug!("skip symlink {}", name);
                continue;
            }
            let child_rel = if rel.is_empty() { name.clone() } else { format!("{}/{}", rel, name) };
            if name == CACHE_FILE {
                continue;
            }
            let child_path = dir.join(&name);
            if ft.is_dir() {
                tree.dirs.push(child_rel.clone());
                walk_dir(&child_path, &child_rel, tree).await?;
            } else if ft.is_file() {
                let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
                tree.files.insert(child_rel, size);
            }
        }
        Ok(())
    })
}

// -------------------------------------------------------- recursive listing -

/// List a whole remote tree with one request per directory (parallel batches).
/// Keys are absolute remote paths (normalized, no trailing slash).
pub async fn list_recursive(backend: &Backend, root: &str, concurrency: usize) -> Result<HashMap<String, DavItem>> {
    let base = normalize_remote(root);
    let mut map: HashMap<String, DavItem> = HashMap::new();
    let mut queue = vec![base.clone()];
    while !queue.is_empty() {
        let batch = std::mem::take(&mut queue);
        let results: Vec<(String, Result<Vec<DavItem>>)> = futures::StreamExt::buffer_unordered(
            futures::stream::iter(batch).map(|d| {
                let b = backend.clone();
                async move { (d.clone(), b.list_dir(&d).await) }
            }),
            concurrency.max(1),
        )
        .collect()
        .await;
        for (dir, res) in results {
            let items = res.with_context(|| format!("listing {}", dir))?;
            let prefix = if dir == "/" { "/".to_string() } else { format!("{}/", dir) };
            for it in items {
                if it.path == dir || it.name.is_empty() {
                    // self entry
                    map.entry(it.path.clone()).or_insert(it);
                    continue;
                }
                if !it.path.starts_with(&prefix) {
                    continue;
                }
                if it.is_dir {
                    queue.push(it.path.clone());
                }
                map.insert(it.path.clone(), it);
            }
        }
    }
    Ok(map)
}

// ------------------------------------------------------------------ upload --

/// Upload a local directory tree to a remote directory (parallel).
pub async fn upload_tree(
    backend: &Backend,
    local_root: &Path,
    remote_root: &str,
    opts: &TransferOpts,
) -> Result<Stats> {
    let t0 = Instant::now();
    let base = normalize_remote(remote_root);
    let tree = walk_local(local_root).await?;
    let mut stats = Stats::default();

    if tree.files.is_empty() && tree.dirs.is_empty() {
        stats.elapsed_secs = t0.elapsed().as_secs_f64();
        return Ok(stats);
    }

    // one listing pass when we need to compare
    let remote_map = if opts.skip_existing || opts.delete_extras {
        Some(list_recursive(backend, &base, opts.concurrency).await?)
    } else {
        None
    };

    let mut cache = if opts.use_cache { Cache::load(local_root) } else { Cache::default() };

    // --- directories -------------------------------------------------------
    if !opts.dry_run {
        ensure_dirs(backend, &base, &tree.dirs, remote_map.as_ref()).await?;
    }

    // --- files -------------------------------------------------------------
    let mut jobs: Vec<(String, u64)> = Vec::new();
    let mut skipped_rel: Vec<String> = Vec::new();
    for (rel, size) in tree.files.iter() {
        let abs = format!("{}/{}", if base == "/" { "" } else { &base }, rel);
        let remote = remote_map.as_ref().and_then(|m| m.get(&abs));
        let cached = cache.entries.get(rel);
        if should_skip_upload(*size, local_mtime(&local_root.join(rel)), remote, cached) {
            stats.skipped += 1;
            skipped_rel.push(rel.clone());
            continue;
        }
        jobs.push((rel.clone(), *size));
    }
    cache.prune_missing(&{
        let mut s: HashSet<String> = tree.files.keys().cloned().collect();
        s.extend(skipped_rel.iter().cloned());
        s
    });

    let total_bytes: u64 = jobs.iter().map(|(_, s)| s).sum();
    let pb = make_progress(opts.progress, total_bytes, if opts.dry_run { "dry-run" } else { "upload" });

    if opts.dry_run {
        for (rel, size) in jobs.iter() {
            println!("  up  {:>9}  {}", human_bytes(*size), rel);
        }
        stats.elapsed_secs = t0.elapsed().as_secs_f64();
        pb.finish_and_clear();
        return Ok(stats);
    }

    let uploaded: Vec<String> = Vec::new();
    let uploaded = Arc::new(std::sync::Mutex::new(uploaded));
    let results: Vec<(String, std::result::Result<Option<String>, String>)> = futures::StreamExt::buffer_unordered(
        futures::stream::iter(jobs.clone()).map(|(rel, size)| {
            let b = backend.clone();
            let lr = local_root.to_path_buf();
            let base = base.clone();
            let pb = pb.clone();
            let up = uploaded.clone();
            async move {
                let r = upload_one(&b, &lr, &rel, &base, size, opts.speedup_hash).await;
                pb.inc(size);
                if r.is_ok() {
                    up.lock().unwrap().push(rel.clone());
                }
                (rel, r.map_err(|e| format!("{}", e)))
            }
        }),
        opts.concurrency,
    )
    .collect()
    .await;
    pb.finish_and_clear();

    let mut sync_tokens: HashMap<String, String> = HashMap::new();
    for (rel, r) in results {
        match r {
            Ok(Some(token)) => {
                stats.uploaded += 1;
                stats.bytes += tree.files.get(&rel).copied().unwrap_or(0);
                sync_tokens.insert(rel, token);
            }
            Ok(None) => {
                stats.uploaded += 1;
                stats.bytes += tree.files.get(&rel).copied().unwrap_or(0);
            }
            Err(e) => {
                stats.failed += 1;
                tracing::error!("upload {}: {}", rel, e);
            }
        }
    }

    // --- remember server tokens --------------------------------------------
    if opts.use_cache {
        // mail.ru returns a hash immediately; WebDAV servers need a listing pass
        let mut need_fetch: HashSet<String> = HashSet::new();
        for rel in uploaded.lock().unwrap().iter() {
            if sync_tokens.contains_key(rel) {
                cache.entries.insert(
                    rel.clone(),
                    CacheEntry {
                        token: sync_tokens[rel].clone(),
                        size: *tree.files.get(rel).unwrap_or(&0),
                        mtime: local_mtime(&local_root.join(rel)),
                    },
                );
            } else {
                let parent_abs = parent_of(rel)
                    .map(|p| {
                        if p == "/" {
                            base.clone()
                        } else {
                            normalize_remote(&format!("{}/{}", base, p))
                        }
                    })
                    .unwrap_or_else(|| base.clone());
                need_fetch.insert(parent_abs);
            }
        }
        fetch_etags(
            backend,
            local_root,
            &base,
            &need_fetch,
            &tree.files,
            &uploaded.lock().unwrap(),
            &mut cache,
        )
        .await;
        cache.save(local_root);
    }

    // --- delete extras -----------------------------------------------------
    if opts.delete_extras {
        if let Some(map) = remote_map.as_ref() {
            let prefix = if base == "/" { "/".to_string() } else { format!("{}/", base) };
            let mut remote_files: Vec<String> = Vec::new();
            let mut remote_dirs: Vec<String> = Vec::new();
            for (p, it) in map.iter() {
                if !p.starts_with(&prefix) || p.as_str() == base {
                    continue;
                }
                if it.is_dir {
                    remote_dirs.push(p.clone());
                } else {
                    remote_files.push(p.clone());
                }
            }
            let local_set: HashSet<&String> = tree.files.keys().collect();
            let mut to_delete: Vec<String> = Vec::new();
            for f in remote_files {
                let rel = rel_of(&base, &f).unwrap_or_default();
                if !local_set.contains(&rel) {
                    to_delete.push(f);
                }
            }
            to_delete.sort();
            let pb = make_progress(opts.progress, (to_delete.len() as u64) * 1000, "delete");
            for chunk in to_delete.chunks(opts.concurrency.max(1)) {
                let futs: Vec<_> = chunk
                    .iter()
                    .map(|f| {
                        let b = backend.clone();
                        let f = f.clone();
                        async move { b.delete(&f).await.map_err(|e| format!("{}: {}", f, e)) }
                    })
                    .collect();
                for (i, r) in futures::future::join_all(futs).await.into_iter().enumerate() {
                    pb.inc(1000);
                    match r {
                        Ok(()) => stats.deleted += 1,
                        Err(e) => {
                            stats.failed += 1;
                            tracing::error!("delete {}: {}", chunk[i], e);
                        }
                    }
                }
            }
            pb.finish_and_clear();
            // empty dirs bottom-up
            let mut dirs_to_del: Vec<String> = remote_dirs
                .into_iter()
                .filter(|d| {
                    let rel = rel_of(&base, d).unwrap_or_default();
                    !tree.dirs.iter().any(|ld| ld == &rel)
                })
                .collect();
            dirs_to_del.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
            for d in dirs_to_del {
                if backend.delete(&d).await.is_ok() {
                    stats.deleted += 1;
                }
            }
        }
    }

    stats.elapsed_secs = t0.elapsed().as_secs_f64();
    Ok(stats)
}

async fn upload_one(
    backend: &Backend,
    local_root: &Path,
    rel: &str,
    base: &str,
    _size: u64,
    speedup: bool,
) -> Result<Option<String>> {
    let path = local_root.join(rel);
    let data = tokio::fs::read(&path).await.with_context(|| format!("read {}", rel))?;
    let abs = format!("{}/{}", if base == "/" { "" } else { base.trim_matches('/') }, rel);
    let abs = normalize_remote(&abs);
    backend.put_bytes(&abs, data, speedup).await
}

fn should_skip_upload(
    local_size: u64,
    local_mtime: i64,
    remote: Option<&DavItem>,
    cached: Option<&CacheEntry>,
) -> bool {
    if let Some(r) = remote {
        // precise: cache remembers the server token (ETag / mail.ru hash)
        if let Some(c) = cached {
            if let Some(etag) = &r.etag {
                if etag == &c.token && c.mtime == local_mtime && c.size == local_size {
                    return true;
                }
            }
        }
        // heuristic for plain --skip-existing: same size is good enough
        if r.size == local_size {
            return true;
        }
    }
    false
}

async fn ensure_dirs(
    backend: &Backend,
    base: &str,
    dirs: &[String],
    remote_map: Option<&HashMap<String, DavItem>>,
) -> Result<()> {
    let mk = |rel: &str| -> String {
        normalize_remote(&format!("{}/{}", base.trim_end_matches('/'), rel))
    };
    let mut to_create: Vec<String> = dirs.iter().map(|d| mk(d)).collect();
    if base != "/" && remote_map.map(|m| !m.contains_key(base)).unwrap_or(true) {
        to_create.insert(0, base.to_string());
    }
    // group by depth, create parents before children; within a level parallel
    let mut levels: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for p in to_create {
        levels.entry(p.matches('/').count()).or_default().push(p);
    }
    for (_, level) in levels {
        let futs: Vec<_> = level
            .iter()
            .map(|p| {
                let b = backend.clone();
                let p = p.clone();
                let known = remote_map.map(|m| m.contains_key(&p)).unwrap_or(false);
                async move {
                    if known {
                        return Ok(());
                    }
                    match &b {
                        Backend::Dav(c) => c.mkcol(&p).await,
                        Backend::MailRu(c) => c.mkdir_all(&p).await,
                    }
                    .map_err(|e| format!("{}: {}", p, e))
                }
            })
            .collect();
        for r in futures::future::join_all(futs).await {
            if let Err(e) = r {
                tracing::warn!("mkdir: {}", e);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn fetch_etags(
    backend: &Backend,
    local_root: &Path,
    base: &str,
    dirs: &HashSet<String>,
    files: &BTreeMap<String, u64>,
    uploaded: &[String],
    cache: &mut Cache,
) {
    if dirs.is_empty() || uploaded.is_empty() {
        return;
    }
    let dirs: Vec<String> = dirs.iter().cloned().collect();
    let futs: Vec<_> = dirs
        .iter()
        .map(|d| {
            let b = backend.clone();
            let d = d.clone();
            async move { b.list_dir(&d).await }
        })
        .collect();
    let mut etag_map: HashMap<String, String> = HashMap::new();
    for res in futures::future::join_all(futs).await {
        if let Ok(items) = res {
            for it in items {
                if !it.is_dir {
                    if let Some(e) = it.etag {
                        etag_map.insert(it.path.clone(), e);
                    }
                }
            }
        }
    }
    for rel in uploaded {
        let abs = normalize_remote(&format!("{}/{}", base.trim_end_matches('/'), rel));
        if let Some(etag) = etag_map.get(&abs) {
            cache.entries.insert(
                rel.clone(),
                CacheEntry {
                    token: etag.clone(),
                    size: files.get(rel).copied().unwrap_or(0),
                    mtime: local_mtime(&local_root.join(rel)),
                },
            );
        }
    }
}

// ---------------------------------------------------------------- download --

/// Download a remote tree into a local directory (parallel).
pub async fn download_tree(
    backend: &Backend,
    remote_root: &str,
    local_root: &Path,
    opts: &TransferOpts,
) -> Result<Stats> {
    let t0 = Instant::now();
    let base = normalize_remote(remote_root);
    let map = list_recursive(backend, &base, opts.concurrency).await?;
    let mut stats = Stats::default();

    let mut cache = if opts.use_cache { Cache::load(local_root) } else { Cache::default() };

    // dirs first
    if !opts.dry_run {
        let mut dirs: Vec<String> = Vec::new();
        for (p, it) in map.iter() {
            if it.is_dir && p.as_str() != base {
                if let Some(rel) = rel_of(&base, p) {
                    dirs.push(rel);
                }
            }
        }
        dirs.sort_by_key(|d| d.matches('/').count());
        for d in dirs {
            tokio::fs::create_dir_all(local_root.join(d)).await?;
        }
    }

    let mut jobs: Vec<(String, u64)> = Vec::new();
    for (p, it) in map.iter() {
        if it.is_dir || p.as_str() == base {
            continue;
        }
        let rel = match rel_of(&base, p) {
            Some(r) => r,
            None => continue,
        };
        let local_path = local_root.join(&rel);
        if opts.skip_existing {
            let mtime_ok = cache
                .entries
                .get(&rel)
                .map(|c| {
                    let etag_ok = it.etag.as_ref().map(|e| e == &c.token).unwrap_or(false);
                    etag_ok && c.size == it.size
                })
                .unwrap_or(false);
            let size_ok = std::fs::metadata(&local_path).map(|m| m.len()).unwrap_or(u64::MAX) == it.size
                && !it.etag.is_some();
            if mtime_ok || size_ok {
                stats.skipped += 1;
                continue;
            }
        }
        jobs.push((rel, it.size));
    }

    let total_bytes: u64 = jobs.iter().map(|(_, s)| s).sum();
    let pb = make_progress(opts.progress, total_bytes, if opts.dry_run { "dry-run" } else { "download" });

    if opts.dry_run {
        for (rel, size) in jobs.iter() {
            println!("  down {:>9}  {}", human_bytes(*size), rel);
        }
        pb.finish_and_clear();
        stats.elapsed_secs = t0.elapsed().as_secs_f64();
        return Ok(stats);
    }

    let results: Vec<(String, u64, std::result::Result<Option<String>, String>)> =
        futures::StreamExt::buffer_unordered(
            futures::stream::iter(jobs.clone()).map(|(rel, size)| {
                let b = backend.clone();
                let lr = local_root.to_path_buf();
                let base = base.clone();
                let pb = pb.clone();
                async move {
                    let r = download_one(&b, &rel, &base, &lr).await;
                    pb.inc(size);
                    (rel, size, r.map_err(|e| format!("{}", e)))
                }
            }),
            opts.concurrency,
        )
        .collect()
        .await;

    let mut keep: HashSet<String> = HashSet::new();
    for (rel, size, r) in results {
        keep.insert(rel.clone());
        match r {
            Ok(Some(token)) => {
                stats.downloaded += 1;
                stats.bytes += size;
                cache.entries.insert(
                    rel.clone(),
                    CacheEntry {
                        token,
                        size,
                        mtime: local_mtime(&local_root.join(&rel)),
                    },
                );
            }
            Ok(None) => {
                stats.downloaded += 1;
                stats.bytes += size;
            }
            Err(e) => {
                stats.failed += 1;
                tracing::error!("download {}: {}", rel, e);
            }
        }
    }
    if opts.use_cache {
        cache.prune_missing(&keep);
        cache.save(local_root);
    }
    pb.finish_and_clear();
    stats.elapsed_secs = t0.elapsed().as_secs_f64();
    Ok(stats)
}

async fn download_one(
    backend: &Backend,
    rel: &str,
    base: &str,
    local_root: &Path,
) -> Result<Option<String>> {
    let abs = normalize_remote(&format!("{}/{}", base.trim_end_matches('/'), rel));
    let out = local_root.join(rel);
    if let Some(parent) = out.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let resp = backend.get_stream(&abs).await?;
    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let tmp = out.with_extension("davfast-part");
    let mut f = tokio::fs::File::create(&tmp).await?;
    let mut bytes: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        let chunk = chunk?;
        bytes += chunk.len() as u64;
        f.write_all(&chunk).await?;
    }
    f.flush().await?;
    drop(f);
    tokio::fs::rename(&tmp, &out).await?;
    tracing::debug!("downloaded {} ({} bytes)", rel, bytes);
    Ok(etag)
}

// ------------------------------------------------------------------- misc ---

fn rel_of(base: &str, abs: &str) -> Option<String> {
    let prefix = if base == "/" { "/" } else { base };
    let rel = abs.strip_prefix(prefix)?.trim_start_matches('/');
    if rel.is_empty() {
        None
    } else {
        Some(rel.to_string())
    }
}

fn parent_of(rel: &str) -> Option<String> {
    let p = rel.rsplit_once('/')?.0;
    if p.is_empty() {
        Some("/".to_string())
    } else {
        Some(p.to_string())
    }
}

fn make_progress(enabled: bool, total: u64, msg: &str) -> ProgressBar {
    if !enabled {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(total.max(1));
    if let Ok(style) = ProgressStyle::with_template(
        "{msg:>8} {bar:40.cyan/blue} {bytes}/{total_bytes} {percent:>3}% {eta}",
    ) {
        let _ = pb.set_style(style.progress_chars("=>-"));
    }
    pb.set_message(msg.to_string());
    pb
}
