//! Device-side state derived from `/mnt/us/documents/Sidle/` contents.
//!
//! The picker filters its grid to "books not on this Kindle." Source of
//! truth for "is it on this Kindle" is the download dir, where every
//! file we ever wrote lives as `<base>.<sha8>.kfx`. We scan the dir,
//! parse the sha8 out of each filename, and hand `main.rs` a `HashSet`
//! to filter against.
//!
//! Missing dir → empty set. That's first-ever-launch (we haven't created
//! `/mnt/us/documents/Sidle/` yet) and also the dev-box path where the
//! dir genuinely doesn't exist. Not an error.

use std::collections::HashSet;
use std::path::Path;

use crate::api::SHA_INFIX_LEN;

/// Read `dir` and return the sha8 prefix of every `<base>.<sha8>.kfx`
/// file found. Files that don't match the shape (legacy downloads,
/// manual sideloads, the Kindle indexer's `.sdr` companion dirs) are
/// silently skipped.
pub fn scan_downloaded_shas(dir: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        if let Some(sha) = extract_sha8(name) {
            out.insert(sha.to_string());
        }
    }
    out
}

/// Pull the `<sha8>` segment out of `<base>.<sha8>.kfx`. Returns `None`
/// if the name doesn't match the shape.
pub(crate) fn extract_sha8(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".kfx")?;
    let (_, sha) = stem.rsplit_once('.')?;
    if sha.len() == SHA_INFIX_LEN && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(sha)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_sha8_from_well_formed_name() {
        assert_eq!(extract_sha8("Title.deadbeef.kfx"), Some("deadbeef"));
        assert_eq!(
            extract_sha8("[A] Title (2024).cafef00d.kfx"),
            Some("cafef00d"),
        );
        // Multi-dot basenames: rsplit_once takes the LAST dot, so the
        // sha is whatever sits between the final two dots.
        assert_eq!(extract_sha8("foo.bar.baz.12345678.kfx"), Some("12345678"),);
    }

    #[test]
    fn rejects_malformed_names() {
        assert_eq!(extract_sha8("Title.kfx"), None); // no sha segment
        assert_eq!(extract_sha8("Title.deadbeef"), None); // no .kfx
        assert_eq!(extract_sha8("Title.deadbeeZ.kfx"), None); // not all hex
        assert_eq!(extract_sha8("Title.deadbee.kfx"), None); // 7 chars
        assert_eq!(extract_sha8("Title.deadbeefa.kfx"), None); // 9 chars
        assert_eq!(extract_sha8("Title.txt"), None); // wrong ext
        assert_eq!(extract_sha8(""), None);
        assert_eq!(extract_sha8(".kfx"), None); // stem empty,
        // no preceding dot
    }

    #[test]
    fn scan_returns_empty_for_missing_dir() {
        // Intentionally a path that won't exist on host or device.
        let bogus = Path::new("/this/does/not/exist/sidle-scan-test");
        assert!(scan_downloaded_shas(bogus).is_empty());
    }
}
