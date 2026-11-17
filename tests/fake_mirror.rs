//! End-to-end tests against two local fake mirrors: "bad" (listed first) serves
//! truncated, corrupted and 0-byte files; "good" serves correct ones.

#![allow(clippy::disallowed_names)]

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use pacoflexo::config::{Config, ScanMode};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ARCH_DIR: &str = "core/os/x86_64";
const FOO: &str = "foo-1.0-1-x86_64.pkg.tar.zst";
const BAR: &str = "bar-2.0-1-x86_64.pkg.tar.zst";
const QUX: &str = "qux-1.0-1-x86_64.pkg.tar.zst";
const DROP: &str = "drop-1.0-1-x86_64.pkg.tar.zst";
const EVIL: &str = "evil-1.0-1-x86_64.pkg.tar.zst";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

#[derive(Clone)]
enum Reply {
    Ok(Vec<u8>),
    /// Announces the full length, sends `cut` bytes, then breaks the connection.
    Broken(Vec<u8>, usize),
    NotFound,
}

#[derive(Clone, Default)]
struct Mirror {
    files: Arc<HashMap<String, Reply>>,
    hits: Arc<Mutex<HashMap<String, usize>>>,
}

impl Mirror {
    fn hits(&self, file: &str) -> usize {
        *self.hits.lock().unwrap().get(file).unwrap_or(&0)
    }
}

async fn serve_mirror(State(m): State<Mirror>, req: Request) -> Response {
    let file = req.uri().path().rsplit('/').next().unwrap().to_string();
    *m.hits.lock().unwrap().entry(file.clone()).or_default() += 1;
    let path = req.uri().path().trim_start_matches('/').to_string();
    match m.files.get(&path).cloned().unwrap_or(Reply::NotFound) {
        Reply::Ok(b) => b.into_response(),
        Reply::Broken(b, cut) => {
            let full = b.len();
            let head = b[..cut].to_vec();
            let stream = futures_util::stream::unfold(0, move |step| {
                let head = head.clone();
                async move {
                    match step {
                        0 => Some((Ok(head), 1)),
                        1 => {
                            // Let the headers and first half reach the proxy.
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            Some((Err(std::io::Error::other("connection lost")), 2))
                        }
                        _ => None,
                    }
                }
            });
            let mut r = Response::new(Body::from_stream(stream));
            r.headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from(full));
            r
        }
        Reply::NotFound => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn spawn(router: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    format!("http://{addr}")
}

async fn spawn_mirror(files: HashMap<String, Reply>) -> (Mirror, String) {
    let m = Mirror {
        files: Arc::new(files),
        ..Default::default()
    };
    let url = spawn(Router::new().fallback(serve_mirror).with_state(m.clone())).await;
    (m, url)
}

fn desc(name: &str, file: &str, data: &[u8]) -> String {
    format!(
        "%FILENAME%\n{file}\n\n%NAME%\n{name}\n\n%VERSION%\n1.0-1\n\n%CSIZE%\n{}\n\n%SHA256SUM%\n{}\n\n",
        data.len(),
        hex::encode(Sha256::digest(data))
    )
}

/// core.db listing foo, qux and drop (all with foo's content). bar and evil
/// are deliberately absent ("old versions").
fn core_db() -> Vec<u8> {
    let foo = fixture(FOO);
    let mut b = tar::Builder::new(Vec::new());
    for (name, file) in [("foo", FOO), ("qux", QUX), ("drop", DROP)] {
        let text = desc(name, file, &foo);
        let mut h = tar::Header::new_gnu();
        h.set_size(text.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, format!("{name}-1.0-1/desc"), text.as_bytes())
            .unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&b.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

fn p(file: &str) -> String {
    format!("{ARCH_DIR}/{file}")
}

struct Env {
    bad: Mirror,
    good: Mirror,
    cfg: Config,
    _dir: tempfile::TempDir,
}

async fn env() -> Env {
    let foo = fixture(FOO);
    let foo_sig = fixture(&format!("{FOO}.sig"));
    let mut corrupt = foo.clone();
    corrupt[1234] ^= 0xff;

    let mut good = HashMap::new();
    good.insert(p("core.db"), Reply::Ok(core_db()));
    for f in [FOO, QUX, DROP] {
        good.insert(p(f), Reply::Ok(foo.clone()));
        good.insert(p(&format!("{f}.sig")), Reply::Ok(foo_sig.clone()));
    }
    for f in [BAR, EVIL] {
        good.insert(p(f), Reply::Ok(fixture(f)));
        good.insert(
            p(&format!("{f}.sig")),
            Reply::Ok(fixture(&format!("{f}.sig"))),
        );
    }

    let mut bad = good.clone();
    // Truncated file, served as if complete.
    bad.insert(p(FOO), Reply::Ok(foo[..100_000].to_vec()));
    // 0-byte signatures.
    bad.insert(p(&format!("{FOO}.sig")), Reply::Ok(vec![]));
    bad.insert(p(&format!("{BAR}.sig")), Reply::Ok(vec![]));
    // Same size, wrong bytes.
    bad.insert(p(QUX), Reply::Ok(corrupt));
    // Connection drops half way.
    bad.insert(p(DROP), Reply::Broken(foo.clone(), 70_000));

    let (bad, bad_url) = spawn_mirror(bad).await;
    let (good, good_url) = spawn_mirror(good).await;

    let dir = tempfile::tempdir().unwrap();
    let fx = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut cfg = Config {
        cache_dir: dir.path().join("cache"),
        state_dir: dir.path().join("state"),
        repos: vec!["core".into()],
        startup_scan: ScanMode::Full,
        ..Default::default()
    };
    cfg.mirrors.urls = vec![format!("{bad_url}/"), format!("{good_url}/")];
    cfg.mirrors.use_status_json = false;
    cfg.mirrors.latency_rank = false;
    cfg.verify.keyring = fx.join("keyring.gpg");
    cfg.verify.revoked = fx.join("revoked");
    Env {
        bad,
        good,
        cfg,
        _dir: dir,
    }
}

async fn start_proxy(cfg: Config) -> (Arc<pacoflexo::state::AppState>, String) {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let st = pacoflexo::startup(cfg).await.unwrap();
    let url = spawn(pacoflexo::server::router(st.clone())).await;
    (st, url)
}

fn cached(cfg: &Config, file: &str) -> Option<Vec<u8>> {
    std::fs::read(cfg.cache_dir.join(ARCH_DIR).join(file)).ok()
}

async fn wait_cached(cfg: &Config, file: &str) -> Vec<u8> {
    for _ in 0..100 {
        if let Some(b) = cached(cfg, file) {
            return b;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{file} never got cached");
}

fn assert_no_empty_files(dir: &Path) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let md = e.metadata().unwrap();
        if md.is_dir() {
            assert_no_empty_files(&e.path());
        } else {
            assert!(md.len() > 0, "empty file cached: {}", e.path().display());
            assert!(
                !e.path().to_string_lossy().contains("/.tmp/"),
                "leftover temp file {}",
                e.path().display()
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn verifies_before_caching_and_falls_back() {
    let env = env().await;
    let (st, proxy) = start_proxy(env.cfg.clone()).await;
    let client = reqwest::Client::new();
    let get = |path: String| client.get(format!("{proxy}{path}")).send();
    let foo = fixture(FOO);

    // Database signatures: upstream 404 is passed through, nothing cached.
    let r = get(format!("/{ARCH_DIR}/core.db.sig")).await.unwrap();
    assert_eq!(r.status(), 404);
    assert!(cached(&env.cfg, "core.db.sig").is_none());
    assert!(cached(&env.cfg, "core.db").is_some());

    // Truncated package on the first mirror: rejected by size, second mirror used.
    let r = get(format!("/repo/archlinux/{ARCH_DIR}/{FOO}"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap(), foo);
    assert_eq!(env.bad.hits(FOO), 1);
    assert_eq!(env.good.hits(FOO), 1);
    assert_eq!(wait_cached(&env.cfg, FOO).await, foo);

    // Second request is a hit; no new upstream traffic.
    let r = get(format!("/{ARCH_DIR}/{FOO}")).await.unwrap();
    assert_eq!(r.bytes().await.unwrap(), foo);
    assert_eq!(env.good.hits(FOO), 1);

    // Range request on the cached file.
    let r = client
        .get(format!("{proxy}/{ARCH_DIR}/{FOO}"))
        .header("Range", "bytes=100-199")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.bytes().await.unwrap(), foo[100..200]);

    // The package signature comes from the good mirror, never the 0-byte one.
    let r = get(format!("/{ARCH_DIR}/{FOO}.sig")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap(), fixture(&format!("{FOO}.sig")));

    // Package not in the db: Content-Length + signature. The bad mirror's
    // 0-byte .sig is rejected and the good mirror's signature is used.
    let r = get(format!("/{ARCH_DIR}/{BAR}")).await.unwrap();
    assert_eq!(r.bytes().await.unwrap(), fixture(BAR));
    assert_eq!(wait_cached(&env.cfg, BAR).await, fixture(BAR));
    assert_eq!(env.bad.hits(&format!("{BAR}.sig")), 1);
    assert_eq!(env.good.hits(&format!("{BAR}.sig")), 1);
    assert_eq!(
        cached(&env.cfg, &format!("{BAR}.sig")).unwrap(),
        fixture(&format!("{BAR}.sig"))
    );

    // Connection dropped mid-transfer: the client still gets the full file,
    // resumed from the next mirror.
    let r = get(format!("/{ARCH_DIR}/{DROP}")).await.unwrap();
    assert_eq!(r.bytes().await.unwrap(), foo);
    assert_eq!(wait_cached(&env.cfg, DROP).await, foo);

    // Same size, wrong bytes: the client that received them gets a broken
    // body (never a complete bad file); the cache gets the good copy.
    let failures_before = st
        .stats
        .verification_failures
        .load(std::sync::atomic::Ordering::Relaxed);
    let r = get(format!("/{ARCH_DIR}/{QUX}")).await.unwrap();
    assert!(r.bytes().await.is_err(), "corrupted body must not complete");
    assert_eq!(wait_cached(&env.cfg, QUX).await, foo);
    let r = get(format!("/{ARCH_DIR}/{QUX}")).await.unwrap();
    assert_eq!(r.bytes().await.unwrap(), foo);
    assert!(
        st.stats
            .verification_failures
            .load(std::sync::atomic::Ordering::Relaxed)
            > failures_before
    );

    // Signed by a revoked key: never cached, never delivered complete.
    let r = get(format!("/{ARCH_DIR}/{EVIL}")).await.unwrap();
    let body = r.bytes().await;
    assert!(body.is_err() || body.unwrap().len() < fixture(EVIL).len());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(cached(&env.cfg, EVIL).is_none());

    // Unknown package: 404 from all mirrors.
    let r = get(format!("/{ARCH_DIR}/nope-1-1-x86_64.pkg.tar.zst"))
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // Status endpoint.
    let s: serde_json::Value = get("/status".into()).await.unwrap().json().await.unwrap();
    assert!(s["hits"].as_u64().unwrap() >= 2);
    assert!(s["verification_failures"].as_u64().unwrap() >= 3);

    assert_no_empty_files(&env.cfg.cache_dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_requests_share_one_download() {
    let env = env().await;
    let (_st, proxy) = start_proxy(env.cfg.clone()).await;
    let client = reqwest::Client::new();
    let reqs = (0..8).map(|_| {
        let c = client.clone();
        let url = format!("{proxy}/{ARCH_DIR}/{BAR}");
        async move { c.get(url).send().await.unwrap().bytes().await.unwrap() }
    });
    for body in futures_util::future::join_all(reqs).await {
        assert_eq!(body, fixture(BAR));
    }
    assert_eq!(env.bad.hits(BAR) + env.good.hits(BAR), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_scrub_drops_bad_files() {
    let env = env().await;
    let dir = env.cfg.cache_dir.join(ARCH_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    let foo = fixture(FOO);
    std::fs::write(dir.join(FOO), &foo[..1000]).unwrap(); // short
    std::fs::write(dir.join(QUX), &foo).unwrap(); // good
    let mut wrong = foo.clone();
    wrong[0] ^= 1;
    std::fs::write(dir.join(DROP), &wrong).unwrap(); // bad sha256
    std::fs::write(dir.join(BAR), fixture(BAR)).unwrap(); // unlisted, no sig
    std::fs::write(dir.join(EVIL), fixture(EVIL)).unwrap(); // unlisted, revoked sig
    std::fs::write(
        dir.join(format!("{EVIL}.sig")),
        fixture(&format!("{EVIL}.sig")),
    )
    .unwrap();
    std::fs::write(dir.join(format!("{QUX}.sig")), b"").unwrap(); // 0-byte sig
    std::fs::write(dir.join("stray.part"), b"x").unwrap();

    let (_st, _proxy) = start_proxy(env.cfg.clone()).await;
    let mut left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, vec!["core.db".to_string(), QUX.to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn keyring_is_reloaded_when_it_changes() {
    let mut env = env().await;
    let revoked = env.cfg.state_dir.with_file_name("revoked");
    // Start with the good packager key revoked (the other key stays usable).
    std::fs::write(&revoked, "FEBA0497432DC1B680BB3AFDE3254378DFF0D791\n").unwrap();
    env.cfg.verify.revoked = revoked.clone();
    let (_st, proxy) = start_proxy(env.cfg.clone()).await;
    let url = format!("{proxy}/{ARCH_DIR}/{BAR}");

    let r = reqwest::get(&url).await.unwrap();
    assert!(
        r.bytes().await.is_err(),
        "must fail while the signer is revoked"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(cached(&env.cfg, BAR).is_none());

    // archlinux-keyring upgrade: the revoked list changes on disk.
    std::fs::write(&revoked, "4E64CB5EB7140DAEA8B53CDF0AE86A508BF4D07F\n").unwrap();
    let later = std::time::SystemTime::now() + Duration::from_secs(10);
    std::fs::File::options()
        .write(true)
        .open(&revoked)
        .unwrap()
        .set_modified(later)
        .unwrap();

    let r = reqwest::get(&url).await.unwrap();
    assert_eq!(r.bytes().await.unwrap(), fixture(BAR));
    assert_eq!(wait_cached(&env.cfg, BAR).await, fixture(BAR));
}
