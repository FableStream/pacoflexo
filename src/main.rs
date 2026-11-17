use clap::Parser;
use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

/// Verifying caching proxy for Arch Linux package repositories.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Configuration file.
    #[arg(short, long, default_value = "/etc/pacoflexo/config.toml")]
    config: PathBuf,
    /// Check the configuration and exit.
    #[arg(long)]
    check: bool,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,pgp=error")),
        )
        .with_ansi(use_ansi(
            std::env::var_os("NO_COLOR").as_deref(),
            std::io::stdout().is_terminal(),
        ))
        .init();
    let args = Args::parse();
    if let Err(e) = run(args).await {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

/// Colour the log output only when it goes to a terminal (not the systemd
/// journal, a pipe or a file) and `NO_COLOR` is unset or empty
/// (<https://no-color.org>).
fn use_ansi(no_color: Option<&OsStr>, is_terminal: bool) -> bool {
    is_terminal && no_color.is_none_or(|v| v.is_empty())
}

async fn run(args: Args) -> Result<(), String> {
    let cfg = pacoflexo::config::Config::load(&args.config)?;
    if args.check {
        println!("{}: ok", args.config.display());
        return Ok(());
    }
    // Bind first so a port conflict fails fast, before any long scrub.
    let mut listeners = vec![];
    for addr in &cfg.listen {
        let l = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("listen {addr}: {e}"))?;
        listeners.push(l);
    }
    let st = pacoflexo::startup(cfg).await?;
    pacoflexo::spawn_background(st.clone());
    let app = pacoflexo::server::router(st.clone());
    let mut tasks = tokio::task::JoinSet::new();
    for l in listeners {
        tracing::info!(
            "listening on {}",
            l.local_addr().map_err(|e| e.to_string())?
        );
        let app = app.clone();
        tasks.spawn(async move { axum::serve(l, app).with_graceful_shutdown(shutdown()).await });
    }
    while let Some(r) = tasks.join_next().await {
        if let Ok(Err(e)) = r {
            tracing::error!("server: {e}");
        }
    }
    st.save_access();
    tracing::info!("stopped");
    Ok(())
}

async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::use_ansi;
    use std::ffi::OsStr;

    #[test]
    fn ansi_only_on_a_terminal_without_no_color() {
        assert!(use_ansi(None, true));
        assert!(!use_ansi(None, false));
        assert!(!use_ansi(Some(OsStr::new("1")), true));
        assert!(!use_ansi(Some(OsStr::new("1")), false));
        // An empty NO_COLOR does not disable colour.
        assert!(use_ansi(Some(OsStr::new("")), true));
    }
}
