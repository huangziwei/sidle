//! Background polling for Kindle connect/disconnect events.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

use crate::library::{LibraryPaths, ingest};
use crate::queue::QueueHandle;
use crate::state::{DbHandle, SharedTransport};
use sidle_core::library::device::dedrm::{self, PullResult};
use sidle_core::library::device::detect;
use sidle_core::library::device::{DeviceInfo, Transport, TransportKind};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub type DeviceState = Arc<Mutex<Option<DeviceInfo>>>;

pub fn new_state() -> DeviceState {
    Arc::new(Mutex::new(None))
}

pub fn spawn(
    app: AppHandle,
    state: DeviceState,
    transport: SharedTransport,
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
            let mut next = detect::detect();

            let (just_connected, just_disconnected, changed) = {
                let mut guard = state.lock().await;
                let prev = guard.clone();

                // Mass-storage refreshes `free_bytes` cheaply on every poll
                if let (Some(new_info), Some(prev_info)) = (next.as_mut(), prev.as_ref()) {
                    let same_device = new_info.serial == prev_info.serial;
                    let is_mtp = matches!(new_info.transport, TransportKind::Mtp { .. });
                    if same_device && is_mtp {
                        if new_info.free_bytes.is_none() {
                            new_info.free_bytes = prev_info.free_bytes;
                            new_info.total_bytes = prev_info.total_bytes;
                        }
                        if new_info.firmware.is_none() {
                            new_info.firmware = prev_info.firmware.clone();
                        }
                    }
                }

                let current_serial = next.as_ref().map(|d| d.serial.clone());
                let is_new = current_serial != last_serial;
                // Disconnect = had a serial last tick, no serial now, OR the
                // serial changed (a different device replaced it).
                let just_disconnected = last_serial.is_some()
                    && (current_serial.is_none() || current_serial != last_serial);
                last_serial = current_serial;
                let just_connected = if is_new { next.clone() } else { None };

                let changed = prev != next;
                *guard = next.clone();
                (just_connected, just_disconnected, changed)
            };

            // Release the shared MTP/mass-storage transport as soon as the
            // device goes away — keeping the `Arc` alive would block re-open
            // on the next plug-in (MTP holds the USB session exclusively).
            if just_disconnected {
                *transport.lock().await = None;
            }

            if changed || first_tick {
                let _ = app.emit("device:status", &next);
            }
            first_tick = false;

            if let Some(device) = just_connected {
                match device.transport {
                    TransportKind::MassStorage { .. } => {
                        // Fire-and-forget — keep the poll loop ticking even
                        // while the connect work is doing IO. Each spawned task
                        // owns clones of the shared handles.
                        tauri::async_runtime::spawn(on_mass_storage_connect(
                            app.clone(),
                            transport.clone(),
                            db.clone(),
                            paths.clone(),
                            queue.clone(),
                            device,
                        ));
                    }
                    TransportKind::Mtp { .. } => {
                        // No DeDRM auto-pull: non-jailbroken (MTP-class)
                        tauri::async_runtime::spawn(on_mtp_connect(
                            app.clone(),
                            state.clone(),
                            transport.clone(),
                            db.clone(),
                            paths.clone(),
                            device,
                        ));
                    }
                }
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

/// Borrow the shared on-device transport, opening it the first time anyone
pub async fn ensure_transport(
    cell: &SharedTransport,
    device: &DeviceInfo,
) -> anyhow::Result<Arc<dyn Transport>> {
    // Hold the lock through the open. mtp-rs cannot tolerate two concurrent
    let mut guard = cell.lock().await;
    if let Some(t) = guard.as_ref() {
        return Ok(t.clone());
    }
    let device_owned = device.clone();
    let opened = tokio::task::spawn_blocking(move || device_owned.open_transport())
        .await
        .map_err(|e| anyhow!("transport-open task panicked: {e}"))??;
    let arc: Arc<dyn Transport> = Arc::from(opened);
    *guard = Some(arc.clone());
    Ok(arc)
}

/// Drop the cached transport so the next [`ensure_transport`] opens fresh.
pub async fn evict_transport(cell: &SharedTransport) {
    *cell.lock().await = None;
}

/// Everything that should happen the moment a mass-storage Kindle is plugged
/// in, in the background so the user can keep working:
///   1. Auto-pull any new DeDRM'd books off `/dedrm` (and enqueue conversions).
async fn on_mass_storage_connect(
    app: AppHandle,
    transport: SharedTransport,
    db: DbHandle,
    paths: LibraryPaths,
    queue: QueueHandle,
    device: DeviceInfo,
) {
    autopull_on_connect(
        app.clone(),
        db.clone(),
        paths.clone(),
        queue,
        device.clone(),
    )
    .await;
    sync_annotations_on_connect(app, transport, db, paths, device).await;
}

/// Everything that should happen when an MTP Kindle (Scribe, 2024+) connects.
/// Sequential, not concurrent: MTP exposes a single USB session, so the
/// device-info read and the annotation pull must not overlap.
async fn on_mtp_connect(
    app: AppHandle,
    state: DeviceState,
    transport: SharedTransport,
    db: DbHandle,
    paths: LibraryPaths,
    device: DeviceInfo,
) {
    refresh_mtp_storage_info(app.clone(), state, transport.clone(), device.clone()).await;
    sync_annotations_on_connect(app, transport, db, paths, device).await;
}

/// Import highlights / notes / bookmarks off the connected Kindle — either
/// transport (mass-storage reads the volume; MTP pulls the `.yjr` over USB).
async fn sync_annotations_on_connect(
    app: AppHandle,
    transport: SharedTransport,
    db: DbHandle,
    paths: LibraryPaths,
    device: DeviceInfo,
) {
    let serial = device.serial.clone();

    let _ = app.emit("annotations:sync-start", ());

    // Borrow the shared transport up front (async path, since opening MTP
    let shared = match ensure_transport(&transport, &device).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[sidle/annsync] {serial}: open transport failed: {e:#}");
            let _ = app.emit("annotations:sync-error", format!("{e:#}"));
            return;
        }
    };

    let app_progress = app.clone();
    let result =
        tokio::task::spawn_blocking(move || -> anyhow::Result<ingest::DeviceImportReport> {
            let on_progress = |stage: &str, current: usize, total: usize, label: &str| {
                let _ = app_progress.emit(
                    "annotations:sync-progress",
                    sidle_core::library::device::annotations::SyncProgress {
                        stage: stage.to_string(),
                        current,
                        total,
                        label: label.to_string(),
                    },
                );
            };
            sidle_core::library::device::annotations::import_device_annotations(
                &device,
                shared.as_ref(),
                &crate::state::Borrowed(&db),
                &paths,
                &on_progress,
            )
        })
        .await;

    // Same MTP-stall recovery as the Tauri commands: on any error the cached
    let evicted = matches!(result, Err(_) | Ok(Err(_)));
    if evicted {
        evict_transport(&transport).await;
    }

    match result {
        Ok(Ok(report)) => {
            eprintln!(
                "[sidle/annsync] {serial}: {} books, {} matched ({} unchanged), {} new; \
                 ink {} books / {} pages ({} unchanged)",
                report.yjr_books,
                report.matched,
                report.unchanged,
                report.annotations.inserted,
                report.ink_books,
                report.ink_pages,
                report.ink_unchanged,
            );
            let _ = app.emit("annotations:sync-done", report);
        }
        Ok(Err(e)) => {
            eprintln!("[sidle/annsync] {serial}: import failed (transport evicted): {e:#}");
            let _ = app.emit("annotations:sync-error", format!("{e:#}"));
        }
        Err(e) => {
            eprintln!("[sidle/annsync] {serial}: import task panicked (transport evicted): {e}");
            let _ = app.emit("annotations:sync-error", e.to_string());
        }
    }
}

/// Scan the device's `/dedrm` folder, import every file not already in the
/// library, and enqueue each fresh row for its background KFX→EPUB
/// conversion. Best-effort: errors are logged but don't unwind.
async fn autopull_on_connect(
    app: AppHandle,
    db: DbHandle,
    paths: LibraryPaths,
    queue: QueueHandle,
    device: DeviceInfo,
) {
    let serial = device.serial.clone();

    // 1a. Hash dedrm files OFF the DB lock.
    let candidates = {
        let device = device.clone();
        match tokio::task::spawn_blocking(move || dedrm::hash_dedrm_candidates(&device)).await {
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

/// One-shot MTP session read after the user plugs in: `GetStorageInfo` plus the
/// firmware. The 2s detect poll opens no session, which would race a push.
async fn refresh_mtp_storage_info(
    app: AppHandle,
    state: DeviceState,
    transport: SharedTransport,
    device: DeviceInfo,
) {
    let serial = device.serial.clone();
    let shared = match ensure_transport(&transport, &device).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[sidle/mtp] device refresh: open failed for {serial}: {e:#}");
            return;
        }
    };
    let (snapshot, firmware) =
        tokio::task::spawn_blocking(move || (shared.free_space(), shared.firmware()))
            .await
            .unwrap_or((None, None));

    if snapshot.is_none() && firmware.is_none() {
        eprintln!("[sidle/mtp] device refresh: no storage info or firmware for {serial}");
        return;
    }

    let updated = {
        let mut guard = state.lock().await;
        match guard.as_mut() {
            // Only apply if the same device is still connected. If the
            // user unplugged before the refresh finished, drop the result
            // rather than resurrecting stale state.
            Some(current) if current.serial == serial => {
                if let Some((free, total)) = snapshot {
                    current.free_bytes = Some(free);
                    current.total_bytes = Some(total);
                }
                if firmware.is_some() {
                    current.firmware = firmware;
                }
                Some(current.clone())
            }
            _ => None,
        }
    };

    if let Some(updated) = updated {
        let _ = app.emit("device:status", &Some(updated));
    }
}

/// Re-read the connected device's free/total over the already-open transport
pub async fn refresh_free_space(app: &AppHandle, state: &DeviceState, transport: &SharedTransport) {
    let device = match state.lock().await.clone() {
        Some(d) => d,
        None => return,
    };
    let serial = device.serial.clone();
    let shared = match ensure_transport(transport, &device).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[sidle/freespace] {serial}: refresh open failed: {e:#}");
            return;
        }
    };
    let snapshot = tokio::task::spawn_blocking(move || shared.free_space())
        .await
        .unwrap_or(None);
    let Some((free, total)) = snapshot else {
        return;
    };

    let updated = {
        let mut guard = state.lock().await;
        match guard.as_mut() {
            // Same-device guard as `refresh_mtp_storage_info`: if the user
            Some(current) if current.serial == serial => {
                current.free_bytes = Some(free);
                current.total_bytes = Some(total);
                Some(current.clone())
            }
            _ => None,
        }
    };

    if let Some(updated) = updated {
        let _ = app.emit("device:status", &Some(updated));
    }
}
