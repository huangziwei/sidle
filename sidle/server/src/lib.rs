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
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rusqlite::Connection;
use tokio::net::TcpListener;

use sidle_core::library::{LibraryPaths, db};

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

pub async fn serve(config: Config) -> Result<()> {
    config.paths.ensure().context("ensure library paths")?;

    let state = AppState {
        paths: config.paths,
        token: Arc::from(config.token),
    };

    let app = Router::new()
        .route("/", get(health))
        .route("/list.json", get(list_json))
        .route("/get/{id}", get(get_book))
        .route("/cover/{id}", get(get_cover))
        .with_state(state);

    let listener = TcpListener::bind(&config.bind)
        .await
        .with_context(|| format!("bind {}", config.bind))?;
    let local = listener.local_addr()?;
    tracing::info!("sidle-server listening on http://{local}");
    axum::serve(listener, app).await.context("axum::serve")?;
    Ok(())
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
        "  GET /cover/{id}    — cover image\n",
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

async fn list_json(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<Vec<db::BookRow>>, StatusCode> {
    check_token(&headers, &query, &state.token)?;
    let conn = open_db(&state.paths)?;
    let books = db::list_books(&conn).map_err(|err| {
        tracing::error!(?err, "list_books failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(books))
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
    serve_file(
        PathBuf::from(&kfx_path),
        "application/octet-stream",
        Some(&filename_from_path(&kfx_path)),
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
    let mime = mime_guess::from_path(&cover_path)
        .first_raw()
        .unwrap_or("image/jpeg");
    serve_file(PathBuf::from(&cover_path), mime, None).await
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
