//! Process-wide state owned by Tauri's manager.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use rusqlite::Connection;
use tauri::{AppHandle, Manager};
use tokio::sync::Mutex;

use crate::device_monitor::DeviceState;
use crate::library::{LibraryPaths, db, extent};
use crate::queue::{self, QueueHandle};
use crate::server::ServerHandle;
use sidle_core::library::apps;
use sidle_core::library::device::Transport;
use sidle_core::library::device::deploy::DeploySource;
use sidle_core::library::device::digest::DigestCache;
use sidle_core::library::device::dist;

pub type DbHandle = Arc<Mutex<Connection>>;

/// The app's one connection, lent out a moment at a time.
///
/// Device sync spends minutes in USB IO between short bursts of database work
/// and must not hold the connection across them; it takes a [`db::Access`] and
/// borrows through it. `blocking_lock` puts every borrow on a blocking task.
pub struct Borrowed<'a>(pub &'a DbHandle);

impl db::Access for Borrowed<'_> {
    fn with<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        f(&self.0.blocking_lock())
    }
}

/// Long-lived, shared on-device IO handle. Opened once per device-connect by
/// the monitor and cleared on disconnect; every Tauri command and the
/// on-connect sync borrow this same `Arc<dyn Transport>`.
///
/// An MTP-class Kindle exposes a single USB session: a second `open_transport`
/// races the first one's bulk transfers for an `exclusive_access` error.
/// `MtpTransport`'s storage mutex serializes the borrowers inside that session.
pub type SharedTransport = Arc<Mutex<Option<Arc<dyn Transport>>>>;

/// Single-entry cache for the reader's search index, keyed by `book_id`. The
/// first search of a book pays the parse; the rest are HashMap walks. Opening
/// another book replaces the entry, which is the whole eviction rule.
pub type ReaderSearchCache = Arc<Mutex<Option<(i64, Arc<sidle_core::library::anchor::BookIndex>)>>>;

/// What the open book's deferred fetches are served from: the parsed book that
/// produces image bytes on request, the built section HTML, and the eid→section
/// index that reaches an unstreamed section.
pub struct ReaderStoreEntry {
    pub images: sidle_core::reader::ImageStore,
    /// Every section's `(href, html)` in spine order, built by the open;
    /// `reader_fetch_sections` hands them out without recompute.
    pub sections: Vec<(String, String)>,
    pub eid_to_section: std::collections::HashMap<i64, usize>,
}

/// Single-entry store backing the open book's on-demand fetches
/// (`reader_fetch_resources` / `reader_fetch_sections` / `reader_eid_section`),
/// filled by `reader_open` and dropped by `reader_release`. Keyed by `book_id`;
/// opening another book replaces it.
pub type ReaderStoreCache = Arc<Mutex<Option<(i64, Arc<ReaderStoreEntry>)>>>;

/// Default to all available cores. Conversion is CPU-bound, and the OS
/// scheduler holds contention with other processes.
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
    /// Source-of-truth paths for the on-device app push (binary +
    /// `device/` mirror). Resolved once at startup by walking up from
    /// `CARGO_MANIFEST_DIR` to the workspace Cargo.toml.
    pub device_app_source: DeploySource,
    /// Reader search's per-session `TextIndex` cache (see [`ReaderSearchCache`]).
    pub reader_search_cache: ReaderSearchCache,
    /// The open book's on-demand fetch store (see [`ReaderStoreCache`]).
    pub reader_store: ReaderStoreCache,
    /// Raised to ask an in-flight reading-log import to stop at its next safe
    /// point. One import runs at a time, under the DB lock, and clears the flag
    /// before it starts.
    pub reading_log_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Walk up from `CARGO_MANIFEST_DIR` to the first `Cargo.toml` declaring
/// `[workspace]`, the repo root. Outside the dev workspace this returns `Err`,
/// and `DeploySource` paths report `SourceMissing` to the UI.
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
        let paths = LibraryPaths::resolve()?;
        paths.ensure()?;

        // `app` moves into the device monitor below, and `device_app_source`
        // reads this. `None` in dev, and when Tauri cannot resolve it.
        let resource_dir = app.path().resource_dir().ok();

        let conn = db::open(&paths.db())?;
        // Recover from crash: any job stuck mid-convert should re-pend.
        let _ = conn.execute(
            "UPDATE conversion_jobs SET status = 'pending', updated_at = ?1
             WHERE status = 'converting'",
            rusqlite::params![db::now_iso()],
        );
        // Backfill `kfx_sha256`: a push derives the on-device filename infix
        // from it, and a row without one reads as "kfx hash missing".
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
        // Backfill `asin` for a row holding a KFX and no value. A device-delete
        // reaches the `<title>_<ASIN>.sdr/` catalog sidecar through it.
        for (book_id, kfx_path) in db::books_missing_asin(&conn).unwrap_or_default() {
            match bokai::Book::open(std::path::Path::new(&kfx_path)) {
                Ok(book) => {
                    // The content_id bokai baked into this KFX — the device's
                    // `.sdr` / `.notebooks` key, read back from the file.
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
        // The registered app sources `dist::refresh` below composes, read while
        // `conn` is in hand.
        let app_rows = db::list_app_sources(&conn).unwrap_or_default();

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

        // The position axis of every book missing one, which the Reading Log
        // attributes a Kindle's sessions against. The conversion worker fills
        // it as books arrive; this is the catch-up.
        //
        // Off `bootstrap`'s body: an unindexed library is minutes of container
        // parsing. One book at a time, parsed off the lock, with the connection
        // taken to store each answer.
        {
            let db = db.clone();
            tauri::async_runtime::spawn(async move {
                let pending = {
                    let conn = db.lock().await;
                    db::books_missing_max_position(&conn).unwrap_or_default()
                };
                if pending.is_empty() {
                    return;
                }
                eprintln!("[sidle/bootstrap] extent: indexing {} books", pending.len());
                for (book_id, kfx_path) in pending {
                    let extent =
                        tauri::async_runtime::spawn_blocking(move || extent::of_file(&kfx_path))
                            .await
                            .unwrap_or(None);
                    let conn = db.lock().await;
                    if let Err(e) = db::set_max_position(&conn, book_id, extent) {
                        eprintln!("[sidle/bootstrap] extent: book {book_id} not stored: {e}");
                    }
                }
                // A freshly indexed book is a new answer to sessions stored
                // against a position nothing matched.
                let conn = db.lock().await;
                match db::resolve_reading_sessions(&conn) {
                    Ok(n) if n > 0 => {
                        eprintln!("[sidle/bootstrap] extent: done, {n} reading sessions attributed")
                    }
                    Ok(_) => eprintln!("[sidle/bootstrap] extent: done"),
                    Err(e) => eprintln!("[sidle/bootstrap] extent: done, attribution failed: {e}"),
                }
            });
        }

        // Watch the daemon's `.sync-pulse.json` to live-repaint an open reader after
        // a LAN annotation sync (the detached server can't emit a Tauri event). Must
        // clone `app` — `monitor::spawn` consumes it below.
        crate::sync_pulse::spawn(app.clone(), paths.clone(), queue.clone());

        let device = crate::device_monitor::new_state();
        let transport: SharedTransport = Arc::new(Mutex::new(None));
        crate::device_monitor::spawn(
            app,
            device.clone(),
            transport.clone(),
            db.clone(),
            paths.clone(),
            queue.clone(),
        );

        // A dev build reads the deploy source from the checkout, where a fresh
        // armv7 cross-build shows up; a packaged build reads `resource_dir`.
        let device_app_source = if cfg!(debug_assertions) {
            match find_workspace_root() {
                Ok(root) => DeploySource::from_workspace_root(&root),
                Err(e) => {
                    eprintln!(
                        "[sidle/bootstrap] device-app: workspace root not found ({e}); \
                         the deploy section will show SourceMissing for all files"
                    );
                    // Synthesize a path that won't resolve — compute_status
                    // handles missing source files gracefully.
                    DeploySource::from_workspace_root(Path::new("/__no_workspace__"))
                }
            }
        } else {
            match resource_dir {
                Some(dir) => DeploySource::from_resource_root(&dir),
                None => {
                    eprintln!(
                        "[sidle/bootstrap] device-app: bundle resource dir unavailable; \
                         the deploy section will show SourceMissing for all files"
                    );
                    DeploySource::from_resource_root(Path::new("/__no_resources__"))
                }
            }
        };

        // What a detached `sidle-server` serves over `/device/...`. Off the
        // launch path: a first run hashes the whole fleet, and nothing here
        // waits on the result.
        let staging_source = device_app_source.clone();
        let dist_dir = paths.device_dist();
        let digest_path = paths.source_digests();
        std::thread::spawn(move || {
            let _ = staging_source.stage_binary();
            let plan = apps::plan_from(&staging_source.mount_dir, &app_rows);
            let mut digests = DigestCache::open(&digest_path);
            match dist::refresh(&plan, &staging_source, &dist_dir, &mut digests) {
                Ok(outcome) => eprintln!("[sidle/bootstrap] device-dist: {outcome:?}"),
                Err(e) => eprintln!("[sidle/bootstrap] device-dist refresh failed: {e:#}"),
            }
            let _ = digests.save();
        });

        Ok(Self {
            db,
            paths,
            queue,
            device,
            transport,
            server: ServerHandle::default(),
            device_app_source,
            reader_search_cache: Arc::new(Mutex::new(None)),
            reader_store: Arc::new(Mutex::new(None)),
            reading_log_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
}
