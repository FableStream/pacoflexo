//! Shared application state.

use crate::config::Config;
use crate::db::{PkgEntry, RepoIndex};
use crate::fetch::Progress;
use crate::mirrors::Mirrors;
use crate::routes::Target;
use crate::verify::Keyring;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

#[derive(Default)]
pub struct Stats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub db_hits: AtomicU64,
    pub db_fetches: AtomicU64,
    pub upstream_errors: AtomicU64,
    pub verification_failures: AtomicU64,
    pub committed: AtomicU64,
    pub scrubbed: AtomicU64,
    pub seed_hits: AtomicU64,
    pub seed_rejects: AtomicU64,
}

impl Stats {
    pub fn inc(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
}

pub struct AppState {
    pub cfg: Config,
    pub client: reqwest::Client,
    pub mirrors: Mirrors,
    /// Reloaded when the keyring file changes (archlinux-keyring upgrades).
    keyring: RwLock<Option<Arc<Keyring>>>,
    keyring_stamp: Mutex<Option<SystemTime>>,
    /// Parsed `<repo>.db` per (repo, arch).
    pub index: RwLock<HashMap<(String, String), Arc<RepoIndex>>>,
    /// Package downloads in progress, by relative path.
    pub inflight: Mutex<HashMap<String, watch::Receiver<Progress>>>,
    pub db_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pub db_fresh: Mutex<HashMap<String, Instant>>,
    /// Last client access (unix secs) per relative path.
    pub access: Mutex<HashMap<String, u64>>,
    pub stats: Stats,
    pub started: Instant,
    tmp_counter: AtomicU64,
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn keyring_stamp(keyring: &Path, revoked: &Path) -> Option<SystemTime> {
    let m = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    m(keyring).max(m(revoked))
}

impl AppState {
    /// The current keyring (None when signature checks are disabled). Reloads
    /// it first if the keyring or revoked file changed on disk.
    pub fn keyring(&self) -> Option<Arc<Keyring>> {
        let v = &self.cfg.verify;
        if v.signatures {
            let now = keyring_stamp(&v.keyring, &v.revoked);
            let mut stamp = self.keyring_stamp.lock().unwrap();
            if now.is_some() && now != *stamp {
                match Keyring::load(&v.keyring, &v.revoked) {
                    Ok(k) => {
                        tracing::info!("keyring changed on disk, reloaded");
                        *self.keyring.write().unwrap() = Some(Arc::new(k));
                        *stamp = now;
                    }
                    Err(e) => {
                        tracing::warn!("keyring changed but failed to load, keeping old one: {e}")
                    }
                }
            }
        }
        self.keyring.read().unwrap().clone()
    }

    pub fn new(cfg: Config) -> Result<AppState, String> {
        let keyring = if cfg.verify.signatures {
            Some(Arc::new(
                Keyring::load(&cfg.verify.keyring, &cfg.verify.revoked)
                    .map_err(|e| format!("keyring: {e}"))?,
            ))
        } else {
            tracing::warn!("signature verification disabled in config");
            None
        };
        let client = reqwest::Client::builder()
            .user_agent(concat!("pacoflexo/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(cfg.mirrors.connect_timeout_secs))
            .read_timeout(Duration::from_secs(cfg.mirrors.read_timeout_secs))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        for dir in [&cfg.cache_dir, &cfg.state_dir] {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let tmp = cfg.cache_dir.join(".tmp");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        let access = std::fs::read(cfg.state_dir.join("access.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let stamp = keyring_stamp(&cfg.verify.keyring, &cfg.verify.revoked);
        Ok(AppState {
            mirrors: Mirrors::new(&cfg.mirrors, &cfg.state_dir),
            cfg,
            client,
            keyring: RwLock::new(keyring),
            keyring_stamp: Mutex::new(stamp),
            index: RwLock::default(),
            inflight: Mutex::default(),
            db_locks: Mutex::default(),
            db_fresh: Mutex::default(),
            access: Mutex::new(access),
            stats: Stats::default(),
            started: Instant::now(),
            tmp_counter: AtomicU64::new(0),
        })
    }

    pub fn cache_path(&self, t: &Target) -> PathBuf {
        self.cfg.cache_dir.join(t.rel_path())
    }

    /// A fresh temp path on the cache filesystem (so `rename` is atomic).
    pub fn tmp_path(&self) -> PathBuf {
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        self.cfg
            .cache_dir
            .join(".tmp")
            .join(format!("{}-{n}.part", std::process::id()))
    }

    pub fn lookup(&self, repo: &str, arch: &str, file: &str) -> Option<PkgEntry> {
        let idx = self.index.read().unwrap();
        idx.get(&(repo.to_string(), arch.to_string()))
            .and_then(|i| i.by_filename.get(file).cloned())
    }

    pub fn set_index(&self, repo: &str, arch: &str, index: RepoIndex) {
        tracing::info!(
            repo,
            arch,
            packages = index.by_filename.len(),
            "index updated"
        );
        self.index
            .write()
            .unwrap()
            .insert((repo.to_string(), arch.to_string()), Arc::new(index));
    }

    pub fn touch(&self, t: &Target) {
        self.access.lock().unwrap().insert(t.rel_path(), unix_now());
    }

    pub fn save_access(&self) {
        let json = serde_json::to_vec(&*self.access.lock().unwrap()).unwrap_or_default();
        let path = self.cfg.state_dir.join("access.json");
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, &path)) {
            tracing::warn!("saving {}: {e}", path.display());
        }
    }

    pub fn db_lock(&self, rel: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.db_locks
            .lock()
            .unwrap()
            .entry(rel.to_string())
            .or_default()
            .clone()
    }
}
