//! Request path parsing for both URL styles:
//!
//! * pacoloco: `/repo/<prefix>/<repo>/os/<arch>/<file>`
//! * flexo:    `/<repo>/os/<arch>/<file>`

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `<repo>.db`, `<repo>.files` (and their `.tar.gz` names)
    Db,
    /// Signature of a database. Arch does not publish these; upstream 404s.
    DbSig,
    Package,
    PackageSig,
    /// Anything else in the repo dir (e.g. `.links.tar.gz`): passed through.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub repo: String,
    pub arch: String,
    pub file: String,
    pub kind: Kind,
}

impl Target {
    /// Path relative to the cache dir and to a mirror base.
    pub fn rel_path(&self) -> String {
        format!("{}/os/{}/{}", self.repo, self.arch, self.file)
    }
}

pub fn parse(path: &str, pacoloco_prefixes: &[String]) -> Option<Target> {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let segs = match segs.as_slice() {
        ["repo", prefix, rest @ ..] if pacoloco_prefixes.iter().any(|p| p == prefix) => rest,
        s => s,
    };
    let [repo, "os", arch, file] = segs else {
        return None;
    };
    if !safe_segment(repo) || !safe_segment(arch) || !safe_segment(file) {
        return None;
    }
    Some(Target {
        repo: repo.to_string(),
        arch: arch.to_string(),
        file: file.to_string(),
        kind: classify(file),
    })
}

pub fn classify(file: &str) -> Kind {
    let (base, sig) = match file.strip_suffix(".sig") {
        Some(b) => (b, true),
        None => (file, false),
    };
    let is_db = [".db", ".files", ".db.tar.gz", ".files.tar.gz"]
        .iter()
        .any(|s| base.ends_with(s));
    let is_pkg = base.contains(".pkg.tar");
    match (is_db, is_pkg, sig) {
        (true, _, false) => Kind::Db,
        (true, _, true) => Kind::DbSig,
        (false, true, false) => Kind::Package,
        (false, true, true) => Kind::PackageSig,
        _ => Kind::Other,
    }
}

/// Path segments may only contain characters found in Arch repo, arch and
/// package file names, and never start with a dot (no `..`, no hidden files).
fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.len() <= 255
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-:@~".contains(&b))
}

/// Splits `name-pkgver-pkgrel-arch.pkg.tar.*` into (name, "pkgver-pkgrel").
pub fn split_pkg_filename(file: &str) -> Option<(&str, &str)> {
    let stem = &file[..file.find(".pkg.tar")?];
    let mut parts = stem.rsplitn(4, '-');
    let _arch = parts.next()?;
    let rel = parts.next()?;
    let ver = parts.next()?;
    let name = parts.next()?;
    if name.is_empty() {
        return None;
    }
    // "ver-rel" is the tail of the stem right after "name-".
    let start = name.len() + 1;
    let end = start + ver.len() + 1 + rel.len();
    Some((name, &stem[start..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefixes() -> Vec<String> {
        vec!["archlinux".into()]
    }

    #[test]
    fn pacoloco_style() {
        let t = parse(
            "/repo/archlinux/extra/os/x86_64/foo-1.0-1-x86_64.pkg.tar.zst",
            &prefixes(),
        )
        .unwrap();
        assert_eq!(t.repo, "extra");
        assert_eq!(t.arch, "x86_64");
        assert_eq!(t.kind, Kind::Package);
        assert_eq!(t.rel_path(), "extra/os/x86_64/foo-1.0-1-x86_64.pkg.tar.zst");
    }

    #[test]
    fn flexo_style() {
        let t = parse("/core/os/x86_64/core.db", &prefixes()).unwrap();
        assert_eq!((t.repo.as_str(), t.kind), ("core", Kind::Db));
        let t = parse("/core/os/x86_64/core.db.sig", &prefixes()).unwrap();
        assert_eq!(t.kind, Kind::DbSig);
        let t = parse("/extra/os/x86_64/extra.files", &prefixes()).unwrap();
        assert_eq!(t.kind, Kind::Db);
        let t = parse("/core/os/x86_64/a-1:2.0-1-any.pkg.tar.zst.sig", &prefixes()).unwrap();
        assert_eq!(t.kind, Kind::PackageSig);
        let t = parse("/core/os/x86_64/core.links.tar.gz", &prefixes()).unwrap();
        assert_eq!(t.kind, Kind::Other);
    }

    #[test]
    fn rejects_bad_paths() {
        let p = prefixes();
        assert!(parse("/repo/other/core/os/x86_64/core.db", &p).is_none());
        assert!(parse("/core/os/x86_64/../x", &p).is_none());
        assert!(parse("/core/os/x86_64/.hidden", &p).is_none());
        assert!(parse("/core/os/x86_64/a/b", &p).is_none());
        assert!(parse("/core/x86_64/core.db", &p).is_none());
        assert!(parse("/core/os/x86_64/", &p).is_none());
        assert!(parse("/core/os/x86_64/a b", &p).is_none());
        assert!(parse("/status", &p).is_none());
    }

    #[test]
    fn splits_package_filenames() {
        assert_eq!(
            split_pkg_filename("foo-bar-1:2.3-4-x86_64.pkg.tar.zst"),
            Some(("foo-bar", "1:2.3-4"))
        );
        assert_eq!(
            split_pkg_filename("python-3.13.1-1.1-x86_64.pkg.tar.xz.sig"),
            Some(("python", "3.13.1-1.1"))
        );
        assert_eq!(split_pkg_filename("core.db"), None);
        assert_eq!(split_pkg_filename("x-86_64.pkg.tar.zst"), None);
    }
}
