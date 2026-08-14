//! Additive backup of a Kindle's "misc" diagnostic artifacts — screenshots and
//! our picker logs — pulled through the [`Transport`] abstraction on
//! every Sync, so one path covers mass-storage (KOA2 etc.) and MTP (Scribe/2024+).
//!
//! Screenshots live in different places by device generation: newer firmware
//! writes `screenshot_*.png` into `screenshots/` (where Sidle's own two-corner
//! capture also lands — see native `eink/screenshot.rs`), while older devices
//! (KOA2) leave the *stock* firmware's captures loose in the USB root. We scan
//! both directories shallowly and match `screenshot*` by name. Logs are the
//! picker's root-level `sidle-native.log` / `sidle-update.log` (plus any other
//! root `*.log` worth having for diagnostics — see native `main.rs` LOG_PATH).
//!
//! Additive, like the annotation sync (Sidle never mutates the device): a
//! screenshot is immutable + timestamped, so we copy-if-absent — never re-pull
//! one already backed up (cheap on MTP, where every read re-walks from the
//! root). A log grows by appending, so we always overwrite with the device's
//! current copy.

use crate::library::device_backup::{MiscKind, classify_misc, store_misc_file};
use anyhow::{Context, Result};

use crate::library::LibraryPaths;
use crate::library::device::transport::{TEntry, TPath, Transport};

/// Counts from one misc backup, folded into the sync's `DeviceImportReport`.
#[derive(Debug, Default, Clone)]
pub struct MiscBackupReport {
    /// Screenshots copied this run — new ones only (already-backed-up skipped).
    pub screenshots_added: usize,
    /// Logs refreshed this run (always re-copied — they grow by appending).
    pub logs_updated: usize,
}

/// Back up the connected device's screenshots + picker logs into
/// `device-backup/<serial>/` under the library root — the USB twin of the WiFi
/// `POST /sync/misc` the picker pushes, sharing the exact same
/// [`store_misc_file`] policy. Best-effort per file: an unreadable screenshot or
/// a directory the device's MTP responder doesn't expose is logged and skipped,
/// never fatal — the annotation sync this rides along with must still succeed.
/// Only a failure to create the local backup dirs (a real local-fs problem)
/// propagates.
pub fn backup_device_misc(
    transport: &dyn Transport,
    serial: &str,
    paths: &LibraryPaths,
) -> Result<MiscBackupReport> {
    paths
        .ensure_device_backup(serial)
        .context("create device-backup dirs")?;

    let mut report = MiscBackupReport::default();

    // `screenshots/` — shallow. Everyone's screenshots land here: Sidle's own
    // two-corner captures plus newer stock firmware's.
    let shots = TPath::parse("screenshots");
    for entry in list_or_warn(transport, &shots) {
        pull_and_store(transport, &shots, &entry, serial, paths, &mut report);
    }

    // USB root — shallow, listed once. KOA2's *stock* firmware drops its
    // captures loose here (not under `screenshots/`); our picker logs sit here too.
    for entry in list_or_warn(transport, &TPath::new()) {
        pull_and_store(transport, &TPath::new(), &entry, serial, paths, &mut report);
    }

    Ok(report)
}

/// Classify `entry`, read it off the device, and store it via core's shared
/// policy, tallying the result. Screenshots skip the (MTP-expensive) read
/// entirely when already backed up — `store_misc_file`'s copy-if-absent guard
/// would also catch it, but only after the read, which is the slow part.
fn pull_and_store(
    transport: &dyn Transport,
    dir: &TPath,
    entry: &TEntry,
    serial: &str,
    paths: &LibraryPaths,
    report: &mut MiscBackupReport,
) {
    if entry.is_dir {
        return;
    }
    let Some(kind) = classify_misc(&entry.name) else {
        return;
    };
    if kind == MiscKind::Screenshot
        && paths
            .device_backup_screenshots(serial)
            .join(&entry.name)
            .exists()
    {
        return; // already backed up — don't re-pull over MTP
    }
    let bytes = match transport.read(&dir.join(&entry.name)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[sidle/misc] read {} failed (skipping): {e:#}", entry.name);
            return;
        }
    };
    match store_misc_file(paths, serial, kind, &entry.name, &bytes) {
        Ok(true) => match kind {
            MiscKind::Screenshot => report.screenshots_added += 1,
            MiscKind::Log => report.logs_updated += 1,
        },
        Ok(false) => {}
        Err(e) => eprintln!("[sidle/misc] store {} failed: {e:#}", entry.name),
    }
}

/// List `dir` over the transport, warning + returning empty on error (a dir the
/// device doesn't expose over MTP, or a transient read hiccup). Both transports
/// already map a missing dir to an empty listing, so this only trips on real IO.
fn list_or_warn(transport: &dyn Transport, dir: &TPath) -> Vec<TEntry> {
    match transport.list(dir) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("[sidle/misc] list {dir} failed (skipping): {e:#}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::LibraryPaths;
    use crate::library::device::mass_storage::transport::MassStorageTransport;

    // Drive the backup through the mass-storage transport against a fake device
    // tree (MTP can't be unit-tested without hardware). Confirms screenshots are
    // pulled from BOTH `screenshots/` and the root (the KOA2 case), logs are
    // pulled from the root, non-matching files are ignored, and a second run is
    // a no-op for screenshots (copy-if-absent) but re-copies logs.
    #[test]
    fn backs_up_screenshots_and_logs_from_both_locations() {
        let device = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();

        // Newer-style: screenshots under `screenshots/`.
        let shots = device.path().join("screenshots");
        std::fs::create_dir_all(&shots).unwrap();
        std::fs::write(shots.join("screenshot_100.png"), b"PNG-A").unwrap();
        std::fs::write(shots.join("screenshot_200.png"), b"PNG-B").unwrap();
        // KOA2-style: a stock capture loose in the root.
        std::fs::write(device.path().join("Screenshot_root.png"), b"PNG-ROOT").unwrap();
        // Our picker logs in the root, plus an unrelated root file to ignore.
        std::fs::write(device.path().join("sidle-native.log"), b"log lines\n").unwrap();
        std::fs::write(device.path().join("sidle-update.log"), b"update lines\n").unwrap();
        std::fs::write(device.path().join("version.txt"), b"5.16.2").unwrap();

        let transport = MassStorageTransport::new(device.path().to_path_buf());
        let paths = LibraryPaths {
            root: lib.path().to_path_buf(),
        };

        let report = backup_device_misc(&transport, "G000TESTSERIAL", &paths).unwrap();
        assert_eq!(report.screenshots_added, 3, "2 in screenshots/ + 1 in root");
        assert_eq!(report.logs_updated, 2, "both picker logs");

        let backed = paths.device_backup_screenshots("G000TESTSERIAL");
        assert!(backed.join("screenshot_100.png").is_file());
        assert!(backed.join("screenshot_200.png").is_file());
        assert!(backed.join("Screenshot_root.png").is_file());
        let logs = paths.device_backup_logs("G000TESTSERIAL");
        assert_eq!(
            std::fs::read(logs.join("sidle-native.log")).unwrap(),
            b"log lines\n"
        );
        assert!(
            !backed.join("version.txt").exists(),
            "non-screenshot ignored"
        );

        // Second run: screenshots already present → skipped; logs re-copied.
        let again = backup_device_misc(&transport, "G000TESTSERIAL", &paths).unwrap();
        assert_eq!(again.screenshots_added, 0, "copy-if-absent skips existing");
        assert_eq!(again.logs_updated, 2, "logs always refreshed");
    }

    #[test]
    fn empty_device_is_a_clean_no_op() {
        let device = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        let transport = MassStorageTransport::new(device.path().to_path_buf());
        let paths = LibraryPaths {
            root: lib.path().to_path_buf(),
        };
        let report = backup_device_misc(&transport, "S", &paths).unwrap();
        assert_eq!(report.screenshots_added, 0);
        assert_eq!(report.logs_updated, 0);
    }
}
