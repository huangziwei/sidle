//! Pull a device's annotations (`.yjr` + `.yjf`) through the [`Transport`]
//! abstraction so the same import works on both mass-storage (KOA2 and other
//! jailbroken/pre-2024 Kindles) and MTP (Scribe, 2024+).
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
/// `.yjr` (annotations) and `.yjf` (last-read position) bytes — either may be
/// absent. Mirrors the mass-storage `std::fs` scan in
/// `ingest::import_from_device`: `.yjr.bad_file` is excluded (doesn't end in
/// `.yjr`), `.sdr` dirs with NEITHER sidecar (pure pagination caches) are
/// skipped, and `documents/Downloads/Items01/` (DRM'd Amazon KFX) is never
/// touched. The global `documents/My Clippings.txt` is deliberately ignored
/// — it would conflate Amazon-store books with Sidle-sideloaded ones.
///
/// Whether the device exposes its `.sdr/*.yjr` sidecars at all is up to its MTP
/// responder — `documents/Sidle/*.kfx` is always enumerable (that's where we
/// sideload), but Amazon's private reading-state dirs may or may not be. An
/// empty result on a Scribe means they aren't.
pub fn collect_device_yjr(transport: &dyn Transport) -> Result<Vec<CollectedYjr>> {
    let sidle = TPath::parse("documents/Sidle");
    let mut collected = Vec::new();

    for entry in transport.list(&sidle)? {
        if !entry.name.ends_with(".sdr") {
            continue;
        }
        let sdr = sidle.join(&entry.name);
        // `unwrap_or_default`: a `.sdr` that's somehow a file (or a transient
        // list failure) is skipped, matching core's `find_sidecar` graceful skip.
        let listing = transport.list(&sdr).unwrap_or_default();
        let yjr_name = listing.iter().find(|e| e.name.ends_with(".yjr")).map(|e| e.name.clone());
        let yjf_name = listing.iter().find(|e| e.name.ends_with(".yjf")).map(|e| e.name.clone());
        if yjr_name.is_none() && yjf_name.is_none() {
            continue; // pagination-cache `.sdr` — no annotations, no position
        }
        let yjr_bytes = match &yjr_name {
            Some(n) => Some(transport.read(&sdr.join(n))?),
            None => None,
        };
        let yjf_bytes = match &yjf_name {
            Some(n) => Some(transport.read(&sdr.join(n))?),
            None => None,
        };
        collected.push(CollectedYjr {
            sdr_name: entry.name,
            yjr_bytes,
            yjf_bytes,
        });
    }

    Ok(collected)
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
    transport: &dyn Transport,
    db: &DbHandle,
) -> Result<DeviceImportReport> {
    let now = db::now_iso();
    match device.mass_storage_mount() {
        Some(mount) => {
            // Mass-storage has a real volume — bypass the transport and use
            // the `std::fs` scanner so the USB scan + DB import can happen
            // under one lock (the volume IO is local-fast).
            let conn = db.blocking_lock();
            ingest::import_from_device(&conn, &mount, &device.serial, &now)
        }
        None => {
            // MTP: the USB walk is slow enough that we deliberately do it
            // BEFORE taking the DB lock — see `collect_device_yjr` above.
            let collected = collect_device_yjr(transport)?;
            let conn = db.blocking_lock();
            ingest::import_collected(&conn, collected, &device.serial, &now)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::mass_storage::transport::MassStorageTransport;

    // Drives the collector through the mass-storage transport (MTP can't be
    // unit-tested without a device). Confirms it finds both sidecars inside a
    // `.sdr`, STILL collects a `.yjf`-only (read, never highlighted) `.sdr`,
    // skips a pure pagination-cache `.sdr`, and ignores `.yjr.bad_file`.
    #[test]
    fn collects_yjr_and_yjf_over_transport() {
        let tmp = tempfile::tempdir().unwrap();
        let sidle = tmp.path().join("documents").join("Sidle");

        // Annotated + read: both `.yjr` (highlights) and `.yjf` (position).
        let annotated = sidle.join("book.deadbeef.sdr");
        std::fs::create_dir_all(&annotated).unwrap();
        std::fs::write(annotated.join("book.deadbeef0000.yjr"), b"YJR-BYTES").unwrap();
        std::fs::write(annotated.join("book.deadbeef0000.yjf"), b"YJF-BYTES").unwrap();
        // A device-rejected write must NOT be picked up (doesn't end in `.yjr`).
        std::fs::write(annotated.join("book.deadbeef0000.yjr.bad_file"), b"junk").unwrap();

        // Read but never highlighted: `.yjf` only — must STILL be collected so
        // its position imports even with no annotations.
        let posonly = sidle.join("read.cafef00d.sdr");
        std::fs::create_dir_all(&posonly).unwrap();
        std::fs::write(posonly.join("read.cafef00d0000.yjf"), b"POS-ONLY").unwrap();

        // A pure pagination-cache `.sdr` (neither sidecar) — skipped.
        std::fs::create_dir_all(sidle.join("cache.feedface.sdr")).unwrap();

        let transport = MassStorageTransport::new(tmp.path().to_path_buf());
        let mut collected = collect_device_yjr(&transport).unwrap();
        collected.sort_by(|a, b| a.sdr_name.cmp(&b.sdr_name)); // dir order is unspecified

        assert_eq!(collected.len(), 2, "annotated + position-only; cache skipped");

        let annotated = &collected[0];
        assert_eq!(annotated.sdr_name, "book.deadbeef.sdr");
        assert_eq!(annotated.yjr_bytes.as_deref(), Some(&b"YJR-BYTES"[..]));
        assert_eq!(annotated.yjf_bytes.as_deref(), Some(&b"YJF-BYTES"[..]));

        let posonly = &collected[1];
        assert_eq!(posonly.sdr_name, "read.cafef00d.sdr");
        assert!(posonly.yjr_bytes.is_none(), "no .yjr for a never-highlighted book");
        assert_eq!(posonly.yjf_bytes.as_deref(), Some(&b"POS-ONLY"[..]));
    }

    #[test]
    fn empty_when_no_sidle_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let transport = MassStorageTransport::new(tmp.path().to_path_buf());
        let collected = collect_device_yjr(&transport).unwrap();
        assert!(collected.is_empty());
    }
}
