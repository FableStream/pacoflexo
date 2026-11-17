//! Upstream fetching.
//!
//! * Databases: fetched whole into a temp file, checked (non-empty, length,
//!   parseable for `<repo>.db`), then renamed into place. Served for
//!   `db_ttl_secs` before revalidating with `If-Modified-Since`.
//! * Packages: one download per file no matter how many clients ask
//!   (single-flight). Bytes are streamed to waiting clients as they arrive,
//!   but the file only enters the cache after size, sha256 and signature
//!   checks pass. The last byte is held back from clients until then, so a
//!   client never receives a complete-looking body that failed verification.

use crate::mirrors::Mirror;
use crate::routes::{Kind, Target};
use crate::state::{AppState, Stats};
use crate::verify;
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_LENGTH, IF_MODIFIED_SINCE, LAST_MODIFIED};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

/// Mirrors tried per request before giving up.
const MAX_TRIES: usize = 6;
const MAX_SIG_SIZE: usize = 64 * 1024;

pub enum Fetched {
    File(PathBuf),
    Bytes(Bytes),
    NotFound,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DlState {
    /// No upstream has answered yet.
    Waiting,
    Running,
    /// Verified and committed (or, when unverifiable, fully passed through).
    Done,
    Failed {
        not_found: bool,
    },
}

/// Published by a package download to everyone reading it.
#[derive(Clone, Debug)]
pub struct Progress {
    /// Attempt number; each mirror attempt writes a new temp file.
    pub generation: u64,
    pub file: Option<Arc<std::fs::File>>,
    pub total: u64,
    pub written: u64,
    pub state: DlState,
    /// Attempts whose bytes failed verification. Readers that consumed them
    /// must abort instead of continuing on the next attempt.
    pub poisoned: Vec<u64>,
}

pub enum Start {
    Cached(PathBuf),
    Inflight(watch::Receiver<Progress>),
}

fn mirrors_for(st: &AppState, first: Option<&Arc<Mirror>>) -> Vec<Arc<Mirror>> {
    let mut list = st.mirrors.snapshot();
    if let Some(f) = first {
        list.retain(|m| m.template != f.template);
        list.insert(0, f.clone());
    }
    list.truncate(MAX_TRIES);
    list
}

fn upstream_error(st: &AppState, m: &Mirror, url: &str, what: impl std::fmt::Display) {
    Stats::inc(&st.stats.upstream_errors);
    Stats::inc(&m.errors);
    tracing::warn!("{url}: {what}");
}

/// Streams a response body into a new temp file. Returns (path, length).
async fn body_to_tmp(st: &AppState, resp: reqwest::Response) -> Result<(PathBuf, u64), String> {
    let tmp = st.tmp_path();
    let res = async {
        let mut f = tokio::fs::File::create_new(&tmp)
            .await
            .map_err(|e| e.to_string())?;
        let mut n = 0u64;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("transfer: {e}"))?;
            f.write_all(&chunk).await.map_err(|e| e.to_string())?;
            n += chunk.len() as u64;
        }
        f.flush().await.map_err(|e| e.to_string())?;
        f.sync_all().await.map_err(|e| e.to_string())?;
        Ok(n)
    }
    .await;
    match res {
        Ok(n) => Ok((tmp, n)),
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

async fn commit(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::rename(tmp, dest).await
}

// ---------------------------------------------------------------- databases

pub async fn get_db(st: &AppState, t: &Target) -> Fetched {
    let rel = t.rel_path();
    let path = st.cache_path(t);
    let lock = st.db_lock(&rel);
    let _guard = lock.lock().await;

    let ttl = Duration::from_secs(st.cfg.db_ttl_secs);
    let fresh = st
        .db_fresh
        .lock()
        .unwrap()
        .get(&rel)
        .is_some_and(|at| at.elapsed() < ttl);
    if fresh && path.is_file() {
        Stats::inc(&st.stats.db_hits);
        return Fetched::File(path);
    }
    Stats::inc(&st.stats.db_fetches);
    let cached_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut not_found = false;

    for m in mirrors_for(st, None) {
        let url = m.url(&t.repo, &t.arch, &t.file);
        let mut req = st.client.get(&url);
        if let Some(mt) = cached_mtime {
            req = req.header(IF_MODIFIED_SINCE, httpdate::fmt_http_date(mt));
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                upstream_error(st, &m, &url, e);
                continue;
            }
        };
        match resp.status() {
            StatusCode::NOT_MODIFIED if path.is_file() => {
                st.db_fresh.lock().unwrap().insert(rel, Instant::now());
                return Fetched::File(path);
            }
            StatusCode::NOT_FOUND => {
                not_found = true;
                // Arch publishes no database signatures: don't ask every mirror.
                if t.kind == Kind::DbSig {
                    break;
                }
                continue;
            }
            s if s.is_success() => {}
            s => {
                upstream_error(st, &m, &url, s);
                continue;
            }
        }
        let last_modified = resp
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| httpdate::parse_http_date(v).ok());
        if let (Some(lm), Some(cm)) = (last_modified, cached_mtime)
            && lm < cm
        {
            tracing::info!("{url}: older than cached copy, trying next mirror");
            continue;
        }
        let expected_len = resp.content_length();
        let (tmp, len) = match body_to_tmp(st, resp).await {
            Ok(x) => x,
            Err(e) => {
                upstream_error(st, &m, &url, e);
                continue;
            }
        };
        if len == 0 || expected_len.is_some_and(|l| l != len) {
            upstream_error(
                st,
                &m,
                &url,
                format!("short body: {len} of {expected_len:?} bytes"),
            );
            let _ = tokio::fs::remove_file(&tmp).await;
            continue;
        }
        if t.kind == Kind::Db && t.file == format!("{}.db", t.repo) {
            let p = tmp.clone();
            let parsed = tokio::task::spawn_blocking(move || {
                std::fs::read(&p).and_then(|b| crate::db::parse_db(&b))
            })
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e)));
            match parsed {
                Ok(entries) => st.set_index(
                    &t.repo,
                    &t.arch,
                    crate::db::RepoIndex::from_entries(entries),
                ),
                Err(e) => {
                    Stats::inc(&st.stats.verification_failures);
                    upstream_error(st, &m, &url, format!("bad database: {e}"));
                    let _ = tokio::fs::remove_file(&tmp).await;
                    continue;
                }
            }
        }
        // Keep upstream's Last-Modified as mtime so client If-Modified-Since works.
        if let Ok(f) = std::fs::File::options().write(true).open(&tmp) {
            let _ = f.set_modified(last_modified.unwrap_or_else(SystemTime::now));
        }
        if let Err(e) = commit(&tmp, &path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Fetched::Failed(format!("commit {}: {e}", path.display()));
        }
        st.db_fresh.lock().unwrap().insert(rel, Instant::now());
        return Fetched::File(path);
    }
    if path.is_file() && t.kind == Kind::Db {
        tracing::warn!("{rel}: all mirrors failed, serving cached copy");
        return Fetched::File(path);
    }
    if not_found {
        Fetched::NotFound
    } else {
        Fetched::Failed("no mirror could provide the file".into())
    }
}

// --------------------------------------------------------------- signatures

/// Fetches a detached signature from upstream. Empty or unparsable files
/// (e.g. a 0-byte `.sig`) are rejected and the next mirror is tried.
pub async fn fetch_sig(
    st: &AppState,
    repo: &str,
    arch: &str,
    file: &str,
    first: Option<&Arc<Mirror>>,
) -> Result<Vec<u8>, bool> {
    let mut not_found = false;
    for m in mirrors_for(st, first) {
        let url = m.url(repo, arch, file);
        let resp = match st.client.get(&url).send().await {
            Ok(r) if r.status() == StatusCode::NOT_FOUND => {
                not_found = true;
                continue;
            }
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                upstream_error(st, &m, &url, r.status());
                continue;
            }
            Err(e) => {
                upstream_error(st, &m, &url, e);
                continue;
            }
        };
        if resp
            .content_length()
            .is_some_and(|l| l as usize > MAX_SIG_SIZE)
        {
            upstream_error(st, &m, &url, "signature too large");
            continue;
        }
        match resp.bytes().await {
            Ok(b) if verify::parse_signatures(&b).is_ok() => return Ok(b.to_vec()),
            Ok(b) => {
                Stats::inc(&st.stats.verification_failures);
                upstream_error(
                    st,
                    &m,
                    &url,
                    format!("invalid signature file ({} bytes)", b.len()),
                );
            }
            Err(e) => upstream_error(st, &m, &url, e),
        }
    }
    Err(not_found)
}

/// Writes `data` next to a cached package as `<file>.sig` (atomically).
async fn write_sidecar(st: &AppState, pkg_path: &Path, data: &[u8]) {
    let mut name = pkg_path.as_os_str().to_owned();
    name.push(".sig");
    let dest = PathBuf::from(name);
    if dest.is_file() {
        return;
    }
    let tmp = st.tmp_path();
    if tokio::fs::write(&tmp, data).await.is_ok() && commit(&tmp, &dest).await.is_ok() {
        return;
    }
    let _ = tokio::fs::remove_file(&tmp).await;
}

pub async fn get_pkg_sig(st: &AppState, t: &Target) -> Fetched {
    let path = st.cache_path(t);
    if path.is_file() {
        return Fetched::File(path);
    }
    let pkg_file = t.file.trim_end_matches(".sig");
    if let Some(sig) = st.lookup(&t.repo, &t.arch, pkg_file).and_then(|e| e.pgpsig) {
        return Fetched::Bytes(sig.into());
    }
    match fetch_sig(st, &t.repo, &t.arch, &t.file, None).await {
        Ok(sig) => {
            // Cache it only if it verifies against a package we already hold.
            let pkg_path = path.with_file_name(pkg_file);
            if let Some(kr) = &st.keyring()
                && pkg_path.is_file()
                && verify_blocking(kr, &pkg_path, &sig).await.is_ok()
            {
                write_sidecar(st, &pkg_path, &sig).await;
            }
            Fetched::Bytes(sig.into())
        }
        Err(true) => Fetched::NotFound,
        Err(false) => Fetched::Failed("no mirror could provide the signature".into()),
    }
}

pub async fn verify_blocking(
    kr: &Arc<verify::Keyring>,
    path: &Path,
    sig: &[u8],
) -> std::io::Result<String> {
    let (kr, p, s) = (kr.clone(), path.to_path_buf(), sig.to_vec());
    tokio::task::spawn_blocking(move || kr.verify_file(&p, &s))
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

// ----------------------------------------------------------------- packages

/// Joins the download of `t` or starts it. Never starts a second download of
/// a file that is already being fetched or already cached.
pub fn start_download(st: &Arc<AppState>, t: &Target) -> Start {
    let rel = t.rel_path();
    let mut map = st.inflight.lock().unwrap();
    if let Some(rx) = map.get(&rel) {
        return Start::Inflight(rx.clone());
    }
    // Checked under the lock: a finished download renames before it
    // unregisters, so a missing entry plus an existing file means "cached".
    let path = st.cache_path(t);
    if path.is_file() {
        return Start::Cached(path);
    }
    let (tx, rx) = watch::channel(Progress {
        generation: 0,
        file: None,
        total: 0,
        written: 0,
        state: DlState::Waiting,
        poisoned: vec![],
    });
    map.insert(rel.clone(), rx.clone());
    drop(map);
    let (st, t) = (st.clone(), t.clone());
    tokio::spawn(async move {
        let final_state = download_package(&st, &t, &tx).await;
        tx.send_modify(|p| p.state = final_state);
        st.inflight.lock().unwrap().remove(&rel);
    });
    Start::Inflight(rx)
}

enum Attempt {
    Ok,
    /// Transport problem: bytes so far are fine, readers may continue on the
    /// next attempt.
    Retry(String),
    /// Bytes are wrong: readers that consumed them must abort.
    Poison(String),
}

async fn download_package(st: &AppState, t: &Target, tx: &watch::Sender<Progress>) -> DlState {
    let expected = st.lookup(&t.repo, &t.arch, &t.file);
    let dest = st.cache_path(t);
    let mut generation = 0;
    let mut all_404 = true;

    for m in mirrors_for(st, None) {
        let url = m.url(&t.repo, &t.arch, &t.file);
        let resp = match st.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                all_404 = false;
                upstream_error(st, &m, &url, e);
                continue;
            }
        };
        if resp.status() == StatusCode::NOT_FOUND {
            continue;
        }
        all_404 = false;
        if !resp.status().is_success() {
            upstream_error(st, &m, &url, resp.status());
            continue;
        }
        let header_len = resp
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
        let total = match (&expected, header_len) {
            (Some(e), Some(l)) if l == e.csize => l,
            (Some(e), None) => e.csize,
            (None, Some(l)) if l > 0 => l,
            (e, l) => {
                Stats::inc(&st.stats.verification_failures);
                upstream_error(
                    st,
                    &m,
                    &url,
                    format!(
                        "size mismatch: Content-Length {l:?}, sync db {:?}",
                        e.as_ref().map(|e| e.csize)
                    ),
                );
                continue;
            }
        };
        generation += 1;
        let tmp = st.tmp_path();
        let result = attempt(st, t, &m, resp, &tmp, &dest, total, generation, tx).await;
        match result {
            Attempt::Ok => return DlState::Done,
            Attempt::Retry(e) => upstream_error(st, &m, &url, e),
            Attempt::Poison(e) => {
                Stats::inc(&st.stats.verification_failures);
                upstream_error(st, &m, &url, format!("verification failed: {e}"));
                tx.send_modify(|p| p.poisoned.push(generation));
            }
        }
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    DlState::Failed { not_found: all_404 }
}

#[allow(clippy::too_many_arguments)]
async fn attempt(
    st: &AppState,
    t: &Target,
    mirror: &Arc<Mirror>,
    resp: reqwest::Response,
    tmp: &Path,
    dest: &Path,
    total: u64,
    generation: u64,
    tx: &watch::Sender<Progress>,
) -> Attempt {
    let file = match std::fs::File::create_new(tmp) {
        Ok(f) => f,
        Err(e) => return Attempt::Retry(format!("temp file: {e}")),
    };
    let reader = match file.try_clone() {
        Ok(f) => Arc::new(f),
        Err(e) => return Attempt::Retry(format!("temp file: {e}")),
    };
    let mut out = tokio::fs::File::from_std(file);
    tx.send_modify(|p| {
        p.generation = generation;
        p.file = Some(reader);
        p.total = total;
        p.written = 0;
        p.state = DlState::Running;
    });

    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => return Attempt::Retry(format!("transfer after {written} bytes: {e}")),
        };
        if written + chunk.len() as u64 > total {
            return Attempt::Poison(format!("body longer than {total} bytes"));
        }
        // tokio's File completes writes in the background; flush so the
        // bytes are readable by other handles before we announce them.
        if let Err(e) = async {
            out.write_all(&chunk).await?;
            out.flush().await
        }
        .await
        {
            return Attempt::Retry(format!("write: {e}"));
        }
        hasher.update(&chunk);
        written += chunk.len() as u64;
        tx.send_modify(|p| p.written = written);
    }
    if written != total {
        return Attempt::Retry(format!("truncated: {written} of {total} bytes"));
    }
    if let Err(e) = out.sync_all().await {
        return Attempt::Retry(format!("sync: {e}"));
    }
    drop(out);

    let expected = st.lookup(&t.repo, &t.arch, &t.file);
    if let Some(e) = &expected {
        let got = hex::encode(hasher.finalize());
        if got != e.sha256 {
            return Attempt::Poison(format!("sha256 {got} != sync db {}", e.sha256));
        }
    }

    let Some(kr) = &st.keyring() else {
        if expected.is_some() {
            return finish(st, tmp, dest, None, tx).await;
        }
        // Not in any sync db and no keyring: pass through, never cache.
        tracing::info!("{}: unverifiable, not caching", t.rel_path());
        tx.send_modify(|p| p.state = DlState::Done);
        return Attempt::Ok;
    };
    let sidecar = st.cfg.cache_dir.join(format!("{}.sig", t.rel_path()));
    let sig = match expected.and_then(|e| e.pgpsig) {
        Some(s) => s,
        None => match tokio::fs::read(&sidecar).await {
            Ok(s) if !s.is_empty() => s,
            _ => match fetch_sig(
                st,
                &t.repo,
                &t.arch,
                &format!("{}.sig", t.file),
                Some(mirror),
            )
            .await
            {
                Ok(s) => s,
                Err(_) => return Attempt::Poison("no valid signature available".into()),
            },
        },
    };
    match verify_blocking(kr, tmp, &sig).await {
        Ok(signer) => {
            tracing::debug!("{}: signed by {signer}", t.rel_path());
            finish(st, tmp, dest, Some(&sig), tx).await
        }
        Err(e) => Attempt::Poison(format!("signature: {e}")),
    }
}

async fn finish(
    st: &AppState,
    tmp: &Path,
    dest: &Path,
    sig: Option<&[u8]>,
    tx: &watch::Sender<Progress>,
) -> Attempt {
    if let Some(sig) = sig {
        write_sidecar(st, dest, sig).await;
    }
    if let Err(e) = commit(tmp, dest).await {
        return Attempt::Retry(format!("commit: {e}"));
    }
    Stats::inc(&st.stats.committed);
    tracing::info!("cached {}", dest.display());
    tx.send_modify(|p| p.state = DlState::Done);
    Attempt::Ok
}

/// Waits for a download to start (or fail) and returns a body stream for it.
pub async fn wait_started(rx: &mut watch::Receiver<Progress>) -> Result<Progress, bool> {
    loop {
        let p = rx.borrow_and_update().clone();
        match p.state {
            DlState::Running | DlState::Done if p.file.is_some() => return Ok(p),
            DlState::Failed { not_found } => return Err(not_found),
            _ => {}
        }
        if rx.changed().await.is_err() {
            let p = rx.borrow().clone();
            return match p.state {
                DlState::Done if p.file.is_some() => Ok(p),
                DlState::Failed { not_found } => Err(not_found),
                _ => Err(false),
            };
        }
    }
}

pub struct Reader {
    rx: watch::Receiver<Progress>,
    generation: u64,
    file: Arc<std::fs::File>,
    total: u64,
    sent: u64,
}

impl Reader {
    pub fn new(rx: watch::Receiver<Progress>, start: Progress) -> Reader {
        Reader {
            rx,
            generation: start.generation,
            file: start.file.expect("started download has a file"),
            total: start.total,
            sent: 0,
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// Next chunk for the client. `None` means the body is complete.
    pub async fn next_chunk(&mut self) -> Option<std::io::Result<Bytes>> {
        let fail = |m: &str| Some(Err(std::io::Error::other(m.to_string())));
        loop {
            if self.sent == self.total {
                return None;
            }
            let p = self.rx.borrow_and_update().clone();
            if p.poisoned.contains(&self.generation) {
                return fail("upstream data failed verification");
            }
            if let DlState::Failed { .. } = p.state {
                return fail("download failed on all mirrors");
            }
            if p.generation != self.generation {
                // Previous attempt broke off; continue from the same offset
                // in the next attempt (same file name = same content).
                match &p.file {
                    Some(f) if p.total == self.total => {
                        self.generation = p.generation;
                        self.file = f.clone();
                    }
                    _ => return fail("upstream size changed"),
                }
            }
            let available = match p.state {
                DlState::Done => p.total,
                _ => p.written.min(self.total.saturating_sub(1)),
            };
            if available > self.sent {
                let n = (available - self.sent).min(256 * 1024) as usize;
                let (file, off) = (self.file.clone(), self.sent);
                let read = tokio::task::spawn_blocking(move || {
                    use std::os::unix::fs::FileExt;
                    let mut buf = vec![0u8; n];
                    file.read_exact_at(&mut buf, off).map(|_| buf)
                })
                .await;
                return match read {
                    Ok(Ok(buf)) => {
                        self.sent += n as u64;
                        Some(Ok(Bytes::from(buf)))
                    }
                    Ok(Err(e)) => Some(Err(e)),
                    Err(e) => Some(Err(std::io::Error::other(e))),
                };
            }
            if self.rx.changed().await.is_err() {
                // Downloader finished; the final state is in the channel.
                let p = self.rx.borrow().clone();
                if p.state != DlState::Done || p.generation != self.generation {
                    return fail("download ended early");
                }
            }
        }
    }
}
