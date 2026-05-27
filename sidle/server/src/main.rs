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

    let config = sidle_server::Config {
        paths,
        bind,
        token,
    };
    sidle_server::serve(config).await
}
