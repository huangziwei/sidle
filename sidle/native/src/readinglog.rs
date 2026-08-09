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

/// Our own archive of reading events, relative to `/mnt/us`.
///
/// The firmware keeps 30 daily dumps — about a month — and prunes the oldest, so
/// a trip longer than that loses its start before any sync can collect it. This
/// directory is the answer: the same event lines, filtered to the marker, kept
/// indefinitely because they are ~80x smaller than the dumps they come from (a
/// measured month was 1.1 MB gzipped against 92 MB of dumps).
const ARCHIVE_DIR: &str = "extensions/sidle/readinglog";

/// What an archive file is called: `rl_<YYMMDDHHMMSS>.txt.gz`, stamped with the
/// newest line it holds.
///
/// Deliberately the same shape as the firmware's dumps, so the newest stamp is
/// readable from the directory listing alone. That is the whole state this keeps
/// — no marker file to lose, nothing to decompress to find out where it got to.
const ARCHIVE_PREFIX: &str = "rl_";

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

/// Every reading event newer than `watermark`, from four sources: the dumps the
/// desktop has not already read, the live log, the rotated syslog chunks, and
/// our own archive.
///
/// Each covers what the others cannot. The live log and chunks are the present,
/// with no wait for a daily snapshot. The dumps are the firmware's month of
/// history, and the only thing available on a device that has never run this.
/// The archive is everything older than the firmware keeps.
///
/// `seen` names snapshots the desktop has read in full — including ones imported
/// from a copy of this folder on the desktop, so a host-side backfill primes the
/// device sync. `watermark` is `YYMMDD:HHMMSS` and filters the other three,
/// which have no stable names worth recording. Both empty means a Kindle the
/// desktop has never seen, so everything is read once.
///
/// The sources overlap heavily and that is intended — the desktop de-duplicates.
/// What matters is that between them nothing is skipped, which the dumps alone
/// demonstrably do not achieve.
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
    // Our own archive last. This is what makes a long trip survivable: the
    // firmware's dumps and chunks are both gone within about a month, so after
    // that the archive is the *only* place the older events still exist, and a
    // sync that ignored it would silently push a truncated history.
    for path in archive_files(us_root, watermark, &mut out) {
        if let Some(old) = read_maybe_gzip(&path) {
            take_events(&old.text, watermark, &mut out.lines);
        }
    }
    out.lines.sort();
    out.lines.dedup();
    out
}

/// The newest event this device has already archived, as the `YYMMDD:HHMMSS` a
/// log line carries, or empty if nothing has been archived yet.
///
/// Read from the filenames alone — the archive is named for its contents, so
/// this is a directory listing and no more.
pub fn archive_watermark(us_root: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(us_root.join(ARCHIVE_DIR)) else {
        return String::new();
    };
    entries
        .flatten()
        .filter_map(|e| archive_stamp(&e.file_name().to_string_lossy()))
        .max()
        .unwrap_or_default()
}

/// How often the archiver runs, matching stock `tinyrot`'s own cadence — it is
/// what rotates the syslog we read, so running at the same rate means never
/// being more than one rotation behind. The cost is set by how much log there
/// is, not by how often we look, so a shorter interval buys coverage for nothing.
pub const ARCHIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Where the running archiver records its pid and the build it came from, so a
/// second one is never started and an obsolete one never survives an update. On
/// the user partition, because that is the only thing we can write.
const DAEMON_PID: &str = "/mnt/us/extensions/sidle/archive.pid";

/// The flag that turns this binary into the archiver. Also how a process in
/// `/proc` is recognised as one.
pub const DAEMON_FLAG: &str = "--archive-daemon";

/// Where the archiver runs from: its own copy of this binary, under a name that
/// shares no text with the picker's.
///
/// The home-screen tile will not open a picker while `pidof sidle` matches
/// anything, and that check reaches the executable behind a process, not only
/// the name the process reports for itself — so an archiver running out of the
/// picker's own `bin/sidle` is a tile that silently does nothing for as long as
/// the archiver lives, which is indefinitely. A separate file is what settles
/// it, and a copy rather than a link because the user partition is FAT, which
/// has neither kind.
const DAEMON_BIN: &str = "/mnt/us/extensions/sidle/bin/readinglogd";

/// What the pidfile and `/proc` together say about the archiver.
#[derive(Debug, PartialEq, Eq)]
pub enum Archiver {
    /// None is running: start one.
    Absent,
    /// This build's archiver is running: leave it alone.
    Running,
    /// An archiver from a different build is running, carrying its pid.
    ///
    /// It has to be replaced rather than left: a self-update swaps `bin/sidle`
    /// underneath it, and nothing would ever start a current one, because the
    /// pidfile it holds is the very thing that says one is already up.
    Outdated(u32),
}

/// What the archiver situation is right now.
///
/// Checked against `/proc` rather than trusting the file: a pidfile survives a
/// crash and a reboot, and a stale one treated as live would silently end the
/// archiving this whole feature exists for. The cmdline test guards the other
/// direction, where the kernel has since reissued that pid to something
/// unrelated.
pub fn archiver() -> Archiver {
    let Ok(text) = std::fs::read_to_string(DAEMON_PID) else {
        return Archiver::Absent;
    };
    let mut fields = text.split_whitespace();
    let Some(pid) = fields.next().and_then(|f| f.parse::<u32>().ok()) else {
        return Archiver::Absent;
    };
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    classify(pid, fields.next(), &cmdline)
}

/// The judgement itself, given a pid from the pidfile, the build stamp recorded
/// beside it, and what `/proc` reports that pid is running.
///
/// A pidfile with no stamp is from a build that did not record one, so it is by
/// definition not this one.
fn classify(pid: u32, stamp: Option<&str>, cmdline: &[u8]) -> Archiver {
    if !String::from_utf8_lossy(cmdline).contains(DAEMON_FLAG) {
        return Archiver::Absent;
    }
    let mine = crate::selfupdate::self_build_ts().to_string();
    if stamp == Some(mine.as_str()) {
        Archiver::Running
    } else {
        Archiver::Outdated(pid)
    }
}

/// Stop an archiver and drop its pidfile, so a fresh one can be started without
/// waiting for this one to notice the signal.
///
/// `pid` must have come from [`archiver`], which confirms against `/proc` that
/// it really is an archiver before naming it.
pub fn stop_archiver(pid: u32) {
    // SAFETY: `kill` sends a signal to a pid and touches nothing in this
    // process. SIGTERM, not SIGKILL — the archiver holds no lock, but a default
    // termination is still the polite one.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let _ = std::fs::remove_file(DAEMON_PID);
}

/// Start an archiver and answer with its pid.
///
/// Runs [`DAEMON_BIN`], not this binary, having first put a current copy of
/// this one there. The copy is rewritten on every start rather than compared:
/// a start only happens when no current archiver is running — after an install,
/// a reboot, or an update — and reading it to decide would cost what writing it
/// costs.
pub fn start_archiver() -> std::io::Result<u32> {
    let bin = Path::new(DAEMON_BIN);
    stage_binary(&std::env::current_exe()?, bin)?;
    Ok(std::process::Command::new(bin)
        .arg(DAEMON_FLAG)
        .spawn()?
        .id())
}

/// Copy `src` over `dst`, through a scratch name beside it.
///
/// Never a write straight to `dst`: that is a file something may be executing,
/// and one cut short would be a binary a later launch tries to run. The rename
/// is within the one FAT mount, so it stays a rename.
fn stage_binary(src: &Path, dst: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(src)?;
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let scratch = dst.with_extension("new");
    std::fs::write(&scratch, &bytes)?;
    std::fs::rename(&scratch, dst)
}

/// Record this process as the running archiver.
pub fn claim_archiver() {
    if let Some(dir) = Path::new(DAEMON_PID).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        DAEMON_PID,
        format!(
            "{} {}",
            std::process::id(),
            crate::selfupdate::self_build_ts()
        ),
    );
}

/// Delete archive files the library has confirmed it holds, and report how many
/// went.
///
/// `watermark` must be what the desktop said it stored, not what this device
/// believes it sent — a line that formed no storable session does not advance
/// it, and deleting past that point would throw away the only remaining copy.
/// A file stamped at or before it holds nothing the library lacks, and the
/// filename is exact about that, because [`archive`] names each file for its
/// newest line.
///
/// The archive exists to survive a gap between syncs. Once the gap closes there
/// is nothing to survive, and a Kindle is not where reading history should
/// accumulate — so it goes, the same archive-then-purge the misc sync uses.
pub fn purge_archive(us_root: &Path, watermark: &str) -> usize {
    if watermark.is_empty() {
        return 0;
    }
    let dir = us_root.join(ARCHIVE_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut gone = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if archive_stamp(&name).is_some_and(|stamp| stamp.as_str() <= watermark)
            && std::fs::remove_file(entry.path()).is_ok()
        {
            gone += 1;
        }
    }
    gone
}

/// The archive files worth opening: those whose newest line is past the
/// watermark. A file stamped at or before it holds nothing the desktop lacks.
///
/// Exact rather than loose, unlike the syslog chunks: this name means "the
/// newest line inside", because [`archive`] wrote it that way.
fn archive_files(us_root: &Path, watermark: &str, out: &mut Collected) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(us_root.join(ARCHIVE_DIR)) else {
        return Vec::new();
    };
    let mut keep = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // A `.part` is a write that did not finish; ignore it entirely rather
        // than read a fragment whose stamp promises more than it holds.
        if name.ends_with(".part") {
            continue;
        }
        match archive_stamp(&name) {
            Some(stamp) if !watermark.is_empty() && stamp.as_str() <= watermark => out.skipped += 1,
            Some(_) => keep.push(entry.path()),
            None => {}
        }
    }
    keep.sort();
    keep
}

/// The `YYMMDD:HHMMSS` an archive file's name encodes: `rl_260809005124.txt.gz`
/// → `260809:005124`.
fn archive_stamp(name: &str) -> Option<String> {
    let digits: String = name
        .strip_prefix(ARCHIVE_PREFIX)?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (digits.len() == 12).then(|| format!("{}:{}", &digits[..6], &digits[6..]))
}

/// Add `lines` to the archive, folding them into the day's existing file.
/// Returns the resulting file's name, or `None` when there was nothing to add.
///
/// **One file per day, not per run.** This runs every half hour, so a file per
/// run would be ~17,500 files a year, each burning a whole FAT cluster to hold a
/// few KB. Instead the day's file is read back, merged, and rewritten under a
/// name carrying the new newest line — a day's events are ~40 KB gzipped, so the
/// read-merge-write is cheaper than the directory it would otherwise create.
///
/// Never an append to the existing file: a torn append corrupts history that was
/// already safe. The new file is written under `.part`, renamed into place, and
/// only then is the old one removed — so a crash at any point leaves either the
/// old file, or both (harmlessly overlapping, since the desktop de-duplicates
/// and the next run merges them again).
pub fn archive(us_root: &Path, lines: &[String]) -> std::io::Result<Option<String>> {
    let Some(newest) = lines.iter().filter_map(|l| line_stamp(l)).max() else {
        return Ok(None);
    };
    let dir = us_root.join(ARCHIVE_DIR);
    std::fs::create_dir_all(&dir)?;

    // The day's file, if it exists: same `YYMMDD`, whatever time it is stamped.
    let today = &newest[..6];
    let existing: Option<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .find(|e| {
            archive_stamp(&e.file_name().to_string_lossy())
                .is_some_and(|stamp| stamp.starts_with(today))
        })
        .map(|e| e.path());

    let mut all: Vec<String> = lines.to_vec();
    if let Some(path) = &existing
        && let Some(held) = read_maybe_gzip(path)
    {
        all.extend(held.text.lines().map(str::to_string));
    }
    all.sort();
    all.dedup();

    let name = format!("{ARCHIVE_PREFIX}{}{}.txt.gz", today, &newest[7..]);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    for line in &all {
        std::io::Write::write_all(&mut gz, line.as_bytes())?;
        std::io::Write::write_all(&mut gz, b"\n")?;
    }
    let bytes = gz.finish()?;

    // The scratch name carries the pid: cron fires every half hour and the first
    // run on a fresh device reads a month of dumps, so two runs can overlap. A
    // shared scratch path would have them interleave writes into one file. With
    // distinct paths the worst case is that the later rename wins and the other
    // run's newest lines are missed — which the next run collects again, since
    // the watermark it reads back did not advance past them.
    let tmp = dir.join(format!("{name}.{}.part", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, dir.join(&name))?;
    if let Some(old) = existing
        && old != dir.join(&name)
    {
        let _ = std::fs::remove_file(old);
    }
    Ok(Some(name))
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

    /// The archive is named for the newest line it holds, so the watermark is a
    /// directory listing — no marker file to lose, nothing to decompress.
    #[test]
    fn the_archive_states_its_own_newest_event_in_its_name() {
        let dir = tempdir();
        assert_eq!(archive_watermark(&dir), "", "nothing archived yet");

        let lines = vec![event("260809:100000"), event("260809:120000")];
        let name = archive(&dir, &lines).unwrap().unwrap();
        assert_eq!(name, "rl_260809120000.txt.gz", "named for the newest line");
        assert_eq!(archive_watermark(&dir), "260809:120000");

        // Nothing to add is not an error, and writes no file.
        assert_eq!(archive(&dir, &[]).unwrap(), None);
        assert_eq!(archive_watermark(&dir), "260809:120000");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Running every half hour must not leave a file per run: the day's events
    /// are folded into one file, renamed forward as it grows, and the previous
    /// name removed. Otherwise a year is ~17,500 FAT clusters.
    #[test]
    fn a_days_runs_fold_into_a_single_file() {
        let dir = tempdir();
        archive(&dir, &[event("260809:100000")]).unwrap();
        archive(&dir, &[event("260809:103000")]).unwrap();
        let name = archive(&dir, &[event("260809:110000")]).unwrap().unwrap();
        assert_eq!(name, "rl_260809110000.txt.gz");

        let files: Vec<String> = std::fs::read_dir(dir.join(ARCHIVE_DIR))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, vec!["rl_260809110000.txt.gz".to_string()]);

        // Every run's events survive the folding — this is history, and losing an
        // earlier run's lines to a later one would be the whole point defeated.
        let found = collect(&dir, "", &[]);
        assert_eq!(
            found.lines,
            vec![
                event("260809:100000"),
                event("260809:103000"),
                event("260809:110000")
            ]
        );

        // A new day starts its own file rather than growing yesterday's.
        archive(&dir, &[event("260810:090000")]).unwrap();
        let mut files: Vec<String> = std::fs::read_dir(dir.join(ARCHIVE_DIR))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(
            files,
            vec![
                "rl_260809110000.txt.gz".to_string(),
                "rl_260810090000.txt.gz".to_string()
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The archive is a *source*, not just a destination. After a long trip the
    /// firmware's dumps and syslog chunks have both been pruned, so the archive
    /// is the only remaining copy — a sync that did not read it would push a
    /// history missing everything older than about a month.
    #[test]
    fn a_sync_after_a_long_gap_gets_its_history_from_the_archive() {
        let dir = tempdir();
        let old = vec![event("260701:090000"), event("260715:093000")];
        archive(&dir, &old).unwrap();

        // A desktop that has seen nothing gets the archived history back, even
        // though no dump or chunk still holds it.
        let found = collect(&dir, "", &[]);
        assert_eq!(found.lines, old);

        // A desktop already past it opens nothing: the filename alone settles it.
        let mut caught_up = collect(&dir, "260715:093000", &[]);
        assert!(caught_up.lines.is_empty());
        assert_eq!(caught_up.skipped, 1);

        // And a half-written archive file is ignored outright rather than read
        // as a whole one — its stamp promises a tail it does not have.
        std::fs::write(
            dir.join(ARCHIVE_DIR).join("rl_260801000000.txt.gz.part"),
            b"junk",
        )
        .unwrap();
        caught_up = collect(&dir, "260715:093000", &[]);
        assert!(caught_up.lines.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The archive is insurance, so it goes once the library confirms it holds
    /// the events — but only up to what the library actually stored.
    #[test]
    fn a_confirmed_sync_clears_the_archive_it_covers() {
        let dir = tempdir();
        archive(&dir, &[event("260701:090000")]).unwrap();
        archive(&dir, &[event("260715:093000")]).unwrap();
        archive(&dir, &[event("260809:120000")]).unwrap();

        // The library got as far as 260715. The later file is the only remaining
        // copy of events it has not stored, so it must survive.
        assert_eq!(purge_archive(&dir, "260715:093000"), 2);
        assert_eq!(archive_watermark(&dir), "260809:120000");
        assert_eq!(collect(&dir, "", &[]).lines, vec![event("260809:120000")]);

        // An empty watermark means the library confirmed nothing — a server that
        // answered oddly, say. Deleting on that would be deleting on silence.
        assert_eq!(purge_archive(&dir, ""), 0);
        assert_eq!(archive_watermark(&dir), "260809:120000");

        assert_eq!(purge_archive(&dir, "260809:120000"), 1);
        assert_eq!(archive_watermark(&dir), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The daemon guard must fail *open* on anything it cannot verify.
    ///
    /// A pidfile outlives the process that wrote it — every reboot leaves one —
    /// so treating its mere existence as "already running" would silently end
    /// the archiving after the first restart, which is the one failure this
    /// feature cannot afford.
    #[test]
    fn a_pid_that_is_no_longer_an_archiver_never_blocks_one_from_starting() {
        // The kernel reissued that pid to something else entirely.
        assert_eq!(
            classify(999, Some("0"), b"/usr/bin/something\0--else\0"),
            Archiver::Absent
        );
        // Nothing is running under it at all, so there is no command line.
        assert_eq!(classify(999, Some("0"), b""), Archiver::Absent);
    }

    /// The archiver's executable may not be named anything `pidof sidle` could
    /// match, by any rule such a check might use.
    ///
    /// The tile that opens the picker refuses to while that name is taken, and
    /// the check reaches the executable behind a process rather than only the
    /// name the process reports — so the file itself has to be unmistakable.
    #[test]
    fn the_archivers_binary_is_named_nothing_like_the_pickers() {
        let name = Path::new(DAEMON_BIN)
            .file_name()
            .and_then(|n| n.to_str())
            .expect("the archiver's binary has a name");
        assert!(
            !name.contains("sidle"),
            "{name:?} could still be taken for the picker"
        );
    }

    /// Staging must leave either the old binary or the whole new one, and no
    /// scratch file for a later launch to trip over.
    #[test]
    fn staging_the_archivers_binary_replaces_it_whole() {
        let dir = tempdir();
        let src = dir.join("sidle");
        let dst = dir.join("bin/readinglogd");
        std::fs::write(&src, b"v1").unwrap();

        stage_binary(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"v1");

        // The update case: a copy is already there and is replaced in full.
        std::fs::write(&src, b"v2-and-longer").unwrap();
        stage_binary(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"v2-and-longer");
        assert!(!dst.with_extension("new").exists(), "scratch left behind");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An archiver that predates the running binary is replaced, not kept.
    ///
    /// A self-update leaves it executing code that has been swapped out from
    /// under it, and the pidfile it holds is the one thing that would stop a
    /// current one from starting — so believing it would freeze the archiver at
    /// whatever version first happened to run.
    #[test]
    fn an_archiver_from_another_build_is_replaced() {
        let cmdline = b"sidle-archive\0--archive-daemon\0";
        // Written before the pidfile recorded a build at all.
        assert_eq!(classify(42, None, cmdline), Archiver::Outdated(42));
        assert_eq!(classify(42, Some("1"), cmdline), Archiver::Outdated(42));

        let mine = crate::selfupdate::self_build_ts().to_string();
        assert_eq!(classify(42, Some(&mine), cmdline), Archiver::Running);
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
