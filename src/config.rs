//! TOML configuration. Every field has a default so an empty file is valid.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Addresses to listen on, e.g. `["0.0.0.0:9129", "0.0.0.0:7878"]`.
    pub listen: Vec<String>,
    /// Package cache. Owned exclusively by pacoflexo; never point this at
    /// /var/cache/pacman/pkg.
    pub cache_dir: PathBuf,
    /// Mirror ranking and access times are persisted here.
    pub state_dir: PathBuf,
    /// Accepted `<name>` values in pacoloco-style `/repo/<name>/...` URLs.
    pub pacoloco_prefixes: Vec<String>,
    /// Repositories whose databases are refreshed in the background (used for
    /// verification, retention and prefetch). Other repos are still proxied.
    pub repos: Vec<String>,
    pub arches: Vec<String>,
    /// How long a cached `.db`/`.files` is served before asking upstream again.
    pub db_ttl_secs: u64,
    /// Background refresh of the databases of `repos`, followed by retention
    /// and prefetch.
    pub refresh_interval_secs: u64,
    /// `full` (size + sha256/signature of every file), `size` or `off`.
    pub startup_scan: ScanMode,
    pub mirrors: MirrorConfig,
    pub verify: VerifyConfig,
    pub retention: RetentionConfig,
    pub prefetch: PrefetchConfig,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScanMode {
    Full,
    Size,
    Off,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MirrorConfig {
    /// Mirror URLs, either a base (`https://host/archlinux/`) or a mirrorlist
    /// template containing `$repo` and `$arch`.
    pub urls: Vec<String>,
    /// Also take candidates from the Arch mirror status JSON.
    pub use_status_json: bool,
    pub status_url: String,
    /// ISO country codes to keep from the status JSON (empty = any).
    pub countries: Vec<String>,
    /// How many status-JSON mirrors (best score first) get a latency test.
    pub max_candidates: usize,
    /// Rank all mirrors by measured latency. If false, `urls` order is kept.
    pub latency_rank: bool,
    pub rank_interval_secs: u64,
    pub connect_timeout_secs: u64,
    /// Max time without receiving any bytes from upstream.
    pub read_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerifyConfig {
    /// Verify package signatures. Size and sha256 against the sync db are
    /// always checked.
    pub signatures: bool,
    pub keyring: PathBuf,
    /// File with one revoked fingerprint per line (archlinux-revoked).
    pub revoked: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// Previous versions kept per package, in addition to the current one.
    pub keep_versions: usize,
    /// Delete packages nobody has requested for this many days (0 = never).
    pub unaccessed_days: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrefetchConfig {
    pub enabled: bool,
    /// Only packages requested within this many days are prefetched.
    pub max_age_days: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: vec!["0.0.0.0:9129".into()],
            cache_dir: "/var/cache/pacoflexo".into(),
            state_dir: "/var/lib/pacoflexo".into(),
            pacoloco_prefixes: vec!["archlinux".into()],
            repos: vec!["core".into(), "extra".into(), "multilib".into()],
            arches: vec!["x86_64".into()],
            db_ttl_secs: 60,
            refresh_interval_secs: 900,
            startup_scan: ScanMode::Full,
            mirrors: MirrorConfig::default(),
            verify: VerifyConfig::default(),
            retention: RetentionConfig::default(),
            prefetch: PrefetchConfig::default(),
        }
    }
}

impl Default for MirrorConfig {
    fn default() -> Self {
        MirrorConfig {
            urls: vec![],
            use_status_json: true,
            status_url: "https://archlinux.org/mirrors/status/json/".into(),
            countries: vec![],
            max_candidates: 20,
            latency_rank: true,
            rank_interval_secs: 6 * 3600,
            connect_timeout_secs: 5,
            read_timeout_secs: 30,
        }
    }
}

impl Default for VerifyConfig {
    fn default() -> Self {
        VerifyConfig {
            signatures: true,
            keyring: "/usr/share/pacman/keyrings/archlinux.gpg".into(),
            revoked: "/usr/share/pacman/keyrings/archlinux-revoked".into(),
        }
    }
}

impl Default for RetentionConfig {
    fn default() -> Self {
        RetentionConfig {
            keep_versions: 1,
            unaccessed_days: 30,
        }
    }
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        PrefetchConfig {
            enabled: false,
            max_age_days: 30,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.listen.is_empty() {
            return Err("listen: at least one address is required".into());
        }
        if self.mirrors.urls.is_empty() && !self.mirrors.use_status_json {
            return Err("mirrors: set urls and/or use_status_json".into());
        }
        if self.cache_dir == Path::new("/var/cache/pacman/pkg") {
            return Err("cache_dir must not be pacman's own cache /var/cache/pacman/pkg".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let text = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.listen.len(), 2);
    }

    #[test]
    fn empty_config_uses_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.db_ttl_secs, 60);
        assert_eq!(cfg.retention.keep_versions, 1);
        cfg.validate().unwrap();
    }

    #[test]
    fn shared_pacman_cache_is_rejected() {
        let cfg: Config = toml::from_str("cache_dir = \"/var/cache/pacman/pkg\"").unwrap();
        assert!(cfg.validate().is_err());
    }
}
