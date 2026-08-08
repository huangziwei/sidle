//! Build the `.yjr` a device should be holding for a book.
//!
//! The reverse of [`super::ingest`]: that reads a Kindle's sidecar into the
//! library, this turns the library's annotations back into sidecar bytes. Both
//! transports (USB and the LAN picker) compose through here, so there is one
//! answer to "what should the device's file look like" regardless of how it
//! gets there.
//!
//! Three rules shape the output:
//!
//! - **Additive.** The device's own file is read and Sidle's annotations are
//!   merged into it. Records the device holds and Sidle doesn't are left alone,
//!   and so is every non-annotation record — `font.prefs`, `ReaderMetrics`, and
//!   whatever else a firmware version keeps. A sidecar is the device's file that
//!   we contribute to, not ours to author.
//! - **Anchored on the pushed KFX.** A device resolves an annotation by
//!   `(element, offset)` and orders it by a linear position on the book's own
//!   scale, so the positions written here come from the very KFX that was
//!   sideloaded ([`super::anchor::BookIndex`]). Positions off any other build of
//!   the book would sort wrongly on the device.
//! - **Nothing invented.** An annotation with no anchor, or of a kind whose slot
//!   in the file is unknown, is skipped rather than guessed at. Handwritten ink
//!   is never pushed: it is device-authored by nature and Sidle cannot originate
//!   it.

use anyhow::Result;
use rusqlite::Connection;

use super::anchor::BookIndex;
use super::db::{self, AnnotationRow};
use super::ingest::{self, CollectedYjr};
use super::yjr::{Anchor, Annotation, Kind, Store};
use bokai::formats::krds::DEFAULT_COLOR;

/// A sidecar ready to write, and what changed about it.
#[derive(Debug, Clone)]
pub struct Composed {
    /// The whole file to write.
    pub bytes: Vec<u8>,
    /// Annotations added that the device didn't already hold.
    pub added: usize,
    /// Records already in the device's file that were repaired in place — today
    /// only a highlight missing the colour its device needs to be selectable.
    pub repaired: usize,
    /// Annotations that had no anchor, or no known slot, and were left behind.
    pub skipped: usize,
}

/// Compose the sidecar for `book_id` from `current` (the device's file, absent
/// if the book has none yet).
///
/// `colors` says whether the target device understands highlight colours; see
/// [`device_uses_colors`]. A device that doesn't must not be sent one.
///
/// `Ok(None)` when the device already holds everything — the caller writes
/// nothing, which keeps a sync that changes nothing from touching the device at
/// all — and also when the device's own file can't be read.
///
/// **An unreadable sidecar is never overwritten.** Composing fresh over it would
/// replace every record the device holds — its own highlights, and the reader
/// state we have no model for — with just our rows. A file we cannot parse is
/// one we understand least, which makes it the last thing that should be
/// rewritten from scratch. The library keeps its copy regardless, so nothing is
/// stranded by declining; the device's own next write repairs the file.
pub fn compose(
    conn: &Connection,
    book_id: i64,
    current: Option<&[u8]>,
    index: &BookIndex,
    colors: bool,
) -> Result<Option<Composed>> {
    let rows = db::list_annotations_for_book(conn, book_id)?;
    let (records, skipped) = to_records(&rows, index, colors);

    let mut store = match current.map(Store::parse) {
        Some(Ok(store)) => store,
        Some(Err(e)) => {
            eprintln!("[sidle/push] device sidecar unreadable, leaving it alone: {e}");
            return Ok(None);
        }
        None => Store::empty(),
    };

    let added = store.merge_annotations(&records);
    // A colour-capable device needs one on every highlight, including records
    // already in its file — a colourless one there is stuck, since the thing it
    // has lost is the ability to be selected and acted on. Repairing it is the
    // only route back, and it converges: once filled, later syncs find nothing.
    let repaired = if colors {
        store.fill_missing_highlight_colors(DEFAULT_COLOR)
    } else {
        0
    };
    if added == 0 && repaired == 0 {
        return Ok(None);
    }
    Ok(Some(Composed {
        bytes: store.encode(),
        added,
        repaired,
        skipped,
    }))
}

/// One sidecar the caller should write to the device.
#[derive(Debug, Clone)]
pub struct Outgoing {
    /// The `.sdr` directory it goes in.
    pub sdr_name: String,
    /// The exact filename inside it. Never invented — see [`plan`].
    pub file_name: String,
    pub bytes: Vec<u8>,
    pub book_id: i64,
    /// Title, for progress reporting.
    pub title: String,
    pub added: usize,
}

/// Decide what to write back to a device, from the sidecars just collected off it.
///
/// ## Losing to the device's own flush
///
/// A Kindle keeps the open book's reader state in memory and rewrites both its
/// sidecars when it resumes — silently overwriting anything put there in the
/// meantime. The write isn't rejected, it simply loses, with no error and no
/// trace.
///
/// Every book is still offered, including the one just read. The alternative —
/// holding back the most recently read book — sounds safer and is in fact the
/// worst possible rule: the book a user just highlighted in is *always* the one
/// they read most recently, so the one book the feature exists for would be the
/// only one never written.
///
/// Losing that race is cheap, because nothing about this push is destructive.
/// Sidle holds the durable copy either way; [`compose`] merges into whatever the
/// device currently has rather than replacing it; and the write is checkpointed
/// in `yjr_sync`, so a sync that finds the device's file changed underneath
/// simply composes and offers it again. The cost of a lost write is one round
/// trip, and it heals itself without the user knowing there was a race.
///
/// A `.sdr` with neither sidecar is skipped: the filename embeds a
/// device-specific infix, and guessing it would litter the device with files no
/// Kindle would ever read.
///
/// `index_for` supplies a book's anchoring index. It is a parameter rather than
/// a lookup here because building one reads and parses the book's whole KFX:
/// keeping that out of the planner leaves these decisions testable, and lets a
/// caller that already has an index in hand reuse it.
pub fn plan(
    conn: &Connection,
    collected: &[CollectedYjr],
    index_for: &dyn Fn(&db::BookRow) -> BookIndex,
) -> Result<Vec<Outgoing>> {
    let colors = device_uses_colors(collected);
    let mut out = Vec::new();

    for item in collected {
        let Some(file_name) = sidecar_target(item) else {
            continue;
        };
        let Some(book) = ingest::match_collected_book(conn, &item.sdr_name)? else {
            continue; // not a library book — nothing of ours to contribute
        };
        let index = index_for(&book);
        let Some(composed) = compose(conn, book.id, item.yjr_bytes.as_deref(), &index, colors)?
        else {
            continue; // the device already holds everything
        };
        out.push(Outgoing {
            sdr_name: item.sdr_name.clone(),
            file_name,
            bytes: composed.bytes,
            book_id: book.id,
            title: book.title,
            added: composed.added,
        });
    }
    Ok(out)
}

/// Whether this device understands highlight colours, judged by whether it has
/// ever written one.
///
/// **A colour sent to a device that has no colours is not ignored — it is fatal
/// to the whole file.** A monochrome Kindle's highlight record carries five
/// values and its parser has no slot for a sixth: on meeting one it rejects the
/// entire sidecar, renames it aside as `.bad_file`, and starts a new empty one.
/// Every highlight in that book vanishes from the device. Learned by doing it to
/// a real Oasis, 2026-08-08.
///
/// So the test is what the device itself writes, read off the sidecars it just
/// handed us. A colour-capable Kindle names a colour on *every* highlight it
/// makes, including the default yellow, so a single coloured record anywhere on
/// the device settles it. A device holding no highlights at all reads as
/// monochrome, which is the safe way to be wrong: the colour is dropped and the
/// highlight still lands, where the opposite mistake costs the user their
/// annotations.
fn device_uses_colors(collected: &[CollectedYjr]) -> bool {
    collected
        .iter()
        .filter_map(|item| item.yjr_bytes.as_deref())
        .filter_map(|bytes| Store::parse(bytes).ok())
        .flat_map(|store| store.annotations())
        .any(|ann| ann.color.is_some())
}

/// The filename to write. The device's own `.yjr` name when it has one; else
/// the `.yjf`'s with its extension swapped, since both carry the same
/// device-specific infix. `None` when the `.sdr` holds neither.
fn sidecar_target(item: &CollectedYjr) -> Option<String> {
    if let Some(name) = &item.yjr_name {
        return Some(name.clone());
    }
    let yjf = item.yjf_name.as_ref()?;
    Some(format!("{}.yjr", yjf.strip_suffix(".yjf")?))
}

/// Turn library rows into sidecar records, dropping what can't be expressed.
/// Returns the records and how many rows were left behind.
fn to_records(rows: &[AnnotationRow], index: &BookIndex, colors: bool) -> (Vec<Annotation>, usize) {
    let mut out = Vec::with_capacity(rows.len());
    let mut skipped = 0;
    for row in rows {
        match to_record(row, index, colors) {
            Some(r) => out.push(r),
            None => skipped += 1,
        }
    }
    (out, skipped)
}

fn to_record(row: &AnnotationRow, index: &BookIndex, colors: bool) -> Option<Annotation> {
    let kind = Kind::parse(&row.kind);
    // Ink is the device's to make; a kind with no known slot would have to be
    // filed by guesswork. Neither belongs in a file we hand to firmware.
    if kind == Kind::Handwritten || kind.cache_key().is_none() {
        return None;
    }
    // A hidden annotation is still the user's, and hiding is a Sidle-side view
    // choice — but pushing one would make it visible on the device, which is the
    // opposite of what hiding asked for.
    if row.hidden {
        return None;
    }

    let eid_start = row.eid_start?;
    let off_start = row.off_start.unwrap_or(0);
    // A bookmark is a point: it repeats its start, exactly as devices write it.
    let eid_end = row.eid_end.unwrap_or(eid_start);
    let off_end = row.off_end.unwrap_or(off_start);

    // Positions come from the pushed KFX. Without them the record would sort
    // wrongly in the device's list, so a row we can't place is skipped.
    let start = Anchor::new(eid_start, off_start, index.position(eid_start, off_start)?);
    let end = Anchor::new(eid_end, off_end, index.position(eid_end, off_end)?);

    let created = row
        .added_at
        .as_deref()
        .and_then(epoch_ms)
        .or_else(|| epoch_ms(&row.imported_at))
        .unwrap_or(0);

    Some(Annotation {
        color: color_for(row, &kind, colors),
        kind,
        anchors: vec![start, end],
        body: row.note_body.clone().filter(|b| !b.is_empty()),
        created_ms: Some(created),
        modified_ms: Some(created),
    })
}

/// The colour value to write for one record, if any.
///
/// The two device families want opposite things, and getting either wrong is
/// visible on the device:
///
/// - **Monochrome**: no colour value, ever. Its parser has no slot for one and
///   rejects the whole sidecar on meeting it, losing every highlight in the book.
/// - **Colour-capable**: a colour on every highlight, including one Sidle has
///   none recorded for. Such a device names a colour on everything it marks, and
///   a highlight record without one displays but cannot be *selected* — no
///   toolbar, so it can't be recoloured, annotated or deleted on the device.
///   Observed on a Colorsoft, 2026-08-09.
///
/// A row with no colour is one captured on a monochrome device, where the user
/// never chose one; [`DEFAULT_COLOR`] is what a Kindle marks with by default, so
/// it is the least surprising thing to give the passage.
///
/// Only highlights. A note carries no colour on a colour-capable device either —
/// it is read through the highlight it hangs on.
fn color_for(row: &AnnotationRow, kind: &Kind, colors: bool) -> Option<String> {
    if !colors {
        return None;
    }
    let recorded = row.color.clone().filter(|c| !c.is_empty());
    match kind {
        Kind::Highlight => Some(recorded.unwrap_or_else(|| DEFAULT_COLOR.to_string())),
        _ => recorded,
    }
}

/// An ISO-8601 timestamp as epoch milliseconds, which is how the format stamps
/// a record.
fn epoch_ms(iso: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|t| t.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::db::{NewAnnotation, NewBook};
    use std::path::Path;

    fn mem_db() -> Connection {
        db::open(Path::new(":memory:")).unwrap()
    }

    /// Two elements with text, so anchors resolve to real positions.
    fn index() -> BookIndex {
        BookIndex::from_parts(
            [(10, "Hello world".to_string()), (20, "Second".to_string())]
                .into_iter()
                .collect(),
            [(10, 100), (20, 200)].into_iter().collect(),
        )
    }

    fn add_book(conn: &Connection) -> i64 {
        db::insert_book(
            conn,
            &NewBook {
                sha256: "sha",
                title: "A Book",
                author: "Author",
                language: "en",
                ppd: None,
                epub_path: None,
                cover_path: None,
                kfx_path: None,
                kfx_sha256: None,
                pdf_path: None,
                file_size: 0,
                imported_at: "2026-01-01T00:00:00+00:00",
                asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
                title_romaji: "",
                author_romaji: "",
            },
        )
        .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn add_annotation(
        conn: &Connection,
        book: i64,
        hash: &str,
        kind: &str,
        eid: Option<i64>,
        color: Option<&str>,
        note: Option<&str>,
        hidden: bool,
    ) {
        db::insert_annotation(
            conn,
            &NewAnnotation {
                dedup_hash: hash,
                book_id: Some(book),
                kind,
                eid_start: eid,
                off_start: Some(0),
                eid_end: eid,
                off_end: Some(4),
                loc_start: None,
                loc_end: None,
                linear_pos: None,
                text: "Hello",
                note_body: note,
                color,
                clip_title: None,
                clip_author: None,
                added_at: Some("2026-08-08T10:24:49+00:00"),
                added_raw: None,
                imported_at: "2026-08-08T10:24:49+00:00",
                source: "sidle",
            },
        )
        .unwrap();
        if hidden {
            let id = db::list_annotations_for_book(conn, book)
                .unwrap()
                .into_iter()
                .find(|r| r.dedup_hash == hash)
                .unwrap()
                .id;
            db::set_annotation_hidden(conn, id, true).unwrap();
        }
    }

    #[test]
    fn composes_a_sidecar_for_a_device_that_has_none() {
        let conn = mem_db();
        let book = add_book(&conn);
        add_annotation(
            &conn,
            book,
            "h1",
            "highlight",
            Some(10),
            Some("blue"),
            None,
            false,
        );

        let out = compose(&conn, book, None, &index(), true)
            .unwrap()
            .expect("something to write");
        assert_eq!(out.added, 1);
        assert_eq!(out.skipped, 0);

        let anns = Store::parse(&out.bytes).unwrap().annotations();
        assert_eq!(anns.len(), 1);
        assert_eq!(anns[0].color.as_deref(), Some("blue"));
        // Positions come from the index, not from the row.
        assert_eq!(anns[0].start().unwrap().position, 100);
        assert_eq!(anns[0].end().unwrap().position, 104);
        assert_eq!(anns[0].created_ms, Some(1_786_184_689_000));
    }

    #[test]
    fn nothing_to_add_writes_nothing() {
        let conn = mem_db();
        let book = add_book(&conn);
        add_annotation(&conn, book, "h1", "highlight", Some(10), None, None, false);

        let first = compose(&conn, book, None, &index(), true).unwrap().unwrap();
        // Composing again against what we just produced is a no-op.
        assert!(
            compose(&conn, book, Some(&first.bytes), &index(), true)
                .unwrap()
                .is_none(),
            "a device already holding everything must not be written to",
        );
    }

    #[test]
    fn the_devices_own_records_and_preferences_survive() {
        let conn = mem_db();
        let book = add_book(&conn);
        add_annotation(&conn, book, "h1", "highlight", Some(20), None, None, false);

        // A device file with its own highlight and a preference record.
        let mut device = Store::empty();
        device.merge_annotations(&[Annotation::highlight(
            Anchor::new(10, 0, 100),
            Anchor::new(10, 4, 104),
            1,
            Some("orange"),
        )]);
        device.roots.push(crate::library::yjr::Object {
            name: "font.prefs".into(),
            values: vec![crate::library::yjr::Value::Utf8(Some(
                "_INVALID_,en:bookerly".into(),
            ))],
        });
        let before = device.encode();

        let out = compose(&conn, book, Some(&before), &index(), true)
            .unwrap()
            .unwrap();
        assert_eq!(out.added, 1, "only ours is new");

        let after = Store::parse(&out.bytes).unwrap();
        assert_eq!(after.annotations().len(), 2, "the device's is kept");
        assert_eq!(
            after.annotations()[0].color.as_deref(),
            Some("orange"),
            "and keeps its colour",
        );
        assert!(
            after.root("font.prefs").is_some(),
            "records we know nothing about are carried across",
        );
    }

    #[test]
    fn rows_that_cannot_be_expressed_are_skipped_not_guessed() {
        let conn = mem_db();
        let book = add_book(&conn);
        add_annotation(&conn, book, "ok", "highlight", Some(10), None, None, false);
        // No anchor at all.
        add_annotation(
            &conn,
            book,
            "anchorless",
            "highlight",
            None,
            None,
            None,
            false,
        );
        // An element this book's index doesn't place.
        add_annotation(
            &conn,
            book,
            "offbook",
            "highlight",
            Some(999),
            None,
            None,
            false,
        );
        // Ink — the device's to make, never ours.
        add_annotation(
            &conn,
            book,
            "ink",
            "handwritten_note",
            Some(10),
            None,
            Some("c0"),
            false,
        );
        // Hidden in Sidle: pushing it would undo the hiding.
        add_annotation(
            &conn,
            book,
            "hidden",
            "highlight",
            Some(20),
            None,
            None,
            true,
        );

        let out = compose(&conn, book, None, &index(), true).unwrap().unwrap();
        assert_eq!(out.added, 1, "only the expressible one");
        assert_eq!(out.skipped, 4);
        assert_eq!(Store::parse(&out.bytes).unwrap().annotations().len(), 1);
    }

    /// A file we can't parse is the one we understand least — replacing it with
    /// a freshly composed one would throw away every record the device holds.
    /// The library keeps its copy either way, so declining costs nothing.
    #[test]
    fn a_damaged_device_sidecar_is_left_alone_not_overwritten() {
        let conn = mem_db();
        let book = add_book(&conn);
        add_annotation(&conn, book, "h1", "highlight", Some(10), None, None, false);

        assert!(
            compose(&conn, book, Some(b"this is not a sidecar"), &index(), true)
                .unwrap()
                .is_none(),
            "a sidecar we cannot read must never be rewritten from scratch",
        );
    }

    /// The incident this rule exists for: a colour written to a monochrome
    /// Kindle is not ignored, it invalidates the entire sidecar. The device
    /// quarantines the file as `.bad_file` and starts an empty one, so every
    /// highlight in that book disappears from the device.
    #[test]
    fn a_monochrome_device_is_never_sent_a_colour() {
        let conn = mem_db();
        let book = add_book(&conn);
        add_annotation(
            &conn,
            book,
            "h1",
            "highlight",
            Some(10),
            Some("orange"),
            None,
            false,
        );

        let out = compose(&conn, book, None, &index(), false)
            .unwrap()
            .expect("the highlight still goes");
        assert_eq!(
            out.added, 1,
            "the highlight is pushed, only the colour drops"
        );
        let anns = Store::parse(&out.bytes).unwrap().annotations();
        assert_eq!(anns[0].color, None, "no colour value may reach the file");

        // The same row against a colour-capable device keeps it.
        let colored = compose(&conn, book, None, &index(), true).unwrap().unwrap();
        assert_eq!(
            Store::parse(&colored.bytes).unwrap().annotations()[0]
                .color
                .as_deref(),
            Some("orange"),
        );
    }

    /// The repair path. A colourless highlight already sitting in the device's
    /// file is exactly the one the user cannot fix themselves — being unable to
    /// select it is the whole symptom — so writing future records correctly is
    /// not enough on its own.
    #[test]
    fn a_colourless_highlight_already_on_the_device_is_repaired() {
        let conn = mem_db();
        let book = add_book(&conn);
        add_annotation(&conn, book, "h", "highlight", Some(10), None, None, false);

        // The device's file, holding that same highlight with no colour — the
        // shape a monochrome-origin push left on a Colorsoft.
        let mut device = Store::empty();
        device.merge_annotations(&[Annotation::highlight(
            Anchor::new(10, 0, 100),
            Anchor::new(10, 4, 104),
            1,
            None,
        )]);
        let before = device.encode();
        assert!(
            Store::parse(&before).unwrap().annotations()[0]
                .color
                .is_none()
        );

        let out = compose(&conn, book, Some(&before), &index(), true)
            .unwrap()
            .expect("a repair is worth a write even with nothing new to add");
        assert_eq!(out.added, 0, "the span is already there");
        assert_eq!(out.repaired, 1);
        assert_eq!(
            Store::parse(&out.bytes).unwrap().annotations()[0]
                .color
                .as_deref(),
            Some(DEFAULT_COLOR),
        );

        // Converges: composing again against the repaired file writes nothing.
        assert!(
            compose(&conn, book, Some(&out.bytes), &index(), true)
                .unwrap()
                .is_none(),
            "the repair must not re-trigger on every sync",
        );
    }

    /// The mirror of the monochrome rule, and the second half of getting colour
    /// right: a colour-capable device wants a colour on *every* highlight. One
    /// written without it displays on the device but cannot be selected — no
    /// toolbar, so it can't be recoloured, annotated or deleted there. A row
    /// captured on a monochrome Kindle has no colour recorded, and that is
    /// exactly the row this would strand.
    #[test]
    fn a_colour_device_gets_a_colour_on_every_highlight() {
        let conn = mem_db();
        let book = add_book(&conn);
        // No colour recorded — captured on a monochrome device.
        add_annotation(
            &conn,
            book,
            "plain",
            "highlight",
            Some(10),
            None,
            None,
            false,
        );
        // A note, which carries no colour on any device.
        add_annotation(
            &conn,
            book,
            "n",
            "note",
            Some(20),
            None,
            Some("a thought"),
            false,
        );

        let out = compose(&conn, book, None, &index(), true).unwrap().unwrap();
        let anns = Store::parse(&out.bytes).unwrap().annotations();
        let hl = anns.iter().find(|a| a.kind == Kind::Highlight).unwrap();
        assert_eq!(
            hl.color.as_deref(),
            Some(DEFAULT_COLOR),
            "an uncoloured highlight would be inert on the device",
        );
        let note = anns.iter().find(|a| a.kind == Kind::Note).unwrap();
        assert_eq!(note.color, None, "a note is read through its highlight");

        // And the monochrome side is unchanged: still no colour at all, since
        // one there costs the user the whole file.
        let mono = compose(&conn, book, None, &index(), false)
            .unwrap()
            .unwrap();
        assert!(
            Store::parse(&mono.bytes)
                .unwrap()
                .annotations()
                .iter()
                .all(|a| a.color.is_none()),
            "a monochrome device must never receive a colour value",
        );
    }

    /// Capability is read off what the device itself wrote. A colour-capable
    /// Kindle names a colour on every highlight it makes, so one is enough; a
    /// device with none reads as monochrome, which is the safe way to be wrong.
    #[test]
    fn colour_support_is_judged_by_what_the_device_has_written() {
        let plain = |color: Option<&str>| {
            let mut store = Store::empty();
            store.merge_annotations(&[Annotation::highlight(
                Anchor::new(10, 0, 100),
                Anchor::new(10, 4, 104),
                1,
                color,
            )]);
            store.encode()
        };
        let with = |bytes: Option<Vec<u8>>| CollectedYjr {
            sdr_name: "x.sdr".into(),
            yjr_bytes: bytes,
            yjf_bytes: None,
            yjr_name: Some("x.yjr".into()),
            yjf_name: None,
        };

        assert!(
            !device_uses_colors(&[]),
            "nothing known reads as monochrome"
        );
        assert!(!device_uses_colors(&[with(None)]));
        assert!(!device_uses_colors(&[with(Some(plain(None)))]));
        assert!(device_uses_colors(&[with(Some(plain(Some("pink"))))]));
        // One coloured record anywhere on the device settles it.
        assert!(device_uses_colors(&[
            with(Some(plain(None))),
            with(Some(plain(Some("blue")))),
        ]));
        // Unparseable files can't testify either way and must not crash it.
        assert!(!device_uses_colors(&[with(Some(b"junk".to_vec()))]));
    }

    /// A `.yjf` whose `lpr` says when the book was last read.
    fn yjf_read_at(ms: i64) -> Vec<u8> {
        use crate::library::yjr::{Object, Value};
        Store {
            version: 1,
            roots: vec![Object {
                name: "lpr".into(),
                values: vec![
                    Value::Byte(2),
                    Value::Utf8(Some(Anchor::new(10, 0, 100).encode())),
                    Value::Long(ms),
                ],
            }],
        }
        .encode()
    }

    fn collected(sdr: &str, yjr: Option<&str>, yjf: Option<(&str, i64)>) -> CollectedYjr {
        CollectedYjr {
            sdr_name: sdr.to_string(),
            yjr_bytes: None,
            yjf_bytes: yjf.map(|(_, ms)| yjf_read_at(ms)),
            yjr_name: yjr.map(str::to_string),
            yjf_name: yjf.map(|(n, _)| n.to_string()),
        }
    }

    /// The book the device read most recently is the one the user was just
    /// highlighting in, so it is the one that most needs writing. Holding it
    /// back to dodge the device's flush would disable the feature exactly where
    /// it is wanted; a lost write is retried on the next sync instead.
    #[test]
    fn the_book_the_device_read_last_is_written_like_any_other() {
        let conn = mem_db();
        book_with_kfx_sha(&conn, "Older", &format!("aaaaaaaa{}", "0".repeat(56)));
        book_with_kfx_sha(&conn, "Newest", &format!("bbbbbbbb{}", "0".repeat(56)));
        let items = vec![
            collected(
                "Older.aaaaaaaa.sdr",
                Some("Older.aaaaaaaa1.yjr"),
                Some(("Older.aaaaaaaa1.yjf", 500)),
            ),
            collected(
                "Newest.bbbbbbbb.sdr",
                Some("Newest.bbbbbbbb1.yjr"),
                Some(("Newest.bbbbbbbb1.yjf", 900)),
            ),
        ];
        let plan = plan(&conn, &items, &|_| index()).unwrap();
        assert_eq!(plan.len(), 2, "no book is held back on a read timestamp");
        let newest = plan
            .iter()
            .find(|o| o.sdr_name == "Newest.bbbbbbbb.sdr")
            .expect("the most recently read book is planned too");
        assert_eq!(newest.title, "Newest");
        assert_eq!(newest.file_name, "Newest.bbbbbbbb1.yjr");
        assert_eq!(newest.added, 1);
    }

    /// A book the `.sdr` infix can find, carrying one highlight to push.
    fn book_with_kfx_sha(conn: &Connection, title: &str, kfx_sha: &str) -> i64 {
        let id = db::insert_book(
            conn,
            &NewBook {
                sha256: title,
                title,
                author: "Author",
                language: "en",
                ppd: None,
                epub_path: None,
                cover_path: None,
                kfx_path: Some("/nonexistent.kfx"),
                kfx_sha256: Some(kfx_sha),
                pdf_path: None,
                file_size: 0,
                imported_at: "2026-01-01T00:00:00+00:00",
                asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
                title_romaji: "",
                author_romaji: "",
            },
        )
        .unwrap();
        add_annotation(conn, id, title, "highlight", Some(10), None, None, false);
        id
    }

    #[test]
    fn a_sidecar_filename_is_taken_from_the_device_never_invented() {
        // The device's own `.yjr` name wins.
        assert_eq!(
            sidecar_target(&collected(
                "x.sdr",
                Some("x0000.yjr"),
                Some(("x0000.yjf", 1))
            ))
            .as_deref(),
            Some("x0000.yjr"),
        );
        // A book read but never annotated: the infix comes off the `.yjf`.
        assert_eq!(
            sidecar_target(&collected("x.sdr", None, Some(("book.abc123def.yjf", 1)))).as_deref(),
            Some("book.abc123def.yjr"),
            "the device-specific infix is carried across, not guessed",
        );
        // Neither sidecar: there is no infix to be had, so nothing is written.
        assert_eq!(sidecar_target(&collected("x.sdr", None, None)), None);
    }

    #[test]
    fn an_sdr_with_no_sidecars_at_all_is_left_alone() {
        let conn = mem_db();
        let book = add_book(&conn);
        add_annotation(&conn, book, "h1", "highlight", Some(10), None, None, false);
        let plan = plan(&conn, &[collected("sha.aaaaaaaa.sdr", None, None)], &|_| {
            index()
        })
        .unwrap();
        assert!(plan.is_empty());
    }

    #[test]
    fn an_sdr_matching_no_library_book_is_left_alone() {
        let conn = mem_db();
        add_book(&conn);
        let plan = plan(
            &conn,
            &[collected("nothing-here.99999999.sdr", Some("n.yjr"), None)],
            &|_| index(),
        )
        .unwrap();
        assert!(
            plan.is_empty(),
            "we contribute nothing to a book we don't have"
        );
    }

    #[test]
    fn a_note_carries_its_body_and_a_bookmark_its_point() {
        let conn = mem_db();
        let book = add_book(&conn);
        add_annotation(
            &conn,
            book,
            "n",
            "note",
            Some(10),
            None,
            Some("a thought"),
            false,
        );
        add_annotation(&conn, book, "b", "bookmark", Some(20), None, None, false);

        let out = compose(&conn, book, None, &index(), true).unwrap().unwrap();
        assert_eq!(out.added, 2);
        let anns = Store::parse(&out.bytes).unwrap().annotations();
        let note = anns.iter().find(|a| a.kind == Kind::Note).unwrap();
        assert_eq!(note.body.as_deref(), Some("a thought"));
        let mark = anns.iter().find(|a| a.kind == Kind::Bookmark).unwrap();
        assert_eq!(mark.start().unwrap().eid, 20);
    }
}
