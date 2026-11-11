//! Local WebDAV bridge/proxy server.
//!
//! Two backends:
//! - `webdav` — transparent high-performance proxy to any WebDAV upstream
//!   (connection pool, keep-alive, HTTP/2 upstream);
//! - `mailru` — WebDAV front-end for the cloud.mail.ru web API: metadata via
//!   JSON REST, uploads via the same flow the web interface uses (upload node
//!   PUT + file/add, hash dedup). Mount it with davfs2 and enjoy web speeds.

use anyhow::Result;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use percent_encoding::percent_decode_str;
use std::sync::Arc;
use std::time::Duration;

use crate::mailru::MailRuClient;
use crate::util::{encode_dav_path, epoch_to_http_date, name_from_path, normalize_remote, xml_escape};

const MAX_BODY: usize = 1024 * 1024 * 1024; // 1 GiB PUT cap for the bridge

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackendKind {
    WebDav,
    MailRu,
}

pub struct ServeConfig {
    pub kind: BackendKind,
    /// upstream WebDAV base URL (backend webdav)
    pub upstream: Option<String>,
    pub upstream_user: Option<String>,
    pub upstream_pass: Option<String>,
    /// logged-in client (backend mailru)
    pub mailru: Option<Arc<MailRuClient>>,
    /// remote root exposed at `/` ("/" = whole cloud)
    pub root: String,
    pub listen: String,
    /// optional Basic auth to protect this local endpoint
    pub auth: Option<(String, String)>,
}

struct AppState {
    kind: BackendKind,
    http: reqwest::Client,
    upstream_base: Option<String>,
    upstream_auth: Option<(String, String)>,
    mailru: Option<Arc<MailRuClient>>,
    root: String,
    auth: Option<(String, String)>,
}

/// Run the bridge server (blocks until Ctrl-C).
pub async fn serve(cfg: ServeConfig) -> Result<()> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("davfast/", env!("CARGO_PKG_VERSION")))
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(30))
        .timeout(Duration::from_secs(900))
        .build()?;

    let (upstream_base, upstream_auth) = match cfg.kind {
        BackendKind::WebDav => {
            let u = cfg.upstream.clone().ok_or_else(|| anyhow::anyhow!("--url is required for backend webdav"))?;
            let mut parsed = reqwest::Url::parse(&u)?;
            let user = cfg.upstream_user.clone().or_else(|| {
                if parsed.username().is_empty() {
                    None
                } else {
                    Some(percent_decode_str(parsed.username()).decode_utf8_lossy().to_string())
                }
            });
            let pass = cfg.upstream_pass.clone().or_else(|| parsed.password().map(|p| percent_decode_str(p).decode_utf8_lossy().to_string()));
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            let base = parsed.to_string().trim_end_matches('/').to_string();
            (Some(base), user.map(|u| (u, pass.unwrap_or_default())))
        }
        BackendKind::MailRu => (None, None),
    };

    let root = normalize_remote(&cfg.root);
    let state = Arc::new(AppState {
        kind: cfg.kind,
        http,
        upstream_base,
        upstream_auth,
        mailru: cfg.mailru,
        root,
        auth: cfg.auth,
    });

    let app = Router::new()
        .route("/", any(handler))
        .route("/*path", any(handler))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    eprintln!("davfast bridge ready: http://{} ({:?} backend, root {})", cfg.listen, cfg.kind, state.root);
    match cfg.kind {
        BackendKind::MailRu => {
            eprintln!("mount hint:  mount -t davfs http://{}/ /mnt/cloud   (davfs2: set use_locks 0)", cfg.listen);
        }
        BackendKind::WebDav => {
            eprintln!("mount hint:  mount -t davfs http://{}/ /mnt/dav       (davfs2: set use_locks 0)", cfg.listen);
        }
    }
    axum::serve(listener, app).await?;
    Ok(())
}

// ------------------------------------------------------------------ glue ----

fn text_response(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(format!("{}\n", msg)))
        .unwrap()
}

fn dav_headers(b: axum::http::response::Builder) -> axum::http::response::Builder {
    b.header("DAV", "1, 2")
        .header("MS-Author-Via", "DAV")
        .header(
            header::ALLOW,
            "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, MKCOL, MOVE, LOCK, UNLOCK",
        )
}

fn empty_response(status: StatusCode) -> Response {
    dav_headers(Response::builder().status(status))
        .body(Body::empty())
        .unwrap()
}

async fn handler(State(st): State<Arc<AppState>>, req: Request) -> Response {
    if let Some(resp) = check_auth(&st, &req) {
        return resp;
    }
    let method = req.method().clone();
    let (parts, body) = req.into_parts();
    let raw = parts.uri.path().trim_start_matches('/').to_string();
    let query = parts.uri.query().map(|s| s.to_string());
    let rel = normalize_remote(&percent_decode_str(&raw).decode_utf8_lossy());
    let full = if st.root == "/" {
        rel.clone()
    } else if rel == "/" {
        st.root.clone()
    } else {
        normalize_remote(&format!("{}/{}", st.root, rel))
    };
    // path-escape guard
    if st.root != "/" && !full.starts_with(&st.root) {
        return text_response(StatusCode::FORBIDDEN, "path escapes configured root");
    }

    match st.kind {
        BackendKind::WebDav => passthrough(&st, method, full, query, parts.headers, body).await,
        BackendKind::MailRu => mailru_handle(&st, method, full, rel, parts, body).await,
    }
}

fn check_auth(st: &AppState, req: &Request) -> Option<Response> {
    let (want_user, want_pass) = st.auth.as_ref()?;
    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Basic "))
        .and_then(|b| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(b).ok()
        });
    // base64 is small enough to inline without the extra dep
    let ok = match provided {
        Some(bytes) => {
            let s = String::from_utf8_lossy(&bytes).to_string();
            let mut it = s.splitn(2, ':');
            let u = it.next().unwrap_or("");
            let p = it.next().unwrap_or("");
            u == want_user && p == want_pass
        }
        None => false,
    };
    if ok {
        return None;
    }
    Some(
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::WWW_AUTHENTICATE, "Basic realm=\"davfast\"")
            .body(Body::from("unauthorized\n"))
            .unwrap(),
    )
}

// -------------------------------------------------------- webdav passthrough -

async fn passthrough(
    st: &AppState,
    method: Method,
    full: String,
    query: Option<String>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    let base = match &st.upstream_base {
        Some(b) => b.clone(),
        None => return text_response(StatusCode::INTERNAL_SERVER_ERROR, "no upstream"),
    };
    let enc = encode_dav_path(&normalize_remote(&full));
    let mut url = format!("{}{}", base.trim_end_matches('/'), enc);
    if enc == "/" {
        url = format!("{}/", base.trim_end_matches('/'));
    }
    if let Some(q) = query {
        url.push('?');
        url.push_str(&q);
    }

    let mut rb = st.http.request(method, &url);
    if let Some((u, p)) = &st.upstream_auth {
        rb = rb.basic_auth(u.clone(), Some(p.clone()));
    }
    for (k, v) in headers.iter() {
        if matches!(
            k.as_str(),
            "host" | "authorization" | "accept-encoding" | "cookie" | "content-length"
        ) {
            continue;
        }
        rb = rb.header(k, v);
    }
    match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) if !b.is_empty() => rb = rb.body(b.to_vec()),
        Ok(_) => {}
        Err(_) => return text_response(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    }

    match rb.send().await {
        Err(e) => text_response(StatusCode::BAD_GATEWAY, &format!("upstream error: {}", e)),
        Ok(r) => {
            let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let mut b = Response::builder().status(status);
            for k in [
                header::CONTENT_TYPE,
                header::CONTENT_LENGTH,
                header::ETAG,
                header::LAST_MODIFIED,
                header::LOCATION,
            ] {
                if let Some(v) = r.headers().get(&k) {
                    b = b.header(k, v);
                }
            }
            let stream = r.bytes_stream();
            match b.body(Body::from_stream(stream)) {
                Ok(resp) => resp,
                Err(_) => text_response(StatusCode::INTERNAL_SERVER_ERROR, "stream error"),
            }
        }
    }
}

// ------------------------------------------------------------ mailru mode ---

struct XmlEntry {
    href: String,
    is_dir: bool,
    size: u64,
    mtime: Option<i64>,
    etag: Option<String>,
}

fn multistatus(entries: &[XmlEntry]) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">");
    for e in entries {
        let href = xml_escape(&encode_dav_path(&e.href));
        let rt = if e.is_dir { "<D:collection/>" } else { "" };
        let len = if e.is_dir { String::new() } else { format!("<D:getcontentlength>{}</D:getcontentlength>", e.size) };
        let lm = e
            .mtime
            .map(|t| format!("<D:getlastmodified>{}</D:getlastmodified>", xml_escape(&epoch_to_http_date(t))))
            .unwrap_or_default();
        let etag = e
            .etag
            .as_ref()
            .map(|t| format!("<D:getetag>\"{}\"</D:getetag>", xml_escape(t)))
            .unwrap_or_default();
        let name = name_from_path(&e.href);
        let dn = if name.is_empty() { String::new() } else { format!("<D:displayname>{}</D:displayname>", xml_escape(&name)) };
        s.push_str(&format!(
            "<D:response><D:href>{}</D:href><D:propstat><D:prop><D:resourcetype>{}{}</D:resourcetype>{}{}{}{}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
            href, rt, dn, len, lm, etag, ""
        ));
    }
    s.push_str("</D:multistatus>");
    s
}

fn xml_response(status: StatusCode, body: String) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

async fn mailru_handle(
    st: &AppState,
    method: Method,
    full: String,
    rel: String,
    parts: axum::http::request::Parts,
    body: axum::body::Body,
) -> Response {
    let mr = match &st.mailru {
        Some(m) => m.clone(),
        None => return text_response(StatusCode::INTERNAL_SERVER_ERROR, "mailru client missing"),
    };

    match method {
        Method::OPTIONS => dav_headers(Response::builder().status(StatusCode::OK)).body(Body::empty()).unwrap(),
        Method::GET | Method::HEAD => match mr.stat(&full).await {
            Ok(Some(item)) if item.is_dir => match mr.list(&full).await {
                Ok(children) => {
                    let mut html = format!("<html><body><h1>{}</h1><ul>", xml_escape(&rel));
                    for c in children {
                        html.push_str(&format!(
                            "<li><a href=\"{}\">{}</a></li>",
                            xml_escape(&encode_dav_path(&format!("{}/{}", rel.trim_end_matches('/'), c.name))),
                            xml_escape(&c.name)
                        ));
                    }
                    html.push_str("</ul></body></html>");
                    let r = Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from(html))
                        .unwrap();
                    if method == Method::HEAD {
                        Response::builder().status(StatusCode::OK).body(Body::empty()).unwrap()
                    } else {
                        r
                    }
                }
                Err(e) => text_response(StatusCode::BAD_GATEWAY, &format!("list: {}", e)),
            },
            Ok(Some(_)) => match mr.download(&full).await {
                Ok(r) => {
                    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::OK);
                    let mut b = Response::builder().status(status);
                    for k in [header::CONTENT_TYPE, header::CONTENT_LENGTH] {
                        if let Some(v) = r.headers().get(&k) {
                            b = b.header(k, v);
                        }
                    }
                    if method == Method::HEAD {
                        Response::builder().status(status).body(Body::empty()).unwrap()
                    } else {
                        match b.body(Body::from_stream(r.bytes_stream())) {
                            Ok(resp) => resp,
                            Err(_) => text_response(StatusCode::INTERNAL_SERVER_ERROR, "stream error"),
                        }
                    }
                }
                Err(e) => text_response(StatusCode::BAD_GATEWAY, &format!("download: {}", e)),
            },
            Ok(None) => text_response(StatusCode::NOT_FOUND, "not found"),
            Err(e) => text_response(StatusCode::BAD_GATEWAY, &format!("stat: {}", e)),
        },
        Method::PUT => match axum::body::to_bytes(body, MAX_BODY).await {
            Ok(b) => match mr.put_file(&full, b.to_vec(), true).await {
                Ok(hash) => Response::builder()
                    .status(StatusCode::CREATED)
                    .header(header::ETAG, format!("\"{}\"", hash))
                    .body(Body::empty())
                    .unwrap(),
                Err(e) => text_response(StatusCode::BAD_GATEWAY, &format!("upload: {}", e)),
            },
            Err(_) => text_response(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
        },
        Method::DELETE => match mr.remove(&full).await {
            Ok(()) => empty_response(StatusCode::NO_CONTENT),
            Err(e) => text_response(StatusCode::BAD_GATEWAY, &format!("remove: {}", e)),
        },
        m if m.as_str() == "MKCOL" => match mr.stat(&full).await {
            Ok(Some(_)) => text_response(StatusCode::METHOD_NOT_ALLOWED, "already exists"),
            Ok(None) => match mr.mkdir_all(&full).await {
                Ok(()) => empty_response(StatusCode::CREATED),
                Err(e) => text_response(StatusCode::BAD_GATEWAY, &format!("mkdir: {}", e)),
            },
            Err(e) => text_response(StatusCode::BAD_GATEWAY, &format!("stat: {}", e)),
        },
        m if m.as_str() == "MOVE" => {
            let dest = parts
                .headers
                .get("Destination")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let dest = match dest {
                Some(d) => d,
                None => return text_response(StatusCode::BAD_REQUEST, "missing Destination"),
            };
            let dest_path = if dest.starts_with("http") {
                reqwest::Url::parse(&dest).ok().map(|u| u.path().to_string())
            } else {
                Some(dest)
            };
            let dest_rel = dest_path
                .map(|p| normalize_remote(&percent_decode_str(&p).decode_utf8_lossy()))
                .unwrap_or_default();
            let dest_full = if st.root == "/" {
                dest_rel.clone()
            } else {
                normalize_remote(&format!("{}/{}", st.root, dest_rel))
            };
            let (src_parent, _) = match full.rsplit_once('/') {
                Some(x) => x,
                None => ("/", ""),
            };
            let (dst_parent, dst_name) = match dest_full.rsplit_once('/') {
                Some((p, n)) if p.is_empty() => ("/", n),
                Some((p, n)) => (p, n),
                None => ("/", dest_full.trim_start_matches('/')),
            };
            let moved_name = name_from_path(&full);
            let r = if src_parent == dst_parent {
                mr.rename(&full, dst_name).await
            } else {
                match mr.move_item(&full, dst_parent).await {
                    Ok(()) => {
                        if moved_name == dst_name {
                            Ok(())
                        } else {
                            mr.rename(&format!("{}/{}", dst_parent, moved_name), dst_name).await
                        }
                    }
                    Err(e) => Err(e),
                }
            };
            match r {
                Ok(()) => empty_response(StatusCode::CREATED),
                Err(e) => text_response(StatusCode::BAD_GATEWAY, &format!("move: {}", e)),
            }
        }
        m if m.as_str() == "PROPFIND" => {
            let depth = parts
                .headers
                .get("Depth")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(1)
                .min(1);
            let item = match mr.stat(&full).await {
                Ok(Some(i)) => i,
                Ok(None) => return text_response(StatusCode::NOT_FOUND, "not found"),
                Err(e) => return text_response(StatusCode::BAD_GATEWAY, &format!("stat: {}", e)),
            };
            let mut entries = vec![XmlEntry {
                href: rel.clone(),
                is_dir: item.is_dir,
                size: item.size,
                mtime: item.mtime,
                etag: item.hash.clone(),
            }];
            if depth == 1 && item.is_dir {
                if let Ok(children) = mr.list(&full).await {
                    for c in children {
                        entries.push(XmlEntry {
                            href: format!("{}/{}", rel.trim_end_matches('/'), c.name),
                            is_dir: c.is_dir,
                            size: c.size,
                            mtime: c.mtime,
                            etag: c.hash.clone(),
                        });
                    }
                }
            }
            xml_response(StatusCode::MULTI_STATUS, multistatus(&entries))
        }
        m if m.as_str() == "PROPPATCH" => {
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>{}</D:href><D:propstat><D:prop/><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>",
                xml_escape(&encode_dav_path(&rel))
            );
            xml_response(StatusCode::MULTI_STATUS, body)
        }
        m if m.as_str() == "LOCK" || m.as_str() == "UNLOCK" => {
            // pretend locking works; davfs2 should be configured with use_locks 0
            let token = HeaderValue::from_static("<opaquelocktoken:davfast-fake-lock>");
            if m.as_str() == "LOCK" {
                let body = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:prop xmlns:D=\"DAV:\"><D:lockdiscovery><D:activelock><D:locktoken><D:href>opaquelocktoken:davfast-fake-lock</D:href></D:locktoken></D:activelock></D:lockdiscovery></D:prop>";
                dav_headers(Response::builder().status(StatusCode::OK))
                    .header(header::CONTENT_TYPE, "application/xml")
                    .header("Lock-Token", token)
                    .body(Body::from(body))
                    .unwrap()
            } else {
                empty_response(StatusCode::OK)
            }
        }
        _ => text_response(StatusCode::METHOD_NOT_ALLOWED, "unsupported method"),
    }
}
