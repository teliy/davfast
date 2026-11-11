//! End-to-end tests against an in-memory WebDAV server: upload / download /
//! incremental sync of a tree with many small files.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use davfast::dav::DavClient;
use davfast::pipeline::{download_tree, list_recursive, upload_tree, Backend, TransferOpts};

// ------------------------------------------------------- in-memory DAV server

#[derive(Default)]
struct MemDav {
    files: Mutex<HashMap<String, Vec<u8>>>,
    dirs: Mutex<HashSet<String>>,
}

const FAKE_DATE: &str = "Mon, 02 Jan 2006 15:04:05 GMT";

fn xml_entry(href: &str, is_dir: bool, size: u64, etag: Option<&str>) -> String {
    let rt = if is_dir { "<D:collection/>" } else { "" };
    let len = if is_dir {
        String::new()
    } else {
        format!("<D:getcontentlength>{}</D:getcontentlength>", size)
    };
    let et = etag
        .map(|e| format!("<D:getetag>\"etag-{}\"</D:getetag>", e))
        .unwrap_or_default();
    format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop><D:resourcetype>{}</D:resourcetype>{}{}<D:getlastmodified>{}</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
        href, rt, len, et, FAKE_DATE
    )
}

async fn mem_handler(State(st): State<Arc<MemDav>>, req: Request) -> Response {
    let method = req.method().clone();
    let depth = req
        .headers()
        .get("Depth")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1);
    let path = percent_encoding::percent_decode_str(req.uri().path())
        .decode_utf8_lossy()
        .trim_end_matches('/')
        .to_string();
    let path = if path.is_empty() { "/".to_string() } else { path };

    let is_dir_path = |p: &str, dirs: &HashSet<String>| p == "/" || dirs.contains(p);

    match method {
        m if m == Method::PUT => {
            let b = match axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024).await {
                Ok(b) => b,
                Err(_) => return Response::builder().status(413).body(Body::empty()).unwrap(),
            };
            st.files.lock().unwrap().insert(path, b.to_vec());
            Response::builder().status(StatusCode::CREATED).body(Body::empty()).unwrap()
        }
        m if m == Method::GET || m == Method::HEAD => {
            let found = st.files.lock().unwrap().get(&path).cloned();
            match found {
                Some(data) => {
                    let etag = format!("\"etag-{}\"", data.len() ^ path.len());
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_LENGTH, data.len())
                        .header(header::ETAG, etag)
                        .body(if m == Method::HEAD { Body::empty() } else { Body::from(data) })
                        .unwrap()
                }
                None => Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap(),
            }
        }
        m if m.as_str() == "MKCOL" => {
            st.dirs.lock().unwrap().insert(path);
            Response::builder().status(StatusCode::CREATED).body(Body::empty()).unwrap()
        }
        m if m == Method::DELETE => {
            let mut files = st.files.lock().unwrap();
            let prefix = format!("{}/", path);
            files.retain(|p, _| !p.starts_with(&prefix));
            let removed_file = files.remove(&path).is_some();
            let mut dirs = st.dirs.lock().unwrap();
            dirs.retain(|p| !p.starts_with(&prefix));
            let removed_dir = dirs.remove(&path);
            if removed_file || removed_dir {
                Response::builder().status(StatusCode::NO_CONTENT).body(Body::empty()).unwrap()
            } else {
                Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap()
            }
        }
        m if m.as_str() == "PROPFIND" => {
            let dirs = st.dirs.lock().unwrap();
            let files = st.files.lock().unwrap();
            if !is_dir_path(&path, &dirs) && !files.contains_key(&path) {
                return Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap();
            }
            let mut xml = String::from(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">",
            );
            let is_dir = is_dir_path(&path, &dirs);
            xml.push_str(&xml_entry(&path, is_dir, 0, None));
            if depth >= 1 && is_dir {
                let prefix = if path == "/" { "/".to_string() } else { format!("{}/", path) };
                let mut children_files: Vec<&String> = files.keys().filter(|p| p.starts_with(&prefix)).collect();
                let mut children_dirs: Vec<&String> = dirs.iter().filter(|p| p.starts_with(&prefix)).collect();
                children_files.sort();
                children_dirs.sort();
                for c in children_dirs {
                    xml.push_str(&xml_entry(c, true, 0, None));
                }
                for c in children_files {
                    let size = files.get(c).map(|v| v.len()).unwrap_or(0) as u64;
                    xml.push_str(&xml_entry(c, false, size, Some(&format!("{}", size as usize ^ c.len()))));
                }
            }
            xml.push_str("</D:multistatus>");
            Response::builder()
                .status(StatusCode::MULTI_STATUS)
                .header(header::CONTENT_TYPE, "application/xml")
                .body(Body::from(xml))
                .unwrap()
        }
        m if m == Method::OPTIONS => Response::builder()
            .status(StatusCode::OK)
            .header("DAV", "1,2")
            .body(Body::empty())
            .unwrap(),
        _ => Response::builder().status(StatusCode::METHOD_NOT_ALLOWED).body(Body::empty()).unwrap(),
    }
}

// ------------------------------------------------------------------- helpers

fn make_local_tree(root: &std::path::Path, n_dirs: usize, files_per_dir: usize) -> HashMap<String, Vec<u8>> {
    let mut expect = HashMap::new();
    for d in 0..n_dirs {
        let dir = root.join(format!("dir{:02}", d));
        std::fs::create_dir_all(&dir).unwrap();
        for f in 0..files_per_dir {
            let idx = d * files_per_dir + f;
            let len = 200 + (idx % 1800);
            let content: Vec<u8> = (0..len).map(|i| ((i + idx) % 251) as u8).collect();
            let name = format!("dir{:02}/file{:02}.bin", d, f);
            std::fs::write(root.join(&name), &content).unwrap();
            expect.insert(name, content);
        }
    }
    expect
}

// --------------------------------------------------------------------- tests

#[tokio::test]
async fn e2e_upload_download_sync_400_files() {
    let st = Arc::new(MemDav::default());
    let app = Router::new()
        .route("/", any(mem_handler))
        .route("/*path", any(mem_handler))
        .with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let base = format!("http://{}/", addr);
    let dav = DavClient::new(&base, None, None, 32).unwrap();
    let backend = Backend::Dav(dav.clone());

    let tmp = std::env::temp_dir().join(format!("davfast-src-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let expect = make_local_tree(&tmp, 10, 40); // 400 files
    assert_eq!(expect.len(), 400);

    // 1. upload
    let opts = TransferOpts {
        concurrency: 32,
        ..Default::default()
    };
    let s1 = upload_tree(&backend, &tmp, "/dav/data", &opts).await.unwrap();
    assert_eq!(s1.uploaded, 400, "uploaded");
    assert_eq!(s1.failed, 0, "failed");

    // 2. listing sees everything
    let map = list_recursive(&backend, "/dav/data", 16).await.unwrap();
    assert_eq!(map.values().filter(|i| !i.is_dir).count(), 400, "remote files");

    // 3. download and byte-compare
    let dl = std::env::temp_dir().join(format!("davfast-dl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dl);
    let s2 = download_tree(&backend, "/dav/data", &dl, &opts).await.unwrap();
    assert_eq!(s2.downloaded, 400, "downloaded");
    for (rel, content) in &expect {
        let got = std::fs::read(dl.join(rel)).unwrap_or_else(|_| panic!("missing {}", rel));
        assert_eq!(&got, content, "content {}", rel);
    }

    // 4. skip_existing: nothing changed -> all skipped by size+etag rules
    let s3 = upload_tree(
        &backend,
        &tmp,
        "/dav/data",
        &TransferOpts {
            concurrency: 32,
            skip_existing: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(s3.skipped, 400, "skipped");
    assert_eq!(s3.uploaded, 0, "re-uploaded");

    // 5. sync with cache: modify one file, delete another
    std::fs::write(tmp.join("dir03/file07.bin"), b"CHANGED CONTENT").unwrap();
    std::fs::remove_file(tmp.join("dir05/file11.bin")).unwrap();
    let s4 = upload_tree(
        &backend,
        &tmp,
        "/dav/data",
        &TransferOpts {
            concurrency: 32,
            skip_existing: true,
            delete_extras: true,
            use_cache: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // size-compare skips the 398 unchanged files; only the edited file goes up
    assert_eq!(s4.uploaded, 1, "sync up after edit");
    assert_eq!(s4.failed, 0);
    assert_eq!(s4.deleted, 1, "remote extra deleted");
    assert!(!st
        .files
        .lock()
        .unwrap()
        .contains_key("/dav/data/dir05/file11.bin"));

    // 6. second sync run with warm cache: everything skipped
    let s5 = upload_tree(
        &backend,
        &tmp,
        "/dav/data",
        &TransferOpts {
            concurrency: 32,
            skip_existing: true,
            delete_extras: true,
            use_cache: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(s5.skipped, 399, "warm-cache skipped");
    assert_eq!(s5.uploaded, 0, "warm-cache uploaded");
    assert_eq!(s5.deleted, 0);

    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&dl);
}

#[tokio::test]
async fn bench_pipeline_throughput_local() {
    // Not a strict test: measures parallel pipeline throughput on a local
    // in-memory server to prove the engine is not serialized. Network speed
    // obviously dominates with real servers.
    let st = Arc::new(MemDav::default());
    let app = Router::new()
        .route("/", any(mem_handler))
        .route("/*path", any(mem_handler))
        .with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let base = format!("http://{}/", addr);
    let dav = DavClient::new(&base, None, None, 64).unwrap();
    let backend = Backend::Dav(dav.clone());

    let tmp = std::env::temp_dir().join(format!("davfast-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let expect = make_local_tree(&tmp, 20, 50); // 1000 files

    for conc in [1usize, 32] {
        let t0 = std::time::Instant::now();
        let s = upload_tree(
            &backend,
            &tmp,
            "/dav/bench",
            &TransferOpts {
                concurrency: conc,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let elapsed = t0.elapsed().as_secs_f64();
        assert_eq!(s.uploaded, 1000);
        let rate = 1000.0 / elapsed;
        println!("concurrency {:>2}: {} files in {:.2}s ({:.0} files/s)", conc, 1000, elapsed, rate);
        if conc == 1 {
            // clean up between runs
            for rel in expect.keys() {
                let _ = st.files.lock().unwrap().remove(&format!("/dav/bench/{}", rel));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
