//! Process-wide state owned by Tauri's manager.

use std::sync::Arc;

use anyhow::Result;
use rusqlite::Connection;
use tauri::AppHandle;
use tokio::sync::Mutex;

use crate::library::{LibraryPaths, db};
use crate::queue::{self, QueueHandle};

pub type DbHandle = Arc<Mutex<Connection>>;

const DEFAULT_WORKERS: usize = 2;

pub struct AppState {
    pub db: DbHandle,
    pub paths: LibraryPaths,
    pub queue: QueueHandle,
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
        let pending = db::pending_or_error_book_ids(&conn).unwrap_or_default();

        let db: DbHandle = Arc::new(Mutex::new(conn));
        let queue = queue::spawn(app, db.clone(), paths.clone(), DEFAULT_WORKERS);

        // Re-enqueue surviving jobs. Use Tauri's runtime — `bootstrap` is
        // called from `setup`, which runs on the OS main thread.
        for id in pending {
            let q = queue.clone();
            tauri::async_runtime::spawn(async move {
                let _ = q.enqueue(id).await;
            });
        }

        Ok(Self { db, paths, queue })
    }
}
