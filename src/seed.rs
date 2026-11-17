//! Read-only seed directories (e.g. the host's /var/cache/pacman/pkg).
//!
//! On a cache miss the exact file name is looked up in each seed dir. A
//! candidate is copied into a temp file in the cache dir (reflink, else
//! hardlink, else plain copy), and the *copy* is verified exactly like an
//! upstream download before the atomic rename. Seed dirs are only ever
//! opened for reading: nothing in them is written, renamed or deleted, and a
//! file that fails verification is simply skipped.

use crate::fetch;
use crate::routes::{Kind, Target};
use crate::state::{AppState, Stats};
use crate::verify;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How a seed file was brought into the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Reflink,
    Hardlink,
    Copy,
}

/// Creates `dst` (which must not exist) with the contents of `src`: a
/// reflink (FICLONE) where the filesystem supports it, else a hardlink, else
/// a plain copy. `src` is only opened read-only.
pub fn clone_or_copy(src: &Path, dst: &Path) -> io::Result<Method> {
    use std::os::fd::AsRawFd;
    let mut s = File::open(src)?;
    {
        let d = File::create_new(dst)?;
        // SAFETY: FICLONE takes the source fd as its argument; both fds are
        // open for the duration of the call.
        let rc = unsafe { libc::ioctl(d.as_raw_fd(), libc::FICLONE as _, s.as_raw_fd()) };
        if rc == 0 {
            return Ok(Method::Reflink);
        }
    }
    std::fs::remove_file(dst)?;
    if std::fs::hard_link(src, dst).is_ok() {
        return Ok(Method::Hardlink);
    }
    let mut d = File::create_new(dst)?;
    let res = io::copy(&mut s, &mut d).and_then(|_| d.sync_all());
    if let Err(e) = res {
        let _ = std::fs::remove_file(dst);
        return Err(e);
    }
    Ok(Method::Copy)
}

/// Seed files that could hold `file`: final names only, regular, non-empty.
fn candidates(st: &AppState, file: &str) -> Vec<PathBuf> {
    if file.starts_with('.') || file.ends_with(".part") || file.contains('/') {
        return vec![];
    }
    st.cfg
        .seed
        .dirs
        .iter()
        .map(|d| d.join(file))
        .filter(|p| p.metadata().is_ok_and(|m| m.is_file() && m.len() > 0))
        .collect()
}

/// Tries to fill the cache entry for `t` from the seed dirs. Returns true
/// when a verified copy was committed.
pub async fn import(st: &AppState, t: &Target) -> bool {
    for src in candidates(st, &t.file) {
        match import_one(st, t, &src).await {
            Ok(method) => {
                Stats::inc(&st.stats.seed_hits);
                tracing::info!(
                    "seeded {} from {} ({method:?})",
                    t.rel_path(),
                    src.display()
                );
                return true;
            }
            Err(e) => {
                Stats::inc(&st.stats.seed_rejects);
                tracing::info!("seed {} skipped: {e}", src.display());
            }
        }
    }
    false
}

async fn import_one(st: &AppState, t: &Target, src: &Path) -> Result<Method, String> {
    let expected = st.lookup(&t.repo, &t.arch, &t.file);
    let keyring = st.keyring();
    let len = src.metadata().map_err(|e| e.to_string())?.len();
    match &expected {
        Some(e) if len != e.csize => {
            return Err(format!("size {len} != sync db {}", e.csize));
        }
        None if keyring.is_none() => {
            return Err("not in sync db and signatures disabled".into());
        }
        _ => {}
    }
    let tmp = st.tmp_path();
    let (s, d) = (src.to_path_buf(), tmp.clone());
    let method = tokio::task::spawn_blocking(move || clone_or_copy(&s, &d))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("import: {e}"))?;
    let res = verify_copy(st, t, src, &tmp, len, expected, keyring).await;
    let res = match res {
        Ok(sig) => {
            let dest = st.cache_path(t);
            if let Some(sig) = &sig {
                fetch::write_sidecar(st, &dest, sig).await;
            }
            fetch::commit(&tmp, &dest)
                .await
                .map_err(|e| format!("commit: {e}"))
        }
        Err(e) => Err(e),
    };
    if res.is_err() {
        // Only our own temp name in the cache dir; never the seed file.
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    res.map(|_| method)
}

/// Verifies the imported copy. Returns the signature that verified it.
async fn verify_copy(
    st: &AppState,
    t: &Target,
    src: &Path,
    tmp: &Path,
    len: u64,
    expected: Option<crate::db::PkgEntry>,
    keyring: Option<Arc<verify::Keyring>>,
) -> Result<Option<Vec<u8>>, String> {
    let p = tmp.to_path_buf();
    let (n, sha) = tokio::task::spawn_blocking(move || verify::sha256_file(&p))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    if n != len {
        return Err(format!("changed while importing ({n} != {len} bytes)"));
    }
    if let Some(e) = &expected
        && sha != e.sha256
    {
        return Err(format!("sha256 {sha} != sync db {}", e.sha256));
    }
    let Some(kr) = keyring else {
        return Ok(None);
    };
    // Signature: from the db, else the seed dir's .sig, else upstream.
    let mut sigs: Vec<Vec<u8>> = vec![];
    if let Some(s) = expected.and_then(|e| e.pgpsig) {
        sigs.push(s);
    } else {
        let mut seed_sig = src.as_os_str().to_owned();
        seed_sig.push(".sig");
        if let Ok(s) = tokio::fs::read(PathBuf::from(seed_sig)).await
            && verify::parse_signatures(&s).is_ok()
        {
            sigs.push(s);
        }
    }
    let mut tried_upstream = false;
    loop {
        for sig in &sigs {
            if fetch::verify_blocking(&kr, tmp, sig).await.is_ok() {
                return Ok(Some(sig.clone()));
            }
        }
        if tried_upstream {
            return Err("signature does not verify".into());
        }
        tried_upstream = true;
        let name = format!("{}.sig", t.file);
        match fetch::fetch_sig(st, &t.repo, &t.arch, &name, None).await {
            Ok(s) => sigs = vec![s],
            Err(_) => return Err("no valid signature available".into()),
        }
    }
}

/// Imports every current package (per the sync dbs) found in the seed dirs
/// that isn't cached yet. Runs in the background with bounded concurrency.
pub async fn scan(st: Arc<AppState>) {
    let current: HashMap<String, (String, String)> = st
        .index
        .read()
        .unwrap()
        .iter()
        .flat_map(|((repo, arch), idx)| {
            idx.by_filename
                .keys()
                .map(move |f| (f.clone(), (repo.clone(), arch.clone())))
        })
        .collect();
    let mut todo: HashMap<String, Target> = HashMap::new();
    for dir in &st.cfg.seed.dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            tracing::warn!("seed dir {} is not readable", dir.display());
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some((repo, arch)) = current.get(&name) else {
                continue;
            };
            let t = Target {
                repo: repo.clone(),
                arch: arch.clone(),
                file: name,
                kind: Kind::Package,
            };
            if !st.cache_path(&t).exists() {
                todo.insert(t.rel_path(), t);
            }
        }
    }
    tracing::info!(candidates = todo.len(), "seed scan started");
    let imported = futures_util::stream::iter(todo.into_values())
        .map(|t| {
            let st = st.clone();
            async move {
                let busy = st.inflight.lock().unwrap().contains_key(&t.rel_path());
                !busy && !st.cache_path(&t).exists() && import(&st, &t).await
            }
        })
        .buffer_unordered(4)
        .filter(|ok| std::future::ready(*ok))
        .count()
        .await;
    tracing::info!(imported, "seed scan finished");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tmpfs (and cross-device) cannot reflink: must fall back and still
    /// produce an identical, independent file without touching the source.
    #[test]
    fn falls_back_on_non_reflink_fs() {
        let shm = Path::new("/dev/shm");
        let base = if shm.is_dir() {
            tempfile::tempdir_in(shm).unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let src = base.path().join("src.pkg.tar.zst");
        std::fs::write(&src, vec![7u8; 100_000]).unwrap();
        let before = src.metadata().unwrap();

        let dst = base.path().join("dst");
        let m = clone_or_copy(&src, &dst).unwrap();
        assert_ne!(m, Method::Reflink, "tmpfs has no reflink");
        assert_eq!(std::fs::read(&dst).unwrap(), std::fs::read(&src).unwrap());

        // Cross-device (tmpfs -> other fs, or same): hardlink fails, copy works.
        let other = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let dst2 = other.path().join("dst");
        clone_or_copy(&src, &dst2).unwrap();
        assert_eq!(std::fs::read(&dst2).unwrap(), std::fs::read(&src).unwrap());

        use std::os::unix::fs::MetadataExt;
        let after = src.metadata().unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.mtime(), after.mtime());
        assert_eq!(before.ino(), after.ino());
        // An existing destination is never overwritten.
        assert!(clone_or_copy(&src, &dst2).is_err());
    }
}
