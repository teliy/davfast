//! davfast CLI entry point.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use futures::StreamExt as _;
use percent_encoding::percent_decode_str;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncWriteExt;

use davfast::dav::DavClient;
use davfast::mailru::MailRuClient;
use davfast::pipeline::{
    download_tree, human_bytes, list_recursive, upload_tree, Backend, TransferOpts,
};
use davfast::server::{serve, BackendKind, ServeConfig};
use davfast::util::normalize_remote;

#[derive(Parser)]
#[command(
    name = "davfast",
    version,
    about = "Fast WebDAV client and cloud.mail.ru bridge — built for thousands of small files",
    long_about = "davfast uploads/downloads/mirrors directory trees over WebDAV or the cloud.mail.ru web API with a parallel pipeline, connection pooling and change detection (ETag/hash). It can also run as a local WebDAV bridge server so davfs2 or any WebDAV client gets web-interface speeds."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List a WebDAV directory
    Ls {
        /// WebDAV URL, e.g. https://example.com/dav/dir/ (creds in URL or env)
        url: String,
    },
    /// Create a remote directory with parents
    Mkdir {
        url: String,
    },
    /// Remove a remote file or directory
    Rm {
        url: String,
        #[arg(short, long)]
        recursive: bool,
    },
    /// Upload a file or a directory tree (parallel)
    Upload {
        /// local file or directory
        local: PathBuf,
        /// remote target: directory URL (trailing /) or full file URL
        url: String,
        #[arg(short, long, default_value_t = 16)]
        concurrency: usize,
        /// skip files that already exist remotely with the same size
        #[arg(long)]
        skip_existing: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Download a file or a directory tree (parallel)
    Download {
        url: String,
        /// local target: file path or directory
        local: PathBuf,
        #[arg(short, long, default_value_t = 16)]
        concurrency: usize,
        #[arg(long)]
        skip_existing: bool,
    },
    /// Mirror local -> remote incrementally (ETag/hash cache, --delete for extras)
    Sync {
        local: PathBuf,
        url: String,
        #[arg(short, long, default_value_t = 16)]
        concurrency: usize,
        /// delete remote files missing locally
        #[arg(long)]
        delete: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Run a local WebDAV bridge/proxy server
    Serve {
        /// "webdav" (proxy to --url) or "mailru" (cloud.mail.ru web API)
        #[arg(long)]
        backend: String,
        /// upstream WebDAV base URL for --backend webdav
        #[arg(long)]
        url: Option<String>,
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
        /// remote root exposed at / (default /)
        #[arg(long, default_value = "/")]
        root: String,
        /// optional Basic auth protecting this bridge
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        pass: Option<String>,
    },
    /// Operations via the cloud.mail.ru web API (like the web interface)
    Mailru {
        #[command(subcommand)]
        cmd: MailruCmd,
    },
}

#[derive(Subcommand)]
enum MailruCmd {
    /// List a cloud folder
    Ls { home: String },
    /// Upload a file or directory tree via the web API (parallel, hash dedup)
    Upload {
        local: PathBuf,
        /// cloud destination folder, e.g. /backups
        home: String,
        #[arg(short, long, default_value_t = 16)]
        concurrency: usize,
        #[arg(long)]
        skip_existing: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Download a file or directory tree via the web API
    Download {
        home: String,
        local: PathBuf,
        #[arg(short, long, default_value_t = 16)]
        concurrency: usize,
        #[arg(long)]
        skip_existing: bool,
    },
    /// Mirror local -> cloud incrementally (hash-based change detection)
    Sync {
        local: PathBuf,
        home: String,
        #[arg(short, long, default_value_t = 16)]
        concurrency: usize,
        #[arg(long)]
        delete: bool,
        #[arg(long)]
        dry_run: bool,
    },
}

fn resolve_dav(url: &str, concurrency: usize) -> Result<(DavClient, String)> {
    let mut u = url::Url::parse(url).context("bad URL")?;
    if u.scheme() != "http" && u.scheme() != "https" {
        bail!("URL must start with http:// or https://");
    }
    let user = if u.username().is_empty() {
        None
    } else {
        Some(percent_decode_str(u.username()).decode_utf8_lossy().to_string())
    };
    let pass = u.password().map(|p| percent_decode_str(p).decode_utf8_lossy().to_string());
    let _ = u.set_username("");
    let _ = u.set_password(None);
    // the URL path becomes the working path; the client base must be host-only
    let path = normalize_remote(u.path());
    let _ = u.set_path("/");
    let user = user.or_else(|| std::env::var("DAVFAST_USER").ok());
    let pass = pass.or_else(|| std::env::var("DAVFAST_PASS").ok());
    if user.is_some() && pass.is_none() {
        bail!("user without password: set DAVFAST_PASS or user:pass in URL");
    }
    let client = DavClient::new(u.as_str(), user.as_deref(), pass.as_deref(), concurrency)?;
    Ok((client, path))
}

fn print_stats(verb: &str, st: &davfast::pipeline::Stats) {
    println!(
        "{}: {} file(s), {}, skipped {}, deleted {}, failed {} in {:.1}s",
        verb,
        st.uploaded + st.downloaded,
        human_bytes(st.bytes),
        st.skipped,
        st.deleted,
        st.failed,
        st.elapsed_secs
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let cli = Cli::parse();
    // restore default SIGPIPE behaviour (Rust ignores it, which turns
    // `davfast ls ... | head` into a panic) — die silently like a unix tool
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let code = run(cli).await;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

async fn run(cli: Cli) -> i32 {
    match cli.cmd {
        Cmd::Ls { url } => match cmd_ls(&url).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {:#}", e);
                1
            }
        },
        Cmd::Mkdir { url } => match cmd_mkdir(&url).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {:#}", e);
                1
            }
        },
        Cmd::Rm { url, recursive } => match cmd_rm(&url, recursive).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {:#}", e);
                1
            }
        },
        Cmd::Upload { local, url, concurrency, skip_existing, dry_run } => {
            match cmd_upload(local, &url, concurrency, skip_existing, dry_run).await {
                Ok(0) => 0,
                Ok(code) => code,
                Err(e) => {
                    eprintln!("error: {:#}", e);
                    1
                }
            }
        }
        Cmd::Download { url, local, concurrency, skip_existing } => {
            match cmd_download(&url, local, concurrency, skip_existing).await {
                Ok(0) => 0,
                Ok(code) => code,
                Err(e) => {
                    eprintln!("error: {:#}", e);
                    1
                }
            }
        }
        Cmd::Sync { local, url, concurrency, delete, dry_run } => {
            match cmd_sync(local, &url, concurrency, delete, dry_run).await {
                Ok(0) => 0,
                Ok(code) => code,
                Err(e) => {
                    eprintln!("error: {:#}", e);
                    1
                }
            }
        }
        Cmd::Serve { backend, url, listen, root, user, pass } => {
            match cmd_serve(&backend, url, &listen, &root, user, pass).await {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("error: {:#}", e);
                    1
                }
            }
        }
        Cmd::Mailru { cmd } => match cmd_mailru(cmd).await {
            Ok(0) => 0,
            Ok(code) => code,
            Err(e) => {
                eprintln!("error: {:#}", e);
                1
            }
        },
    }
}

// ----------------------------------------------------------------- webdav ---

async fn cmd_ls(url: &str) -> Result<()> {
    let (dav, path) = resolve_dav(url, 8)?;
    let items = dav.propfind(&path, 1).await?;
    // for directories the server echoes the entry itself — drop it
    let items: Vec<_> = items.into_iter().filter(|i| !(i.is_dir && i.path == path)).collect();
    let mut dirs: Vec<_> = items.iter().filter(|i| i.is_dir).collect();
    let mut files: Vec<_> = items.iter().filter(|i| !i.is_dir).collect();
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));
    for it in dirs.iter().chain(files.iter()) {
        let size = if it.is_dir {
            "<dir>".to_string()
        } else {
            human_bytes(it.size)
        };
        println!("{:>9}  {}", size, it.name);
    }
    Ok(())
}

async fn cmd_mkdir(url: &str) -> Result<()> {
    let (dav, path) = resolve_dav(url, 8)?;
    // create the chain piece by piece
    let mut cur = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        cur.push('/');
        cur.push_str(seg);
        dav.mkcol(&cur).await?;
    }
    println!("created {}", path);
    Ok(())
}

async fn cmd_rm(url: &str, recursive: bool) -> Result<()> {
    let (dav, path) = resolve_dav(url, 8)?;
    if path == "/" {
        bail!("refusing to remove root");
    }
    let children = dav.propfind(&path, 1).await?;
    let has_children = children.iter().any(|c| c.path != path);
    if has_children && !recursive {
        bail!("{} is not empty; use --recursive", path);
    }
    if !has_children {
        dav.delete(&path).await?;
        println!("removed {}", path);
        return Ok(());
    }
    // recursive: delete files first, then deepest dirs first
    let map = list_recursive(&Backend::Dav(dav.clone()), &path, 16).await?;
    let mut files: Vec<String> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    for (p, it) in map.iter() {
        if p.as_str() == path {
            continue;
        }
        if it.is_dir {
            dirs.push(p.clone());
        } else {
            files.push(p.clone());
        }
    }
    for f in files {
        dav.delete(&f).await?;
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
    for d in dirs {
        dav.delete(&d).await?;
    }
    dav.delete(&path).await?;
    println!("removed {} (recursive)", path);
    Ok(())
}

async fn cmd_upload(local: PathBuf, url: &str, concurrency: usize, skip_existing: bool, dry_run: bool) -> Result<i32> {
    let (dav, path) = resolve_dav(url, concurrency)?;
    let backend = Backend::Dav(dav);
    let opts = TransferOpts {
        concurrency,
        skip_existing,
        dry_run,
        progress: true,
        ..Default::default()
    };
    let md = tokio::fs::metadata(&local).await?;
    let stats = if md.is_dir() {
        upload_tree(&backend, &local, &path, &opts).await?
    } else {
        let name = local
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .context("bad local file name")?;
        let target = if url.ends_with('/') {
            format!("{}/{}", path, name)
        } else {
            path.clone()
        };
        let t0 = Instant::now();
        let data = tokio::fs::read(&local).await?;
        if !opts.dry_run {
            backend.put_bytes(&target, data, false).await?;
        }
        davfast::pipeline::Stats {
            uploaded: 1,
            bytes: std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0),
            elapsed_secs: t0.elapsed().as_secs_f64(),
            ..Default::default()
        }
    };
    print_stats("upload", &stats);
    Ok(if stats.failed > 0 { 1 } else { 0 })
}

async fn cmd_download(url: &str, local: PathBuf, concurrency: usize, skip_existing: bool) -> Result<i32> {
    let (dav, path) = resolve_dav(url, concurrency)?;
    let backend = Backend::Dav(dav);
    let opts = TransferOpts {
        concurrency,
        skip_existing,
        progress: true,
        ..Default::default()
    };
    let items = backend.list_dir(&path).await?;
    let self_entry = items.iter().find(|i| i.path == path);
    let stats = match self_entry {
        Some(e) if !e.is_dir => {
            let name = if path == "/" {
                "index".to_string()
            } else {
                davfast::util::name_from_path(&path)
            };
            let out = if local.is_dir() || local.to_string_lossy().ends_with('/') {
                local.join(name)
            } else {
                local.clone()
            };
            let t0 = Instant::now();
            if let Some(parent) = out.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let resp = backend.get_stream(&path).await?;
            let tmp = out.with_extension("davfast-part");
            let mut f = tokio::fs::File::create(&tmp).await?;
            let mut n: u64 = 0;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                n += chunk.len() as u64;
                f.write_all(&chunk).await?;
            }
            f.flush().await?;
            drop(f);
            tokio::fs::rename(&tmp, &out).await?;
            davfast::pipeline::Stats {
                downloaded: 1,
                bytes: n,
                elapsed_secs: t0.elapsed().as_secs_f64(),
                ..Default::default()
            }
        }
        _ => download_tree(&backend, &path, &local, &opts).await?,
    };
    print_stats("download", &stats);
    Ok(if stats.failed > 0 { 1 } else { 0 })
}

async fn cmd_sync(local: PathBuf, url: &str, concurrency: usize, delete: bool, dry_run: bool) -> Result<i32> {
    let (dav, path) = resolve_dav(url, concurrency)?;
    if !local.is_dir() {
        bail!("sync requires a local directory");
    }
    let backend = Backend::Dav(dav);
    let opts = TransferOpts {
        concurrency,
        skip_existing: true,
        delete_extras: delete,
        dry_run,
        progress: !dry_run,
        use_cache: true,
        ..Default::default()
    };
    let stats = upload_tree(&backend, &local, &path, &opts).await?;
    print_stats("sync", &stats);
    Ok(if stats.failed > 0 { 1 } else { 0 })
}

// ----------------------------------------------------------------- mailru ---

async fn cmd_mailru(cmd: MailruCmd) -> Result<i32> {
    let client = MailRuClient::from_env()?;
    client.login().await?;
    let arc = Arc::new(client);
    let backend = Backend::MailRu(arc);

    match cmd {
        MailruCmd::Ls { home } => {
            let items = backend.list_dir(&home).await?;
            let mut dirs: Vec<_> = items.iter().filter(|i| i.is_dir).collect();
            let mut files: Vec<_> = items.iter().filter(|i| !i.is_dir).collect();
            dirs.sort_by(|a, b| a.name.cmp(&b.name));
            files.sort_by(|a, b| a.name.cmp(&b.name));
            for it in dirs.iter().chain(files.iter()) {
                let size = if it.is_dir { "<dir>".to_string() } else { human_bytes(it.size) };
                println!("{:>9}  {}", size, it.name);
            }
            Ok(0)
        }
        MailruCmd::Upload { local, home, concurrency, skip_existing, dry_run } => {
            let md = tokio::fs::metadata(&local).await?;
            let opts = TransferOpts {
                concurrency,
                skip_existing,
                dry_run,
                progress: !dry_run,
                speedup_hash: true,
                ..Default::default()
            };
            let stats = if md.is_dir() {
                upload_tree(&backend, &local, &home, &opts).await?
            } else {
                let name = local
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .context("bad local file name")?;
                let target = if home.ends_with('/') {
                    format!("{}/{}", home, name)
                } else {
                    home.clone()
                };
                let t0 = Instant::now();
                let data = tokio::fs::read(&local).await?;
                if !dry_run {
                    backend.put_bytes(&target, data, true).await?;
                }
                davfast::pipeline::Stats {
                    uploaded: 1,
                    bytes: std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0),
                    elapsed_secs: t0.elapsed().as_secs_f64(),
                    ..Default::default()
                }
            };
            print_stats("mailru upload", &stats);
            Ok(if stats.failed > 0 { 1 } else { 0 })
        }
        MailruCmd::Download { home, local, concurrency, skip_existing } => {
            let opts = TransferOpts {
                concurrency,
                skip_existing,
                progress: true,
                ..Default::default()
            };
            let stats = download_tree(&backend, &home, &local, &opts).await?;
            print_stats("mailru download", &stats);
            Ok(if stats.failed > 0 { 1 } else { 0 })
        }
        MailruCmd::Sync { local, home, concurrency, delete, dry_run } => {
            if !local.is_dir() {
                bail!("sync requires a local directory");
            }
            let opts = TransferOpts {
                concurrency,
                skip_existing: true,
                delete_extras: delete,
                dry_run,
                progress: !dry_run,
                speedup_hash: true,
                use_cache: true,
                ..Default::default()
            };
            let stats = upload_tree(&backend, &local, &home, &opts).await?;
            print_stats("mailru sync", &stats);
            Ok(if stats.failed > 0 { 1 } else { 0 })
        }
    }
}

// ------------------------------------------------------------------ serve ---

async fn cmd_serve(
    backend: &str,
    url: Option<String>,
    listen: &str,
    root: &str,
    user: Option<String>,
    pass: Option<String>,
) -> Result<()> {
    let kind = match backend.to_lowercase().as_str() {
        "webdav" => BackendKind::WebDav,
        "mailru" => BackendKind::MailRu,
        other => bail!("unknown backend {:?} (use webdav | mailru)", other),
    };

    let (upstream, upstream_user, upstream_pass) = if kind == BackendKind::WebDav {
        let u = url.clone().context("--url is required for backend webdav")?;
        let mut parsed = reqwest::Url::parse(&u)?;
        let user = if parsed.username().is_empty() {
            std::env::var("DAVFAST_USER").ok()
        } else {
            Some(percent_decode_str(parsed.username()).decode_utf8_lossy().to_string())
        };
        let pass = parsed.password().map(|p| percent_decode_str(p).decode_utf8_lossy().to_string()).or_else(|| std::env::var("DAVFAST_PASS").ok());
        let _ = parsed.set_username("");
        let _ = parsed.set_password(None);
        (Some(parsed.to_string()), user, pass)
    } else {
        (None, None, None)
    };

    let mailru = if kind == BackendKind::MailRu {
        let client = MailRuClient::from_env()?;
        client.login().await?;
        Some(Arc::new(client))
    } else {
        None
    };

    let auth = match (user, pass) {
        (Some(u), Some(p)) => Some((u, p)),
        _ => None,
    };

    serve(ServeConfig {
        kind,
        upstream,
        upstream_user,
        upstream_pass,
        mailru,
        root: root.to_string(),
        listen: listen.to_string(),
        auth,
    })
    .await
}

