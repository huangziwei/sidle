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

use crate::device::ink;
use crate::device::misc;
use crate::device::{DeviceInfo, TPath, Transport};
use crate::library::ingest::{self, CollectedYjr, DeviceImportReport};
use crate::library::{LibraryPaths, db};
use crate::state::DbHandle;

/// Per-item sync progress for the status bar, emitted as `annotations:sync-progress`.
/// `stage` is `"annotations"` (highlights/notes/bookmarks) or `"ink"` (handwriting);
/// `current`/`total` count books/notebooks; `label` is the book title.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncProgress {
    pub stage: String,
    pub current: usize,
    pub total: usize,
    pub label: String,
}

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
    // Resolve `documents/Sidle` ONCE and read each `.sdr`'s sidecars by handle in
    // one session (see [`Transport::read_files_in_children`]) — not a
    // path-based `list` + `read` per `.sdr`, which re-walks the whole Sidle dir on
    // every call (O(books²), the same blowup the ink walk had). `.yjr.bad_file` is
    // excluded (it doesn't end in `.yjr`); a `.sdr` with neither sidecar (a
    // pure pagination cache) yields no matching files and is dropped.
    let pulled =
        transport.read_files_in_children(&sidle, &|name| name.ends_with(".sdr"), &|file| {
            file.ends_with(".yjr") || file.ends_with(".yjf")
        })?;
    Ok(pulled
        .into_iter()
        .map(|(sdr_name, files)| {
            let pick = |suffix: &str| {
                files
                    .iter()
                    .find(|(n, _)| n.ends_with(suffix))
                    .map(|(_, b)| b.clone())
            };
            CollectedYjr {
                sdr_name,
                yjr_bytes: pick(".yjr"),
                yjf_bytes: pick(".yjf"),
            }
        })
        .collect())
}

/// Import annotations off any connected Kindle into the library — mass-storage
/// via core's `std::fs` scan, MTP via the [`Transport`] scan above. Blocking
/// (USB / DB IO); call on the blocking pool. Idempotent (`dedup_hash`), so it's
/// safe on every connect.
///
/// On MTP this also syncs **handwritten ink** drawn on sideloaded docs (the
/// `.notebooks/<asin>!!PDOC!!notebook/nbk` pull → [`crate::library::ink`]) in the
/// SAME pass, folding its counts into the returned report — so one
/// "sync annotations" covers highlights/notes/bookmarks, last-read position, AND
/// ink. (Mass-storage Kindles have no handwriting, so that branch skips it.)
///
/// For MTP the USB scans run *before* the DB lock is taken — `GetObject` over
/// USB is slow enough that holding the connection mutex through it would stall
/// the frontend's DB queries. Mass-storage keeps core's existing behavior (a
/// fast local `std::fs` scan under the lock).
/// `on_progress(stage, current, total, label)` reports which item is syncing now,
/// for the status bar: `stage` is `"annotations"` or `"ink"`, `current`/`total`
/// count books/notebooks, `label` is the book title. Pass a no-op to ignore it.
pub fn import_device_annotations(
    device: &DeviceInfo,
    transport: &dyn Transport,
    db: &DbHandle,
    paths: &LibraryPaths,
    on_progress: &dyn Fn(&str, usize, usize, &str),
) -> Result<DeviceImportReport> {
    let now = db::now_iso();
    let mut report = match device.mass_storage_mount() {
        Some(mount) => {
            // Mass-storage has a real volume — bypass the transport and use
            // the `std::fs` scanner so the USB scan + DB import can happen
            // under one lock (the volume IO is local-fast). No ink: handwriting
            // is a Scribe (MTP) feature.
            let conn = db.blocking_lock();
            ingest::import_from_device(&conn, &mount, &device.serial, &now)
        }
        None => {
            // The library's content_ids — so the ink walk knows which
            // `.notebooks/<id>!!PDOC!!` dirs are ours (a quick read, lock released
            // before the slow USB walks).
            let known_asins: std::collections::HashSet<String> = {
                let conn = db.blocking_lock();
                db::book_asins(&conn)?.into_iter().collect()
            };
            // MTP: do BOTH slow USB walks (annotations + ink notebooks) BEFORE
            // taking the DB lock — see `collect_device_yjr` above.
            // TEMP instrumentation: time each USB phase so we can see on-device
            // exactly where the pre-import wait goes. Remove once happy.
            let t = std::time::Instant::now();
            let collected = collect_device_yjr(transport)?;
            eprintln!(
                "[sidle/annsync] PHASE yjr walk: {} .sdr in {:.2}s",
                collected.len(),
                t.elapsed().as_secs_f32()
            );
            let t = std::time::Instant::now();
            let inks = ink::collect_device_ink(transport, &known_asins)?;
            eprintln!(
                "[sidle/annsync] PHASE ink walk: {} our nbks in {:.2}s",
                inks.len(),
                t.elapsed().as_secs_f32()
            );
            // The handwritten-ink anchors live in the same `.yjr`s we just pulled.
            let notes = ink::handwritten_notes(&collected);

            let report = {
                let t = std::time::Instant::now();
                let conn = db.blocking_lock();
                let mut report = ingest::import_collected_with_progress(
                    &conn,
                    collected,
                    &device.serial,
                    &now,
                    &|cur, tot, label| on_progress("annotations", cur, tot, label),
                )?;
                ink::import_collected_ink(
                    &conn,
                    paths,
                    &device.serial,
                    &now,
                    &inks,
                    &notes,
                    &mut report,
                    &|cur, tot, label| on_progress("ink", cur, tot, label),
                )?;
                eprintln!(
                    "[sidle/annsync] PHASE import (under lock): {:.2}s",
                    t.elapsed().as_secs_f32()
                );
                report
            }; // DB lock released here, before the USB cleanup below

            // No on-device cleanup: Sidle never deletes data on the device (a
            // backup must not mutate its source). Stranded `.notebooks/<id>!!PDOC!!`
            // dirs are the device owner's to clear. See
            // .claude/plans/backup-source-of-truth.md.
            Ok(report)
        }
    }?;

    // Additive backup of the device's screenshots + KUAL logs, over whichever
    // transport this Kindle uses. Best-effort: a misc-backup failure must never
    // fail the annotation sync it rides along with — log it and move on with the
    // counts we did get. Runs on every Sync (manual button + auto-on-connect).
    match misc::backup_device_misc(transport, &device.serial, paths) {
        Ok(m) => {
            report.misc_screenshots = m.screenshots_added;
            report.misc_logs = m.logs_updated;
        }
        Err(e) => eprintln!("[sidle/annsync] misc backup failed (non-fatal): {e:#}"),
    }

    Ok(report)
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

        assert_eq!(
            collected.len(),
            2,
            "annotated + position-only; cache skipped"
        );

        let annotated = &collected[0];
        assert_eq!(annotated.sdr_name, "book.deadbeef.sdr");
        assert_eq!(annotated.yjr_bytes.as_deref(), Some(&b"YJR-BYTES"[..]));
        assert_eq!(annotated.yjf_bytes.as_deref(), Some(&b"YJF-BYTES"[..]));

        let posonly = &collected[1];
        assert_eq!(posonly.sdr_name, "read.cafef00d.sdr");
        assert!(
            posonly.yjr_bytes.is_none(),
            "no .yjr for a never-highlighted book"
        );
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
