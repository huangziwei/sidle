//! Reading-session events the Kindle logs about itself, collected for sync.
//!
//! The firmware writes `/var/local/log/messages` continuously and, at most once
//! a day, gzips a snapshot of it into `system/logbackup/`. Under the `cvm` tag
//! those lines carry `ReadingTimerController` events — a dated record of every
//! page turn — which the desktop turns into a per-day reading log.
//!
//! **This module does no parsing.** It selects lines and sends them; the session
//! rules (running counters, gap splitting, the two end-of-book constants) live
//! in one place on the desktop, and a second implementation here would be the
//! one that drifts.
//!
//! The whole design is about *not* reading things. A full archive is ~90 MB of
//! gzip; re-reading it every Sync to discover there is nothing new costs seconds
//! on a Mac and far worse here. So the desktop states a watermark — the newest
//! event it already holds from this device — and:
//!
//! - a dump whose **filename** timestamp is at or before it is skipped unopened,
//!   because a snapshot taken at time T contains nothing after T;
//! - only the recent tail of the syslog is read in the steady state, and only
//!   its lines newer than the watermark are kept.
//!
//! With nothing new since the last sync that is two directory listings and a
//! scan of the newest syslog chunks, and an empty push that never leaves the
//! device.
//!
//! **The live file alone is nowhere near enough.** `tinyrot` rotates
//! `/var/local/log/messages` on a size cap, and on a busy device that is roughly
//! every ten minutes — one measured dump spanned 230 rotations in 38 hours. What
//! it rotates away is not lost: it becomes
//! `/var/local/log/messages_<seq>_<YYYYMMDDHHMMSS>.gz` beside it. Reading only
//! the live file would therefore see about ten minutes of history and leave the
//! rest to arrive a day later in the next dump — or not at all, since `tinyrot`
//! prunes chunks and the dumps demonstrably skip content when it does (a real
//! archive lost 20 h of one day between two consecutive daily dumps).
//!
//! So the chunks are read too, filtered by the same watermark. That is not an
//! invention: it is what the firmware's own `showlog` does, and a dump is
//! nothing more than its output gzipped —
//!
//! ```text
//! ALLFILES=`ls -1 $ARCHIVE_DIR/${LOG}_*.gz | xargs`
//! cat $ALLFILES | zcat >> "$OUTFILE"
//! cat /var/log/$LOG >> "$OUTFILE"
//! ```
//!
//! Reading the same set directly is what removes the wait for a daily snapshot,
//! and the dumps then matter only for history older than the chunks still on
//! disk — which is also all a host can reach, `/var/local/log` being on the root
//! filesystem.

use std::io::Read;
use std::path::{Path, PathBuf};

/// The tag every reading event carries; the cheap prefilter before anything else.
const MARKER: &str = "ReadingTimerController";

/// The live syslog the firmware appends to, and the source every dump is a
/// snapshot of. Absolute: it is on the root filesystem, not under `/mnt/us`.
const LIVE_LOG: &str = "/var/local/log/messages";

/// The directory holding [`LIVE_LOG`] and its rotated chunks.
const LOG_DIR: &str = "/var/local/log";

/// What a rotated chunk's name begins with: `messages_00000807_20260807101501.gz`.
/// The trailing `_` is what separates it from `messages` itself.
const CHUNK_PREFIX: &str = "messages_";

/// Where the firmware keeps its daily snapshots, relative to `/mnt/us`.
const DUMP_DIR: &str = "system/logbackup";

/// What one collection pass looked at, for the sync log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Collected {
    /// Distinct event lines newer than the watermark.
    pub lines: Vec<String>,
    /// Names of the dumps read **in full**, sent alongside the lines so the
    /// desktop can record them and skip them next time. A dump that decoded only
    /// partway is deliberately absent, so the next sync reads it again.
    pub read: Vec<String>,
    /// Dumps skipped without being opened — the work this avoids is the entire
    /// point.
    pub skipped: usize,
    /// Dumps that decoded only partway, usually because the firmware was still
    /// writing one. Their prefix was taken; they are not reported as read.
    pub truncated: usize,
}

/// The `YYMMDD:HHMMSS` a dump's name encodes, matching the prefix its lines
/// carry so the two compare directly.
///
/// `log_backup_260809005124.txt.gz` was written at `260809:005124`.
fn dump_stamp(name: &str) -> Option<String> {
    let digits: String = name
        .strip_prefix("log_backup_")?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (digits.len() == 12).then(|| format!("{}:{}", &digits[..6], &digits[6..]))
}

/// The `YYMMDD:HHMMSS` a syslog line starts with, or `None` if it starts with
/// something else.
fn line_stamp(line: &str) -> Option<&str> {
    let b = line.as_bytes();
    if b.len() < 13 || b[6] != b':' {
        return None;
    }
    let s = &line[..13];
    s.bytes()
        .all(|c| c.is_ascii_digit() || c == b':')
        .then_some(s)
}

/// Every reading event newer than `watermark`, from three sources: the dumps the
/// desktop has not already read, the rotated syslog chunks, and the live log.
///
/// `seen` names snapshots the desktop has read in full — including ones imported
/// from a copy of this folder on the desktop, so a host-side backfill primes the
/// device sync. `watermark` is `YYMMDD:HHMMSS` and filters the chunks and the
/// live log, which have no stable names worth recording. Both empty means a
/// Kindle the desktop has never seen, so everything is read once.
///
/// The three sources overlap heavily and that is intended — the desktop
/// de-duplicates. What matters is that between them nothing is skipped, which
/// the dumps alone do not achieve.
pub fn collect(us_root: &Path, watermark: &str, seen: &[String]) -> Collected {
    let mut out = Collected::default();
    // Deliberately a plain Vec plus a sort: the desktop de-duplicates anyway
    // (dumps overlap heavily by design), and a set of every line would hold the
    // whole selection in memory twice on a device with 512 MB shared with the
    // framework.
    for (name, path) in dumps(us_root, watermark, seen, &mut out) {
        if let Some(dump) = read_maybe_gzip(&path) {
            // Its lines go either way; only a dump decoded to the end is
            // reported as read, so a half-written one is picked up again next
            // sync instead of being skipped on its name forever.
            if dump.complete {
                out.read.push(name);
            } else {
                out.truncated += 1;
            }
            take_events(&dump.text, watermark, &mut out.lines);
        }
    }
    // The live file, then the rotated chunks — the same set the firmware's own
    // `showlog` concatenates, which is what makes a sync independent of whether a
    // dump was ever written for the period.
    //
    // Deliberately the reverse of `showlog`'s order, because `showlog` holds
    // tinyrot's lock for the duration and this does not. Read the chunks first
    // and a rotation in the gap moves the live file's contents into a chunk
    // listed a moment too early, and those lines are in neither read. Live
    // first, and the same rotation merely delivers them twice — which costs
    // nothing, since the whole selection is de-duplicated below.
    //
    // Not `read_to_string`: the syslog carries bytes that are not valid UTF-8,
    // and that returns `Err` for the whole file rather than for the offending
    // line — which would silently drop every event since the last rotation.
    if let Some(live) = read_maybe_gzip(Path::new(LIVE_LOG)) {
        take_events(&live.text, watermark, &mut out.lines);
    }
    for path in chunks(Path::new(LOG_DIR), watermark, &mut out) {
        // A chunk pruned between the listing and the read simply yields nothing;
        // one caught mid-rotation yields its intact prefix. Neither needs the
        // lock, which is why not taking it is acceptable here.
        if let Some(chunk) = read_maybe_gzip(&path) {
            take_events(&chunk.text, watermark, &mut out.lines);
        }
    }
    out.lines.sort();
    out.lines.dedup();
    out
}

/// The `YYMMDD:HHMMSS` a rotated chunk's name encodes, from its 14-digit
/// `YYYYMMDDHHMMSS` field: `messages_00000807_20260807101501.gz` →
/// `260807:101501`. The century is dropped so it compares directly against the
/// stamp a syslog line carries.
fn chunk_stamp(name: &str) -> Option<String> {
    let digits: String = name
        .strip_prefix(CHUNK_PREFIX)?
        .rsplit_once('.')?
        .0
        .rsplit_once('_')?
        .1
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (digits.len() == 14).then(|| format!("{}:{}", &digits[2..8], &digits[8..]))
}

/// The rotated syslog chunks worth opening, oldest first.
///
/// The stamp in a chunk's name is the rotation instant, and it is not documented
/// which side of the content that is — the chunk closed then, or the next one
/// opened then. So the filter is deliberately loose: everything stamped after the
/// watermark, **plus the newest chunk stamped at or before it**, which is the one
/// that can straddle. Correct under either reading, at the cost of one extra
/// chunk. Line-level filtering in [`take_events`] is what actually guarantees
/// nothing already held is sent, so a chunk read needlessly costs time only.
fn chunks(dir: &Path, watermark: &str, out: &mut Collected) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dated: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // An unparseable name is read rather than skipped, on the same principle
        // as the dumps: spend the gunzip instead of silently dropping a period.
        match chunk_stamp(&name) {
            Some(stamp) => dated.push((stamp, entry.path())),
            None if name.starts_with(CHUNK_PREFIX) => dated.push((String::new(), entry.path())),
            None => {}
        }
    }
    dated.sort();
    if watermark.is_empty() {
        return dated.into_iter().map(|(_, p)| p).collect();
    }
    // The straddle chunk is the last one at or before the watermark.
    let first = dated
        .iter()
        .rposition(|(stamp, _)| stamp.as_str() <= watermark)
        .unwrap_or(0);
    out.skipped += first;
    dated.split_off(first).into_iter().map(|(_, p)| p).collect()
}

/// The dumps worth opening. Two independent reasons to skip one, both decided
/// from its name alone and neither needing the file to be touched: the desktop
/// has already read it, or it was written before the newest event the desktop
/// holds — a snapshot taken at time T contains nothing after T.
fn dumps(
    us_root: &Path,
    watermark: &str,
    seen: &[String],
    out: &mut Collected,
) -> Vec<(String, PathBuf)> {
    let dir = us_root.join(DUMP_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        // No logbackup directory (or no permission): the live log alone still
        // carries everything since the last rotation, which in the steady state
        // is all there is.
        return Vec::new();
    };
    let mut keep = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if seen.contains(&name) {
            out.skipped += 1;
            continue;
        }
        // An unparseable name is read rather than skipped: better to spend the
        // gunzip than to silently drop a day because the firmware renamed
        // something.
        match dump_stamp(&name) {
            Some(stamp) if !watermark.is_empty() && stamp.as_str() <= watermark => out.skipped += 1,
            _ => keep.push((name, entry.path())),
        }
    }
    keep.sort();
    keep
}

/// Append every event line in `text` that is newer than `watermark`.
fn take_events(text: &str, watermark: &str, out: &mut Vec<String>) {
    for line in text.lines() {
        if !line.contains(MARKER) {
            continue;
        }
        // A line the desktop already holds is not worth a byte on the wire. A
        // line with no timestamp is kept: it is cheaper to send than to reason
        // about, and the desktop's parser ignores what it cannot place.
        match line_stamp(line) {
            Some(stamp) if !watermark.is_empty() && stamp <= watermark => continue,
            _ => out.push(line.to_string()),
        }
    }
}

/// A decoded dump, and whether it decoded all the way to the end.
struct Decoded {
    text: String,
    /// False when the archive was truncated, so `text` is a valid prefix rather
    /// than the whole file. Such a dump must not be reported as read.
    complete: bool,
}

/// Decode a log file, gunzipping it when it is gzipped. A truncated dump yields
/// its intact prefix — the firmware rotates while writing, and the newest dump
/// is routinely cut short, but its prefix is perfectly good data.
///
/// The decode error is kept rather than discarded: on this side the likeliest
/// truncation is a live one — `log_backup.sh` gzipping a dump at the very moment
/// a sync reads it — and that file will be complete a minute later. Reporting it
/// as read would make the desktop skip it forever on a name that never changes.
fn read_maybe_gzip(path: &Path) -> Option<Decoded> {
    let bytes = std::fs::read(path).ok()?;
    // An empty file is a dump the firmware has created but not yet written, not
    // a log with nothing in it — so it is not read, and not reported as read.
    if bytes.is_empty() {
        return None;
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut buf = Vec::new();
        let complete = flate2::read::GzDecoder::new(&bytes[..])
            .read_to_end(&mut buf)
            .is_ok();
        (!buf.is_empty()).then(|| Decoded {
            text: String::from_utf8_lossy(&buf).into_owned(),
            complete,
        })
    } else {
        Some(Decoded {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            complete: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(stamp: &str) -> String {
        format!("{stamp} cvm[6144]: I ReadingTimerController:Information::NextPage,Title:<private>")
    }

    #[test]
    fn a_dumps_name_states_when_it_was_written() {
        assert_eq!(
            dump_stamp("log_backup_260809005124.txt.gz").as_deref(),
            Some("260809:005124")
        );
        assert_eq!(dump_stamp("messages"), None);
        assert_eq!(dump_stamp("log_backup_2608.txt.gz"), None);
    }

    #[test]
    fn a_dump_older_than_the_watermark_is_never_opened() {
        let dir = tempdir();
        let backup = dir.join(DUMP_DIR);
        std::fs::create_dir_all(&backup).unwrap();
        // Two dumps: one from before the desktop's watermark, one from after.
        for name in [
            "log_backup_260701000000.txt.gz",
            "log_backup_260809000000.txt.gz",
        ] {
            std::fs::write(backup.join(name), b"not even gzip").unwrap();
        }
        let mut out = Collected::default();
        let keep = dumps(&dir, "260801:000000", &[], &mut out);

        assert_eq!(
            out.skipped, 1,
            "the older dump is skipped on its name alone"
        );
        assert_eq!(keep.len(), 1);
        assert!(keep[0].0.contains("260809"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_watermark_reads_everything_once() {
        let dir = tempdir();
        let backup = dir.join(DUMP_DIR);
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::write(backup.join("log_backup_260701000000.txt.gz"), b"x").unwrap();
        let mut out = Collected::default();
        // A Kindle the desktop has never seen has no watermark, and must not
        // read that as "everything is already known".
        assert_eq!(dumps(&dir, "", &[], &mut out).len(), 1);
        assert_eq!(out.skipped, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_events_after_the_watermark_are_taken() {
        let text = [
            event("260808:120000"),
            event("260809:120000"),
            "260809:130000 cvm[1]: I Something else entirely".to_string(),
        ]
        .join("\n");
        let mut lines = Vec::new();
        take_events(&text, "260808:120000", &mut lines);
        // The line *at* the watermark is already held; the later one is new; the
        // non-event line is not ours whatever its timestamp.
        assert_eq!(lines, vec![event("260809:120000")]);
    }

    #[test]
    fn a_line_the_desktop_already_holds_costs_nothing_on_the_wire() {
        let text = event("260701:090000");
        let mut lines = Vec::new();
        take_events(&text, "260809:000000", &mut lines);
        assert!(lines.is_empty());
    }

    /// A dump caught mid-write yields its prefix but is not reported as read, so
    /// the next sync picks it up complete.
    ///
    /// This is the likeliest truncation on the device: `log_backup.sh` gzips a
    /// snapshot at whatever moment the firmware decides, which can be during a
    /// sync. Names never change, so reporting a half-decoded dump as read would
    /// have the desktop skip it unopened for good.
    #[test]
    fn a_half_written_dump_is_taken_but_not_reported_as_read() {
        use std::io::Write;
        let dir = tempdir();
        let backup = dir.join(DUMP_DIR);
        std::fs::create_dir_all(&backup).unwrap();

        let whole = format!("{}\n{}\n", event("260809:100000"), event("260809:100500"));
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(whole.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();

        let name = "log_backup_260809101500.txt.gz";
        std::fs::write(backup.join(name), &gz[..gz.len() - 12]).unwrap();
        let cut = collect(&dir, "", &[]);
        assert_eq!(cut.truncated, 1);
        assert!(
            cut.read.is_empty(),
            "the desktop must not be told this one is done with"
        );

        // Complete, a minute later, under the same name.
        std::fs::write(backup.join(name), &gz).unwrap();
        let full = collect(&dir, "", &[]);
        assert_eq!(full.truncated, 0);
        assert_eq!(full.read, vec![name.to_string()]);
        assert_eq!(full.lines.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rotated_chunks_name_states_when_it_was_rotated() {
        assert_eq!(
            chunk_stamp("messages_00000807_20260807101501.gz").as_deref(),
            Some("260807:101501")
        );
        assert_eq!(chunk_stamp("messages"), None);
        assert_eq!(chunk_stamp("log_backup_260809005124.txt.gz"), None);
        assert_eq!(chunk_stamp("messages_00000807_2026080710.gz"), None);
    }

    /// The rotated chunks carry everything between the last dump and the last few
    /// minutes, so the watermark filter must keep the chunk that straddles it.
    ///
    /// Whether a chunk's stamp marks the start or the end of its content is not
    /// known, so dropping every chunk stamped at or before the watermark could
    /// discard events the desktop has never seen. One extra chunk is the price of
    /// not having to know.
    #[test]
    fn chunk_selection_keeps_the_one_that_straddles_the_watermark() {
        let dir = tempdir();
        for name in [
            "messages", // the live file — not a chunk
            "messages_00000805_20260809080000.gz",
            "messages_00000806_20260809090000.gz",
            "messages_00000807_20260809100000.gz",
            "messages_00000808_20260809110000.gz",
            "wpa_supplicant", // some other log entirely
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }

        let mut out = Collected::default();
        let keep = chunks(&dir, "260809:093000", &mut out);
        let names: Vec<String> = keep
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "messages_00000806_20260809090000.gz".to_string(),
                "messages_00000807_20260809100000.gz".to_string(),
                "messages_00000808_20260809110000.gz".to_string(),
            ],
            "the 09:00 chunk straddles a 09:30 watermark and must be kept"
        );
        assert_eq!(out.skipped, 1, "only the 08:00 chunk is wholly behind");

        // A Kindle the desktop has never seen reads every chunk it still has.
        let mut fresh = Collected::default();
        assert_eq!(chunks(&dir, "", &mut fresh).len(), 4);
        assert_eq!(fresh.skipped, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A unique scratch dir. The picker's tests run on the host, and the crate
    /// deliberately has no dev-dependency on a tempfile crate.
    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "sidle-rl-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
