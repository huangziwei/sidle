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

use std::fmt;
use std::io::Read;
use std::time::Duration;

use anyhow::{Context, anyhow};
use serde::Deserialize;

use crate::config::ServerConfig;

/// Errors from talking to sidle-server. `TokenMismatch` is broken out so
/// the toast layer in `main.rs` can show a "plug Kindle into sidle"
/// breadcrumb instead of the opaque "Failed: GET ... status code 403"
/// that users have to grep the log for.
#[derive(Debug)]
pub enum SidleError {
    /// Server returned 401 or 403 — the bearer token in our
    /// `etc/server.conf` no longer matches the one sidle-server is
    /// validating against (rotated `.server-token`, fresh install).
    /// User action is to re-deploy via the desktop app's KUAL button.
    TokenMismatch,
    Other(anyhow::Error),
}

impl fmt::Display for SidleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SidleError::TokenMismatch => {
                write!(f, "token rejected by sidle-server (401/403)")
            }
            SidleError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for SidleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SidleError::TokenMismatch => None,
            SidleError::Other(e) => e.source(),
        }
    }
}

impl From<anyhow::Error> for SidleError {
    fn from(e: anyhow::Error) -> Self {
        SidleError::Other(e)
    }
}

pub type Result<T> = std::result::Result<T, SidleError>;

/// Issue a GET against sidle-server with the token header, translating
/// `ureq::Error::Status(401|403)` to [`SidleError::TokenMismatch`].
/// Every other transport/status error becomes `SidleError::Other`.
fn get_with_token(url: &str, token: &str, timeout: Duration) -> Result<ureq::Response> {
    match ureq::get(url)
        .set("X-Sidle-Token", token)
        .timeout(timeout)
        .call()
    {
        Ok(res) => Ok(res),
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            Err(SidleError::TokenMismatch)
        }
        Err(e) => Err(anyhow!("GET {url}: {e}").into()),
    }
}

/// Timeout for the boot-time `list_books` request. Short so the boot
/// toast surfaces quickly when the server is down/wedged — anything
/// over a couple of seconds reads as "nothing happened" on e-ink and
/// the user gives up before any error renders. LAN-only, so 3s is
/// plenty for a healthy round-trip on a JSON-only endpoint.
const LIST_TIMEOUT: Duration = Duration::from_secs(3);
/// Timeout for cover fetches. The server reads sqlite + opens the
/// cover file from disk per request; first hit after wake can be slow.
/// Covers download serially today (priority #3 in the UI plan will
/// move them to per-page lazy), so one slow cover blocks the next —
/// 3s was too tight and produced grey placeholders for any book the
/// server didn't answer instantly. 15s gives the slow cases room.
const COVER_TIMEOUT: Duration = Duration::from_secs(15);
/// Cap per-cover bytes so a corrupt server response can't OOM us. Real
/// covers fit comfortably under 200KB; 8MB is wildly generous but still
/// bounded.
const COVER_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Length of the sha256 prefix sidle uses in on-device filenames
/// (`<basename>.<sha8>.kfx`). Must match `sidle_core::library::paths::
/// SHA_INFIX_LEN` — kept as a local const because `sidle-native` doesn't
/// depend on sidle-core (cross-compile boundary; sidle-core pulls in
/// rusqlite/image and would bloat the armv7l binary).
pub(crate) const SHA_INFIX_LEN: usize = 8;

#[derive(Debug, Deserialize, Clone)]
pub struct Book {
    pub id: i64,
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub language: String,
    /// Full sha256 of the KFX bytes (64 hex chars). Used to derive the
    /// on-device filename so it matches what sidle-tauri's USB push would
    /// write — otherwise USB-side delete sees the file as `NotOurs` and
    /// refuses to touch it. `#[serde(default)]` so older servers without
    /// the column still parse (download will then fail loudly).
    #[serde(default)]
    pub kfx_sha256: Option<String>,
}

pub fn list_books(cfg: &ServerConfig) -> Result<Vec<Book>> {
    let url = format!("http://{}:{}/list.json", cfg.host, cfg.port);
    let res = get_with_token(&url, &cfg.token, LIST_TIMEOUT)?;
    let body = res
        .into_string()
        .with_context(|| format!("read body of {url}"))?;
    let books: Vec<Book> =
        serde_json::from_str(&body).with_context(|| format!("parse {url}"))?;
    Ok(books)
}

pub fn fetch_cover(cfg: &ServerConfig, id: i64) -> Result<Vec<u8>> {
    let url = format!("http://{}:{}/cover/{}", cfg.host, cfg.port, id);
    let res = get_with_token(&url, &cfg.token, COVER_TIMEOUT)?;
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

pub fn download_book(cfg: &ServerConfig, book: &Book) -> Result<Download> {
    let url = format!("http://{}:{}/get/{}", cfg.host, cfg.port, book.id);
    // download uses the longer timeout — re-issue the request locally
    // instead of routing through `get_with_token` (which uses TIMEOUT).
    let res = match ureq::get(&url)
        .set("X-Sidle-Token", &cfg.token)
        .timeout(DOWNLOAD_TIMEOUT)
        .call()
    {
        Ok(res) => res,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            return Err(SidleError::TokenMismatch);
        }
        Err(e) => return Err(anyhow!("GET {url}: {e}").into()),
    };
    let cd_filename = parse_cd_filename(res.header("content-disposition"));
    let filename = pick_filename(cd_filename.as_deref(), book)?;
    let mut bytes = Vec::new();
    res.into_reader()
        .take(KFX_MAX_BYTES as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read body of {url}"))?;
    Ok(Download { filename, bytes })
}

/// Resolve the on-device filename. Prefers the server's
/// Content-Disposition when it already has the `<basename>.<sha8>.kfx`
/// shape; otherwise synthesizes the name locally from `book.kfx_sha256`.
/// Fails hard if neither source gives us a sha — writing to a name
/// without the sha8 infix would leave the file unrecognized by
/// sidle-tauri's USB-side delete (`NotOurs`), which is worse than
/// surfacing the failure to the user.
fn pick_filename(cd_filename: Option<&str>, book: &Book) -> Result<String> {
    if let Some(name) = cd_filename {
        if looks_like_sha8_kfx(name) {
            return Ok(name.to_string());
        }
    }
    let sha = book.kfx_sha256.as_deref().ok_or_else(|| {
        anyhow!(
            "server omitted kfx_sha256 from /list.json and Content-Disposition \
             lacks a sha8-tagged filename; refusing to save to a name that \
             sidle would treat as foreign on the device"
        )
    })?;
    if sha.len() < SHA_INFIX_LEN {
        return Err(anyhow!(
            "kfx_sha256 from /list.json is {} chars, expected at least {}",
            sha.len(),
            SHA_INFIX_LEN,
        )
        .into());
    }
    // If C-D gave us *some* name (just not the sha8-tagged shape), reuse
    // its stem; otherwise fall back to the book's title. Either way the
    // on-device identity is the sha8 — sidle-tauri keys on that, not on
    // the basename, so a divergent stem here is cosmetic.
    let stem = cd_filename
        .and_then(|n| n.strip_suffix(".kfx"))
        .filter(|s| !s.is_empty())
        .unwrap_or(book.title.as_str());
    Ok(format!("{stem}.{}.kfx", &sha[..SHA_INFIX_LEN]))
}

fn looks_like_sha8_kfx(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".kfx") else { return false; };
    let Some((_, sha)) = stem.rsplit_once('.') else { return false; };
    sha.len() == SHA_INFIX_LEN && sha.chars().all(|c| c.is_ascii_hexdigit())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_book(sha: Option<&str>) -> Book {
        Book {
            id: 7,
            title: "Sample Title".into(),
            author: String::new(),
            language: String::new(),
            kfx_sha256: sha.map(str::to_string),
        }
    }

    const FULL_SHA: &str = "deadbeefcafef00d1234567890abcdefdeadbeefcafef00d1234567890abcdef";

    #[test]
    fn prefers_cd_when_already_sha8_tagged() {
        let book = make_book(Some(FULL_SHA));
        let name = pick_filename(Some("[A] Title (2024).deadbeef.kfx"), &book).unwrap();
        assert_eq!(name, "[A] Title (2024).deadbeef.kfx");
    }

    #[test]
    fn synthesizes_from_sha_when_cd_absent() {
        let book = make_book(Some(FULL_SHA));
        let name = pick_filename(None, &book).unwrap();
        assert_eq!(name, "Sample Title.deadbeef.kfx");
    }

    #[test]
    fn synthesizes_using_cd_stem_when_present_but_untagged() {
        let book = make_book(Some(FULL_SHA));
        let name = pick_filename(Some("[A] Title (2024).kfx"), &book).unwrap();
        assert_eq!(name, "[A] Title (2024).deadbeef.kfx");
    }

    #[test]
    fn fails_without_sha_or_cd() {
        let book = make_book(None);
        assert!(pick_filename(None, &book).is_err());
    }

    #[test]
    fn fails_without_sha_when_cd_is_untagged() {
        let book = make_book(None);
        assert!(pick_filename(Some("[A] Title.kfx"), &book).is_err());
    }

    #[test]
    fn looks_like_sha8_kfx_basics() {
        assert!(looks_like_sha8_kfx("foo.deadbeef.kfx"));
        assert!(!looks_like_sha8_kfx("foo.deadbeef.epub"));
        assert!(!looks_like_sha8_kfx("foo.kfx"));
        assert!(!looks_like_sha8_kfx("foo.deadbeeZ.kfx"));
        assert!(!looks_like_sha8_kfx("foo.deadbee.kfx"));
    }
}
