//! Package checks: size + sha256 against the sync db, and the detached
//! OpenPGP signature against the Arch keyring (pure Rust, via `pgp`).
//!
//! Trust model: any key in the keyring file whose primary fingerprint is not
//! listed in the revoked file is accepted. The keyring is the one shipped by
//! the (signed) archlinux-keyring package, so it only contains Arch master
//! and packager keys. This matches `sqv --keyring archlinux.gpg`.

use pgp::composed::{Deserializable, DetachedSignature, SignedPublicKey};
use pgp::types::KeyDetails;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

pub struct Keyring {
    keys: Vec<SignedPublicKey>,
}

impl Keyring {
    pub fn load(keyring: &Path, revoked: &Path) -> io::Result<Keyring> {
        let revoked: HashSet<String> = match std::fs::read_to_string(revoked) {
            Ok(text) => text
                .lines()
                .filter_map(|l| l.split_whitespace().next())
                .filter(|l| !l.starts_with('#'))
                .map(|l| l.to_ascii_uppercase())
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => HashSet::new(),
            Err(e) => return Err(e),
        };
        let data = std::fs::read(keyring)?;
        let mut keys = Vec::new();
        let (mut skipped, mut dropped) = (0, 0);
        // archlinux.gpg is ASCII-armored; accept binary keyrings too.
        let iter = if data.starts_with(b"-----BEGIN") {
            SignedPublicKey::from_armor_many(&data[..])
                .map_err(io::Error::other)?
                .0
        } else {
            SignedPublicKey::from_bytes_many(&data[..]).map_err(io::Error::other)?
        };
        for key in iter {
            match key {
                Ok(k) if revoked.contains(&fpr(&k)) => dropped += 1,
                Ok(k) => keys.push(k),
                Err(e) => {
                    skipped += 1;
                    tracing::debug!("keyring: skipping unparsable key: {e}");
                }
            }
        }
        if keys.is_empty() {
            return Err(io::Error::other(format!(
                "{}: no usable keys",
                keyring.display()
            )));
        }
        tracing::info!(
            keys = keys.len(),
            revoked = dropped,
            unparsable = skipped,
            "loaded keyring {}",
            keyring.display()
        );
        Ok(Keyring { keys })
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Verifies `sig` (binary or armored detached signature) over the file at
    /// `path`. Returns the fingerprint of the primary key that made it.
    /// Blocking: call from `spawn_blocking`.
    pub fn verify_file(&self, path: &Path, sig: &[u8]) -> io::Result<String> {
        let sigs = parse_signatures(sig)?;
        for sig in &sigs {
            let issuers: Vec<Vec<u8>> = sig
                .signature
                .issuer_fingerprint()
                .iter()
                .map(|f| f.as_bytes().to_vec())
                .chain(
                    sig.signature
                        .issuer_key_id()
                        .iter()
                        .map(|k| k.as_ref().to_vec()),
                )
                .collect();
            let matches = |fp: &[u8], id: &[u8]| {
                issuers.is_empty() || issuers.iter().any(|i| i == fp || i == id)
            };
            for key in &self.keys {
                let primary_hit = matches(
                    key.primary_key.fingerprint().as_bytes(),
                    key.primary_key.legacy_key_id().as_ref(),
                );
                if primary_hit && verify_with(sig, &key.primary_key, path)? {
                    return Ok(fpr(key));
                }
                for sub in &key.public_subkeys {
                    let hit = matches(
                        sub.key.fingerprint().as_bytes(),
                        sub.key.legacy_key_id().as_ref(),
                    );
                    if hit && verify_with(sig, &sub.key, path)? {
                        return Ok(fpr(key));
                    }
                }
            }
        }
        Err(io::Error::other(
            "signature does not verify against any trusted key",
        ))
    }
}

fn verify_with(
    sig: &DetachedSignature,
    key: &impl pgp::types::VerifyingKey,
    path: &Path,
) -> io::Result<bool> {
    let reader = BufReader::with_capacity(1 << 16, File::open(path)?);
    Ok(sig.signature.verify(key, reader).is_ok())
}

fn fpr(key: &SignedPublicKey) -> String {
    format!("{:X}", key.primary_key.fingerprint())
}

/// Parses a detached signature file. Empty or unparsable input is an error.
pub fn parse_signatures(sig: &[u8]) -> io::Result<Vec<DetachedSignature>> {
    if sig.is_empty() {
        return Err(io::Error::other("empty signature"));
    }
    let parsed: Vec<DetachedSignature> = if sig.starts_with(b"-----BEGIN") {
        let (s, _) = DetachedSignature::from_armor_single(sig).map_err(io::Error::other)?;
        vec![s]
    } else {
        DetachedSignature::from_bytes_many(sig)
            .map_err(io::Error::other)?
            .collect::<Result<_, _>>()
            .map_err(io::Error::other)?
    };
    if parsed.is_empty() {
        return Err(io::Error::other("no signature packets"));
    }
    Ok(parsed)
}

/// Size and sha256 of a file (blocking).
pub fn sha256_file(path: &Path) -> io::Result<(u64, String)> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut n = 0u64;
    loop {
        let r = f.read(&mut buf)?;
        if r == 0 {
            break;
        }
        h.update(&buf[..r]);
        n += r as u64;
    }
    Ok((n, hex::encode(h.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fx(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn keyring() -> Keyring {
        Keyring::load(&fx("keyring.gpg"), &fx("revoked")).unwrap()
    }

    #[test]
    fn good_signature_verifies() {
        let kr = keyring();
        assert_eq!(kr.len(), 1, "revoked key must be dropped");
        let sig = std::fs::read(fx("foo-1.0-1-x86_64.pkg.tar.zst.sig")).unwrap();
        let who = kr
            .verify_file(&fx("foo-1.0-1-x86_64.pkg.tar.zst"), &sig)
            .unwrap();
        assert_eq!(who, "FEBA0497432DC1B680BB3AFDE3254378DFF0D791");
    }

    #[test]
    fn wrong_file_empty_sig_and_revoked_key_fail() {
        let kr = keyring();
        let sig = std::fs::read(fx("foo-1.0-1-x86_64.pkg.tar.zst.sig")).unwrap();
        assert!(
            kr.verify_file(&fx("bar-2.0-1-x86_64.pkg.tar.zst"), &sig)
                .is_err()
        );
        assert!(
            kr.verify_file(&fx("foo-1.0-1-x86_64.pkg.tar.zst"), b"")
                .is_err()
        );
        let evil = std::fs::read(fx("evil-1.0-1-x86_64.pkg.tar.zst.sig")).unwrap();
        assert!(
            kr.verify_file(&fx("evil-1.0-1-x86_64.pkg.tar.zst"), &evil)
                .is_err()
        );
    }

    /// Checks against the real Arch keyring and pacman's own cache, when present.
    #[test]
    fn real_arch_package_if_available() {
        let keyring = Path::new("/usr/share/pacman/keyrings/archlinux.gpg");
        let Ok(dir) = std::fs::read_dir("/var/cache/pacman/pkg") else {
            return;
        };
        if !keyring.exists() {
            return;
        }
        let pkg = dir.flatten().map(|e| e.path()).find(|p| {
            let s = p.to_string_lossy();
            s.ends_with(".pkg.tar.zst")
                && p.metadata().map(|m| m.len() < 2_000_000).unwrap_or(false)
                && p.with_extension("zst.sig").exists()
        });
        let Some(pkg) = pkg else { return };
        let kr = Keyring::load(
            keyring,
            Path::new("/usr/share/pacman/keyrings/archlinux-revoked"),
        )
        .unwrap();
        let sig = std::fs::read(pkg.with_extension("zst.sig")).unwrap();
        kr.verify_file(&pkg, &sig)
            .unwrap_or_else(|e| panic!("{}: {e}", pkg.display()));
    }
}
