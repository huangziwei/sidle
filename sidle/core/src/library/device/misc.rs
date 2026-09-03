//! Additive backup of the folders a Kindle is configured to share, pulled
//! through [`Transport`] so one path covers both mass-storage and MTP.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::library::LibraryPaths;
use crate::library::device::transport::{TPath, Transport};
use crate::library::device_backup::{
    SyncCollection, SyncCollections, UpdatePolicy, already_stored, store_collection_file,
};

/// How deep a recursive collection may descend. A device folder is someone
/// else's to organize, and an unbounded walk over MTP is slow enough to look
/// hung; five levels is more nesting than a notes or drafts folder ever has.
const MAX_DEPTH: usize = 5;

/// Counts from one misc backup, folded into the sync's `DeviceImportReport`.
#[derive(Debug, Default, Clone)]
pub struct MiscBackupReport {
    /// Files stored, per collection id.
    pub stored: BTreeMap<String, usize>,
    /// Of those, the ones from [`UpdatePolicy::Once`] collections — a file we
    /// had never seen before. What a "your sync found something" notice should
    /// count.
    pub new_files: usize,
    /// The rest: files from `Always` collections, re-copied because they may
    /// have grown. A quiet sync still refreshes these, so they are not news.
    pub refreshed: usize,
}

impl MiscBackupReport {
    /// Total files stored across every collection.
    pub fn total(&self) -> usize {
        self.stored.values().sum()
    }
}

/// Back up the connected device's configured collections into
/// `device-backup/<serial>/`. Best-effort; only a local-fs failure propagates.
pub fn backup_device_misc(
    transport: &dyn Transport,
    serial: &str,
    paths: &LibraryPaths,
    config: &SyncCollections,
) -> Result<MiscBackupReport> {
    paths
        .ensure_device_backup(serial)
        .context("create device-backup dir")?;

    let mut report = MiscBackupReport::default();
    for collection in &config.collections {
        let mut stored = 0usize;
        for dir in collection.scan_dirs() {
            walk(
                transport,
                &TPath::parse(dir),
                "",
                collection,
                serial,
                paths,
                &mut stored,
                0,
            );
        }
        report.stored.insert(collection.id.clone(), stored);
        match collection.update {
            UpdatePolicy::Once => report.new_files += stored,
            UpdatePolicy::Always => report.refreshed += stored,
        }
    }
    Ok(report)
}

/// Scan one device directory for `collection`, recursing when it asks for it.
#[allow(clippy::too_many_arguments)]
fn walk(
    transport: &dyn Transport,
    dir: &TPath,
    rel: &str,
    collection: &SyncCollection,
    serial: &str,
    paths: &LibraryPaths,
    stored: &mut usize,
    depth: usize,
) {
    let entries = match transport.list(dir) {
        Ok(entries) => entries,
        Err(e) => {
            // A dir the device doesn't expose over MTP, or a transient read
            // hiccup. Both transports map a missing dir to an empty listing, so
            // this only trips on real IO.
            eprintln!("[sidle/misc] list {dir} failed (skipping): {e:#}");
            return;
        }
    };
    for entry in entries {
        let child_rel = if rel.is_empty() {
            entry.name.clone()
        } else {
            format!("{rel}/{}", entry.name)
        };
        if entry.is_dir {
            if collection.recursive && depth + 1 < MAX_DEPTH && !entry.name.starts_with('.') {
                walk(
                    transport,
                    &dir.join(&entry.name),
                    &child_rel,
                    collection,
                    serial,
                    paths,
                    stored,
                    depth + 1,
                );
            }
            continue;
        }
        if !collection.includes(&entry.name)
            || already_stored(paths, serial, collection, &child_rel)
        {
            continue;
        }
        let bytes = match transport.read(&dir.join(&entry.name)) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[sidle/misc] read {child_rel} failed (skipping): {e:#}");
                continue;
            }
        };
        match store_collection_file(paths, serial, collection, &child_rel, &bytes) {
            Ok(true) => *stored += 1,
            Ok(false) => {}
            Err(e) => eprintln!("[sidle/misc] store {child_rel} failed: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::LibraryPaths;
    use crate::library::device::mass_storage::transport::MassStorageTransport;
    use crate::library::device_backup::SyncCollection;

    // Drive the backup through the mass-storage transport against a fake device
    #[test]
    fn backs_up_the_default_collections() {
        let device = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();

        // Newer-style: screenshots under `screenshots/`.
        let shots = device.path().join("screenshots");
        std::fs::create_dir_all(&shots).unwrap();
        std::fs::write(shots.join("screenshot_100.png"), b"PNG-A").unwrap();
        std::fs::write(shots.join("screenshot_200.png"), b"PNG-B").unwrap();
        // The firmware's companion file: cleared by the picker, never stored.
        std::fs::write(shots.join("wininfo_screenshot_100.txt"), b"win").unwrap();
        // KOA2-style: a stock capture loose in the root.
        std::fs::write(device.path().join("Screenshot_root.png"), b"PNG-ROOT").unwrap();
        // Logs live in `logs/` now; a stray root log is not a log we take.
        let logs = device.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("sidle-native.log"), b"log lines\n").unwrap();
        std::fs::write(logs.join("sidle-update.log"), b"update lines\n").unwrap();
        std::fs::write(device.path().join("stray.log"), b"not scanned\n").unwrap();
        std::fs::write(device.path().join("version.txt"), b"5.16.2").unwrap();

        let transport = MassStorageTransport::new(device.path().to_path_buf());
        let paths = LibraryPaths {
            root: lib.path().to_path_buf(),
        };
        let config = SyncCollections::defaults();

        let report = backup_device_misc(&transport, "G000TESTSERIAL", &paths, &config).unwrap();
        assert_eq!(
            report.stored["screenshots"], 3,
            "2 in screenshots/ + 1 root"
        );
        assert_eq!(report.stored["logs"], 2, "both picker logs");

        let backed = paths.device_backup_collection("G000TESTSERIAL", "screenshots");
        assert!(backed.join("screenshot_100.png").is_file());
        assert!(backed.join("screenshot_200.png").is_file());
        assert!(backed.join("Screenshot_root.png").is_file());
        assert!(
            !backed.join("wininfo_screenshot_100.txt").exists(),
            "purge-only companion is never stored"
        );
        assert!(
            !backed.join("version.txt").exists(),
            "non-screenshot ignored"
        );
        let stored_logs = paths.device_backup_collection("G000TESTSERIAL", "logs");
        assert_eq!(
            std::fs::read(stored_logs.join("sidle-native.log")).unwrap(),
            b"log lines\n"
        );
        assert!(
            !stored_logs.join("stray.log").exists(),
            "only logs/ is scanned for logs"
        );

        // Second run: screenshots already present → skipped; logs re-copied.
        let again = backup_device_misc(&transport, "G000TESTSERIAL", &paths, &config).unwrap();
        assert_eq!(again.stored["screenshots"], 0, "Once skips what we hold");
        assert_eq!(again.stored["logs"], 2, "Always refreshes");
    }

    #[test]
    fn a_recursive_collection_keeps_its_subfolders() {
        let device = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(device.path().join("writing/2026")).unwrap();
        std::fs::write(device.path().join("writing/index.md"), b"root").unwrap();
        std::fs::write(device.path().join("writing/2026/draft.md"), b"nested").unwrap();
        std::fs::write(device.path().join("writing/2026/notes.txt"), b"other").unwrap();

        let paths = LibraryPaths {
            root: lib.path().to_path_buf(),
        };
        // Note the id is not the folder's name: the two are free to differ, and
        // the folder is the one that gets renamed.
        let config = SyncCollections {
            collections: vec![SyncCollection {
                id: "drafts".into(),
                label: "Drafts".into(),
                dirs: vec!["writing".into()],
                include: vec!["*.md".into()],
                recursive: true,
                update: UpdatePolicy::Always,
                clear_device: false,
                purge: Vec::new(),
            }],
        };
        let transport = MassStorageTransport::new(device.path().to_path_buf());
        let report = backup_device_misc(&transport, "S", &paths, &config).unwrap();
        assert_eq!(report.stored["drafts"], 2);

        let backed = paths.device_backup_collection("S", "drafts");
        assert!(backed.join("index.md").is_file());
        assert_eq!(
            std::fs::read_to_string(backed.join("2026/draft.md")).unwrap(),
            "nested"
        );
        assert!(!backed.join("2026/notes.txt").exists());
    }

    #[test]
    fn a_folder_that_isnt_on_the_device_yields_nothing() {
        let device = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        let transport = MassStorageTransport::new(device.path().to_path_buf());
        let paths = LibraryPaths {
            root: lib.path().to_path_buf(),
        };
        let report =
            backup_device_misc(&transport, "S", &paths, &SyncCollections::defaults()).unwrap();
        assert_eq!(report.total(), 0);
        // Nothing was stored, so no collection dir was left behind either.
        assert!(!paths.device_backup_collection("S", "screenshots").exists());
    }
}
