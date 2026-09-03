//! Sidle-server HTTPS client.
//!
//! Three endpoints, all token-gated, all sync via `ureq`:

use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::config::ServerConfig;

/// Errors from talking to sidle-server.
#[derive(Debug)]
pub enum SidleError {
    /// Server returned 401 or 403 — the bearer token in our
    /// `etc/server.conf` no longer matches the one sidle-server is
    /// validating against (rotated `.server-token`, fresh install).
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

/// The CA the picker pins, pushed by the desktop app's install alongside
pub const CA_PATH: &str = "/mnt/us/extensions/sidle/etc/ca.pem";

/// Build the one shared agent: TLS, with our CA as the **sole** trust root.
pub fn build_agent(
    configure: impl FnOnce(
        ureq::config::ConfigBuilder<ureq::typestate::AgentScope>,
    ) -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope>,
) -> Result<ureq::Agent> {
    use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};

    let pem = std::fs::read(CA_PATH).with_context(|| {
        format!("read {CA_PATH} — reinstall from the desktop app to place the CA")
    })?;
    let ca = Certificate::from_pem(&pem)
        .map_err(|e| anyhow!("parse {CA_PATH}: {e} — the CA on device is not a usable PEM"))?;

    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .unversioned_rustls_crypto_provider(std::sync::Arc::new(rustls_rustcrypto::provider()))
        .root_certs(RootCerts::Specific(std::sync::Arc::new(vec![ca])))
        .build();

    let config = configure(ureq::Agent::config_builder().tls_config(tls)).build();
    Ok(ureq::Agent::new_with_config(config))
}

/// Issue a GET against sidle-server with the token header, translating
/// `ureq::Error::Status(401|403)` to [`SidleError::TokenMismatch`].
/// Every other transport/status error becomes `SidleError::Other`.
pub(crate) fn get_with_token(
    agent: &ureq::Agent,
    url: &str,
    token: &str,
    timeout: Duration,
) -> Result<Response> {
    match agent
        .get(url)
        .header("X-Sidle-Token", token)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
    {
        Ok(res) => Ok(res),
        Err(ureq::Error::StatusCode(code)) if code == 401 || code == 403 => {
            Err(SidleError::TokenMismatch)
        }
        Err(e) => Err(anyhow!("GET {url}: {e}").into()),
    }
}

/// What ureq 3 hands back. Named once here so the dozen call sites below do not
/// each spell out the generic — and so a future ureq change is one edit.
pub(crate) type Response = ureq::http::Response<ureq::Body>;

/// Read a whole response body as text, bounded.
pub(crate) fn read_text(res: &mut Response, limit: usize) -> anyhow::Result<String> {
    res.body_mut()
        .with_config()
        .limit(limit as u64)
        .read_to_string()
        .map_err(|e| anyhow!("read response body: {e}"))
}

/// Cap for the JSON control responses (`/list.json`, the sync endpoints'
/// receipts). Generous for a library listing of a few thousand books, and far
/// below anything that would trouble the device.
pub(crate) const JSON_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Global timeout for one [`is_sidle_server`] request.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether `host:port` presents a leaf issued by [`CA_PATH`]'s CA.
///
/// `agent` trusts that one root, making the handshake the identity check.
pub fn is_sidle_server(agent: &ureq::Agent, host: &str, port: u16) -> bool {
    let url = format!("https://{host}:{port}/");
    agent
        .get(&url)
        .config()
        .timeout_global(Some(PROBE_TIMEOUT))
        .build()
        .call()
        .is_ok()
}

/// Timeout for the boot-time `list_books` request. Short so the boot
const LIST_TIMEOUT: Duration = Duration::from_secs(3);
/// Timeout for cover fetches. Covers are now ~30–50KB color thumbnails
const COVER_TIMEOUT: Duration = Duration::from_secs(15);
/// Cap per-cover bytes so a corrupt server response can't OOM us. Real
/// covers fit comfortably under 200KB; 8MB is wildly generous but still
/// bounded.
const COVER_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Length of the sha256 prefix sidle uses in on-device filenames
/// (`<basename>.<sha8>.kfx`). Must match `sidle_core::library::paths::
pub(crate) const SHA_INFIX_LEN: usize = 8;

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Book {
    pub id: i64,
    pub title: String,
    /// Full sha256 of the KFX bytes (64 hex chars). Its first 8 match the sha8 infix
    /// of files already on the device, which is how a held book is hidden.
    #[serde(default)]
    pub kfx_sha256: Option<String>,
    /// Canonical on-device filename (`<basename>.<sha8>.kfx`), computed
    #[serde(default)]
    pub device_filename: Option<String>,

    // ---- Sort + facet metadata ----
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
    /// Conversion direction, `"<source>_to_<target>"` (`"pdf_to_kfx"`,
    /// `"epub_to_kfx"`, `"kfx_to_epub"`) — the only record of which format a book
    /// was imported *from*, which is what the Format facet groups by.
    #[serde(default)]
    pub kind: Option<String>,
    /// The content_id baked into the KFX Sidle pushed. The device names this
    #[serde(default)]
    pub asin: Option<String>,
    #[serde(default)]
    pub imported_at: String,
    /// User-defined tags. Server canonicalizes them (trimmed, lowercased,
    /// deduped, in-order); the `tags` facet (`ui::filter`) reads them as-is.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Cover revision (ms mtime) from the server, folded into the on-device
    /// cover-cache filename (`cover_cache`) so a desktop recrawl that changes
    /// the cover bumps the rev and self-invalidates the stale thumbnail.
    #[serde(default)]
    pub cover_rev: i64,
    /// Content revision of the KFX on the server: the file's ms mtime. Because
    #[serde(default)]
    pub kfx_rev: i64,
    /// Canonical (space/punctuation-free, ASCII-folded, lowercase) search key the
    /// server derives from the book's editable romaji + auto-romanized
    /// series/publisher/tags + raw fields (`sidle_core::library::romaji::search_key`).
    #[serde(default)]
    pub search_key: String,
}

impl Book {
    /// The format this book was imported *from* — `"PDF"`, `"EPUB"` or `"KFX"`.
    pub fn source_format(&self) -> &'static str {
        match self
            .kind
            .as_deref()
            .unwrap_or("epub_to_kfx")
            .split("_to_")
            .next()
            .unwrap_or("epub")
        {
            "pdf" => "PDF",
            "kfx" => "KFX",
            _ => "EPUB",
        }
    }
}

pub fn list_books(agent: &ureq::Agent, cfg: &ServerConfig) -> Result<Vec<Book>> {
    let url = format!("https://{}:{}/list.json", cfg.host, cfg.port);
    let mut res = get_with_token(agent, &url, &cfg.token, LIST_TIMEOUT)?;
    let body =
        read_text(&mut res, JSON_MAX_BYTES).with_context(|| format!("read body of {url}"))?;
    let mut books: Vec<Book> =
        serde_json::from_str(&body).with_context(|| format!("parse {url}"))?;
    for book in &mut books {
        sanitize(book);
    }
    Ok(books)
}

/// Strip zero-width / format characters and trim the text fields the picker
/// sorts and facets on.
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

/// Drop [`crate::font::is_invisible`] characters anywhere in `s`, then trim
fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !crate::font::is_invisible(*c))
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn fetch_cover(agent: &ureq::Agent, cfg: &ServerConfig, id: i64) -> Result<Vec<u8>> {
    // `?thumb=1` asks for the small colour thumbnail made at import, ~30–50 KB. The
    // server falls back to full-res when it is not on disk yet.
    let url = format!("https://{}:{}/cover/{}?thumb=1", cfg.host, cfg.port, id);
    let mut res = get_with_token(agent, &url, &cfg.token, COVER_TIMEOUT)?;
    let mut bytes = Vec::new();
    res.body_mut()
        .as_reader()
        .take(COVER_MAX_BYTES as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read body of {url}"))?;
    Ok(bytes)
}

/// Sanity cap on a single book download. A real KFX — even an image-heavy
const KFX_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub struct Download {
    pub filename: String,
    /// The response body, left unread. The caller streams it straight to disk
    pub reader: Box<dyn Read + Send>,
    /// The server's `Content-Length`, if present. The caller checks the bytes
    pub expected_len: Option<u64>,
}

pub fn download_book(agent: &ureq::Agent, cfg: &ServerConfig, book: &Book) -> Result<Download> {
    // Resolve the on-device name first, from data the list endpoint already
    // gave us — so a row the server couldn't name fails before we spend the
    // download instead of after.
    let filename = device_filename(book)?;
    let url = format!("https://{}:{}/get/{}", cfg.host, cfg.port, book.id);
    // No overall request timeout: a big book over a sleepy radio can take
    let res = match agent.get(&url).header("X-Sidle-Token", &cfg.token).call() {
        Ok(res) => res,
        Err(ureq::Error::StatusCode(code)) if code == 401 || code == 403 => {
            return Err(SidleError::TokenMismatch);
        }
        Err(e) => return Err(anyhow!("GET {url}: {e}").into()),
    };
    let expected_len = res
        .headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    // `into_reader` consumes the response, so the length has to come off the
    // headers first — reversing these silently drops the size check that makes
    // `stream_download` able to tell a finished transfer from a truncated one.
    let reader: Box<dyn Read + Send> = Box::new(res.into_body().into_reader().take(KFX_MAX_BYTES));
    Ok(Download {
        filename,
        reader,
        expected_len,
    })
}

/// Fetch the reading-state sidecar the library holds for one book and write it
/// beside the freshly downloaded file.
pub fn pull_sidecar(
    agent: &ureq::Agent,
    cfg: &ServerConfig,
    book_id: i64,
    sidle_dir: &Path,
    device_filename: &str,
) -> Result<bool> {
    let Some(stem) = device_filename.strip_suffix(".kfx") else {
        return Ok(false);
    };
    let url = format!(
        "https://{}:{}/sidecar/{book_id}?serial={}",
        cfg.host, cfg.port, cfg.serial
    );
    let mut res = agent
        .get(&url)
        .header("X-Sidle-Token", &cfg.token)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    // 204: the library holds no annotations for this book, which is the common
    // case and not a failure.
    if res.status() == 204 {
        return Ok(false);
    }
    let mut bytes = Vec::new();
    res.body_mut()
        .as_reader()
        .take(SIDECAR_MAX_BYTES)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read body of {url}"))?;
    if bytes.is_empty() {
        return Ok(false);
    }

    let dir = sidle_dir.join(format!("{stem}.sdr"));
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{stem}.yjr"));
    std::fs::write(&path, &bytes).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// A reading-state sidecar is records, not content — tens of KB for a heavily
/// annotated book. The cap is generous and only there so a wrong route or a
/// captive-portal page cannot be written into the `.sdr` as one.
const SIDECAR_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Stream a [`download_book`] body to `target`, atomically: write a sibling
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
             update sidle-server (Update on Kindle in the desktop app)",
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
#[derive(Serialize)]
struct SyncRequest {
    device_serial: String,
    sdrs: Vec<SyncSdr>,
    /// Handwritten ink drawn on sideloaded books, one entry per host book.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    inks: Vec<SyncInk>,
}

#[derive(Serialize)]
struct SyncInk {
    asin: String,
    nbk_b64: String,
}

#[derive(Serialize)]
struct SyncSdr {
    sdr_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    yjr_b64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    yjf_b64: Option<String>,
    /// The sidecars' own filenames, so the server can address a write-back
    /// without inventing the device-specific infix they carry.
    #[serde(skip_serializing_if = "Option::is_none")]
    yjr_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    yjf_name: Option<String>,
}

/// A sidecar the desktop wants written here, from the sync response.
#[derive(Debug, Deserialize)]
pub struct OutgoingSdr {
    sdr_name: String,
    file_name: String,
    yjr_b64: String,
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
    /// Orphaned `.sdr` dirs pruned off the device this sync. Set locally by
    /// [`push_annotations`], not by the server, so it survives the early exit.
    #[serde(default)]
    pub pruned: usize,
    /// Sidecars the desktop wants written onto this device — highlights made in
    /// Sidle's reader coming the other way. Consumed by [`push_annotations`] and
    /// replaced by [`Self::written`].
    #[serde(default)]
    pub write: Vec<OutgoingSdr>,
    /// How many of those actually landed. Set locally, like `pruned`.
    #[serde(default)]
    pub written: usize,
    /// Ink pages the library decoded out of the notebooks we just sent. The
    #[serde(default)]
    pub ink_pages: usize,
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
        if self.pruned > 0 {
            parts.push(format!("{} stale removed", self.pruned));
        }
        if self.written > 0 {
            parts.push(format!("{} sent here", self.written));
        }
        if self.ink_pages > 0 {
            let plural = if self.ink_pages == 1 { "" } else { "s" };
            parts.push(format!("{} ink page{plural}", self.ink_pages));
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

/// `GET /sync/annotations` — the ink content shas the library already decoded
/// off this device, `{asin: nbk_sha}`.
#[derive(Deserialize, Default)]
struct InkManifest {
    #[serde(default)]
    ink: HashMap<String, String>,
}

/// `GET /sync/notebooks` — `{uuid: nbk_sha}` for every notebook the library
/// holds. Not device-keyed: a notebook is one entity wherever it was written.
#[derive(Deserialize, Default)]
struct NotebookManifest {
    #[serde(default)]
    notebooks: HashMap<String, String>,
}

/// Fetch a sync route's "what do you already have?" manifest.
fn fetch_manifest<T: for<'de> Deserialize<'de> + Default>(
    agent: &ureq::Agent,
    cfg: &ServerConfig,
    url: &str,
    skip: bool,
) -> T {
    if skip {
        return T::default();
    }
    let res = agent
        .get(url)
        .query("device_serial", &cfg.serial)
        .header("X-Sidle-Token", &cfg.token)
        .config()
        .timeout_global(Some(SYNC_TIMEOUT))
        .build()
        .call();
    let text = match res {
        Ok(mut r) => match read_text(&mut r, JSON_MAX_BYTES) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[sidle/sync] read GET {url} failed ({e}) — sending everything");
                return T::default();
            }
        },
        Err(e) => {
            eprintln!("[sidle/sync] GET {url} failed ({e}) — sending everything");
            return T::default();
        }
    };
    serde_json::from_str(&text).unwrap_or_else(|e| {
        eprintln!("[sidle/sync] parse GET {url} failed ({e}) — sending everything");
        T::default()
    })
}

/// Scan the on-device reading-state sidecars and push them to sidle-server's
pub fn push_annotations(
    agent: &ureq::Agent,
    cfg: &ServerConfig,
    sidle_dir: &Path,
    ink: &[crate::handwriting::Nbk],
) -> Result<SyncReport> {
    if cfg.serial.is_empty() {
        return Err(anyhow!(
            "server.conf has no SERIAL= — re-run Update on Kindle in the desktop app \
             (annotations are keyed per device)"
        )
        .into());
    }

    let url = format!("https://{}:{}/sync/annotations", cfg.host, cfg.port);
    let (sdrs, pruned) = collect_sidecars(sidle_dir)?;
    // Ask before sending: the library reports the ink it has already decoded off
    // this device, and anything matching is left where it is. An `nbk` runs tens
    // of KB and a book's ink rarely changes again once it's drawn.
    let have: InkManifest = fetch_manifest(agent, cfg, &url, ink.is_empty());
    let inks: Vec<SyncInk> = ink
        .iter()
        .filter(|n| have.ink.get(&n.id) != Some(&n.sha))
        .filter_map(|n| {
            // Re-read now that we know it's going: the scan hashed and released
            // the bytes so a no-op sync never holds a notebook in memory.
            match std::fs::read(&n.path) {
                Ok(bytes) => Some(SyncInk {
                    asin: n.id.clone(),
                    nbk_b64: BASE64.encode(bytes),
                }),
                Err(e) => {
                    eprintln!("[sidle/ink] read {} failed: {e}", n.path.display());
                    None
                }
            }
        })
        .collect();

    if sdrs.is_empty() && inks.is_empty() {
        // Nothing live to sync — skip the round-trip, but still report any
        // orphaned copies we pruned off the device this pass.
        return Ok(SyncReport {
            pruned,
            ..Default::default()
        });
    }

    let req = SyncRequest {
        device_serial: cfg.serial.clone(),
        sdrs,
        inks,
    };
    let body = serde_json::to_vec(&req).context("serialize sync request")?;

    let mut res = match agent
        .post(&url)
        .header("X-Sidle-Token", &cfg.token)
        .header("Content-Type", "application/json")
        .config()
        .timeout_global(Some(SYNC_TIMEOUT))
        .build()
        .send(&body)
    {
        Ok(res) => res,
        Err(ureq::Error::StatusCode(code)) if code == 401 || code == 403 => {
            return Err(SidleError::TokenMismatch);
        }
        Err(e) => return Err(anyhow!("POST {url}: {e}").into()),
    };
    let body =
        read_text(&mut res, JSON_MAX_BYTES).with_context(|| format!("read body of {url}"))?;
    let mut report: SyncReport =
        serde_json::from_str(&body).with_context(|| format!("parse {url}"))?;
    // `pruned` is device-side hygiene, not in the server's report — fold it in
    // so the sync toast can surface "N stale removed".
    report.pruned = pruned;
    // The other direction: write the sidecars the desktop composed for us. This
    // device owns the filesystem, so it does the writing; the desktop only
    // decided what should be in them.
    report.written = write_incoming_sidecars(sidle_dir, &report.write);
    report.write = Vec::new();
    Ok(report)
}

/// Write the sidecars the desktop sent back, returning how many landed.
///
/// Best-effort per file: a sidecar that fails to write is logged and skipped,
/// never fatal — the pull half of this sync already succeeded, and the desktop
/// will offer the same file again next time.
///
/// Only ever writes into an existing `.sdr`; it never creates one. A directory
/// that isn't there means the device hasn't opened that book, and a sidecar
/// sitting in a folder the reader never made is a file nothing will read.
fn write_incoming_sidecars(sidle_dir: &Path, outgoing: &[OutgoingSdr]) -> usize {
    let mut written = 0;
    for item in outgoing {
        let dir = sidle_dir.join(&item.sdr_name);
        if !dir.is_dir() {
            continue;
        }
        let bytes = match BASE64.decode(&item.yjr_b64) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "[sidle] sync: bad sidecar payload for {}: {e}",
                    item.sdr_name
                );
                continue;
            }
        };
        let path = dir.join(&item.file_name);
        match std::fs::write(&path, &bytes) {
            Ok(()) => written += 1,
            Err(e) => eprintln!("[sidle] sync: write {} failed: {e}", path.display()),
        }
    }
    written
}

/// Read the `.yjr`/`.yjf` sidecars from every `*.sdr` that still has its book,
/// base64 each. Returns them plus the count of orphaned `.sdr` pruned.
/// with no matching `<stem>.kfx` is a copy the user deleted on the device.
/// Those are removed and not synced: only a live book's reading-state belongs
/// in the library. A live `.sdr` with neither sidecar (a pagination cache) is
/// kept but not pushed.
fn collect_sidecars(sidle_dir: &Path) -> Result<(Vec<SyncSdr>, usize)> {
    let mut sdrs = Vec::new();
    let mut pruned = 0usize;
    if let Ok(entries) = std::fs::read_dir(sidle_dir) {
        for entry in entries.flatten() {
            let sdr = entry.path();
            if sdr.extension().and_then(|e| e.to_str()) != Some("sdr") {
                continue;
            }
            // No live `.kfx` beside it → the user deleted this copy on the device.
            // Prune the orphaned sidecar and skip it; don't sync a dead copy.
            if !sdr.with_extension("kfx").exists() {
                if std::fs::remove_dir_all(&sdr).is_ok() {
                    pruned += 1;
                }
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
                yjr_b64: yjr.as_ref().map(|(_, b)| BASE64.encode(b)),
                yjf_b64: yjf.as_ref().map(|(_, b)| BASE64.encode(b)),
                yjr_name: yjr.map(|(n, _)| n),
                yjf_name: yjf.map(|(n, _)| n),
            });
        }
    }

    Ok((sdrs, pruned))
}

/// The first file in `sdr_dir` whose name ends with `suffix` (e.g. `.yjr`),
/// read into bytes — matching `find_sidecar`'s `ends_with` rule in sidle-core.
/// A sidecar's bytes *and* its filename. The name matters as much as the bytes:
fn read_sidecar(sdr_dir: &Path, suffix: &str) -> Result<Option<(String, Vec<u8>)>> {
    let Ok(entries) = std::fs::read_dir(sdr_dir) else {
        return Ok(None);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| n.ends_with(suffix))
            .map(str::to_string);
        if let Some(name) = name {
            let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            return Ok(Some((name, bytes)));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Notebook push — GET/POST /sync/notebooks
// ---------------------------------------------------------------------------

/// The push bundle: the standalone notebooks whose bytes the library doesn't
/// already hold. Mirrors `sidle-server`'s `NotebookSyncRequest`.
#[derive(Serialize)]
struct NotebookRequest {
    notebooks: Vec<SyncNotebook>,
}

#[derive(Serialize)]
struct SyncNotebook {
    uuid: String,
    nbk_b64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cover_b64: Option<String>,
    updated_at: String,
}

/// What the library stored, for the picker's toast.
#[derive(Debug, Default, Deserialize)]
pub struct NotebookReport {
    #[serde(default)]
    pub imported: usize,
    /// Notebooks the library refused to re-create because they were deleted in
    /// Sidle. Not an error and not shown in the toast, but logged — it is the
    /// answer to "why is my notebook not coming back?".
    #[serde(default)]
    pub suppressed: usize,
    #[serde(default)]
    pub failed: Vec<String>,
}

impl NotebookReport {
    /// A terse toast fragment, or `None` when nothing was stored — the normal
    /// case once a notebook has been backed up, and not worth a line.
    pub fn summary(&self) -> Option<String> {
        if self.imported == 0 && self.failed.is_empty() {
            return None;
        }
        let plural = if self.imported == 1 { "" } else { "s" };
        let mut s = format!("{} notebook{plural} backed up", self.imported);
        if !self.failed.is_empty() {
            s.push_str(&format!(", {} failed", self.failed.len()));
        }
        Some(s)
    }
}

/// Back the Scribe's standalone handwritten notebooks up to the library —
/// `GET /sync/notebooks` for what it already holds, then `POST` the rest.
pub fn push_notebooks(
    agent: &ureq::Agent,
    cfg: &ServerConfig,
    found: &[crate::handwriting::Standalone],
) -> Result<NotebookReport> {
    let url = format!("https://{}:{}/sync/notebooks", cfg.host, cfg.port);
    let have: NotebookManifest = fetch_manifest(agent, cfg, &url, found.is_empty());

    let todo: Vec<&crate::handwriting::Standalone> = found
        .iter()
        .filter(|n| have.notebooks.get(&n.nbk.id) != Some(&n.nbk.sha))
        .collect();

    let mut report = NotebookReport::default();
    for batch in batches(&todo, NOTEBOOK_BATCH_BYTES) {
        let notebooks: Vec<SyncNotebook> = batch
            .iter()
            .filter_map(|n| {
                // Re-read only what's going, and only a batch at a time: a
                // notebook runs to a couple of MB, this device has 512 MB shared
                // with the framework, and base64 inflates by a third.
                let bytes = match std::fs::read(&n.nbk.path) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!(
                            "[sidle/notebooks] read {} failed: {e}",
                            n.nbk.path.display()
                        );
                        return None;
                    }
                };
                Some(SyncNotebook {
                    uuid: n.nbk.id.clone(),
                    nbk_b64: BASE64.encode(bytes),
                    // Best-effort: no cover just means the viewer renders page 0.
                    cover_b64: n
                        .cover
                        .as_deref()
                        .and_then(|p| std::fs::read(p).ok())
                        .map(|b| BASE64.encode(b)),
                    updated_at: n.updated_at.clone(),
                })
            })
            .collect();
        if notebooks.is_empty() {
            continue;
        }
        let body = serde_json::to_vec(&NotebookRequest { notebooks })
            .context("serialize notebook request")?;
        let mut res = match agent
            .post(&url)
            .header("X-Sidle-Token", &cfg.token)
            .header("Content-Type", "application/json")
            .config()
            .timeout_global(Some(SYNC_TIMEOUT))
            .build()
            .send(&body)
        {
            Ok(res) => res,
            Err(ureq::Error::StatusCode(code)) if code == 401 || code == 403 => {
                return Err(SidleError::TokenMismatch);
            }
            Err(e) => return Err(anyhow!("POST {url}: {e}").into()),
        };
        let text =
            read_text(&mut res, JSON_MAX_BYTES).with_context(|| format!("read body of {url}"))?;
        let part: NotebookReport =
            serde_json::from_str(&text).with_context(|| format!("parse {url}"))?;
        report.imported += part.imported;
        report.suppressed += part.suppressed;
        report.failed.extend(part.failed);
    }
    Ok(report)
}

/// How many `nbk` bytes one `POST /sync/notebooks` may carry.
const NOTEBOOK_BATCH_BYTES: u64 = 12 * 1024 * 1024;

/// Split notebooks into groups whose `nbk` bytes sum to at most `budget`.
fn batches<'a>(
    items: &[&'a crate::handwriting::Standalone],
    budget: u64,
) -> Vec<Vec<&'a crate::handwriting::Standalone>> {
    let mut out: Vec<Vec<&crate::handwriting::Standalone>> = Vec::new();
    let mut cur: Vec<&crate::handwriting::Standalone> = Vec::new();
    let mut used = 0u64;
    for n in items {
        let size = std::fs::metadata(&n.nbk.path).map(|m| m.len()).unwrap_or(0);
        if !cur.is_empty() && used + size > budget {
            out.push(std::mem::take(&mut cur));
            used = 0;
        }
        cur.push(n);
        used += size;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
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

/// Push one decrypted book to sidle-server's `POST /sync/book`, which imports it
/// as the USB `/dedrm` pull does. Streamed from disk, never held in RAM.
pub fn push_book(agent: &ureq::Agent, cfg: &ServerConfig, path: &Path) -> Result<BookPush> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let url = format!("https://{}:{}/sync/book?ext={ext}", cfg.host, cfg.port);
    let mut res = match agent
        .post(&url)
        .header("X-Sidle-Token", &cfg.token)
        .header("Content-Type", "application/octet-stream")
        .send(file)
    {
        Ok(res) => res,
        Err(ureq::Error::StatusCode(code)) if code == 401 || code == 403 => {
            return Err(SidleError::TokenMismatch);
        }
        Err(e) => return Err(anyhow!("POST {url}: {e}").into()),
    };
    let body =
        read_text(&mut res, JSON_MAX_BYTES).with_context(|| format!("read body of {url}"))?;
    let reply: BookPushReply =
        serde_json::from_str(&body).with_context(|| format!("parse {url}"))?;
    Ok(match reply.outcome.as_str() {
        "duplicate" => BookPush::Duplicate,
        _ => BookPush::Imported,
    })
}

// ---------------------------------------------------------------------------
// Misc backup — GET /sync/misc (which folders), POST /sync/misc (the files)
// ---------------------------------------------------------------------------

/// One folder the library asked this Kindle to back up. Mirrors `sidle-core`'s
#[derive(Debug, Clone, Deserialize)]
pub struct Collection {
    pub id: String,
    pub label: String,
    /// Folders to scan, relative to `/mnt/us`. `"."` (or `""`) is the root.
    pub dirs: Vec<String>,
    /// Filenames to send, as [`glob_match`] patterns.
    pub include: Vec<String>,
    #[serde(default)]
    pub recursive: bool,
    /// Delete the sent files off this Kindle once the push landed.
    #[serde(default)]
    pub clear_device: bool,
    /// Filenames deleted off this Kindle after the push but never sent — the
    /// firmware's `wininfo_screenshot_*.txt` companions and the like.
    #[serde(default)]
    pub purge: Vec<String>,
}

impl Collection {
    fn includes(&self, name: &str) -> bool {
        !is_never_sent(name) && self.include.iter().any(|p| glob_match(p, name))
    }

    fn purges(&self, name: &str) -> bool {
        self.purge.iter().any(|p| glob_match(p, name))
    }
}

/// What the picker scans when the library can't be asked (see [`push_misc`]).
/// Deliberately the same two the library seeds itself with.
fn default_collections() -> Vec<Collection> {
    vec![
        Collection {
            id: "screenshots".into(),
            label: "Screenshots".into(),
            dirs: vec!["screenshots".into(), ".".into()],
            include: vec!["screenshot*".into()],
            recursive: false,
            clear_device: true,
            purge: vec!["wininfo_screenshot*".into()],
        },
        Collection {
            id: "logs".into(),
            label: "Logs".into(),
            dirs: vec!["logs".into()],
            include: vec!["*.log".into()],
            recursive: true,
            clear_device: false,
            purge: Vec::new(),
        },
    ]
}

/// The `GET /sync/misc` body: the library's collection list.
#[derive(Deserialize)]
struct CollectionsReply {
    collections: Vec<Collection>,
}

/// The push bundle: each file base64 in JSON, tagged with the collection it was
/// scanned for. Mirrors `sidle-server`'s `MiscSyncRequest`. `device_serial`
/// comes from `server.conf`.
#[derive(Serialize)]
struct MiscRequest {
    device_serial: String,
    files: Vec<MiscFile>,
}

#[derive(Serialize)]
struct MiscFile {
    collection: String,
    /// The file's path relative to its collection's folder — `2026/draft.md`
    /// for a recursive collection, a bare filename otherwise.
    path: String,
    data_b64: String,
}

/// The server's `MiscSyncResult`: files stored, per collection id.
#[derive(Deserialize)]
struct MiscReply {
    #[serde(default)]
    stored: std::collections::BTreeMap<String, usize>,
}

/// What the push backed up, labelled for the picker's toast.
#[derive(Debug, Default)]
pub struct MiscReport {
    /// `(label, count)` per collection that stored something, in config order.
    pub stored: Vec<(String, usize)>,
}

impl MiscReport {
    /// A terse toast fragment like `Screenshots 2, Logs 1`, or `None` when the
    /// push backed nothing up (so the caller omits it entirely).
    pub fn summary(&self) -> Option<String> {
        if self.stored.is_empty() {
            return None;
        }
        Some(
            self.stored
                .iter()
                .map(|(label, n)| format!("{label} {n}"))
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

/// Ask the desktop which folders it wants backed up.
fn fetch_collections(agent: &ureq::Agent, cfg: &ServerConfig) -> Vec<Collection> {
    let url = format!("https://{}:{}/sync/misc", cfg.host, cfg.port);
    let fetched = (|| -> Result<Vec<Collection>> {
        let mut res = match agent
            .get(&url)
            .header("X-Sidle-Token", &cfg.token)
            .config()
            .timeout_global(Some(SYNC_TIMEOUT))
            .build()
            .call()
        {
            Ok(res) => res,
            Err(ureq::Error::StatusCode(code)) if code == 401 || code == 403 => {
                return Err(SidleError::TokenMismatch);
            }
            Err(e) => return Err(anyhow!("GET {url}: {e}").into()),
        };
        let text =
            read_text(&mut res, JSON_MAX_BYTES).with_context(|| format!("read body of {url}"))?;
        let reply: CollectionsReply =
            serde_json::from_str(&text).with_context(|| format!("parse GET {url}"))?;
        Ok(reply.collections)
    })();
    match fetched {
        Ok(c) => c,
        Err(e) => {
            // eprintln lands in the picker log via sidle.sh's `2>>` redirect.
            eprintln!("[sidle/misc] collection list unavailable ({e}) — using defaults");
            default_collections()
        }
    }
}

/// How many file bytes one `POST /sync/misc` may carry.
const MISC_BATCH_BYTES: u64 = 8 * 1024 * 1024;

/// Back this Kindle's configured folders up to `POST /sync/misc` — the WiFi
/// backup the desktop's Files tab views. `us_root` is `/mnt/us`.
pub fn push_misc(agent: &ureq::Agent, cfg: &ServerConfig, us_root: &Path) -> Result<MiscReport> {
    if cfg.serial.is_empty() {
        return Err(anyhow!(
            "server.conf has no SERIAL= — re-run Update on Kindle in the desktop app \
             (backups are keyed per device)"
        )
        .into());
    }

    let collections = fetch_collections(agent, cfg);
    let scan = collect_misc_files(us_root, &collections);
    if scan.entries.is_empty() && scan.purge.is_empty() {
        // Nothing on the device to back up or tidy — skip the round-trip.
        return Ok(MiscReport::default());
    }

    let url = format!("https://{}:{}/sync/misc", cfg.host, cfg.port);
    let mut stored: std::collections::BTreeMap<String, usize> = Default::default();
    let mut cleared: Vec<&std::path::PathBuf> = Vec::new();
    for batch in misc_batches(&scan.entries, MISC_BATCH_BYTES) {
        let mut files = Vec::with_capacity(batch.len());
        let mut batch_cleared = Vec::new();
        for e in batch {
            // Read here, not during the scan: one batch in memory at a time. A
            let Some(bytes) = std::fs::read(&e.path).ok().filter(|b| !b.is_empty()) else {
                continue;
            };
            files.push(MiscFile {
                collection: e.collection.clone(),
                path: e.rel.clone(),
                data_b64: BASE64.encode(&bytes),
            });
            if e.clear {
                batch_cleared.push(&e.path);
            }
        }
        if files.is_empty() {
            continue;
        }
        let req = MiscRequest {
            device_serial: cfg.serial.clone(),
            files,
        };
        let body = serde_json::to_vec(&req).context("serialize misc request")?;

        let mut res = match agent
            .post(&url)
            .header("X-Sidle-Token", &cfg.token)
            .header("Content-Type", "application/json")
            .config()
            .timeout_global(Some(SYNC_TIMEOUT))
            .build()
            .send(&body)
        {
            Ok(res) => res,
            Err(ureq::Error::StatusCode(code)) if code == 401 || code == 403 => {
                return Err(SidleError::TokenMismatch);
            }
            Err(e) => return Err(anyhow!("POST {url}: {e}").into()),
        };
        let body =
            read_text(&mut res, JSON_MAX_BYTES).with_context(|| format!("read body of {url}"))?;
        let reply: MiscReply =
            serde_json::from_str(&body).with_context(|| format!("parse {url}"))?;
        for (id, n) in reply.stored {
            *stored.entry(id).or_default() += n;
        }
        cleared.extend(batch_cleared);
    }

    // Every batch landed (a non-2xx would have returned above) — clear what this
    for path in cleared.into_iter().chain(scan.purge.iter()) {
        if let Err(e) = std::fs::remove_file(path) {
            eprintln!(
                "[sidle/misc] delete {} after sync failed: {e}",
                path.display()
            );
        }
    }

    Ok(MiscReport {
        stored: collections
            .iter()
            .filter_map(|c| {
                stored
                    .get(&c.id)
                    .filter(|n| **n > 0)
                    .map(|n| (c.label.clone(), *n))
            })
            .collect(),
    })
}

/// Split scanned files into groups whose bytes sum to at most `budget`. A single
fn misc_batches(entries: &[MiscEntry], budget: u64) -> Vec<Vec<&MiscEntry>> {
    let mut out: Vec<Vec<&MiscEntry>> = Vec::new();
    let mut cur: Vec<&MiscEntry> = Vec::new();
    let mut used = 0u64;
    for e in entries {
        if !cur.is_empty() && used + e.size > budget {
            out.push(std::mem::take(&mut cur));
            used = 0;
        }
        cur.push(e);
        used += e.size;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------------------
// Reading log push — GET/POST /sync/reading-log
// ---------------------------------------------------------------------------

/// The push bundle: the reading-event lines this Kindle found past the desktop's
/// watermark. Mirrors `sidle-server`'s `ReadingLogRequest`.
#[derive(Serialize)]
struct ReadingLogRequest<'a> {
    device_serial: &'a str,
    lines: &'a [String],
    /// The snapshots those lines came from, so the desktop records them and this
    /// Kindle never opens them again.
    dumps: &'a [String],
}

/// The server's answer to "what do you already have from me?" — the snapshots it
/// has read, and how far into the live log it has got.
#[derive(Deserialize, Default)]
struct ReadingWatermark {
    #[serde(default)]
    watermark: String,
    #[serde(default)]
    seen: Vec<String>,
}

/// What the desktop stored, for the picker's toast.
#[derive(Debug, Default, Deserialize)]
pub struct ReadingLogReport {
    #[serde(default)]
    pub sessions: usize,
    #[serde(default)]
    pub added: usize,
    /// Sittings the library already held and these events carried further — what
    /// a Sync in the middle of a sitting produces, rather than a new session.
    #[serde(default)]
    pub extended: usize,
    #[serde(default)]
    pub attributed: usize,
    /// How far the library now holds this device's events, as `YYMMDD:HHMMSS`.
    /// What the local archive is pruned against.
    #[serde(default)]
    pub watermark: String,
    /// Dumps skipped on their filename alone — not from the server, filled in
    /// locally so the log can show that the watermark did its job.
    #[serde(skip)]
    pub skipped: usize,
    /// Which of this device's four log sources the lines came from.
    #[serde(skip)]
    pub from: crate::readinglog::Sources,
    /// Archive files deleted because the library confirmed it holds them.
    #[serde(skip)]
    pub purged: usize,
}

impl ReadingLogReport {
    /// A terse toast fragment, or `None` when nothing was read since the last
    /// Sync — which is the normal case and does not deserve a line.
    pub fn summary(&self) -> Option<String> {
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        match (self.added, self.extended) {
            (0, 0) => None,
            (0, n) => Some(format!("reading session{} extended", plural(n))),
            (n, 0) => Some(format!("{n} reading session{}", plural(n))),
            (n, m) => Some(format!("{n} reading session{}, {m} extended", plural(n))),
        }
    }
}

/// Push this Kindle's new reading events to the desktop.
pub fn push_reading_log(
    agent: &ureq::Agent,
    cfg: &ServerConfig,
    us_root: &Path,
) -> Result<ReadingLogReport> {
    if cfg.serial.is_empty() {
        return Err(anyhow!(
            "server.conf has no SERIAL= — re-run Update on Kindle in the desktop app \
             (reading sessions are keyed per device)"
        )
        .into());
    }

    let base = format!("https://{}:{}/sync/reading-log", cfg.host, cfg.port);
    let mark: ReadingWatermark = match agent
        .get(&base)
        .query("serial", &cfg.serial)
        .header("X-Sidle-Token", &cfg.token)
        .config()
        .timeout_global(Some(SYNC_TIMEOUT))
        .build()
        .call()
    {
        // Read + `serde_json`, not ureq's `into_json`: that needs the `json`
        // feature, which pulls dependencies this crate keeps out.
        Ok(mut res) => {
            let text = read_text(&mut res, JSON_MAX_BYTES)
                .with_context(|| format!("read body of GET {base}"))?;
            serde_json::from_str(&text).with_context(|| format!("parse GET {base}"))?
        }
        Err(ureq::Error::StatusCode(code)) if code == 401 || code == 403 => {
            return Err(SidleError::TokenMismatch);
        }
        Err(e) => return Err(anyhow!("GET {base}: {e}").into()),
    };

    let found = crate::readinglog::collect(us_root, &mark.watermark, &mark.seen);
    if !crate::readinglog::has_reading(&found.lines) && found.read.is_empty() {
        // The common case: nothing has been read since the last sync, so there
        // is nothing to send and no reason to make the request.
        return Ok(ReadingLogReport {
            skipped: found.skipped,
            from: found.from,
            ..Default::default()
        });
    }

    let req = ReadingLogRequest {
        device_serial: &cfg.serial,
        lines: &found.lines,
        dumps: &found.read,
    };
    let body = serde_json::to_vec(&req).context("serialize reading-log request")?;
    let mut res = match agent
        .post(&base)
        .header("X-Sidle-Token", &cfg.token)
        .header("Content-Type", "application/json")
        .config()
        .timeout_global(Some(SYNC_TIMEOUT))
        .build()
        .send(&body)
    {
        Ok(res) => res,
        Err(ureq::Error::StatusCode(code)) if code == 401 || code == 403 => {
            return Err(SidleError::TokenMismatch);
        }
        Err(e) => return Err(anyhow!("POST {base}: {e}").into()),
    };
    let text =
        read_text(&mut res, JSON_MAX_BYTES).with_context(|| format!("read body of {base}"))?;
    let mut report: ReadingLogReport =
        serde_json::from_str(&text).with_context(|| format!("parse {base}"))?;
    report.skipped = found.skipped;
    report.from = found.from;
    // Archive-then-purge, the same shape the misc sync uses: the local copy
    // existed only to survive a gap between syncs, and the gap just closed.
    report.purged = crate::readinglog::purge_archive(us_root, &report.watermark);
    Ok(report)
}

/// How deep a recursive collection descends. Mirrors core's `MAX_DEPTH`: a
/// device folder is someone else's to organize, and five levels is more nesting
/// than a notes or drafts folder ever has.
const MAX_DEPTH: usize = 5;

/// One file the scan found: everything the push needs except its bytes, which
/// are read a batch at a time.
struct MiscEntry {
    collection: String,
    /// Path relative to the collection's scanned folder — what it is stored
    /// under in the library.
    rel: String,
    path: std::path::PathBuf,
    size: u64,
    /// This file's collection clears the device. Only ever acted on for a file
    /// whose bytes actually went into a request that succeeded.
    clear: bool,
}

/// One pass over every collection's folders.
#[derive(Default)]
struct MiscScan {
    entries: Vec<MiscEntry>,
    /// On-device paths matched by a collection's `purge`: unlinked once the push
    purge: Vec<std::path::PathBuf>,
}

/// Find every file the `collections` ask for beneath `us_root`. Names and sizes
fn collect_misc_files(us_root: &Path, collections: &[Collection]) -> MiscScan {
    let mut scan = MiscScan::default();
    for collection in collections {
        let mut seen = std::collections::HashSet::new();
        for dir in &collection.dirs {
            let base = if dir == "." || dir.is_empty() {
                us_root.to_path_buf()
            } else {
                us_root.join(dir)
            };
            gather_misc(&base, "", collection, &mut scan, &mut seen, 0);
        }
    }
    scan
}

/// Append one directory's matching files to `scan`, recursing when the collection
/// asks. `rel` is the path the file is stored under, which `seen` keys on.
fn gather_misc(
    dir: &Path,
    rel: &str,
    collection: &Collection,
    scan: &mut MiscScan,
    seen: &mut std::collections::HashSet<String>,
    depth: usize,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if collection.recursive && depth + 1 < MAX_DEPTH && !name.starts_with('.') {
                gather_misc(&entry.path(), &child_rel, collection, scan, seen, depth + 1);
            }
            continue;
        }
        // Purge matches are cleared without ever being read — the point of them
        // is that they're worth removing and worth nothing in the library.
        if collection.purges(&name) {
            scan.purge.push(entry.path());
            continue;
        }
        if !collection.includes(&name) || meta.len() == 0 || seen.contains(&child_rel) {
            continue;
        }
        seen.insert(child_rel.clone());
        scan.entries.push(MiscEntry {
            collection: collection.id.clone(),
            rel: child_rel,
            path: entry.path(),
            size: meta.len(),
            clear: collection.clear_device,
        });
    }
}

/// Names no collection ever sends, whatever its patterns say: an in-flight
/// write, and the dotfiles a desktop OS leaves behind after someone opens the
/// Kindle in a file browser. Mirrors core's `is_never_backed_up`.
fn is_never_sent(name: &str) -> bool {
    name.starts_with('.') || name.to_ascii_lowercase().ends_with(".partial")
}

/// Case-insensitive glob over a bare filename, `*` the only metacharacter.
/// Mirrors `sidle_core::library::device_backup::glob_match`; the two must agree.
fn glob_match(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.to_lowercase().chars().collect();
    let text: Vec<char> = name.to_lowercase().chars().collect();

    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut retry) = (None, 0usize);
    while t < text.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            retry = t;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            retry += 1;
            t = retry;
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&c| c == '*')
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
            kind: None,
            asin: None,
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
        // A leading BOM (U+FEFF) code-point-sorts a title to the end.
        // Stripped → the digit leads → correct order.
        assert_eq!(clean("\u{FEFF}01 〝文学少女〟"), "01 〝文学少女〟");
        // A BOM buried mid-title is removed too.
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

    // Batching keeps a first sync from a well-used Scribe inside the device's RAM: a
    // run splits at the budget, and one notebook bigger than the budget still goes.
    #[test]
    fn notebooks_batch_by_bytes_and_never_drop_an_oversized_one() {
        let base = scratch("batches");
        std::fs::create_dir_all(&base).unwrap();
        let make = |name: &str, size: usize| {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("nbk");
            std::fs::write(&path, vec![0u8; size]).unwrap();
            crate::handwriting::Standalone {
                nbk: crate::handwriting::Nbk {
                    id: name.to_string(),
                    path,
                    sha: String::new(),
                },
                cover: None,
                updated_at: String::new(),
            }
        };
        let items = [make("a", 400), make("b", 400), make("c", 400)];
        let refs: Vec<&crate::handwriting::Standalone> = items.iter().collect();

        // 1000-byte budget: a+b fill it, c starts the next batch.
        let split = batches(&refs, 1000);
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].len(), 2);
        assert_eq!(split[1].len(), 1);

        // Everything fits → one request.
        assert_eq!(batches(&refs, 10_000).len(), 1);

        // A notebook larger than the whole budget is sent alone, not dropped:
        // going alone is the only way it is ever backed up.
        let huge = [make("huge", 4000)];
        let huge_refs: Vec<&crate::handwriting::Standalone> = huge.iter().collect();
        let split = batches(&huge_refs, 1000);
        assert_eq!(split.len(), 1);
        assert_eq!(split[0].len(), 1);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn collect_sidecars_reads_sidecars_and_skips_pagination_cache() {
        let base = scratch("collect");
        let docs = base.join("documents");
        let sidle = docs.join("Sidle");

        // A live book: .sdr with both sidecars AND its .kfx present → synced.
        let sdr = sidle.join("book.deadbeef.sdr");
        std::fs::create_dir_all(&sdr).unwrap();
        std::fs::write(sdr.join("book.deadbeef0000.yjr"), b"yjr-bytes").unwrap();
        std::fs::write(sdr.join("book.deadbeef0000.yjf"), b"yjf-bytes").unwrap();
        std::fs::write(sidle.join("book.deadbeef.kfx"), b"kfx").unwrap();
        // A pagination-cache .sdr (neither sidecar) whose .kfx lives → kept, not
        // synced, not pruned.
        let cache = sidle.join("other.cafe0000.sdr");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("page.cache"), b"x").unwrap();
        std::fs::write(sidle.join("other.cafe0000.kfx"), b"kfx").unwrap();
        // An orphaned .sdr with annotations but NO .kfx (the user deleted the
        // book on the device) → pruned, not synced.
        let orphan = sidle.join("gone.beefcafe.sdr");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("gone.beefcafe0.yjr"), b"stale").unwrap();

        let (sdrs, pruned) = collect_sidecars(&sidle).unwrap();
        assert_eq!(sdrs.len(), 1, "only the live book's .sdr is synced");
        assert_eq!(sdrs[0].sdr_name, "book.deadbeef.sdr");
        let yjr_expected = BASE64.encode(b"yjr-bytes");
        let yjf_expected = BASE64.encode(b"yjf-bytes");
        assert_eq!(sdrs[0].yjr_b64.as_deref(), Some(yjr_expected.as_str()));
        assert_eq!(sdrs[0].yjf_b64.as_deref(), Some(yjf_expected.as_str()));
        assert_eq!(pruned, 1, "the orphaned .sdr (no .kfx) was pruned");
        assert!(
            !orphan.exists(),
            "orphaned .sdr dir removed from the device"
        );
        assert!(
            cache.exists(),
            "pagination-cache .sdr kept — its .kfx is live"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn collect_sidecars_empty_when_no_tree() {
        let base = scratch("empty");
        // documents/Sidle doesn't exist → empty bundle, no error.
        let (sdrs, pruned) = collect_sidecars(&base.join("documents/Sidle")).unwrap();
        assert!(sdrs.is_empty());
        assert_eq!(pruned, 0);
    }

    #[test]
    fn collect_misc_files_scans_both_locations_and_dedups() {
        let us = scratch("misc");
        std::fs::create_dir_all(us.join("screenshots")).unwrap();
        std::fs::create_dir_all(us.join("logs")).unwrap();
        // Newer-style screenshots under screenshots/, with the firmware's
        // companion file beside one of them.
        std::fs::write(us.join("screenshots/screenshot_100.png"), b"A").unwrap();
        std::fs::write(us.join("screenshots/screenshot_200.png"), b"B").unwrap();
        std::fs::write(
            us.join("screenshots/wininfo_screenshot_2026_08_15T01_47_50+0200.txt"),
            b"win",
        )
        .unwrap();
        // KOA2 stock capture loose in the root, and one name that appears in
        // BOTH screenshots/ and the root (must be sent only once).
        std::fs::write(us.join("Screenshot_root.png"), b"C").unwrap();
        std::fs::write(us.join("screenshot_100.png"), b"DUP").unwrap();
        // Logs live in logs/ now — a stray root log is not scanned.
        std::fs::write(us.join("logs/sidle-native.log"), b"log\n").unwrap();
        std::fs::write(us.join("stray.log"), b"not scanned\n").unwrap();
        // Unrelated root files that must be ignored.
        std::fs::write(us.join("version.txt"), b"5.16").unwrap();

        let scan = collect_misc_files(&us, &default_collections());
        let mut sent: Vec<_> = scan
            .entries
            .iter()
            .map(|e| format!("{}:{}", e.collection, e.rel))
            .collect();
        sent.sort();
        assert_eq!(
            sent,
            vec![
                "logs:sidle-native.log",
                "screenshots:Screenshot_root.png",
                "screenshots:screenshot_100.png",
                "screenshots:screenshot_200.png",
            ],
            "both screenshot dirs scanned, dup collapsed, only logs/ for logs"
        );
        // The screenshots/ copy wins the dedup — the first dir listed wins.
        let hundred = scan
            .entries
            .iter()
            .find(|e| e.rel == "screenshot_100.png")
            .unwrap();
        assert_eq!(hundred.path, us.join("screenshots/screenshot_100.png"));

        // The wininfo companion is cleared without ever being sent.
        let purge: Vec<_> = scan
            .purge
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            purge,
            vec!["wininfo_screenshot_2026_08_15T01_47_50+0200.txt"]
        );

        // Screenshots are the collection that clears; the log is not — and a
        // screenshot is only ever cleared after its bytes go out (see push_misc).
        let mut clears: Vec<_> = scan
            .entries
            .iter()
            .filter(|e| e.clear)
            .map(|e| e.rel.clone())
            .collect();
        clears.sort();
        assert_eq!(
            clears,
            vec![
                "Screenshot_root.png",
                "screenshot_100.png",
                "screenshot_200.png"
            ],
            "logs must not be scheduled for deletion"
        );

        let _ = std::fs::remove_dir_all(&us);
    }

    /// A collection the library added: its own folder, its own pattern, its own
    /// subfolders — and nothing of it deleted from the device.
    #[test]
    fn collect_misc_files_walks_a_recursive_collection() {
        let us = scratch("misc-recursive");
        std::fs::create_dir_all(us.join("writing/2026")).unwrap();
        std::fs::write(us.join("writing/index.md"), b"root").unwrap();
        std::fs::write(us.join("writing/2026/draft.md"), b"nested").unwrap();
        std::fs::write(us.join("writing/2026/notes.txt"), b"other").unwrap();

        // The id is the library's storage key, not the folder's name: the two
        // are free to differ, and the folder is the one that gets renamed.
        let collections = vec![Collection {
            id: "drafts".into(),
            label: "Drafts".into(),
            dirs: vec!["writing".into()],
            include: vec!["*.md".into()],
            recursive: true,
            clear_device: false,
            purge: Vec::new(),
        }];
        let scan = collect_misc_files(&us, &collections);
        let mut sent: Vec<_> = scan.entries.iter().map(|e| e.rel.clone()).collect();
        sent.sort();
        assert_eq!(sent, vec!["2026/draft.md", "index.md"]);
        assert!(scan.purge.is_empty(), "nothing purged off the device");
        assert!(
            scan.entries.iter().all(|e| !e.clear),
            "nothing cleared off the device"
        );

        // A folder that isn't on this Kindle is simply nothing to send.
        let missing = vec![Collection {
            id: "nowhere".into(),
            label: "Nowhere".into(),
            dirs: vec!["nowhere".into()],
            include: vec!["*".into()],
            recursive: true,
            clear_device: false,
            purge: Vec::new(),
        }];
        assert!(collect_misc_files(&us, &missing).entries.is_empty());

        let _ = std::fs::remove_dir_all(&us);
    }

    /// A haul bigger than one request is split, and a single file over the
    /// budget still goes rather than being dropped — the Kindle's memory, not
    /// the folder's size, is what bounds a push.
    #[test]
    fn misc_batches_bound_one_request() {
        let entry = |rel: &str, size: u64| MiscEntry {
            collection: "logs".into(),
            rel: rel.into(),
            path: std::path::PathBuf::from(rel),
            size,
            clear: false,
        };
        let items = [entry("a", 400), entry("b", 400), entry("c", 400)];
        let split = misc_batches(&items, 1000);
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].len(), 2);
        assert_eq!(split[1].len(), 1);

        assert_eq!(misc_batches(&items, 10_000).len(), 1);

        let huge = [entry("huge", 4000)];
        let split = misc_batches(&huge, 1000);
        assert_eq!(split.len(), 1);
        assert_eq!(split[0].len(), 1);
    }

    #[test]
    fn glob_matches_what_the_library_asks_for() {
        assert!(glob_match("screenshot*", "Screenshot_ROOT.PNG"));
        assert!(!glob_match("screenshot*", "wininfo_screenshot_1.txt"));
        assert!(glob_match(
            "wininfo_screenshot*",
            "wininfo_screenshot_1.txt"
        ));
        assert!(glob_match("*.log", "sidle-native.log"));
        assert!(!glob_match("*.log", "book.kfx"));
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        // Never sent, whatever the pattern says.
        let all = Collection {
            id: "x".into(),
            label: "X".into(),
            dirs: vec![".".into()],
            include: vec!["*".into()],
            recursive: false,
            clear_device: false,
            purge: Vec::new(),
        };
        assert!(all.includes("draft.md"));
        assert!(!all.includes(".DS_Store"));
        assert!(!all.includes("screenshot_1.png.partial"));
    }

    #[test]
    fn misc_report_summary() {
        assert_eq!(MiscReport::default().summary(), None);
        let r = MiscReport {
            stored: vec![("Screenshots".into(), 2), ("Logs".into(), 1)],
        };
        assert_eq!(r.summary().as_deref(), Some("Screenshots 2, Logs 1"));
        let r = MiscReport {
            stored: vec![("Screenshots".into(), 1)],
        };
        assert_eq!(r.summary().as_deref(), Some("Screenshots 1"));
    }

    /// Syncing in the middle of a sitting stores real reading and must say so.
    #[test]
    fn reading_log_summary_speaks_for_a_sitting_carried_further() {
        assert_eq!(ReadingLogReport::default().summary(), None);
        let extended = ReadingLogReport {
            sessions: 1,
            extended: 1,
            ..Default::default()
        };
        assert_eq!(
            extended.summary().as_deref(),
            Some("reading session extended")
        );
        let both = ReadingLogReport {
            added: 2,
            extended: 1,
            ..Default::default()
        };
        assert_eq!(
            both.summary().as_deref(),
            Some("2 reading sessions, 1 extended")
        );
        let fresh = ReadingLogReport {
            added: 1,
            ..Default::default()
        };
        assert_eq!(fresh.summary().as_deref(), Some("1 reading session"));
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
