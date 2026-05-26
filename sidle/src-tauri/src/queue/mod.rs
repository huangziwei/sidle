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
    Enqueue(i64),
    SetWorkers(usize),
    Shutdown,
}

#[derive(Clone)]
pub struct QueueHandle {
    tx: mpsc::Sender<QueueMsg>,
    workers: Arc<Mutex<usize>>,
}

impl QueueHandle {
    pub async fn enqueue(&self, book_id: i64) -> Result<()> {
        self.tx.send(QueueMsg::Enqueue(book_id)).await?;
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
    let mut pending: Vec<i64> = Vec::new();
    let mut in_flight: tokio::task::JoinSet<i64> = tokio::task::JoinSet::new();
    let mut cap: usize = initial_workers.max(1);

    loop {
        // Spawn as many as cap allows.
        while in_flight.len() < cap
            && let Some(book_id) = pending.pop()
        {
            let app = app.clone();
            let db = db.clone();
            let paths = paths.clone();
            in_flight.spawn(async move {
                worker::run_job(&app, &db, &paths, book_id).await;
                book_id
            });
        }

        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(QueueMsg::Enqueue(id)) => {
                        if !pending.contains(&id) {
                            pending.insert(0, id);
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
