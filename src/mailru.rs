//! cloud.mail.ru web API client — the "bridge" backend.
//!
//! Implements the same flow the web interface uses (documented by open
//! implementations such as kireevmp/mailru-cloud-api, mad-gooze/PyMailCloud
//! and the rclone `mailru` backend):
//!
//! 1. `POST auth.mail.ru/cgi-bin/auth`  -> session cookies
//! 2. `POST cloud.mail.ru/api/v2/tokens/csrf` -> CSRF token
//! 3. `GET  cloud.mail.ru/api/v2/dispatcher` -> upload / download node URLs
//! 4. `PUT  {upload node}` content -> response `"<hash>;<size>"`
//! 5. `POST cloud.mail.ru/api/v2/file/add` {home, hash, size} -> registered
//!
//! Step 4+5 means the server deduplicates by hash: re-uploading a file that
//! the cloud already stores completes almost instantly (rclone calls this
//! "put by hash" / speedup).

use anyhow::{anyhow, bail, Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

use crate::dav::DavItem;
use crate::util::{mailru_file_hash, name_from_path, normalize_remote, parse_upload_response};

pub const DEFAULT_AUTH_URL: &str = "https://auth.mail.ru/cgi-bin/auth";
pub const DEFAULT_REST_BASE: &str = "https://cloud.mail.ru";

#[derive(Debug, Clone, Default)]
pub struct MailRuItem {
    /// Full cloud path ("/Dir/file.txt").
    pub home: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<i64>,
    pub hash: Option<String>,
}

pub struct MailRuClient {
    http: Client,
    pub login: String,
    password: String,
    auth_url: String,
    rest_base: String,
    dispatcher_path: String,
    token: Mutex<Option<(String, Instant)>>,
    upload_url: RwLock<Option<String>>,
    get_url: RwLock<Option<String>>,
    known_dirs: RwLock<HashSet<String>>,
}

impl MailRuClient {
    /// Build a client from explicit credentials.
    /// Environment overrides: `DAVFAST_MAILRU_AUTH_URL`, `DAVFAST_MAILRU_REST_BASE`,
    /// `DAVFAST_MAILRU_DISPATCHER` (relative API path, default `api/v2/dispatcher`).
    pub fn new(login: &str, password: &str) -> Result<Self> {
        let http = Client::builder()
            .user_agent(concat!("davfast/", env!("CARGO_PKG_VERSION")))
            .cookie_store(true)
            .pool_max_idle_per_host(32)
            .tcp_nodelay(true)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(900))
            .build()?;
        Ok(Self {
            http,
            login: login.to_string(),
            password: password.to_string(),
            auth_url: std::env::var("DAVFAST_MAILRU_AUTH_URL")
                .unwrap_or_else(|_| DEFAULT_AUTH_URL.to_string()),
            rest_base: std::env::var("DAVFAST_MAILRU_REST_BASE")
                .unwrap_or_else(|_| DEFAULT_REST_BASE.to_string()),
            dispatcher_path: std::env::var("DAVFAST_MAILRU_DISPATCHER")
                .unwrap_or_else(|_| "api/v2/dispatcher".to_string()),
            token: Mutex::new(None),
            upload_url: RwLock::new(None),
            get_url: RwLock::new(None),
            known_dirs: RwLock::new(HashSet::new()),
        })
    }

    /// Build from `MAILRU_LOGIN` / `MAILRU_PASSWORD` environment variables.
    pub fn from_env() -> Result<Self> {
        let login = std::env::var("MAILRU_LOGIN")
            .or_else(|_| std::env::var("MAILRU_USER"))
            .context("set MAILRU_LOGIN and MAILRU_PASSWORD")?;
        let password = std::env::var("MAILRU_PASSWORD").context("set MAILRU_LOGIN and MAILRU_PASSWORD")?;
        Self::new(&login, &password)
    }

    fn enc_email(&self) -> String {
        utf8_percent_encode(&self.login, NON_ALPHANUMERIC).to_string()
    }

    /// Authenticate and obtain a CSRF token. Requires an *app password*
    /// ("пароль для внешнего приложения") if 2FA / external app rules apply.
    pub async fn login(&self) -> Result<()> {
        let (name, domain) = match self.login.split_once('@') {
            Some((n, d)) => (n.to_string(), d.to_string()),
            None => (self.login.clone(), "mail.ru".to_string()),
        };
        let resp = self
            .http
            .post(&self.auth_url)
            .form(&[
                ("page", "https://cloud.mail.ru/"),
                ("Login", name.as_str()),
                ("Domain", domain.as_str()),
                ("Password", self.password.as_str()),
            ])
            .send()
            .await
            .context("auth.mail.ru request failed")?;
        if !resp.status().is_success() {
            bail!("auth.mail.ru -> {}", resp.status());
        }
        // success is verified by requesting a CSRF token with the session
        let token = self.fetch_csrf().await?;
        *self.token.lock().await = Some((token, Instant::now()));
        self.refresh_dispatchers().await?;
        tracing::info!("mail.ru login OK for {}", self.login);
        Ok(())
    }

    async fn refresh_login(&self) -> Result<()> {
        *self.token.lock().await = None;
        self.login().await
    }

    async fn fetch_csrf(&self) -> Result<String> {
        let url = format!("{}/api/v2/tokens/csrf", self.rest_base);
        let email = self.enc_email();
        let resp = self
            .http
            .post(&url)
            .query(&[("api", "2"), ("x-email", email.as_str())])
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("tokens/csrf -> {} (authentication failed?)", resp.status());
        }
        let text = resp.text().await?;
        let v: Value = serde_json::from_str(&text).with_context(|| format!("bad csrf response: {}", &text[..text.len().min(200)]))?;
        let token = v["body"]["token"]
            .as_str()
            .ok_or_else(|| anyhow!("no token in csrf response: {}", &text[..text.len().min(200)]))?
            .to_string();
        Ok(token)
    }

    /// CSRF token, cached for ~23 hours.
    pub async fn token(&self) -> Result<String> {
        let mut guard = self.token.lock().await;
        if let Some((t, at)) = guard.as_ref() {
            if at.elapsed() < Duration::from_secs(23 * 3600) {
                return Ok(t.clone());
            }
        }
        let t = self.fetch_csrf().await?;
        *guard = Some((t.clone(), Instant::now()));
        Ok(t)
    }

    /// Query the dispatcher for upload/download node URLs.
    pub async fn refresh_dispatchers(&self) -> Result<()> {
        let token = self.token().await?;
        let email = self.enc_email();
        let url = format!("{}/{}", self.rest_base, self.dispatcher_path);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("api", "2"),
                ("x-email", email.as_str()),
                ("token", token.as_str()),
            ])
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("dispatcher -> {}", resp.status());
        }
        let v: Value = resp.json().await.context("dispatcher JSON")?;
        let body = &v["body"];
        let up = first_url(&body["upload"]).ok_or_else(|| anyhow!("dispatcher: no upload url in {}", v))?;
        let get = first_url(&body["get"]).ok_or_else(|| anyhow!("dispatcher: no get url in {}", v))?;
        tracing::debug!("dispatchers: upload={} get={}", up, get);
        *self.upload_url.write().await = Some(up);
        *self.get_url.write().await = Some(get);
        Ok(())
    }

    async fn ensure_ready(&self) -> Result<()> {
        self.token().await?;
        if self.upload_url.read().await.is_none() || self.get_url.read().await.is_none() {
            self.refresh_dispatchers().await?;
        }
        Ok(())
    }

    /// POST an api call (form-encoded) with the CSRF token; retries once after
    /// a re-login when the server answers 401/403.
    async fn api_post(&self, path: &str, form: &[(&str, String)]) -> Result<Value> {
        for attempt in 0..2u8 {
            let token = self.token().await?;
            let mut f: Vec<(&str, String)> = vec![("token", token)];
            f.extend(form.iter().cloned());
            let url = format!("{}/{}?api=2&x-email={}", self.rest_base, path, self.enc_email());
            let resp = self.http.post(&url).form(&f).send().await?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if (status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED) && attempt == 0 {
                tracing::debug!("{} -> {}, refreshing session", path, status);
                self.refresh_login().await?;
                continue;
            }
            let v: Value = serde_json::from_str(&text)
                .with_context(|| format!("{}: bad JSON response: {}", path, &text[..text.len().min(300)]))?;
            let inner = v["status"].as_i64().unwrap_or(0);
            if inner != 200 {
                bail!("{} failed: {}", path, v["body"]);
            }
            return Ok(v);
        }
        bail!("{} failed after re-login", path)
    }

    /// GET an api call; returns the raw JSON envelope.
    async fn api_get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        for attempt in 0..2u8 {
            let token = self.token().await?;
            let mut q: Vec<(String, String)> = vec![
                ("api".into(), "2".into()),
                ("x-email".into(), self.enc_email()),
                ("token".into(), token),
            ];
            for (k, v) in query {
                q.push((k.to_string(), v.to_string()));
            }
            let url = format!("{}/{}", self.rest_base, path);
            let resp = self.http.get(&url).query(&q).send().await?;
            let status = resp.status();
            if (status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED) && attempt == 0 {
                self.refresh_login().await?;
                continue;
            }
            let text = resp.text().await.unwrap_or_default();
            return serde_json::from_str(&text)
                .with_context(|| format!("GET {}: bad JSON: {}", path, &text[..text.len().min(300)]));
        }
        bail!("GET {} failed after re-login", path)
    }

    fn parse_item(o: &Value, fallback_dir: Option<&str>) -> Option<MailRuItem> {
        let kind = o.get("type").or_else(|| o.get("kind"))?.as_str()?;
        let is_dir = kind == "folder";
        let home = o
            .get("home")
            .and_then(|h| h.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                // some responses only carry name + tree
                let n = o.get("name")?.as_str()?.to_string();
                Some(format!("{}/{}", fallback_dir?.trim_end_matches('/'), n))
            })?;
        let name = o
            .get("name")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| name_from_path(&home));
        let size = item_u64(o.get("size")).unwrap_or(0);
        let mtime = o.get("mtime").and_then(|m| m.as_i64());
        let hash = o
            .get("hash")
            .and_then(|h| h.as_str())
            .map(|s| s.to_lowercase());
        Some(MailRuItem {
            home: normalize_remote(&home),
            name,
            is_dir,
            size,
            mtime,
            hash,
        })
    }

    /// List a folder (children only).
    pub async fn list(&self, home: &str) -> Result<Vec<MailRuItem>> {
        self.ensure_ready().await?;
        let h = normalize_remote(home);
        let v = self
            .api_get("api/v2/folder", &[("home", h.clone())])
            .await?;
        let status = v["status"].as_i64().unwrap_or(0);
        if status != 200 {
            if status == 404 {
                return Ok(Vec::new());
            }
            bail!("folder list {}: {}", h, v["body"]);
        }
        let list = &v["body"]["list"];
        let mut out = Vec::new();
        if let Value::Array(arr) = list {
            for it in arr {
                if let Some(m) = Self::parse_item(it, Some(&h)) {
                    out.push(m);
                }
            }
        }
        Ok(out)
    }

    /// Information about a single file/folder (None when missing).
    pub async fn stat(&self, home: &str) -> Result<Option<MailRuItem>> {
        self.ensure_ready().await?;
        let h = normalize_remote(home);
        let v = self.api_get("api/v2/file", &[("home", h.clone())]).await?;
        let status = v["status"].as_i64().unwrap_or(0);
        if status != 200 {
            return Ok(None);
        }
        Ok(Self::parse_item(&v["body"], None))
    }

    /// Create a single folder. Fails when it already exists.
    pub async fn mkdir(&self, home: &str) -> Result<()> {
        self.ensure_ready().await?;
        let h = normalize_remote(home);
        if h == "/" {
            return Ok(());
        }
        self.api_post("api/v2/folder/add", &[("home", h)]).await?;
        Ok(())
    }

    /// Create a folder chain, ignoring "already exists" errors.
    pub async fn mkdir_all(&self, home: &str) -> Result<()> {
        let h = normalize_remote(home);
        let mut cur = String::new();
        for seg in h.split('/').filter(|s| !s.is_empty()) {
            cur.push('/');
            cur.push_str(seg);
            match self.mkdir(&cur).await {
                Ok(()) => {}
                Err(e) => {
                    let msg = format!("{}", e);
                    if msg.contains("exist") || msg.contains("409") || msg.contains("12") {
                        tracing::debug!("mkdir {}: exists", cur);
                    } else {
                        tracing::debug!("mkdir {}: {}", cur, msg);
                    }
                }
            }
        }
        self.known_dirs.write().await.insert(h);
        Ok(())
    }

    /// Delete a file or a folder tree.
    pub async fn remove(&self, home: &str) -> Result<()> {
        self.ensure_ready().await?;
        let h = normalize_remote(home);
        if h == "/" {
            bail!("refusing to remove cloud root");
        }
        self.api_post("api/v2/file/remove", &[("home", h)]).await?;
        Ok(())
    }

    /// Move an item into another folder.
    pub async fn move_item(&self, home: &str, to_folder: &str) -> Result<()> {
        self.ensure_ready().await?;
        self.api_post(
            "api/v2/file/move",
            &[
                ("home", normalize_remote(home)),
                ("folder", normalize_remote(to_folder)),
            ],
        )
        .await?;
        Ok(())
    }

    /// Rename an item in place.
    pub async fn rename(&self, home: &str, new_name: &str) -> Result<()> {
        self.ensure_ready().await?;
        self.api_post(
            "api/v2/file/rename",
            &[("home", normalize_remote(home)), ("name", new_name.to_string())],
        )
        .await?;
        Ok(())
    }

    /// Register an already-known (hash, size) pair — server-side dedup makes
    /// this instant when the content is already stored in the cloud.
    pub async fn add_by_hash(&self, home: &str, hash: &str, size: u64) -> Result<()> {
        self.ensure_ready().await?;
        self.api_post(
            "api/v2/file/add",
            &[
                ("home", normalize_remote(home)),
                ("hash", hash.to_string()),
                ("size", size.to_string()),
            ],
        )
        .await?;
        Ok(())
    }

    /// Upload file content following the web flow: PUT bytes to the upload
    /// node, then register with `file/add`. Returns the file hash.
    pub async fn put_file(&self, home: &str, data: Vec<u8>, speedup: bool) -> Result<String> {
        self.ensure_ready().await?;
        let h = normalize_remote(home);
        if h == "/" {
            bail!("upload target must be a file path, got /");
        }
        if data.len() > 2 * 1024 * 1024 * 1024 {
            bail!("file {} exceeds the 2 GB mail.ru limit", h);
        }
        let local_hash = mailru_file_hash(&data);
        if speedup {
            if self.add_by_hash(&h, &local_hash, data.len() as u64).await.is_ok() {
                tracing::debug!("{}: registered by hash (dedup hit)", h);
                return Ok(local_hash);
            }
        }
        self.ensure_parent(&h).await?;
        let up = self
            .upload_url
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("no upload url (dispatcher failed)"))?;
        let url = format!(
            "{}?x-email={}&cloud_domain=2",
            up,
            self.enc_email()
        );
        let resp = self
            .http
            .put(&url)
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .send()
            .await
            .context("upload node PUT failed")?;
        if !resp.status().is_success() {
            bail!("upload node PUT {} -> {}", h, resp.status());
        }
        let text = resp.text().await?;
        let (hash, _size) = parse_upload_response(&text)?;
        self.add_by_hash(&h, &hash, _size).await?;
        Ok(hash)
    }

    /// Make sure the parent folder chain of `home` exists (cheap, cached).
    async fn ensure_parent(&self, home: &str) -> Result<()> {
        let h = normalize_remote(home);
        let parent = match h.rsplit_once('/') {
            Some((p, _)) if p.is_empty() => "/".to_string(),
            Some((p, _)) => p.to_string(),
            None => "/".to_string(),
        };
        if parent == "/" {
            return Ok(());
        }
        if self.known_dirs.read().await.contains(&parent) {
            return Ok(());
        }
        let exists = self.stat(&parent).await?.map(|i| i.is_dir).unwrap_or(false);
        if !exists {
            self.mkdir_all(&parent).await?;
        } else {
            self.known_dirs.write().await.insert(parent);
        }
        Ok(())
    }

    /// Open a download stream for a file (`GET {download node}{home}`).
    pub async fn download(&self, home: &str) -> Result<reqwest::Response> {
        self.ensure_ready().await?;
        let h = normalize_remote(home);
        let get = self
            .get_url
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("no download url (dispatcher failed)"))?;
        let base = get.trim_end_matches('/');
        let url = format!("{}{}", base, crate::util::encode_dav_path(&h));
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            bail!("download {} -> {}", h, resp.status());
        }
        Ok(resp)
    }

    /// Adapt to the generic pipeline item type.
    pub fn to_dav_item(it: &MailRuItem) -> DavItem {
        DavItem {
            path: it.home.clone(),
            name: it.name.clone(),
            is_dir: it.is_dir,
            size: it.size,
            etag: it.hash.clone(),
            modified_epoch: it.mtime,
        }
    }
}

/// Dispatcher `upload` / `get` fields appear in two shapes:
/// `[{url: "..."}]` or `{"count": N, "url": ["..."]}` — handle both.
fn first_url(v: &Value) -> Option<String> {
    match v {
        Value::Array(arr) => arr
            .iter()
            .find_map(|x| x.get("url").and_then(|u| u.as_str()).map(String::from)),
        Value::Object(o) => match o.get("url") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Array(a)) => a.iter().find_map(|x| x.as_str().map(String::from)),
            _ => None,
        },
        _ => None,
    }
}

fn item_u64(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}
