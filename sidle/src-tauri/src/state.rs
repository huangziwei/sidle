//! Process-wide state owned by Tauri's manager.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use rusqlite::Connection;
use tauri::AppHandle;
use tokio::sync::Mutex;

use crate::device::kual::KualSource;
use crate::device::monitor::{self, DeviceState};
use crate::library::{LibraryPaths, db};
use crate::queue::{self, QueueHandle};
use crate::server::ServerHandle;

pub type DbHandle = Arc<Mutex<Connection>>;

/// Single-entry cache for the reader's search `TextIndex`. Keyed by `book_id`,
/// holds at most one — switching books rebuilds. First search per book pays
/// the parse cost (same as `reader_open`); subsequent are HashMap walks. Lives
/// for the app session; the keyed-by-`book_id` replacement is the eviction.
pub type ReaderSearchCache = Arc<Mutex<Option<(i64, Arc<boko::kfx_to_epub::TextIndex>)>>>;

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
    pub server: ServerHandle,
    /// Source-of-truth paths for the KUAL deploy button (binary +
    /// bundle dir). Resolved once at startup by walking up from
    /// `CARGO_MANIFEST_DIR` to the workspace Cargo.toml.
    pub kual_source: KualSource,
    /// Reader search's per-session `TextIndex` cache (see [`ReaderSearchCache`]).
    pub reader_search_cache: ReaderSearchCache,
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
            None => return Err(anyhow!("workspace root not found above {}", start.display())),
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
        // boko's stamped ASIN). One-time `Book::open` on each KFX is heavier
        // than the sha256 backfill above, but the work happens at most once
        // per row and only for the rare pre-fix subset. Without it,
        // device-delete can't clean the on-device `<title>_<ASIN>.sdr/`
        // catalog sidecar Kindle invents.
        for (book_id, kfx_path) in db::books_missing_asin(&conn).unwrap_or_default() {
            match boko::Book::open(std::path::Path::new(&kfx_path)) {
                Ok(book) => {
                    if let Some(asin) = boko::kfx::metadata::resolve_export_asin(book.metadata()) {
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
        crate::sync_pulse::spawn(app.clone(), paths.clone());

        let device = monitor::new_state();
        monitor::spawn(
            app,
            device.clone(),
            db.clone(),
            paths.clone(),
            queue.clone(),
        );

        let kual_source = match find_workspace_root() {
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
        };

        Ok(Self {
            db,
            paths,
            queue,
            device,
            server: ServerHandle::default(),
            kual_source,
            reader_search_cache: Arc::new(Mutex::new(None)),
        })
    }
}
