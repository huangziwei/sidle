//! [`Transport`] over MTP via mtp-rs.
//!
//! mtp-rs is async by design (every method on `Storage` returns a future).
//! Our [`Transport`] trait is sync because the mass-storage impl wraps
//! `std::fs`. We bridge with `futures::executor::block_on`:
//!
//! - mtp-rs is runtime-agnostic — its main `nusb` dep has neither the `tokio`
//!   nor the `smol` feature enabled, and the bulk-transfer futures are
//!   real waker-driven futures (see nusb's `device.rs::next_complete`)
//!   that any executor can drive. No tokio runtime needed.
//! - `tokio::runtime::Runtime::block_on` would panic from inside
//!   `spawn_blocking` ("can't block a runtime from within a runtime"); a
//!   plain futures executor sidesteps that.
//!
//! Atomicity: weaker than mass-storage. `SendObjectInfo` allocates the object
//! handle before `SendObject` streams the bytes, so a failed mid-upload can
//! leave a zero-or-partial-byte object visible to the device's indexer. Both
//! ways that happens are visible to the caller — a USB unplug, and
//! `Error::Cancelled`, which propagates — so nothing here hides a torn write.
//! Upload-as-`.partial` + `MoveObject` would close the window on devices that
//! advertise `SetObjectPropValue` (`MtpDevice::supports_rename()`).

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use futures::executor::block_on;
use mtp_rs::mtp::{MtpDevice, NewObjectInfo, Progress, Storage};
use mtp_rs::ptp::{ObjectHandle, ObjectInfo};

use crate::library::device::transport::{ChildFiles, TEntry, TPath, Transport};

pub struct MtpTransport {
    /// The bound MTP storage, behind a `Mutex` that serves two purposes:
    ///
    /// 1. Live free space. [`free_space`](Transport::free_space) re-reads
    ///    `GetStorageInfo` via [`Storage::refresh`], which needs `&mut self` —
    ///    but the [`Transport`] trait only hands out `&self`. The mutex is the
    ///    interior mutability that lets the popover show the real free/total
    ///    after a push or delete instead of a stale session-open snapshot.
    /// 2. Single-session op lock. Every on-wire operation holds the guard for
    ///    the whole `block_on`, so the high-level trait calls (which chain
    ///    multiple MTP round-trips: walk, list, upload) run one at a time and
    ///    concurrent push/delete from the UI can't interleave transaction IDs
    ///    against the one PTP session.
    storage: Mutex<Storage>,
    /// Firmware/OS version parsed from `system/version.txt`, read off the
    /// object tree at session open. `None` if the file wasn't reachable.
    firmware: Option<String>,
}

impl MtpTransport {
    /// Open the MTP device at `location_id` and bind the first storage. The
    /// bound `Storage` holds the PTP session (and the claimed USB interface)
    /// open for the transport's lifetime.
    pub fn open(location_id: u64) -> Result<Self> {
        let (storage, firmware) = block_on(async {
            let device = MtpDevice::open_by_location(location_id)
                .await
                .map_err(map_mtp_err)
                .context("open MTP device")?;
            let storage = device
                .storages()
                .await
                .map_err(map_mtp_err)
                .context("list MTP storages")?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("Kindle reports no MTP storage — try reconnecting"))?;
            // Firmware: the Kindle exposes its real filesystem over MTP, so read
            // `system/version.txt` — the same file mass-storage parses. The MTP
            // `GetDeviceInfo.device_version` field comes back empty on Kindle,
            // and none of its device properties carry the OS version. One extra
            // round-trip at session open, alongside the storage-info read.
            let firmware = read_firmware(&storage).await;
            Ok::<_, anyhow::Error>((storage, firmware))
        })?;
        // Free/total ride along inside `storage.info()` (captured at open);
        // `free_space` re-reads them live via `Storage::refresh` on demand.
        Ok(Self {
            storage: Mutex::new(storage),
            firmware,
        })
    }
}

/// Read and parse `system/version.txt` off the device for the firmware string.
/// Best-effort: any failure (file absent, download error, no version token in
/// the line) yields `None` — firmware is informational and must never block or
/// fail the session open.
async fn read_firmware(storage: &Storage) -> Option<String> {
    let path = TPath::parse(crate::library::device::VERSION_TXT_REL);
    let handle = resolve(storage, &path).await.ok().flatten()?;
    let bytes = storage.download(handle).await.ok()?;
    crate::library::device::parse_firmware(&String::from_utf8_lossy(&bytes))
}

/// Walk `path` segment-by-segment from storage root, returning the final
/// segment's MTP handle. `Ok(None)` if any segment is missing — callers
/// distinguish "absent" from real errors (USB drop, device busy).
async fn resolve(storage: &Storage, path: &TPath) -> Result<Option<ObjectHandle>> {
    let mut parent: Option<ObjectHandle> = None;
    for segment in path.segments() {
        let entries = storage.list_objects(parent).await.map_err(map_mtp_err)?;
        match entries.into_iter().find(|o| &o.filename == segment) {
            Some(obj) => parent = Some(obj.handle),
            None => return Ok(None),
        }
    }
    Ok(parent)
}

/// Walk `path`, creating any missing folders along the way. Returns the final
/// folder's handle. Errors if a path component exists as a file (not a folder).
async fn ensure_folder(storage: &Storage, path: &TPath) -> Result<Option<ObjectHandle>> {
    let mut parent: Option<ObjectHandle> = None;
    for segment in path.segments() {
        let entries = storage.list_objects(parent).await.map_err(map_mtp_err)?;
        let matched = entries.into_iter().find(|o| &o.filename == segment);
        parent = match matched {
            Some(obj) if obj.is_folder() => Some(obj.handle),
            Some(_) => bail!("MTP path component `{segment}` exists but isn't a folder"),
            None => Some(
                storage
                    .create_folder(parent, segment)
                    .await
                    .map_err(map_mtp_err)
                    .with_context(|| format!("create folder {segment}"))?,
            ),
        };
    }
    Ok(parent)
}

/// Upload `bytes` to `path` over MTP with byte-progress, overwriting any
/// existing object. Shared by [`MtpTransport::write_atomic`] (no-op progress)
/// and [`MtpTransport::copy_in_atomic_with_progress`]. The bytes are chunked so
/// the PTP send issues bounded bulk-OUT transfers (mirror of the 64 KiB
/// download chunking) and `on_progress` ticks repeatedly over a multi-MiB push
/// rather than once at the end. mtp-rs drives the callback from inside the send
/// stream, which is why it must be `Send + Sync`.
async fn upload_streamed(
    storage: &Storage,
    path: &TPath,
    bytes: &[u8],
    on_progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<()> {
    let parent_path = path.parent().unwrap_or_default();
    let name = path
        .name()
        .ok_or_else(|| anyhow!("MTP upload: empty path"))?;
    let parent = ensure_folder(storage, &parent_path).await?;

    // MTP has no atomic overwrite: delete-then-upload, accepting a tiny window
    // where neither the old nor new object is present — see the module header.
    // Push routes here for the real (color-cover, send) writes; the window is
    // between an existing same-name object's delete and the new upload, which
    // only happens on a re-push of an edited book.
    let entries = storage.list_objects(parent).await.map_err(map_mtp_err)?;
    if let Some(existing) = entries.into_iter().find(|o| o.filename == name) {
        storage
            .delete(existing.handle)
            .await
            .map_err(map_mtp_err)
            .with_context(|| format!("MTP delete {name} before overwrite"))?;
    }

    let total = bytes.len() as u64;
    let info = NewObjectInfo::file(name, total);
    let chunks: Vec<std::result::Result<Bytes, std::io::Error>> = bytes
        .chunks(256 * 1024)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    let stream = futures::stream::iter(chunks);
    on_progress(0, total);
    storage
        .upload_with_progress(parent, info, stream, |p: Progress| {
            on_progress(p.bytes_transferred, p.total_bytes.unwrap_or(total));
            ControlFlow::Continue(())
        })
        .await
        .map_err(map_mtp_err)
        .with_context(|| format!("MTP upload {name}"))?;
    Ok(())
}

impl Transport for MtpTransport {
    fn read(&self, path: &TPath) -> Result<Vec<u8>> {
        self.read_with_progress(path, &|_, _| {})
    }

    fn read_with_progress(&self, path: &TPath, on_progress: &dyn Fn(u64, u64)) -> Result<Vec<u8>> {
        let guard = self.storage.lock().expect("storage lock poisoned");
        let storage = &*guard;
        block_on(async {
            let handle = resolve(storage, path)
                .await?
                .ok_or_else(|| anyhow!("MTP read: object not found at `{path}`"))?;
            // Stream the object in bounded (64 KiB) chunks instead of `download()`,
            // which asks for the entire payload in ONE bulk-IN transfer. The Scribe
            // stalls a multi-MB single-transfer GetObject — the host sees IOKit
            // `kIOReturnNotResponding` (0xe00002ed) and the transport gets evicted
            // ("disconnect"). `download_stream` issues many small
            // `receive_bulk(64 KiB)` reads, each well within what the device serves.
            let mut dl = storage.download_stream(handle).await.map_err(map_mtp_err)?;
            let total = dl.size();
            let mut buf = Vec::with_capacity(total as usize);
            while let Some(chunk) = dl.next_chunk().await {
                buf.extend_from_slice(&chunk.map_err(map_mtp_err)?);
                on_progress(buf.len() as u64, total);
            }
            Ok(buf)
        })
    }

    fn write_atomic(&self, path: &TPath, bytes: &[u8]) -> Result<()> {
        let guard = self.storage.lock().expect("storage lock poisoned");
        block_on(upload_streamed(&guard, path, bytes, &|_, _| {}))
    }

    fn copy_in_atomic(&self, src_local: &Path, dest: &TPath) -> Result<()> {
        self.copy_in_atomic_with_progress(src_local, dest, &|_, _| {})
    }

    fn copy_in_atomic_with_progress(
        &self,
        src_local: &Path,
        dest: &TPath,
        on_progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<()> {
        // Buffer the local file fully before uploading. Real KFX files are
        // typically <30MB, occasionally up to ~100MB for image-heavy comics —
        // in-memory buffering keeps the upload path identical to `write_atomic`
        // and dodges streaming a `std::io::Read` across the async boundary.
        // `upload_streamed` then re-chunks the buffer so the SEND still streams
        // bounded bulk-OUT transfers (and ticks `on_progress`). Revisit if
        // pushes start failing on RAM pressure.
        let bytes =
            std::fs::read(src_local).with_context(|| format!("read {}", src_local.display()))?;
        let guard = self.storage.lock().expect("storage lock poisoned");
        block_on(upload_streamed(&guard, dest, &bytes, on_progress))
    }

    fn delete(&self, path: &TPath) -> Result<bool> {
        let guard = self.storage.lock().expect("storage lock poisoned");
        let storage = &*guard;
        block_on(async {
            let handle = match resolve(storage, path).await? {
                Some(h) => h,
                None => return Ok(false),
            };
            storage
                .delete(handle)
                .await
                .map_err(map_mtp_err)
                .with_context(|| format!("MTP delete {path}"))?;
            Ok(true)
        })
    }

    fn delete_dir(&self, path: &TPath) -> Result<bool> {
        // PTP `DeleteObject` is a single transaction — mtp-rs explicitly
        // doesn't loop, and device behavior on non-empty folders is undefined.
        // So we walk children, accumulate handles in preorder, and delete in
        // reverse so leaves go before their parents.
        let guard = self.storage.lock().expect("storage lock poisoned");
        let storage = &*guard;
        block_on(async {
            let root = match resolve(storage, path).await? {
                Some(h) => h,
                None => return Ok(false),
            };
            let mut stack = vec![root];
            let mut to_delete: Vec<ObjectHandle> = Vec::new();
            while let Some(h) = stack.pop() {
                to_delete.push(h);
                let children = storage.list_objects(Some(h)).await.map_err(map_mtp_err)?;
                for child in children {
                    stack.push(child.handle);
                }
            }
            for h in to_delete.into_iter().rev() {
                storage
                    .delete(h)
                    .await
                    .map_err(map_mtp_err)
                    .with_context(|| format!("MTP delete_dir {path} (handle {h:?})"))?;
            }
            Ok(true)
        })
    }

    fn read_files_in_children(
        &self,
        dir: &TPath,
        pick_dir: &dyn Fn(&str) -> bool,
        pick_file: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<ChildFiles>> {
        // Resolve `dir` ONCE, then list each matching child + read its matching
        // files BY HANDLE — no per-file re-walk of `dir` from the storage root (the
        // O(N²) that made sync scale badly with library size). `.yjr`/`.yjf`/`nbk`
        // are tiny; this is one `dir` resolve + a quick listing & small read per
        // child, all within the one open session.
        let guard = self.storage.lock().expect("storage lock poisoned");
        let storage = &*guard;
        block_on(async {
            let Some(parent) = resolve(storage, dir).await? else {
                return Ok(Vec::new());
            };
            let children = storage
                .list_objects(Some(parent))
                .await
                .map_err(map_mtp_err)?;
            let mut out: Vec<ChildFiles> = Vec::new();
            for child in children {
                if !child.is_folder() || !pick_dir(&child.filename) {
                    continue;
                }
                let files: Vec<ObjectInfo> = storage
                    .list_objects(Some(child.handle))
                    .await
                    .map_err(map_mtp_err)?
                    .into_iter()
                    .filter(|o| o.is_file() && pick_file(&o.filename))
                    .collect();
                let mut got_files: Vec<(String, Vec<u8>)> = Vec::new();
                for obj in files {
                    let mut dl = storage
                        .download_stream(obj.handle)
                        .await
                        .map_err(map_mtp_err)?;
                    let mut buf = Vec::with_capacity(dl.size() as usize);
                    while let Some(chunk) = dl.next_chunk().await {
                        buf.extend_from_slice(&chunk.map_err(map_mtp_err)?);
                    }
                    if !buf.is_empty() {
                        got_files.push((obj.filename, buf));
                    }
                }
                if !got_files.is_empty() {
                    out.push((child.filename.clone(), got_files));
                }
            }
            Ok(out)
        })
    }

    fn exists(&self, path: &TPath) -> Result<bool> {
        let guard = self.storage.lock().expect("storage lock poisoned");
        let storage = &*guard;
        block_on(async { Ok(resolve(storage, path).await?.is_some()) })
    }

    fn list(&self, dir: &TPath) -> Result<Vec<TEntry>> {
        let guard = self.storage.lock().expect("storage lock poisoned");
        let storage = &*guard;
        block_on(async {
            let parent = if dir.is_empty() {
                None
            } else {
                match resolve(storage, dir).await? {
                    Some(h) => Some(h),
                    None => return Ok(Vec::new()),
                }
            };
            let entries = storage.list_objects(parent).await.map_err(map_mtp_err)?;
            Ok(entries
                .into_iter()
                .map(|o| TEntry {
                    name: o.filename.clone(),
                    is_dir: o.is_folder(),
                    size: o.is_file().then_some(o.size),
                    // `list_objects` already did a GetObjectInfo per child, so the
                    // DateModified is in hand — no extra round-trip.
                    modified: o.modified.as_ref().map(mtp_modified_iso),
                })
                .collect())
        })
    }

    fn free_space(&self) -> Option<(u64, u64)> {
        // Live re-read: `Storage::refresh` issues a fresh `GetStorageInfo` over
        // the open session so the popover reflects the real free/total after a
        // push or delete, not the session-open snapshot. Best-effort — if the
        // round-trip fails we fall back to the last known `info()` (open-time or
        // a prior refresh) rather than regressing the display to "—".
        let mut guard = self.storage.lock().expect("storage lock poisoned");
        if let Err(e) = block_on(guard.refresh()) {
            eprintln!("[sidle/mtp] free_space refresh failed, using cached: {e}");
        }
        let info = guard.info();
        Some((info.free_space_bytes, info.max_capacity))
    }

    fn firmware(&self) -> Option<String> {
        self.firmware.clone()
    }

    fn display_path(&self, path: &TPath) -> String {
        // Audit-log rendering. Prefix `mtp:` so `device_history` rows can be
        // visually distinguished from mass-storage's full filesystem paths.
        format!("mtp:{path}")
    }
}

/// Map mtp-rs errors to anyhow with actionable text for the common
/// "another app owns the device" case.
/// MTP `DateModified` (`YYYYMMDDThhmmss`, the device's wall clock) → a naive ISO
/// string `YYYY-MM-DDTHH:MM:SS`. Deliberately no timezone: the value renders as
/// the Kindle's own clock (matching Finder / Image Capture), which is what the
/// user expects for "last edited on the device".
fn mtp_modified_iso(dt: &mtp_rs::ptp::DateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
    )
}

fn map_mtp_err(err: mtp_rs::Error) -> anyhow::Error {
    if err.is_exclusive_access() {
        anyhow!(
            "Kindle is in use by another app. Quit Image Capture, OpenMTP, \
             Android File Transfer, or Calibre and try again. (underlying: {err})"
        )
    } else {
        anyhow!(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_access_error_gets_actionable_text() {
        // Same wording nusb produces on macOS when another process has
        // claimed the device — see mtp-rs's `error.rs` tests.
        let io_err = std::io::Error::other("could not be opened for exclusive access");
        let mapped = map_mtp_err(mtp_rs::Error::Io(io_err));
        let msg = format!("{mapped:#}");
        assert!(
            msg.contains("Quit Image Capture"),
            "user-actionable hint missing from busy error: {msg}"
        );
    }

    #[test]
    fn ordinary_io_error_passes_through() {
        let io_err = std::io::Error::other("bus reset");
        let mapped = map_mtp_err(mtp_rs::Error::Io(io_err));
        let msg = format!("{mapped:#}");
        // No exclusive-access hint for unrelated IO errors.
        assert!(
            !msg.contains("Image Capture"),
            "false positive busy hint: {msg}"
        );
        assert!(msg.contains("bus reset"));
    }
}
