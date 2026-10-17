//! Process-wide state owned by Tauri's manager.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use rusqlite::Connection;
use tauri::{AppHandle, Manager};
use tokio::sync::Mutex;

use crate::device_monitor::DeviceState;
use crate::library::{LibraryPaths, db};
use crate::queue::{self, QueueHandle};
use crate::server::ServerHandle;
use sidle_core::library::apps;
use sidle_core::library::device::Transport;
use sidle_core::library::device::deploy::DeploySource;
use sidle_core::library::device::digest::DigestCache;
use sidle_core::library::device::dist;

pub type DbHandle = Arc<Mutex<Connection>>;

/// [`DbHandle`] lent through [`db::Access`], each borrow a `blocking_lock`.
pub struct Borrowed<'a>(pub &'a DbHandle);

impl db::Access for Borrowed<'_> {
    fn with<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        f(&self.0.blocking_lock())
    }
}

/// One `Arc<dyn Transport>` per device connection, `None` once it drops.
pub type SharedTransport = Arc<Mutex<Option<Arc<dyn Transport>>>>;

/// The search index of one `book_id`, replaced by the next book's.
pub type ReaderSearchCache = Arc<Mutex<Option<(i64, Arc<sidle_core::library::anchor::BookIndex>)>>>;

/// What the open book's deferred fetches are served from.
pub struct ReaderStoreEntry {
    pub images: sidle_core::reader::ImageStore,
    /// Every section's `(href, html)` in spine order.
    pub sections: Vec<(String, String)>,
    pub eid_to_section: std::collections::HashMap<i64, usize>,
}

/// One `(book_id, ReaderStoreEntry)`, filled by `reader_open`, dropped by `reader_release`.
pub type ReaderStoreCache = Arc<Mutex<Option<(i64, Arc<ReaderStoreEntry>)>>>;

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
    pub transport: SharedTransport,
    pub server: ServerHandle,
    /// Where the on-device app's binary and `device/` tree are read from.
    pub device_app_source: DeploySource,
    pub reader_search_cache: ReaderSearchCache,
    pub reader_store: ReaderStoreCache,
}

/// The first ancestor of `CARGO_MANIFEST_DIR` whose `Cargo.toml` declares
/// `[workspace]`; `Err` outside one.
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

        let resource_dir = app.path().resource_dir().ok();

        let conn = db::open(&paths.db())?;
        // A `converting` job left by a crash goes back to `pending`.
        let _ = conn.execute(
            "UPDATE conversion_jobs SET status = 'pending', updated_at = ?1
             WHERE status = 'converting'",
            rusqlite::params![db::now_iso()],
        );
        // `kfx_sha256` keys the on-device filename; a row lacking one gets it here.
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
        // `asin` names the book's `<title>_<ASIN>.sdr/` on the device; a row
        // lacking one reads it from its KFX.
        for (book_id, kfx_path) in db::books_missing_asin(&conn).unwrap_or_default() {
            match bokai::Book::open(std::path::Path::new(&kfx_path)) {
                Ok(book) => {
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
        let app_rows = db::list_app_sources(&conn).unwrap_or_default();

        let db: DbHandle = Arc::new(Mutex::new(conn));
        let queue = queue::spawn(app.clone(), db.clone(), paths.clone(), default_workers());

        // `bootstrap` runs from `setup` on the OS main thread, outside a tokio runtime.
        for id in pending {
            let q = queue.clone();
            tauri::async_runtime::spawn(async move {
                let _ = q.enqueue(id).await;
            });
        }

        // `sync_pulse` turns the server's pulse files into Tauri events.
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

        let device_app_source = if cfg!(debug_assertions) {
            match find_workspace_root() {
                Ok(root) => DeploySource::from_workspace_root(&root),
                Err(e) => {
                    eprintln!(
                        "[sidle/bootstrap] device-app: workspace root not found ({e}); \
                         the deploy section will show SourceMissing for all files"
                    );
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

        // `dist::refresh` stages what `sidle-server` serves over `/device/...`,
        // on a thread of its own.
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
        })
    }
}
