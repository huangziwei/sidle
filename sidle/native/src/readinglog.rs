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
//! - only the live `messages` file is ever read in the steady state, and only
//!   its lines newer than the watermark are kept.
//!
//! With nothing new since the last sync that is one directory listing plus one
//! scan of the live log, and an empty push that never leaves the device.

use std::io::Read;
use std::path::{Path, PathBuf};

/// The tag every reading event carries; the cheap prefilter before anything else.
const MARKER: &str = "ReadingTimerController";

/// The live syslog the firmware appends to, and the source every dump is a
/// snapshot of. Absolute: it is on the root filesystem, not under `/mnt/us`.
const LIVE_LOG: &str = "/var/local/log/messages";

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

/// Every reading event newer than `watermark`, from the live log and from any
/// dump the desktop has not already read.
///
/// `seen` names snapshots the desktop has read in full — including ones imported
/// from a copy of this folder on the desktop, so a host-side backfill primes the
/// device sync. `watermark` is `YYMMDD:HHMMSS` and filters the live log, which
/// has no stable name. Both empty means a Kindle the desktop has never seen, so
/// everything is read once.
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
    if let Ok(text) = std::fs::read_to_string(LIVE_LOG) {
        take_events(&text, watermark, &mut out.lines);
    }
    out.lines.sort();
    out.lines.dedup();
    out
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
