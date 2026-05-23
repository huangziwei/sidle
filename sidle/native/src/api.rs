//! Sidle-server HTTP client.
//!
//! Three endpoints, all token-gated, all sync via `ureq`:
//! - `GET /list.json`  → library as JSON (incl. the on-device save name)
//! - `GET /cover/{id}` → cover image bytes (M6)
//! - `GET /get/{id}`   → KFX bytes (M7)
//!
//! Token is sent as `X-Sidle-Token` header. The server also accepts
//! `?token=` query but the header is cleaner for programmatic clients.
//!
//! Book shape mirrors `sidle_core::library::db::BookRow`. The core display +
//! download fields are `id`/`title` (display), `kfx_sha256` (on-device dedupe),
//! and `device_filename` (the save name — `/get/{id}`'s Content-Disposition is
//! unusable here, see [`device_filename`]). The remaining metadata fields
//! (`author`, `language`, `publisher`, …) feed the picker's filter + sort
//! (`ui::sort`); the server already flattens them into `/list.json`, so reading
//! them is a client-only change. serde silently drops unknown JSON fields, so
//! this stays compatible if the server adds columns. The full shape lives at
//! sidle/core/src/library/db.rs.

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
///
/// Takes a shared [`ureq::Agent`] (built once in `main`) rather than the
/// `ureq::get()` convenience fn, which spins up a fresh connection pool per
/// call — that meant every one of the 9 covers on a page opened a *new* TCP
/// connection, re-waking the Kindle's power-saving radio each time. One agent
/// = HTTP keep-alive across the page's fetches over a single warm connection.
fn get_with_token(
    agent: &ureq::Agent,
    url: &str,
    token: &str,
    timeout: Duration,
) -> Result<ureq::Response> {
    match agent
        .get(url)
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
/// Timeout for cover fetches. Covers are now ~20KB grayscale thumbnails
/// (`?thumb=1`), so the common case is fast — but the server falls back to
/// the full-res cover (up to ~1MB) for any book whose thumbnail hasn't been
/// generated yet (boot backfill still running), and covers fetch serially, so
/// keep generous headroom rather than risk a grey placeholder on a slow one.
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
    /// Full sha256 of the KFX bytes (64 hex chars). The first 8 chars are
    /// matched against the sha8 infix of files already under
    /// `/mnt/us/documents/Sidle/` to hide books already on the device
    /// (`main.rs`'s download dedupe). `#[serde(default)]` so an older server
    /// without the column still parses.
    #[serde(default)]
    pub kfx_sha256: Option<String>,
    /// Canonical on-device filename (`<basename>.<sha8>.kfx`), computed
    /// server-side with the same rule sidle-tauri's USB push uses — so a LAN
    /// download lands under a byte-identical name and isn't flagged
    /// `NotOurs` by the USB-side delete. This is the name we save the
    /// download as; see [`device_filename`]. `#[serde(default)]` so an older
    /// server without the field still parses (download then fails loudly
    /// rather than guessing a divergent name).
    #[serde(default)]
    pub device_filename: Option<String>,

    // ---- Sort + facet metadata ----
    // Mirror the same-named columns on `db::BookRow`, which the server already
    // flattens into every `/list.json` entry (`server/src/lib.rs`
    // `BookListEntry`) — so consuming them needs no server/protocol change.
    // Each is `#[serde(default)]` for the same reason as the two fields above:
    // an older server that doesn't ship a column still parses, the field just
    // takes its type default. Read by `ui::sort` and `ui::filter`. (`published_at`
    // is deliberately absent — it's a desktop list column, not in the picker's
    // net sort keys or facets, so carrying it would be dead code.)
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub series_name: Option<String>,
    /// Position within the series; REAL on the server (half-numbers like 1.5).
    #[serde(default)]
    pub series_index: Option<f64>,
    #[serde(default)]
    pub file_size: i64,
    #[serde(default)]
    pub imported_at: String,
    /// User-defined tags. Server canonicalizes them (trimmed, lowercased,
    /// deduped, in-order); the `tags` facet (`ui::filter`) reads them as-is.
    #[serde(default)]
    pub tags: Vec<String>,
}

pub fn list_books(agent: &ureq::Agent, cfg: &ServerConfig) -> Result<Vec<Book>> {
    let url = format!("http://{}:{}/list.json", cfg.host, cfg.port);
    let res = get_with_token(agent, &url, &cfg.token, LIST_TIMEOUT)?;
    let body = res
        .into_string()
        .with_context(|| format!("read body of {url}"))?;
    let mut books: Vec<Book> =
        serde_json::from_str(&body).with_context(|| format!("parse {url}"))?;
    for book in &mut books {
        sanitize(book);
    }
    Ok(books)
}

/// Strip zero-width / format characters and trim the text fields the picker
/// sorts and facets on.
///
/// Why: some imported titles carry stray Unicode format characters — notably a
/// leading BOM (U+FEFF) — that the desktop's `localeCompare` silently ignores,
/// but the picker's code-point `str` ordering does not. A leading U+FEFF
/// (0xFEFF) sorts near the top of the BMP, so a BOM-prefixed title is shoved to
/// the *end* of a Title sort. That split the "文学少女" series on device: vol 07
/// (BOM buried mid-title, so it starts with `0`) sorted first while vols 01-08
/// (leading BOM) sorted last. Removing these characters makes code-point order
/// agree with the desktop here — after stripping, all eight titles start with
/// their digit. Display is unaffected (the characters are invisible).
///
/// This sanitizes the picker's in-memory copy only; the library DB is untouched
/// (and downloads key off `device_filename`, not these fields).
fn sanitize(book: &mut Book) {
    book.title = clean(&book.title);
    book.author = clean(&book.author);
    book.language = clean(&book.language);
    book.publisher = book.publisher.as_deref().map(clean);
    book.series_name = book.series_name.as_deref().map(clean);
    for tag in &mut book.tags {
        *tag = clean(tag);
    }
}

/// Drop [`is_ignorable`] characters anywhere in `s`, then trim surrounding
/// whitespace.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !is_ignorable(*c)).collect::<String>().trim().to_string()
}

/// Zero-width / formatting code points that carry no visible glyph and that
/// locale collation treats as ignorable: BOM / zero-width space family, bidi
/// marks, word joiner + invisible operators, and the soft hyphen.
fn is_ignorable(c: char) -> bool {
    matches!(c,
        '\u{00AD}'                  // soft hyphen
        | '\u{200B}'..='\u{200F}'   // ZWSP, ZWNJ, ZWJ, LRM, RLM
        | '\u{2060}'..='\u{2064}'   // word joiner + invisible operators
        | '\u{FEFF}'                // BOM / zero-width no-break space
    )
}

pub fn fetch_cover(agent: &ureq::Agent, cfg: &ServerConfig, id: i64) -> Result<Vec<u8>> {
    // `?thumb=1` → the server returns the small grayscale thumbnail produced
    // at import (see sidle_core::library::thumbnail) — ~20KB instead of the
    // full-res color cover. The server falls back to full-res if the thumbnail
    // isn't on disk yet, so this is always safe to request.
    let url = format!("http://{}:{}/cover/{}?thumb=1", cfg.host, cfg.port, id);
    let res = get_with_token(agent, &url, &cfg.token, COVER_TIMEOUT)?;
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

pub fn download_book(agent: &ureq::Agent, cfg: &ServerConfig, book: &Book) -> Result<Download> {
    // Resolve the on-device name first, from data the list endpoint already
    // gave us — so a row the server couldn't name fails before we spend the
    // download instead of after.
    let filename = device_filename(book)?;
    let url = format!("http://{}:{}/get/{}", cfg.host, cfg.port, book.id);
    // download uses the longer timeout — re-issue the request on the shared
    // agent instead of routing through `get_with_token` (which uses the short
    // cover/list timeout).
    let res = match agent
        .get(&url)
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
    let mut bytes = Vec::new();
    res.into_reader()
        .take(KFX_MAX_BYTES as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read body of {url}"))?;
    Ok(Download { filename, bytes })
}

/// The on-device filename, taken straight from `/list.json`'s
/// `device_filename`. The server computes it with the same
/// `kfx_device_filename` rule sidle-tauri's USB push uses, so a LAN download
/// and a USB push land under byte-identical names — which is what lets the
/// USB-side delete recognize a KUAL-downloaded file instead of treating it
/// as foreign (`NotOurs`).
///
/// We deliberately do NOT read this off the `/get/{id}` `Content-Disposition`
/// header: `ureq` discards header values containing non-ASCII bytes
/// (RFC 7230 field-vchar filter — see its own `test_iso8859_utf8_mixup`),
/// and every book in the library has a Japanese filename, so that header is
/// always invisible to us. Leaning on it is what made LAN downloads fall
/// back to a divergent title-only name.
///
/// Hard-errors rather than guessing when the field is absent or malformed: a
/// wrong name silently orphans the file on the device (USB delete won't find
/// it), which is worse than a visible failure in the toast banner.
fn device_filename(book: &Book) -> Result<String> {
    match book.device_filename.as_deref() {
        Some(name) if looks_like_sha8_kfx(name) => Ok(name.to_string()),
        Some(bad) => Err(anyhow!(
            "server sent an unrecognized device_filename {bad:?} for \"{}\" \
             (id {}); refusing to save under a name sidle wouldn't recognize \
             on the device",
            book.title,
            book.id,
        )
        .into()),
        None => Err(anyhow!(
            "server did not provide device_filename for \"{}\" (id {}) — \
             update sidle-server (Update KUAL in the desktop app)",
            book.title,
            book.id,
        )
        .into()),
    }
}

fn looks_like_sha8_kfx(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".kfx") else { return false; };
    let Some((_, sha)) = stem.rsplit_once('.') else { return false; };
    sha.len() == SHA_INFIX_LEN && sha.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_book(device_filename: Option<&str>) -> Book {
        Book {
            id: 7,
            title: "Sample Title".into(),
            // Irrelevant to filename resolution now — used only by main.rs's
            // on-device dedupe. Set to a plausible full sha for realism.
            kfx_sha256: Some(
                "deadbeefcafef00d1234567890abcdefdeadbeefcafef00d1234567890abcdef".into(),
            ),
            device_filename: device_filename.map(str::to_string),
            // Filter/sort metadata is irrelevant to filename resolution — these
            // tests predate it. Defaults keep the literal compiling.
            author: String::new(),
            language: String::new(),
            publisher: None,
            series_name: None,
            series_index: None,
            file_size: 0,
            imported_at: String::new(),
            tags: Vec::new(),
        }
    }

    #[test]
    fn uses_server_device_filename_verbatim() {
        // The non-ASCII name round-trips intact: it rides in the JSON body,
        // not a header, so ureq's ASCII-only header filter never sees it.
        let book = make_book(Some("[河野 裕] サクラダリセット５ ONE HAND EDEN.9ea26f33.kfx"));
        assert_eq!(
            device_filename(&book).unwrap(),
            "[河野 裕] サクラダリセット５ ONE HAND EDEN.9ea26f33.kfx"
        );
    }

    #[test]
    fn errors_when_device_filename_absent() {
        // Older server that doesn't send the field → loud failure, never a
        // guessed (and divergent) name.
        assert!(device_filename(&make_book(None)).is_err());
    }

    #[test]
    fn errors_on_untagged_device_filename() {
        // A name without the `.<sha8>.kfx` shape is rejected, not saved.
        assert!(device_filename(&make_book(Some("just-a-title.kfx"))).is_err());
        assert!(device_filename(&make_book(Some("foo.deadbeeZ.kfx"))).is_err());
    }

    #[test]
    fn looks_like_sha8_kfx_basics() {
        assert!(looks_like_sha8_kfx("foo.deadbeef.kfx"));
        assert!(!looks_like_sha8_kfx("foo.deadbeef.epub"));
        assert!(!looks_like_sha8_kfx("foo.kfx"));
        assert!(!looks_like_sha8_kfx("foo.deadbeeZ.kfx"));
        assert!(!looks_like_sha8_kfx("foo.deadbee.kfx"));
    }

    #[test]
    fn clean_strips_bom_and_zero_width() {
        // The real bug: a leading BOM (U+FEFF) made a title code-point-sort to
        // the end. Stripped → the digit leads → correct order.
        assert_eq!(clean("\u{FEFF}01 〝文学少女〟"), "01 〝文学少女〟");
        // A BOM buried mid-title (vol 07's case) is removed too.
        assert_eq!(clean("07 \u{FEFF}〝x"), "07 〝x");
        // Other zero-width junk + surrounding whitespace.
        assert_eq!(clean("  \u{200B}Hello\u{200D} "), "Hello");
        // Only-ignorables collapses to empty (→ facet sentinel downstream).
        assert_eq!(clean("\u{FEFF}\u{200B}"), "");
        // Plain text is untouched.
        assert_eq!(clean("Normal Title 7"), "Normal Title 7");
    }
}
