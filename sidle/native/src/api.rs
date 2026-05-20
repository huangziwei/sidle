//! Sidle-server HTTP client.
//!
//! Three endpoints, all token-gated, all sync via `ureq`:
//! - `GET /list.json`  → library as JSON
//! - `GET /cover/{id}` → cover image bytes (M6)
//! - `GET /get/{id}`   → KFX bytes + Content-Disposition filename (M7)
//!
//! Token is sent as `X-Sidle-Token` header. The server also accepts
//! `?token=` query but the header is cleaner for programmatic clients.
//!
//! Book shape mirrors `sidle_core::library::db::BookRow` but only the
//! fields the picker needs (id + display strings). serde silently drops
//! unknown JSON fields, so this stays compatible if the server adds
//! columns. The full shape lives at sidle/core/src/library/db.rs.

use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::config::ServerConfig;

const TIMEOUT: Duration = Duration::from_secs(10);
/// Cap per-cover bytes so a corrupt server response can't OOM us. Real
/// covers fit comfortably under 200KB; 8MB is wildly generous but still
/// bounded.
const COVER_MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Deserialize, Clone)]
pub struct Book {
    pub id: i64,
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub language: String,
}

pub fn list_books(cfg: &ServerConfig) -> Result<Vec<Book>> {
    let url = format!("http://{}:{}/list.json", cfg.host, cfg.port);
    let res = ureq::get(&url)
        .set("X-Sidle-Token", &cfg.token)
        .timeout(TIMEOUT)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let body = res
        .into_string()
        .with_context(|| format!("read body of {url}"))?;
    let books: Vec<Book> =
        serde_json::from_str(&body).with_context(|| format!("parse {url}"))?;
    Ok(books)
}

pub fn fetch_cover(cfg: &ServerConfig, id: i64) -> Result<Vec<u8>> {
    let url = format!("http://{}:{}/cover/{}", cfg.host, cfg.port, id);
    let res = ureq::get(&url)
        .set("X-Sidle-Token", &cfg.token)
        .timeout(TIMEOUT)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let mut bytes = Vec::new();
    res.into_reader()
        .take(COVER_MAX_BYTES as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read body of {url}"))?;
    Ok(bytes)
}

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// Cap KFX size at 256MB — any real book is well under that. Defense
/// against a runaway response.
const KFX_MAX_BYTES: usize = 256 * 1024 * 1024;

pub struct Download {
    pub filename: String,
    pub bytes: Vec<u8>,
}

pub fn download_book(cfg: &ServerConfig, id: i64) -> Result<Download> {
    let url = format!("http://{}:{}/get/{}", cfg.host, cfg.port, id);
    let res = ureq::get(&url)
        .set("X-Sidle-Token", &cfg.token)
        .timeout(DOWNLOAD_TIMEOUT)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    // Pull the filename from Content-Disposition. Server emits
    // `attachment; filename="<name>"` — see sidle/server/src/lib.rs.
    let filename = parse_cd_filename(res.header("content-disposition"))
        .unwrap_or_else(|| format!("book-{id}.kfx"));
    let mut bytes = Vec::new();
    res.into_reader()
        .take(KFX_MAX_BYTES as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read body of {url}"))?;
    Ok(Download { filename, bytes })
}

fn parse_cd_filename(hdr: Option<&str>) -> Option<String> {
    let hdr = hdr?;
    let after = hdr.split_once("filename=")?.1;
    let raw = if let Some(stripped) = after.strip_prefix('"') {
        stripped.split_once('"').map(|(a, _)| a).unwrap_or("")
    } else {
        after.split(';').next().unwrap_or("").trim()
    };
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}
