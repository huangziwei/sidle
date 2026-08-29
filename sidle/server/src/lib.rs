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

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use rusqlite::Connection;

use sidle_core::library::{
    LibraryPaths, db,
    device::{digest::DigestCache, dist},
    device_backup::{self, SyncCollections},
    import::{self, ImportOutcome},
    ingest::{self, CollectedYjr, DeviceImportReport},
    ink::{self, CollectedInk},
    notebook::{self, NotebookOutcome},
    paths::kfx_device_filename,
    push, reading_log,
};

// We call `db::open` per request (rather than holding a long-lived `Arc<
// Mutex<Connection>>` like the Tauri side) because the server's workload is
// stateless reads. Cost: re-runs the idempotent migrations on every hit,
// which is a handful of PRAGMA / `has_column` queries — negligible at
// single-user LAN scale.

/// Runtime configuration assembled by either the CLI or the embedded
/// caller. The token is loaded/generated outside and passed in so the same
/// secret can also be written into the on-device app's `etc/server.conf` at
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
/// `kill`, an external port-kill, an app-initiated stop, and Ctrl-C all drain
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

    let paths = config.paths.clone();
    let state = AppState {
        paths: config.paths,
        token: Arc::from(config.token),
    };

    let app = build_router(state);

    // rustls 0.23 needs a process-wide default provider before any config is
    // built. `ring` rather than aws-lc-rs: it is already in the workspace graph,
    // so this adds no new C build. Erroring means someone else installed one
    // first (the embedded case), which is fine — theirs wins and works.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Fail loudly when the material is missing rather than falling back to
    // plaintext: a server that quietly serves HTTP is exactly the outcome TLS
    // exists to prevent, and the token would ride in the clear on every request
    // without anything visibly wrong.
    let tls = RustlsConfig::from_pem_file(paths.server_cert(), paths.server_key())
        .await
        .with_context(|| {
            format!(
                "load TLS material ({} / {}) — run the desktop app once to issue it",
                paths.server_cert().display(),
                paths.server_key().display()
            )
        })?;

    // std listener, not tokio's: `from_tcp_rustls` takes it, and binding here
    // lets us log the port the OS actually chose (a `:0` bind in tests) before
    // handing it over.
    let listener = std::net::TcpListener::bind(&config.bind)
        .with_context(|| format!("bind {}", config.bind))?;
    listener
        .set_nonblocking(true)
        .context("set listener nonblocking")?;
    let local = listener.local_addr()?;
    tracing::info!("sidle-server listening on https://{local}");

    // axum-server drains via a `Handle` rather than a shutdown future, so bridge
    // the two: the caller's future still decides *when*, and in-flight requests
    // still get to finish. The timeout bounds a wedged request so a stop can't
    // hang forever — SIGTERM callers (the app's stop, a supervisor, plain `kill`)
    // expect the process to actually go away.
    let handle = axum_server::Handle::new();
    let drain = handle.clone();
    tokio::spawn(async move {
        shutdown.await;
        tracing::info!("sidle-server: draining in-flight requests");
        drain.graceful_shutdown(Some(SHUTDOWN_GRACE));
    });

    let serving = axum_server::from_tcp_rustls(listener, tls.clone())
        .context("adopt TCP listener for TLS")?
        .handle(handle)
        .serve(app.into_make_service());

    // `follow_lan_address` inside this future lives as long as `serving`: an
    // aborted embedded task drops both.
    tokio::select! {
        served = serving => served.context("axum_server::serve")?,
        () = follow_lan_address(
            paths.clone(),
            tls,
            sidle_core::library::device::deploy::detect_lan_ipv4,
            ADDRESS_POLL,
        ) => {}
    }
    Ok(())
}

/// Interval between [`follow_lan_address`] checks of `address`.
const ADDRESS_POLL: std::time::Duration = std::time::Duration::from_secs(30);

/// Re-issue and hot-swap `tls` for each new value of `address`.
///
/// `issue_server_cert` names the machine's address in a SAN; a client dialling
/// an address the leaf omits refuses the handshake. `ensure_ca` leaves the root
/// alone, so a device keeps trusting the CA it holds across every swap.
///
/// `address` of `None` keeps the loaded leaf: an interface that stopped routing
/// leaves the addresses in the leaf reachable.
///
/// Runs until dropped.
async fn follow_lan_address(
    paths: LibraryPaths,
    tls: RustlsConfig,
    address: impl Fn() -> Option<std::net::Ipv4Addr>,
    poll: std::time::Duration,
) {
    use sidle_core::library::daemon;

    let mut current = address();
    loop {
        tokio::time::sleep(poll).await;
        let found = address();
        if found.is_none() || found == current {
            continue;
        }
        current = found;

        let issued = {
            let paths = paths.clone();
            tokio::task::spawn_blocking(move || daemon::ensure_tls_material(&paths)).await
        };
        match issued {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::warn!(
                    ?err,
                    "LAN address moved but the leaf could not be re-issued"
                );
                continue;
            }
            Err(err) => {
                tracing::warn!(?err, "TLS re-issue task panicked");
                continue;
            }
        }
        match tls
            .reload_from_pem_file(paths.server_cert(), paths.server_key())
            .await
        {
            Ok(()) => tracing::info!(
                address = ?current,
                "LAN address moved — server certificate re-issued and reloaded"
            ),
            Err(err) => tracing::warn!(?err, "re-issued leaf could not be loaded"),
        }
    }
}

/// How long a drain waits for in-flight requests before the listener closes
/// regardless. Generous enough for a book download to finish on a slow device
/// link, short enough that a stop is still a stop.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// 32 MB body cap on `POST /sync/annotations` and `POST /sync/reading-log`. A
/// whole library's `.yjr`/`.yjf` sidecars (KB each) fit with generous headroom,
/// as does the ink that rides along with them (an `nbk` of pen strokes on a book
/// is tens of KB); bounds the JSON buffer the body extractor builds so a
/// stray/oversized POST can't exhaust memory.
const SYNC_BODY_LIMIT: usize = 32 * 1024 * 1024;
/// Body cap on `POST /sync/notebooks`. A standalone notebook is a much larger
/// `nbk` than a book's ink — a well-used one reaches a couple of MB — so this
/// sits higher. It is not sized for a whole Scribe at once on purpose: the
/// device batches its upload to bound its own RAM, and a cap it can't reach is
/// no cap at all.
const NOTEBOOK_BODY_LIMIT: usize = 64 * 1024 * 1024;
/// Body cap on `POST /sync/book` — one decrypted book, buffered whole before
/// it's handed to the importer. Generous for an image-heavy purchase;
/// single-user LAN, and the transient RAM is fine.
const BOOK_BODY_LIMIT: usize = 512 * 1024 * 1024;
/// Body cap on `POST /sync/misc` — one batch of the Kindle's configured folders,
/// base64 in one JSON bundle. The picker batches to a fraction of this (its own
/// memory is the tighter constraint), so the headroom is for a single outsized
/// file, which goes in a batch of its own. Since a collection can point anywhere,
/// a 413 on one is a clear error rather than a silent drop; the picker deletes
/// nothing when a batch fails, so the next Sync re-sends it.
const MISC_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// The axum app: routes + per-route layers. Factored out of
/// [`serve_with_shutdown`] so tests can drive it via `Router::oneshot` without
/// binding a socket — exercising the real route table and the body-limit layer.
pub(crate) fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(health))
        .route("/list.json", get(list_json))
        .route("/get/{id}", get(get_book))
        .route("/cover/{id}", get(get_cover))
        .route("/sidecar/{id}", get(get_sidecar))
        // Write surface: the Kindle pushes its `.yjr`/`.yjf` here for ingest —
        // the LAN twin of the USB import. Token-gated like the reads; body-limited
        // so an oversized POST can't exhaust memory.
        // The GET is the ink half's watermark: what this library already holds
        // for the asking device, so the POST carries only changed notebooks.
        .route(
            "/sync/annotations",
            get(ink_manifest)
                .post(sync_annotations)
                .layer(DefaultBodyLimit::max(SYNC_BODY_LIMIT)),
        )
        // The Scribe's standalone notebooks, same ask-then-send shape.
        .route(
            "/sync/notebooks",
            get(notebook_manifest)
                .post(sync_notebooks)
                .layer(DefaultBodyLimit::max(NOTEBOOK_BODY_LIMIT)),
        )
        .route(
            "/sync/book",
            post(sync_book).layer(DefaultBodyLimit::max(BOOK_BODY_LIMIT)),
        )
        // The Kindle pushes the folders this library asked for here on Sync — a
        // WiFi backup, stored under `device-backup/<serial>/` (no DB, view-only
        // in the desktop Files tab). The GET is what it asks with: the library
        // owns the collection list, so a folder added or renamed in the app's
        // settings reaches the picker on its next Sync with no redeploy.
        // Token-gated + body-limited like the rest.
        .route(
            "/sync/misc",
            get(misc_collections)
                .post(sync_misc)
                .layer(DefaultBodyLimit::max(MISC_BODY_LIMIT)),
        )
        // The Kindle asks how far this library has already read, then pushes only
        // the events past that point. The GET is what keeps a sync cheap: it lets
        // the device skip whole log dumps on their filename alone.
        .route(
            "/sync/reading-log",
            get(reading_log_watermark)
                .post(sync_reading_log)
                .layer(DefaultBodyLimit::max(SYNC_BODY_LIMIT)),
        )
        // On-device app self-update pull: the picker fetches its own next binary
        // from the staged `device-dist/` bundle (written by the desktop app).
        // Reads only, token-gated like the rest — no new write surface.
        .route("/device/manifest.json", get(get_dist_manifest))
        .route("/device/file/{*name}", get(get_dist_file))
        .with_state(state)
}

/// Reads `data_dir/.server-token`, generating + persisting a fresh 32-byte
/// hex token on first run. The on-device app's `etc/server.conf` will carry
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
    // Header takes precedence — it is what every client sends. The `?token=`
    // fallback covers a plain browser navigation or a pasted URL, where setting
    // a custom header isn't possible.
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
/// the desktop app's USB-side delete recognizes it instead of flagging it as
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
    // The Kindle picker's cover cache token (`cover_rev`) now rides in on the
    // flattened `row` — `db::BookRow::cover_rev`, the ms mtime of the served
    // image, computed once in `row_to_book`. No separate field here (a sibling
    // would collide with the flattened key).
    /// Content revision of the served KFX: the file's ms mtime. `kfx_sha256` is
    /// a frozen device identity, so a reconvert that rewrites the bytes doesn't
    /// change the on-device filename — the picker compares this against the
    /// rev it last downloaded and re-pulls a stale book in place. 0 when the row
    /// has no `kfx_path` (not downloadable anyway). Distinct sibling key, so no
    /// collision with the flattened `row`.
    kfx_rev: i64,
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
            let kfx_rev = row
                .kfx_path
                .as_deref()
                .map(db::path_mtime_millis)
                .unwrap_or(0);
            BookListEntry {
                row,
                device_filename,
                kfx_rev,
            }
        })
        .collect::<Vec<_>>();
    Ok(Json(entries))
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
    // the desktop app's USB push uses, so that a book downloaded via the picker is
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

/// The reading-state sidecar for one book, composed from the library alone.
///
/// A book arriving on a device for the first time has no `.sdr` yet, so it
/// appears in no sync bundle and the ordinary push — which answers what the
/// device sent — has nothing to answer. Its highlights would sit in the library
/// until the reader opened the book and synced again. This is the same
/// composer, asked directly: give me what this book's sidecar should contain
/// for a device holding nothing.
///
/// `204` when the library has nothing to write, so a caller can ask for every
/// download without treating "no highlights yet" as a failure.
async fn get_sidecar(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    let serial = query.get("serial").cloned().unwrap_or_default();
    let paths = state.paths.clone();

    let composed = tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>, StatusCode> {
        let conn = open_db(&paths)?;
        let book = db::get_book(&conn, id)
            .map_err(|err| {
                tracing::error!(?err, "sidecar: get_book failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
            .ok_or(StatusCode::NOT_FOUND)?;
        let colors = db::device_uses_colors(&conn, &serial).unwrap_or(false);
        let index = ingest::book_index(book.kfx_path.as_deref());
        push::compose(&conn, book.id, None, &index, colors)
            .map(|c| c.map(|c| c.bytes))
            .map_err(|err| {
                tracing::error!(?err, "sidecar: compose failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })
    })
    .await
    .map_err(|err| {
        tracing::error!(?err, "sidecar: join failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })??;

    match composed {
        Some(bytes) => Ok((
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
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
// Write surface — POST /sync/annotations
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
    /// Handwritten ink drawn on sideloaded books, one entry per host book, as
    /// pulled from `.notebooks/<asin>!!PDOC!!notebook/nbk`.
    ///
    /// It rides in the annotation bundle rather than a route of its own because
    /// an ink page anchors to its host page through the `handwritten_note`
    /// records in that book's `.yjr` — the anchors are in `sdrs`, so separating
    /// the two would import every page unanchored. Same reason the USB path
    /// folds ink into its annotation pass. Empty from a device with no pen.
    #[serde(default)]
    inks: Vec<SyncInk>,
}

/// One host book's ink notebook. Sent only when its content sha differs from
/// what `GET /sync/annotations` reported, so a steady-state sync carries none.
#[derive(serde::Deserialize)]
struct SyncInk {
    /// The host book's baked content_id (`books.asin`) — the `<asin>` in the
    /// device's `.notebooks/<asin>!!PDOC!!notebook` dir name.
    asin: String,
    /// The `nbk` (a KDF SQLite file), base64.
    nbk_b64: String,
}

/// What `GET /sync/annotations` answers: the ink content shas this library has
/// already decoded for the asking device, `{asin: nbk_sha}`.
///
/// The device holds its own filesystem, so it hashes `.notebooks/` locally and
/// sends only what this map doesn't already account for. Per-device because
/// `ink_sync` is: the same book inked on two Kindles is two separate facts.
#[derive(serde::Serialize)]
struct InkManifest {
    ink: HashMap<String, String>,
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
    /// The sidecars' own filenames. A write-back has to reuse the exact name —
    /// it carries a device-specific infix — so the device tells us what it is
    /// rather than the server guessing.
    #[serde(default)]
    yjr_name: Option<String>,
    #[serde(default)]
    yjf_name: Option<String>,
}

/// A sidecar the device should write, returned alongside the import report so
/// one round trip covers both directions.
#[derive(serde::Serialize)]
struct OutgoingSdr {
    sdr_name: String,
    file_name: String,
    /// The whole `.yjr` to write, base64.
    yjr_b64: String,
}

/// The LAN sync's answer: what came in, and what should go back out.
///
/// The report is flattened so the fields the picker already reads stay where
/// they were; `write` is additive.
#[derive(serde::Serialize)]
struct SyncResponse {
    #[serde(flatten)]
    report: DeviceImportReport,
    write: Vec<OutgoingSdr>,
}

/// The device serial a manifest request is asking about. Refused rather than
/// defaulted: an empty serial would hand one Kindle another's checkpoint, and
/// the device would then skip uploading ink this library has never seen.
fn required_serial(query: &HashMap<String, String>) -> Result<String, StatusCode> {
    match query.get("device_serial") {
        Some(s) if !s.is_empty() => Ok(s.clone()),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

/// `GET /sync/annotations?device_serial=…` — the ink shas already decoded for
/// this device, so its next POST carries only notebooks that actually changed.
async fn ink_manifest(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<InkManifest>, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    let serial = required_serial(&query)?;
    let conn = open_db(&state.paths)?;
    let ink = db::ink_sync_shas(&conn, &serial).map_err(|err| {
        tracing::error!(?err, "sync/annotations: ink manifest query failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(InkManifest {
        ink: ink.into_iter().collect(),
    }))
}

/// Ingest pushed annotations — the LAN twin of the USB import. Token-gated, then
/// base64-decode → `Vec<CollectedYjr>` → the **exact** USB-path function
/// [`ingest::import_collected`], so a LAN import is byte-for-byte the same DB
/// operation as a USB sync. Returns the `DeviceImportReport`
/// so the picker can show "N new highlights", mirroring the USB report.
///
/// `Json` is the last parameter because it consumes the request body (axum
/// requires body-consuming extractors last).
async fn sync_annotations(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(req): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, StatusCode> {
    check_token(&headers, &query, &state.token)?;

    // Decode every base64 blob up front: a malformed one is a 400 (client bug),
    // kept distinct from a 500 (our ingest failing) below.
    let mut collected = Vec::with_capacity(req.sdrs.len());
    for sdr in req.sdrs {
        collected.push(CollectedYjr {
            sdr_name: sdr.sdr_name,
            yjr_bytes: decode_b64_opt(sdr.yjr_b64.as_deref())?,
            yjf_bytes: decode_b64_opt(sdr.yjf_b64.as_deref())?,
            yjr_name: sdr.yjr_name,
            yjf_name: sdr.yjf_name,
        });
    }
    let mut inks = Vec::with_capacity(req.inks.len());
    for ink in req.inks {
        inks.push(CollectedInk {
            asin: ink.asin,
            nbk_bytes: decode_b64(&ink.nbk_b64)?,
        });
    }

    let paths = state.paths.clone();
    let device_serial = req.device_serial;

    // rusqlite is blocking; run the whole import (and the pulse write) off the
    // async executor. Per-request `db::open` is the server's existing pattern;
    // the `busy_timeout` it sets serializes this writer against the GUI's.
    let response = tokio::task::spawn_blocking(move || -> Result<SyncResponse, StatusCode> {
        let conn = db::open(&paths.db()).map_err(|err| {
            tracing::error!(?err, "sync: open library.db failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        // Plan the write-back from the sidecars as they arrived, before the
        // import folds them into the library — the plan compares the device's
        // file against what the library holds, and that comparison is the same
        // either way. Cloned because the import consumes them.
        let for_push = collected.clone();
        // The ink anchors live in the very sidecars that just arrived — read
        // them before the import consumes the collection.
        let notes = ink::handwritten_notes(&collected);
        let mut report = ingest::import_collected(&conn, collected, &device_serial, &db::now_iso())
            .map_err(|err| {
                tracing::error!(?err, "sync: import_collected failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        // Ink, through the same core function the USB pass calls — so a LAN sync
        // and a cable sync are one DB operation, not two implementations. A
        // failure here must not lose the annotations already committed above, so
        // it's logged and the report goes back with its ink counts at zero.
        if !inks.is_empty()
            && let Err(err) = ink::import_collected_ink(
                &conn,
                &paths,
                &device_serial,
                &db::now_iso(),
                &inks,
                &notes,
                &mut report,
                &|_, _, _| {},
            )
        {
            tracing::error!(?err, "sync: ink import failed");
        }
        let report = report;

        // The device writes these itself — it is the one holding the filesystem.
        // Same planner as USB, so both routes agree on what a sidecar should
        // contain and which book to leave alone.
        let write = match push::plan(&conn, &for_push, &|book| {
            ingest::book_index(book.kfx_path.as_deref())
        }) {
            Ok(plan) => {
                let mut out = Vec::with_capacity(plan.len());
                for item in plan {
                    // Checkpoint the bytes we are handing over, so the next sync
                    // can tell whether the device kept them.
                    let sha = import::sha256_of_bytes(&item.bytes);
                    if let Err(err) = db::set_yjr_sync_sha(
                        &conn,
                        &device_serial,
                        item.book_id,
                        &sha,
                        &db::now_iso(),
                    ) {
                        tracing::error!(?err, "sync: yjr checkpoint failed");
                    }
                    out.push(OutgoingSdr {
                        sdr_name: item.sdr_name,
                        file_name: item.file_name,
                        yjr_b64: encode_b64(&item.bytes),
                    });
                }
                out
            }
            Err(err) => {
                // A push that can't be planned must never fail the import that
                // already succeeded.
                tracing::error!(?err, "sync: annotation push planning failed");
                Vec::new()
            }
        };
        // Live-repaint signal — only when the import changed annotation state worth
        // repainting an open reader for. The GUI watches this file (see the
        // desktop app's `sync_pulse`) and re-emits the `annotations:sync-done`
        // event the USB path already fires; the daemon cannot emit a Tauri event
        // into the app directly.
        if import_changed_anything(&report) {
            write_sync_pulse(&paths, &device_serial, &report);
        }
        Ok(SyncResponse { report, write })
    })
    .await
    .map_err(|err| {
        tracing::error!(?err, "sync: import task panicked");
        StatusCode::INTERNAL_SERVER_ERROR
    })??;

    Ok(Json(response))
}

// ---------------------------------------------------------------------------
// Standalone notebooks — GET/POST /sync/notebooks
// ---------------------------------------------------------------------------

/// What `GET /sync/notebooks` answers: `{uuid: nbk_sha}` for every notebook the
/// library holds.
///
/// Unlike the ink manifest this takes no device — a notebook is one library
/// entity regardless of which Scribe wrote it, so two devices holding the same
/// uuid hold the same notebook.
#[derive(serde::Serialize)]
struct NotebookManifest {
    notebooks: HashMap<String, String>,
}

/// The push bundle: the standalone notebooks whose bytes differ from the
/// manifest (a device sends nothing when nothing changed).
#[derive(serde::Deserialize)]
struct NotebookSyncRequest {
    notebooks: Vec<SyncNotebook>,
}

#[derive(serde::Deserialize)]
struct SyncNotebook {
    /// The `.notebooks/<uuid>` dir name — the notebook's identity.
    uuid: String,
    /// The `nbk` (a KDF SQLite file), base64.
    nbk_b64: String,
    /// `.notebooks/thumbnails/<uuid>.png`, base64, when the device has one.
    /// Absent just means the viewer falls back to rendering page 0.
    #[serde(default)]
    cover_b64: Option<String>,
    /// The `nbk`'s on-device "Date Modified" (naive ISO) — the notebook's
    /// `updated_at`. Only the device knows it, so only the device can say.
    updated_at: String,
}

/// What `POST /sync/notebooks` stored, for the picker's toast.
#[derive(serde::Serialize, Default)]
struct NotebookSyncResult {
    imported: usize,
    unchanged: usize,
    /// Notebooks deleted in Sidle, which a re-push must not resurrect.
    suppressed: usize,
    /// `<uuid>: <error>` for each notebook that failed to decode or store. The
    /// rest of the bundle still lands — one corrupt `nbk` doesn't fail a sync.
    failed: Vec<String>,
}

/// `GET /sync/notebooks` — the notebook shas this library already holds.
async fn notebook_manifest(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<NotebookManifest>, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    let conn = open_db(&state.paths)?;
    let notebooks = db::notebook_shas(&conn).map_err(|err| {
        tracing::error!(?err, "sync/notebooks: manifest query failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(NotebookManifest {
        notebooks: notebooks.into_iter().collect(),
    }))
}

/// `POST /sync/notebooks` — back up the Scribe's standalone handwritten
/// notebooks, the LAN twin of the desktop's MTP pull.
///
/// Goes through the same core import that pull uses, so a notebook that arrives
/// over WiFi and one that arrives over a cable produce the same rows, the same
/// page-SVG cache, and the same deletion suppression.
async fn sync_notebooks(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(req): Json<NotebookSyncRequest>,
) -> Result<Json<NotebookSyncResult>, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    if req.notebooks.is_empty() {
        return Ok(Json(NotebookSyncResult::default()));
    }
    // Decode up front: a malformed blob is a client bug (400), kept distinct
    // from a decode/store failure below (which is per-notebook, not fatal).
    let mut pushed = Vec::with_capacity(req.notebooks.len());
    for nb in req.notebooks {
        let nbk = decode_b64(&nb.nbk_b64)?;
        let cover = decode_b64_opt(nb.cover_b64.as_deref())?;
        pushed.push((nb.uuid, nbk, cover, nb.updated_at));
    }

    let paths = state.paths.clone();
    // Decode + SVG render + SQLite writes are all blocking.
    let out = tokio::task::spawn_blocking(move || -> Result<NotebookSyncResult, StatusCode> {
        let conn = db::open(&paths.db()).map_err(|err| {
            tracing::error!(?err, "sync/notebooks: open db");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        let mut out = NotebookSyncResult::default();
        for (uuid, nbk, cover, updated_at) in &pushed {
            match notebook::import_notebook_bytes(
                &conn,
                &paths,
                uuid,
                nbk,
                cover.as_deref(),
                updated_at,
            ) {
                Ok(NotebookOutcome::Imported(_)) => out.imported += 1,
                Ok(NotebookOutcome::Unchanged(_)) => out.unchanged += 1,
                Ok(NotebookOutcome::Suppressed) => out.suppressed += 1,
                Err(err) => {
                    tracing::error!(?err, uuid, "sync/notebooks: import failed");
                    out.failed.push(format!("{uuid}: {err:#}"));
                }
            }
        }
        Ok(out)
    })
    .await
    .map_err(|err| {
        tracing::error!(?err, "sync/notebooks: import task panicked");
        StatusCode::INTERNAL_SERVER_ERROR
    })??;

    Ok(Json(out))
}

/// What `POST /sync/book` reports back, so the Kindle can toast imported-vs-
/// already-there. `id` is the library book id in both cases.
#[derive(serde::Serialize)]
struct BookSyncResult {
    /// `"imported"` (new) or `"duplicate"` (already in the library by content
    /// hash — a harmless re-push).
    outcome: &'static str,
    id: i64,
}

/// The filename extension [`sync_book`] stages an upload under, from what the
/// pusher put in `?ext=`.
///
/// Kept to lowercase ASCII letters, digits and `-`, and to the length of the
/// longest extension the library reads: a query value reaches no path outside
/// the temp dir and hides no second extension. Anything else, `None` included,
/// takes `kfx-zip`, the extension of every KFX push. An extension that survives
/// but names a format the library cannot read passes through, and `import_file`
/// answers with its own message.
fn staged_ext(requested: Option<&str>) -> String {
    requested
        .filter(|e| {
            !e.is_empty()
                && e.len() <= "kfx-zip".len()
                && e.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        .unwrap_or("kfx-zip")
        .to_string()
}

/// Ingest one decrypted book pushed over the LAN — the WiFi twin of the desktop's
/// USB `/dedrm` auto-pull. Token-gated, then the raw body is written to a temp
/// file and handed to the **exact** import the USB pull uses
/// ([`import::import_file`]): a WiFi import is byte-for-byte the same DB
/// operation, hash-deduped against a book the USB pull took.
///
/// `?ext=` names the extension to stage the bytes under, which `import_file`
/// dispatches on. The Kindle sends the decrypted file's own —
/// `kfx-zip` for a KFX, `azw3`/`azw4`/`mobi` for the MOBI family. See
/// [`staged_ext`] for what an absent or unusable value falls back to.
///
/// `Bytes` is last, a body-consuming extractor. [`write_book_pulse`] signals the
/// app; absent a listening app, `state.rs` re-enqueues the pending job on the
/// next launch.
async fn sync_book(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<BookSyncResult>, StatusCode> {
    check_token(&headers, &query, &state.token)?;

    let paths = state.paths.clone();
    let ext = staged_ext(query.get("ext").map(String::as_str));
    let result = tokio::task::spawn_blocking(move || -> Result<BookSyncResult, StatusCode> {
        // import_file dispatches on the extension, so stage the bytes under the
        // one the pusher named; the sha keeps concurrent uploads from colliding.
        let sha = import::sha256_of_bytes(&body);
        let tmp = std::env::temp_dir().join(format!("sidle-upload-{sha}.{ext}"));
        std::fs::write(&tmp, &body).map_err(|err| {
            tracing::error!(?err, "sync/book: write temp failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        let conn = db::open(&paths.db()).map_err(|err| {
            tracing::error!(?err, "sync/book: open library.db failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        let outcome = import::import_file(&conn, &paths, &tmp);
        let _ = std::fs::remove_file(&tmp);

        match outcome {
            Ok(ImportOutcome::Imported {
                book,
                needs_enqueue,
            }) => {
                // Signal the app to enqueue the conversion + refresh the shelf.
                write_book_pulse(&paths, book.id, needs_enqueue);
                Ok(BookSyncResult {
                    outcome: "imported",
                    id: book.id,
                })
            }
            Ok(ImportOutcome::Duplicate(book)) => Ok(BookSyncResult {
                outcome: "duplicate",
                id: book.id,
            }),
            Err(err) => {
                tracing::error!(?err, "sync/book: import failed");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    })
    .await
    .map_err(|err| {
        tracing::error!(?err, "sync/book: import task panicked");
        StatusCode::INTERNAL_SERVER_ERROR
    })??;

    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// GET /sync/misc (which folders to send), POST /sync/misc (the files)
// ---------------------------------------------------------------------------

/// Serve the library's sync-collection config, so the picker knows which device
/// folders to scan before it pushes. Token-gated like the rest. A config file
/// that won't parse is a 500 rather than a silent fallback to the defaults —
/// the picker falls back on its own when the fetch fails, and it should be a
/// visible failure that a hand-edited config sent it back to the defaults.
async fn misc_collections(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<SyncCollections>, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    SyncCollections::load(&state.paths)
        .map(Json)
        .map_err(|err| {
            tracing::error!(?err, "sync/misc: config unreadable");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

/// The push bundle the Kindle picker sends on Sync: each file base64 in JSON
/// (same no-multipart-dep reason as `/sync/annotations`), tagged with the
/// collection it was scanned for. The collection id is the only thing the
/// server takes from the client, and it takes it only as a lookup key: the
/// storage policy applied is this library's own, never one the client states.
#[derive(serde::Deserialize)]
struct MiscSyncRequest {
    /// The Kindle's serial — keys the `device-backup/<serial>/` subtree, same
    /// as annotations. Provisioned to the picker via `server.conf`.
    device_serial: String,
    files: Vec<MiscSyncFile>,
}

#[derive(serde::Deserialize)]
struct MiscSyncFile {
    /// Which collection this file was scanned for, as served by the GET above.
    collection: String,
    /// The file's path relative to its collection's device folder, e.g.
    /// `screenshot_1719430000.png` or `2026/draft.md`.
    path: String,
    /// File bytes, base64 (standard alphabet, with padding).
    data_b64: String,
}

/// What `POST /sync/misc` backed up, for the picker's toast: files stored per
/// collection id. Collections that stored nothing are omitted.
#[derive(serde::Serialize, Default)]
struct MiscSyncResult {
    stored: BTreeMap<String, usize>,
}

/// Store the Kindle's pushed files under `device-backup/<serial>/<collection>/`
/// — the WiFi backup the desktop Files tab views. Token-gated, then base64-decode
/// and hand each file to the shared [`device_backup::store_collection_file`]
/// policy (identical to the desktop's USB pull). No DB touch — these are files,
/// not library rows. A file naming a collection this library doesn't have is
/// ignored, as is one whose path doesn't survive sanitizing.
///
/// `Json` is last (a body-consuming extractor).
async fn sync_misc(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(req): Json<MiscSyncRequest>,
) -> Result<Json<MiscSyncResult>, StatusCode> {
    check_token(&headers, &query, &state.token)?;

    // Decode every blob up front so a malformed one is a clean 400 (client bug),
    // kept distinct from a 500 (our write failing) below.
    let mut decoded = Vec::with_capacity(req.files.len());
    for f in req.files {
        decoded.push((f.collection, f.path, decode_b64(&f.data_b64)?));
    }

    let paths = state.paths.clone();
    let serial = req.device_serial;

    // Filesystem writes are blocking; run them off the async executor.
    let result = tokio::task::spawn_blocking(move || -> Result<MiscSyncResult, StatusCode> {
        let config = SyncCollections::load(&paths).map_err(|err| {
            tracing::error!(?err, "sync/misc: config unreadable");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        let mut result = MiscSyncResult::default();
        for (id, path, bytes) in decoded {
            let Some(collection) = config.get(&id) else {
                tracing::warn!(id, "sync/misc: no such collection — ignoring");
                continue;
            };
            let wrote =
                device_backup::store_collection_file(&paths, &serial, collection, &path, &bytes)
                    .map_err(|err| {
                        tracing::error!(?err, path, "sync/misc: store failed");
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;
            if wrote {
                *result.stored.entry(id).or_default() += 1;
            }
        }
        Ok(result)
    })
    .await
    .map_err(|err| {
        tracing::error!(?err, "sync/misc: store task panicked");
        StatusCode::INTERNAL_SERVER_ERROR
    })??;

    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// Reading log — GET /sync/reading-log (watermark), POST /sync/reading-log
// ---------------------------------------------------------------------------

/// What the library already holds from one Kindle.
///
/// Two different skips, because the device has two kinds of source:
///
/// - `seen` names the log snapshots already read in full. A snapshot is
///   immutable, so its name is proof, and the device skips it without opening
///   it — the difference between a sync that gunzips 90 MB and one that lists a
///   directory.
/// - `watermark` is the newest event stored, as the `YYMMDD:HHMMSS` a syslog
///   line starts with. It filters the *live* log, which has no stable name and
///   is appended to continuously.
///
/// Both are empty for a device that has never synced, which correctly means
/// "read everything once".
#[derive(serde::Serialize, Default)]
struct ReadingWatermark {
    watermark: String,
    seen: Vec<String>,
}

/// The picker's push: the reading-event lines it found newer than the
/// watermark, verbatim.
///
/// Lines, not parsed sessions, on purpose. The session rules are subtle — a
/// running per-book counter, gap splitting, two different end-of-book constants
/// — and they live in exactly one implementation on this side. The device
/// selects; the library parses.
#[derive(serde::Deserialize)]
struct ReadingLogRequest {
    device_serial: String,
    lines: Vec<String>,
    /// The snapshots the device read to produce those lines, so this library can
    /// skip them next time exactly as a re-import does.
    #[serde(default)]
    dumps: Vec<String>,
}

/// What `POST /sync/reading-log` stored, for the picker's toast.
#[derive(serde::Serialize, Default)]
struct ReadingLogResult {
    sessions: usize,
    added: usize,
    /// Sittings already stored that these events carried further — the reader
    /// was in the middle of one when the last Sync happened.
    extended: usize,
    attributed: usize,
    /// How far this library now holds events for the pushing device, in the
    /// `YYMMDD:HHMMSS` a log line carries — the same value a subsequent `GET`
    /// would return.
    ///
    /// Returned so the device can drop its own archive of anything at or before
    /// it. That archive exists only to survive a long gap between syncs, so the
    /// moment the library confirms it holds the events, keeping them on a Kindle
    /// is waste. Confirmed, not assumed: the device deletes against what this
    /// says was stored, never against what it believes it sent.
    watermark: String,
}

/// `GET /sync/reading-log?serial=…` — how far this library has already read.
async fn reading_log_watermark(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<ReadingWatermark>, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    let serial = query.get("serial").cloned().unwrap_or_default();
    if serial.is_empty() {
        // Without a serial there is no per-device answer, and guessing one would
        // hand back another Kindle's progress.
        return Err(StatusCode::BAD_REQUEST);
    }
    let conn = open_db(&state.paths)?;
    let newest = db::reading_watermark(&conn, &serial).map_err(|err| {
        tracing::error!(?err, "sync/reading-log: watermark query failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let seen = db::seen_dumps(&conn, &serial).map_err(|err| {
        tracing::error!(?err, "sync/reading-log: seen-dumps query failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(ReadingWatermark {
        watermark: newest
            .as_deref()
            .and_then(reading_log::log_stamp)
            .unwrap_or_default(),
        seen: seen.into_iter().collect(),
    }))
}

/// `POST /sync/reading-log` — store the events the Kindle just found.
///
/// Idempotent by the same uniqueness index the desktop import relies on, so a
/// re-push of an overlapping window adds nothing.
async fn sync_reading_log(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    Json(req): Json<ReadingLogRequest>,
) -> Result<Json<ReadingLogResult>, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    if req.device_serial.is_empty() {
        // A session with no device would be stored as provenance-unknown and
        // then claimed by whichever Kindle syncs next. Refuse instead.
        return Err(StatusCode::BAD_REQUEST);
    }
    if req.lines.is_empty() && req.dumps.is_empty() {
        return Ok(Json(ReadingLogResult::default()));
    }

    let paths = state.paths.clone();
    // Parsing plus SQLite writes are blocking; keep them off the async executor.
    let out = tokio::task::spawn_blocking(move || -> Result<ReadingLogResult, StatusCode> {
        let conn = db::open(&paths.db()).map_err(|err| {
            tracing::error!(?err, "sync/reading-log: open db");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        // A `BTreeSet` both de-duplicates and orders, which is what the parser
        // requires — the device's own dumps overlap heavily by design.
        let events: std::collections::BTreeSet<String> = req.lines.into_iter().collect();
        // The sitting this Kindle was in when it last synced. These events are
        // only what it has logged since, so the run they continue is not among
        // them and has to come from the row it was stored as — otherwise a
        // sitting split across two Syncs is stored as two, and the reading
        // between them is credited to neither.
        let resume = reading_log::Resume::newest(&conn, &req.device_serial).map_err(|err| {
            tracing::error!(?err, "sync/reading-log: open-sitting query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        let stored = reading_log::store_events(
            &conn,
            &events,
            0,
            &req.device_serial,
            resume.as_ref(),
            &mut sidle_core::library::job::ignore,
        )
        .map_err(|err| {
            tracing::error!(?err, "sync/reading-log: store failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        // Recorded only after the events are stored, so a failure leaves the
        // snapshot to be read again rather than skipped forever.
        for name in &req.dumps {
            db::mark_dump_read(&conn, &req.device_serial, name).map_err(|err| {
                tracing::error!(?err, name, "sync/reading-log: mark dump read failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        }
        // What the device is cleared to drop, which is exactly what this
        // library now holds durably — the raw lines, not the sessions made of
        // them.
        //
        // The sessions cannot answer it. Their watermark is per device, so a
        // sitting stored for one book carries it past another book's events;
        // the device then purges the only copy of lines nothing here kept, and
        // a book the parser could not measure this week can never be measured.
        // Archiving the lines first is what makes the newest of them a safe
        // thing to say, and an archive that failed to write says nothing.
        let held = match reading_log::archive_pushed(&paths.root, &req.device_serial, &events) {
            Ok(()) => events
                .iter()
                .filter_map(|l| reading_log::log_line_stamp(l))
                .max()
                .unwrap_or_default(),
            Err(err) => {
                tracing::error!(?err, "sync/reading-log: archiving the pushed lines failed");
                String::new()
            }
        };
        // Rows the Reading Log page draws from. A push that stored nothing
        // writes none.
        if stored.added > 0 || stored.extended > 0 || stored.attributed > 0 {
            write_reading_pulse(&paths, &req.device_serial, &stored);
        }
        Ok(ReadingLogResult {
            sessions: stored.sessions,
            added: stored.added,
            extended: stored.extended,
            attributed: stored.attributed,
            watermark: held,
        })
    })
    .await
    .map_err(|err| {
        tracing::error!(?err, "sync/reading-log: store task panicked");
        StatusCode::INTERNAL_SERVER_ERROR
    })??;

    Ok(Json(out))
}

/// Base64-encode a blob for the wire (standard alphabet, with padding) — the
/// same shape the device sends its sidecars in.
fn encode_b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Base64-decode a required blob (standard alphabet, with padding); a decode
/// error maps to `400 Bad Request`.
fn decode_b64(s: &str) -> Result<Vec<u8>, StatusCode> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|_| StatusCode::BAD_REQUEST)
}

/// Base64-decode an optional blob (standard alphabet, with padding); a decode
/// error maps to `400 Bad Request`.
fn decode_b64_opt(s: Option<&str>) -> Result<Option<Vec<u8>>, StatusCode> {
    s.map(decode_b64).transpose()
}

/// Did the import change anything an open reader would render? New or removed
/// annotations, a moved last-read position, or fresh ink pages warrant a repaint
/// pulse; a re-sync of unchanged `.yjr` (pure duplicates) does not.
/// (`unresolved` rows are a subset of `inserted`, so checking `inserted` already
/// covers them; `ink_unchanged` is the ink equivalent of a duplicate and is
/// deliberately not counted.)
fn import_changed_anything(r: &DeviceImportReport) -> bool {
    r.annotations.inserted > 0 || r.positions > 0 || r.relinked > 0 || r.ink_pages > 0
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

/// Atomically write `<root>/.reading-pulse.json` — the reading-log twin of
/// [`write_sync_pulse`]. `POST /sync/reading-log` lands sessions the desktop's
/// Reading Log page holds in memory, and this is what tells it to refetch.
fn write_reading_pulse(paths: &LibraryPaths, device_serial: &str, stored: &reading_log::Imported) {
    let pulse = serde_json::json!({
        "ts": db::now_iso(),
        "device_serial": device_serial,
        "added": stored.added,
        "extended": stored.extended,
        "attributed": stored.attributed,
    });
    let Ok(bytes) = serde_json::to_vec(&pulse) else {
        return;
    };
    let final_path = paths.root.join(".reading-pulse.json");
    let tmp_path = paths.root.join(".reading-pulse.json.tmp");
    if let Err(err) = std::fs::write(&tmp_path, &bytes) {
        tracing::warn!(?err, "reading pulse: write tmp failed");
        return;
    }
    if let Err(err) = std::fs::rename(&tmp_path, &final_path) {
        tracing::warn!(?err, "reading pulse: rename failed");
        let _ = std::fs::remove_file(&tmp_path);
    }
}

/// Atomically write `<root>/.book-pulse.json` — the book twin of
/// [`write_sync_pulse`]. The detached server can't emit a Tauri event, so the
/// app's `sync_pulse` watcher reads this to enqueue the pending `kfx_to_epub`
/// conversion and refresh the shelf. Only a **new** import writes one (a
/// duplicate re-push changes nothing), so a manual re-sync of already-synced
/// books stays quiet on the desktop. Best-effort — the DB write already
/// succeeded, so a failed pulse just defers the conversion to the next launch.
fn write_book_pulse(paths: &LibraryPaths, book_id: i64, needs_enqueue: bool) {
    let pulse = serde_json::json!({
        "ts": db::now_iso(),
        "books": [ { "id": book_id, "needs_enqueue": needs_enqueue } ],
    });
    let Ok(bytes) = serde_json::to_vec(&pulse) else {
        return;
    };
    let final_path = paths.root.join(".book-pulse.json");
    let tmp_path = paths.root.join(".book-pulse.json.tmp");
    if let Err(err) = std::fs::write(&tmp_path, &bytes) {
        tracing::warn!(?err, "book pulse: write tmp failed");
        return;
    }
    if let Err(err) = std::fs::rename(&tmp_path, &final_path) {
        tracing::warn!(?err, "book pulse: rename failed");
        let _ = std::fs::remove_file(&tmp_path);
    }
}

/// `GET /device/manifest.json` → the offered fleet with every entry re-read
/// through `device-dist/sources.json` (token-gated). 404 when nothing has been
/// described; the picker reads that as "no update".
async fn get_dist_manifest(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    let dist_dir = state.paths.device_dist();
    let digest_path = state.paths.source_digests();
    let manifest = tokio::task::spawn_blocking(move || {
        let manifest = dist::read_manifest(&dist_dir)?;
        let sources = dist::read_sources(&dist_dir)?;
        let mut digests = DigestCache::open(&digest_path);
        let fresh = dist::restamp(&manifest, &sources, &mut digests);
        let _ = digests.save();
        Some(fresh)
    })
    .await
    .ok()
    .flatten()
    .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(manifest).into_response())
}

/// `GET /device/file/{*name}` → the bytes `device-dist/sources.json` maps
/// `name` to (token-gated). `name` is a key in that index and never a path
/// fragment: `../library.db` finds no entry and 404s before anything opens.
async fn get_dist_file(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    let sources = dist::read_sources(&state.paths.device_dist()).ok_or(StatusCode::NOT_FOUND)?;
    let source = sources.source_of(&name).ok_or(StatusCode::NOT_FOUND)?;
    serve_file(source.to_path_buf(), "application/octet-stream", None).await
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
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap(),
    );
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
        AppState, Config, Router, RustlsConfig, build_router, follow_lan_address, get, health,
        import_changed_anything, load_or_generate_token, serve_with_shutdown, staged_ext,
    };
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use sidle_core::library::LibraryPaths;
    use sidle_core::library::db::{self, NewBook};
    use sidle_core::library::device_backup::{self, SyncCollections};
    use sidle_core::library::ingest::{self, CollectedYjr, DeviceImportReport};
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

    /// A client built exactly the way the Kindle picker will be: the pure-Rust
    /// RustCrypto provider, and our own CA as the *sole* trust root — no public
    /// roots at all, so a certificate from anywhere else cannot satisfy it.
    ///
    /// Sharing the device's stack is the point. A test that used the host's
    /// reqwest/ring client would prove the server serves TLS to *something*,
    /// which is not the question; the question is whether the leaf we issue is
    /// accepted by the constrained client that has to accept it.
    fn device_shaped_agent(ca_pem: &str) -> ureq::Agent {
        use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};
        let ca = Certificate::from_pem(ca_pem.as_bytes()).expect("parse CA pem");
        ureq::Agent::config_builder()
            .tls_config(
                TlsConfig::builder()
                    .provider(TlsProvider::Rustls)
                    .unversioned_rustls_crypto_provider(Arc::new(rustls_rustcrypto::provider()))
                    .root_certs(RootCerts::Specific(Arc::new(vec![ca])))
                    .build(),
            )
            .build()
            .new_agent()
    }

    /// [`follow_lan_address`] over a real handshake: `axum_server` starts on a
    /// leaf covering `127.0.0.1` alone, a pinned client dialling `localhost`
    /// is refused, and the address change re-issues a leaf the same client and
    /// the same pinned root accept.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_moved_machine_re_issues_and_hot_swaps_its_leaf() {
        use std::net::Ipv4Addr;
        use std::sync::atomic::{AtomicBool, Ordering};

        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        // No `localhost` SAN — the name `dial` uses below.
        sidle_core::library::tls::issue_server_cert(&paths, &["127.0.0.1".to_string()]).unwrap();
        let ca_pem = sidle_core::library::tls::ca_cert_pem(&paths).unwrap();

        let _ = rustls::crypto::ring::default_provider().install_default();
        let tls = RustlsConfig::from_pem_file(paths.server_cert(), paths.server_key())
            .await
            .unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/", get(health));
        let serving = tokio::spawn(
            axum_server::from_tcp_rustls(listener, tls.clone())
                .unwrap()
                .serve(app.into_make_service()),
        );

        let dial = {
            let ca_pem = ca_pem.clone();
            move || {
                let ca_pem = ca_pem.clone();
                tokio::task::spawn_blocking(move || {
                    device_shaped_agent(&ca_pem)
                        .get(&format!("https://localhost:{port}/"))
                        .call()
                        .map(|r| r.status().as_u16())
                        .map_err(|e| e.to_string())
                })
            }
        };

        // `127.0.0.1` is the SAN the loaded leaf carries.
        let ready_pem = ca_pem.clone();
        tokio::task::spawn_blocking(move || {
            let agent = device_shaped_agent(&ready_pem);
            let url = format!("https://127.0.0.1:{port}/");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                if agent.get(&url).call().is_ok() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            panic!("server never accepted on 127.0.0.1:{port}");
        })
        .await
        .unwrap();

        assert!(
            dial().await.unwrap().is_err(),
            "a leaf without a `localhost` SAN must not satisfy a client dialling `localhost`"
        );

        // `address` yields 192.0.2.1 once, then 192.0.2.2 for every call after.
        let moved = AtomicBool::new(false);
        let watch = follow_lan_address(
            paths.clone(),
            tls,
            move || {
                Some(if moved.swap(true, Ordering::Relaxed) {
                    Ipv4Addr::new(192, 0, 2, 2)
                } else {
                    Ipv4Addr::new(192, 0, 2, 1)
                })
            },
            std::time::Duration::from_millis(10),
        );

        let served = tokio::select! {
            () = watch => unreachable!("the watch runs until dropped"),
            served = async {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    if let Ok(code) = dial().await.unwrap() {
                        return Ok(code);
                    }
                    if std::time::Instant::now() > deadline {
                        return Err("leaf never re-issued to cover `localhost`");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            } => served,
        };
        assert_eq!(
            served,
            Ok(200),
            "the same pinned root must accept the re-issued leaf"
        );

        serving.abort();
    }

    /// Graceful shutdown, over a real TLS handshake: the daemon serves `GET /`
    /// (200) to a client that trusts only our CA, then when the shutdown future
    /// resolves `serve_with_shutdown` returns `Ok(())` and frees the port.
    ///
    /// This is simultaneously the proof that the whole certificate path works —
    /// `library::tls` issues a leaf, the server presents it, and the device's
    /// exact client stack validates it against the pinned root, including the
    /// `127.0.0.1` IP SAN. Nothing about that is verifiable from the cert bytes
    /// alone, which is why it lives here rather than in sidle-core.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serves_tls_to_a_pinned_client_then_shuts_down_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        // The server refuses to start without material, so issue it the same way
        // the desktop app will.
        sidle_core::library::tls::issue_server_cert(&paths, &["127.0.0.1".to_string()]).unwrap();
        let ca_pem = sidle_core::library::tls::ca_cert_pem(&paths).unwrap();

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

        // ureq is blocking, so drive it off the runtime. Retries until the
        // spawned task has bound and is accepting.
        let served = tokio::task::spawn_blocking(move || {
            use std::time::{Duration, Instant};
            let agent = device_shaped_agent(&ca_pem);
            let url = format!("https://127.0.0.1:{port}/");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match agent.get(&url).call() {
                    Ok(res) => return Ok(res.status().as_u16()),
                    Err(e) if Instant::now() > deadline => return Err(e.to_string()),
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            served,
            Ok(200),
            "pinned client could not complete a TLS request against the issued leaf"
        );

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
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
            {
                refused = true;
                break;
            }
        }
        assert!(refused, "port {port} never freed after shutdown");
    }

    // --- POST /sync/annotations (the LAN==USB gate) ------------------------

    /// A synthetic bookmark `.yjr`: `[marker][len:3 BE][payload]` tokens — the
    /// same recipe `sidle-core`'s ingest tests use. One bookmark = the marker
    /// key followed by a single anchor-handle string value.
    /// A `.yjr` holding one bookmark, encoded through the format's own codec so
    /// this fixture is bytes a real Kindle would produce.
    fn yjr_bookmark(eid: i64, off: i64, position: i64) -> Vec<u8> {
        use sidle_core::library::yjr::{Anchor, Annotation, Kind, Store};
        let mut store = Store::empty();
        store.merge_annotations(&[Annotation {
            kind: Kind::Bookmark,
            anchors: vec![Anchor::new(eid, off, position)],
            body: None,
            color: None,
            created_ms: Some(0),
            modified_ms: Some(0),
        }]);
        store.encode()
    }

    /// A fresh library root + one book whose `kfx_sha256` the `book.deadbeef.sdr`
    /// infix prefix-matches (so the bookmark resolves). Returns `(paths, book_id)`.
    fn library_with_matching_book(root: &Path) -> (LibraryPaths, i64) {
        let paths = LibraryPaths {
            root: root.to_path_buf(),
        };
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
                amazon_asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
                title_romaji: "shiori no aru hon",
                author_romaji: "Author",
                source_format: None,
            },
        )
        .unwrap();
        (paths, book_id)
    }

    /// A book arriving on a device for the first time has no `.sdr`, so it is in
    /// no sync bundle and the push that answers a bundle can never reach it.
    /// This route is how it gets its highlights anyway.
    #[tokio::test]
    async fn sidecar_route_composes_for_a_device_holding_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, book_id) = library_with_matching_book(tmp.path());
        let token = load_or_generate_token(&paths.root).unwrap();
        let state = AppState {
            paths: paths.clone(),
            token: Arc::from(token.as_str()),
        };

        let get = |id: i64| {
            Request::builder()
                .uri(format!("/sidecar/{id}?serial=DEV"))
                .header("X-Sidle-Token", token.clone())
                .body(Body::empty())
                .unwrap()
        };

        // Nothing in the library for it yet: not a failure, just nothing to write.
        let resp = build_router(state.clone())
            .oneshot(get(book_id))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // A book that isn't there is not the same as one with nothing to say.
        let resp = build_router(state.clone())
            .oneshot(get(book_id + 999))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Same gate as every other route — a sidecar carries the reader's own
        // highlights and is not served to an unauthenticated caller.
        let resp = build_router(state)
            .oneshot(
                Request::builder()
                    .uri(format!("/sidecar/{book_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
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
        b.body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    /// A `POST /sync/book` request with raw body bytes and an optional token,
    /// naming no `?ext=`: the server stages under `staged_ext`'s default.
    fn book_request(token: Option<&str>, body: Vec<u8>) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri("/sync/book")
            .header("content-type", "application/octet-stream");
        if let Some(t) = token {
            b = b.header("x-sidle-token", t);
        }
        b.body(Body::from(body)).unwrap()
    }

    /// A `POST /sync/misc` request with a JSON body and an optional token.
    fn misc_request(token: Option<&str>, body: serde_json::Value) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri("/sync/misc")
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("x-sidle-token", t);
        }
        b.body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    /// The decisive LAN==USB gate: a `.yjr` pushed through `POST /sync/annotations`
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
                yjr_name: None,
                yjf_name: None,
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
        let mut report_http: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        // LAN == USB: identical report ...
        //
        // Minus `write`, which has no USB counterpart: over LAN the device does
        // its own writing, so the sidecars to push travel in the response, while
        // the USB path writes them itself and reports only counts. What is being
        // compared here is the *import* result, which must not differ by route.
        let write = report_http
            .as_object_mut()
            .expect("a JSON object")
            .remove("write")
            .expect("the response carries a write list");
        assert_eq!(write, serde_json::json!([]), "nothing to push back here");
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
        assert!(
            pulse.exists(),
            "sync pulse not written after a changing import"
        );
        let pulse_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&pulse).unwrap()).unwrap();
        assert_eq!(pulse_json["device_serial"], device_serial);
    }

    /// `POST /sync/misc` applies each collection's own policy — screenshots
    /// copy-if-absent, logs overwrite — under `device-backup/<serial>/`, ignores
    /// a collection this library doesn't have, and reports the counts. The WiFi
    /// twin of the desktop USB pull.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_misc_stores_per_collection_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        let token = load_or_generate_token(&paths.root).unwrap();
        let serial = "G000TESTSERIAL";
        let state = AppState {
            paths: paths.clone(),
            token: Arc::from(token.as_str()),
        };

        let body = serde_json::json!({
            "device_serial": serial,
            "files": [
                { "collection": "screenshots", "path": "screenshot_100.png",
                  "data_b64": BASE64.encode(b"PNG-A") },
                { "collection": "logs", "path": "sidle-native.log",
                  "data_b64": BASE64.encode(b"log v1\n") },
                { "collection": "nosuch", "path": "version.txt",
                  "data_b64": BASE64.encode(b"5.16") },
            ],
        });
        let resp = build_router(state.clone())
            .oneshot(misc_request(Some(&token), body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let report: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(report["stored"]["screenshots"], 1);
        assert_eq!(report["stored"]["logs"], 1);
        assert!(
            report["stored"].get("nosuch").is_none(),
            "a collection this library doesn't have stores nothing"
        );

        assert_eq!(
            std::fs::read(
                paths
                    .device_backup_collection(serial, "screenshots")
                    .join("screenshot_100.png")
            )
            .unwrap(),
            b"PNG-A"
        );
        assert!(!paths.device_backup_collection(serial, "nosuch").exists());

        // Re-push: the screenshot is copy-if-absent (unchanged, count 0); the log
        // overwrites with its grown content (count 1).
        let body2 = serde_json::json!({
            "device_serial": serial,
            "files": [
                { "collection": "screenshots", "path": "screenshot_100.png",
                  "data_b64": BASE64.encode(b"IGNORED") },
                { "collection": "logs", "path": "sidle-native.log",
                  "data_b64": BASE64.encode(b"log v1\nv2\n") },
            ],
        });
        let resp = build_router(state)
            .oneshot(misc_request(Some(&token), body2))
            .await
            .unwrap();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let report: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            report["stored"].get("screenshots").is_none(),
            "already held → not re-written"
        );
        assert_eq!(report["stored"]["logs"], 1, "log overwritten");
        assert_eq!(
            std::fs::read(
                paths
                    .device_backup_collection(serial, "screenshots")
                    .join("screenshot_100.png")
            )
            .unwrap(),
            b"PNG-A",
            "immutable screenshot untouched by re-push"
        );
        assert_eq!(
            std::fs::read(
                paths
                    .device_backup_collection(serial, "logs")
                    .join("sidle-native.log")
            )
            .unwrap(),
            b"log v1\nv2\n"
        );
    }

    /// A file pushed into a collection this library added lands under that
    /// collection's own dir, subfolders intact — the whole point of the config
    /// being the library's rather than the picker's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_misc_serves_the_config_it_applies() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        let mut config = SyncCollections::defaults();
        config.collections.push(device_backup::SyncCollection {
            id: "drafts".into(),
            label: "Drafts".into(),
            dirs: vec!["writing".into()],
            include: vec!["*.md".into()],
            recursive: true,
            update: device_backup::UpdatePolicy::Always,
            clear_device: false,
            purge: Vec::new(),
        });
        config.save(&paths).unwrap();

        let token = load_or_generate_token(&paths.root).unwrap();
        let state = AppState {
            paths: paths.clone(),
            token: Arc::from(token.as_str()),
        };

        // The picker asks first…
        let req = Request::builder()
            .method("GET")
            .uri("/sync/misc")
            .header("x-sidle-token", &token)
            .body(Body::empty())
            .unwrap();
        let resp = build_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let served: SyncCollections = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(served, config);

        // …then sends what that config asked for.
        let body = serde_json::json!({
            "device_serial": "S",
            "files": [
                { "collection": "drafts", "path": "2026/draft.md",
                  "data_b64": BASE64.encode(b"# hi") },
            ],
        });
        let resp = build_router(state)
            .oneshot(misc_request(Some(&token), body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            std::fs::read_to_string(
                paths
                    .device_backup_collection("S", "drafts")
                    .join("2026/draft.md")
            )
            .unwrap(),
            "# hi"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn misc_collections_rejects_missing_token() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        let token = load_or_generate_token(&paths.root).unwrap();
        let state = AppState {
            paths,
            token: Arc::from(token.as_str()),
        };
        let req = Request::builder()
            .method("GET")
            .uri("/sync/misc")
            .body(Body::empty())
            .unwrap();
        let resp = build_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_misc_rejects_missing_token() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        let token = load_or_generate_token(&paths.root).unwrap();
        let state = AppState {
            paths,
            token: Arc::from(token.as_str()),
        };
        let body = serde_json::json!({ "device_serial": "S", "files": [] });
        let resp = build_router(state)
            .oneshot(misc_request(None, body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_annotations_rejects_missing_token() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, _) = library_with_matching_book(tmp.path());
        let state = AppState {
            paths,
            token: Arc::from("the-real-token"),
        };
        let body = serde_json::json!({ "device_serial": "X", "sdrs": [] });
        let resp = build_router(state)
            .oneshot(sync_request(None, body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_book_rejects_missing_token() {
        // No token → 403 before the body is ever imported (check_token is first).
        let tmp = tempfile::tempdir().unwrap();
        let (paths, _) = library_with_matching_book(tmp.path());
        let state = AppState {
            paths,
            token: Arc::from("the-real-token"),
        };
        let resp = build_router(state)
            .oneshot(book_request(None, b"whatever".to_vec()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_book_with_token_reaches_importer() {
        // A valid token but a body that isn't a real `.kfx-zip` passes the gate
        // and fails inside `import_file` (500) — proving the endpoint stages the
        // bytes and calls the importer, not that it rejects early.
        let tmp = tempfile::tempdir().unwrap();
        let (paths, _) = library_with_matching_book(tmp.path());
        let token = load_or_generate_token(&paths.root).unwrap();
        let state = AppState {
            paths,
            token: Arc::from(token.as_str()),
        };
        let resp = build_router(state)
            .oneshot(book_request(Some(&token), b"not a real kfx-zip".to_vec()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn a_pushed_extension_decides_the_staged_name() {
        // What the picker sends for each family the engine decrypts.
        assert_eq!(staged_ext(Some("kfx-zip")), "kfx-zip");
        for ext in ["azw3", "azw4", "mobi"] {
            assert_eq!(staged_ext(Some(ext)), ext, "{ext}");
        }
        // A picker that names no extension is pushing a KFX.
        assert_eq!(staged_ext(None), "kfx-zip");
        assert_eq!(staged_ext(Some("")), "kfx-zip");
        // Nothing that could leave the temp dir or hide a second extension
        // survives.
        for bad in [
            "../../etc/passwd",
            "kfx-zip/../x",
            "tar.gz",
            "KFX-ZIP",
            "a b",
            "toolongext",
        ] {
            assert_eq!(staged_ext(Some(bad)), "kfx-zip", "{bad}");
        }
        // An extension the library cannot read passes through, and `import_file`
        // names it in the error.
        assert_eq!(staged_ext(Some("djvu")), "djvu");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_annotations_rejects_bad_base64() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, _) = library_with_matching_book(tmp.path());
        let token = load_or_generate_token(&paths.root).unwrap();
        let state = AppState {
            paths,
            token: Arc::from(token.as_str()),
        };
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
        let state = AppState {
            paths,
            token: Arc::from(token.as_str()),
        };
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

        // Ink is rendered by the reader too, so a sync that brought only
        // handwriting still has to repaint an open book.
        let mut ink_only = DeviceImportReport {
            ink_books: 1,
            ink_pages: 2,
            ..Default::default()
        };
        assert!(import_changed_anything(&ink_only), "new ink pages → pulse");
        // A re-sync whose nbk hadn't changed is the ink twin of a duplicate.
        ink_only.ink_pages = 0;
        ink_only.ink_books = 0;
        ink_only.ink_unchanged = 1;
        assert!(!import_changed_anything(&ink_only), "unchanged ink → quiet");
    }

    // --- GET /device/... (LAN self-update pull) ----------------------------

    /// A `GET` request with an optional `x-sidle-token` header.
    fn get_request(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(uri);
        if let Some(t) = token {
            b = b.header("x-sidle-token", t);
        }
        b.body(Body::empty()).unwrap()
    }

    /// Write a `device-dist/` pair under `paths`, mirroring what the desktop
    /// app's `dist::refresh` writes: a manifest, and an index pointing each
    /// served path at a file outside `device-dist/`. Returns the picker's bytes.
    fn stage_fake_dist(paths: &LibraryPaths) -> Vec<u8> {
        let dist = paths.device_dist();
        let bytes = b"\x7fELF-fake-armv7-picker".to_vec();
        // Where each app's own build left its files. Nothing copies them.
        let checkouts = paths.root.join("checkouts");
        let write = |abs: &std::path::Path, body: &[u8]| {
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(abs, body).unwrap();
        };
        let picker = checkouts.join("picker/bin/sidle");
        let tile = checkouts.join("sprocket/out/Sprocket.sh");
        write(&picker, &bytes);
        write(&tile, TILE);

        // Digests that miss the files: `dist::restamp` reads the sources.
        let manifest = serde_json::json!({
            "version": 2,
            "files": [ { "name": "bin/sidle", "sha256": "deadbeef",
                         "size": bytes.len(), "built_at": 0 } ],
            "apps": [
                { "id": "sidle", "name": "Sidle", "built_at": 0, "files": [
                    { "path": "extensions/sidle/bin/sidle", "sha256": "deadbeef",
                      "size": bytes.len(), "apply": "staged" } ] },
                { "id": "sprocket", "name": "Sprocket", "built_at": 0, "files": [
                    { "path": "documents/Sprocket.sh", "sha256": "cafe",
                      "size": TILE.len(), "apply": "direct" } ] }
            ]
        });
        let entry = |p: &std::path::Path| serde_json::json!({ "source": p });
        let sources = serde_json::json!({
            "files": {
                "bin/sidle": entry(&picker),
                "extensions/sidle/bin/sidle": entry(&picker),
                "documents/Sprocket.sh": entry(&tile),
            }
        });
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(
            dist.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dist.join("sources.json"),
            serde_json::to_vec(&sources).unwrap(),
        )
        .unwrap();
        bytes
    }

    /// A tile, small enough to assert on whole.
    const TILE: &[u8] = b"# Name: Sprocket\n";

    fn staged_state(root: &Path) -> (AppState, String, Vec<u8>) {
        let paths = LibraryPaths {
            root: root.to_path_buf(),
        };
        paths.ensure().unwrap();
        let token = load_or_generate_token(&paths.root).unwrap();
        let bytes = stage_fake_dist(&paths);
        let state = AppState {
            paths,
            token: Arc::from(token.as_str()),
        };
        (state, token, bytes)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dist_manifest_and_file_serve_staged_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token, bin_bytes) = staged_state(tmp.path());

        let resp = build_router(state.clone())
            .oneshot(get_request("/device/manifest.json", Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(manifest["version"], 2);
        assert_eq!(manifest["files"][0]["name"], "bin/sidle");

        // The promised digest is the one the served bytes hash to.
        let promised = manifest["files"][0]["sha256"].as_str().unwrap();
        assert_eq!(
            promised,
            sidle_core::library::device::deploy::sha256_bytes(&bin_bytes)
        );
        assert_eq!(
            manifest["apps"][1]["files"][0]["sha256"].as_str().unwrap(),
            sidle_core::library::device::deploy::sha256_bytes(TILE),
            "the tile's promise is read off the tile"
        );

        // The catch-all `{*name}` captures the `bin/sidle` key, and the bytes
        // come back byte-identical to the file the index points at.
        let resp = build_router(state)
            .oneshot(get_request("/device/file/bin/sidle", Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), bin_bytes.as_slice(), "served == the source");
        assert_eq!(
            sidle_core::library::device::deploy::sha256_bytes(&body),
            promised,
            "what was promised is what was handed over"
        );
    }

    /// A source rewritten after `stage_fake_dist`: the manifest follows it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_source_rebuilt_after_staging_is_offered_as_it_stands() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token, _) = staged_state(tmp.path());
        let tile = state.paths.root.join("checkouts/sprocket/out/Sprocket.sh");
        const REBUILT: &[u8] = b"# Name: Sprocket\n# Icon: new\n";
        std::fs::write(&tile, REBUILT).unwrap();

        let resp = build_router(state)
            .oneshot(get_request("/device/manifest.json", Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let entry = &manifest["apps"][1]["files"][0];
        assert_eq!(
            entry["sha256"].as_str().unwrap(),
            sidle_core::library::device::deploy::sha256_bytes(REBUILT)
        );
        assert_eq!(entry["size"], REBUILT.len());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dist_file_serves_an_app_from_its_own_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token, _) = staged_state(tmp.path());

        let resp = build_router(state)
            .oneshot(get_request(
                "/device/file/documents/Sprocket.sh",
                Some(&token),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a mount path outside the picker's own bundle"
        );
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), TILE);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dist_file_404s_names_absent_from_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token, _) = staged_state(tmp.path());
        // Names `sources.json` holds no entry for. The lookup is what keeps the
        // token and every other file unreachable, traversal names included.
        for uri in [
            "/device/file/extensions/sidle/etc/server.conf",
            "/device/file/sources.json",
            "/device/file/extensions/sidle/bin/sidle.sh",
        ] {
            let resp = build_router(state.clone())
                .oneshot(get_request(uri, Some(&token)))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "unlisted {uri} must 404"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dist_endpoints_reject_missing_token() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _, _) = staged_state(tmp.path());
        for uri in ["/device/manifest.json", "/device/file/bin/sidle"] {
            let resp = build_router(state.clone())
                .oneshot(get_request(uri, None))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "{uri} must require a token"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dist_manifest_404_when_nothing_staged() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        let token = load_or_generate_token(&paths.root).unwrap();
        let state = AppState {
            paths,
            token: Arc::from(token.as_str()),
        };
        let resp = build_router(state)
            .oneshot(get_request("/device/manifest.json", Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // --- /sync/reading-log (the device's own push) -------------------------

    /// One reading event in the device's syslog shape. Same fixture the parser's
    /// own tests use; here it only has to survive the wire and reach the parser.
    fn reading_line(stamp: &str, kind: &str, total_ms: i64, words: i64) -> String {
        format!(
            "{stamp} cvm[6144]: I ReadingTimerController:Information::{kind},\
             Title:<private>,Asin:<private>,IntervalTime:900,\
             TotalTime:{total_ms},TotalWords:{words},Total%:0.5,\
             CurrentPos:YJPosition: AAA:12,EndPos:YJPosition: BBB:148207,\
             NextTOCEntryPosition:YJPosition: CCC:99,\
             CurrentPos:YJPosition: AAA:12,EndPos:YJPosition: DDD:6612;"
        )
    }

    /// A `POST /sync/reading-log` request with a JSON body and an optional token.
    fn reading_log_request(token: Option<&str>, body: serde_json::Value) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri("/sync/reading-log")
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("x-sidle-token", t);
        }
        b.body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// A library + a token, no books. Reading events store on their own.
    fn bare_state(root: &Path) -> (AppState, String) {
        let paths = LibraryPaths {
            root: root.to_path_buf(),
        };
        paths.ensure().unwrap();
        let token = load_or_generate_token(&paths.root).unwrap();
        let state = AppState {
            paths,
            token: Arc::from(token.as_str()),
        };
        (state, token)
    }

    /// The whole point of the pair of routes: what the device is told to fetch
    /// shrinks after it pushes, and pushing the same window twice stores nothing
    /// the second time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_push_moves_the_watermark_and_a_repush_adds_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token) = bare_state(tmp.path());
        let serial = "G000TESTSERIAL";
        let watermark_uri = format!("/sync/reading-log?serial={serial}");

        // A Kindle that has never synced is told to read everything: no
        // watermark to filter the live log by, no snapshot it may skip.
        let resp = build_router(state.clone())
            .oneshot(get_request(&watermark_uri, Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let before = json_body(resp).await;
        assert_eq!(before["watermark"], "");
        assert!(before["seen"].as_array().unwrap().is_empty());

        let dump = "log_backup_260803101500.txt.gz";
        let body = serde_json::json!({
            "device_serial": serial,
            "lines": [
                reading_line("260803:100000", "NextPage", 60_000, 100),
                reading_line("260803:100500", "NextPage", 120_000, 220),
                reading_line("260803:101000", "CloseBook", 180_000, 300),
            ],
            "dumps": [dump],
        });
        let resp = build_router(state.clone())
            .oneshot(reading_log_request(Some(&token), body.clone()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let report = json_body(resp).await;
        assert_eq!(report["sessions"], 1);
        assert_eq!(report["added"], 1);
        assert_eq!(
            report["attributed"], 0,
            "no book in this library carries that end position"
        );
        // The device deletes its own archive against this, so it must state what
        // was stored rather than what arrived.
        assert_eq!(report["watermark"], "260803:101000");

        // Now the device is told to skip the snapshot outright and to filter the
        // live log to what came after the last event stored.
        let resp = build_router(state.clone())
            .oneshot(get_request(&watermark_uri, Some(&token)))
            .await
            .unwrap();
        let after = json_body(resp).await;
        assert_eq!(
            after["watermark"], "260803:101000",
            "the newest event, in the form a syslog line starts with"
        );
        assert_eq!(after["seen"], serde_json::json!([dump]));

        // A device that pushes an overlapping window anyway — a sync that was
        // interrupted after storing but before the toast, say — stores nothing
        // twice.
        let resp = build_router(state.clone())
            .oneshot(reading_log_request(Some(&token), body))
            .await
            .unwrap();
        let again = json_body(resp).await;
        assert_eq!(again["sessions"], 1, "the same session was parsed again");
        assert_eq!(again["added"], 0, "and recognised as already held");

        // One Kindle's progress is never handed to another.
        let resp = build_router(state)
            .oneshot(get_request(
                "/sync/reading-log?serial=G000OTHERDEVICE",
                Some(&token),
            ))
            .await
            .unwrap();
        let other = json_body(resp).await;
        assert_eq!(other["watermark"], "");
        assert!(other["seen"].as_array().unwrap().is_empty());
    }

    /// Nothing about a device's reading is reachable without the token, in
    /// either direction.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reading_log_endpoints_reject_missing_token() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _) = bare_state(tmp.path());

        let resp = build_router(state.clone())
            .oneshot(get_request("/sync/reading-log?serial=S", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let body = serde_json::json!({ "device_serial": "S", "lines": [], "dumps": [] });
        let resp = build_router(state)
            .oneshot(reading_log_request(None, body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// Both routes are per-device, so an unnamed device is refused rather than
    /// answered with — or credited to — some other Kindle's progress.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unnamed_device_is_refused_rather_than_guessed_at() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token) = bare_state(tmp.path());

        let resp = build_router(state.clone())
            .oneshot(get_request("/sync/reading-log", Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = serde_json::json!({
            "device_serial": "",
            "lines": [reading_line("260803:100000", "NextPage", 60_000, 100)],
            "dumps": [],
        });
        let resp = build_router(state)
            .oneshot(reading_log_request(Some(&token), body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// A sync that found nothing new is the common case, and must cost the
    /// library nothing: no parse, no write, an empty report.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sync_with_nothing_new_stores_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token) = bare_state(tmp.path());
        let body = serde_json::json!({
            "device_serial": "G000TESTSERIAL",
            "lines": [],
            "dumps": [],
        });
        let resp = build_router(state)
            .oneshot(reading_log_request(Some(&token), body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let report = json_body(resp).await;
        assert_eq!(report["sessions"], 0);
        assert_eq!(report["added"], 0);
        // Nothing stored for this device, so nothing is confirmed — and the
        // device must not read that as licence to delete its archive.
        assert_eq!(report["watermark"], "");
    }

    // --- Handwriting: ink on /sync/annotations, notebooks on their own route ---
    //
    // Decoding an `nbk` is `bokai::formats::nbk`'s job and needs a real KDF
    // SQLite file, which this crate has no fixture for. What is covered here is
    // everything around that decode: which notebooks the device is told to send,
    // what happens to one it can't decode, and that neither route is reachable
    // without a token.

    fn notebook_request(token: Option<&str>, body: serde_json::Value) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri("/sync/notebooks")
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("x-sidle-token", t);
        }
        b.body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    /// The manifest is what makes a sync cheap, and it is keyed per device: the
    /// same book inked on two Kindles is two separate facts, so one device's
    /// checkpoint must never let another skip an upload.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_ink_manifest_reports_only_the_asking_devices_checkpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token) = bare_state(tmp.path());
        let conn = db::open(&state.paths.db()).unwrap();
        db::set_ink_sync_sha(&conn, "SCRIBE01", "ASINA", "sha-a", "t0").unwrap();
        db::set_ink_sync_sha(&conn, "SCRIBE01", "ASINB", "sha-b", "t0").unwrap();
        db::set_ink_sync_sha(&conn, "OTHERKINDLE", "ASINC", "sha-c", "t0").unwrap();

        let resp = build_router(state.clone())
            .oneshot(get_request(
                "/sync/annotations?device_serial=SCRIBE01",
                Some(&token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let m = json_body(resp).await;
        assert_eq!(m["ink"]["ASINA"], "sha-a");
        assert_eq!(m["ink"]["ASINB"], "sha-b");
        assert!(
            m["ink"].get("ASINC").is_none(),
            "another Kindle's checkpoint would make this one skip an upload"
        );

        // A device that has never synced is told nothing, so it sends everything.
        let resp = build_router(state)
            .oneshot(get_request(
                "/sync/annotations?device_serial=NEWDEVICE",
                Some(&token),
            ))
            .await
            .unwrap();
        assert_eq!(json_body(resp).await["ink"], serde_json::json!({}));
    }

    /// Without a serial there is no per-device answer, and defaulting to one
    /// would hand back a checkpoint that isn't this device's — which would make
    /// it skip ink this library has never seen. Refuse instead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_ink_manifest_refuses_an_unnamed_device() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token) = bare_state(tmp.path());
        let resp = build_router(state)
            .oneshot(get_request("/sync/annotations", Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// The notebook manifest is library-wide, not per device: a notebook is one
    /// entity wherever it was written, so any Scribe asking gets the same answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_notebook_manifest_lists_the_library_regardless_of_device() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token) = bare_state(tmp.path());
        let conn = db::open(&state.paths.db()).unwrap();
        let uuid = "da85e6f7-9672-2e2b-ef94-e57fc3502e45";
        db::upsert_notebook(&conn, uuid, 3, "sha-nb", "t0", "t0").unwrap();

        for who in ["SCRIBE01", "OTHERKINDLE"] {
            let resp = build_router(state.clone())
                .oneshot(get_request(
                    &format!("/sync/notebooks?device_serial={who}"),
                    Some(&token),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(json_body(resp).await["notebooks"][uuid], "sha-nb");
        }
    }

    /// One notebook that won't decode must not cost the sync the others: it is
    /// named in `failed` and the request still succeeds. (All three are garbage
    /// here — the point is the shape of the answer, not the decode.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_notebook_that_cannot_be_decoded_is_reported_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token) = bare_state(tmp.path());
        let body = serde_json::json!({
            "notebooks": [
                {
                    "uuid": "da85e6f7-9672-2e2b-ef94-e57fc3502e45",
                    "nbk_b64": BASE64.encode(b"not a KDF database"),
                    "updated_at": "2026-08-10T11:22:33",
                },
                {
                    "uuid": "7507c10c-d7eb-a652-c030-2090b7bb1660",
                    "nbk_b64": BASE64.encode(b"also not one"),
                    "cover_b64": BASE64.encode(b"PNG"),
                    "updated_at": "2026-08-10T11:22:34",
                },
            ],
        });
        let resp = build_router(state)
            .oneshot(notebook_request(Some(&token), body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "one bad nbk is not a 500");
        let out = json_body(resp).await;
        assert_eq!(out["imported"], 0);
        assert_eq!(
            out["failed"].as_array().map(Vec::len),
            Some(2),
            "each failure is named so the log can show which notebook it was"
        );
    }

    /// A malformed blob is the client's bug, and must read as one — distinct
    /// from an `nbk` that arrived intact but wouldn't decode.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_notebooks_rejects_bad_base64() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, token) = bare_state(tmp.path());
        let body = serde_json::json!({
            "notebooks": [ {
                "uuid": "da85e6f7-9672-2e2b-ef94-e57fc3502e45",
                "nbk_b64": "!!!not base64!!!",
                "updated_at": "2026-08-10T11:22:33",
            } ],
        });
        let resp = build_router(state)
            .oneshot(notebook_request(Some(&token), body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn notebook_endpoints_reject_missing_token() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _token) = bare_state(tmp.path());

        let resp = build_router(state.clone())
            .oneshot(get_request("/sync/notebooks", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = build_router(state.clone())
            .oneshot(notebook_request(None, serde_json::json!({"notebooks": []})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = build_router(state)
            .oneshot(get_request("/sync/annotations?device_serial=X", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// Ink rides in the annotation bundle, so a notebook whose host book isn't
    /// in the library must be skipped without costing the sidecars that arrived
    /// with it — the highlights are the thing that must always land.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ink_for_an_unknown_book_does_not_cost_the_annotations() {
        let tmp = tempfile::tempdir().unwrap();
        let (paths, book) = library_with_matching_book(tmp.path());
        let token = load_or_generate_token(&paths.root).unwrap();
        let state = AppState {
            paths: paths.clone(),
            token: Arc::from(token.as_str()),
        };
        let body = serde_json::json!({
            "device_serial": "G000TESTSERIAL",
            "sdrs": [ {
                "sdr_name": "book.deadbeef.sdr",
                "yjr_b64": BASE64.encode(yjr_bookmark(1492, 0, 9)),
            } ],
            "inks": [ {
                "asin": "NOSUCHASIN",
                "nbk_b64": BASE64.encode(b"not a KDF database"),
            } ],
        });
        let resp = build_router(state)
            .oneshot(sync_request(Some(&token), body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let report = json_body(resp).await;
        assert_eq!(report["ink_books"], 0);
        assert!(
            report["annotations"]["inserted"].as_u64().unwrap() >= 1,
            "the bookmark still imported: {report}"
        );
        let conn = db::open(&paths.db()).unwrap();
        assert!(
            !db::list_annotations_for_book(&conn, book)
                .unwrap()
                .is_empty()
        );
    }
}
