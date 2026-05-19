//! Background polling for Kindle connect/disconnect events.
//!
//! Polls every 2 seconds, dedupes against the last snapshot, emits
//! `device:status` over Tauri events when state changes (and also on the very
//! first tick so the frontend has an initial value without a separate fetch).
//!
//! On every transition into "Kindle connected" (the previous snapshot had a
//! different serial — None, or a different device — and the current one has
//! a serial), the monitor fires off a one-shot auto-pull of `<kindle>/dedrm`.
//! Each new file is imported via the standard pipeline and its background
//! conversion enqueued, so the user doesn't have to click anything for a
//! freshly-DRM-stripped book to land in the library.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

use crate::device::DeviceInfo;
use crate::device::dedrm::{self, PullResult};
use crate::device::detect;
use crate::library::LibraryPaths;
use crate::queue::QueueHandle;
use crate::state::DbHandle;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub type DeviceState = Arc<Mutex<Option<DeviceInfo>>>;

pub fn new_state() -> DeviceState {
    Arc::new(Mutex::new(None))
}

pub fn spawn(
    app: AppHandle,
    state: DeviceState,
    db: DbHandle,
    paths: LibraryPaths,
    queue: QueueHandle,
) {
    tauri::async_runtime::spawn(async move {
        let mut first_tick = true;
        // Tracked separately from `state` so we can tell None→Some and
        // Some(A)→Some(B) apart without comparing whole `DeviceInfo`s.
        let mut last_serial: Option<String> = None;

        loop {
            let next = detect::detect();

            let just_connected: Option<DeviceInfo> = {
                let current_serial = next.as_ref().map(|d| d.serial.clone());
                let is_new = current_serial != last_serial;
                last_serial = current_serial;
                if is_new { next.clone() } else { None }
            };

            let changed = {
                let mut guard = state.lock().await;
                let prev = guard.clone();
                let changed = prev != next;
                *guard = next.clone();
                changed
            };
            if changed || first_tick {
                let _ = app.emit("device:status", &next);
            }
            first_tick = false;

            if let Some(device) = just_connected {
                // Fire-and-forget — keep the poll loop ticking even while
                // the auto-pull is doing IO. Each spawned task owns clones
                // of the shared handles.
                tauri::async_runtime::spawn(autopull_on_connect(
                    app.clone(),
                    db.clone(),
                    paths.clone(),
                    queue.clone(),
                    device,
                ));
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

/// Scan the device's `/dedrm` folder, import every file not already in the
/// library, and enqueue each fresh row for its background KFX→EPUB
/// conversion. Best-effort: errors are logged but don't unwind.
///
/// The DB lock is acquired briefly per-step (one short scan, then one short
/// acquisition per imported file) rather than held for the whole batch. That
/// keeps concurrent `library_list` invokes from the frontend unblocked, so
/// the initial gallery renders without waiting for the autopull, and newly-
/// imported rows can show up progressively as each one lands.
async fn autopull_on_connect(
    app: AppHandle,
    db: DbHandle,
    paths: LibraryPaths,
    queue: QueueHandle,
    device: DeviceInfo,
) {
    let serial = device.serial.clone();

    // 1a. Hash dedrm files OFF the DB lock. Reading several MB-each off a
    //     USB-attached Kindle takes a second or two; doing it under the
    //     mutex would block the frontend's first `library_list` invoke and
    //     leave the gallery empty during that window.
    let candidates = {
        let device = device.clone();
        match tokio::task::spawn_blocking(move || dedrm::hash_dedrm_candidates(&device)).await
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[sidle/autopull] {serial}: hash task panicked: {e}");
                return;
            }
        }
    };
    // 1b. Filter against the library with a brief lock (one quick SELECT
    //     per candidate).
    let to_pull: Vec<PathBuf> = {
        let db = db.clone();
        match tokio::task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            dedrm::filter_new_candidates(&conn, candidates)
        })
        .await
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[sidle/autopull] {serial}: filter task panicked: {e}");
                return;
            }
        }
    };

    let total = to_pull.len();
    if total == 0 {
        return;
    }

    // Immediate progress signal so the status bar updates the moment a
    // Kindle connects, even before the first import completes.
    let _ = app.emit(
        "device:autopull-progress",
        AutoPullProgress { done: 0, total },
    );

    // 2. Import each path with its own (brief) lock acquisition.
    let mut imported = 0usize;
    let mut duplicate = 0usize;
    let mut failed = 0usize;
    for (i, path) in to_pull.into_iter().enumerate() {
        let pair = {
            let db = db.clone();
            let paths = paths.clone();
            let device = device.clone();
            let path = path.clone();
            tokio::task::spawn_blocking(move || {
                let conn = db.blocking_lock();
                dedrm::pull_one(&conn, &paths, &device, &path)
            })
            .await
        };
        let (result, enqueue) = match pair {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[sidle/autopull] {serial}: pull task panicked: {e}");
                continue;
            }
        };

        match &result {
            PullResult::Imported { .. } => imported += 1,
            PullResult::Duplicate { .. } => duplicate += 1,
            PullResult::Failed { .. } => failed += 1,
        }

        // Fire the per-book event immediately so the frontend can refresh
        // this single row in instead of waiting until the whole batch is
        // done.
        let _ = app.emit("device:pull-progress", &result);
        let _ = app.emit(
            "device:autopull-progress",
            AutoPullProgress { done: i + 1, total },
        );

        if let Some(book_id) = enqueue {
            let _ = queue.enqueue(book_id).await;
        }
    }

    eprintln!(
        "[sidle/autopull] {serial}: imported {imported}, duplicate {duplicate}, failed {failed}"
    );
    let _ = app.emit(
        "device:autopull-done",
        AutoPullSummary {
            imported: imported as u32,
            duplicate: duplicate as u32,
            failed: failed as u32,
        },
    );
}

#[derive(serde::Serialize, Clone)]
struct AutoPullProgress {
    done: usize,
    total: usize,
}

#[derive(serde::Serialize, Clone)]
struct AutoPullSummary {
    imported: u32,
    duplicate: u32,
    failed: u32,
}
