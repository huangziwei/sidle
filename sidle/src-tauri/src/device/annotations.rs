//! Pull a device's annotations (`.yjr` + `My Clippings.txt`) through the
//! [`Transport`] abstraction so the same import works on both mass-storage
//! (KOA2 and other jailbroken/pre-2024 Kindles) and MTP (Scribe, 2024+).
//!
//! The transport-agnostic import logic lives in `sidle_core::library::ingest`
//! ([`ingest::import_collected`]); this module is the *device-side scan* that
//! feeds it. It lives in the app (not core) because [`Transport`] is an app
//! concept — core has its own `std::fs` collector (`ingest::import_from_device`)
//! for mass-storage and its hermetic tests.

use anyhow::Result;

use crate::device::{DeviceInfo, TPath, Transport};
use crate::library::ingest::{self, CollectedYjr, DeviceImportReport};
use crate::library::db;
use crate::state::DbHandle;

/// Scan `documents/Sidle/` over `transport`, returning each `.sdr` directory's
/// `.yjr` bytes plus the `My Clippings.txt` text (if present). Mirrors the
/// mass-storage `std::fs` scan in `ingest::import_from_device`: `.yjr.bad_file`
/// is excluded (doesn't end in `.yjr`), pagination-cache `.sdr` dirs with no
/// `.yjr` are skipped, and `documents/Downloads/Items01/` (DRM'd Amazon KFX) is
/// never touched.
///
/// Whether the device exposes its `.sdr/*.yjr` sidecars at all is up to its MTP
/// responder — `documents/Sidle/*.kfx` is always enumerable (that's where we
/// sideload), but Amazon's private reading-state dirs may or may not be. An
/// empty result on a Scribe means they aren't.
pub fn collect_device_yjr(
    transport: &dyn Transport,
) -> Result<(Vec<CollectedYjr>, Option<String>)> {
    let sidle = TPath::parse("documents/Sidle");
    let mut collected = Vec::new();

    for entry in transport.list(&sidle)? {
        if !entry.name.ends_with(".sdr") {
            continue;
        }
        let sdr = sidle.join(&entry.name);
        // `unwrap_or_default`: a `.sdr` that's somehow a file (or a transient
        // list failure) is skipped, matching core's `find_yjr_in` graceful skip.
        let yjr_name = transport
            .list(&sdr)
            .unwrap_or_default()
            .into_iter()
            .find(|e| e.name.ends_with(".yjr"))
            .map(|e| e.name);
        let Some(yjr_name) = yjr_name else {
            continue; // pagination-cache `.sdr`, no annotations
        };
        let yjr_bytes = transport.read(&sdr.join(&yjr_name))?;
        collected.push(CollectedYjr {
            sdr_name: entry.name,
            yjr_bytes,
        });
    }

    // Orphan archive — best-effort. `from_utf8_lossy` matches core's
    // `clippings::parse_file`; absent or unreadable just means no orphans.
    let clip = TPath::parse("documents/My Clippings.txt");
    let clippings_txt = transport
        .read(&clip)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned());

    Ok((collected, clippings_txt))
}

/// Import annotations off any connected Kindle into the library — mass-storage
/// via core's `std::fs` scan, MTP via the [`Transport`] scan above. Blocking
/// (USB / DB IO); call on the blocking pool. Idempotent (`dedup_hash`), so it's
/// safe on every connect.
///
/// For MTP the USB scan runs *before* the DB lock is taken — `GetObject` over
/// USB is slow enough that holding the connection mutex through it would stall
/// the frontend's DB queries. Mass-storage keeps core's existing behavior (a
/// fast local `std::fs` scan under the lock).
pub fn import_device_annotations(
    device: &DeviceInfo,
    db: &DbHandle,
) -> Result<DeviceImportReport> {
    let now = db::now_iso();
    match device.mass_storage_mount() {
        Some(mount) => {
            let conn = db.blocking_lock();
            ingest::import_from_device(&conn, &mount, &now)
        }
        None => {
            let transport = device.open_transport()?;
            let (collected, clippings) = collect_device_yjr(transport.as_ref())?;
            let conn = db.blocking_lock();
            ingest::import_collected(&conn, collected, clippings.as_deref(), &now)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::mass_storage::transport::MassStorageTransport;

    // Drives the collector through the mass-storage transport (MTP can't be
    // unit-tested without a device). Confirms it finds the `.yjr` inside a
    // `.sdr`, skips a pagination-cache `.sdr`, ignores `.yjr.bad_file`, and
    // reads `My Clippings.txt`.
    #[test]
    fn collects_yjr_and_clippings_over_transport() {
        let tmp = tempfile::tempdir().unwrap();
        let sidle = tmp.path().join("documents").join("Sidle");

        let annotated = sidle.join("book.deadbeef.sdr");
        std::fs::create_dir_all(&annotated).unwrap();
        std::fs::write(annotated.join("book.deadbeef0000.yjr"), b"YJR-BYTES").unwrap();
        // A device-rejected write must NOT be picked up (doesn't end in `.yjr`).
        std::fs::write(annotated.join("book.deadbeef0000.yjr.bad_file"), b"junk").unwrap();

        // A pagination-cache `.sdr` with no `.yjr` — skipped.
        std::fs::create_dir_all(sidle.join("other.cafef00d.sdr")).unwrap();

        std::fs::write(
            tmp.path().join("documents").join("My Clippings.txt"),
            b"clip text",
        )
        .unwrap();

        let transport = MassStorageTransport::new(tmp.path().to_path_buf());
        let (collected, clippings) = collect_device_yjr(&transport).unwrap();

        assert_eq!(collected.len(), 1, "one annotated .sdr");
        assert_eq!(collected[0].sdr_name, "book.deadbeef.sdr");
        assert_eq!(collected[0].yjr_bytes, b"YJR-BYTES");
        assert_eq!(clippings.as_deref(), Some("clip text"));
    }

    #[test]
    fn empty_when_no_sidle_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let transport = MassStorageTransport::new(tmp.path().to_path_buf());
        let (collected, clippings) = collect_device_yjr(&transport).unwrap();
        assert!(collected.is_empty());
        assert!(clippings.is_none());
    }
}
