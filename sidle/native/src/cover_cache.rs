//! On-device cover thumbnail cache.
//!
//! The picker fetches a ~20KB grayscale thumbnail per book over the LAN. That's
//! fast, but a relaunch would re-fetch every visible cover — and since the
//! picker hides already-downloaded books, it mostly shows books the in-memory
//! cache never warmed. This disk cache bridges across launches: a cover fetched
//! once is read straight off `/mnt/us` next time, skipping the network.
//!
//! Keyed by book id alone. Known caveat: a cover *recrawled* on the desktop
//! won't refresh here until the cached file is cleared — accepted for personal
//! use (see the plan's deferred `cover_rev` note).
//!
//! FAT-safe atomic write: bytes are written to a `.partial` sibling then
//! renamed over the target, so a crash mid-write can't leave a truncated JPEG
//! that would later decode to garbage. Mirrors core's `import::write_bytes_atomic`.

use std::path::{Path, PathBuf};

/// Cache file for one book's thumbnail: `<id>.jpg`. The bytes are whatever the
/// server returned for `?thumb=1` (a JPEG today).
fn cache_file(dir: &Path, id: i64) -> PathBuf {
    dir.join(format!("{id}.jpg"))
}

/// Read a cached thumbnail. `None` on any miss (absent or unreadable) — the
/// caller falls back to a network fetch.
pub fn load(dir: &Path, id: i64) -> Option<Vec<u8>> {
    std::fs::read(cache_file(dir, id)).ok()
}

/// Write a thumbnail to the cache. Atomic via temp+rename so a concurrent or
/// next-launch reader never sees a half-written file. Callers treat caching as
/// best-effort: a write failure here must never fail the fetch that produced
/// the bytes, so the `io::Result` is for logging only.
pub fn store(dir: &Path, id: i64, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let dest = cache_file(dir, id);
    let tmp = dest.with_extension("partial");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &dest)
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
        assert!(load(&dir, 7).is_none(), "cold cache should miss");
        store(&dir, 7, b"\xff\xd8\xff thumbnail bytes").unwrap();
        assert_eq!(load(&dir, 7).as_deref(), Some(&b"\xff\xd8\xff thumbnail bytes"[..]));
        // A different id is still a miss.
        assert!(load(&dir, 8).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_overwrites_atomically() {
        let dir = scratch("ow");
        store(&dir, 1, b"old").unwrap();
        store(&dir, 1, b"newer-and-longer").unwrap();
        assert_eq!(load(&dir, 1).as_deref(), Some(&b"newer-and-longer"[..]));
        // No stray .partial left behind after a successful rename.
        assert!(!dir.join("1.partial").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_creates_missing_dir() {
        let dir = scratch("mk");
        assert!(!dir.exists());
        store(&dir, 42, b"x").unwrap();
        assert!(dir.join("42.jpg").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
