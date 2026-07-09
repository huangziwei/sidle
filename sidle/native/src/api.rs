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
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

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
pub(crate) fn get_with_token(
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
/// Timeout for cover fetches. Covers are now ~30–50KB color thumbnails
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

#[derive(Debug, Deserialize, Clone, Default)]
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
    /// Cover revision (ms mtime) from the server, folded into the on-device
    /// cover-cache filename (`cover_cache`) so a desktop recrawl that changes
    /// the cover bumps the rev and self-invalidates the stale thumbnail.
    /// `#[serde(default)]` → 0 against an older server, i.e. cache by id alone
    /// (the prior behavior).
    #[serde(default)]
    pub cover_rev: i64,
    /// Content revision of the KFX on the server: the file's ms mtime. Because
    /// `kfx_sha256` (hence `device_filename`) is a frozen identity, a desktop
    /// reconvert that rewrites the bytes leaves the on-device name unchanged —
    /// this is the only signal that the device copy is stale. The picker records
    /// it at download time (`crate::updates`) and re-pulls in place when the
    /// server's value moves. `#[serde(default)]` → 0 against an older server, i.e.
    /// "no update tracking" (the prior behavior).
    #[serde(default)]
    pub kfx_rev: i64,
    /// Canonical (space/punctuation-free, ASCII-folded, lowercase) search key the
    /// server derives from the book's editable romaji + auto-romanized
    /// series/publisher/tags + raw fields (`sidle_core::library::romaji::search_key`).
    /// The picker substring-matches the typed (also-`canon`'d) query against this —
    /// the on-screen Latin keyboard's whole reason for being. `#[serde(default)]`
    /// → `""` against an older server that doesn't ship it; [`crate::search`] then
    /// falls back to canon'ing the raw title/author on-device (Latin-only match).
    #[serde(default)]
    pub search_key: String,
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
    s.chars()
        .filter(|c| !is_ignorable(*c))
        .collect::<String>()
        .trim()
        .to_string()
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
    // `?thumb=1` → the server returns the small color thumbnail produced at
    // import (see sidle_core::library::thumbnail) — ~30–50KB instead of the
    // full-res cover. The server falls back to full-res if the thumbnail isn't
    // on disk yet, so this is always safe to request.
    let url = format!("http://{}:{}/cover/{}?thumb=1", cfg.host, cfg.port, id);
    let res = get_with_token(agent, &url, &cfg.token, COVER_TIMEOUT)?;
    let mut bytes = Vec::new();
    res.into_reader()
        .take(COVER_MAX_BYTES as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read body of {url}"))?;
    Ok(bytes)
}

/// Sanity cap on a single book download. A real KFX — even an image-heavy
/// manga — is far below this; it only bounds a runaway/misbehaving response
/// so a broken server can't fill the device. Deliberately generous: the old
/// 256 MB cap silently *truncated* larger books, because a `.take()` cutoff
/// reads as a clean EOF — the picker saved a 256 MB partial and reported
/// "Downloaded". The real short-transfer guard is now the `Content-Length`
/// check the caller runs against `expected_len`; this is just a storage
/// backstop. `u64` (not `usize`) so it stays a transfer bound, never a 32-bit
/// device's address-space limit.
const KFX_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub struct Download {
    pub filename: String,
    /// The response body, left unread. The caller streams it straight to disk
    /// instead of buffering the whole book in device RAM — a 300 MB+ book
    /// would otherwise risk an OOM on a 512 MB Kindle. Capped at
    /// `KFX_MAX_BYTES`.
    pub reader: Box<dyn Read + Send>,
    /// The server's `Content-Length`, if present. The caller checks the bytes
    /// actually written against it and fails a short transfer, so a dropped
    /// connection or a capped stream surfaces as an error instead of a
    /// silently-truncated file that reports success.
    pub expected_len: Option<u64>,
}

pub fn download_book(agent: &ureq::Agent, cfg: &ServerConfig, book: &Book) -> Result<Download> {
    // Resolve the on-device name first, from data the list endpoint already
    // gave us — so a row the server couldn't name fails before we spend the
    // download instead of after.
    let filename = device_filename(book)?;
    let url = format!("http://{}:{}/get/{}", cfg.host, cfg.port, book.id);
    // No overall request timeout: a big book over a sleepy radio can take
    // minutes, and an overall deadline would kill a transfer that's making
    // steady progress (the 256 MB-truncation bug's slow-path twin). The
    // session agent's per-read timeout (`timeout_read`, set in `main`) bounds a
    // genuinely stalled socket instead. The body is returned unread so the
    // caller can stream it to disk with live progress + cancel.
    let res = match agent.get(&url).set("X-Sidle-Token", &cfg.token).call() {
        Ok(res) => res,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            return Err(SidleError::TokenMismatch);
        }
        Err(e) => return Err(anyhow!("GET {url}: {e}").into()),
    };
    let expected_len = res
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok());
    let reader: Box<dyn Read + Send> = Box::new(res.into_reader().take(KFX_MAX_BYTES));
    Ok(Download {
        filename,
        reader,
        expected_len,
    })
}

/// Stream a [`download_book`] body to `target`, atomically: write a sibling
/// `.part`, verify the byte count against `Content-Length`, then rename over
/// `target`. The in-place update pass ([`crate::updates`]) uses this to overwrite
/// a book's *existing* on-device file — the frozen filename is kept, so the
/// Kindle keeps its `.sdr` (highlights + reading position). No live UI (batch
/// callers toast per book). A short transfer errors and leaves the original
/// file untouched, so a dropped radio can't truncate a book already on device.
pub fn stream_download(dl: Download, target: &std::path::Path) -> Result<u64> {
    use std::io::Write as _;
    let fname = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("book.kfx");
    let part = target.with_file_name(format!("{fname}.part"));
    let mut reader = dl.reader;
    let mut file =
        std::fs::File::create(&part).with_context(|| format!("create {}", part.display()))?;
    let mut written: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                return Err(anyhow!("read body for {}: {e}", target.display()).into());
            }
        };
        if let Err(e) = file.write_all(&buf[..n]) {
            let _ = std::fs::remove_file(&part);
            return Err(anyhow!("write {}: {e}", part.display()).into());
        }
        written += n as u64;
    }
    if let Some(expected) = dl.expected_len
        && written != expected
    {
        let _ = std::fs::remove_file(&part);
        return Err(anyhow!(
            "short transfer for {}: {written} of {expected} bytes",
            target.display()
        )
        .into());
    }
    let _ = file.flush();
    drop(file);
    std::fs::rename(&part, target)
        .with_context(|| format!("rename {} -> {}", part.display(), target.display()))?;
    Ok(written)
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
    let Some(stem) = name.strip_suffix(".kfx") else {
        return false;
    };
    let Some((_, sha)) = stem.rsplit_once('.') else {
        return false;
    };
    sha.len() == SHA_INFIX_LEN && sha.chars().all(|c| c.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// Annotation push — POST /sync/annotations (the LAN twin of a USB sync)
// ---------------------------------------------------------------------------

/// The push bundle: each `.sdr`'s reading-state sidecars (base64). Mirrors
/// `sidle-server`'s `SyncRequest` DTO. `device_serial` comes from `server.conf`
/// (`ServerConfig::serial`), written by the desktop app at install — so the
/// picker needs no on-device serial lookup.
#[derive(Serialize)]
struct SyncRequest {
    device_serial: String,
    sdrs: Vec<SyncSdr>,
}

#[derive(Serialize)]
struct SyncSdr {
    sdr_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    yjr_b64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    yjf_b64: Option<String>,
}

/// The server's import report, the subset the picker surfaces in a toast. serde
/// ignores the report fields we don't read, so this stays compatible as the
/// server's `DeviceImportReport` grows.
#[derive(Debug, Default, Deserialize)]
pub struct SyncReport {
    #[serde(default)]
    pub positions: usize,
    /// `.sdr` dirs whose book isn't in the library (highlights archived as
    /// orphans). Normally empty — everything under `documents/Sidle/` was
    /// sideloaded from the library — so a non-zero count is worth surfacing.
    #[serde(default)]
    pub unmatched: Vec<String>,
    #[serde(default)]
    pub annotations: SyncStats,
}

#[derive(Debug, Default, Deserialize)]
pub struct SyncStats {
    #[serde(default)]
    pub inserted: usize,
}

impl SyncReport {
    /// One-line toast summary, e.g. `annotation sync: 3 new, 2 positions`.
    /// `nothing new` when an idempotent re-sync changed nothing. A trailing
    /// `(N unmatched)` flags orphaned highlights when any.
    pub fn summary(&self) -> String {
        let new = self.annotations.inserted;

        let mut parts = Vec::new();
        if new > 0 {
            parts.push(format!("{new} new"));
        }
        if self.positions > 0 {
            parts.push(format!("{} positions", self.positions));
        }
        let mut s = if parts.is_empty() {
            "annotation sync: nothing new".to_string()
        } else {
            format!("annotation sync: {}", parts.join(", "))
        };
        if !self.unmatched.is_empty() {
            s.push_str(&format!(" ({} unmatched)", self.unmatched.len()));
        }
        s
    }
}

/// The import can rebuild a TextIndex per changed book server-side; give it
/// generous headroom over the list/cover timeouts (still LAN-only).
const SYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// Scan the on-device reading-state sidecars and push them to sidle-server's
/// `POST /sync/annotations` — the LAN twin of a USB sync. `sidle_dir` is
/// `/mnt/us/documents/Sidle` (the download dir). Returns the server's
/// [`SyncReport`].
///
/// Errors with a re-install breadcrumb if `server.conf` carries no `SERIAL=`
/// (a pre-sync install): annotations are keyed per device, so the serial is
/// mandatory for the push (but not for boot/list/download, hence it's optional
/// in [`ServerConfig`]).
pub fn push_annotations(
    agent: &ureq::Agent,
    cfg: &ServerConfig,
    sidle_dir: &Path,
) -> Result<SyncReport> {
    if cfg.serial.is_empty() {
        return Err(anyhow!(
            "server.conf has no SERIAL= — re-run Update KUAL in the desktop app \
             (annotations are keyed per device)"
        )
        .into());
    }

    let sdrs = collect_sidecars(sidle_dir)?;
    if sdrs.is_empty() {
        // Nothing on the device to sync — skip the round-trip, report empty.
        return Ok(SyncReport::default());
    }

    let req = SyncRequest {
        device_serial: cfg.serial.clone(),
        sdrs,
    };
    let body = serde_json::to_vec(&req).context("serialize sync request")?;

    let url = format!("http://{}:{}/sync/annotations", cfg.host, cfg.port);
    let res = match agent
        .post(&url)
        .set("X-Sidle-Token", &cfg.token)
        .set("Content-Type", "application/json")
        .timeout(SYNC_TIMEOUT)
        .send_bytes(&body)
    {
        Ok(res) => res,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            return Err(SidleError::TokenMismatch);
        }
        Err(e) => return Err(anyhow!("POST {url}: {e}").into()),
    };
    let body = res
        .into_string()
        .with_context(|| format!("read body of {url}"))?;
    let report: SyncReport = serde_json::from_str(&body).with_context(|| format!("parse {url}"))?;
    Ok(report)
}

/// Read the `.yjr`/`.yjf` sidecars from every `*.sdr` under `sidle_dir`, base64
/// each. Mirrors the device-side scan in
/// `sidle_core::library::ingest::import_from_device` (which sidle-native can't
/// call — no sidle-core dep across the cross-compile boundary). A `.sdr` with
/// neither sidecar (a pagination cache) is skipped.
fn collect_sidecars(sidle_dir: &Path) -> Result<Vec<SyncSdr>> {
    let mut sdrs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(sidle_dir) {
        for entry in entries.flatten() {
            let sdr = entry.path();
            if sdr.extension().and_then(|e| e.to_str()) != Some("sdr") {
                continue;
            }
            let yjr = read_sidecar(&sdr, ".yjr")?;
            let yjf = read_sidecar(&sdr, ".yjf")?;
            if yjr.is_none() && yjf.is_none() {
                continue; // pagination-cache .sdr — nothing to sync
            }
            let sdr_name = sdr
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            sdrs.push(SyncSdr {
                sdr_name,
                yjr_b64: yjr.map(|b| BASE64.encode(b)),
                yjf_b64: yjf.map(|b| BASE64.encode(b)),
            });
        }
    }

    Ok(sdrs)
}

/// The first file in `sdr_dir` whose name ends with `suffix` (e.g. `.yjr`),
/// read into bytes — matching `find_sidecar`'s `ends_with` rule in sidle-core.
fn read_sidecar(sdr_dir: &Path, suffix: &str) -> Result<Option<Vec<u8>>> {
    let Ok(entries) = std::fs::read_dir(sdr_dir) else {
        return Ok(None);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_match = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(suffix));
        if is_match {
            let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Decrypted-book push — POST /sync/book (the WiFi twin of the USB /dedrm pull)
// ---------------------------------------------------------------------------

/// What the server did with a pushed decrypted book.
pub enum BookPush {
    /// New to the library.
    Imported,
    /// Already present (matched by content hash) — a harmless re-push.
    Duplicate,
}

#[derive(Deserialize)]
struct BookPushReply {
    outcome: String,
}

/// Push one decrypted `.kfx-zip` to sidle-server's `POST /sync/book`, which
/// imports it exactly as the desktop's USB `/dedrm` auto-pull would (hash-
/// deduped, so USB and WiFi coexist). Streams the file straight from disk rather
/// than buffering the whole book in the Kindle's RAM.
pub fn push_book(agent: &ureq::Agent, cfg: &ServerConfig, path: &Path) -> Result<BookPush> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let url = format!("http://{}:{}/sync/book", cfg.host, cfg.port);
    let res = match agent
        .post(&url)
        .set("X-Sidle-Token", &cfg.token)
        .set("Content-Type", "application/octet-stream")
        .send(file)
    {
        Ok(res) => res,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            return Err(SidleError::TokenMismatch);
        }
        Err(e) => return Err(anyhow!("POST {url}: {e}").into()),
    };
    let body = res
        .into_string()
        .with_context(|| format!("read body of {url}"))?;
    let reply: BookPushReply =
        serde_json::from_str(&body).with_context(|| format!("parse {url}"))?;
    Ok(match reply.outcome.as_str() {
        "duplicate" => BookPush::Duplicate,
        _ => BookPush::Imported,
    })
}

// ---------------------------------------------------------------------------
// Misc backup push — POST /sync/misc (screenshots + KUAL logs, the WiFi backup)
// ---------------------------------------------------------------------------

/// The push bundle: each screenshot / KUAL log, base64 in JSON. Mirrors
/// `sidle-server`'s `MiscSyncRequest`. `device_serial` comes from `server.conf`.
#[derive(Serialize)]
struct MiscRequest {
    device_serial: String,
    files: Vec<MiscFile>,
}

#[derive(Serialize)]
struct MiscFile {
    name: String,
    data_b64: String,
}

/// The server's `MiscSyncResult` — what it backed up, for the picker's toast.
#[derive(Debug, Default, Deserialize)]
pub struct MiscReport {
    #[serde(default)]
    pub screenshots: usize,
    #[serde(default)]
    pub logs: usize,
}

impl MiscReport {
    /// A terse toast fragment like `2 screenshots, 1 log`, or `None` when the
    /// push backed nothing up (so the caller omits it entirely).
    pub fn summary(&self) -> Option<String> {
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let mut parts = Vec::new();
        if self.screenshots > 0 {
            parts.push(format!(
                "{} screenshot{}",
                self.screenshots,
                plural(self.screenshots)
            ));
        }
        if self.logs > 0 {
            parts.push(format!("{} log{}", self.logs, plural(self.logs)));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
        }
    }
}

/// Push the Kindle's screenshots + KUAL logs to `POST /sync/misc` — the WiFi
/// backup the desktop "Misc." tab views. `us_root` is `/mnt/us`. On a successful
/// push the screenshots are **deleted from the device** (they're safely backed
/// up now, and a Kindle's screenshot folder is scratch space) — logs stay, since
/// they're the live append-only diagnostic trail. That clear-on-sync also means
/// each Sync only re-uploads screenshots taken since the last one. Returns the
/// server's [`MiscReport`].
pub fn push_misc(agent: &ureq::Agent, cfg: &ServerConfig, us_root: &Path) -> Result<MiscReport> {
    if cfg.serial.is_empty() {
        return Err(anyhow!(
            "server.conf has no SERIAL= — re-run Update KUAL in the desktop app \
             (backups are keyed per device)"
        )
        .into());
    }

    let (files, screenshot_paths) = collect_misc_files(us_root);
    if files.is_empty() {
        // Nothing on the device to back up — skip the round-trip.
        return Ok(MiscReport::default());
    }

    let req = MiscRequest {
        device_serial: cfg.serial.clone(),
        files,
    };
    let body = serde_json::to_vec(&req).context("serialize misc request")?;

    let url = format!("http://{}:{}/sync/misc", cfg.host, cfg.port);
    let res = match agent
        .post(&url)
        .set("X-Sidle-Token", &cfg.token)
        .set("Content-Type", "application/json")
        .timeout(SYNC_TIMEOUT)
        .send_bytes(&body)
    {
        Ok(res) => res,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            return Err(SidleError::TokenMismatch);
        }
        Err(e) => return Err(anyhow!("POST {url}: {e}").into()),
    };
    let body = res
        .into_string()
        .with_context(|| format!("read body of {url}"))?;
    let report: MiscReport = serde_json::from_str(&body).with_context(|| format!("parse {url}"))?;

    // The server has them now (a non-2xx would have returned above) — clear the
    // screenshots off the device. Only screenshots: logs are left in place. Best-
    // effort; a failed unlink just leaves that file for the next Sync to re-push.
    // eprintln lands in `sidle-native.log` via sidle.sh's `2>>` redirect.
    for path in &screenshot_paths {
        if let Err(e) = std::fs::remove_file(path) {
            eprintln!(
                "[sidle/misc] delete {} after sync failed: {e}",
                path.display()
            );
        }
    }

    Ok(report)
}

/// Read every screenshot (`screenshot*` under `screenshots/` and the USB root)
/// and KUAL log (`*.log` at the root) beneath `us_root`, base64 each. Dedups by
/// filename so a screenshot in both `screenshots/` and the root is sent once.
/// Best-effort per file: an unreadable / empty one is skipped. Returns the push
/// bundle plus the on-device paths of the screenshots in it — those get deleted
/// after a successful push (see [`push_misc`]).
fn collect_misc_files(us_root: &Path) -> (Vec<MiscFile>, Vec<std::path::PathBuf>) {
    let mut files = Vec::new();
    let mut screenshot_paths = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // `screenshots/` — everyone's screenshots (Sidle's own + newer stock firmware).
    gather_misc(
        &us_root.join("screenshots"),
        true,
        &mut files,
        &mut screenshot_paths,
        &mut seen,
    );
    // USB root — KOA2's stock screenshots live loose here; so do our KUAL logs.
    gather_misc(us_root, false, &mut files, &mut screenshot_paths, &mut seen);
    (files, screenshot_paths)
}

/// Append matching files from one directory (non-recursive) to `out`. With
/// `shots_only`, only screenshots match; otherwise screenshots AND logs. `seen`
/// dedups by filename across the two scanned directories. A screenshot's device
/// path is recorded in `screenshot_paths` so [`push_misc`] can delete it after a
/// successful push (logs are never recorded — they stay on the device).
fn gather_misc(
    dir: &Path,
    shots_only: bool,
    out: &mut Vec<MiscFile>,
    screenshot_paths: &mut Vec<std::path::PathBuf>,
    seen: &mut std::collections::HashSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_shot = is_screenshot(&name);
        let wanted = if shots_only {
            is_shot
        } else {
            is_shot || is_log(&name)
        };
        if !wanted || seen.contains(&name) {
            continue;
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue; // only files carry bytes
        }
        let path = entry.path();
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => {
                seen.insert(name.clone());
                if is_shot {
                    screenshot_paths.push(path);
                }
                out.push(MiscFile {
                    name,
                    data_b64: BASE64.encode(&bytes),
                });
            }
            _ => {}
        }
    }
}

/// A Kindle screenshot filename (either generation), case-insensitive. Mirrors
/// `sidle_core::library::device_backup::classify_misc` — the native crate can't
/// depend on sidle-core across the cross-compile boundary.
fn is_screenshot(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("screenshot") && !lower.ends_with(".partial")
}

/// A KUAL log filename (`*.log`), case-insensitive. Mirrors core's `classify_misc`.
fn is_log(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".log")
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
            cover_rev: 0,
            kfx_rev: 0,
            search_key: String::new(),
        }
    }

    #[test]
    fn uses_server_device_filename_verbatim() {
        // The non-ASCII name round-trips intact: it rides in the JSON body,
        // not a header, so ureq's ASCII-only header filter never sees it.
        let book = make_book(Some(
            "[河野 裕] サクラダリセット５ ONE HAND EDEN.9ea26f33.kfx",
        ));
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

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sidle-sync-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn collect_sidecars_reads_sidecars_and_skips_pagination_cache() {
        let base = scratch("collect");
        let docs = base.join("documents");
        let sidle = docs.join("Sidle");

        // A .sdr with both sidecars (annotations + last-read position).
        let sdr = sidle.join("book.deadbeef.sdr");
        std::fs::create_dir_all(&sdr).unwrap();
        std::fs::write(sdr.join("book.deadbeef0000.yjr"), b"yjr-bytes").unwrap();
        std::fs::write(sdr.join("book.deadbeef0000.yjf"), b"yjf-bytes").unwrap();
        // A pagination-cache .sdr (neither sidecar) — must be skipped.
        let cache = sidle.join("other.cafe.sdr");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("page.cache"), b"x").unwrap();

        let sdrs = collect_sidecars(&sidle).unwrap();
        assert_eq!(sdrs.len(), 1, "pagination-cache .sdr should be skipped");
        assert_eq!(sdrs[0].sdr_name, "book.deadbeef.sdr");
        let yjr_expected = BASE64.encode(b"yjr-bytes");
        let yjf_expected = BASE64.encode(b"yjf-bytes");
        assert_eq!(sdrs[0].yjr_b64.as_deref(), Some(yjr_expected.as_str()));
        assert_eq!(sdrs[0].yjf_b64.as_deref(), Some(yjf_expected.as_str()));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn collect_sidecars_empty_when_no_tree() {
        let base = scratch("empty");
        // documents/Sidle doesn't exist → empty bundle, no error.
        let sdrs = collect_sidecars(&base.join("documents/Sidle")).unwrap();
        assert!(sdrs.is_empty());
    }

    #[test]
    fn collect_misc_files_scans_both_locations_and_dedups() {
        let us = scratch("misc");
        std::fs::create_dir_all(us.join("screenshots")).unwrap();
        // Newer-style screenshots under screenshots/.
        std::fs::write(us.join("screenshots/screenshot_100.png"), b"A").unwrap();
        std::fs::write(us.join("screenshots/screenshot_200.png"), b"B").unwrap();
        // KOA2 stock capture loose in the root + a KUAL log; and one name that
        // appears in BOTH screenshots/ and the root (must be sent only once).
        std::fs::write(us.join("Screenshot_root.png"), b"C").unwrap();
        std::fs::write(us.join("sidle-native.log"), b"log\n").unwrap();
        std::fs::write(us.join("screenshot_100.png"), b"DUP").unwrap();
        // Unrelated root files that must be ignored.
        std::fs::write(us.join("version.txt"), b"5.16").unwrap();

        let (files, screenshot_paths) = collect_misc_files(&us);
        let mut names: Vec<_> = files.iter().map(|f| f.name.clone()).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "Screenshot_root.png",
                "screenshot_100.png",
                "screenshot_200.png",
                "sidle-native.log",
            ],
            "both dirs scanned, dup name collapsed, non-misc ignored"
        );
        // The screenshots/ copy wins the dedup (its bytes, not the root DUP).
        let hundred = files
            .iter()
            .find(|f| f.name == "screenshot_100.png")
            .unwrap();
        assert_eq!(hundred.data_b64, BASE64.encode(b"A"));

        // Only screenshots are queued for on-device deletion — never the log.
        let mut del: Vec<_> = screenshot_paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        del.sort();
        assert_eq!(
            del,
            vec![
                "Screenshot_root.png",
                "screenshot_100.png",
                "screenshot_200.png"
            ],
            "logs must not be scheduled for deletion"
        );

        let _ = std::fs::remove_dir_all(&us);
    }

    #[test]
    fn misc_report_summary() {
        assert_eq!(MiscReport::default().summary(), None);
        let r = MiscReport {
            screenshots: 2,
            logs: 1,
        };
        assert_eq!(r.summary().as_deref(), Some("2 screenshots, 1 log"));
        let r = MiscReport {
            screenshots: 1,
            logs: 0,
        };
        assert_eq!(r.summary().as_deref(), Some("1 screenshot"));
    }

    #[test]
    fn summary_reports_counts_and_nothing_new() {
        let r = SyncReport::default();
        assert_eq!(r.summary(), "annotation sync: nothing new");

        let mut r = SyncReport::default();
        r.annotations.inserted = 3;
        r.positions = 2;
        assert_eq!(r.summary(), "annotation sync: 3 new, 2 positions");

        // Orphaned highlights flagged with a trailing count.
        let mut r = SyncReport::default();
        r.annotations.inserted = 2;
        r.unmatched = vec!["a.sdr".into(), "b.sdr".into()];
        assert_eq!(r.summary(), "annotation sync: 2 new (2 unmatched)");
        // Unmatched-only (nothing imported) still reads sensibly.
        let r = SyncReport {
            unmatched: vec!["a.sdr".into()],
            ..Default::default()
        };
        assert_eq!(r.summary(), "annotation sync: nothing new (1 unmatched)");
    }
}
