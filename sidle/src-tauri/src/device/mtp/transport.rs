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

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use futures::executor::block_on;
use mtp_rs::mtp::{MtpDevice, NewObjectInfo, Storage};
use mtp_rs::ptp::ObjectHandle;

use crate::device::transport::{TEntry, TPath, Transport};

pub struct MtpTransport {
    storage: Storage,
    /// Serializes all on-wire MTP operations against one session — the plan's
    /// "single-session lock". mtp-rs already serializes PTP operations
    /// internally, but we still want the *high-level* trait calls (which
    /// chain multiple MTP round-trips: walk, list, upload) to run one at a
    /// time so concurrent push/delete from the UI doesn't interleave.
    op_lock: Mutex<()>,
    /// `(free, total)` snapshot taken at session open. Real-time refresh
    /// would need an extra `GetStorageInfo` round-trip per call; Phase 4 can
    /// expose a refresh hook if the UI needs live numbers.
    free_at_open: u64,
    total_capacity: u64,
}

impl MtpTransport {
    /// Open the MTP device at `location_id` and bind the first storage.
    pub fn open(location_id: u64) -> Result<Self> {
        let (storage, free, total) = block_on(async {
            let device = MtpDevice::open_by_location(location_id)
                .await
                .map_err(map_mtp_err)
                .context("open MTP device")?;
            let storages = device
                .storages()
                .await
                .map_err(map_mtp_err)
                .context("list MTP storages")?;
            let storage = storages
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("Kindle reports no MTP storage — try reconnecting"))?;
            let free = storage.info().free_space_bytes;
            let total = storage.info().max_capacity;
            Ok::<_, anyhow::Error>((storage, free, total))
        })?;

        Ok(Self {
            storage,
            op_lock: Mutex::new(()),
            free_at_open: free,
            total_capacity: total,
        })
    }

    /// Walk `path` segment-by-segment from storage root, returning the final
    /// segment's MTP handle. `Ok(None)` if any segment is missing — callers
    /// distinguish "absent" from real errors (USB drop, device busy).
    async fn resolve(&self, path: &TPath) -> Result<Option<ObjectHandle>> {
        let mut parent: Option<ObjectHandle> = None;
        for segment in path.segments() {
            let entries = self
                .storage
                .list_objects(parent)
                .await
                .map_err(map_mtp_err)?;
            match entries.into_iter().find(|o| &o.filename == segment) {
                Some(obj) => parent = Some(obj.handle),
                None => return Ok(None),
            }
        }
        Ok(parent)
    }

    /// Walk `path`, creating any missing folders along the way. Returns the
    /// final folder's handle. Errors if a path component exists as a file
    /// (not a folder).
    async fn ensure_folder(&self, path: &TPath) -> Result<Option<ObjectHandle>> {
        let mut parent: Option<ObjectHandle> = None;
        for segment in path.segments() {
            let entries = self
                .storage
                .list_objects(parent)
                .await
                .map_err(map_mtp_err)?;
            let matched = entries.into_iter().find(|o| &o.filename == segment);
            parent = match matched {
                Some(obj) if obj.is_folder() => Some(obj.handle),
                Some(_) => {
                    return Err(anyhow!(
                        "MTP path component `{segment}` exists but isn't a folder"
                    ));
                }
                None => Some(
                    self.storage
                        .create_folder(parent, segment)
                        .await
                        .map_err(map_mtp_err)
                        .with_context(|| format!("create folder {segment}"))?,
                ),
            };
        }
        Ok(parent)
    }
}

impl Transport for MtpTransport {
    fn read(&self, path: &TPath) -> Result<Vec<u8>> {
        let _g = self.op_lock.lock().expect("op_lock poisoned");
        block_on(async {
            let handle = self
                .resolve(path)
                .await?
                .ok_or_else(|| anyhow!("MTP read: object not found at `{path}`"))?;
            self.storage.download(handle).await.map_err(map_mtp_err)
        })
    }

    fn write_atomic(&self, path: &TPath, bytes: &[u8]) -> Result<()> {
        let _g = self.op_lock.lock().expect("op_lock poisoned");
        block_on(async {
            let parent_path = path.parent().unwrap_or_default();
            let name = path
                .name()
                .ok_or_else(|| anyhow!("MTP write_atomic: empty path"))?;
            let parent = self.ensure_folder(&parent_path).await?;

            // MTP has no atomic overwrite. Delete-then-upload is simpler than
            // upload-as-temp + rename and keeps the code symmetric with the
            // pristine-write case. Tradeoff: a tiny window where neither the
            // old nor the new object is present. Push routes through
            // `copy_in_atomic`, not here — `write_atomic` is only used for
            // small auxiliary writes today (currently none in the new
            // scan-based model), so the window is academic.
            let entries = self
                .storage
                .list_objects(parent)
                .await
                .map_err(map_mtp_err)?;
            if let Some(existing) = entries.into_iter().find(|o| o.filename == name) {
                self.storage
                    .delete(existing.handle)
                    .await
                    .map_err(map_mtp_err)
                    .with_context(|| format!("MTP delete {name} before overwrite"))?;
            }

            let info = NewObjectInfo::file(name, bytes.len() as u64);
            let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(
                Bytes::copy_from_slice(bytes),
            )]);
            self.storage
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
        let _g = self.op_lock.lock().expect("op_lock poisoned");
        block_on(async {
            let handle = match self.resolve(path).await? {
                Some(h) => h,
                None => return Ok(false),
            };
            self.storage
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
        let _g = self.op_lock.lock().expect("op_lock poisoned");
        block_on(async {
            let root = match self.resolve(path).await? {
                Some(h) => h,
                None => return Ok(false),
            };
            let mut stack = vec![root];
            let mut to_delete: Vec<ObjectHandle> = Vec::new();
            while let Some(h) = stack.pop() {
                to_delete.push(h);
                let children = self
                    .storage
                    .list_objects(Some(h))
                    .await
                    .map_err(map_mtp_err)?;
                for child in children {
                    stack.push(child.handle);
                }
            }
            for h in to_delete.into_iter().rev() {
                self.storage
                    .delete(h)
                    .await
                    .map_err(map_mtp_err)
                    .with_context(|| format!("MTP delete_dir {path} (handle {h:?})"))?;
            }
            Ok(true)
        })
    }

    fn exists(&self, path: &TPath) -> Result<bool> {
        let _g = self.op_lock.lock().expect("op_lock poisoned");
        block_on(async { Ok(self.resolve(path).await?.is_some()) })
    }

    fn list(&self, dir: &TPath) -> Result<Vec<TEntry>> {
        let _g = self.op_lock.lock().expect("op_lock poisoned");
        block_on(async {
            let parent = if dir.is_empty() {
                None
            } else {
                match self.resolve(dir).await? {
                    Some(h) => Some(h),
                    None => return Ok(Vec::new()),
                }
            };
            let entries = self
                .storage
                .list_objects(parent)
                .await
                .map_err(map_mtp_err)?;
            Ok(entries
                .into_iter()
                .map(|o| TEntry {
                    name: o.filename.clone(),
                    is_dir: o.is_folder(),
                    size: o.is_file().then_some(o.size),
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
