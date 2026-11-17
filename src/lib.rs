//! pacoflexo: a verifying caching proxy for Arch Linux package repositories.

pub mod config;
pub mod db;
pub mod fetch;
pub mod maintenance;
pub mod mirrors;
pub mod routes;
pub mod seed;
pub mod server;
pub mod state;
pub mod vercmp;
pub mod verify;

use std::sync::Arc;
use std::time::Duration;

/// Installs the rustls crypto provider (ring). Safe to call more than once.
pub fn init_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Builds the state, ranks mirrors if needed, loads and refreshes the sync
/// databases and scrubs the cache. Call before serving.
pub async fn startup(cfg: config::Config) -> Result<Arc<state::AppState>, String> {
    init_tls();
    let st = Arc::new(state::AppState::new(cfg)?);
    if st.mirrors.is_empty() || st.cfg.mirrors.latency_rank {
        let s = st.clone();
        // Ranking in the background is fine once we have something to use.
        let job = async move { s.mirrors.rank(&s.cfg.mirrors, &s.client).await };
        if st.mirrors.is_empty() {
            job.await;
        } else {
            tokio::spawn(job);
        }
    }
    if st.mirrors.is_empty() {
        return Err("no usable upstream mirrors".into());
    }
    let s = st.clone();
    tokio::task::spawn_blocking(move || maintenance::load_indexes(&s))
        .await
        .map_err(|e| e.to_string())?;
    maintenance::refresh_dbs(&st).await;
    let s = st.clone();
    let mode = st.cfg.startup_scan;
    tokio::task::spawn_blocking(move || maintenance::scrub(&s, mode))
        .await
        .map_err(|e| e.to_string())?;
    Ok(st)
}

/// Optional one-off seed scan, then periodic db refresh, retention, prefetch, access-time persistence and
/// mirror re-ranking.
pub fn spawn_background(st: Arc<state::AppState>) {
    if st.cfg.seed.scan_on_start && !st.cfg.seed.dirs.is_empty() {
        tokio::spawn(seed::scan(st.clone()));
    }
    let s = st.clone();
    tokio::spawn(async move {
        let mut tick =
            tokio::time::interval(Duration::from_secs(s.cfg.refresh_interval_secs.max(30)));
        tick.tick().await;
        loop {
            tick.tick().await;
            maintenance::refresh_dbs(&s).await;
            let s2 = s.clone();
            let _ = tokio::task::spawn_blocking(move || maintenance::retention(&s2)).await;
            if s.cfg.prefetch.enabled {
                maintenance::prefetch(&s).await;
            }
            s.save_access();
        }
    });
    if st.cfg.mirrors.latency_rank {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(
                st.cfg.mirrors.rank_interval_secs.max(60),
            ));
            tick.tick().await;
            loop {
                tick.tick().await;
                st.mirrors.rank(&st.cfg.mirrors, &st.client).await;
            }
        });
    }
}
