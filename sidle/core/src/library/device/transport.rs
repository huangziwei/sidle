//! Transport abstraction over the on-device filesystem.
//!
//! Mass-storage Kindles expose a real FAT/exFAT volume — `std::fs` calls
//! against `/Volumes/Kindle/...` Just Work. MTP-class Kindles (Scribe and
//! everything 2024+) expose a tree of objects accessed over USB through
//! Apple's `IOUSBHost`; the same logical paths (`documents/Sidle/foo.kfx`)
//! map to a chain of MTP object IDs. Pushing, listing, deleting books and
//! their `.sdr/` sidecars all go through this trait so the layers above
//! don't have to care which world they're in.

use std::path::Path;

use anyhow::Result;

/// Logical path on the device, e.g. `documents/Sidle/foo.kfx`. Path components
/// are kept as separate segments so each transport can map them to its own
/// namespace (filesystem path or MTP object-ID chain) without re-parsing.
///
/// Roots and leading slashes are normalized away — every transport interprets
/// these paths relative to its own notion of the storage root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct TPath {
    segments: Vec<String>,
}

impl TPath {
    /// Empty path. Used as the root for `list`/`exists` checks against a
    /// transport's storage root.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a slash-delimited string into segments. Empty segments (leading,
    /// trailing, or doubled-up slashes) are dropped.
    pub fn parse(s: &str) -> Self {
        Self {
            segments: s
                .split('/')
                .filter(|seg| !seg.is_empty())
                .map(String::from)
                .collect(),
        }
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    pub fn name(&self) -> Option<&str> {
        self.segments.last().map(|s| s.as_str())
    }

    pub fn parent(&self) -> Option<TPath> {
        if self.segments.is_empty() {
            None
        } else {
            Some(TPath {
                segments: self.segments[..self.segments.len() - 1].to_vec(),
            })
        }
    }

    pub fn join(&self, name: &str) -> TPath {
        let mut segments = self.segments.clone();
        segments.push(name.to_string());
        TPath { segments }
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}

impl std::fmt::Display for TPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.segments.join("/"))
    }
}

/// One immediate child of a directory listing.
#[derive(Debug, Clone)]
pub struct TEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    /// On-device "Date Modified" as a naive wall-clock ISO string
    /// (`YYYY-MM-DDTHH:MM:SS`), when the transport reports one. MTP carries it in
    /// the `GetObjectInfo` the listing already fetched; mass-storage reads the
    /// filesystem mtime. `None` when unavailable. Used as a notebook's
    /// `updated_at` (the Kindle advances it only on a real edit).
    pub modified: Option<String>,
}

/// One matched child directory's haul from [`Transport::read_files_in_children`]:
/// the child's name paired with its picked `(filename, bytes)` entries.
pub type ChildFiles = (String, Vec<(String, Vec<u8>)>);

/// On-device IO surface. Each method is logically atomic from the caller's
/// view: a partial `write_atomic` either lands fully or leaves the prior
/// object untouched, a `delete` is a no-op when the object is already gone,
/// and so on. Implementations decide how to get there (filesystem rename on
/// mass-storage; `SendObjectInfo`/`SendObject` on MTP).
pub trait Transport: Send + Sync {
    fn read(&self, path: &TPath) -> Result<Vec<u8>>;

    /// Like [`read`](Self::read), but reports progress as the read advances:
    /// `on_progress(bytes_read_so_far, total_bytes)` is invoked one or more
    /// times, ending with `bytes_read_so_far == total_bytes`. For transports
    /// whose read is slow enough to look hung without a live counter — MTP pulls
    /// a multi-MiB book over USB in many small bulk-IN chunks, which takes
    /// seconds. The default reads the whole object via [`read`](Self::read) and
    /// reports a single final tick; only MTP overrides it. `total` of 0 means the
    /// size wasn't known up front.
    fn read_with_progress(&self, path: &TPath, on_progress: &dyn Fn(u64, u64)) -> Result<Vec<u8>> {
        let bytes = self.read(path)?;
        let n = bytes.len() as u64;
        on_progress(n, n);
        Ok(bytes)
    }

    /// Atomic byte-write primitive of the trait's CRUD surface. Exercised by
    /// the mass-storage tests (round-trip, parent creation) and available for
    /// small auxiliary writes; the book-push path streams through
    /// [`copy_in_atomic_with_progress`](Self::copy_in_atomic_with_progress)
    /// instead, so there's no non-test caller in the lib build today.
    #[allow(dead_code)]
    fn write_atomic(&self, path: &TPath, bytes: &[u8]) -> Result<()>;
    /// Copy a local file into the transport at `dest`. Atomic on success;
    /// no observable `dest` if interrupted mid-copy.
    fn copy_in_atomic(&self, src_local: &Path, dest: &TPath) -> Result<()>;

    /// Like [`copy_in_atomic`](Self::copy_in_atomic), but reports progress as
    /// the write advances: `on_progress(bytes_written, total_bytes)` fires one
    /// or more times, ending at `bytes_written == total_bytes`. The mirror of
    /// [`read_with_progress`](Self::read_with_progress) for the send side — a
    /// multi-MiB push over MTP takes seconds, so the UI needs a live byte
    /// counter rather than a single "done" toast. The callback is `Send + Sync`
    /// because MTP drives it from inside an upload stream the PTP layer requires
    /// be `Send`. The default copies via [`copy_in_atomic`](Self::copy_in_atomic)
    /// and reports one final tick; mass-storage and MTP override it to stream.
    fn copy_in_atomic_with_progress(
        &self,
        src_local: &Path,
        dest: &TPath,
        on_progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<()> {
        self.copy_in_atomic(src_local, dest)?;
        let n = std::fs::metadata(src_local).map(|m| m.len()).unwrap_or(0);
        on_progress(n, n);
        Ok(())
    }
    /// Returns `Ok(false)` when the object was already absent.
    fn delete(&self, path: &TPath) -> Result<bool>;
    /// Recursively delete a directory and its contents. `Ok(false)` when the
    /// directory was already absent. Used to wipe the Kindle-created
    /// `<basename>.sdr/` sidecar (reading progress, annotations, highlights)
    /// next to a `.kfx` on remove.
    fn delete_dir(&self, path: &TPath) -> Result<bool>;
    /// For each immediate child directory of `dir` whose name passes `pick_dir`,
    /// read every file in it whose name passes `pick_file`. Resolves `dir` (and
    /// each child) only ONCE. Returns `(child_dir_name, Vec<(file_name, bytes)>)`,
    /// omitting children that have no readable / non-empty matching file.
    ///
    /// The point is MTP scaling: a path-based [`read`](Self::read) re-walks `dir`
    /// from the storage root on every call, so reading files under N children is
    /// O(N × dir-size) — quadratic in library size. This resolves `dir` once and
    /// reads by handle in one session, so it's linear. Used for both ink
    /// (`.notebooks/<id>/nbk`) and annotations
    /// (`documents/Sidle/<book>.sdr/{.yjr,.yjf}`). Default impl (mass-storage:
    /// local fs, cheap) just lists + `read`s each.
    fn read_files_in_children(
        &self,
        dir: &TPath,
        pick_dir: &dyn Fn(&str) -> bool,
        pick_file: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<ChildFiles>> {
        let mut out = Vec::new();
        for entry in self.list(dir)? {
            if !entry.is_dir || !pick_dir(&entry.name) {
                continue;
            }
            let child = dir.join(&entry.name);
            let mut files = Vec::new();
            for f in self.list(&child).unwrap_or_default() {
                if !f.is_dir && pick_file(&f.name) {
                    match self.read(&child.join(&f.name)) {
                        Ok(bytes) if !bytes.is_empty() => files.push((f.name, bytes)),
                        _ => {}
                    }
                }
            }
            if !files.is_empty() {
                out.push((entry.name, files));
            }
        }
        Ok(out)
    }
    /// Existence probe. Unused by the scan-based push/delete path, but kept
    /// as a transport primitive — tests rely on it and a future "is this
    /// file still there" UI check could too.
    #[allow(dead_code)]
    fn exists(&self, path: &TPath) -> Result<bool>;
    /// Immediate children of `dir`. Empty when `dir` is absent.
    fn list(&self, dir: &TPath) -> Result<Vec<TEntry>>;
    /// `(free, total)` bytes when known. None when the transport has no
    /// usable storage-info call (or it failed at this moment).
    fn free_space(&self) -> Option<(u64, u64)>;

    /// Firmware/OS version string when the transport learns it at session open
    /// (MTP reads `system/version.txt` off the object tree). Mass-storage gets
    /// firmware from the same file at detect time, so it keeps the default
    /// `None` here.
    fn firmware(&self) -> Option<String> {
        None
    }

    /// Human-readable rendering of `path` for audit logs. Mass-storage
    /// renders the full filesystem path so existing `device_history.device_path`
    /// rows stay byte-identical; MTP can pick whatever's useful.
    fn display_path(&self, path: &TPath) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tpath_parse_drops_empty_segments() {
        let p = TPath::parse("/documents//Sidle/foo.kfx/");
        assert_eq!(
            p.segments(),
            &[
                "documents".to_string(),
                "Sidle".to_string(),
                "foo.kfx".to_string()
            ]
        );
    }

    #[test]
    fn tpath_parent_and_name() {
        let p = TPath::parse("documents/Sidle/foo.kfx");
        assert_eq!(p.name(), Some("foo.kfx"));
        let parent = p.parent().expect("has parent");
        assert_eq!(parent.name(), Some("Sidle"));
        assert_eq!(format!("{parent}"), "documents/Sidle");
    }

    #[test]
    fn tpath_join_appends_segment() {
        let p = TPath::parse("documents/Sidle").join("foo.kfx");
        assert_eq!(format!("{p}"), "documents/Sidle/foo.kfx");
    }

    #[test]
    fn tpath_empty_round_trip() {
        let empty = TPath::new();
        assert!(empty.is_empty());
        assert_eq!(empty.parent(), None);
        assert_eq!(empty.name(), None);
        assert_eq!(format!("{empty}"), "");
    }
}
