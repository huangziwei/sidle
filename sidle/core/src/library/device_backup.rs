//! Shared storage policy for the Kindle's "misc" backup artifacts — screenshots
//! and picker logs — landed under `device-backup/<serial>/` (see
//! [`LibraryPaths::device_backup_screenshots`](crate::library::LibraryPaths::device_backup_screenshots)
//! / [`device_backup_logs`](crate::library::LibraryPaths::device_backup_logs)).
//!
//! Two callers feed this with the same policy, so a screenshot backed up over
//! WiFi is byte-identical to one backed up over USB:
//! - `sidle-server`'s WiFi receive (`POST /sync/misc`), which the on-device picker
//!   picker pushes to when the user taps **Sync** — the primary path.
//! - the desktop app's USB pull (`device::misc`), when a Kindle is plugged in.

use std::path::Path;

use crate::library::LibraryPaths;

/// What kind of misc artifact a device filename is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiscKind {
    /// A screenshot (`screenshot*`): immutable, so stored copy-if-absent.
    Screenshot,
    /// A log (`*.log`): grows by appending, so stored overwrite-always.
    Log,
}

/// Classify a device filename, or `None` if it isn't an artifact we back up.
/// Screenshots match the `screenshot` prefix (stock firmware and Sidle's own
/// two-corner capture both write `screenshot_<unix>.png`); logs match the `.log`
/// suffix (the picker's `sidle-native.log` / `sidle-update.log`).
/// Case-insensitive; an in-flight `.partial` write is not yet a screenshot.
pub fn classify_misc(name: &str) -> Option<MiscKind> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".partial") {
        None
    } else if lower.starts_with("screenshot") {
        Some(MiscKind::Screenshot)
    } else if lower.ends_with(".log") {
        Some(MiscKind::Log)
    } else {
        None
    }
}

/// Store one misc file for `serial` under `device-backup/<serial>/`. Screenshots
/// are copy-if-absent (immutable — never re-write one we already hold); logs
/// overwrite (they grow). Empty `bytes` are skipped so a truncated source can't
/// clobber a good prior backup. Returns `Ok(true)` when a file was written,
/// `Ok(false)` when skipped.
///
/// `name` is reduced to its final path component, so a crafted `../…` name from
/// a network client (the WiFi push) can't escape the backup dir.
pub fn store_misc_file(
    paths: &LibraryPaths,
    serial: &str,
    kind: MiscKind,
    name: &str,
    bytes: &[u8],
) -> std::io::Result<bool> {
    if bytes.is_empty() {
        return Ok(false);
    }
    // Strip any directory components a caller (or attacker) put in `name`.
    let Some(base) = Path::new(name).file_name().and_then(|n| n.to_str()) else {
        return Ok(false); // e.g. ".." — no real filename
    };
    let (dir, overwrite) = match kind {
        MiscKind::Screenshot => (paths.device_backup_screenshots(serial), false),
        MiscKind::Log => (paths.device_backup_logs(serial), true),
    };
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(base);
    if !overwrite && dest.exists() {
        return Ok(false);
    }
    std::fs::write(&dest, bytes)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_name() {
        assert_eq!(
            classify_misc("screenshot_100.png"),
            Some(MiscKind::Screenshot)
        );
        assert_eq!(
            classify_misc("Screenshot_ROOT.PNG"),
            Some(MiscKind::Screenshot)
        );
        assert_eq!(classify_misc("sidle-native.log"), Some(MiscKind::Log));
        assert_eq!(classify_misc("screenshot_1.png.partial"), None);
        assert_eq!(classify_misc("book.kfx"), None);
        assert_eq!(classify_misc("version.txt"), None);
    }

    #[test]
    fn screenshots_copy_if_absent_logs_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        let s = "G000TEST";

        // First write of each lands.
        assert!(
            store_misc_file(&paths, s, MiscKind::Screenshot, "screenshot_1.png", b"A").unwrap()
        );
        assert!(store_misc_file(&paths, s, MiscKind::Log, "sidle-native.log", b"v1\n").unwrap());

        // Screenshot re-write is skipped (immutable); its bytes are untouched.
        assert!(
            !store_misc_file(&paths, s, MiscKind::Screenshot, "screenshot_1.png", b"B").unwrap()
        );
        assert_eq!(
            std::fs::read(paths.device_backup_screenshots(s).join("screenshot_1.png")).unwrap(),
            b"A"
        );

        // Log re-write overwrites (it grew).
        assert!(
            store_misc_file(&paths, s, MiscKind::Log, "sidle-native.log", b"v1\nv2\n").unwrap()
        );
        assert_eq!(
            std::fs::read(paths.device_backup_logs(s).join("sidle-native.log")).unwrap(),
            b"v1\nv2\n"
        );

        // Empty source is skipped.
        assert!(!store_misc_file(&paths, s, MiscKind::Log, "empty.log", b"").unwrap());
    }

    #[test]
    fn path_traversal_in_name_is_neutralized() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        // A crafted name must not escape device-backup/<serial>/screenshots/.
        store_misc_file(&paths, "S", MiscKind::Screenshot, "../../evil.png", b"X").unwrap();
        assert!(!tmp.path().join("evil.png").exists());
        assert!(
            paths
                .device_backup_screenshots("S")
                .join("evil.png")
                .is_file()
        );
    }
}
