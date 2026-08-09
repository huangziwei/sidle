//! Recover reading sessions from a Kindle's own system logs.
//!
//! The device writes `/var/local/log/messages` and, at most once a day, gzips a
//! snapshot of it into `system/logbackup/`. Under the `cvm` tag those logs carry
//! `ReadingTimerController` events — one per page turn, plus book open/close —
//! which together are an unbiased, dated record of every reading session. The
//! `.sdr` sidecars carry nothing comparable: their timer is a pair of running
//! counters with no clock attached.
//!
//! Three properties of the source shape everything here:
//!
//! - **The book is redacted.** Every line reads `Title:<private>,Asin:<private>`
//!   and no line anywhere carries a path. What each line *does* carry is the
//!   book's last position, which identifies it against
//!   [`super::extent`] — see [`super::db::books_with_last_position`].
//! - **Dumps replay one another.** Each is a snapshot of the same rolling
//!   buffer, so consecutive files overlap heavily; a real archive was 60%
//!   duplicate lines. De-duplication is not tidiness, it is correctness: fed the
//!   raw concatenation, a counter-delta reading of the log double-counts every
//!   overlap and inflates the result several-fold.
//! - **`TotalTime` is a running per-book counter, not session time.** It pairs
//!   with `Total%`, the fraction of the book read, and survives across sessions.
//!   A session's contribution is its *delta*.
//!
//! Times are device-local wall clock. The syslog prefix carries no offset, and
//! the device's clock has been seen running ~60 s ahead of the UTC stamps
//! embedded in the same lines — irrelevant at the resolution of "what did I read
//! on Tuesday", which is what this feeds.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

use rusqlite::Connection;

use super::db::{self, ReadingSession};
use super::job;

/// The tag every event line carries; the cheap prefilter before any parsing.
const MARKER: &str = "ReadingTimerController";

/// A page event long enough to be idle rather than reading is still counted —
/// the device's own counter is the authority — but a session is cut when two
/// events are further apart than this, because the reader plainly left. Without
/// it, opening a book in the morning and again at night reads as one session.
const SESSION_GAP_SECS: i64 = 30 * 60;

/// What one [`import`] pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Imported {
    /// Log files read, including any that were truncated.
    pub files: usize,
    /// Log files skipped unopened because this archive was already imported.
    pub skipped: usize,
    /// Distinct event lines after de-duplication.
    pub events: usize,
    /// Sessions parsed out of them.
    pub sessions: usize,
    /// Sessions actually new to the library (the rest were already held).
    pub added: usize,
    /// Sessions that now name a book, counted across the whole table.
    ///
    /// This, not `added`, is what the reading log gained: a session on a book
    /// the library does not hold is stored but counted nowhere. The two differ
    /// in both directions — an archive can carry time on deleted books, and a
    /// pass can name rows an earlier pass left unresolved.
    pub attributed: usize,
    /// True when the pass stopped early. Sessions stored before that point
    /// stay stored; re-running is safe and picks up the rest.
    pub cancelled: bool,
    /// Set when the archive names a Kindle other than the one it was being
    /// imported for. Nothing was stored.
    pub conflict: Option<String>,
}

/// One parsed session, before it is given a device or stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub started_at: String,
    pub ended_at: String,
    pub end_position: i64,
    pub seconds: i64,
    /// `NextPage` events only — screens advanced, at whatever font size the
    /// device was set to. Never a page count: nothing in a converted book is
    /// paginated, and paging backwards is not reading a page again.
    pub page_turns: i64,
    pub words: i64,
}

// ---------------------------------------------------------------------------
// Reading the archive

/// The names of the log files an archive holds, without opening one.
///
/// Cheap enough to run before deciding anything: a directory walk and no I/O per
/// file. A snapshot's name encodes the second it was written, so it identifies
/// both the file and — once the library has read it once — the Kindle that wrote
/// it (see [`db::dumps_owner`]).
pub fn archive_names(paths: &[impl AsRef<Path>]) -> Vec<String> {
    paths
        .iter()
        .flat_map(|p| walk(p.as_ref()))
        .filter_map(|f| f.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect()
}

/// What one pass over an archive found.
#[derive(Debug, Default)]
pub struct Scan {
    pub events: BTreeSet<String>,
    /// A device serial the library already knows, spotted verbatim somewhere in
    /// the raw text.
    ///
    /// Opportunistic and never load-bearing: the reading events themselves never
    /// name a device, and whether anything else in the log does depends on what
    /// is installed. Its value is as a **contradiction check** — an archive that
    /// names one Kindle must not be filed under another.
    pub serial: Option<String>,
    /// Files opened and decoded.
    pub files: usize,
    /// Files skipped because this archive has already been imported. Never
    /// decompressed, never even opened.
    pub skipped: usize,
    /// Names of the files actually read, so the caller can record them as seen
    /// once the events they held are safely stored.
    pub read: Vec<String>,
    pub cancelled: bool,
}

/// Collect the distinct event lines from every log file under `paths`, which may
/// name plain-text logs, gzipped dumps, or directories of either.
///
/// A file whose name is in `seen` was read by an earlier import and is skipped
/// without being opened — a log snapshot is immutable, so having read it once is
/// a fact and re-reading it can only produce what is already stored.
///
/// A [`BTreeSet`] does the de-duplication and the ordering in one step: the
/// `YYMMDD:HHMMSS` prefix sorts chronologically, so the result is a clean
/// timeline regardless of what order the files were read in.
///
/// Decompression is deliberately lenient. Dumps are routinely truncated
/// mid-write — a real archive had 5 of 31 fail `gzip -t`, including the newest —
/// but a truncated file's intact prefix is perfectly good data, and heavy
/// overlap between dumps usually supplies the lost tail anyway. Rejecting such a
/// file would discard events for no gain.
pub fn collect_events(
    paths: &[impl AsRef<Path>],
    seen: &std::collections::HashSet<String>,
    known_serials: &[String],
    watch: job::Watch<'_>,
) -> Scan {
    let mut out = Scan::default();
    // Enumerating first costs one cheap directory walk and buys a real total,
    // so the bar is a proportion from the first tick rather than a spinner.
    let all: Vec<_> = paths.iter().flat_map(|p| walk(p.as_ref())).collect();
    let total = all.len();
    for (done, file) in all.into_iter().enumerate() {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // The whole point: a snapshot already read holds nothing new, and
        // decompressing it to prove that is the cost this avoids. Checked before
        // the progress tick so a re-import does not crawl through 30 no-ops.
        if seen.contains(&name) {
            out.skipped += 1;
            continue;
        }
        if watch(job::Report {
            phase: "read",
            done,
            total,
            label: &name,
        })
        .is_break()
        {
            out.cancelled = true;
            return out;
        }
        if let Some(text) = read_maybe_gzip(&file) {
            out.files += 1;
            out.read.push(name);
            // One vectorised scan of the whole buffer per known device, and only
            // until the first hit — not a per-line test.
            if out.serial.is_none() {
                out.serial = known_serials
                    .iter()
                    .find(|s| text.contains(s.as_str()))
                    .cloned();
            }
            for line in text.lines().filter(|l| l.contains(MARKER)) {
                out.events.insert(line.to_string());
            }
        }
    }
    out
}

/// Every file at or under `path`, one level of directory recursion at a time.
fn walk(path: &Path) -> Vec<std::path::PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut out: Vec<_> = entries.flatten().flat_map(|e| walk(&e.path())).collect();
    out.sort();
    out
}

/// Decode a log file, gunzipping it when it is gzipped. Returns whatever
/// decoded, including the valid prefix of a truncated archive.
fn read_maybe_gzip(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        // A truncated member yields Err *after* writing what it decoded, so the
        // buffer is kept either way; only a header-level failure yields nothing.
        let _ = flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut out);
        (!out.is_empty()).then(|| String::from_utf8_lossy(&out).into_owned())
    } else {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

// ---------------------------------------------------------------------------
// Parsing

/// Pull `name:<digits>` out of a line. Anchored on the preceding comma so
/// `TotalTime` cannot match inside a longer field name.
fn field(line: &str, name: &str) -> Option<i64> {
    let at = line.find(&format!(",{name}:"))? + name.len() + 2;
    let rest = &line[at..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The book's own end position — the first `EndPos:` on the line.
///
/// Page events state `EndPos` twice: first the book's end, then the end of the
/// current chapter. Only the first identifies the book, so this must not be a
/// last-match or all-matches scan.
fn end_position(line: &str) -> Option<i64> {
    let at = line.find("EndPos:YJPosition: ")? + "EndPos:YJPosition: ".len();
    let rest = &line[at..];
    let colon = rest.find(':')?;
    let tail = &rest[colon + 1..];
    let end = tail
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(tail.len());
    tail[..end].parse().ok()
}

/// `ReadingTimerController:Information::<Kind>` — the event kind.
fn kind(line: &str) -> Option<&str> {
    let at = line.find("Information::")? + "Information::".len();
    let rest = &line[at..];
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// `YYMMDD:HHMMSS` at the start of a syslog line, as (`YYYY-MM-DD`, seconds
/// into the day, `YYYY-MM-DDTHH:MM:SS`).
fn stamp(line: &str) -> Option<(String, i64, String)> {
    let raw = line.as_bytes();
    if raw.len() < 13 || raw[6] != b':' || !line[..6].bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let clock = &line[7..13];
    if !clock.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (y, mo, d) = (&line[0..2], &line[2..4], &line[4..6]);
    let (h, mi, s) = (&clock[0..2], &clock[2..4], &clock[4..6]);
    let day = format!("20{y}-{mo}-{d}");
    let secs: i64 =
        h.parse::<i64>().ok()? * 3600 + mi.parse::<i64>().ok()? * 60 + s.parse::<i64>().ok()?;
    let at = format!("{day}T{h}:{mi}:{s}");
    Some((day, secs, at))
}

/// `YYYY-MM-DDTHH:MM:SS` back to the `YYMMDD:HHMMSS` a syslog line starts with.
///
/// The inverse of [`stamp`], and the form a watermark travels in: the device
/// compares it against raw log prefixes and dump filenames, both of which are
/// this shape, so the comparison is a plain string ordering with no date
/// arithmetic on a device that has no date library.
pub fn log_stamp(iso: &str) -> Option<String> {
    let b = iso.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' {
        return None;
    }
    let out = format!(
        "{}{}{}:{}{}{}",
        &iso[2..4],
        &iso[5..7],
        &iso[8..10],
        &iso[11..13],
        &iso[14..16],
        &iso[17..19]
    );
    out.bytes()
        .all(|c| c.is_ascii_digit() || c == b':')
        .then_some(out)
}

/// Turn an ordered, de-duplicated event stream into sessions.
///
/// A session is a run of page events on one book. It ends when the book changes,
/// when the reader closes it, or when the gap to the next event exceeds
/// [`SESSION_GAP_SECS`]. Its duration is the span of the book's running
/// `TotalTime` counter across the run — not the wall clock between first and
/// last event, which would count the time the device sat asleep on an open page.
pub fn parse_sessions<'a>(events: impl IntoIterator<Item = &'a str>) -> Vec<Session> {
    let mut out = Vec::new();
    let mut open: Option<Open> = None;
    let mut prev_day_secs: Option<(String, i64)> = None;

    for line in events {
        let Some(k) = kind(line) else { continue };
        let Some((day, secs, at)) = stamp(line) else {
            continue;
        };
        // The gap is measured against the previous *event of any kind*, so a
        // stretch of non-page activity still breaks a session.
        let gapped = match &prev_day_secs {
            Some((prev_day, prev_secs)) => {
                *prev_day != day || secs.saturating_sub(*prev_secs) > SESSION_GAP_SECS
            }
            None => false,
        };
        prev_day_secs = Some((day.clone(), secs));

        let is_page = matches!(k, "NextPage" | "PreviousPage" | "GoToPosition");
        if !is_page && k != "CloseBook" {
            continue;
        }
        let Some(position) = end_position(line) else {
            continue;
        };

        if let Some(cur) = &open
            && (cur.end_position != position || gapped)
        {
            out.extend(open.take().map(Open::finish));
        }
        let cur = open.get_or_insert_with(|| Open::new(position, &at));
        cur.observe(line, &at, k);
        if k == "CloseBook" {
            out.extend(open.take().map(Open::finish));
        }
    }
    out.extend(open.map(Open::finish));
    out
}

/// A session under construction.
struct Open {
    end_position: i64,
    started_at: String,
    ended_at: String,
    time_lo: Option<i64>,
    time_hi: i64,
    words_lo: Option<i64>,
    words_hi: i64,
    page_turns: i64,
}

impl Open {
    fn new(end_position: i64, at: &str) -> Self {
        Self {
            end_position,
            started_at: at.to_string(),
            ended_at: at.to_string(),
            time_lo: None,
            time_hi: 0,
            words_lo: None,
            words_hi: 0,
            page_turns: 0,
        }
    }

    fn observe(&mut self, line: &str, at: &str, k: &str) {
        self.ended_at = at.to_string();
        if k == "NextPage" {
            self.page_turns += 1;
        }
        if let Some(t) = field(line, "TotalTime") {
            self.time_lo = Some(self.time_lo.map_or(t, |lo| lo.min(t)));
            self.time_hi = self.time_hi.max(t);
        }
        if let Some(w) = field(line, "TotalWords") {
            self.words_lo = Some(self.words_lo.map_or(w, |lo| lo.min(w)));
            self.words_hi = self.words_hi.max(w);
        }
        // A book reopened after finishing restarts its counter; the run so far is
        // already banked because a fresh `Open` is made per run.
    }

    fn finish(self) -> Session {
        Session {
            started_at: self.started_at,
            ended_at: self.ended_at,
            end_position: self.end_position,
            seconds: (self.time_hi - self.time_lo.unwrap_or(self.time_hi)) / 1000,
            page_turns: self.page_turns,
            words: self.words_hi - self.words_lo.unwrap_or(self.words_hi),
        }
    }
}

/// Map each book's per-line fingerprint to the number that actually identifies
/// it against the library.
///
/// The device states **two** different end-of-book constants. Page events repeat
/// `EndPos`, the last *word* position — good for grouping, but a few positions
/// short of the book's end. Only the occasional `BookEndPosition` event states
/// `FromBook`, the last valid position, which is what
/// [`super::db::books_with_last_position`] joins on. They are not
/// interchangeable: one archive's book showed 148853 against a `FromBook` of
/// 148859, so joining on the former silently matches nothing.
///
/// `BookEndPosition` fires on most book opens, so a corpus that contains a
/// book's sessions almost always contains its mapping too; the first sighting
/// wins, since two builds of one title differ in both numbers together.
fn frombook_map<'a>(
    events: impl IntoIterator<Item = &'a str>,
) -> std::collections::HashMap<i64, i64> {
    let mut map = std::collections::HashMap::new();
    let mut pending: Option<i64> = None;
    for line in events {
        if let Some(at) = line.find("BookEndPosition.FromBook:YJPosition: ") {
            let rest = &line[at + "BookEndPosition.FromBook:YJPosition: ".len()..];
            pending = rest.split_once(':').and_then(|(_, tail)| {
                let end = tail
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(tail.len());
                tail[..end].parse().ok()
            });
        }
        if let (Some(from_book), Some(ep)) = (pending, end_position(line)) {
            map.entry(ep).or_insert(from_book);
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Import

/// Which Kindle an archive belongs to, decided from the archive.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// These snapshots have been imported before, and the library recorded which
    /// device they came from. Exact — a snapshot name encodes the second it was
    /// written, so no two Kindles share one.
    Recorded(String),
    /// Nothing here has been seen before, so the archive cannot say. The caller
    /// supplies the device (the plugged-in one), or refuses.
    Unrecognised,
    /// Files recorded against different Kindles — one folder holding two
    /// devices' logs. Importing it as either would misfile the other's.
    Mixed(Vec<String>),
}

/// Work out which Kindle wrote an archive **without opening a single file**.
///
/// Only the names are consulted, which is what makes this free and what makes it
/// exact: a `log_backup_<YYMMDDHHMMSS>` is unique to the device-second that
/// produced it, so a name the library has already read identifies its device
/// outright. This is why an archive never has to be identified by hand twice —
/// and why the first import of a genuinely new folder is the only moment the
/// device has to come from somewhere else.
pub fn identify(conn: &Connection, paths: &[impl AsRef<Path>]) -> rusqlite::Result<Origin> {
    let names = archive_names(paths);
    Ok(match db::dumps_owner(conn, &names)? {
        Ok(Some(serial)) => Origin::Recorded(serial),
        Ok(None) => Origin::Unrecognised,
        Err(several) => Origin::Mixed(several),
    })
}

/// Read every log under `paths`, store the sessions, and attribute what can be
/// attributed.
///
/// Re-importing the same archive costs almost nothing. Snapshots already read
/// are recorded by name and skipped **unopened** — not decompressed and then
/// found to be redundant, which is what the uniqueness index alone would do and
/// what made a second pass over a 92 MB archive as slow as the first.
///
/// `device_serial` is the Kindle these logs came from. Callers that *know* — the
/// device pushing its own — pass it; a host-side import of a copied folder should
/// resolve it with [`identify`] rather than ask, because a person picking from a
/// list will eventually pick wrong and the mistake is invisible afterwards.
///
/// If the archive names a device the library knows and it contradicts
/// `device_serial`, the import stores nothing and reports
/// [`Imported::conflict`]: filing one Kindle's reading under another is worse
/// than not importing at all.
pub fn import(
    conn: &Connection,
    paths: &[impl AsRef<Path>],
    device_serial: &str,
    watch: job::Watch<'_>,
) -> rusqlite::Result<Imported> {
    let seen = db::seen_dumps(conn, device_serial)?;
    let known = db::known_device_serials(conn)?;
    let found = collect_events(paths, &seen, &known, watch);
    if let Some(named) = &found.serial
        && !device_serial.is_empty()
        && named != device_serial
    {
        return Ok(Imported {
            files: found.files,
            skipped: found.skipped,
            events: found.events.len(),
            conflict: Some(named.clone()),
            ..Imported::default()
        });
    }
    if found.cancelled {
        // Nothing has been written yet, so an interrupted read leaves the
        // library exactly as it was — including the seen-set, so the files it
        // did manage are read again rather than silently dropped.
        return Ok(Imported {
            files: found.files,
            skipped: found.skipped,
            events: found.events.len(),
            cancelled: true,
            ..Imported::default()
        });
    }
    let mut out = store_events(conn, &found.events, found.files, device_serial, watch)?;
    out.skipped = found.skipped;
    // Marked only now: a file counts as read once its events are stored, so an
    // interrupted or failed store leaves it to be read again.
    if !out.cancelled {
        for name in &found.read {
            db::mark_dump_read(conn, device_serial, name)?;
        }
    }
    Ok(out)
}

/// Store already-collected event lines. The half of [`import`] that does not
/// touch the filesystem.
///
/// Split out for the device push, which arrives as lines over HTTP rather than
/// as files on disk: a Kindle reads its own logs and sends what is new, and the
/// host runs **this same parser** over them. One implementation of the session
/// rules — the counter deltas, the gap splitting, the two end-of-book constants
/// — is the point; a second copy on the device would be the one that drifts.
///
/// `events` must be de-duplicated and chronological, which a [`BTreeSet`] of raw
/// lines gives for free.
pub fn store_events(
    conn: &Connection,
    events: &BTreeSet<String>,
    files: usize,
    device_serial: &str,
    watch: job::Watch<'_>,
) -> rusqlite::Result<Imported> {
    // Sessions are grouped by the fingerprint every page event carries, then
    // rekeyed to the one the library can actually be joined against.
    let identity = frombook_map(events.iter().map(String::as_str));
    let sessions = parse_sessions(events.iter().map(String::as_str));

    let mut out = Imported {
        files,
        events: events.len(),
        sessions: sessions.len(),
        ..Imported::default()
    };
    let total = sessions.len();
    for (done, s) in sessions.iter().enumerate() {
        if watch(job::Report {
            phase: "store",
            done,
            total,
            label: &s.started_at,
        })
        .is_break()
        {
            out.cancelled = true;
            break;
        }
        // A session with no measurable duration is a book being opened and shut,
        // not reading; storing it would litter the calendar with empty days.
        if s.seconds <= 0 {
            continue;
        }
        let row = ReadingSession {
            device_serial: device_serial.to_string(),
            day: s.started_at[..10].to_string(),
            started_at: s.started_at.clone(),
            ended_at: s.ended_at.clone(),
            // Falls back to the grouping fingerprint when the archive never
            // showed a `BookEndPosition` for this book: still a stable per-book
            // key, just one the library cannot name yet.
            end_position: identity
                .get(&s.end_position)
                .copied()
                .unwrap_or(s.end_position),
            book_id: None,
            seconds: s.seconds,
            page_turns: s.page_turns,
            words: s.words,
        };
        if db::insert_reading_session(conn, &row)? {
            out.added += 1;
        }
    }
    out.attributed = db::resolve_reading_sessions(conn)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page event, shaped like the device's own. `end` is the book's last
    /// position; the second `EndPos` is the chapter's, which must be ignored.
    fn page(stamp: &str, kind: &str, end: i64, total_ms: i64, words: i64) -> String {
        format!(
            "{stamp} cvm[6144]: I ReadingTimerController:Information::{kind},\
             Title:<private>,Asin:<private>,IntervalTime:900,\
             TotalTime:{total_ms},TotalWords:{words},Total%:0.5,\
             CurrentPos:YJPosition: AAA:12,EndPos:YJPosition: BBB:{end},\
             NextTOCEntryPosition:YJPosition: CCC:99,\
             CurrentPos:YJPosition: AAA:12,EndPos:YJPosition: DDD:6612;"
        )
    }

    #[test]
    fn a_session_measures_the_counter_delta_not_the_wall_clock() {
        let lines = [
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            page("260803:100500", "NextPage", 148_207, 120_000, 220),
            page("260803:101000", "CloseBook", 148_207, 180_000, 300),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str));
        assert_eq!(out.len(), 1);
        // Wall clock spans 10 minutes; the counter only moved 2, and the counter
        // is what the device actually measured as reading.
        assert_eq!(out[0].seconds, 120);
        assert_eq!(out[0].end_position, 148_207);
        assert_eq!(out[0].words, 200);
        assert_eq!(out[0].page_turns, 2);
        assert_eq!(out[0].started_at, "2026-08-03T10:00:00");
    }

    #[test]
    fn the_first_endpos_is_the_book_not_the_chapter() {
        // The chapter's EndPos (6612) trails the book's on every page line; a
        // last-match scan would file every book under one fingerprint.
        let line = page("260803:100000", "NextPage", 148_207, 1000, 1);
        assert_eq!(end_position(&line), Some(148_207));
    }

    #[test]
    fn replayed_lines_do_not_double_count() {
        // Overlapping dumps deliver the same events twice. De-duplication is the
        // caller's job (a BTreeSet), so feeding a sorted set must be idempotent.
        let lines = [
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            page("260803:100500", "NextPage", 148_207, 120_000, 220),
        ];
        let doubled: BTreeSet<String> = lines.iter().chain(lines.iter()).cloned().collect();
        let out = parse_sessions(doubled.iter().map(String::as_str));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seconds, 60);
    }

    #[test]
    fn a_long_gap_splits_one_book_into_two_sessions() {
        let lines = [
            page("260803:080000", "NextPage", 148_207, 60_000, 100),
            page("260803:080100", "NextPage", 148_207, 120_000, 200),
            // Same book, same day, hours later — morning and evening are not one
            // sitting.
            page("260803:230000", "NextPage", 148_207, 300_000, 400),
            page("260803:230100", "NextPage", 148_207, 360_000, 500),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str));
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].seconds, out[1].seconds), (60, 60));
    }

    #[test]
    fn switching_books_closes_the_previous_session() {
        let lines = [
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            page("260803:100100", "NextPage", 148_207, 120_000, 200),
            page("260803:100200", "NextPage", 764_576, 5_000, 10),
            page("260803:100300", "NextPage", 764_576, 65_000, 90),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].end_position, 148_207);
        assert_eq!(out[1].end_position, 764_576);
        assert_eq!(out[1].seconds, 60);
    }

    #[test]
    fn the_join_key_is_from_book_not_the_last_word_position() {
        // The two differ by a handful of positions, and only `FromBook` matches
        // the library's axis. Grouping on `EndPos` while joining on it too named
        // 5 books out of 56 on a real archive; keying on `FromBook` named 54.
        let open = "260803:095956 cvm[6144]: I ReadingTimerController:Information::\
                    BookEndPosition.FromBook:YJPosition: AboVAAAAAAAA:148213,\
                    BookEndPosition.LastWordPos.override:YJPosition: AbcVAAAPAAAA:148207,\
                    CurrentPos:YJPosition: AAA:524,EndPos:YJPosition: AbcVAAAPAAAA:148207;"
            .to_string();
        let lines = [
            open,
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
        ];
        let map = frombook_map(lines.iter().map(String::as_str));
        assert_eq!(map.get(&148_207), Some(&148_213));
    }

    #[test]
    fn a_field_name_is_matched_whole() {
        let line = page("260803:100000", "NextPage", 1, 42_000, 7);
        assert_eq!(field(&line, "TotalTime"), Some(42_000));
        // `IntervalTime` must not satisfy a search for `TotalTime`, nor
        // `TotalWords` for `TotalWord`.
        assert_eq!(field(&line, "IntervalTime"), Some(900));
        assert_eq!(field(&line, "Time"), None);
    }

    #[test]
    fn a_dump_already_read_is_skipped_without_being_opened() {
        let dir = std::env::temp_dir().join(format!("sidle-rl-seen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = "log_backup_260803100000.txt";
        std::fs::write(
            dir.join(name),
            page("260803:100000", "NextPage", 1, 60_000, 10),
        )
        .unwrap();

        let fresh = collect_events(&[&dir], &Default::default(), &[], &mut job::ignore);
        assert_eq!((fresh.files, fresh.skipped), (1, 0));
        assert_eq!(fresh.read, vec![name.to_string()]);
        assert_eq!(fresh.events.len(), 1);

        // The same archive, now known. Nothing is opened, so nothing is found —
        // which is the point: the events are already stored.
        let seen = std::collections::HashSet::from([name.to_string()]);
        let again = collect_events(&[&dir], &seen, &[], &mut job::ignore);
        assert_eq!((again.files, again.skipped), (0, 1));
        assert!(again.events.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_watermark_converts_to_the_form_a_log_line_carries() {
        assert_eq!(
            log_stamp("2026-08-09T00:35:59").as_deref(),
            Some("260809:003559")
        );
        assert_eq!(log_stamp("not a timestamp"), None);
    }
}
