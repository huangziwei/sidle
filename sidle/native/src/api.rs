//! Sidle-server HTTPS client.
//!
//! Three endpoints, all token-gated, all sync via `ureq`:
//! - `GET /list.json`  → library as JSON (incl. the on-device save name)
//! - `GET /cover/{id}` → cover image bytes (M6)
//! - `GET /get/{id}`   → KFX bytes (M7)
//!
//! Token is sent as `X-Sidle-Token` header. The server also accepts
//! `?token=` query but the header is cleaner for programmatic clients.
//!
//! # Trust
//!
//! Every request is TLS, including on the home LAN, and the only certificate
//! this binary will accept is one issued by the CA at [`CA_PATH`] — see
//! [`build_agent`]. There is no plaintext fall-back and no scheme setting: a
//! server that cannot present our leaf is a server we do not talk to. That is
//! the whole point, since the token grants the entire library plus the write
//! surface and never rotates, so one captured request would be a permanent
//! compromise.
//!
//! Book shape mirrors `sidle_core::library::db::BookRow`. The core display +
//! download fields are `id`/`title` (display), `kfx_sha256` (on-device dedupe),
//! and `device_filename` (the save name — `/get/{id}`'s Content-Disposition is
//! unusable here, see the `device_filename` field). The remaining metadata fields
//! (`author`, `language`, `publisher`, …) feed the picker's filter + sort
//! (`ui::sort`); the server already flattens them into `/list.json`, so reading
//! them is a client-only change. serde silently drops unknown JSON fields, so
//! this stays compatible if the server adds columns. The full shape lives at
//! sidle/core/src/library/db.rs.

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

/// Errors from talking to sidle-server. `TokenMismatch` is broken out so
/// the toast layer in `main.rs` can show a "plug Kindle into sidle"
/// breadcrumb instead of the opaque "Failed: GET ... status code 403"
/// that users have to grep the log for.
#[derive(Debug)]
pub enum SidleError {
    /// Server returned 401 or 403 — the bearer token in our
    /// `etc/server.conf` no longer matches the one sidle-server is
    /// validating against (rotated `.server-token`, fresh install).
    /// User action is to re-deploy via the desktop app's Install on Kindle button.
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
/// `server.conf`. Sibling of [`crate::CONFIG_PATH`] on purpose: the two are
/// written by the same deploy and are useless apart — an address without a
/// trust root cannot be dialled, and a root without an address has nothing to
/// verify.
pub const CA_PATH: &str = "/mnt/us/extensions/sidle/etc/ca.pem";

/// Build the one shared agent: TLS, with our CA as the **sole** trust root.
///
/// Sole rather than additional. ureq is compiled without `rustls-webpki-roots`,
/// so the Mozilla root set is not in this binary at all — which makes the pin
/// structural. No public CA can mint a certificate this picker will accept,
/// which is a stronger position than trusting the public set and then hoping.
///
/// The provider is RustCrypto's rather than ring's, and that is a build
/// decision before it is a crypto one: ring carries a C build script, cargo
/// unifies features so it would be compiled whether or not it were ever called,
/// and that alone breaks the pure-Rust `rust-lld` cross path this single static
/// armv7 binary depends on. It is passed explicitly because there is no default
/// to fall back on under `rustls-no-provider` — a missing provider should be a
/// visible wiring decision, not a runtime surprise.
///
/// `configure` receives the builder so callers can set their own timeouts; the
/// gallery and the `--update` path want different ones.
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
///
/// ureq 3 requires an explicit limit to read a body to a `String`, which is a
/// better default than ureq 2's unbounded `into_string()`: every one of these
/// responses is a small JSON document, and an unbounded read of a wedged or
/// hostile server is exactly the thing a 512 MB device cannot absorb.
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
    /// download as. `#[serde(default)]` so an older
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
    /// Conversion direction, `"<source>_to_<target>"` (`"pdf_to_kfx"`,
    /// `"epub_to_kfx"`, `"kfx_to_epub"`) — the only record of which format a book
    /// was imported *from*, which is what the Format facet groups by.
    ///
    /// Already on the wire: `/list.json` flattens the whole library row, so this
    /// arrived before the picker had a use for it and reading it needs no
    /// protocol change. `#[serde(default)]` → `None` against an older server,
    /// which [`Book::source_format`] reads as EPUB, matching the desktop.
    #[serde(default)]
    pub kind: Option<String>,
    /// The content_id baked into the KFX Sidle pushed. The device names this
    /// book's ink notebook with it (`.notebooks/<asin>!!PDOC!!notebook`), so
    /// this is what tells the handwriting sync which of those directories hold
    /// ink of ours and which are Amazon's own cloud documents.
    ///
    /// Already on the wire for the same reason as [`Self::kind`]. `None` for a
    /// book that has no KFX yet — such a book can't have been pushed, so it
    /// can't have ink either.
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
    /// → `""` against an older server that doesn't ship it; the `search` module then
    /// falls back to canon'ing the raw title/author on-device (Latin-only match).
    #[serde(default)]
    pub search_key: String,
}

impl Book {
    /// The format this book was imported *from* — `"PDF"`, `"EPUB"` or `"KFX"`.
    ///
    /// Read off the conversion `kind` (`"<source>_to_<target>"`), the same
    /// derivation the desktop's `source_format` uses, so both surfaces group a
    /// book the same way. Upper-cased because these are format names and the
    /// facet menu shows them verbatim.
    ///
    /// Why it earns a facet: format decides whether a book is *readable on this
    /// device*. A PDF is fixed-layout and cannot reflow, so it is unusable on a
    /// 7" panel and good on a 10.2" one — which makes this the one facet whose
    /// useful setting differs per Kindle, in both directions.
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

/// Drop [`crate::font::is_invisible`] characters anywhere in `s`, then trim
/// surrounding whitespace. The set is the renderer's, because the two
/// questions have the same answer: a code point that carries no glyph is the
/// one collation also ignores.
fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !crate::font::is_invisible(*c))
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn fetch_cover(agent: &ureq::Agent, cfg: &ServerConfig, id: i64) -> Result<Vec<u8>> {
    // `?thumb=1` → the server returns the small color thumbnail produced at
    // import (see sidle_core::library::thumbnail) — ~30–50KB instead of the
    // full-res cover. The server falls back to full-res if the thumbnail isn't
    // on disk yet, so this is always safe to request.
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
    let url = format!("https://{}:{}/get/{}", cfg.host, cfg.port, book.id);
    // No overall request timeout: a big book over a sleepy radio can take
    // minutes, and an overall deadline would kill a transfer that's making
    // steady progress (the 256 MB-truncation bug's slow-path twin). The
    // session agent's per-read timeout (`timeout_read`, set in `main`) bounds a
    // genuinely stalled socket instead. The body is returned unread so the
    // caller can stream it to disk with live progress + cancel.
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
///
/// A book that has never been opened has no `.sdr`, so it appears in no sync
/// bundle and the ordinary annotation sync — which answers what this device
/// sent — never reaches it. Its highlights would wait for the reader to open
/// the book and sync again. Asking at download time closes that: the book
/// arrives already carrying what the library knows about it.
///
/// The `.sdr` is created here rather than waited for. Writing it now is also
/// the one moment the flush race cannot be lost — the reader cannot have this
/// book loaded when it did not exist a moment ago.
///
/// Best-effort by construction: `Ok(false)` when the library has nothing to
/// write, and every failure is the caller's to log, never to fail a download
/// that already succeeded.
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
/// USB-side delete recognize a picker-downloaded file instead of treating it
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
/// `sidle-server`'s `SyncRequest` DTO. `device_serial` comes from `server.conf`
/// (`ServerConfig::serial`), written by the desktop app at install — so the
/// picker needs no on-device serial lookup.
#[derive(Serialize)]
struct SyncRequest {
    device_serial: String,
    sdrs: Vec<SyncSdr>,
    /// Handwritten ink drawn on sideloaded books, one entry per host book.
    ///
    /// It travels with the sidecars rather than on a route of its own because
    /// the desktop anchors each ink page to its host page using the
    /// `handwritten_note` records inside those very `.yjr`s. Sent apart, every
    /// page would import unanchored.
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
    /// Orphaned `.sdr` dirs pruned off the device this sync (a `.sdr` with no
    /// matching `.kfx` — a copy the user deleted; the Kindle leaves the sidecar
    /// behind). Set locally by [`push_annotations`], not from the server (hence
    /// `#[serde(default)]`), so it survives even the "nothing to sync" early exit.
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
    /// per-book count rides in the same response and the desktop shows it; the
    /// picker's toast has room for one number, and pages are the one that says
    /// how much handwriting actually landed.
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
///
/// `skip` short-circuits the request when the device has nothing of that kind —
/// the usual case on a Kindle without a pen, where a round trip would only
/// confirm there is nothing to compare.
///
/// Any failure yields an empty manifest, which reads as "the library has
/// nothing" and sends everything. That is the safe direction: the import is
/// idempotent on content sha, so a redundant upload costs only bandwidth, while
/// a wrongly-skipped one loses a page. A rejected token isn't swallowed either —
/// the POST that follows hits the same wall and reports it properly.
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
/// `POST /sync/annotations` — the LAN twin of a USB sync. `sidle_dir` is
/// `/mnt/us/documents/Sidle` (the download dir). `ink` is the handwriting found
/// on this device's books ([`crate::handwriting::scan`]); pass an empty slice on
/// a Kindle with no pen. Returns the server's [`SyncReport`].
///
/// Errors with a re-install breadcrumb if `server.conf` carries no `SERIAL=`
/// (a pre-sync install): annotations are keyed per device, so the serial is
/// mandatory for the push (but not for boot/list/download, hence it's optional
/// in [`ServerConfig`]).
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

/// Read the `.yjr`/`.yjf` sidecars from every `*.sdr` under `sidle_dir` that
/// still has its book, base64 each. Returns the sidecars to push plus the count
/// of orphaned `.sdr` **pruned**. Mirrors the device-side scan in
/// `sidle_core::library::ingest::import_from_device` (which sidle-native can't
/// call — no sidle-core dep across the cross-compile boundary).
///
/// The Kindle keeps a book's `.sdr` when you delete its `.kfx`, so an `.sdr`
/// with no matching `<stem>.kfx` is a copy the user deleted on the device (the
/// reason stale reconvert copies pile up — 裸命 had six). Those are removed and
/// not synced: only a live book's reading-state belongs in the library. A live
/// `.sdr` with neither sidecar (a pagination cache) is kept but not pushed.
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
/// writing one back has to reuse it exactly, since it carries a device-specific
/// infix no host can derive.
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
///
/// `found` is [`crate::handwriting::scan`]'s notebook list. Nothing to send is
/// the steady state and costs one small GET; a device with no pen costs nothing
/// at all.
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
///
/// Sized for the device, not the server: base64 inflates by a third and the
/// serialized body is a second copy, so a batch costs roughly 2.7× this in peak
/// RAM — and this Kindle has 512 MB shared with the reader framework. Each batch
/// is committed on its own, so a library of any size syncs in bounded memory.
const NOTEBOOK_BATCH_BYTES: u64 = 12 * 1024 * 1024;

/// Split notebooks into groups whose `nbk` bytes sum to at most `budget`.
///
/// A single notebook larger than the budget still goes alone rather than being
/// dropped — sending it is the only way it ever gets backed up, and the server's
/// cap is the real ceiling.
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

/// Push one decrypted `.kfx-zip` to sidle-server's `POST /sync/book`, which
/// imports it exactly as the desktop's USB `/dedrm` auto-pull would (hash-
/// deduped, so USB and WiFi coexist). Streams the file straight from disk rather
/// than buffering the whole book in the Kindle's RAM.
pub fn push_book(agent: &ureq::Agent, cfg: &ServerConfig, path: &Path) -> Result<BookPush> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let url = format!("https://{}:{}/sync/book", cfg.host, cfg.port);
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
// Misc backup push — POST /sync/misc (screenshots + picker logs, the WiFi backup)
// ---------------------------------------------------------------------------

/// The push bundle: each screenshot / picker log, base64 in JSON. Mirrors
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

/// Push the Kindle's screenshots + picker logs to `POST /sync/misc` — the WiFi
/// backup the desktop "Misc." tab views. `us_root` is `/mnt/us`. On a successful
/// push the screenshots are **deleted from the device** (they're safely backed
/// up now, and a Kindle's screenshot folder is scratch space) — logs stay, since
/// they're the live append-only diagnostic trail. That clear-on-sync also means
/// each Sync only re-uploads screenshots taken since the last one. Returns the
/// server's [`MiscReport`].
pub fn push_misc(agent: &ureq::Agent, cfg: &ServerConfig, us_root: &Path) -> Result<MiscReport> {
    if cfg.serial.is_empty() {
        return Err(anyhow!(
            "server.conf has no SERIAL= — re-run Update on Kindle in the desktop app \
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

    let url = format!("https://{}:{}/sync/misc", cfg.host, cfg.port);
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
    /// Which of this device's four log sources the lines came from. Local, and
    /// the one thing that separates "nothing was read" from "the minutes since
    /// the last rotation were never reached" — the two look identical in a
    /// report that only counts sessions.
    #[serde(skip)]
    pub from: crate::readinglog::Sources,
    /// Archive files deleted because the library confirmed it holds them.
    #[serde(skip)]
    pub purged: usize,
}

impl ReadingLogReport {
    /// A terse toast fragment, or `None` when nothing was read since the last
    /// Sync — which is the normal case and does not deserve a line.
    ///
    /// A sitting carried further says so in its own words. Tapping Sync in the
    /// middle of one is the common case and it stores real reading; reporting it
    /// as nothing at all is what makes a reader think the sitting was lost.
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
///
/// Two round trips, and the first one is what makes this cheap: the desktop says
/// how far it has already read, and everything at or before that is skipped —
/// whole gzipped dumps, unopened, on their filename alone. With nothing new
/// since the last Sync this reads one directory, scans the live log, and returns
/// without a second request.
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
    if found.lines.is_empty() && found.read.is_empty() {
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
    // Against the library's own watermark, never against what was sent — the
    // library is the one that knows what it managed to store.
    report.purged = crate::readinglog::purge_archive(us_root, &report.watermark);
    Ok(report)
}

/// Read every screenshot (`screenshot*` under `screenshots/` and the USB root)
/// and picker log (`*.log` at the root) beneath `us_root`, base64 each. Dedups by
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
    // USB root — KOA2's stock screenshots live loose here; so do our picker logs.
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

/// A picker log filename (`*.log`), case-insensitive. Mirrors core's `classify_misc`.
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

    /// Batching is what keeps a first sync from a well-used Scribe inside the
    /// device's RAM, so it has to hold on the two cases that matter: a run of
    /// notebooks splits at the budget, and one bigger than the whole budget
    /// still goes rather than being silently skipped.
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
        // Newer-style screenshots under screenshots/.
        std::fs::write(us.join("screenshots/screenshot_100.png"), b"A").unwrap();
        std::fs::write(us.join("screenshots/screenshot_200.png"), b"B").unwrap();
        // KOA2 stock capture loose in the root + a picker log; and one name that
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

    /// Syncing in the middle of a sitting stores real reading and must say so.
    ///
    /// It is the ordinary case — a reader puts the book down, taps Sync, picks
    /// it up again — and a toast that mentions only new sessions leaves it
    /// looking exactly like a Sync that found nothing at all.
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
