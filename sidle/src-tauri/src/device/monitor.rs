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

use anyhow::anyhow;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

use crate::device::dedrm::{self, PullResult};
use crate::device::detect;
use crate::device::{DeviceInfo, Transport, TransportKind};
use crate::library::{LibraryPaths, ingest};
use crate::queue::QueueHandle;
use crate::state::{DbHandle, SharedTransport};

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
                // via `statvfs`. MTP can't: each refresh would claim the USB
                // interface, which can race with a user-initiated push. So
                // `mtp::detect()` returns `None` for free/total/firmware, and
                // we preserve the last known values across polls when the
                // serial matches. Initial population comes from the on-connect
                // refresh task spawned below; without this carry-over the
                // session-derived fields would flicker back to "—" every tick.
                if let (Some(new_info), Some(prev_info)) = (next.as_mut(), prev.as_ref())
                {
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
                        // Kindles have no `/dedrm` folder, and the jailbreak
                        // that creates it isn't available for Scribe-and-later
                        // firmware. Refresh free space, then sync annotations —
                        // sequentially, since MTP allows only one USB session
                        // at a time.
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
/// asks. Subsequent callers reuse the same `Arc<dyn Transport>` — so two
/// simultaneous Tauri commands or an on-connect monitor task all share the one
/// MTP session, and `MtpTransport::op_lock` serializes their on-wire ops
/// inside it. The session is released when the monitor clears the cell on
/// disconnect (see `just_disconnected` in [`spawn`]).
///
/// Opening the MTP transport calls `mtp_rs` synchronously (it wraps an internal
/// `block_on`), so we route through the blocking pool — calling from a Tauri
/// async command directly would block the executor.
pub async fn ensure_transport(
    cell: &SharedTransport,
    device: &DeviceInfo,
) -> anyhow::Result<Arc<dyn Transport>> {
    // Hold the lock through the open. mtp-rs cannot tolerate two concurrent
    // `MtpDevice::open_by_location` calls against the same USB device — the
    // PTP transaction-ID counters interleave and the device returns mismatched
    // response containers ("Transaction ID mismatch: expected 1, got 0"). The
    // 2024+ Kindle Scribe firmware is especially picky here. Tokio's `Mutex`
    // is .await-aware, so holding it across `spawn_blocking` is fine; serial
    // waiters wake up in order and either reuse the cached Arc (if the first
    // succeeded) or take their own turn at opening (if it failed).
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
/// Called from a command's error path when on-wire IO threw — the cached `Arc`
/// is likely talking to a wedged USB endpoint (e.g. mtp-rs's reaction to an
/// "endpoint stalled" or a TXID desync), and reusing it just multiplies
/// errors. The next caller pays one fresh `MtpDevice::open_by_location`,
/// which lets mtp-rs renegotiate the session against the device — recovers
/// the common-case stall, surfaces a clear error if the firmware is still
/// confused (in which case the user needs to physically replug).
pub async fn evict_transport(cell: &SharedTransport) {
    *cell.lock().await = None;
}

/// Everything that should happen the moment a mass-storage Kindle is plugged
/// in, in the background so the user can keep working:
///   1. Auto-pull any new DeDRM'd books off `/dedrm` (and enqueue conversions).
///   2. Sync highlights / notes / bookmarks off the device into the library.
///
/// The pull runs first so a freshly added book is already in the DB (and so
/// matchable by `kfx_sha256` infix) before the annotation sync tries to link
/// its `.yjr` to a library row.
async fn on_mass_storage_connect(
    app: AppHandle,
    transport: SharedTransport,
    db: DbHandle,
    paths: LibraryPaths,
    queue: QueueHandle,
    device: DeviceInfo,
) {
    autopull_on_connect(app.clone(), db.clone(), paths.clone(), queue, device.clone()).await;
    sync_annotations_on_connect(app, transport, db, paths, device).await;
}

/// Everything that should happen when an MTP Kindle (Scribe, 2024+) connects.
/// Sequential, not concurrent: MTP exposes a single USB session, so the
/// device-info read and the annotation pull must not overlap.
///   1. One-shot session read (free space + firmware) so the popover stops
///      showing "—" for those fields.
///   2. Sync highlights / notes / bookmarks off the device.
///
/// Whether step 2 finds anything depends on the device exposing its `.sdr/.yjr`
/// sidecars over MTP; if it doesn't, the import is a harmless no-op (0 books).
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
/// Runs on the blocking pool; the import is idempotent (`dedup_hash`), so
/// re-running on every connect is safe and cheap.
///
/// Emits `annotations:sync-start` before and `annotations:sync-done` (with the
/// [`ingest::DeviceImportReport`]) / `annotations:sync-error` after, so the
/// status bar can show progress without a modal or stealing focus.
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
    // routes through `spawn_blocking` itself). Failure here means the device
    // came + went before the connect task fired — surface it like any other
    // sync error rather than panicking.
    let shared = match ensure_transport(&transport, &device).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[sidle/annsync] {serial}: open transport failed: {e:#}");
            let _ = app.emit("annotations:sync-error", format!("{e:#}"));
            return;
        }
    };

    let app_progress = app.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<ingest::DeviceImportReport> {
        let on_progress = |stage: &str, current: usize, total: usize, label: &str| {
            let _ = app_progress.emit(
                "annotations:sync-progress",
                crate::device::annotations::SyncProgress {
                    stage: stage.to_string(),
                    current,
                    total,
                    label: label.to_string(),
                },
            );
        };
        crate::device::annotations::import_device_annotations(
            &device,
            shared.as_ref(),
            &db,
            &paths,
            &on_progress,
        )
    })
    .await;

    // Same MTP-stall recovery as the Tauri commands: on any error the cached
    // session is likely talking to a wedged USB endpoint, so drop it. The
    // monitor itself doesn't auto-retry — the next user action (popover
    // refresh, manual re-sync) will trigger a fresh open via
    // `ensure_transport`.
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

/// One-shot MTP session read after the user plugs in: `GetStorageInfo` for
/// free/total bytes plus the firmware (`device_version`, captured when the
/// transport opened). The 2s detect poll deliberately doesn't open MTP
/// sessions — each open claims the USB interface and would race with a
/// user-initiated push. Instead we do it once on connect, write the result
/// back into `state.device`, and emit a follow-up `device:status` so the
/// popover swaps "—" for real numbers.
///
/// Staleness after a push/delete is the trade-off: the cached free/total
/// won't drop until the next reconnect. Acceptable for sidle's usage
/// pattern (occasional manual push); if it becomes annoying, the push
/// command can refresh in-place via `mtp_rs::Storage::refresh` before
/// dropping its transport.
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
