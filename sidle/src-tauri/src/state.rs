//! Process-wide state owned by Tauri's manager.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use rusqlite::Connection;
use tauri::{AppHandle, Manager};
use tokio::sync::Mutex;

use crate::device::Transport;
use crate::device::kual::{self, KualSource};
use crate::device::monitor::{self, DeviceState};
use crate::library::{LibraryPaths, db};
use crate::queue::{self, QueueHandle};
use crate::server::ServerHandle;

pub type DbHandle = Arc<Mutex<Connection>>;

/// Long-lived, shared on-device IO handle. Opened once per device-connect by the
/// monitor and cleared on disconnect; every Tauri command and the on-connect
/// sync borrow this same `Arc<dyn Transport>` instead of calling
/// `DeviceInfo::open_transport()` themselves.
///
/// Two reasons it has to be shared, not per-call: MTP-class Kindles expose a
/// single USB session — a second `open_transport` while the first is live races
/// the first one's bulk transfers and the device returns `exclusive_access` to
/// whichever loses. Mass-storage doesn't care (the transport is a `PathBuf`
/// wrapper), but using the same lifecycle for both keeps the call sites uniform.
/// Concurrency inside the shared session is already serialized by
/// `MtpTransport`'s storage mutex, so multiple borrowers don't need to
/// coordinate here.
pub type SharedTransport = Arc<Mutex<Option<Arc<dyn Transport>>>>;

/// Single-entry cache for the reader's search index. Keyed by `book_id`,
/// holds at most one — switching books rebuilds. First search per book pays
/// the parse cost (same as `reader_open`); subsequent are HashMap walks. Lives
/// for the app session; the keyed-by-`book_id` replacement is the eviction.
pub type ReaderSearchCache = Arc<Mutex<Option<(i64, Arc<sidle_core::library::anchor::BookIndex>)>>>;

/// Everything the open book's deferred fetches are served from: the parsed
/// book that produces image bytes on request, the full built section HTML
/// (source for windowed section streaming on large text books), and the
/// eid→section index (jumps into sections the webview hasn't streamed yet).
pub struct ReaderStoreEntry {
    pub images: sidle_core::reader::ImageStore,
    /// Every section's `(href, html)` in spine order — already built by the
    /// open; `reader_fetch_sections` hands them out without recompute.
    pub sections: Vec<(String, String)>,
    pub eid_to_section: std::collections::HashMap<i64, usize>,
}

/// Single-entry store backing the open book's on-demand fetches
/// (`reader_fetch_resources` / `reader_fetch_sections` / `reader_eid_section`).
/// Populated by `reader_open`, dropped by `reader_release` — the frontend
/// releases it on reader close and once everything deferred has been delivered
/// (the webview keeps the data; the store is then dead weight). Keyed by
/// `book_id`; opening another book replaces it.
pub type ReaderStoreCache = Arc<Mutex<Option<(i64, Arc<ReaderStoreEntry>)>>>;

/// Default to all available cores. Conversion is CPU-bound; the OS scheduler
/// handles contention with other apps better than we can from a guessed cap.
fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

pub struct AppState {
    pub db: DbHandle,
    pub paths: LibraryPaths,
    pub queue: QueueHandle,
    pub device: DeviceState,
    /// The shared device IO handle. See [`SharedTransport`].
    pub transport: SharedTransport,
    pub server: ServerHandle,
    /// Source-of-truth paths for the KUAL deploy button (binary +
    /// bundle dir). Resolved once at startup by walking up from
    /// `CARGO_MANIFEST_DIR` to the workspace Cargo.toml.
    pub kual_source: KualSource,
    /// Reader search's per-session `TextIndex` cache (see [`ReaderSearchCache`]).
    pub reader_search_cache: ReaderSearchCache,
    /// The open book's on-demand fetch store (see [`ReaderStoreCache`]).
    pub reader_store: ReaderStoreCache,
}

/// Walk up from `CARGO_MANIFEST_DIR` (`<repo>/sidle/src-tauri`) until
/// we hit a `Cargo.toml` declaring `[workspace]` — that's the repo
/// root. Robust to layout changes that don't move the workspace
/// manifest. If the desktop app is ever shipped packaged (outside the
/// dev workspace), this returns Err and KualSource paths will report
/// `SourceMissing` to the UI.
pub(crate) fn find_workspace_root() -> Result<PathBuf> {
    let start = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut p: &Path = start.as_path();
    loop {
        let candidate = p.join("Cargo.toml");
        if candidate.exists()
            && std::fs::read_to_string(&candidate)
                .ok()
                .is_some_and(|s| s.contains("[workspace]"))
        {
            return Ok(p.to_path_buf());
        }
        match p.parent() {
            Some(parent) => p = parent,
            None => {
                return Err(anyhow!(
                    "workspace root not found above {}",
                    start.display()
                ));
            }
        }
    }
}

impl AppState {
    pub fn bootstrap(app: AppHandle) -> Result<Self> {
        let _ = std::fs::write("/tmp/sidle-bootstrap.log", "bootstrap: enter\n");
        let paths = LibraryPaths::resolve()?;
        let _ = std::fs::write(
            "/tmp/sidle-bootstrap.log",
            format!("bootstrap: paths = {}\n", paths.root.display()),
        );
        paths.ensure()?;
        let _ = std::fs::write("/tmp/sidle-bootstrap.log", "bootstrap: paths ensured\n");

        // Capture the bundle resource dir while `app` is still ours — it's moved
        // into the device monitor below, but the KUAL source resolution further
        // down (packaged builds) needs it. `None` in dev / if Tauri can't resolve.
        let resource_dir = app.path().resource_dir().ok();

        let conn = db::open(&paths.db())?;
        // Recover from crash: any job stuck mid-convert should re-pend.
        let _ = conn.execute(
            "UPDATE conversion_jobs SET status = 'pending', updated_at = ?1
             WHERE status = 'converting'",
            rusqlite::params![db::now_iso()],
        );
        // Backfill `kfx_sha256` for any pre-`kfx_sha256`-column rows. Push
        // requires the hash to derive the on-device filename infix; rows
        // imported before the column existed would otherwise be marked
        // "kfx hash missing — reconvert" until manual intervention.
        for (book_id, kfx_path) in db::books_missing_kfx_sha(&conn).unwrap_or_default() {
            match sidle_core::library::import::sha256_of_file(std::path::Path::new(&kfx_path)) {
                Ok(sha) => {
                    let _ = db::set_kfx_path_and_sha(&conn, book_id, &kfx_path, &sha);
                }
                Err(e) => {
                    eprintln!(
                        "[sidle/bootstrap] book {book_id}: backfill kfx_sha256 \
                         failed for {kfx_path}: {e}; row stays unsendable until reconvert"
                    );
                }
            }
        }
        // Backfill `asin` for any row that has a KFX but the value never made
        // it to the row (rows converted before the worker started capturing
        // bokai's stamped ASIN). One-time `Book::open` on each KFX is heavier
        // than the sha256 backfill above, but the work happens at most once
        // per row and only for the rare pre-fix subset. Without it,
        // device-delete can't clean the on-device `<title>_<ASIN>.sdr/`
        // catalog sidecar Kindle invents.
        for (book_id, kfx_path) in db::books_missing_asin(&conn).unwrap_or_default() {
            match bokai::Book::open(std::path::Path::new(&kfx_path)) {
                Ok(book) => {
                    // The content_id bokai BAKED into this KFX — the device's
                    // `.sdr` / `.notebooks` key. Read it back rather than
                    // recomputing via `resolve_export_asin` (which used a
                    // different input and produced a value that never matched the
                    // baked one for PDF→KFX books).
                    if let Some(asin) = book.metadata().asin.clone() {
                        let _ = db::set_asin(&conn, book_id, &asin);
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[sidle/bootstrap] book {book_id}: backfill asin failed for \
                         {kfx_path}: {e}; catalog .sdr cleanup will be skipped on delete"
                    );
                }
            }
        }
        let pending = db::pending_or_error_book_ids(&conn).unwrap_or_default();

        let db: DbHandle = Arc::new(Mutex::new(conn));
        let queue = queue::spawn(app.clone(), db.clone(), paths.clone(), default_workers());

        // Re-enqueue surviving jobs. Use Tauri's runtime — `bootstrap` is
        // called from `setup`, which runs on the OS main thread.
        for id in pending {
            let q = queue.clone();
            tauri::async_runtime::spawn(async move {
                let _ = q.enqueue(id).await;
            });
        }

        // Watch the daemon's `.sync-pulse.json` to live-repaint an open reader after
        // a LAN annotation sync (the detached server can't emit a Tauri event). Must
        // clone `app` — `monitor::spawn` consumes it below.
        crate::sync_pulse::spawn(app.clone(), paths.clone(), queue.clone());

        let device = monitor::new_state();
        let transport: SharedTransport = Arc::new(Mutex::new(None));
        monitor::spawn(
            app,
            device.clone(),
            transport.clone(),
            db.clone(),
            paths.clone(),
            queue.clone(),
        );

        // Dev reads the KUAL source straight from the checkout (a live `cargo
        // build`/armv7 cross-build shows up, and the staleness hint works);
        // a packaged build carries the assets as bundle resources. `resource_dir`
        // is captured above because `app` is moved into the monitor before this.
        let kual_source = if cfg!(debug_assertions) {
            match find_workspace_root() {
                Ok(root) => KualSource::from_workspace_root(&root),
                Err(e) => {
                    eprintln!(
                        "[sidle/bootstrap] kual: workspace root not found ({e}); \
                         KUAL deploy section will show SourceMissing for all files"
                    );
                    // Synthesize a path that won't resolve — compute_status
                    // handles missing source files gracefully.
                    KualSource::from_workspace_root(Path::new("/__no_workspace__"))
                }
            }
        } else {
            match resource_dir {
                Some(dir) => KualSource::from_resource_root(&dir),
                None => {
                    eprintln!(
                        "[sidle/bootstrap] kual: bundle resource dir unavailable; \
                         KUAL deploy section will show SourceMissing for all files"
                    );
                    KualSource::from_resource_root(Path::new("/__no_resources__"))
                }
            }
        };

        // Stage the LAN self-update bundle so a detached `sidle-server` can serve
        // the current picker binary over `/kual/...` without a cable. mtime-gated
        // (a no-op once warm); `SourceMissing` (binary not cross-built yet) and IO
        // errors are non-fatal — they must never block app launch.
        match kual::stage_dist(&kual_source, &paths.kual_dist()) {
            Ok(outcome) => eprintln!("[sidle/bootstrap] kual-dist: {outcome:?}"),
            Err(e) => eprintln!("[sidle/bootstrap] kual-dist staging failed: {e:#}"),
        }

        Ok(Self {
            db,
            paths,
            queue,
            device,
            transport,
            server: ServerHandle::default(),
            kual_source,
            reader_search_cache: Arc::new(Mutex::new(None)),
            reader_store: Arc::new(Mutex::new(None)),
        })
    }
}
