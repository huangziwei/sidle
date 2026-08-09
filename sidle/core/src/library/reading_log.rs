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
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Imported {
    /// Log files read, including any that were truncated.
    pub files: usize,
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

/// Collect the distinct event lines from every log file under `paths`, which may
/// name plain-text logs, gzipped dumps, or directories of either.
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
    watch: job::Watch<'_>,
) -> (BTreeSet<String>, usize, bool) {
    let mut events = BTreeSet::new();
    let mut files = 0;
    // Enumerating first costs one cheap directory walk and buys a real total,
    // so the bar is a proportion from the first tick rather than a spinner.
    let all: Vec<_> = paths.iter().flat_map(|p| walk(p.as_ref())).collect();
    let total = all.len();
    for (done, file) in all.into_iter().enumerate() {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if watch(job::Report {
            phase: "read",
            done,
            total,
            label: &name,
        })
        .is_break()
        {
            return (events, files, true);
        }
        if let Some(text) = read_maybe_gzip(&file) {
            files += 1;
            for line in text.lines().filter(|l| l.contains(MARKER)) {
                events.insert(line.to_string());
            }
        }
    }
    (events, files, false)
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

/// Read every log under `paths`, store the sessions, and attribute what can be
/// attributed.
///
/// Safe to run repeatedly over the same archive: identical sessions collide on
/// the uniqueness index and are ignored, so a second pass reports 0 added.
///
/// `device_serial` is **stated, never inferred.** The logs do not identify the
/// device that wrote them — not once in a 30-day archive — so the caller has to
/// say: the device itself knows its own serial, and a host-side import of a
/// copied archive is told which device it came from. An empty serial means
/// genuinely unknown provenance, and such rows are later claimed by the first
/// import that does name a device (see
/// [`db::insert_reading_session`]).
pub fn import(
    conn: &Connection,
    paths: &[impl AsRef<Path>],
    device_serial: &str,
    watch: job::Watch<'_>,
) -> rusqlite::Result<Imported> {
    let (events, files, cancelled) = collect_events(paths, watch);
    if cancelled {
        // Nothing has been written yet, so an interrupted read leaves the
        // library exactly as it was.
        return Ok(Imported {
            files,
            events: events.len(),
            cancelled: true,
            ..Imported::default()
        });
    }
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
}
