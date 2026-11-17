//! Startup scrub, retention and prefetch.

use crate::config::ScanMode;
use crate::db::RepoIndex;
use crate::fetch::{self, DlState, Start};
use crate::routes::{self, Kind, Target};
use crate::state::{AppState, Stats, unix_now};
use crate::vercmp::vercmp;
use crate::verify;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `<cache>/<repo>/os/<arch>` directories that exist.
fn repo_dirs(cache: &Path) -> Vec<(String, String, PathBuf)> {
    let mut out = vec![];
    let Ok(repos) = std::fs::read_dir(cache) else {
        return out;
    };
    for r in repos.flatten() {
        let repo = r.file_name().to_string_lossy().into_owned();
        if repo.starts_with('.') {
            continue;
        }
        let Ok(arches) = std::fs::read_dir(r.path().join("os")) else {
            continue;
        };
        for a in arches.flatten() {
            if a.path().is_dir() {
                out.push((
                    repo.clone(),
                    a.file_name().to_string_lossy().into_owned(),
                    a.path(),
                ));
            }
        }
    }
    out
}

fn files_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Loads the indexes of all cached `<repo>.db` files.
pub fn load_indexes(st: &AppState) {
    for (repo, arch, dir) in repo_dirs(&st.cfg.cache_dir) {
        let db = dir.join(format!("{repo}.db"));
        match std::fs::read(&db).and_then(|b| crate::db::parse_db(&b)) {
            Ok(entries) => st.set_index(&repo, &arch, RepoIndex::from_entries(entries)),
            Err(e) if db.exists() => {
                tracing::warn!("{}: {e}; removing", db.display());
                let _ = std::fs::remove_file(&db);
            }
            Err(_) => {}
        }
    }
}

/// Refreshes the databases of the configured repos (respecting the TTL).
pub async fn refresh_dbs(st: &AppState) {
    for repo in &st.cfg.repos {
        for arch in &st.cfg.arches {
            let t = Target {
                repo: repo.clone(),
                arch: arch.clone(),
                file: format!("{repo}.db"),
                kind: Kind::Db,
            };
            if let fetch::Fetched::Failed(e) = fetch::get_db(st, &t).await {
                tracing::warn!("refreshing {}: {e}", t.rel_path());
            }
        }
    }
}

fn remove(st: &AppState, path: &Path, why: &str) {
    tracing::warn!("scrub: removing {} ({why})", path.display());
    Stats::inc(&st.stats.scrubbed);
    let _ = std::fs::remove_file(path);
}

/// Drops every cached file that cannot be shown to be good: 0-byte files,
/// packages whose size/sha256 disagree with the current sync db, packages not
/// in any db without a verifying signature, and stray files.
pub fn scrub(st: &AppState, mode: ScanMode) {
    if mode == ScanMode::Off {
        return;
    }
    let full = mode == ScanMode::Full;
    for (repo, arch, dir) in repo_dirs(&st.cfg.cache_dir) {
        let files = files_in(&dir);
        let present: HashSet<&str> = files.iter().map(|s| s.as_str()).collect();
        for f in &files {
            let path = dir.join(f);
            let len = path.metadata().map(|m| m.len()).unwrap_or(0);
            if len == 0 {
                remove(st, &path, "empty");
                continue;
            }
            match routes::classify(f) {
                Kind::Db | Kind::DbSig => {}
                Kind::Other => remove(st, &path, "not a package, signature or database"),
                Kind::PackageSig => {
                    let pkg = f.trim_end_matches(".sig");
                    if !present.contains(pkg) {
                        remove(st, &path, "signature without package");
                    } else if std::fs::read(&path)
                        .map_err(|e| e.to_string())
                        .and_then(|b| verify::parse_signatures(&b).map_err(|e| e.to_string()))
                        .is_err()
                    {
                        remove(st, &path, "unparsable signature");
                    }
                }
                Kind::Package => {
                    if let Err(why) = check_package(st, &repo, &arch, &path, f, len, full) {
                        remove(st, &path, &why);
                        let _ = std::fs::remove_file(dir.join(format!("{f}.sig")));
                    }
                }
            }
        }
    }
    tracing::info!(
        removed = st.stats.scrubbed.load(std::sync::atomic::Ordering::Relaxed),
        "cache scrub finished"
    );
}

fn check_package(
    st: &AppState,
    repo: &str,
    arch: &str,
    path: &Path,
    file: &str,
    len: u64,
    full: bool,
) -> Result<(), String> {
    if let Some(e) = st.lookup(repo, arch, file) {
        if len != e.csize {
            return Err(format!("size {len} != sync db {}", e.csize));
        }
        if full {
            let (_, sha) = verify::sha256_file(path).map_err(|e| e.to_string())?;
            if sha != e.sha256 {
                return Err(format!("sha256 {sha} != sync db {}", e.sha256));
            }
        }
        return Ok(());
    }
    // Not in a current db (older version): needs a verifying signature.
    let sig = std::fs::read(format!("{}.sig", path.display()))
        .map_err(|_| "not in sync db and no signature".to_string())?;
    match (&st.keyring, full) {
        (Some(kr), true) => kr
            .verify_file(path, &sig)
            .map(|_| ())
            .map_err(|e| format!("signature: {e}")),
        (None, true) => Err("not in sync db and signatures disabled".into()),
        (_, false) => verify::parse_signatures(&sig)
            .map(|_| ())
            .map_err(|e| e.to_string()),
    }
}

/// Files (package file names) to delete for one package name, given the
/// versions in the cache and the current version in the sync db: keep
/// everything at least as new as the current version plus `keep_prev`
/// older ones. Without a current version the newest `max(keep_prev, 1)` stay.
pub fn select_removals(
    cached: &[(String, String)],
    current: Option<&str>,
    keep_prev: usize,
) -> Vec<String> {
    let mut v: Vec<&(String, String)> = cached.iter().collect();
    v.sort_by(|a, b| vercmp(&b.1, &a.1));
    let keep = match current {
        Some(cur) => {
            let newer = v
                .iter()
                .take_while(|(_, ver)| vercmp(ver, cur).is_ge())
                .count();
            newer + keep_prev
        }
        None => keep_prev.max(1),
    };
    v.into_iter().skip(keep).map(|(f, _)| f.clone()).collect()
}

pub fn retention(st: &AppState) {
    let keep_prev = st.cfg.retention.keep_versions;
    let max_idle = st.cfg.retention.unaccessed_days * 86400;
    let now = unix_now();
    let access = st.access.lock().unwrap().clone();
    let inflight: HashSet<String> = st.inflight.lock().unwrap().keys().cloned().collect();
    let mut removed = 0;
    for (repo, arch, dir) in repo_dirs(&st.cfg.cache_dir) {
        let idx = st
            .index
            .read()
            .unwrap()
            .get(&(repo.clone(), arch.clone()))
            .cloned();
        let current = idx.as_ref().map(|i| i.current_by_name());
        let mut by_name: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for f in files_in(&dir) {
            if routes::classify(&f) != Kind::Package {
                continue;
            }
            if let Some((name, ver)) = routes::split_pkg_filename(&f) {
                by_name
                    .entry(name.to_string())
                    .or_default()
                    .push((f.clone(), ver.to_string()));
            }
        }
        for (name, files) in by_name {
            let cur = current
                .as_ref()
                .and_then(|c| c.get(name.as_str()))
                .map(|e| e.version.as_str());
            let mut doomed = select_removals(&files, cur, keep_prev);
            if max_idle > 0 {
                for (f, _) in &files {
                    let rel = format!("{repo}/os/{arch}/{f}");
                    let last = access
                        .get(&rel)
                        .copied()
                        .unwrap_or_else(|| mtime_secs(&dir.join(f)).unwrap_or(now));
                    if now.saturating_sub(last) > max_idle && !doomed.contains(f) {
                        doomed.push(f.clone());
                    }
                }
            }
            for f in doomed {
                let rel = format!("{repo}/os/{arch}/{f}");
                if inflight.contains(&rel) {
                    continue;
                }
                tracing::info!("retention: removing {rel}");
                let _ = std::fs::remove_file(dir.join(&f));
                let _ = std::fs::remove_file(dir.join(format!("{f}.sig")));
                st.access.lock().unwrap().remove(&rel);
                removed += 1;
            }
        }
    }
    tracing::info!(removed, "retention finished");
}

fn mtime_secs(p: &Path) -> Option<u64> {
    let m = p.metadata().ok()?.modified().ok()?;
    m.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Downloads the current version of every package clients requested within
/// `max_age_days`, one at a time.
pub async fn prefetch(st: &Arc<AppState>) {
    let max_age = st.cfg.prefetch.max_age_days * 86400;
    let now = unix_now();
    let mut wanted: HashMap<(String, String, String), ()> = HashMap::new();
    for (rel, last) in st.access.lock().unwrap().iter() {
        if now.saturating_sub(*last) > max_age {
            continue;
        }
        let Some(t) = routes::parse(rel, &[]) else {
            continue;
        };
        if let Some((name, _)) = routes::split_pkg_filename(&t.file) {
            wanted.insert((t.repo, t.arch, name.to_string()), ());
        }
    }
    let mut fetched = 0;
    for (repo, arch, name) in wanted.into_keys() {
        let file = {
            let idx = st.index.read().unwrap();
            let Some(i) = idx.get(&(repo.clone(), arch.clone())) else {
                continue;
            };
            match i.current_by_name().get(name.as_str()) {
                Some(e) => e.filename.clone(),
                None => continue,
            }
        };
        let t = Target {
            kind: Kind::Package,
            repo,
            arch,
            file,
        };
        if let Start::Inflight(mut rx) = fetch::start_download(st, &t) {
            tracing::info!("prefetch: {}", t.rel_path());
            fetched += 1;
            while rx.changed().await.is_ok() {
                if matches!(rx.borrow().state, DlState::Done | DlState::Failed { .. }) {
                    break;
                }
            }
        }
    }
    tracing::info!(fetched, "prefetch finished");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(vers: &[&str]) -> Vec<(String, String)> {
        vers.iter()
            .map(|v| (format!("foo-{v}-x86_64.pkg.tar.zst"), v.to_string()))
            .collect()
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    #[test]
    fn keeps_current_plus_n_previous() {
        let f = files(&["1.0-1", "1.2-1", "1.10-1", "1.9-2", "1.9-1"]);
        assert_eq!(
            sorted(select_removals(&f, Some("1.10-1"), 1)),
            vec![
                "foo-1.0-1-x86_64.pkg.tar.zst",
                "foo-1.2-1-x86_64.pkg.tar.zst",
                "foo-1.9-1-x86_64.pkg.tar.zst",
            ]
        );
        assert_eq!(
            select_removals(&f, Some("1.10-1"), 10),
            Vec::<String>::new()
        );
        assert_eq!(select_removals(&f, Some("1.10-1"), 0).len(), 4);
    }

    #[test]
    fn keeps_versions_newer_than_db() {
        // Cache already holds 2.0 while our db still says 1.9.
        let f = files(&["2.0-1", "1.9-1", "1.8-1", "1.7-1"]);
        assert_eq!(
            select_removals(&f, Some("1.9-1"), 1),
            vec!["foo-1.7-1-x86_64.pkg.tar.zst"]
        );
    }

    #[test]
    fn epoch_wins_and_unknown_package_keeps_newest() {
        let f = files(&["1:0.5-1", "3.0-1"]);
        assert_eq!(
            select_removals(&f, Some("1:0.5-1"), 0),
            vec!["foo-3.0-1-x86_64.pkg.tar.zst"]
        );
        let f = files(&["1.0-1", "2.0-1", "3.0-1"]);
        assert_eq!(
            sorted(select_removals(&f, None, 1)),
            vec![
                "foo-1.0-1-x86_64.pkg.tar.zst",
                "foo-2.0-1-x86_64.pkg.tar.zst"
            ]
        );
    }
}
