//! [`Transport`] over MTP via mtp-rs.
//!
//! mtp-rs is async by design (every method on `Storage` returns a future).
//! Our [`Transport`] trait is sync because the mass-storage impl wraps
//! `std::fs`. We bridge with `futures::executor::block_on`:
//!
//! - mtp-rs is runtime-agnostic — its main `nusb` dep has neither the `tokio`
//!   nor the `smol` feature enabled, and the bulk-transfer futures are
//!   real waker-driven futures (see `ref/nusb/src/device.rs::next_complete`)
//!   that any executor can drive. No tokio runtime needed.
//! - `tokio::runtime::Runtime::block_on` would panic from inside
//!   `spawn_blocking` ("can't block a runtime from within a runtime"); a
//!   plain futures executor sidesteps that.
//!
//! Atomicity: weaker than mass-storage. `SendObjectInfo` allocates the object
//! handle before `SendObject` streams the bytes, so a failed mid-upload can
//! leave a zero-or-partial-byte object visible to the device's indexer.
//! Phase 3 accepts this — the failure modes are (a) USB unplug, which the
//! user sees, and (b) `Error::Cancelled`, which we propagate. A Phase 4
//! polish item is upload-as-`.partial` + `MoveObject`/rename when the device
//! advertises `SetObjectPropValue` support (`MtpDevice::supports_rename()`).

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use futures::executor::block_on;
use mtp_rs::mtp::{MtpDevice, NewObjectInfo, Storage};
use mtp_rs::ptp::{ObjectHandle, ObjectInfo};

use crate::device::transport::{TEntry, TPath, Transport};

/// Largest run of object bytes Sidle pulls within ONE MTP session before
/// closing and reopening it. The Kindle Scribe's MTP responder serves ~8 MiB of
/// object data per PTP session and then stalls its bulk pipe — fatally, until
/// the cable is replugged (IOKit `kIOReturnNotResponding` / 0xe00002ed). A fresh
/// session resets that budget, so large files are read in sub-8-MiB segments,
/// reopening the session between each. 6 MiB keeps a safe margin. Verified
/// on-device: a 15 MiB KFX pulls cleanly in 3 sessions; a single >8 MiB read,
/// whole or ranged, wedges the device.
const SESSION_READ_BUDGET: u64 = 6 << 20;

/// One `GetPartialObject` request size. Small enough that no single ranged read
/// trips the responder; [`SESSION_READ_BUDGET`] is the real limit.
const PARTIAL_CHUNK: u32 = 1 << 20;

/// A live MTP session: a device handle plus its bound storage. `Storage` keeps
/// its own `Arc` to the session, so closing it (resetting the device's
/// per-session transfer budget) requires dropping BOTH — hence both live here
/// and a `Session` drop ends the session and releases the USB interface.
struct Session {
    _device: MtpDevice,
    storage: Storage,
}

pub struct MtpTransport {
    /// IOKit location of the device, so the session can be reopened (after a
    /// read segment closes it) without re-enumerating — a USB reset would
    /// unmount the device; a plain reopen does not.
    location_id: u64,
    /// The live session, lazily (re)opened. `read()` closes it between segments
    /// to reset the device's ~8 MiB budget; the next op reopens it. The `Mutex`
    /// also serializes every high-level operation against the one device — two
    /// concurrent sessions would collide on the exclusively-claimed interface —
    /// so it doubles as the op-lock.
    session: Mutex<Option<Session>>,
    /// `(free, total)` snapshot taken at first open.
    free_at_open: u64,
    total_capacity: u64,
}

impl MtpTransport {
    /// Open the MTP device at `location_id` and bind the first storage.
    pub fn open(location_id: u64) -> Result<Self> {
        let session = open_session(location_id)?;
        let info = session.storage.info();
        let (free_at_open, total_capacity) = (info.free_space_bytes, info.max_capacity);
        Ok(Self {
            location_id,
            session: Mutex::new(Some(session)),
            free_at_open,
            total_capacity,
        })
    }
}

/// Open a fresh device + storage (a new PTP session). Retries briefly: right
/// after a prior session drops, the interface can still be releasing, so a
/// back-to-back reopen (the read-segment path) may need a beat.
fn open_session(location_id: u64) -> Result<Session> {
    block_on(async {
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(250));
            }
            match try_open_session(location_id).await {
                Ok(s) => return Ok(s),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("open MTP session failed")))
    })
}

async fn try_open_session(location_id: u64) -> Result<Session> {
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
    Ok(Session {
        _device: device,
        storage,
    })
}

/// Borrow the live session's storage, opening one if the slot is empty.
fn session_storage(slot: &mut Option<Session>, location_id: u64) -> Result<&Storage> {
    if slot.is_none() {
        *slot = Some(open_session(location_id)?);
    }
    Ok(&slot.as_ref().expect("just opened").storage)
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

/// Like [`resolve`], but returns the leaf's full [`ObjectInfo`] (handle + size).
/// `read` needs the size to know how many bytes to pull across its segments.
async fn resolve_object(storage: &Storage, path: &TPath) -> Result<Option<ObjectInfo>> {
    let segments = path.segments();
    let mut parent: Option<ObjectHandle> = None;
    for (i, segment) in segments.iter().enumerate() {
        let entries = storage.list_objects(parent).await.map_err(map_mtp_err)?;
        match entries.into_iter().find(|o| &o.filename == segment) {
            Some(obj) if i + 1 == segments.len() => return Ok(Some(obj)),
            Some(obj) => parent = Some(obj.handle),
            None => return Ok(None),
        }
    }
    Ok(None)
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

impl Transport for MtpTransport {
    fn read(&self, path: &TPath) -> Result<Vec<u8>> {
        let mut slot = self.session.lock().expect("session lock poisoned");
        let mut buf: Vec<u8> = Vec::new();
        loop {
            // Run each segment in a FRESH session. The Scribe stalls (fatally,
            // until replug) past ~8 MiB of object data per session — true of a
            // whole `GetObject` AND of cumulative `GetPartialObject` ranges — but
            // a reopened session resets that budget. Closing the slot here drops
            // the prior `Session` (both Arcs → session closed, interface
            // released) so `session_storage` opens a clean one. We always start
            // fresh so a session left part-used by an earlier op can't push us
            // over mid-read.
            *slot = None;
            let storage = session_storage(&mut slot, self.location_id)?;
            let done = block_on(async {
                // Re-resolve in this session — object handles are session-scoped;
                // the byte offset handed to `GetPartialObject` is absolute, so it
                // stays valid across the reopen.
                let info = resolve_object(storage, path)
                    .await?
                    .ok_or_else(|| anyhow!("MTP read: object not found at `{path}`"))?;
                let total = info.size;
                let start = buf.len() as u64;
                let mut got = start;
                while got < total && got - start < SESSION_READ_BUDGET {
                    let want = PARTIAL_CHUNK.min((total - got) as u32);
                    let bytes = storage
                        .download_partial(info.handle, got, want)
                        .await
                        .map_err(map_mtp_err)?;
                    if bytes.is_empty() {
                        bail!("MTP read: device returned an empty range at offset {got}");
                    }
                    buf.extend_from_slice(&bytes);
                    got += bytes.len() as u64;
                }
                Ok::<bool, anyhow::Error>(got >= total)
            })?;
            if done {
                return Ok(buf);
            }
        }
    }

    fn write_atomic(&self, path: &TPath, bytes: &[u8]) -> Result<()> {
        let mut slot = self.session.lock().expect("session lock poisoned");
        let storage = session_storage(&mut slot, self.location_id)?;
        block_on(async {
            let parent_path = path.parent().unwrap_or_default();
            let name = path
                .name()
                .ok_or_else(|| anyhow!("MTP write_atomic: empty path"))?;
            let parent = ensure_folder(storage, &parent_path).await?;

            // MTP has no atomic overwrite. Delete-then-upload is simpler than
            // upload-as-temp + rename and keeps the code symmetric with the
            // pristine-write case. Tradeoff: a tiny window where neither the
            // old nor the new object is present. Push routes through
            // `copy_in_atomic`, not here — `write_atomic` is only used for
            // small auxiliary writes today (currently none in the new
            // scan-based model), so the window is academic.
            let entries = storage.list_objects(parent).await.map_err(map_mtp_err)?;
            if let Some(existing) = entries.into_iter().find(|o| o.filename == name) {
                storage
                    .delete(existing.handle)
                    .await
                    .map_err(map_mtp_err)
                    .with_context(|| format!("MTP delete {name} before overwrite"))?;
            }

            let info = NewObjectInfo::file(name, bytes.len() as u64);
            let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(
                Bytes::copy_from_slice(bytes),
            )]);
            storage
                .upload(parent, info, stream)
                .await
                .map_err(map_mtp_err)
                .with_context(|| format!("MTP upload {name}"))?;
            Ok(())
        })
    }

    fn copy_in_atomic(&self, src_local: &Path, dest: &TPath) -> Result<()> {
        // Buffer the local file fully before uploading. Real KFX files are
        // typically <30MB, occasionally up to ~100MB for image-heavy comics —
        // in-memory buffering keeps the upload code path identical to
        // `write_atomic` and dodges the cross-async-boundary complexity of
        // streaming a `std::io::Read` into a `futures::Stream`. Revisit if
        // pushes start failing on RAM pressure.
        let bytes = std::fs::read(src_local)
            .with_context(|| format!("read {}", src_local.display()))?;
        self.write_atomic(dest, &bytes)
    }

    fn delete(&self, path: &TPath) -> Result<bool> {
        let mut slot = self.session.lock().expect("session lock poisoned");
        let storage = session_storage(&mut slot, self.location_id)?;
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
        let mut slot = self.session.lock().expect("session lock poisoned");
        let storage = session_storage(&mut slot, self.location_id)?;
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

    fn exists(&self, path: &TPath) -> Result<bool> {
        let mut slot = self.session.lock().expect("session lock poisoned");
        let storage = session_storage(&mut slot, self.location_id)?;
        block_on(async { Ok(resolve(storage, path).await?.is_some()) })
    }

    fn list(&self, dir: &TPath) -> Result<Vec<TEntry>> {
        let mut slot = self.session.lock().expect("session lock poisoned");
        let storage = session_storage(&mut slot, self.location_id)?;
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
        // Snapshot from session open. Live refresh would require another
        // `GetStorageInfo` round-trip — Phase 4 can wire that to a refresh
        // button if the static value drifts noticeably in practice.
        Some((self.free_at_open, self.total_capacity))
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
        // claimed the device — see ref/mtp-rs/src/error.rs tests.
        let io_err =
            std::io::Error::other("could not be opened for exclusive access");
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
        assert!(!msg.contains("Image Capture"), "false positive busy hint: {msg}");
        assert!(msg.contains("bus reset"));
    }
}
