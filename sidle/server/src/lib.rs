//! sidle LAN HTTP server.
//!
//! Read-only over the existing on-disk library: serves `/list.json`,
//! `/get/{id}`, and `/cover/{id}` from the same `library.db` +
//! `books/<sha>/` layout the Tauri desktop app writes. Token-gated so a
//! casual LAN scan can't browse the shelf.
//!
//! Two launch modes use the same `serve(config)`:
//! - Embedded — Tauri spawns it as a tokio task, sharing the runtime.
//! - Standalone — `sidle-server` CLI binary parses args, calls `serve()`.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::Connection;
use tokio::net::TcpListener;

use sidle_core::library::{
    LibraryPaths, db,
    ingest::{self, CollectedYjr, DeviceImportReport},
    paths::kfx_device_filename,
};

// We call `db::open` per request (rather than holding a long-lived `Arc<
// Mutex<Connection>>` like the Tauri side) because the server's workload is
// stateless reads. Cost: re-runs the idempotent migrations on every hit,
// which is a handful of PRAGMA / `has_column` queries — negligible at
// single-user LAN scale.

/// Runtime configuration assembled by either the CLI or the embedded
/// caller. The token is loaded/generated outside and passed in so the same
/// secret can also be written into the KUAL bundle's `etc/server.conf` at
/// install time (Phase 6).
pub struct Config {
    pub paths: LibraryPaths,
    /// `host:port` for the listener. Defaults to `0.0.0.0:8731` from the
    /// CLI.
    pub bind: String,
    pub token: String,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) paths: LibraryPaths,
    pub(crate) token: Arc<str>,
}

/// Serve until the task is dropped/aborted (embedded mode) — no signal
/// handling. The Tauri app calls this and stops the server by aborting the
/// tokio task; installing a process-wide signal handler here would step on the
/// app's own. The standalone binary uses [`serve_with_shutdown`] instead.
pub async fn serve(config: Config) -> Result<()> {
    serve_with_shutdown(config, std::future::pending::<()>()).await
}

/// [`serve`] with a shutdown trigger threaded in, so axum drains in-flight
/// requests when the future resolves instead of dropping the listener abruptly.
/// The standalone `sidle-server` passes a SIGTERM/SIGINT future (so a graceful
/// `kill`, sakabar's port-kill, an app-initiated stop, and Ctrl-C all drain
/// cleanly); a test can pass a oneshot to drive shutdown deterministically.
pub async fn serve_with_shutdown(
    config: Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    config.paths.ensure().context("ensure library paths")?;

    // Backfill cover thumbnails for books imported before thumbnails existed
    // (and self-heal any that went missing). Spawned in the background so we
    // start listening immediately — a book whose thumbnail isn't ready yet just
    // falls back to its full-res cover in `get_cover` until it lands.
    // Idempotent + mtime-gated, so this is a near-instant no-op once warm.
    let thumb_paths = config.paths.clone();
    tokio::spawn(async move {
        match tokio::task::spawn_blocking(move || {
            sidle_core::library::thumbnail::backfill_thumbnails(&thumb_paths)
        })
        .await
        {
            Ok(Ok(n)) if n > 0 => tracing::info!("cover thumbnails: backfilled {n}"),
            Ok(Ok(_)) => {}
            Ok(Err(err)) => tracing::warn!(?err, "cover thumbnail backfill failed"),
            Err(err) => tracing::warn!(?err, "cover thumbnail backfill task panicked"),
        }
    });

    let state = AppState {
        paths: config.paths,
        token: Arc::from(config.token),
    };

    let app = build_router(state);

    let listener = TcpListener::bind(&config.bind)
        .await
        .with_context(|| format!("bind {}", config.bind))?;
    let local = listener.local_addr()?;
    tracing::info!("sidle-server listening on http://{local}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .context("axum::serve")?;
    Ok(())
}

/// 32 MB body cap on `POST /sync/annotations`. A whole library's `.yjr`/`.yjf`
/// sidecars (KB each) fit with generous headroom; bounds the JSON buffer the body
/// extractor builds so a stray/oversized POST can't exhaust memory.
const SYNC_BODY_LIMIT: usize = 32 * 1024 * 1024;

/// The axum app: routes + per-route layers. Factored out of
/// [`serve_with_shutdown`] so tests can drive it via `Router::oneshot` without
/// binding a socket — exercising the real route table and the body-limit layer.
pub(crate) fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(health))
        .route("/list.json", get(list_json))
        .route("/get/{id}", get(get_book))
        .route("/cover/{id}", get(get_cover))
        // P3 write surface: the Kindle pushes its `.yjr`/`.yjf` here for ingest —
        // the LAN twin of the USB import. Token-gated like the reads; body-limited
        // so an oversized POST can't exhaust memory.
        .route(
            "/sync/annotations",
            post(sync_annotations).layer(DefaultBodyLimit::max(SYNC_BODY_LIMIT)),
        )
        // KUAL self-update pull: the picker fetches its own next binary from the
        // staged `kual-dist/` bundle (written by the desktop app). Reads only,
        // token-gated like the rest — no new write surface.
        .route("/kual/manifest.json", get(get_kual_manifest))
        .route("/kual/file/{*name}", get(get_kual_file))
        .with_state(state)
}

/// Reads `data_dir/.server-token`, generating + persisting a fresh 32-byte
/// hex token on first run. The KUAL bundle's `etc/server.conf` will carry
/// the same value after Phase 6 wires up the install flow.
pub fn load_or_generate_token(data_dir: &StdPath) -> Result<String> {
    let path = data_dir.join(".server-token");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let t = s.trim();
        if !t.is_empty() {
            return Ok(t.to_string());
        }
    }
    std::fs::create_dir_all(data_dir).context("create data-dir")?;

    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = hex::encode(bytes);

    std::fs::write(&path, &token).context("write .server-token")?;
    // 0600 — token is a bearer secret, no need for group/world to read.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(token)
}

pub(crate) fn check_token(
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    expected: &str,
) -> Result<(), StatusCode> {
    // Header takes precedence (programmatic callers — KUAL helper, curl
    // scripts), then `?token=` fallback for browser navigations on `/kindle`
    // / `/dl/{id}` where setting a custom header isn't possible from a click.
    let got = headers
        .get("x-sidle-token")
        .and_then(|v| v.to_str().ok())
        .or_else(|| query.get("token").map(|s| s.as_str()))
        .ok_or(StatusCode::FORBIDDEN)?;
    // Constant-time compare — the token is short and the workload is single-
    // user so the practical impact is nil, but it costs nothing to do right.
    if !constant_eq(got.as_bytes(), expected.as_bytes()) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Unauthenticated liveness page. Browsers hitting `http://host:port/`
/// otherwise see a blank 404 — this gives a clear "yes, it's up" without
/// leaking any library content. The real endpoints stay token-gated.
async fn health() -> Response {
    let body = concat!(
        "sidle-server up.\n\n",
        "Endpoints (require token via X-Sidle-Token header or ?token= query):\n",
        "  GET /list.json     — library as JSON\n",
        "  GET /get/{id}      — book .kfx bytes\n",
        "  GET /cover/{id}    — cover image (?thumb=1 for a small color thumbnail)\n",
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    (headers, body).into_response()
}

pub(crate) fn open_db(paths: &LibraryPaths) -> Result<Connection, StatusCode> {
    db::open(&paths.db()).map_err(|err| {
        tracing::error!(?err, "open library.db failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

/// One `/list.json` entry: the full library row plus `device_filename`, the
/// canonical on-device name (`<basename>.<sha8>.kfx`) the client should save
/// the download as.
///
/// The name is computed here, server-side, with the same
/// [`kfx_device_filename`] rule the USB push uses — so a book pulled over the
/// LAN lands under a byte-identical name to one pushed over USB, and
/// sidle-tauri's USB-side delete recognizes it instead of flagging it as
/// foreign.
///
/// Why ship it in the JSON body rather than let the client read it off the
/// `/get/{id}` `Content-Disposition` header: every on-device name here is
/// non-ASCII (Japanese), and the native client's HTTP library (`ureq`)
/// silently drops header values containing bytes outside visible ASCII
/// (RFC 7230 field-vchar) — so that header is invisible to it. The body has
/// no such restriction. `None` only until a row has both `kfx_path` and
/// `kfx_sha256` (conversion + hashing done); such a row isn't downloadable
/// anyway (`get_book` 404s without a `kfx_path`).
#[derive(serde::Serialize)]
struct BookListEntry {
    #[serde(flatten)]
    row: db::BookRow,
    device_filename: Option<String>,
    /// Millisecond mtime of whatever `get_cover` would serve for this book (the
    /// color thumb if present, else the full-res cover), or 0 if it has no
    /// cover. The Kindle picker folds this into its on-device cover-cache
    /// filename so a desktop cover-recrawl — or a thumbnail format rebuild —
    /// bumps the rev and self-invalidates the stale thumbnail
    /// (`sidle/native/src/cover_cache.rs`).
    cover_rev: i64,
}

async fn list_json(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<Vec<BookListEntry>>, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    let conn = open_db(&state.paths)?;
    let books = db::list_books(&conn).map_err(|err| {
        tracing::error!(?err, "list_books failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let entries = books
        .into_iter()
        .map(|row| {
            let device_filename = match (row.kfx_path.as_deref(), row.kfx_sha256.as_deref()) {
                (Some(path), Some(sha)) => Some(kfx_device_filename(path, sha)),
                _ => None,
            };
            let cover_rev = cover_rev_millis(&state.paths, &row);
            BookListEntry {
                row,
                device_filename,
                cover_rev,
            }
        })
        .collect::<Vec<_>>();
    Ok(Json(entries))
}

/// Revision token for a book's cover: the millisecond mtime of whatever
/// [`get_cover`] would serve (the color thumb if present, else the full-res
/// cover). Returns 0 when the book has no cover or the file can't be stat'd.
///
/// Shipped in `/list.json` so the Kindle picker can key its cover cache on
/// `(id, rev)` and refetch automatically after a desktop recrawl rewrites the
/// cover — `recrawl`/`set_cover` rewrite the sidecar (and regenerate the
/// thumb), which bumps the mtime here.
fn cover_rev_millis(paths: &LibraryPaths, row: &db::BookRow) -> i64 {
    let path = {
        let thumb = paths.cover_thumb(&row.sha256);
        if thumb.exists() {
            thumb
        } else {
            match row.cover_path.as_deref() {
                Some(p) => PathBuf::from(p),
                None => return 0,
            }
        }
    };
    std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

async fn get_book(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    let conn = open_db(&state.paths)?;
    let book = db::get_book(&conn, id)
        .map_err(|err| {
            tracing::error!(?err, "get_book failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    let kfx_path = book.kfx_path.ok_or(StatusCode::NOT_FOUND)?;
    // The on-device name has to match the `<basename>.<sha8>.kfx` shape
    // sidle-tauri's USB push uses, so that a book downloaded via KUAL is
    // recognized by `device_list_ours` / `delete_one` and not flagged as
    // foreign. The bootstrap backfill (`state.rs`) populates `kfx_sha256`
    // for every row before the server takes requests; the fallback only
    // protects against an in-flight race where a freshly-converted row
    // hasn't had its sha hashed yet.
    let device_name = book
        .kfx_sha256
        .as_deref()
        .map(|sha| kfx_device_filename(&kfx_path, sha))
        .unwrap_or_else(|| filename_from_path(&kfx_path));
    serve_file(
        PathBuf::from(&kfx_path),
        "application/octet-stream",
        Some(&device_name),
    )
    .await
}

async fn get_cover(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    let conn = open_db(&state.paths)?;
    let book = db::get_book(&conn, id)
        .map_err(|err| {
            tracing::error!(?err, "get_book failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    let cover_path = book.cover_path.ok_or(StatusCode::NOT_FOUND)?;

    // The Kindle picker asks for `?thumb=1` — serve the small color thumbnail
    // produced at import (see sidle_core::library::thumbnail). If it isn't on
    // disk yet (the boot backfill hasn't reached this book, or it failed), fall
    // through to the full-res cover so the picker still gets something — just
    // slower for that one book until the thumbnail lands.
    let want_thumb = query.get("thumb").is_some_and(|v| v == "1" || v == "true");
    if want_thumb {
        let thumb = state.paths.cover_thumb(&book.sha256);
        if tokio::fs::try_exists(&thumb).await.unwrap_or(false) {
            return serve_file(thumb, "image/jpeg", None).await;
        }
    }

    let mime = mime_guess::from_path(&cover_path)
        .first_raw()
        .unwrap_or("image/jpeg");
    serve_file(PathBuf::from(&cover_path), mime, None).await
}

// ---------------------------------------------------------------------------
// P3 write surface — POST /sync/annotations
// ---------------------------------------------------------------------------

/// The push bundle the Kindle picker (or any USB-less client) sends: each
/// `.sdr`'s reading-state sidecars, base64 in JSON so the armv7 native client
/// needs no multipart dep — it already speaks serde. The byte content mirrors
/// exactly what the USB scanner ([`ingest::import_from_device`]) reads off a
/// mounted volume. Base64 is standard alphabet *with* padding (`STANDARD`).
#[derive(serde::Deserialize)]
struct SyncRequest {
    /// The Kindle's own serial — annotations are keyed per device (delete
    /// propagation, last-read position). Provisioned to the picker via
    /// `server.conf` (the Mac reads the USB iSerial at mount time).
    device_serial: String,
    sdrs: Vec<SyncSdr>,
}

#[derive(serde::Deserialize)]
struct SyncSdr {
    /// `<stem>.<sha8>.sdr` — the `sha8` infix matches a library book's `kfx_sha256`.
    sdr_name: String,
    /// `<book>.yjr` bytes (annotations), base64. Absent if read-without-highlight.
    #[serde(default)]
    yjr_b64: Option<String>,
    /// `<book>.yjf` bytes (last-read position), base64.
    #[serde(default)]
    yjf_b64: Option<String>,
}

/// Ingest pushed annotations — the LAN twin of the USB import. Token-gated, then
/// base64-decode → `Vec<CollectedYjr>` → the **exact** USB-path function
/// [`ingest::import_collected`], so a LAN import is byte-for-byte the same DB
/// operation as a USB sync (that is the P3 gate). Returns the `DeviceImportReport`
/// so the picker can show "N new highlights", mirroring the USB report.
///
/// `Json` is the last parameter because it consumes the request body (axum
/// requires body-consuming extractors last).
async fn sync_annotations(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(req): Json<SyncRequest>,
) -> Result<Json<DeviceImportReport>, StatusCode> {
    check_token(&headers, &query, &state.token)?;

    // Decode every base64 blob up front: a malformed one is a 400 (client bug),
    // kept distinct from a 500 (our ingest failing) below.
    let mut collected = Vec::with_capacity(req.sdrs.len());
    for sdr in req.sdrs {
        collected.push(CollectedYjr {
            sdr_name: sdr.sdr_name,
            yjr_bytes: decode_b64_opt(sdr.yjr_b64.as_deref())?,
            yjf_bytes: decode_b64_opt(sdr.yjf_b64.as_deref())?,
        });
    }

    let paths = state.paths.clone();
    let device_serial = req.device_serial;

    // rusqlite is blocking; run the whole import (and the pulse write) off the
    // async executor. Per-request `db::open` is the server's existing pattern;
    // the `busy_timeout` it sets serializes this writer against the GUI's.
    let report = tokio::task::spawn_blocking(move || -> Result<DeviceImportReport, StatusCode> {
        let conn = db::open(&paths.db()).map_err(|err| {
            tracing::error!(?err, "sync: open library.db failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        let report = ingest::import_collected(
            &conn,
            collected,
            &device_serial,
            &db::now_iso(),
        )
        .map_err(|err| {
            tracing::error!(?err, "sync: import_collected failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        // Live-repaint signal — only when the import changed annotation state worth
        // repainting an open reader for. The GUI watches this file (sidle-reader.md
        // P3) and re-emits the `annotations:sync-done` event the USB path already
        // fires; the daemon can't emit a Tauri event into the app directly.
        if import_changed_anything(&report) {
            write_sync_pulse(&paths, &device_serial, &report);
        }
        Ok(report)
    })
    .await
    .map_err(|err| {
        tracing::error!(?err, "sync: import task panicked");
        StatusCode::INTERNAL_SERVER_ERROR
    })??;

    Ok(Json(report))
}

/// Base64-decode an optional blob (standard alphabet, with padding); a decode
/// error maps to `400 Bad Request`.
fn decode_b64_opt(s: Option<&str>) -> Result<Option<Vec<u8>>, StatusCode> {
    use base64::Engine as _;
    s.map(|s| {
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(|_| StatusCode::BAD_REQUEST)
    })
    .transpose()
}

/// Did the import change anything an open reader would render? New or removed
/// annotations, or a moved last-read position, warrant a repaint pulse; a re-sync
/// of unchanged `.yjr` (pure duplicates) does not. (`unresolved` rows are a subset
/// of `inserted`, so checking `inserted` already covers them.)
fn import_changed_anything(r: &DeviceImportReport) -> bool {
    r.annotations.inserted > 0 || r.positions > 0 || r.relinked > 0
}

/// Atomically write `<root>/.sync-pulse.json` — the cross-process signal the GUI
/// watches to live-repaint an open reader after a LAN sync. Sits beside
/// `server.pid`/`server.log`. Best-effort: a failed pulse just means no live
/// repaint (the next GUI poll / manual reload still shows the rows); the DB write
/// already succeeded, so this never fails the request.
fn write_sync_pulse(paths: &LibraryPaths, device_serial: &str, report: &DeviceImportReport) {
    let pulse = serde_json::json!({
        "ts": db::now_iso(),
        "device_serial": device_serial,
        "report": report,
    });
    let Ok(bytes) = serde_json::to_vec(&pulse) else {
        return;
    };
    let final_path = paths.root.join(".sync-pulse.json");
    let tmp_path = paths.root.join(".sync-pulse.json.tmp");
    if let Err(err) = std::fs::write(&tmp_path, &bytes) {
        tracing::warn!(?err, "sync pulse: write tmp failed");
        return;
    }
    if let Err(err) = std::fs::rename(&tmp_path, &final_path) {
        tracing::warn!(?err, "sync pulse: rename failed");
        let _ = std::fs::remove_file(&tmp_path);
    }
}

/// Minimal `Deserialize` view of `kual-dist/manifest.json`: the server only
/// needs each entry's `name` to whitelist the served files. Mirrors the
/// desktop's `KualManifest` (which also carries `sha256`/`size`) rather than
/// sharing the type across the crate boundary — the same mirror-struct
/// convention the sync DTOs use; serde drops the extra fields.
#[derive(serde::Deserialize)]
struct KualManifest {
    files: Vec<KualManifestEntry>,
}

#[derive(serde::Deserialize)]
struct KualManifestEntry {
    name: String,
}

fn read_kual_manifest(dist_dir: &StdPath) -> Option<KualManifest> {
    let bytes = std::fs::read(dist_dir.join("manifest.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `GET /kual/manifest.json` → the staged manifest verbatim (token-gated). 404
/// when nothing's been staged yet; the picker reads that as "no update".
async fn get_kual_manifest(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    let path = state.paths.kual_dist().join("manifest.json");
    serve_file(path, "application/json", None).await
}

/// `GET /kual/file/{*name}` → bytes from `kual-dist/<name>` (token-gated).
///
/// `name` is whitelisted against the manifest's entries: only a file the
/// manifest declares is servable. That's both the access bound (just the staged
/// set) and the path-traversal guard — a `name` like `../library.db` isn't a
/// manifest entry, so it 404s before any path join. The catch-all `{*name}` is
/// needed because entries carry a `/` (e.g. `bin/sidle`).
async fn get_kual_file(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    let dist = state.paths.kual_dist();
    let manifest = read_kual_manifest(&dist).ok_or(StatusCode::NOT_FOUND)?;
    if !manifest.files.iter().any(|f| f.name == name) {
        return Err(StatusCode::NOT_FOUND);
    }
    serve_file(dist.join(&name), "application/octet-stream", None).await
}

async fn serve_file(
    path: PathBuf,
    content_type: &str,
    download_filename: Option<&str>,
) -> Result<Response, StatusCode> {
    let bytes = tokio::fs::read(&path).await.map_err(|err| {
        tracing::warn!(?err, path = %path.display(), "serve_file: read failed");
        StatusCode::NOT_FOUND
    })?;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(content_type).unwrap());
    if let Some(name) = download_filename {
        // Quote-escape any embedded `"` in the filename. RFC 6266 says we
        // should also percent-encode for non-ASCII via `filename*=UTF-8''…`
        // but every Kindle test target so far handles plain `filename="…"`
        // with UTF-8 bytes, so keep this simple until something breaks.
        let safe = name.replace('"', "'");
        let val = format!("attachment; filename=\"{safe}\"");
        if let Ok(hv) = HeaderValue::from_str(&val) {
            headers.insert(header::CONTENT_DISPOSITION, hv);
        }
    }
    Ok((headers, bytes).into_response())
}

fn filename_from_path(p: &str) -> String {
    StdPath::new(p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("book.kfx")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        AppState, Config, build_router, import_changed_anything, load_or_generate_token,
        serve_with_shutdown,
    };
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use sidle_core::library::db::{self, NewBook};
    use sidle_core::library::ingest::{self, CollectedYjr, DeviceImportReport};
    use sidle_core::library::LibraryPaths;
    use std::path::Path;
    use std::sync::Arc;
    use tower::ServiceExt as _;

    fn pick_free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Graceful shutdown: the daemon serves `GET /` (200), then when the
    /// shutdown future resolves `serve_with_shutdown` returns `Ok(())` and frees
    /// the port — versus the old `axum::serve(..).await`, which only returned on
    /// a bind error. This is the wiring SIGTERM/sakabar-stop/app-stop all rely
    /// on; true in-flight-request drain timing is exercised by the live gate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_shutdown_returns_ok_and_frees_port() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths { root: tmp.path().to_path_buf() };
        paths.ensure().unwrap();
        let token = load_or_generate_token(&paths.root).unwrap();
        let port = pick_free_port();
        let config = Config {
            paths,
            bind: format!("127.0.0.1:{port}"),
            token,
        };

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_with_shutdown(config, async move {
            let _ = rx.await;
        }));

        // Prove it serves HTTP, with a blocking std client off the runtime so we
        // don't need tokio's io-util ext traits. Retries until the spawned axum
        // task has bound + is accepting.
        let served = tokio::task::spawn_blocking(move || {
            use std::io::{Read, Write};
            use std::time::{Duration, Instant};
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                    s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                        .unwrap();
                    let mut buf = String::new();
                    let _ = s.read_to_string(&mut buf);
                    return buf.starts_with("HTTP/1.1 200");
                }
                if Instant::now() > deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
        .await
        .unwrap();
        assert!(served, "daemon never served a 200 on /");

        // Fire shutdown → serve_with_shutdown must return Ok promptly.
        tx.send(()).unwrap();
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("serve task did not finish after the shutdown signal")
            .expect("serve task panicked");
        assert!(res.is_ok(), "serve returned an error: {res:?}");

        // Port released (allow the OS a moment).
        let mut refused = false;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused, "port {port} never freed after shutdown");
    }

    // --- POST /sync/annotations (the P3 LAN==USB gate) ---------------------

    /// A synthetic bookmark `.yjr`: `[marker][len:3 BE][payload]` tokens — the
    /// same recipe `sidle-core`'s ingest tests use. One bookmark = the marker
    /// key followed by a single anchor-handle string value.
    fn yjr_bookmark(eid: u32, off: u32, linear: u64) -> Vec<u8> {
        fn token(marker: u8, payload: &[u8]) -> Vec<u8> {
            let len = payload.len();
            let mut v = vec![marker, (len >> 16) as u8, (len >> 8) as u8, len as u8];
            v.extend_from_slice(payload);
            v
        }
        let mut raw = vec![1u8];
        raw.extend_from_slice(&eid.to_le_bytes());
        raw.extend_from_slice(&off.to_le_bytes());
        let handle = format!(
            "{}:{linear}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(&raw)
        );
        let mut yjr = Vec::new();
        yjr.extend(token(0xfe, b"annotation.personal.bookmark"));
        yjr.extend(token(0x03, handle.as_bytes()));
        yjr
    }

    /// A fresh library root + one book whose `kfx_sha256` the `book.deadbeef.sdr`
    /// infix prefix-matches (so the bookmark resolves). Returns `(paths, book_id)`.
    fn library_with_matching_book(root: &Path) -> (LibraryPaths, i64) {
        let paths = LibraryPaths { root: root.to_path_buf() };
        paths.ensure().unwrap();
        let conn = db::open(&paths.db()).unwrap();
        let book_id = db::insert_book(
            &conn,
            &NewBook {
                sha256: "book-sha",
                title: "栞のある本",
                author: "Author",
                language: "ja",
                ppd: None,
                epub_path: None,
                cover_path: None,
                kfx_path: None, // empty index; a bookmark imports on its anchor alone
                kfx_sha256: Some(
                    "deadbeef00000000000000000000000000000000000000000000000000000000",
                ),
                pdf_path: None,
                file_size: 0,
                imported_at: "t0",
                asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
            },
        )
        .unwrap();
        (paths, book_id)
    }

    /// Build a `POST /sync/annotations` request; the `AppState` goes into
    /// `build_router` at the call site.
    fn sync_request(token: Option<&str>, body: serde_json::Value) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri("/sync/annotations")
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("x-sidle-token", t);
        }
        b.body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap()
    }

    /// The decisive P3 gate: a `.yjr` pushed through `POST /sync/annotations`
    /// produces the **identical** `DeviceImportReport` and the **identical**
    /// stored annotation rows as calling `import_collected` on the same bundle
    /// directly (the USB path) — because the handler routes through that exact
    /// function. Two separate library roots so the two imports don't interfere.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_annotations_equals_usb_import() {
        let yjr = yjr_bookmark(1492, 0, 9);
        let device_serial = "G000TESTSERIAL";
        let sdr_name = "book.deadbeef.sdr";

        // Reference (USB path): import_collected directly on root A.
        let tmp_a = tempfile::tempdir().unwrap();
        let (paths_a, book_a) = library_with_matching_book(tmp_a.path());
        let conn_a = db::open(&paths_a.db()).unwrap();
        let report_ref = ingest::import_collected(
            &conn_a,
            vec![CollectedYjr {
                sdr_name: sdr_name.to_string(),
                yjr_bytes: Some(yjr.clone()),
                yjf_bytes: None,
            }],
            device_serial,
            "now",
        )
        .unwrap();
        assert_eq!(report_ref.matched, 1);
        assert!(report_ref.annotations.inserted >= 1);
        let rows_a = db::list_annotations_for_book(&conn_a, book_a).unwrap();

        // Actual (LAN path): POST the same bundle (base64) to root B.
        let tmp_b = tempfile::tempdir().unwrap();
        let (paths_b, book_b) = library_with_matching_book(tmp_b.path());
        let token = load_or_generate_token(&paths_b.root).unwrap();
        let state = AppState {
            paths: paths_b.clone(),
            token: Arc::from(token.as_str()),
        };
        let body = serde_json::json!({
            "device_serial": device_serial,
            "sdrs": [ { "sdr_name": sdr_name, "yjr_b64": BASE64.encode(&yjr) } ],
        });
        let resp = build_router(state)
            .oneshot(sync_request(Some(&token), body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let report_http: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        // LAN == USB: identical report ...
        assert_eq!(report_http, serde_json::to_value(&report_ref).unwrap());

        // ... and identical stored rows (compare the content hash + text, not the
        // per-DB `imported_at` timestamp).
        let conn_b = db::open(&paths_b.db()).unwrap();
        let rows_b = db::list_annotations_for_book(&conn_b, book_b).unwrap();
        assert!(!rows_a.is_empty(), "USB path stored no rows");
        assert_eq!(rows_a.len(), rows_b.len(), "LAN row count differs from USB");
        assert_eq!(rows_a[0].dedup_hash, rows_b[0].dedup_hash);
        assert_eq!(rows_a[0].text, rows_b[0].text);

        // The live-repaint pulse landed (this import changed state).
        let pulse = paths_b.root.join(".sync-pulse.json");
        assert!(pulse.exists(), "sync pulse not written after a changing import");
        let pulse_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&pulse).unwrap()).unwrap();
        assert_eq!(pulse_json["device_serial"], device_serial);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_annotations_rejects_missing_token() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, _) = library_with_matching_book(tmp.path());
        let state = AppState { paths, token: Arc::from("the-real-token") };
        let body = serde_json::json!({ "device_serial": "X", "sdrs": [] });
        let resp = build_router(state)
            .oneshot(sync_request(None, body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_annotations_rejects_bad_base64() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, _) = library_with_matching_book(tmp.path());
        let token = load_or_generate_token(&paths.root).unwrap();
        let state = AppState { paths, token: Arc::from(token.as_str()) };
        let body = serde_json::json!({
            "device_serial": "X",
            "sdrs": [ { "sdr_name": "book.deadbeef.sdr", "yjr_b64": "@@@not base64@@@" } ],
        });
        let resp = build_router(state)
            .oneshot(sync_request(Some(&token), body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_annotations_enforces_body_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, _) = library_with_matching_book(tmp.path());
        let token = load_or_generate_token(&paths.root).unwrap();
        let state = AppState { paths, token: Arc::from(token.as_str()) };
        // A body past the 32 MB cap → rejected by DefaultBodyLimit before the
        // handler runs (413), regardless of token validity.
        // A body past the 32 MB cap is built by inflating a sdr_name string —
        // anything inside the JSON suffices; the layer trips before our handler.
        let huge = "a".repeat(super::SYNC_BODY_LIMIT + 1024);
        let body = serde_json::json!({
            "device_serial": "X",
            "sdrs": [ { "sdr_name": huge } ],
        });
        let resp = build_router(state)
            .oneshot(sync_request(Some(&token), body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn pulse_fires_only_on_actual_change() {
        let mut r = DeviceImportReport::default();
        assert!(!import_changed_anything(&r), "empty report → no pulse");
        r.annotations.inserted = 1;
        assert!(import_changed_anything(&r), "a new annotation → pulse");
    }

    // --- GET /kual/... (LAN self-update pull) ------------------------------

    /// A `GET` request with an optional `x-sidle-token` header.
    fn get_request(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(uri);
        if let Some(t) = token {
            b = b.header("x-sidle-token", t);
        }
        b.body(Body::empty()).unwrap()
    }

    /// Stage a `kual-dist/` bundle (one binary + manifest) under `paths`,
    /// mirroring what the desktop app's `stage_dist` writes. Returns the bytes.
    fn stage_fake_dist(paths: &LibraryPaths) -> Vec<u8> {
        let dist = paths.kual_dist();
        std::fs::create_dir_all(dist.join("bin")).unwrap();
        let bytes = b"\x7fELF-fake-armv7-picker".to_vec();
        std::fs::write(dist.join("bin/sidle"), &bytes).unwrap();
        let manifest = serde_json::json!({
            "files": [ { "name": "bin/sidle", "sha256": "deadbeef", "size": bytes.len() } ]
        });
        std::fs::write(
            dist.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        bytes
    }

    fn staged_state(root: &Path) -> (AppState, String, Vec<u8>) {
        let paths = LibraryPaths { root: root.to_path_buf() };
        paths.ensure().unwrap();
        let token = load_or_generate_token(&paths.root).unwrap();
        let bytes = stage_fake_dist(&paths);
        let state = AppState { paths, token: Arc::from(token.as_str()) };
        (state, token, bytes)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kual_manifest_and_file_serve_staged_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token, bin_bytes) = staged_state(tmp.path());

        let resp = build_router(state.clone())
            .oneshot(get_request("/kual/manifest.json", Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(manifest["files"][0]["name"], "bin/sidle");

        // The catch-all `{*name}` captures the `bin/sidle` path and the bytes
        // come back byte-identical to what was staged.
        let resp = build_router(state)
            .oneshot(get_request("/kual/file/bin/sidle", Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), bin_bytes.as_slice(), "served == staged binary");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kual_file_404s_names_absent_from_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token, _) = staged_state(tmp.path());
        // Real bundle files that the v1 manifest deliberately does NOT list —
        // the whitelist is what keeps the token + other device files unreachable
        // (and, by the same check, blocks any traversal name).
        for uri in ["/kual/file/etc/server.conf", "/kual/file/bin/sidle.sh"] {
            let resp = build_router(state.clone())
                .oneshot(get_request(uri, Some(&token)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "unlisted {uri} must 404");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kual_endpoints_reject_missing_token() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _, _) = staged_state(tmp.path());
        for uri in ["/kual/manifest.json", "/kual/file/bin/sidle"] {
            let resp = build_router(state.clone())
                .oneshot(get_request(uri, None))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri} must require a token");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kual_manifest_404_when_nothing_staged() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths { root: tmp.path().to_path_buf() };
        paths.ensure().unwrap();
        let token = load_or_generate_token(&paths.root).unwrap();
        let state = AppState { paths, token: Arc::from(token.as_str()) };
        let resp = build_router(state)
            .oneshot(get_request("/kual/manifest.json", Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
