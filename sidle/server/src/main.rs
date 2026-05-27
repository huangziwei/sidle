//! `sidle-server` CLI entry point. Stand-alone mode: read `--data-dir`,
//! load (or generate) the bearer token, bind on `0.0.0.0:<port>`, serve.
//!
//! The Tauri app does the same dance in-process via `sidle_server::serve`,
//! sharing the runtime; this binary is for the "GUI quit, Kindle should
//! still reach the library" case.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use sidle_core::library::LibraryPaths;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "sidle LAN HTTP server — read-only library access for KUAL pulls"
)]
struct Cli {
    /// Override sidle's data directory. Without this flag, resolves the same
    /// root the Tauri desktop app uses — the relocate pointer in `config.json`
    /// if set, else `~/Library/Application Support/sidle` (via `dirs::data_dir()`).
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// TCP port to listen on. Always binds `0.0.0.0:<port>` so the Kindle
    /// on the LAN can reach it.
    #[arg(long, default_value_t = 8731)]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let paths = match cli.data_dir {
        Some(root) => LibraryPaths { root },
        None => LibraryPaths::resolve().context("resolve library root")?,
    };

    let token = sidle_server::load_or_generate_token(&paths.root)
        .context("load or generate server token")?;

    let bind = format!("0.0.0.0:{}", cli.port);

    // PID file so the desktop app / sakabar / CLI can stop this daemon precisely
    // and show who's serving. Written here in the standalone binary — never in the
    // shared `serve()` — so the app's (former) in-process use couldn't write the
    // app's own PID here. Removed on graceful exit; a SIGKILL leaves it stale,
    // which the app tolerates (it trusts the `/` probe over the file).
    let pid_path = paths.root.join("server.pid");
    if let Err(e) = std::fs::write(&pid_path, std::process::id().to_string()) {
        tracing::warn!(?e, path = %pid_path.display(), "could not write PID file");
    }

    let config = sidle_server::Config { paths, bind, token };
    let result = sidle_server::serve_with_shutdown(config, shutdown_signal()).await;
    let _ = std::fs::remove_file(&pid_path);
    result
}

/// Resolves on SIGINT (Ctrl-C) or SIGTERM so `serve_with_shutdown` drains
/// in-flight requests before exit. SIGTERM is what the desktop app's stop,
/// sakabar's port-kill, and a plain `kill` send; SIGINT is an interactive Ctrl-C.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("sidle-server: shutdown signal received, draining");
}
