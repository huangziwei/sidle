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

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::config::ServerConfig;

const TIMEOUT: Duration = Duration::from_secs(10);

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
