//! Pull a device's annotations (`.yjr` + `.yjf`) through the [`Transport`]
//! abstraction so the same import works on both mass-storage (KOA2 and other
//! jailbroken/pre-2024 Kindles) and MTP (Scribe, 2024+).

use anyhow::Result;

use crate::library::device::ink;
use crate::library::device::misc;
use crate::library::device::{DeviceInfo, TPath, Transport};
use crate::library::ingest::{self, CollectedYjr, DeviceImportReport};
use crate::library::ink::{handwritten_notes, import_collected_ink};
use crate::library::{LibraryPaths, db, device_backup, import, push};

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
pub fn collect_device_yjr(transport: &dyn Transport) -> Result<Vec<CollectedYjr>> {
    let sidle = TPath::parse("documents/Sidle");
    // Resolve `documents/Sidle` ONCE and read each `.sdr`'s sidecars by handle in one
    // session, not a path-based `list` + `read` per `.sdr`, which is O(books²).
    // every call (O(books²)). `.yjr.bad_file` is
    // excluded (it doesn't end in `.yjr`); a `.sdr` with neither sidecar (a
    // pure pagination cache) yields no matching files and is dropped.
    let pulled =
        transport.read_files_in_children(&sidle, &|name| name.ends_with(".sdr"), &|file| {
            file.ends_with(".yjr") || file.ends_with(".yjf")
        })?;
    Ok(pulled
        .into_iter()
        .map(|(sdr_name, files)| {
            let pick = |suffix: &str| files.iter().find(|(n, _)| n.ends_with(suffix));
            let (yjr, yjf) = (pick(".yjr"), pick(".yjf"));
            CollectedYjr {
                sdr_name,
                yjr_bytes: yjr.map(|(_, b)| b.clone()),
                yjf_bytes: yjf.map(|(_, b)| b.clone()),
                // The filenames are what a write-back has to aim at: they carry
                // a device-specific infix that must be reused, never invented.
                yjr_name: yjr.map(|(n, _)| n.clone()),
                yjf_name: yjf.map(|(n, _)| n.clone()),
            }
        })
        .collect())
}

/// Write Sidle's annotations back into the device's own sidecars.
fn push_device_annotations(
    transport: &dyn Transport,
    db: &impl db::Access,
    collected: &[CollectedYjr],
    device_serial: &str,
    now: &str,
    report: &mut DeviceImportReport,
    on_progress: &dyn Fn(&str, usize, usize, &str),
) -> Result<()> {
    let plan = db.with(|conn| {
        push::plan(conn, collected, &|book| {
            ingest::book_index(book.kfx_path.as_deref())
        })
    })?;
    if plan.is_empty() {
        return Ok(());
    }

    let sidle = TPath::parse("documents/Sidle");
    let total = plan.len();
    for (i, outgoing) in plan.iter().enumerate() {
        on_progress("push", i + 1, total, &outgoing.title);
        let path = sidle.join(&outgoing.sdr_name).join(&outgoing.file_name);
        if let Err(e) = transport.write_atomic(&path, &outgoing.bytes) {
            eprintln!(
                "[sidle/annsync] sidecar write failed for {}: {e:#}",
                outgoing.sdr_name
            );
            continue;
        }
        // Checkpoint what we wrote. The next connect compares the device's file
        let sha = import::sha256_of_bytes(&outgoing.bytes);
        db.with(|conn| db::set_yjr_sync_sha(conn, device_serial, outgoing.book_id, &sha, now))?;
        report.pushed_books += 1;
        report.pushed_annotations += outgoing.added;
    }
    Ok(())
}

/// Import annotations off any connected Kindle into the library — mass-storage
pub fn import_device_annotations(
    device: &DeviceInfo,
    transport: &dyn Transport,
    db: &impl db::Access,
    paths: &LibraryPaths,
    on_progress: &dyn Fn(&str, usize, usize, &str),
) -> Result<DeviceImportReport> {
    let now = db::now_iso();
    let mut report = match device.mass_storage_mount() {
        Some(mount) => {
            // Mass-storage has a real volume — bypass the transport and use
            let imported =
                db.with(|conn| ingest::import_from_device(conn, &mount, &device.serial, &now));
            // Push back over the transport, which for mass-storage is the same
            // local volume — a second walk here is cheap, and it keeps one push
            // implementation for both kinds of device.
            let mut imported = imported?;
            let collected = collect_device_yjr(transport)?;
            if let Err(e) = push_device_annotations(
                transport,
                db,
                &collected,
                &device.serial,
                &now,
                &mut imported,
                on_progress,
            ) {
                eprintln!("[sidle/annsync] annotation push failed (non-fatal): {e:#}");
            }
            Ok::<_, anyhow::Error>(imported)
        }
        None => {
            // The library's content_ids — so the ink walk knows which
            // `.notebooks/<id>!!PDOC!!` dirs are ours (a quick read, lock released
            // before the slow USB walks).
            let known_asins: std::collections::HashSet<String> =
                db.with(db::book_asins)?.into_iter().collect();
            // MTP: do BOTH slow USB walks (annotations + ink notebooks) BEFORE
            // taking the DB lock — see `collect_device_yjr` above.
            let collected = collect_device_yjr(transport)?;
            let inks = ink::collect_device_ink(transport, &known_asins)?;
            // The handwritten-ink anchors live in the same `.yjr`s we just pulled.
            let notes = handwritten_notes(&collected);

            // The push plans off the same sidecars the import just read, so the
            // slow device walk happens once.
            let for_push = collected.clone();

            // One borrow for both imports — they write the same tables about the
            // same sync — released before the push below goes back to USB.
            let report = db.with(|conn| {
                let mut report = ingest::import_collected_with_progress(
                    conn,
                    collected,
                    &device.serial,
                    &now,
                    &|cur, tot, label| on_progress("annotations", cur, tot, label),
                )?;
                import_collected_ink(
                    conn,
                    paths,
                    &device.serial,
                    &now,
                    &inks,
                    &notes,
                    &mut report,
                    &|cur, tot, label| on_progress("ink", cur, tot, label),
                )?;
                Ok::<_, anyhow::Error>(report)
            })?;

            // No on-device cleanup: Sidle never deletes data on the device, and the push
            // below only ever adds to a sidecar.
            let mut report = report;
            if let Err(e) = push_device_annotations(
                transport,
                db,
                &for_push,
                &device.serial,
                &now,
                &mut report,
                on_progress,
            ) {
                eprintln!("[sidle/annsync] annotation push failed (non-fatal): {e:#}");
            }
            Ok(report)
        }
    }?;

    // Additive backup of the device folders the library syncs, over whichever
    // transport this Kindle uses. It must never fail the annotation sync it rides on.
    match device_backup::SyncCollections::load(paths) {
        Ok(config) => match misc::backup_device_misc(transport, &device.serial, paths, &config) {
            Ok(m) => {
                report.misc_new = m.new_files;
                report.misc_refreshed = m.refreshed;
            }
            Err(e) => eprintln!("[sidle/annsync] misc backup failed (non-fatal): {e:#}"),
        },
        Err(e) => eprintln!("[sidle/annsync] device-sync.json unreadable (skipping): {e:#}"),
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::device::mass_storage::transport::MassStorageTransport;

    // Drives the collector through the mass-storage transport (MTP can't be
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
