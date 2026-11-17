//! Sync database parsing. A `.db` is a (zstd/gzip/plain) tar of
//! `<name>-<version>/desc` files in `%KEY%\nvalue\n\n` format.

use base64::Engine;
use std::collections::HashMap;
use std::io::{self, Read};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkgEntry {
    pub name: String,
    pub version: String,
    pub filename: String,
    pub csize: u64,
    /// Lowercase hex.
    pub sha256: String,
    /// Detached binary signature from `%PGPSIG%`, when the db carries one.
    pub pgpsig: Option<Vec<u8>>,
}

/// One parsed `<repo>.db`.
#[derive(Debug, Default)]
pub struct RepoIndex {
    pub by_filename: HashMap<String, PkgEntry>,
}

impl RepoIndex {
    pub fn from_entries(entries: Vec<PkgEntry>) -> Self {
        RepoIndex {
            by_filename: entries
                .into_iter()
                .map(|e| (e.filename.clone(), e))
                .collect(),
        }
    }

    /// Current filename of each package name.
    pub fn current_by_name(&self) -> HashMap<&str, &PkgEntry> {
        self.by_filename
            .values()
            .map(|e| (e.name.as_str(), e))
            .collect()
    }
}

pub fn decompress(bytes: &[u8]) -> io::Result<Box<dyn Read + '_>> {
    Ok(match bytes {
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Box::new(zstd::stream::read::Decoder::new(bytes)?),
        [0x1f, 0x8b, ..] => Box::new(flate2::read::GzDecoder::new(bytes)),
        [0xfd, b'7', b'z', b'X', b'Z', 0, ..] => {
            return Err(io::Error::other(
                "xz-compressed databases are not supported",
            ));
        }
        _ => Box::new(bytes),
    })
}

pub fn parse_db(bytes: &[u8]) -> io::Result<Vec<PkgEntry>> {
    let mut archive = tar::Archive::new(decompress(bytes)?);
    let mut out = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        if !path.ends_with("/desc") {
            continue;
        }
        let mut text = String::new();
        entry.read_to_string(&mut text)?;
        match parse_desc(&text) {
            Some(e) => out.push(e),
            None => return Err(io::Error::other(format!("{path}: incomplete desc entry"))),
        }
    }
    if out.is_empty() {
        return Err(io::Error::other("database contains no packages"));
    }
    Ok(out)
}

pub fn parse_desc(text: &str) -> Option<PkgEntry> {
    let mut fields: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut key: Option<&str> = None;
    for line in text.lines() {
        if line.is_empty() {
            key = None;
        } else if line.starts_with('%') && line.ends_with('%') && line.len() > 2 {
            key = Some(&line[1..line.len() - 1]);
        } else if let Some(k) = key {
            fields.entry(k).or_default().push(line);
        }
    }
    let one = |k: &str| fields.get(k).and_then(|v| v.first()).map(|s| s.trim());
    let sha256 = one("SHA256SUM")?.to_ascii_lowercase();
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let pgpsig =
        one("PGPSIG").and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok());
    Some(PkgEntry {
        name: one("NAME")?.to_string(),
        version: one("VERSION")?.to_string(),
        filename: one("FILENAME")?.to_string(),
        csize: one("CSIZE")?.parse().ok()?,
        sha256,
        pgpsig,
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::Write;

    pub fn desc(name: &str, version: &str, filename: &str, csize: u64, sha: &str) -> String {
        format!(
            "%FILENAME%\n{filename}\n\n%NAME%\n{name}\n\n%VERSION%\n{version}\n\n\
             %DESC%\nsomething\n\n%CSIZE%\n{csize}\n\n%ISIZE%\n1\n\n%SHA256SUM%\n{sha}\n\n\
             %DEPENDS%\nglibc\nzlib\n\n"
        )
    }

    pub fn tar_of(entries: &[(String, String)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (dir, text) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(text.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, format!("{dir}/desc"), text.as_bytes())
                .unwrap();
        }
        b.into_inner().unwrap()
    }

    fn sample() -> Vec<u8> {
        let sha = "a".repeat(64);
        tar_of(&[
            (
                "foo-1.0-1".into(),
                desc("foo", "1.0-1", "foo-1.0-1-x86_64.pkg.tar.zst", 123, &sha),
            ),
            (
                "bar-2:3-4".into(),
                desc("bar", "2:3-4", "bar-2:3-4-any.pkg.tar.zst", 9, &sha)
                    + "%PGPSIG%\naGVsbG8=\n\n",
            ),
        ])
    }

    fn check(entries: Vec<PkgEntry>) {
        let idx = RepoIndex::from_entries(entries);
        let foo = &idx.by_filename["foo-1.0-1-x86_64.pkg.tar.zst"];
        assert_eq!((foo.name.as_str(), foo.csize), ("foo", 123));
        assert_eq!(foo.pgpsig, None);
        let bar = &idx.by_filename["bar-2:3-4-any.pkg.tar.zst"];
        assert_eq!(bar.version, "2:3-4");
        assert_eq!(bar.pgpsig.as_deref(), Some(&b"hello"[..]));
        assert_eq!(idx.current_by_name()["bar"].csize, 9);
    }

    #[test]
    fn parses_plain_gzip_and_zstd() {
        let tar = sample();
        check(parse_db(&tar).unwrap());

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar).unwrap();
        check(parse_db(&gz.finish().unwrap()).unwrap());

        check(parse_db(&zstd::encode_all(&tar[..], 3).unwrap()).unwrap());
    }

    #[test]
    fn rejects_garbage_and_truncation() {
        assert!(parse_db(b"").is_err());
        assert!(parse_db(&[0u8; 1024]).is_err());
        let z = zstd::encode_all(&sample()[..], 3).unwrap();
        assert!(parse_db(&z[..z.len() / 2]).is_err());
    }

    #[test]
    fn desc_requires_checksum_fields() {
        assert!(parse_desc("%FILENAME%\nx\n\n%NAME%\nx\n").is_none());
        let bad_sha = desc("a", "1-1", "a-1-1-any.pkg.tar.zst", 1, "xyz");
        assert!(parse_desc(&bad_sha).is_none());
    }
}
