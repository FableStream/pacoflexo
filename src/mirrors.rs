//! Upstream mirror list: configured URLs plus (optionally) the Arch mirror
//! status JSON, ranked by a latency test and persisted in the state dir.

use crate::config::MirrorConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Serialize, Deserialize)]
pub struct Mirror {
    /// Template containing `$repo` and `$arch`.
    pub template: String,
    pub latency_ms: Option<u64>,
    #[serde(skip)]
    pub errors: AtomicU64,
}

impl Clone for Mirror {
    fn clone(&self) -> Self {
        Mirror {
            template: self.template.clone(),
            latency_ms: self.latency_ms,
            errors: AtomicU64::new(self.errors.load(Ordering::Relaxed)),
        }
    }
}

impl Mirror {
    pub fn new(url: &str) -> Mirror {
        Mirror {
            template: normalize(url),
            latency_ms: None,
            errors: AtomicU64::new(0),
        }
    }

    pub fn url(&self, repo: &str, arch: &str, file: &str) -> String {
        let base = self.template.replace("$repo", repo).replace("$arch", arch);
        format!("{}/{}", base.trim_end_matches('/'), file)
    }
}

/// `https://host/archlinux/` -> `https://host/archlinux/$repo/os/$arch`
pub fn normalize(url: &str) -> String {
    if url.contains("$repo") {
        url.trim_end_matches('/').to_string()
    } else {
        format!("{}/$repo/os/$arch", url.trim_end_matches('/'))
    }
}

pub struct Mirrors {
    list: RwLock<Vec<std::sync::Arc<Mirror>>>,
    state_file: PathBuf,
}

#[derive(Deserialize)]
struct StatusJson {
    urls: Vec<StatusMirror>,
}

#[derive(Deserialize)]
struct StatusMirror {
    url: String,
    protocol: String,
    active: bool,
    completion_pct: Option<f64>,
    delay: Option<u64>,
    score: Option<f64>,
    country_code: Option<String>,
}

impl Mirrors {
    /// Starts from the persisted ranking if there is one, else the configured
    /// URLs in order.
    pub fn new(cfg: &MirrorConfig, state_dir: &Path) -> Mirrors {
        let state_file = state_dir.join("mirrors.json");
        let persisted: Option<Vec<Mirror>> = std::fs::read(&state_file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let list = match persisted {
            Some(p) if !p.is_empty() && cfg.latency_rank => p,
            _ => cfg.urls.iter().map(|u| Mirror::new(u)).collect(),
        };
        Mirrors {
            list: RwLock::new(list.into_iter().map(std::sync::Arc::new).collect()),
            state_file,
        }
    }

    pub fn snapshot(&self) -> Vec<std::sync::Arc<Mirror>> {
        self.list.read().unwrap().clone()
    }

    pub fn is_empty(&self) -> bool {
        self.list.read().unwrap().is_empty()
    }

    /// Re-collects candidates and (if `latency_rank`) ranks them by latency.
    pub async fn rank(&self, cfg: &MirrorConfig, client: &reqwest::Client) {
        let mut candidates: Vec<String> = cfg.urls.iter().map(|u| normalize(u)).collect();
        if cfg.use_status_json {
            match fetch_status(cfg, client).await {
                Ok(urls) => candidates.extend(urls),
                Err(e) => tracing::warn!("mirror status json: {e}"),
            }
        }
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.clone()));

        let probes = candidates.into_iter().map(|template| {
            let client = client.clone();
            let probe_it = cfg.latency_rank;
            async move {
                Mirror {
                    latency_ms: if probe_it {
                        probe(&client, &template).await
                    } else {
                        None
                    },
                    template,
                    errors: AtomicU64::new(0),
                }
            }
        });
        let mut ranked = futures_util::future::join_all(probes).await;
        if !cfg.latency_rank {
            *self.list.write().unwrap() = ranked.into_iter().map(std::sync::Arc::new).collect();
            return;
        }
        ranked.sort_by_key(|m| m.latency_ms.unwrap_or(u64::MAX));
        if ranked.iter().all(|m| m.latency_ms.is_none()) && !self.is_empty() {
            tracing::warn!("mirror ranking: no mirror answered, keeping previous order");
            return;
        }
        tracing::info!(
            "mirror ranking: {}",
            ranked
                .iter()
                .take(5)
                .map(|m| format!("{} ({:?}ms)", m.template, m.latency_ms))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let Ok(json) = serde_json::to_vec_pretty(&ranked) {
            let tmp = self.state_file.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.state_file);
            }
        }
        *self.list.write().unwrap() = ranked.into_iter().map(std::sync::Arc::new).collect();
    }
}

async fn fetch_status(cfg: &MirrorConfig, client: &reqwest::Client) -> Result<Vec<String>, String> {
    let status: StatusJson = client
        .get(&cfg.status_url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let mut ok: Vec<StatusMirror> = status
        .urls
        .into_iter()
        .filter(|m| {
            m.active
                && m.protocol == "https"
                && m.completion_pct.unwrap_or(0.0) >= 1.0
                && m.delay.is_some_and(|d| d < 3600)
                && m.score.is_some()
                && (cfg.countries.is_empty()
                    || m.country_code
                        .as_deref()
                        .is_some_and(|c| cfg.countries.iter().any(|x| x.eq_ignore_ascii_case(c))))
        })
        .collect();
    ok.sort_by(|a, b| {
        a.score
            .unwrap_or(f64::MAX)
            .total_cmp(&b.score.unwrap_or(f64::MAX))
    });
    Ok(ok
        .into_iter()
        .take(cfg.max_candidates)
        .map(|m| normalize(&m.url))
        .collect())
}

/// Time until response headers for `core.db`, best of two tries.
async fn probe(client: &reqwest::Client, template: &str) -> Option<u64> {
    let m = Mirror::new(template);
    let url = m.url("core", "x86_64", "core.db");
    let mut best = None;
    for _ in 0..2 {
        let start = Instant::now();
        let ok = client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        if ok {
            let ms = start.elapsed().as_millis() as u64;
            best = Some(best.map_or(ms, |b: u64| b.min(ms)));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_building() {
        let m = Mirror::new("https://example.org/archlinux/");
        assert_eq!(
            m.url("core", "x86_64", "core.db"),
            "https://example.org/archlinux/core/os/x86_64/core.db"
        );
        let m = Mirror::new("https://example.org/arch/$repo/os/$arch");
        assert_eq!(
            m.url("extra", "x86_64", "a.pkg.tar.zst"),
            "https://example.org/arch/extra/os/x86_64/a.pkg.tar.zst"
        );
    }
}
