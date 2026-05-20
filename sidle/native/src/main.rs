//! Sidle native — Milestone 4 HTTP + JSON list.
//!
//! Reads `etc/server.conf`, GETs `/list.json` over LAN, logs every title
//! to `/mnt/us/sidle-native.log`. No fb / touch yet — M5 reintroduces
//! them to render the list on the panel.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

mod api;
mod config;
mod eink;

const LOG_PATH: &str = "/mnt/us/sidle-native.log";
const CONFIG_PATH: &str = "/mnt/us/extensions/sidle/etc/server.conf";

fn main() {
    let result = run();
    log(format!("done: {result:?}"));
}

fn run() -> anyhow::Result<()> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    log(format!("sidle-native M4 start: ts={ts}"));

    let cfg = config::load(Path::new(CONFIG_PATH))?;
    log(format!("server: http://{}:{}", cfg.host, cfg.port));

    let books = api::list_books(&cfg)?;
    log(format!("books: {}", books.len()));
    for book in &books {
        log(format!(
            "  [{}] {} — {} ({})",
            book.id, book.title, book.author, book.language,
        ));
    }
    Ok(())
}

fn log(line: impl AsRef<str>) {
    let line = line.as_ref();
    let log_path = if std::path::Path::new("/mnt/us").is_dir() {
        LOG_PATH
    } else {
        "./sidle-native.log"
    };
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(f, "{line}");
    }
    let _ = writeln!(std::io::stderr(), "{line}");
}
