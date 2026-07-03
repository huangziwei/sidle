//! On-device cover thumbnail cache.
//!
//! The picker fetches a ~30–50KB color thumbnail per book over the LAN. That's
//! fast, but a relaunch would re-fetch every visible cover — and since the
//! picker hides already-downloaded books, it mostly shows books the in-memory
//! cache never warmed. This disk cache bridges across launches: a cover fetched
//! once is read straight off `/mnt/us` next time, skipping the network.
//!
//! Keyed by book id + cover revision: `<id>.<rev>.jpg`, where `rev` is the
//! cover's mtime shipped in `/list.json` (`cover_rev`). A desktop cover-recrawl
//! bumps the rev, so the next launch misses the old file and refetches — self-
//! invalidating. `store` prunes prior revisions, so the cache holds one file
//! per book, not one per cover version. (An older server omits `cover_rev`, so
//! every book gets rev 0 — i.e. caches by id alone, the prior behavior.)
//!
//! FAT-safe atomic write: bytes are written to a `.partial` sibling then
//! renamed over the target, so a crash mid-write can't leave a truncated JPEG
//! that would later decode to garbage. Mirrors core's `import::write_bytes_atomic`.

use std::path::{Path, PathBuf};

/// Cache file for one book's thumbnail: `<id>.<rev>.jpg`. The bytes are
/// whatever the server returned for `?thumb=1` (a JPEG today); `rev` is the
/// book's `cover_rev` so a changed cover lands under a new name.
fn cache_file(dir: &Path, id: i64, rev: i64) -> PathBuf {
    dir.join(format!("{id}.{rev}.jpg"))
}

/// Read a cached thumbnail for this id+rev. `None` on any miss (absent or
/// unreadable) — the caller falls back to a network fetch. A bumped `rev`
/// (desktop recrawl) misses the old file, which is what drives the refetch.
pub fn load(dir: &Path, id: i64, rev: i64) -> Option<Vec<u8>> {
    std::fs::read(cache_file(dir, id, rev)).ok()
}

/// Write a thumbnail to the cache. Atomic via temp+rename so a concurrent or
/// next-launch reader never sees a half-written file. Callers treat caching as
/// best-effort: a write failure here must never fail the fetch that produced
/// the bytes, so the `io::Result` is for logging only. After a successful
/// write, older revisions of this book are pruned so the cache holds one file
/// per book.
pub fn store(dir: &Path, id: i64, rev: i64, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let dest = cache_file(dir, id, rev);
    let tmp = dest.with_extension("partial");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &dest)?;
    prune_old(dir, id, &dest);
    Ok(())
}

/// Remove this book's other `<id>.*.jpg` cache files, keeping only `keep`.
/// Best-effort: a failure just leaves an orphaned thumbnail and never fails the
/// store. The `<id>.` prefix can't match a different book (`1.` won't match
/// `10.…` — the dot guards it).
fn prune_old(dir: &Path, id: i64, keep: &Path) {
    let prefix = format!("{id}.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == *keep {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with(&prefix) && name.ends_with(".jpg") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sidle-covcache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn cold_load_misses_then_store_roundtrips() {
        let dir = scratch("rt");
        assert!(load(&dir, 7, 100).is_none(), "cold cache should miss");
        store(&dir, 7, 100, b"\xff\xd8\xff thumbnail bytes").unwrap();
        assert_eq!(
            load(&dir, 7, 100).as_deref(),
            Some(&b"\xff\xd8\xff thumbnail bytes"[..])
        );
        // Same id, newer rev (cover changed on the desktop) is a miss.
        assert!(load(&dir, 7, 101).is_none());
        // A different id is still a miss.
        assert!(load(&dir, 8, 100).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_new_rev_prunes_old_rev() {
        let dir = scratch("rev");
        store(&dir, 1, 100, b"old-cover").unwrap();
        assert!(dir.join("1.100.jpg").exists());
        // A recrawl bumps the rev; storing the new one drops the old file and
        // leaves no stray .partial behind.
        store(&dir, 1, 200, b"new-cover-and-longer").unwrap();
        assert_eq!(
            load(&dir, 1, 200).as_deref(),
            Some(&b"new-cover-and-longer"[..])
        );
        assert!(!dir.join("1.100.jpg").exists(), "old rev should be pruned");
        assert!(!dir.join("1.200.partial").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_leaves_other_books_alone() {
        let dir = scratch("multi");
        store(&dir, 1, 100, b"book1").unwrap();
        store(&dir, 10, 100, b"book10").unwrap(); // id-prefix lookalike
        // Re-storing book 1 must not touch book 10's file.
        store(&dir, 1, 200, b"book1-v2").unwrap();
        assert!(load(&dir, 10, 100).is_some(), "other book untouched");
        assert!(load(&dir, 1, 100).is_none(), "own old rev pruned");
        assert!(load(&dir, 1, 200).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_creates_missing_dir() {
        let dir = scratch("mk");
        assert!(!dir.exists());
        store(&dir, 42, 1, b"x").unwrap();
        assert!(dir.join("42.1.jpg").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
