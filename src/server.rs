//! HTTP front end: routing, cache hits (with Range support), misses streamed
//! from the single-flight download, and `/status`.

use crate::fetch::{self, Fetched, Reader, Start};
use crate::routes::{self, Kind, Target};
use crate::state::{AppState, Stats};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tower::ServiceExt;

pub fn router(st: Arc<AppState>) -> Router {
    Router::new()
        .route("/status", get(status))
        .fallback(handle)
        .with_state(st)
}

fn plain(code: StatusCode, msg: &str) -> Response {
    (code, format!("{msg}\n")).into_response()
}

async fn serve_file(path: PathBuf, req: Request) -> Response {
    match tower_http::services::ServeFile::new(path)
        .oneshot(req)
        .await
    {
        Ok(r) => r.map(Body::new),
        Err(e) => plain(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

fn octets(data: bytes::Bytes) -> Response {
    let mut r = Response::new(Body::from(data));
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    r
}

async fn handle(State(st): State<Arc<AppState>>, req: Request) -> Response {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    let Some(t) = routes::parse(req.uri().path(), &st.cfg.pacoloco_prefixes) else {
        return plain(StatusCode::NOT_FOUND, "not found");
    };
    let fetched = match t.kind {
        Kind::Db | Kind::DbSig => fetch::get_db(&st, &t).await,
        Kind::PackageSig => fetch::get_pkg_sig(&st, &t).await,
        Kind::Package => return package(st, t, req).await,
        Kind::Other => return passthrough(&st, &t).await,
    };
    match fetched {
        Fetched::File(p) => serve_file(p, req).await,
        Fetched::Bytes(b) => octets(b),
        Fetched::NotFound => plain(StatusCode::NOT_FOUND, "not found upstream"),
        Fetched::Failed(e) => plain(StatusCode::BAD_GATEWAY, &e),
    }
}

async fn package(st: Arc<AppState>, t: Target, req: Request) -> Response {
    st.touch(&t);
    let path = st.cache_path(&t);
    if path.is_file() {
        Stats::inc(&st.stats.hits);
        return serve_file(path, req).await;
    }
    let mut rx = match fetch::start_download(&st, &t) {
        Start::Cached(p) => {
            Stats::inc(&st.stats.hits);
            return serve_file(p, req).await;
        }
        Start::Inflight(rx) => rx,
    };
    Stats::inc(&st.stats.misses);
    let started = match fetch::wait_started(&mut rx).await {
        Ok(p) => p,
        Err(true) => return plain(StatusCode::NOT_FOUND, "not found upstream"),
        Err(false) => return plain(StatusCode::BAD_GATEWAY, "no mirror could provide the file"),
    };
    // A Range request on an in-flight file gets the full body (RFC 9110 allows
    // ignoring Range); pacman simply restarts its download.
    let reader = Reader::new(rx, started);
    let len = reader.total();
    let body = futures_util::stream::unfold(reader, |mut r| async move {
        r.next_chunk().await.map(|c| (c, r))
    });
    let mut resp = Response::new(Body::from_stream(body));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    resp
}

/// Files that are neither databases nor packages: proxied, never cached.
async fn passthrough(st: &AppState, t: &Target) -> Response {
    for m in st.mirrors.snapshot().iter().take(3) {
        let url = m.url(&t.repo, &t.arch, &t.file);
        match st.client.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                let len = r.content_length();
                let mut resp = Response::new(Body::from_stream(r.bytes_stream()));
                if let Some(l) = len {
                    resp.headers_mut()
                        .insert(header::CONTENT_LENGTH, HeaderValue::from(l));
                }
                return resp;
            }
            Ok(r) if r.status() == StatusCode::NOT_FOUND => {
                return plain(StatusCode::NOT_FOUND, "not found upstream");
            }
            _ => continue,
        }
    }
    plain(StatusCode::BAD_GATEWAY, "no mirror could provide the file")
}

async fn status(State(st): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let dir = st.cfg.cache_dir.clone();
    let (files, bytes) = tokio::task::spawn_blocking(move || dir_usage(&dir))
        .await
        .unwrap_or((0, 0));
    let s = &st.stats;
    let ld = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    let mirrors: Vec<_> = st
        .mirrors
        .snapshot()
        .iter()
        .map(|m| {
            serde_json::json!({
                "url": m.template,
                "latency_ms": m.latency_ms,
                "errors": ld(&m.errors),
            })
        })
        .collect();
    let repos: Vec<_> = st
        .index
        .read()
        .unwrap()
        .iter()
        .map(|((r, a), i)| serde_json::json!({"repo": r, "arch": a, "packages": i.by_filename.len()}))
        .collect();
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": st.started.elapsed().as_secs(),
        "cache": { "dir": st.cfg.cache_dir, "files": files, "bytes": bytes },
        "hits": ld(&s.hits),
        "misses": ld(&s.misses),
        "db_hits": ld(&s.db_hits),
        "db_fetches": ld(&s.db_fetches),
        "committed": ld(&s.committed),
        "upstream_errors": ld(&s.upstream_errors),
        "verification_failures": ld(&s.verification_failures),
        "scrubbed": ld(&s.scrubbed),
        "inflight": st.inflight.lock().unwrap().len(),
        "signing_keys": st.keyring().map(|k| k.len()),
        "repos": repos,
        "mirrors": mirrors,
    }))
}

fn dir_usage(dir: &std::path::Path) -> (u64, u64) {
    let mut acc = (0, 0);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return acc;
    };
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            let (f, b) = dir_usage(&e.path());
            acc = (acc.0 + f, acc.1 + b);
        } else {
            acc = (acc.0 + 1, acc.1 + md.len());
        }
    }
    acc
}
