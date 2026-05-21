//! Process-wide state owned by Tauri's manager.

use std::sync::Arc;

use anyhow::Result;
use rusqlite::Connection;
use tauri::AppHandle;
use tokio::sync::Mutex;

use crate::device::monitor::{self, DeviceState};
use crate::library::{LibraryPaths, db};
use crate::queue::{self, QueueHandle};
use crate::server::ServerHandle;

pub type DbHandle = Arc<Mutex<Connection>>;

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
}

impl AppState {
    pub fn bootstrap(app: AppHandle) -> Result<Self> {
        let _ = std::fs::write("/tmp/sidle-bootstrap.log", "bootstrap: enter\n");
        let paths = LibraryPaths::default_root()?;
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

        let device = monitor::new_state();
        monitor::spawn(
            app,
            device.clone(),
            db.clone(),
            paths.clone(),
            queue.clone(),
        );

        Ok(Self {
            db,
            paths,
            queue,
            device,
            server: ServerHandle::default(),
        })
    }
}
