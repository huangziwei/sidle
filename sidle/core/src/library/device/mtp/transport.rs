//! [`Transport`] over MTP via mtp-rs.
//!
//! mtp-rs is async by design (every method on `Storage` returns a future).

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
