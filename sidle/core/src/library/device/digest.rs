//! Content hashes of files on this machine, kept across calls.
//!
//! A push and a status check both need the sha256 of every file the fleet
//! installs. Hashing on each call reads the whole tree, and most of a tree is
//! files no build touches — a vendored subtree, a font set.
//!
//! [`DigestCache`] hashes a file once per version of it. A [`Stamp`] — mtime,
//! size and inode, from one `metadata` call — is what a recorded hash is held
//! against.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::deploy::sha256_bytes;

/// What one `metadata` call says about a file. Any change to any field means
/// the recorded hash describes bytes that are gone: a rewrite in place moves
/// `mtime_ms`, a rename over the path moves `inode`, and a truncation moves
/// `size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub mtime_ms: u64,
    pub size: u64,
    pub inode: u64,
}

impl Stamp {
    /// The stamp of `file`, or `None` when it does not exist.
    pub fn of(file: &Path) -> Option<Self> {
        let md = std::fs::metadata(file).ok()?;
        let mtime_ms = md
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;
        Some(Self {
            mtime_ms,
            size: md.len(),
            inode: md.ino(),
        })
    }
}

/// A file's hash and length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDigest {
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    stamp: Stamp,
    sha256: String,
}

/// Hashes keyed by absolute path, backed by one JSON file.
///
/// [`Self::open`] and [`Self::save`] bracket a run of lookups; a caller that
/// hashes nothing new writes nothing back.
#[derive(Debug)]
pub struct DigestCache {
    path: PathBuf,
    entries: BTreeMap<PathBuf, Entry>,
    dirty: bool,
    hashed: usize,
}

impl DigestCache {
    /// Read the cache at `path`. An absent, unreadable or unparseable file
    /// opens as empty, and every lookup hashes.
    pub fn open(path: &Path) -> Self {
        let entries = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path: path.to_path_buf(),
            entries,
            dirty: false,
            hashed: 0,
        }
    }

    /// A cache no lookup can persist, for a caller with nowhere to keep one.
    pub fn ephemeral() -> Self {
        Self {
            path: PathBuf::new(),
            entries: BTreeMap::new(),
            dirty: false,
            hashed: 0,
        }
    }

    /// Write the cache back when a lookup hashed something.
    pub fn save(&self) -> Result<()> {
        if !self.dirty || self.path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let json = serde_json::to_vec(&self.entries).context("serialize the digest cache")?;
        super::deploy::atomic_write(&self.path, &json)
    }

    /// The digest of `file`, hashing it when no entry matches its stamp.
    /// `None` when the file does not exist.
    pub fn of(&mut self, file: &Path) -> Result<Option<FileDigest>> {
        let Some(stamp) = Stamp::of(file) else {
            return Ok(None);
        };
        if let Some(entry) = self.entries.get(file)
            && entry.stamp == stamp
        {
            return Ok(Some(FileDigest {
                sha256: entry.sha256.clone(),
                size: stamp.size,
            }));
        }
        let bytes = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
        let sha256 = sha256_bytes(&bytes);
        self.entries.insert(
            file.to_path_buf(),
            Entry {
                stamp,
                sha256: sha256.clone(),
            },
        );
        self.dirty = true;
        self.hashed += 1;
        Ok(Some(FileDigest {
            sha256,
            size: stamp.size,
        }))
    }

    /// How many files this cache has read since it was opened.
    pub fn hashed(&self) -> usize {
        self.hashed
    }

    /// How many paths the cache holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// Rewrite `path` in place, moving its mtime past what a stamp recorded.
    fn rewrite(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        let f = std::fs::File::options().write(true).open(path).unwrap();
        let ahead = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        f.set_times(std::fs::FileTimes::new().set_modified(ahead))
            .unwrap();
    }

    #[test]
    fn a_second_lookup_of_an_untouched_file_reads_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("bin/app");
        write(&file, b"v1");
        let mut cache = DigestCache::ephemeral();

        let first = cache.of(&file).unwrap().unwrap();
        assert_eq!(first.sha256, sha256_bytes(b"v1"));
        assert_eq!(first.size, 2);

        // Unreadable, with `metadata` answering: a hit takes the stamp and
        // opens nothing.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(cache.of(&file).unwrap().unwrap(), first);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn a_rewritten_file_is_hashed_again() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("bin/app");
        write(&file, b"v1");
        let mut cache = DigestCache::ephemeral();
        cache.of(&file).unwrap();

        rewrite(&file, b"v2");
        assert_eq!(
            cache.of(&file).unwrap().unwrap().sha256,
            sha256_bytes(b"v2")
        );
    }

    #[test]
    fn a_file_replaced_by_rename_is_hashed_again() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("bin/app");
        write(&file, b"aa");
        let mut cache = DigestCache::ephemeral();
        cache.of(&file).unwrap();
        let before = Stamp::of(&file).unwrap();

        // A build that writes a temp and renames over the path: same length,
        // and the mtime can land in the same millisecond. `inode` is what moves.
        let temp = tmp.path().join("bin/app.tmp");
        write(&temp, b"bb");
        let f = std::fs::File::options().write(true).open(&temp).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(before.mtime_ms),
        ))
        .unwrap();
        std::fs::rename(&temp, &file).unwrap();

        let after = Stamp::of(&file).unwrap();
        assert_eq!(after.mtime_ms, before.mtime_ms);
        assert_eq!(after.size, before.size);
        assert_ne!(after.inode, before.inode);
        assert_eq!(
            cache.of(&file).unwrap().unwrap().sha256,
            sha256_bytes(b"bb")
        );
    }

    #[test]
    fn a_missing_file_has_no_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = DigestCache::ephemeral();
        assert_eq!(cache.of(&tmp.path().join("nothing")).unwrap(), None);
    }

    #[test]
    fn a_saved_cache_answers_the_next_process() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("bin/app");
        write(&file, b"v1");
        let cache_path = tmp.path().join("digests.json");

        let mut cache = DigestCache::open(&cache_path);
        cache.of(&file).unwrap();
        cache.save().unwrap();

        let mut reopened = DigestCache::open(&cache_path);
        assert_eq!(reopened.len(), 1);
        assert_eq!(
            reopened.of(&file).unwrap().unwrap().sha256,
            sha256_bytes(b"v1")
        );
        assert!(!reopened.dirty, "a hit leaves nothing to write back");
    }

    #[test]
    fn nothing_hashed_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("digests.json");
        DigestCache::open(&cache_path).save().unwrap();
        assert!(!cache_path.exists());
    }
}
