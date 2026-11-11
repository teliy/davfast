//! Async WebDAV client: PROPFIND / MKCOL / PUT / GET / DELETE / MOVE with a
//! connection pool, keep-alive, HTTP/2 and per-request retries.
//!
//! The client is deliberately "batch friendly": directory listings are parsed
//! into plain structs so the transfer pipeline can compare thousands of files
//! with a *single* PROPFIND per directory instead of one request per file.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use bytes::Bytes;
use quick_xml::events::Event;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::{Client, Method, StatusCode};
use std::time::Duration;
use url::Url;

use crate::util::{encode_dav_path, href_to_path, name_from_path, normalize_remote};

const RETRIES: usize = 3;
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:"><D:prop>
<D:getetag/><D:getcontentlength/><D:getlastmodified/><D:resourcetype/><D:displayname/>
</D:prop></D:propfind>"#;

/// One entry of a directory listing.
#[derive(Debug, Clone, Default)]
pub struct DavItem {
    /// Absolute decoded path, no trailing slash ("/dir/file.txt").
    pub path: String,
    /// Display name (last segment).
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub etag: Option<String>,
    pub modified_epoch: Option<i64>,
}

#[derive(Clone)]
pub struct DavClient {
    http: Client,
    base: Url,
}

impl DavClient {
    pub fn new(
        base: &str,
        user: Option<&str>,
        pass: Option<&str>,
        pool_size: usize,
    ) -> Result<Self> {
        let mut b = Client::builder()
            .user_agent(concat!("davfast/", env!("CARGO_PKG_VERSION")))
            .pool_max_idle_per_host(pool_size.max(8))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_nodelay(true)
            .tcp_keepalive(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(900))
            .http2_adaptive_window(true);
        if let (Some(u), Some(p)) = (user, pass) {
            let creds = format!("{}:{}", u, p);
            let mut hm = HeaderMap::new();
            hm.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(creds)
                ))?,
            );
            b = b.default_headers(hm);
        }
        let mut url = Url::parse(base).context("bad WebDAV base URL")?;
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        Ok(Self {
            http: b.build()?,
            base: url,
        })
    }

    /// Build the full URL for a remote path (base path prefix is preserved).
    pub fn url_for(&self, path: &str) -> String {
        let p = normalize_remote(path);
        let enc = encode_dav_path(&p);
        let base_path = self.base.path().trim_end_matches('/').to_string();
        let full = if enc == "/" {
            format!("{}/", base_path)
        } else {
            format!("{}{}", base_path, enc)
        };
        let mut u = self.base.clone();
        u.set_path(&full);
        u.set_query(None);
        u.set_fragment(None);
        u.to_string()
    }

    /// PROPFIND a directory (or file). `depth` 0 = self only, 1 = self + children.
    /// Returns the parsed entries **including** the requested path itself
    /// (servers always return it first).
    pub async fn propfind(&self, path: &str, depth: u32) -> Result<Vec<DavItem>> {
        let url = self.url_for(path);
        let method = Method::from_bytes(b"PROPFIND").unwrap();
        for attempt in 0..RETRIES {
            let resp = self
                .http
                .request(method.clone(), &url)
                .header("Depth", depth.to_string())
                .header("Content-Type", "application/xml")
                .body(Bytes::from_static(PROPFIND_BODY.as_bytes()))
                .send()
                .await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) if attempt + 1 < RETRIES => {
                    tracing::debug!("PROPFIND {} network error ({}), retrying", path, e);
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(anyhow!("PROPFIND {}: {}", path, e)),
            };
            let status = resp.status();
            if status == StatusCode::NOT_FOUND {
                return Ok(Vec::new());
            }
            if status.as_u16() == 207 || status.is_success() {
                let text = resp.text().await.context("reading PROPFIND body")?;
                return parse_multistatus(&text);
            }
            if retryable(status) && attempt + 1 < RETRIES {
                tracing::debug!("PROPFIND {} -> {}, retrying", path, status);
                backoff(attempt).await;
                continue;
            }
            bail!("PROPFIND {} -> {}", path, status);
        }
        bail!("PROPFIND {} retries exhausted", path)
    }

    /// MKCOL a directory; "already exists" answers (405/301) are treated as OK.
    pub async fn mkcol(&self, path: &str) -> Result<()> {
        let url = self.url_for(path);
        for attempt in 0..RETRIES {
            let resp = match self.http.request(Method::from_bytes(b"MKCOL").unwrap(), &url).send().await {
                Ok(r) => r,
                Err(_e) if attempt + 1 < RETRIES => {
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(anyhow!("MKCOL {}: {}", path, e)),
            };
            let status = resp.status();
            if status.is_success() || status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::MOVED_PERMANENTLY || status.as_u16() == 409 {
                return Ok(());
            }
            if retryable(status) && attempt + 1 < RETRIES {
                backoff(attempt).await;
                continue;
            }
            bail!("MKCOL {} -> {}", path, status);
        }
        bail!("MKCOL {} retries exhausted", path)
    }

    /// PUT an in-memory body.
    pub async fn put_bytes(&self, path: &str, data: Vec<u8>) -> Result<()> {
        let url = self.url_for(path);
        for attempt in 0..RETRIES {
            let resp = self
                .http
                .put(&url)
                .header("Content-Type", "application/octet-stream")
                .body(data.clone())
                .send()
                .await;
            let resp = match resp {
                Ok(r) => r,
                Err(_e) if attempt + 1 < RETRIES => {
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(anyhow!("PUT {}: {}", path, e)),
            };
            let status = resp.status();
            if status.is_success() {
                return Ok(());
            }
            if retryable(status) && attempt + 1 < RETRIES {
                backoff(attempt).await;
                continue;
            }
            bail!("PUT {} -> {}", path, status);
        }
        bail!("PUT {} retries exhausted", path)
    }

    /// PUT a file from disk, streamed (constant memory).
    pub async fn put_file(&self, path: &str, file: tokio::fs::File, _size: u64) -> Result<()> {
        let url = self.url_for(path);
        let stream = tokio_util::io::ReaderStream::new(file);
        let body = reqwest::Body::wrap_stream(stream);
        let resp = self
            .http
            .put(&url)
            .header("Content-Type", "application/octet-stream")
            .body(body)
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            bail!("PUT {} -> {}", path, status)
        }
    }

    /// GET a resource; the response body is streamed to the caller.
    pub async fn get_stream(&self, path: &str) -> Result<reqwest::Response> {
        let url = self.url_for(path);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            bail!("GET {} -> {}", path, resp.status());
        }
        Ok(resp)
    }

    /// GET a resource into memory.
    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let resp = self.get_stream(path).await?;
        let b = resp.bytes().await?;
        Ok(b.to_vec())
    }

    /// DELETE a resource (servers usually delete collections recursively).
    pub async fn delete(&self, path: &str) -> Result<()> {
        let url = self.url_for(path);
        for attempt in 0..RETRIES {
            let resp = match self.http.delete(&url).send().await {
                Ok(r) => r,
                Err(_e) if attempt + 1 < RETRIES => {
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(anyhow!("DELETE {}: {}", path, e)),
            };
            let status = resp.status();
            if status.is_success() || status == StatusCode::NOT_FOUND {
                return Ok(());
            }
            if retryable(status) && attempt + 1 < RETRIES {
                backoff(attempt).await;
                continue;
            }
            bail!("DELETE {} -> {}", path, status);
        }
        bail!("DELETE {} retries exhausted", path)
    }

    /// MOVE a resource to a new path (Destination header).
    pub async fn move_to(&self, from: &str, to: &str) -> Result<()> {
        let url = self.url_for(from);
        let dest = self.url_for(to);
        let resp = self
            .http
            .request(Method::from_bytes(b"MOVE").unwrap(), &url)
            .header("Destination", dest)
            .header("Overwrite", "T")
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            bail!("MOVE {} -> {} -> {}", from, status, to)
        }
    }
}

fn retryable(s: StatusCode) -> bool {
    s.is_server_error()
        || s == StatusCode::TOO_MANY_REQUESTS
        || s == StatusCode::REQUEST_TIMEOUT
        || s == StatusCode::SERVICE_UNAVAILABLE
}

async fn backoff(attempt: usize) {
    let ms = 300u64 * (1u64 << attempt.min(4));
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Normalize hrefs into absolute decoded paths + derived display names.
fn fix_items(mut items: Vec<DavItem>) -> Vec<DavItem> {
    for it in items.iter_mut() {
        it.path = href_to_path(&it.path);
        it.name = name_from_path(&it.path);
    }
    items
}

fn local_name(n: &[u8]) -> &[u8] {
    match n.iter().rposition(|&c| c == b':') {
        Some(i) => &n[i + 1..],
        None => n,
    }
}

/// Parse a WebDAV multistatus (207) XML document.
/// Namespace prefixes (`D:`, `d:`, none) are handled transparently.
pub fn parse_multistatus(xml: &str) -> Result<Vec<DavItem>> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut items: Vec<DavItem> = Vec::new();
    let mut cur: Option<DavItem> = None;
    let mut prop: Vec<u8> = Vec::new();

    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let ln = local_name(e.name().0).to_vec();
                match ln.as_slice() {
                    b"response" => cur = Some(DavItem::default()),
                    b"href" | b"getetag" | b"getcontentlength" | b"getlastmodified"
                    | b"displayname" => prop = ln,
                    _ => {}
                }
            }
            Event::Empty(e) => {
                if local_name(e.name().0) == b"collection" {
                    if let Some(i) = cur.as_mut() {
                        i.is_dir = true;
                    }
                }
            }
            Event::Text(t) => {
                if prop.is_empty() {
                    continue;
                }
                let s = t.unescape().unwrap_or_default().trim().to_string();
                if s.is_empty() {
                    continue;
                }
                if let Some(it) = cur.as_mut() {
                    match prop.as_slice() {
                        b"href" => it.path = s,
                        b"getetag" => it.etag = Some(s),
                        b"getcontentlength" => it.size = s.parse().unwrap_or(0),
                        b"getlastmodified" => {
                            it.modified_epoch = httpdate::parse_http_date(&s)
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs() as i64);
                        }
                        _ => {}
                    }
                }
            }
            Event::End(e) => {
                let ln = local_name(e.name().0);
                match ln {
                    b"response" => {
                        if let Some(i) = cur.take() {
                            items.push(i);
                        }
                    }
                    b"href" | b"getetag" | b"getcontentlength" | b"getlastmodified"
                    | b"displayname" => prop.clear(),
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(fix_items(items))
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML_NS_D: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
 <D:response>
  <D:href>/dav/</D:href>
  <D:propstat><D:prop>
   <D:resourcetype><D:collection/></D:resourcetype>
   <D:getlastmodified>Mon, 02 Jan 2006 15:04:05 GMT</D:getlastmodified>
  </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
 </D:response>
 <D:response>
  <D:href>/dav/%D0%BF%D0%B0%D0%BF%D0%BA%D0%B0/</D:href>
  <D:propstat><D:prop>
   <D:resourcetype><D:collection/></D:resourcetype>
   <D:getlastmodified>Mon, 02 Jan 2006 15:04:05 GMT</D:getlastmodified>
  </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
 </D:response>
 <D:response>
  <D:href>/dav/dir/file.txt</D:href>
  <D:propstat><D:prop>
   <D:resourcetype/>
   <D:getcontentlength>1234</D:getcontentlength>
   <D:getetag>"abc-1"</D:getetag>
   <D:getlastmodified>Mon, 02 Jan 2006 15:04:05 GMT</D:getlastmodified>
  </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
 </D:response>
</D:multistatus>"#;

    #[test]
    fn parse_ns_prefixed() {
        let items = parse_multistatus(XML_NS_D).unwrap();
        assert_eq!(items.len(), 3);
        assert!(items[0].is_dir);
        assert_eq!(items[0].path, "/dav");
        assert!(items[1].is_dir);
        assert_eq!(items[1].path, "/dav/папка");
        assert_eq!(items[1].name, "папка");
        assert!(!items[2].is_dir);
        assert_eq!(items[2].size, 1234);
        assert_eq!(items[2].etag.as_deref(), Some("\"abc-1\""));
        assert_eq!(items[2].modified_epoch, Some(1136214245));
    }

    #[test]
    fn parse_no_prefix() {
        let xml = r#"<multistatus xmlns="DAV:">
<response><href>/a/b.txt</href><propstat><prop>
<resourcetype/><getcontentlength>7</getcontentlength></prop>
<status>HTTP/1.1 200 OK</status></propstat></response>
</multistatus>"#;
        let items = parse_multistatus(xml).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, "/a/b.txt");
        assert_eq!(items[0].size, 7);
    }

    #[test]
    fn percent_in_href() {
        // hrefs arriving decoded must survive as-is; encoded ones decoded
        assert_eq!(href_to_path("/a%20b/c.txt"), "/a b/c.txt");
    }

    #[tokio::test]
    async fn url_for_encoding() {
        let dav = DavClient::new("http://127.0.0.1:9/dav/", None, None, 4).unwrap();
        assert_eq!(dav.url_for("/"), "http://127.0.0.1:9/dav/");
        assert_eq!(dav.url_for("/a b/ф.txt"), "http://127.0.0.1:9/dav/a%20b/%D1%84.txt");
    }
}
