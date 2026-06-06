//! Background conversion queue.
//!
//! A single dispatcher task owns the pending-job list and a `JoinSet` of
//! in-flight conversions; it spawns up to `workers` blocking tasks at a time.
//! Messages on the input channel come from `library_import`, `conversion_retry`,
//! and `conversion_set_workers`.

pub mod worker;

use std::sync::Arc;

use anyhow::Result;
use tauri::{AppHandle, Emitter};
use tokio::sync::{Mutex, mpsc};

use crate::library::LibraryPaths;
use crate::state::DbHandle;

#[derive(Debug)]
enum QueueMsg {
    /// `(book_id, reconvert, color)` — `reconvert` = forced re-run (source→target
    /// only, skip the import-time cover enrichment that mutates the source KFX).
    /// `color` = encode EPUB→KFX interior plates as full-color JXR (else
    /// grayscale, the default); ignored by the other conversion directions.
    Enqueue(i64, bool, bool),
    SetWorkers(usize),
    Shutdown,
}

#[derive(Clone)]
pub struct QueueHandle {
    tx: mpsc::Sender<QueueMsg>,
    workers: Arc<Mutex<usize>>,
}

impl QueueHandle {
    /// Enqueue a first-time conversion (import / autopull): runs the format
    /// conversion **and** the import-time cover enrichment.
    pub async fn enqueue(&self, book_id: i64) -> Result<()> {
        // First-time conversion ⇒ grayscale (the pipeline default).
        self.tx.send(QueueMsg::Enqueue(book_id, false, false)).await?;
        Ok(())
    }

    /// Enqueue a forced re-convert (the "Force re-convert" button): source→target
    /// only — skips the cover-enrichment tail-step so the source KFX (and its
    /// `kfx_sha256`) is left untouched, preserving device annotation-sync matching.
    pub async fn enqueue_reconvert(&self, book_id: i64, color: bool) -> Result<()> {
        self.tx.send(QueueMsg::Enqueue(book_id, true, color)).await?;
        Ok(())
    }

    pub async fn set_workers(&self, n: usize) -> Result<()> {
        let n = n.clamp(1, 16);
        self.tx.send(QueueMsg::SetWorkers(n)).await?;
        *self.workers.lock().await = n;
        Ok(())
    }

    pub async fn current_workers(&self) -> usize {
        *self.workers.lock().await
    }

    #[allow(dead_code)]
    pub async fn shutdown(&self) {
        let _ = self.tx.send(QueueMsg::Shutdown).await;
    }
}

pub fn spawn(
    app: AppHandle,
    db: DbHandle,
    paths: LibraryPaths,
    initial_workers: usize,
) -> QueueHandle {
    let (tx, rx) = mpsc::channel::<QueueMsg>(256);
    let workers = Arc::new(Mutex::new(initial_workers));
    // Use Tauri's runtime — Tauri's `setup` runs on the OS main thread, outside
    // any tokio runtime context, so a bare `tokio::spawn` would panic with
    // "there is no reactor running". `tauri::async_runtime::spawn` routes
    // through Tauri's internal tokio runtime.
    tauri::async_runtime::spawn(dispatcher(
        app,
        db,
        paths,
        rx,
        Arc::clone(&workers),
        initial_workers,
    ));
    QueueHandle { tx, workers }
}

async fn dispatcher(
    app: AppHandle,
    db: DbHandle,
    paths: LibraryPaths,
    mut rx: mpsc::Receiver<QueueMsg>,
    workers_state: Arc<Mutex<usize>>,
    initial_workers: usize,
) {
    let mut pending: Vec<(i64, bool, bool)> = Vec::new();
    let mut in_flight: tokio::task::JoinSet<i64> = tokio::task::JoinSet::new();
    let mut cap: usize = initial_workers.max(1);

    loop {
        // Spawn as many as cap allows.
        while in_flight.len() < cap
            && let Some((book_id, reconvert, color)) = pending.pop()
        {
            let app = app.clone();
            let db = db.clone();
            let paths = paths.clone();
            in_flight.spawn(async move {
                worker::run_job(&app, &db, &paths, book_id, reconvert, color).await;
                book_id
            });
        }

        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(QueueMsg::Enqueue(id, reconvert, color)) => {
                        if !pending.iter().any(|(b, _, _)| *b == id) {
                            pending.insert(0, (id, reconvert, color));
                        }
                    }
                    Some(QueueMsg::SetWorkers(n)) => {
                        cap = n.max(1);
                        *workers_state.lock().await = cap;
                    }
                    Some(QueueMsg::Shutdown) | None => break,
                }
            }
            joined = in_flight.join_next(), if !in_flight.is_empty() => {
                if let Some(Err(e)) = joined {
                    eprintln!("conversion task panicked: {e}");
                }
            }
        }
    }

    while in_flight.join_next().await.is_some() {}
}

pub fn emit_status(app: &AppHandle, book_id: i64, status: &str, error: Option<&str>) {
    #[derive(serde::Serialize, Clone)]
    struct Evt<'a> {
        book_id: i64,
        status: &'a str,
        error: Option<&'a str>,
    }
    let _ = app.emit(
        "conversion:status",
        Evt {
            book_id,
            status,
            error,
        },
    );
}

/// Emit a per-book conversion progress tick. `fraction` is a monotonic 0.0–1.0
/// estimate — the worker maps boko's per-phase reports through
/// `progress_fraction` — and `label` is the human step shown beside the bar
/// (e.g. "Encoding images"). Keyed by `book_id` so concurrent workers each
/// drive their own row. Best-effort, like `emit_status`.
pub fn emit_progress(app: &AppHandle, book_id: i64, fraction: f32, label: &str) {
    #[derive(serde::Serialize, Clone)]
    struct Evt<'a> {
        book_id: i64,
        fraction: f32,
        label: &'a str,
    }
    let _ = app.emit(
        "conversion:progress",
        Evt {
            book_id,
            fraction,
            label,
        },
    );
}
