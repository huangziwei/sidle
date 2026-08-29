//! Recover reading sessions from a Kindle's own system logs.
//!
//! The device writes `/var/log/messages` and, at most once a day, gzips a
//! snapshot of it into `system/logbackup/`. Those logs carry
//! `ReadingTimerController` events — one per page turn, plus book open/close —
//! which together are an unbiased, dated record of every reading session. The
//! `.sdr` sidecars carry nothing comparable: their timer is a pair of running
//! counters with no clock attached.
//!
//! Four properties of the source shape everything here:
//!
//! - **An event is not reliably named.** The `cvm` reader writes the event name
//!   first: `Information::NextPage,<fields>`. The Corretto/KPP reader loses the
//!   head of a payload to a `SyslogFormatter` "Argument Value Mismatch", leaving
//!   the name mid-line after a `;` or absent altogether. What a line carries is
//!   dependable; what it is called is not. See [`observation`].
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
//! Times are device-local wall clock, taken from the syslog line prefix, which
//! is the only clock a `ReadingTimerController` line carries — its payload holds
//! counters and no absolute stamp. Local is the one this wants: reading at 23:00
//! means 23:00 where the reader was, not 21:00 in Greenwich.
//!
//! The zone that clock is in comes from the `fastmetrics` records, whose
//! `close_timestamp` and `action_start_time` are epoch milliseconds under the
//! same local prefix — see [`utc_offset`]. A session records it, so a device
//! carried across zones states which one each sitting was stamped in.
//!
//! So the day a session counts to, and the midnight a run is cut at, are the
//! reader's own — never the host's, and never UTC, which west of Greenwich would
//! move a late night's reading a day early and east of it a day late. Nothing
//! downstream converts: the stamps stay the naive local strings written here.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

use rusqlite::Connection;

use super::db::{self, ReadingSession};
use super::job;

/// The tag every reading-timer line carries; the cheap prefilter before any
/// parsing.
const MARKER: &str = "ReadingTimerController";

/// The `fastmetrics` records the reader shell emits beside [`MARKER`].
///
/// [`MARKER`] counts words and a WPM, and times only a book it can count words
/// in. These come from the reader shell:
/// `ereader_book_consume_content` spans a page with its `words_count`,
/// `ereader_book_page_turn` and `ereader_book_linear_page_actions` name a turn,
/// `ereader_open_book` and `ereader_close_book` bracket a book with its
/// `book_category`, and `ereader_reader_latency_ops` and
/// `ereader_reader_page_turn_latency_ops` carry a `cde_key`.
///
/// Bracketed: `ereader_open_book` is a prefix of
/// `ereader_open_book_failure_backup`.
pub const METRIC_MARKERS: [&str; 8] = [
    "SchemaName[ereader_open_book]",
    "SchemaName[ereader_close_book]",
    "SchemaName[ereader_book_consume_content]",
    "SchemaName[ereader_book_page_turn]",
    "SchemaName[ereader_book_linear_page_actions]",
    "SchemaName[ereader_content_point]",
    "SchemaName[ereader_reader_latency_ops]",
    "SchemaName[ereader_reader_page_turn_latency_ops]",
];

/// The tags on the lines that say whether the device was awake, the second
/// family this reads.
///
/// They carry no reading and name no book. What they carry is whether the
/// device was awake, which is the only bound available on a sitting the reading
/// timer refused to count — see [`Awake`].
///
/// Two shapes, because one is not written everywhere. The metrics record is the
/// `powerd` state machine transcribed whole, and a Kindle old enough not to emit
/// it still fires the same transitions on LIPC — where `outOfScreenSaver` and
/// `goingToScreenSaver` bracket exactly the `ACTIVE` state the record names.
/// `suspending` closes a span the screensaver never got to.
pub const POWER_MARKERS: [&str; 4] = [
    "ereader_powerd_state_change",
    "lipc:evts:name=outOfScreenSaver, origin=com.lab126.powerd",
    "lipc:evts:name=goingToScreenSaver, origin=com.lab126.powerd",
    "lipc:evts:name=suspending, origin=com.lab126.powerd",
];

/// A page event long enough to be idle rather than reading is still counted —
/// the device's own counter is the authority — but a session is cut when two
/// events are further apart than this, because the reader plainly left. Without
/// it, opening a book in the morning and again at night reads as one session.
const SESSION_GAP_SECS: i64 = 30 * 60;

/// How far a session's opening counter may outrun the wall clock before it is
/// rejected as belonging to some other book. `StoredBookData` states whole
/// seconds, so a legitimate one can round a second past the clock; a minute
/// clears that without admitting a stale value from another book.
const SEED_SLACK_SECS: i64 = 60;

/// What one [`import`] pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Imported {
    /// Log files read, including any that were truncated.
    pub files: usize,
    /// Log files skipped unopened because this archive was already imported.
    pub skipped: usize,
    /// Of `files`, how many decoded only partway. They still contributed their
    /// prefix, and they stay eligible for re-import, so this is a note rather
    /// than a failure — but a silent one would hide missing reading time.
    pub truncated: usize,
    /// Distinct event lines after de-duplication.
    pub events: usize,
    /// Sessions parsed out of them.
    pub sessions: usize,
    /// Sessions actually new to the library (the rest were already held).
    pub added: usize,
    /// Sittings already held that this pass measured **further** — the same run,
    /// seen to a later point than the events available last time could reach.
    ///
    /// A sitting spans as many syncs as it takes to finish, and each one carries
    /// it forward from where the last left it (see [`Resume`]). Counted apart
    /// from `added` because the reader is told a different thing: not that a new
    /// sitting appeared, but that the one they are in the middle of grew.
    pub extended: usize,
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
    /// Seconds of counted reading per clock hour of the session's own day,
    /// ascending by hour, summing to exactly [`Self::seconds`].
    ///
    /// **This is evidence, not a derivation, and it exists only here.** The
    /// device states its counter at each event, so the parser knows which hours
    /// a sitting's reading actually fell in — and after this pass nobody will:
    /// the events are never offered twice, and a stored session keeps only its
    /// window and its total. Anything reconstructing hours from those two has to
    /// assume the reading was spread evenly across the sitting, which is how an
    /// hour spent on one chapter before midnight becomes an even smear over the
    /// three hours the sitting happened to span.
    pub hours: Vec<(u8, i64)>,
    /// The book's running counter where this run began and where it was last
    /// seen, in milliseconds — the two values [`Self::seconds`] is the difference
    /// of, kept so a later pass can carry the run forward. `None` when no event
    /// in the run stated a counter at all.
    ///
    /// Stored with the session because the events are never offered twice: a
    /// sitting that outlives the sync it started in can only be continued by a
    /// pass that knows where the last one stopped counting. See [`Resume`].
    pub start_counter_ms: Option<i64>,
    pub end_counter_ms: Option<i64>,
    /// The same two readings of the device's own word counter.
    pub start_words: Option<i64>,
    pub end_words: Option<i64>,
    /// Where [`Self::seconds`] came from.
    pub measure: Measure,
    /// The book's catalog key, where a reader-shell record named it during the
    /// run. See [`cde_key`]. `None` on every firmware that writes no such
    /// record, which leaves `end_position` the only identity.
    pub asin: Option<String>,
    /// Seconds the reader's own clock stood ahead of UTC. See [`utc_offset`].
    ///
    /// `started_at` and `ended_at` are local wall clock and stay that way: a
    /// reader who read at 23:00 read at 23:00 where they were. This is what
    /// places that instant, for a reader who crossed a zone between sittings.
    pub tz_offset_s: Option<i64>,
}

/// How a session's seconds were arrived at, ranked best first.
///
/// Three regimes, and a reader is told which. A book `ReadingTimerController`
/// declines to time yields no counter at all, and the two below it answer for
/// exactly that content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Measure {
    /// The span of the device's own `TotalTime` counter. Its accounting,
    /// excluding the pauses an awake reader takes.
    #[default]
    Counted,
    /// The dwell of each `ereader_book_consume_content` page, through
    /// [`dwell_ms`]. Measured against `TotalTime` on a device writing both, the
    /// spacing of those records reproduces `IntervalTime` to the second.
    Dwell,
    /// [`Awake`]'s bound on a run with neither of the above — how long the
    /// device was `ACTIVE` with the book open.
    Awake,
}

impl Measure {
    /// The word a stored row carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Counted => "counted",
            Self::Dwell => "dwell",
            Self::Awake => "awake",
        }
    }

    /// Read a stored row's word. An unknown one reads as [`Self::Counted`],
    /// the value every row written before the column existed carries.
    pub fn from_stored(s: &str) -> Self {
        match s {
            "dwell" => Self::Dwell,
            "awake" => Self::Awake,
            _ => Self::Counted,
        }
    }

    /// Whether the seconds are a measurement of reading rather than a bound on
    /// it.
    pub fn is_measured(self) -> bool {
        matches!(self, Self::Counted | Self::Dwell)
    }

    /// Where this sits in the order the variants are declared in, 0 best. A
    /// stored row is replaced by any measure ranking below its own.
    pub fn rank(self) -> i64 {
        match self {
            Self::Counted => 0,
            Self::Dwell => 1,
            Self::Awake => 2,
        }
    }
}

/// A sitting a device left open, handed back to the parser so the next batch of
/// events continues it instead of starting over.
///
/// **A reader does not stop reading because a sync happened.** Events reach the
/// library in batches — every Sync sends what the device has logged since the
/// last one — and a sitting in progress is split across as many batches as it
/// takes to finish. Parsed batch by batch in isolation, each piece becomes its
/// own session: the sitting fragments, and the counter advance that fell
/// *between* two batches is credited to neither, because a batch measures only
/// the span of the counter it can see. A ten-minute stretch read between two
/// syncs is exactly that case, and it is silently shed.
///
/// So the newest session stored for a device is read back and offered to the
/// parser as the run already under way. Its own break rules then decide whether
/// the next event belongs to it — same book, no half-hour gap, same day — and
/// where it does, the session is measured from its original start to the newest
/// event, hours and all. The result carries the same `started_at`, so it
/// updates that row rather than adding one beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    pub started_at: String,
    pub ended_at: String,
    pub end_position: i64,
    /// The counter the run began at and the last value seen for it. Both or
    /// neither: a run measured from the counter cannot be continued without
    /// them, and a run the device never counted has none to give.
    pub start_counter_ms: Option<i64>,
    pub end_counter_ms: Option<i64>,
    pub start_words: Option<i64>,
    pub end_words: Option<i64>,
    pub page_turns: i64,
    /// The hours already booked against the run, so a continued session rebuilds
    /// the whole distribution rather than an incremental slice of one.
    pub hours: Vec<(u8, i64)>,
    /// What the run has been credited so far, and how. A run with no counter
    /// carries its length here alone, and the next batch adds to it — the power
    /// records and page records covering the earlier stretch went with the
    /// batch that measured them.
    pub seconds: i64,
    pub measure: Measure,
}

impl Resume {
    /// The sitting `device_serial` left open, or `None` when the library holds
    /// no session for it that could still be running.
    ///
    /// "Could still be running" is not decided here — whether the reader came
    /// back within the half hour, stayed in the same book, and stayed on the
    /// same day is [`parse_sessions`]'s own judgement, made against the events
    /// themselves. This only supplies the candidate.
    pub fn newest(conn: &Connection, device_serial: &str) -> rusqlite::Result<Option<Self>> {
        let Some(row) = db::newest_reading_session(conn, device_serial)? else {
            return Ok(None);
        };
        // A [`Measure::Counted`] row with no counters states its length and not
        // where on the counter it sits. The other two regimes carry none and
        // continue from `seconds`.
        let counters = row.start_counter_ms.zip(row.end_counter_ms);
        if counters.is_none() && row.measure == Measure::Counted {
            return Ok(None);
        }
        let hours = db::session_hours(conn, &row)?;
        Ok(Some(Self {
            started_at: row.started_at,
            ended_at: row.ended_at,
            end_position: row.end_position,
            start_counter_ms: counters.map(|(lo, _)| lo),
            end_counter_ms: counters.map(|(_, hi)| hi),
            start_words: row.start_words,
            end_words: row.end_words,
            page_turns: row.page_turns,
            hours,
            seconds: row.seconds,
            measure: row.measure,
        }))
    }
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
    /// Names of the files read **in full**, so the caller can record them as
    /// seen once the events they held are safely stored. A truncated file is
    /// deliberately absent: it must stay eligible for a later, complete copy.
    pub read: Vec<String>,
    /// Files that decoded only partway. Counted, not hidden — a dump silently
    /// yielding half its events looks exactly like a quiet day.
    pub truncated: usize,
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
///
/// Lenient about the data, strict about the claim: a short file's events are
/// taken, but its name goes to `truncated` rather than `read`, so it is never
/// recorded as done with. The two must not be conflated, because a `seen` entry
/// is keyed by a name that never changes and is therefore permanent.
///
/// Usually the file will never grow: the device writes each snapshot once, on
/// its own date, and never revisits it — in a real archive the short files were
/// dates where `log_backup.sh` logged its trigger but never reached the prune
/// step that follows a completed backup, so the Kindle left them short and
/// always will. Re-reading such a file costs a few bytes and finds the same
/// prefix, which is the right price. The case that justifies it is the other
/// one: a file cut short in transfer, or read while the device was still
/// writing it, is whole on the Kindle, and recording it as read would skip the
/// good copy unopened for good.
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
        if let Some(file) = read_maybe_gzip(&file) {
            out.files += 1;
            // `read` takes a file decoded to its end. A truncated one gives up
            // its prefix under a name a complete copy also carries, and
            // `mark_dump_read` on it makes the skip permanent.
            if file.complete {
                out.read.push(name);
            } else {
                out.truncated += 1;
            }
            // One vectorised scan of the whole buffer per known device, and only
            // until the first hit — not a per-line test.
            if out.serial.is_none() {
                out.serial = known_serials
                    .iter()
                    .find(|s| file.text.contains(s.as_str()))
                    .cloned();
            }
            for line in file.text.lines().filter(|l| {
                l.contains(MARKER)
                    || l.contains(CATALOG_MARKER)
                    || METRIC_MARKERS.iter().any(|m| l.contains(m))
                    || POWER_MARKERS.iter().any(|m| l.contains(m))
            }) {
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

/// A decoded log file, and whether it decoded all the way to the end.
struct Decoded {
    text: String,
    /// False when the archive was truncated, so `text` is a valid prefix rather
    /// than the whole file. The caller must not record such a file as read.
    complete: bool,
}

/// Decode a log file, gunzipping it when it is gzipped. Returns whatever
/// decoded, including the valid prefix of a truncated archive.
fn read_maybe_gzip(path: &Path) -> Option<Decoded> {
    let bytes = std::fs::read(path).ok()?;
    // An empty file is a backup that produced nothing. Two of a real archive's
    // 31 were 0 bytes, on dates where `log_backup.sh` started and never
    // finished.
    if bytes.is_empty() {
        return None;
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        // A truncated member yields Err *after* writing what it decoded; a
        // header-level failure yields nothing. `complete` carries the
        // difference to [`Collected::read`].
        let complete = flate2::read::GzDecoder::new(&bytes[..])
            .read_to_end(&mut out)
            .is_ok();
        (!out.is_empty()).then(|| Decoded {
            text: String::from_utf8_lossy(&out).into_owned(),
            complete,
        })
    } else {
        Some(Decoded {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            complete: true,
        })
    }
}

// ---------------------------------------------------------------------------
// Parsing

/// Pull `name:<digits>` out of a line.
///
/// Anchored on the preceding separator so `TotalTime` cannot match inside a
/// longer field name. Any of three will do: a field normally follows the comma
/// after its neighbour, the first field of a line follows the `Information::`
/// prefix, and the first field of a payload read on its own has nothing in front
/// of it at all. Which field comes first is not fixed, because one firmware
/// routinely writes a payload with its head cut off (see [`observation`]).
fn field(line: &str, name: &str) -> Option<i64> {
    let needle = format!("{name}:");
    let bytes = line.as_bytes();
    let at = line.match_indices(&needle).find_map(|(at, _)| {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        matches!(before, None | Some(b',') | Some(b':')).then_some(at + needle.len())
    })?;
    let rest = &line[at..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The payloads a line carries, each `<Event>,<fields>` with the event name
/// possibly missing.
///
/// Usually one. A line carries two when the head of the first was cut, leaving
/// its tail in front of the `;` that begins the next. Fields have to be read
/// from a single payload: each states its own positions, so reading across the
/// boundary pairs one event's counter with another's book.
fn payloads(line: &str) -> impl Iterator<Item = &str> {
    line.split_once("Information::")
        .map_or("", |(_, rest)| rest)
        .split(';')
        .filter(|p| !p.is_empty())
}

/// The book's own end position within one payload, or `None` when the payload
/// states only the chapter's.
///
/// A payload states `EndPos` twice — the book's, then the current chapter's —
/// with the `NextTOCEntry…` group between them. What distinguishes them is which
/// side of that group they fall on, not which comes first: a payload can arrive
/// with its head cut away and the book's `EndPos` gone with it, leaving the
/// chapter's leading a payload it does not describe. The whole group is the
/// marker, not one field of it, because a cut lands anywhere.
///
/// Which is why the book's is the **last** `EndPos` before that group and not
/// the first. A cut lands mid-payload, so a line can open with the *tail* of the
/// payload before it — a chapter block, `NextTOCEntry` long gone — and the
/// book's own block then sits second, still correctly ahead of this payload's
/// group. Taking the first reads a chapter boundary as the book's identity, and
/// since that boundary moves as the reader advances, the sitting is cut into a
/// fresh run at every chapter and each fragment measures nothing.
///
/// Counted over three devices' archives: on uncut payloads the two readings are
/// the same value — 19,753 of 19,753 on a Colorsoft, 2,155 of 2,155 on a KOA2 —
/// so this is a no-op wherever the firmware writes a whole payload, and it is
/// only the mangled ones it rescues (3 of 9 on a Scribe, 1 of 2,155 on the
/// KOA2). With no group in the payload at all there is no marker to read, and
/// the first is the best available guess.
fn end_position(payload: &str) -> Option<i64> {
    const KEY: &str = "EndPos:YJPosition: ";
    let at = match payload.find("NextTOCEntry") {
        // No `EndPos` ahead of the group means the book's was cut away, and
        // what remains describes the chapter — no answer, rather than a wrong
        // one.
        Some(toc) => payload[..toc].rfind(KEY)?,
        None => payload.find(KEY)?,
    };
    let rest = &payload[at + KEY.len()..];
    let colon = rest.find(':')?;
    let tail = &rest[colon + 1..];
    let end = tail
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(tail.len());
    tail[..end].parse().ok()
}

/// The book a whole line is about, from whichever of its payloads names one.
fn book_position(line: &str) -> Option<i64> {
    payloads(line).find_map(end_position)
}

/// Every `(book fingerprint, point the reader stood at)` the events state.
///
/// The bridge from a log to a library, and the reason it is gathered from *every*
/// line rather than from the sessions: a line states a position whether or not
/// the parser can make a sitting out of it. `BookEndPosition` carries one and is
/// no session's observation; a book opened and shut carries one and its session
/// is discarded for having no duration. Both are the same evidence — a point that
/// a `.yjr` sidecar also names is that book — and the log offers it once.
///
/// Read within one payload, never across: a line can carry two, and each states
/// its own book, so pairing one payload's position with another's book files the
/// reader in a book they were not in.
fn positions_seen<'a>(events: impl IntoIterator<Item = &'a str>) -> Vec<(i64, Location)> {
    let mut out = Vec::new();
    for line in events {
        for payload in payloads(line) {
            if let (Some(book), Some(at)) = (end_position(payload), location(payload, "CurrentPos"))
            {
                out.push((book, at));
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// A point in a book, as the device writes it.
///
/// Every position on a line reads `<handle>:<coordinate>`. The coordinate alone
/// is a number two books can share; the handle carries the **source element
/// id**, which is the book's own vocabulary. Together they are specific enough
/// to identify a book by — see [`db::device_positions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Location {
    pub eid: i64,
    pub offset: i64,
    pub linear_pos: i64,
}

/// `<tag><eid><offset>` — one tag byte then two little-endian `u32`s, base64'd.
/// The same encoding the `.yjr` sidecars use for their anchors.
fn decode_handle(handle: &str) -> Option<(i64, i64)> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(handle)
        .ok()?;
    let eid = u32::from_le_bytes(raw.get(1..5)?.try_into().ok()?);
    let offset = u32::from_le_bytes(raw.get(5..9)?.try_into().ok()?);
    Some((eid as i64, offset as i64))
}

/// Read `<name>:YJPosition: <handle>:<coordinate>` out of one payload.
fn location(payload: &str, name: &str) -> Option<Location> {
    let key = format!("{name}:YJPosition: ");
    let rest = &payload[payload.find(&key)? + key.len()..];
    let (handle, tail) = rest.split_once(':')?;
    let end = tail
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(tail.len());
    let (eid, offset) = decode_handle(handle)?;
    Some(Location {
        eid,
        offset,
        linear_pos: tail[..end].parse().ok()?,
    })
}

/// True when the line names `event` as an event rather than merely containing
/// the word.
///
/// An event is written `<sep><Event>,`, where `<sep>` is the `Information::`
/// prefix or the `;` that terminates the payload before it. Requiring a
/// separator is what stops `CloseBook` matching inside a field value, and
/// accepting `;` is what finds an event that is not first on its line — which on
/// one firmware is where most of them are.
fn names(line: &str, event: &str) -> bool {
    let bytes = line.as_bytes();
    line.match_indices(event).any(|(at, _)| {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let after = bytes.get(at + event.len()).copied();
        matches!(before, Some(b':') | Some(b';')) && matches!(after, None | Some(b',') | Some(b';'))
    })
}

/// What one log line contributes to a session.
struct Observation {
    /// The book's own end position: this line's fingerprint for its book.
    position: i64,
    /// The book's running reading counter, in milliseconds.
    total_ms: Option<i64>,
    words: Option<i64>,
    page_turn: bool,
    closes: bool,
}

/// Read a line as an observation of some book's reading counter, or `None` when
/// the line is not about a book.
///
/// A line qualifies on what it carries — a running counter beside a book's end
/// position — rather than on being a named page event, because a mangled
/// payload keeps its fields and loses its name. Only page events and closes
/// carry `TotalTime`, so this selects the same lines on a log where every event
/// *is* named, and recovers the rest on one where they are not.
///
/// The name is still read wherever it survives, because nothing else can say
/// that a close ended the session or that an advance was forward rather than
/// back.
fn observation(line: &str) -> Option<Observation> {
    let page_turn = names(line, "NextPage");
    let closes = names(line, "CloseBook");
    // A named page event with no counter still marks a turn; the device omits
    // `TotalTime` from the ones it declines to credit.
    let named = page_turn || closes || names(line, "PreviousPage") || names(line, "GoToPosition");
    // The payload holding the counter, or — for those uncredited events — any
    // that at least says which book.
    let chosen = payloads(line)
        .find(|p| field(p, "TotalTime").is_some() && end_position(p).is_some())
        .or_else(|| {
            named
                .then(|| payloads(line).find(|p| end_position(p).is_some()))
                .flatten()
        })
        // A payload carrying `CurrentPos` and an end position, with no
        // `TotalTime` and no name. On a book the device refuses to time it is
        // the whole record of the sitting.
        .or_else(|| {
            payloads(line)
                .find(|p| end_position(p).is_some() && p.contains("CurrentPos:YJPosition: "))
        })?;
    Some(Observation {
        position: end_position(chosen)?,
        total_ms: field(chosen, "TotalTime"),
        words: field(chosen, "TotalWords"),
        page_turn,
        closes,
    })
}

/// A book's reading counter at the instant it was opened, in milliseconds, from
/// an `OpenBook` line's `StoredBookData`.
///
/// `TimeRead:9,229 sec.` — whole seconds, thousands-separated. `null` means a
/// book with no history, which is a counter of zero, not an absent one.
fn opened_at_counter(line: &str) -> Option<i64> {
    let rest = line.split_once("StoredBookData:")?.1;
    if rest.starts_with("null") {
        return Some(0);
    }
    let digits = rest.strip_prefix("TimeRead:")?;
    let end = digits
        .find(|c: char| !c.is_ascii_digit() && c != ',')
        .unwrap_or(digits.len());
    digits[..end]
        .replace(',', "")
        .parse::<i64>()
        .ok()
        .map(|s| s * 1000)
}

/// One stamped moment in the log.
#[derive(Debug, Clone)]
struct Moment {
    /// `YYYY-MM-DD`, the day the line fell on.
    day: String,
    /// Seconds into `day`.
    secs: i64,
    /// The same instant as a single running count of seconds.
    ///
    /// Elapsed time is measured on this and never on `secs`, which runs
    /// backwards at midnight: subtracting there reads a whole night's absence as
    /// no time at all.
    abs: i64,
    /// `YYYY-MM-DDTHH:MM:SS` — the form a session stores.
    at: String,
}

/// `YYMMDD:HHMMSS` at the start of a syslog line.
fn stamp(line: &str) -> Option<Moment> {
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
    // A real date, so `abs` counts days that exist. A prefix that parses as six
    // digits but names no day is not a log line this can place in time.
    let date = chrono::NaiveDate::from_ymd_opt(
        2000 + y.parse::<i32>().ok()?,
        mo.parse().ok()?,
        d.parse().ok()?,
    )?;
    let at = format!("{day}T{h}:{mi}:{s}");
    Some(Moment {
        day,
        secs,
        abs: chrono::Datelike::num_days_from_ce(&date) as i64 * 86_400 + secs,
        at,
    })
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

/// The `YYMMDD:HHMMSS` a syslog line begins with, or `None` when it begins with
/// something else.
///
/// The form a watermark travels in, read straight off a line rather than routed
/// through a stored session: what a device may drop is decided by which lines
/// this library holds, and a line is the thing that has to be named.
pub fn log_line_stamp(line: &str) -> Option<String> {
    stamp(line).map(|_| line[..13].to_string())
}

/// `YYYY-MM-DDTHH:MM:SS` as the parser's own instant, for a stored session being
/// read back — the round trip of what [`stamp`] produced when it was written.
fn moment(iso: &str) -> Option<Moment> {
    stamp(&log_stamp(iso)?)
}

/// When the device was awake, as spans of absolute seconds.
///
/// **The fallback measure, and only ever a fallback.** The Kindle's reading
/// timer is words-and-WPM driven, so a book it can count no words in — manga, a
/// fixed-layout magazine — is never timed at all: no `TotalTime`, no
/// `IntervalTime`, and the book's own info screen on the device reads zero.
/// Every rule in this module measures a sitting as the span of a counter that,
/// for such a book, does not exist.
///
/// What the log still states is when the device was awake. `powerd` writes a
/// clean state machine — `ACTIVE`, then `SCREEN SAVER` once the reader stops
/// touching it, then `READY TO SUSPEND`, `SUSPENDED`, `HIBERNATE` — so the time
/// a book was open *and* the device `ACTIVE` is an upper bound on the reading
/// done in it, and a tight one: measured from the last reading event to the end
/// of an `ACTIVE` span over a 30-day archive, the screensaver follows within a
/// median of 69 s and a p90 of 600 s. A book left open on a sleeping device
/// contributes nothing; one abandoned awake overcounts by the idle timeout and
/// no more.
///
/// That state machine reaches the log two ways — as a metrics record naming both
/// states, and as the LIPC events `powerd` fires at the same instants — and a
/// Kindle writes one, the other, or both. Reading only the record loses the
/// bound entirely on firmware that does not emit it, which is not a quieter
/// answer but a missing one: every sitting the timer refused there is dropped
/// for measuring zero. See [`power_change`].
///
/// Checked against 154 sittings whose counter *is* known, the bound reads 1.13×
/// the counted time at the median (p10 1.03), against 1.18× for unbounded wall
/// clock. Deliberately without a per-interval cap: a 120 s cap scores better on
/// that corpus only because its page turns are ~40 s apart, and against a
/// magazine's five-minute cadence the same cap would report six minutes for a
/// twenty-minute sitting.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Awake {
    spans: Vec<(i64, i64)>,
}

impl Awake {
    /// Read the power lines out of an event stream.
    ///
    /// A span opens where the device enters `ACTIVE` and closes at the next line
    /// that leaves it. One still open at the end of the batch is dropped rather
    /// than run to infinity: the device is awake *now*, the sitting it would
    /// bound is still in progress, and a later batch carries its own close.
    ///
    /// One family of line at a time, the record preferred. A Kindle that writes
    /// both writes them at the same second, and folding the two together lets a
    /// leave arriving after its own enter within that second close a span before
    /// it began — costing not the second but everything up to the next enter.
    pub fn from_events<'a>(events: impl IntoIterator<Item = &'a str>) -> Self {
        let changes: Vec<(Power, i64, Woke)> = events
            .into_iter()
            .filter_map(|line| {
                let (family, woke) = power_change(line)?;
                Some((family, stamp(line)?.abs, woke))
            })
            .collect();
        let family = if changes.iter().any(|(f, ..)| *f == Power::Record) {
            Power::Record
        } else {
            Power::Event
        };

        let mut spans = Vec::new();
        let mut open: Option<i64> = None;
        for (_, at, woke) in changes.iter().filter(|(f, ..)| *f == family) {
            match (woke, open) {
                (Woke::Active, None) => open = Some(*at),
                (Woke::Idle, Some(from)) if *at > from => {
                    spans.push((from, *at));
                    open = None;
                }
                (Woke::Idle, Some(_)) => open = None,
                _ => {}
            }
        }
        spans.sort_unstable();
        Self { spans }
    }

    /// Seconds of `ACTIVE` between two instants.
    pub fn between(&self, from: i64, to: i64) -> i64 {
        self.spans
            .iter()
            .map(|(s, e)| (to.min(*e) - from.max(*s)).max(0))
            .sum()
    }

    /// Whether any power line was seen at all. With none, a sitting the device
    /// declined to count stays unmeasured rather than being credited the whole
    /// wall clock — an unbounded guess is worse than the honest zero the device
    /// itself reports.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

/// The widest a page's dwell may run past what its words justify, and the
/// narrowest it may fall short. Below the floor the page was skipped past;
/// above the ceiling the reader was idle on it, and the ceiling is what counts.
const DWELL_FLOOR: f64 = 0.5;
const DWELL_CEILING: f64 = 1.5;

/// The WPM band inside which a rate is usable, and the words a page is assumed
/// to carry where it states none.
const WPM_MIN: f64 = 0.0;
const WPM_MAX: f64 = 500.0;

/// What a page with no usable rate may count, in seconds. A fixed-layout page
/// states no words, so no rate applies to it and only the dwell itself remains.
const WORDLESS_FLOOR: f64 = 3.0;
const WORDLESS_CEILING: f64 = 120.0;

/// How much of a page's dwell counts as reading, in milliseconds.
///
/// The device's own rule, from `PageHeuristicsImpl` in
/// `ReadingDataAggregatorService.jar`, defaults from `KFTResources`. Applied
/// verbatim: it has a defined answer for a page carrying no words, which is
/// exactly the content `ReadingTimerController` refuses, and matching it keeps
/// these figures comparable with the ones the device shows for itself.
fn dwell_ms(wpm: Option<f64>, words: i64, dwell_ms: i64) -> i64 {
    let secs = dwell_ms as f64 / 1000.0;
    match wpm {
        Some(wpm) if wpm > WPM_MIN && wpm < WPM_MAX && words > 0 => {
            let expected = words as f64 / (wpm / 60.0);
            if secs < DWELL_FLOOR * expected {
                0
            } else {
                (secs.min(DWELL_CEILING * expected) * 1000.0) as i64
            }
        }
        _ if secs < WORDLESS_FLOOR => 0,
        _ => (secs.min(WORDLESS_CEILING) * 1000.0) as i64,
    }
}

/// Which family of line stated a power change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Power {
    /// The metrics record, naming the state moved from and the state moved to.
    Record,
    /// The LIPC event `powerd` fires as it makes the same move.
    Event,
}

/// Whether a power change left the device awake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Woke {
    Active,
    Idle,
}

/// Read a line as a change into or out of `ACTIVE`.
///
/// The record states both ends of the move, so it is read on both: entering
/// `ACTIVE` opens a span and leaving it closes one. The LIPC event states only
/// the move it is named for, and the pair that brackets `ACTIVE` is
/// `outOfScreenSaver` / `goingToScreenSaver` — verified against a device that
/// writes both, where every record naming `ACTIVE` on either side has its event
/// in the same second and neither family has a transition the other lacks.
/// `suspending` closes a span too, for a device that reaches sleep without
/// passing the screensaver.
///
/// Not every wake is a wake to a reader: a Kindle rises from suspend on its own
/// to sync, and says so with `wakeupFromSuspend` and `resuming` while staying in
/// the screensaver. Neither opens a span.
fn power_change(line: &str) -> Option<(Power, Woke)> {
    if line.contains(POWER_MARKERS[0]) {
        return match (
            field_text(line, "curr_state").is_some_and(|s| s == "ACTIVE"),
            field_text(line, "prev_state").is_some_and(|s| s == "ACTIVE"),
        ) {
            (true, _) => Some((Power::Record, Woke::Active)),
            (_, true) => Some((Power::Record, Woke::Idle)),
            _ => None,
        };
    }
    if line.contains(POWER_MARKERS[1]) {
        return Some((Power::Event, Woke::Active));
    }
    if POWER_MARKERS[2..].iter().any(|m| line.contains(m)) {
        return Some((Power::Event, Woke::Idle));
    }
    None
}

/// Read `"<name>" : "<value>"` out of a metrics record's JSON-ish body.
fn field_text<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let at = line.find(&format!("\"{name}\""))? + name.len() + 2;
    let rest = &line[at..];
    let open = rest.find('"')?;
    let tail = &rest[open + 1..];
    Some(&tail[..tail.find('"')?])
}

/// Read `"<name>" : <number>` out of the same body. Distinct from
/// [`field_text`], which reads past an unquoted value into the next field.
fn field_num(line: &str, name: &str) -> Option<i64> {
    let at = line.find(&format!("\"{name}\" : "))? + name.len() + 5;
    let rest = &line[at..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// What one `fastmetrics` record contributes to the run open around it.
///
/// The records name no book. They state a page and a turn for whatever the
/// reader had open, which is the run [`parse_sessions`] is already tracking off
/// the `ReadingTimerController` lines — and those lines still open and position
/// a book the timer declines to count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Metric {
    /// `ereader_book_consume_content`: a page, with the words on it.
    Page { words: i64 },
    /// A forward turn, from `ereader_book_page_turn` or
    /// `ereader_book_linear_page_actions`.
    Forward,
    /// A backward turn, which advances no reading.
    Back,
    /// `ereader_close_book`.
    Close,
}

/// What tags a line the collector wrote about the device's own catalog, rather
/// than one the firmware logged. See [`catalog_row`].
pub const CATALOG_MARKER: &str = "SidleCatalog:";

/// A book the device's content catalog names, as `(extent, key)`.
///
/// `extent` is `p_contentSize`, which equals `BookEndPosition.FromBook` to the
/// digit and is therefore the same number a session is keyed by. `key` is
/// `p_cdeKey` — `books.asin` for a Sidle sideload, `books.amazon_asin` for a
/// store book.
///
/// This is what the two `cvm` firmwares have instead of [`cde_key`]: they
/// redact the book on every reading line, and the reader shell that names it
/// outright arrives with 5.19.
fn catalog_row(line: &str) -> Option<(i64, &str)> {
    let rest = line.split_once(CATALOG_MARKER)?.1;
    let extent = rest.split_once("extent=")?.1;
    let extent: i64 = extent[..extent.find(',')?].parse().ok()?;
    let key = rest.split_once("cde_key=")?.1;
    let end = key.find([',', ';'])?;
    (end > 0).then_some((extent, &key[..end]))
}

/// The `cde_key` a reader-shell record states for the book it is about.
///
/// The reading-timer lines redact the book on every one of them
/// (`Title:<private>,Asin:<private>`), and these do not: the latency records
/// name it outright, one per open, per close and per page turn. It is the
/// catalog's own key — an Amazon ASIN for a store book, and for a sideload the
/// content id Sidle bakes in, so it joins `books.amazon_asin` and `books.asin`
/// respectively.
///
/// `N/A` is the reader shell's own filler for a book it has no key for.
fn cde_key(line: &str) -> Option<&str> {
    if !METRIC_MARKERS.iter().any(|m| line.contains(m)) {
        return None;
    }
    match field_text(line, "cde_key") {
        Some(k) if !k.is_empty() && k != "N/A" => Some(k),
        _ => None,
    }
}

/// Days from the Common Era to 1970-01-01, the origin [`Moment::abs`] counts
/// from and an epoch stamp does not.
const CE_DAYS_TO_EPOCH: i64 = 719_163;

/// Timezones are whole minutes from UTC, and a record is logged a moment after
/// the instant it states.
const OFFSET_STEP: i64 = 60;

/// Seconds the device's local clock stands ahead of UTC, from a record stating
/// an instant the line's own prefix also states.
///
/// The prefix is local wall clock and carries no zone; `close_timestamp` and
/// `action_start_time` are epoch milliseconds. Their difference is the offset,
/// exact to the second before rounding — a Colorsoft logging `close_timestamp`
/// 1786882311101 under the prefix `260816:141151` is +02:00 on the nose.
///
/// The reading-timer lines state no absolute instant at all, so before these
/// records nothing in a reading log said which zone its days were counted in.
fn utc_offset(now: &Moment, line: &str) -> Option<i64> {
    if !METRIC_MARKERS.iter().any(|m| line.contains(m)) {
        return None;
    }
    let epoch_ms =
        field_num(line, "close_timestamp").or_else(|| field_num(line, "action_start_time"))?;
    let naive = now.abs - CE_DAYS_TO_EPOCH * 86_400;
    let raw = naive - epoch_ms.div_euclid(1000);
    // A device whose clock is simply wrong states no zone worth recording.
    (raw.abs() < 24 * 3600).then(|| (raw as f64 / OFFSET_STEP as f64).round() as i64 * OFFSET_STEP)
}

/// Read a line as a `fastmetrics` record.
fn metric(line: &str) -> Option<Metric> {
    if line.contains(METRIC_MARKERS[2]) {
        return Some(Metric::Page {
            words: field_num(line, "words_count").unwrap_or(0),
        });
    }
    if line.contains(METRIC_MARKERS[1]) {
        return Some(Metric::Close);
    }
    if line.contains(METRIC_MARKERS[3]) || line.contains(METRIC_MARKERS[4]) {
        // The two records that carry an `action_id`: `ereader_book_page_turn`
        // on one stack, naming `PrevPageTurnWithSWIPE`, and
        // `ereader_book_linear_page_actions` on the other, naming
        // `NextPageWithTap`. `ereader_content_point` beside them carries a
        // `point_type` and no action.
        return match field_text(line, "action_id") {
            Some(a) if a.starts_with("Next") => Some(Metric::Forward),
            Some(a) if a.starts_with("Prev") => Some(Metric::Back),
            _ => None,
        };
    }
    None
}

/// Turn an ordered, de-duplicated event stream into sessions.
///
/// A session is a run of page events on one book. It ends when the book changes,
/// when the reader closes it, when the gap to the next event exceeds
/// [`SESSION_GAP_SECS`] — and at midnight, because every figure the log reports
/// is grouped by the day a session began. Its duration is the span of the book's
/// running `TotalTime` counter across the run — not the wall clock between first
/// and last event, which would count the time the device sat asleep on an open
/// page.
///
/// Midnight is the one break the reader did not make, and it is the only one
/// where the interval that straddles it is **divided** rather than dropped: see
/// [`Break`].
///
/// `resume` is the sitting the reader may still be in — the run these events
/// continue rather than follow. Without it every batch of events starts a new
/// run at its first line, which is only correct when the batch begins where the
/// reading did; see [`Resume`].
pub fn parse_sessions<'a>(
    events: impl IntoIterator<Item = &'a str>,
    resume: Option<&Resume>,
) -> Vec<Session> {
    // Collected: [`Awake`] reads the whole stream before the first sitting
    // closes against it, and the power lines interleave with the reading ones.
    // One pointer per line, over a batch held in memory.
    let lines: Vec<&str> = events.into_iter().collect();
    let awake = Awake::from_events(lines.iter().copied());

    let mut out = Vec::new();
    let mut open: Option<Open> = Open::resume(resume);
    let mut prev_abs: Option<i64> = open.as_ref().map(|cur| cur.last.abs);
    let mut opened: Option<Opened> = None;
    // A break, once noticed, waits here until an observation can act on it.
    let mut gapped = false;
    let mut first = true;
    // The catalog key most recently named, for the run it belongs to. Cleared
    // at a break: the key names the book the reader shell had open, and past a
    // break that is a different book until its own record says otherwise.
    let mut named: Option<String> = None;
    // The zone most recently stated. Unlike `named` this survives a break: a
    // device does not change zone between two sittings of one batch, and a
    // sitting whose own records state none is still in the zone the batch did.
    let mut zone: Option<i64> = None;
    // `fastmetrics` records seen before any run is open. On a book the timer
    // refuses, the first `ReadingTimerController` line carrying a position can
    // lag the open by minutes. Drained into the run; dropped at a break.
    let mut pending: Vec<(Moment, Metric)> = Vec::new();

    for line in lines.iter().copied() {
        let Some(now) = stamp(line) else {
            continue;
        };
        if std::mem::take(&mut first) && open.as_ref().is_some_and(|cur| now.abs < cur.last.abs) {
            // Events predating the run they were offered against — a whole
            // archive replayed host-side, which holds the run itself. The run
            // is dropped and its stored row left as it is.
            open = None;
            prev_abs = None;
        }
        // Measured against the previous event of any kind, held for an
        // observation to act on: `OpenBook`, `Reading_Resumed` and
        // `TapOnFooter` carry neither counter nor position.
        gapped |= prev_abs.is_some_and(|prev| now.abs - prev > SESSION_GAP_SECS);
        prev_abs = Some(now.abs);

        if let Some(counter_ms) = opened_at_counter(line) {
            opened = Some(Opened {
                counter_ms,
                at: now.clone(),
            });
        }

        if let Some(key) = cde_key(line) {
            named = Some(key.to_string());
            if let Some(cur) = open.as_mut() {
                cur.asin = named.clone();
            }
        }
        if let Some(offset) = utc_offset(&now, line) {
            zone = Some(offset);
            if let Some(cur) = open.as_mut() {
                cur.tz_offset_s = zone;
            }
        }
        let Some(obs) = observation(line) else {
            // A `fastmetrics` record measures the run the
            // `ReadingTimerController` lines have open. It names no book, so
            // one arriving ahead of a run waits in `pending`.
            if let Some(m) = metric(line) {
                match open.as_mut() {
                    Some(cur) => cur.observe_metric(&now, &m, &awake),
                    None => pending.push((now.clone(), m)),
                }
            }
            continue;
        };
        // Consumed here and only here: whatever happened between two
        // observations is decided at the second of them.
        let gapped = std::mem::take(&mut gapped);

        // Consumed at the first observation after the open, used or not: an
        // open belongs to the session it precedes. An observation with no
        // counter cannot vouch for one, and leaves it pending.
        let mut seed = match obs.total_ms {
            Some(_) => opened.take().and_then(|o| o.vouch(&now, obs.total_ms)),
            None => opened.as_ref().and_then(|o| o.opened_run(&now)),
        };

        match open
            .as_ref()
            .and_then(|cur| cur.broken_by(&now, &obs, gapped))
        {
            None => {}
            Some(Break::Left) => {
                out.extend(open.take().and_then(|cur| cur.emit(&awake)));
                named = None;
                pending.clear();
            }
            Some(Break::Midnight(boundary)) => {
                out.extend(open.take().map(|cur| cur.finish_at(&boundary)));
                // The run was already under way, so nothing that happened after
                // midnight can be its start: the boundary is, and an `OpenBook`
                // seen in between describes a book that was already open.
                seed = Some(boundary);
            }
        }
        let fresh = open.is_none();
        let cur = open.get_or_insert_with(|| Open::new(obs.position, &now, seed.take()));
        if fresh {
            cur.asin = named.clone();
            cur.tz_offset_s = zone;
        }
        // The page records that arrived before this run existed, in order, from
        // where the run itself began. Anything older belongs to no sitting.
        if fresh {
            let from = cur.began.abs;
            for (at, m) in std::mem::take(&mut pending) {
                if at.abs >= from {
                    cur.observe_metric(&at, &m, &awake);
                }
            }
        }
        // A run opened by a position-only line adopts the first counter an
        // open vouches for. Unadopted, that counter becomes both ends of the
        // span and the sitting measures nothing.
        if let Some(counter) = seed.take().and_then(|s| s.counter_ms)
            && cur.time_lo.is_none()
        {
            cur.time_lo = Some(counter);
            cur.time_hi = counter;
            cur.last_time = Some(counter);
        }
        cur.observe(&now, &obs);
        if obs.closes {
            out.extend(open.take().and_then(|cur| cur.emit(&awake)));
        }
    }
    out.extend(open.and_then(|cur| cur.emit(&awake)));
    out
}

/// How a run in progress ends when a new observation cannot join it.
enum Break {
    /// The reader left, or moved to another book. The run ends where it was last
    /// seen and the next one starts from scratch — whatever the counter did in
    /// between is not reading anyone did on either side of the break.
    Left,
    /// Midnight, with the reader still reading. The reader broke nothing, so the
    /// interval straddling the boundary is real reading time that belongs to
    /// both days, and the run is cut *at* midnight with the counters
    /// interpolated there: the day just ended keeps the share before, the day
    /// beginning starts from the same value and keeps the share after.
    ///
    /// Cutting it like any other break would credit that interval to neither —
    /// the device states a counter only at each event, so the advance across the
    /// boundary falls between the two sessions and is lost. A reader who stops
    /// shortly after midnight loses the whole of it, which is what makes a late
    /// night look like an early evening that ended at 23:5x.
    Midnight(Start),
}

/// An `OpenBook` seen but not yet attached to a session.
struct Opened {
    counter_ms: i64,
    at: Moment,
}

/// Where a session begins: the counters it resumes from and the instant it
/// started at. Either the `OpenBook` that vouched for them, or the midnight a
/// run was cut at.
///
/// The counter is absent for a run in a book the device does not time. The
/// instant is not: the reader opened it then whether or not anything counted.
struct Start {
    counter_ms: Option<i64>,
    /// The word counter at that same instant, where it is known. An `OpenBook`
    /// states no word count; a midnight cut interpolates one.
    words: Option<i64>,
    at: Moment,
}

impl Opened {
    /// This open as a session start, or `None` when it cannot be vouched for.
    ///
    /// An open states no position, so it cannot prove which book it belongs to.
    /// Two guards stand in: the counter must not already exceed the first
    /// observation's, and the reading it would add must fit inside the wall
    /// clock since the open, because counted reading time cannot outrun the
    /// clock. A stale open from another book fails one or the other; a genuine
    /// one clears both with room to spare.
    fn vouch(self, now: &Moment, first_total: Option<i64>) -> Option<Start> {
        let total = first_total?;
        let elapsed = now.abs.checked_sub(self.at.abs).filter(|e| *e >= 0)?;
        (self.at.day == now.day
            && self.counter_ms <= total
            && total - self.counter_ms <= (elapsed + SEED_SLACK_SECS) * 1000)
            .then_some(Start {
                counter_ms: Some(self.counter_ms),
                words: None,
                at: self.at,
            })
    }

    /// This open as the instant a run began, for a run with no counter to
    /// vouch it against.
    ///
    /// The counter guards cannot be applied — there is nothing to compare — so
    /// the day is all that stands between this and a stale open from another
    /// book. That is enough here: the figure it produces is bounded by how long
    /// the device was awake, so a start too early costs nothing where the
    /// reader was not there.
    fn opened_run(&self, now: &Moment) -> Option<Start> {
        (self.at.day == now.day && self.at.abs <= now.abs).then(|| Start {
            counter_ms: None,
            words: None,
            at: self.at.clone(),
        })
    }
}

/// The share of a counter's advance that falls before a boundary inside the
/// interval it was measured over, in proportion to the wall clock either side.
///
/// The device credits an interval in full at its far end, so this is the only
/// thing the log says about where inside the interval the reading happened:
/// evenly, which is also how [`super::db::reading_clock`] spreads a session
/// across the hours it covers.
fn share(advance: i64, elapsed: i64, before: i64) -> i64 {
    if elapsed <= 0 || advance <= 0 {
        return 0;
    }
    advance * before.clamp(0, elapsed) / elapsed
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
    /// The last observation's instant and counters — the near side of the
    /// interval a midnight cut has to divide, and of the one each new
    /// observation books against the clock.
    last: Moment,
    last_time: Option<i64>,
    last_words: Option<i64>,
    /// Milliseconds of counted reading booked against each hour of the day, as
    /// the intervals arrive. Kept here because this is the only place the
    /// intervals exist: a stored session has only its window and its total.
    hours_ms: [i64; 24],
    /// The first instant this run was seen at, and seconds it was credited
    /// before this batch — the two a run with no counter is rebuilt from. See
    /// [`Awake`].
    began: Moment,
    carried_secs: i64,
    /// Forward turns from the `fastmetrics` records, counted apart from
    /// `page_turns`. One stack names every turn on its
    /// `ReadingTimerController` lines and the other names none, while both
    /// write these — so adding the two together doubles a turn on the first.
    metric_turns: i64,
    /// Milliseconds of page dwell through [`dwell_ms`], and the page open at
    /// the far end of the interval each new page closes.
    dwell_total_ms: i64,
    dwell_hours_ms: [i64; 24],
    open_page: Option<(Moment, i64)>,
    /// The catalog key a reader-shell record named for this run. See [`cde_key`].
    asin: Option<String>,
    /// The offset most recently stated during the run. See [`utc_offset`].
    tz_offset_s: Option<i64>,
    /// Whether any event of this batch has joined the run. False only for a
    /// resumed run no event continued, which is a stored session this batch has
    /// nothing new to say about.
    touched: bool,
}

impl Open {
    /// `start` is where the book was opened, where an `OpenBook` vouched for it
    /// or where midnight cut the run before it. That is the session's true floor
    /// in three senses: the first *logged* observation can sit minutes past the
    /// open, so taking it as the floor discards the reading in between, reports a
    /// sitting that took no time at all, and books what it does count to the
    /// wrong hours. Without one, the first observation is the best available
    /// start.
    fn new(end_position: i64, now: &Moment, start: Option<Start>) -> Self {
        let (time_lo, words_lo, from) = match start {
            Some(s) => (s.counter_ms, s.words, s.at),
            None => (None, None, now.clone()),
        };
        let from_moment = from.clone();
        Self {
            end_position,
            started_at: from.at.clone(),
            ended_at: now.at.clone(),
            time_lo,
            time_hi: time_lo.unwrap_or(0),
            words_lo,
            words_hi: words_lo.unwrap_or(0),
            page_turns: 0,
            last: from,
            // A vouched start is a counter reading at a known instant, so the
            // stretch from there to the first observation is an interval like any
            // other and is booked like one.
            last_time: time_lo,
            last_words: words_lo,
            hours_ms: [0; 24],
            began: from_moment,
            carried_secs: 0,
            metric_turns: 0,
            dwell_total_ms: 0,
            dwell_hours_ms: [0; 24],
            open_page: None,
            asin: None,
            tz_offset_s: None,
            touched: false,
        }
    }

    /// The run a previous batch of events left open, rebuilt from the session it
    /// was stored as.
    ///
    /// Everything a run needs is on the row: where it started on the clock and
    /// on the counter, where it was last seen on both, and the hours booked so
    /// far. Restored whole rather than as a fresh run anchored at the stored end,
    /// so the session this produces carries the *original* `started_at` — that
    /// being the identity the row is stored under — and reports the sitting's
    /// whole length rather than the part of it this batch could see.
    ///
    /// `None` for a caller with no sitting to continue, which is the ordinary
    /// case: a device syncing for the first time, or one whose last session
    /// finished long ago.
    fn resume(resume: Option<&Resume>) -> Option<Self> {
        let r = resume?;
        let last = moment(&r.ended_at)?;
        let mut hours_ms = [0; 24];
        for (hour, seconds) in &r.hours {
            if let Some(slot) = hours_ms.get_mut(*hour as usize) {
                *slot = seconds * 1000;
            }
        }
        // Both ends of the word counter or neither: one alone would have the run
        // report the difference between a reading and nothing.
        let (words_lo, words_hi) = match (r.start_words, r.end_words) {
            (Some(lo), Some(hi)) => (Some(lo), hi),
            _ => (None, 0),
        };
        Some(Self {
            end_position: r.end_position,
            started_at: r.started_at.clone(),
            ended_at: r.ended_at.clone(),
            time_lo: r.start_counter_ms,
            time_hi: r.end_counter_ms.unwrap_or(0),
            words_lo,
            words_hi,
            page_turns: r.page_turns,
            // A run with no counter resumes from where it was last seen. The
            // records covering the earlier stretch went with the batch that
            // measured it.
            began: last.clone(),
            carried_secs: match r.measure {
                Measure::Counted => 0,
                _ => r.seconds,
            },
            last,
            last_time: r.end_counter_ms,
            last_words: r.end_words,
            hours_ms,
            metric_turns: 0,
            dwell_total_ms: 0,
            dwell_hours_ms: [0; 24],
            open_page: None,
            asin: None,
            tz_offset_s: None,
            touched: false,
        })
    }

    /// The run as a session, unless it is a resumed one this batch never
    /// continued — that is the stored row unchanged, and re-offering it would
    /// have every sync report a sitting it did nothing to.
    fn emit(self, awake: &Awake) -> Option<Session> {
        self.touched.then(|| self.finish(awake))
    }

    /// Book an interval's counted reading against the clock hours it ran
    /// through, `from` and `to` being seconds into the day.
    ///
    /// Evenly across the interval, which is the whole of what the log says: the
    /// device credits an interval in full at its far end and states nothing
    /// about where inside it the reading fell. An interval is a page turn wide,
    /// so that assumption spans minutes — where spreading a *session* spans
    /// hours, and is the guess this exists to avoid.
    fn credit(hours_ms: &mut [i64; 24], from: i64, to: i64, advance_ms: i64) {
        if advance_ms <= 0 {
            return;
        }
        let hour = |secs: i64| ((secs / 3600) as usize).min(23);
        let span = to - from;
        if span <= 0 {
            hours_ms[hour(from)] += advance_ms;
            return;
        }
        let mut placed = 0;
        for h in (from / 3600)..=((to - 1) / 3600) {
            let overlap = to.min((h + 1) * 3600) - from.max(h * 3600);
            if overlap <= 0 {
                continue;
            }
            let share = advance_ms * overlap / span;
            placed += share;
            hours_ms[hour(h * 3600)] += share;
        }
        // The division's remainder to the hour the interval began in, so the
        // hours of a session add back up to the session.
        hours_ms[hour(from)] += advance_ms - placed;
    }

    fn observe(&mut self, now: &Moment, obs: &Observation) {
        self.touched = true;
        if let (Some(from), Some(to)) = (self.last_time, obs.total_ms) {
            Self::credit(&mut self.hours_ms, self.last.secs, now.secs, to - from);
        }
        self.ended_at = now.at.clone();
        self.last = now.clone();
        if obs.page_turn {
            self.page_turns += 1;
        }
        if let Some(t) = obs.total_ms {
            self.time_lo = Some(self.time_lo.map_or(t, |lo| lo.min(t)));
            self.time_hi = self.time_hi.max(t);
            self.last_time = Some(t);
        }
        if let Some(w) = obs.words {
            self.words_lo = Some(self.words_lo.map_or(w, |lo| lo.min(w)));
            self.words_hi = self.words_hi.max(w);
            self.last_words = Some(w);
        }
        // A book reopened after finishing restarts its counter; the run so far is
        // already banked because a fresh `Open` is made per run.
    }

    /// Fold one `fastmetrics` record into the run.
    ///
    /// A page closes the interval the page before it opened, and [`dwell_ms`]
    /// says how much of that interval counts. The run's end and its break rules
    /// are the `ReadingTimerController` lines' to decide, so this moves neither
    /// `last` nor `ended_at` and does not mark the run touched — a batch of
    /// nothing but these records describes a book no line here has opened.
    fn observe_metric(&mut self, now: &Moment, m: &Metric, awake: &Awake) {
        match m {
            Metric::Forward => self.metric_turns += 1,
            Metric::Back => {}
            Metric::Close => self.open_page = None,
            Metric::Page { words } => {
                if let Some((from, from_words)) = self.open_page.take() {
                    let elapsed = (now.abs - from.abs) * 1000;
                    // A page turned while the device slept spans the sleep. The
                    // awake bound cuts it back where power records exist, and
                    // [`dwell_ms`]'s own ceiling holds where they do not.
                    let elapsed = match awake.is_empty() {
                        true => elapsed,
                        false => awake.between(from.abs, now.abs) * 1000,
                    };
                    let counts = dwell_ms(self.wpm(), from_words, elapsed);
                    self.dwell_total_ms += counts;
                    Self::credit(&mut self.dwell_hours_ms, from.secs, now.secs, counts);
                }
                self.open_page = Some((now.clone(), *words));
            }
        }
    }

    /// The rate the device states for this book, from the word and time
    /// counters it has moved so far. `None` leaves [`dwell_ms`] on its wordless
    /// branch, which is the answer for a book stating no words at all.
    fn wpm(&self) -> Option<f64> {
        let secs = (self.time_hi - self.time_lo?) as f64 / 1000.0;
        let words = (self.words_hi - self.words_lo?) as f64;
        (secs > 0.0 && words > 0.0).then(|| words / (secs / 60.0))
    }

    /// Whether this observation ends the run, and how. See [`Break`].
    fn broken_by(&self, now: &Moment, obs: &Observation, gapped: bool) -> Option<Break> {
        if self.end_position != obs.position || gapped {
            return Some(Break::Left);
        }
        if self.last.day == now.day {
            return None;
        }
        // Mid-run, over midnight: both counters are interpolated at the
        // boundary from the observations either side. With a counter missing on
        // either side the day change is [`Break::Left`].
        let (Some(from), Some(to)) = (self.last_time, obs.total_ms) else {
            return Some(Break::Left);
        };
        let elapsed = now.abs - self.last.abs;
        let before = now.abs - now.secs - self.last.abs;
        Some(Break::Midnight(Start {
            counter_ms: Some(from + share(to - from, elapsed, before)),
            words: self
                .last_words
                .zip(obs.words)
                .map(|(from, to)| from + share(to - from, elapsed, before)),
            at: Moment {
                day: now.day.clone(),
                secs: 0,
                abs: now.abs - now.secs,
                at: format!("{}T00:00:00", now.day),
            },
        }))
    }

    /// Close the run at the midnight it was cut at, crediting this day the share
    /// of the unfinished interval that fell before the boundary.
    ///
    /// The stored end is one second short of the boundary, that being the last
    /// instant this day has: reading recorded at `T00:00:00` is the next day's.
    fn finish_at(mut self, boundary: &Start) -> Session {
        // A midnight cut is only made where both sides state a counter, so this
        // boundary carries one.
        let at_boundary = boundary.counter_ms.unwrap_or(self.time_hi);
        if let Some(from) = self.last_time {
            // Midnight is 86400 seconds into *this* day; the same instant is
            // second zero of the next, where the run resumes.
            Self::credit(
                &mut self.hours_ms,
                self.last.secs,
                86_400,
                at_boundary - from,
            );
        }
        self.time_hi = self.time_hi.max(at_boundary);
        if let Some(w) = boundary.words {
            self.words_hi = self.words_hi.max(w);
        }
        self.ended_at = format!("{}T23:59:59", self.last.day);
        self.finish(&Awake::default())
    }

    /// The run as a session, under the best [`Measure`] its records support.
    ///
    /// [`Measure::Counted`] first: the device's own accounting, excluding the
    /// pauses an awake reader takes. Where the counter never moved across the
    /// whole run — the device declining to time a book it can count no words in
    /// — [`Measure::Dwell`] measures the same run off the page records, and
    /// [`Measure::Awake`] bounds it where those are absent too.
    ///
    /// A run with none of the three keeps the counter's zero. An unbounded wall
    /// clock credits a book left open overnight with the night.
    fn finish(self, awake: &Awake) -> Session {
        let counted = (self.time_hi - self.time_lo.unwrap_or(self.time_hi)) / 1000;
        let dwell = self.dwell_total_ms / 1000;
        let (seconds, measure) = match (counted, dwell) {
            (c, _) if c > 0 => (c, Measure::Counted),
            (_, d) if d > 0 => (self.carried_secs + d, Measure::Dwell),
            _ if awake.is_empty() => (0, Measure::Counted),
            _ => (
                self.carried_secs + awake.between(self.began.abs, self.last.abs),
                Measure::Awake,
            ),
        };
        Session {
            hours: match measure {
                Measure::Counted => hours_in_seconds(&self.hours_ms, seconds),
                Measure::Dwell => hours_in_seconds(&self.dwell_hours_ms, seconds),
                Measure::Awake => spread(&self.began, &self.last, seconds),
            },
            started_at: self.started_at,
            ended_at: self.ended_at,
            end_position: self.end_position,
            seconds,
            // `page_turns` where that stack names any, `metric_turns` where it
            // names none. One stack names every turn and the other names none,
            // while both write the records; a sum doubles the first.
            page_turns: match self.page_turns {
                0 => self.metric_turns,
                named => named,
            },
            words: self.words_hi - self.words_lo.unwrap_or(self.words_hi),
            // Both ends of the counter, not just their difference: a later batch
            // of events continues this run from where it stopped counting, and
            // only the values say where that is.
            start_counter_ms: self.time_lo,
            end_counter_ms: self.time_lo.map(|_| self.time_hi),
            start_words: self.words_lo,
            end_words: self.words_lo.map(|_| self.words_hi),
            measure,
            asin: self.asin,
            tz_offset_s: self.tz_offset_s,
        }
    }
}

/// Spread an estimated total evenly across the hours its window covers.
///
/// An estimate has no intervals behind it — that is what makes it an estimate —
/// so there is nothing finer to place it by, and claiming otherwise would dress
/// a guess as a measurement. Even spreading is the same assumption
/// [`super::db::reading_clock`] already makes of a session it has to divide.
fn spread(from: &Moment, to: &Moment, seconds: i64) -> Vec<(u8, i64)> {
    if seconds <= 0 {
        return Vec::new();
    }
    let span = (to.secs - from.secs).max(1);
    let mut out = Vec::new();
    let mut placed = 0;
    for hour in (from.secs / 3600)..=((to.secs.max(from.secs + 1) - 1) / 3600).min(23) {
        let overlap = to.secs.min((hour + 1) * 3600) - from.secs.max(hour * 3600);
        if overlap <= 0 {
            continue;
        }
        let share = seconds * overlap / span;
        placed += share;
        out.push((hour as u8, share));
    }
    match out.first_mut() {
        Some(first) => first.1 += seconds - placed,
        // A window covering no whole hour still happened somewhere, and the
        // hour it began in is where.
        None => out.push(((from.secs / 3600).min(23) as u8, seconds)),
    }
    out.retain(|(_, s)| *s > 0);
    out
}

/// The hours a session's milliseconds fall in, as whole seconds summing to
/// exactly `seconds`.
///
/// Truncating each hour on its own would shed up to a second per hour, so a
/// day's hours would quietly fall short of the day beside them. The running
/// total is what gets truncated instead, which spends the error inside the
/// session rather than losing it. A last correction covers the case the
/// milliseconds cannot account for — a counter that ran backwards mid-run, say —
/// because the two figures are shown side by side and must agree.
fn hours_in_seconds(hours_ms: &[i64; 24], seconds: i64) -> Vec<(u8, i64)> {
    let mut out = Vec::new();
    let (mut running, mut placed) = (0, 0);
    for (hour, ms) in hours_ms.iter().enumerate() {
        running += ms;
        let secs = running / 1000 - placed;
        placed += secs;
        if secs > 0 {
            out.push((hour as u8, secs));
        }
    }
    if placed != seconds
        && let Some(busiest) = out
            .iter_mut()
            .max_by_key(|(_, s)| *s)
            .filter(|(_, s)| *s + seconds - placed > 0)
    {
        busiest.1 += seconds - placed;
    }
    out
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
///
/// "Most" is not "all", which is why the pending value is dropped at every
/// `OpenBook`. A book that states no `BookEndPosition` would otherwise inherit
/// whichever one was last seen and be filed as an entirely different book.
/// Dropping it leaves such a book keyed by its own `EndPos`: unnamed until an
/// archive shows its mapping, which is the answer that is at least true.
fn frombook_map<'a>(
    events: impl IntoIterator<Item = &'a str>,
) -> std::collections::HashMap<i64, i64> {
    let mut map = std::collections::HashMap::new();
    let mut pending: Option<i64> = None;
    for line in events {
        if names(line, "OpenBook") {
            pending = None;
        }
        if let Some(at) = line.find("BookEndPosition.FromBook:YJPosition: ") {
            let rest = &line[at + "BookEndPosition.FromBook:YJPosition: ".len()..];
            pending = rest.split_once(':').and_then(|(_, tail)| {
                let end = tail
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(tail.len());
                tail[..end].parse().ok()
            });
        }
        if let (Some(from_book), Some(ep)) = (pending, book_position(line)) {
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
            truncated: found.truncated,
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
            truncated: found.truncated,
            events: found.events.len(),
            cancelled: true,
            ..Imported::default()
        });
    }
    // No sitting to resume: this reads whole archives, so it holds the run's own
    // earlier events and builds it from them rather than from a stored summary.
    let mut out = store_events(conn, &found.events, found.files, device_serial, None, watch)?;
    out.skipped = found.skipped;
    out.truncated = found.truncated;
    // Marked only now: a file counts as read once its events are stored, so an
    // interrupted or failed store leaves it to be read again.
    if !out.cancelled {
        for name in &found.read {
            db::mark_dump_read(conn, device_serial, name)?;
        }
    }
    Ok(out)
}

/// Keep the raw lines a device pushed, under `<root>/reading-log/<serial>/`.
///
/// **The device's copy is not a backup once this exists — it is the only other
/// copy, and it is deleted.** A Kindle archives what it logs and purges that
/// archive at the watermark the library hands back; the watermark is per
/// device, so a session stored for one book carries it past another book's
/// events, and those events are then dropped by a device that believes they are
/// safe here. What made them unstorable is rarely permanent — a book the parser
/// could not measure today is one a better parser measures tomorrow — so the
/// lines are kept whether or not they became a session.
///
/// Written in the same shape the device's own archive uses, one gzipped file
/// per day, so [`import`] can read the folder back with no special case: the
/// stored history and a fresh archive are the same thing.
///
/// A day's file is read, merged and rewritten rather than appended to, because
/// a torn append corrupts history that was already safe. The rename is what
/// publishes it.
pub fn archive_pushed(
    root: &Path,
    device_serial: &str,
    lines: &BTreeSet<String>,
) -> std::io::Result<()> {
    let Some(newest) = lines.iter().filter_map(|l| stamp(l)).map(|m| m.day).max() else {
        return Ok(());
    };
    let dir = root.join("reading-log").join(device_serial);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("rl_{}.txt.gz", newest.replace('-', "")));

    let mut all: BTreeSet<String> = lines.clone();
    if path.exists()
        && let Some(held) = read_maybe_gzip(&path)
    {
        all.extend(held.text.lines().map(str::to_string));
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    for line in &all {
        std::io::Write::write_all(&mut gz, line.as_bytes())?;
        std::io::Write::write_all(&mut gz, b"\n")?;
    }
    let bytes = gz.finish()?;
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
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
///
/// `resume` is the sitting these events may continue — see [`Resume`]. A caller
/// holding a whole archive passes `None`, because the run's own earlier events
/// are in `events`; a caller taking a device's newest events passes what the
/// library already stored, because they are not.
pub fn store_events(
    conn: &Connection,
    events: &BTreeSet<String>,
    files: usize,
    device_serial: &str,
    resume: Option<&Resume>,
    watch: job::Watch<'_>,
) -> rusqlite::Result<Imported> {
    // Grouped by the fingerprint every page event carries, then rekeyed to the
    // one [`db::books_with_last_position`] joins on. `record_book_ends` carries
    // the pairing across archives.
    let mut identity = frombook_map(events.iter().map(String::as_str));
    let learned: Vec<(i64, i64)> = identity.iter().map(|(k, v)| (*k, *v)).collect();
    db::record_book_ends(conn, &learned)?;
    for (last_word, from_book) in db::known_book_ends(conn)? {
        identity.entry(last_word).or_insert(from_book);
    }
    // Where the reader was seen standing, ahead of any session. A device sends
    // only what is newer than the newest session stored, and the `.yjr` sidecar
    // that names the book may arrive a sync later.
    for (fingerprint, at) in positions_seen(events.iter().map(String::as_str)) {
        let key = identity.get(&fingerprint).copied().unwrap_or(fingerprint);
        db::record_log_points(conn, key, &[(at.eid, at.offset, at.linear_pos)])?;
    }
    // A stored row is keyed by either of the book's two end constants. The run
    // is offered under the one the events use, which [`parse_sessions`] reads
    // as the same book.
    let resume = resume.map(|r| Resume {
        end_position: identity
            .iter()
            .find(|(_, key)| **key == r.end_position)
            .map_or(r.end_position, |(raw, _)| *raw),
        ..r.clone()
    });
    let sessions = parse_sessions(events.iter().map(String::as_str), resume.as_ref());
    // The books the device's catalog names, keyed by the number a session is.
    // A row states what the device holds, whatever this batch read.
    for (extent, key) in events.iter().filter_map(|l| catalog_row(l)) {
        db::record_log_asin(conn, extent, key)?;
    }
    // The keys the reader shell named these books by, against the fingerprints
    // the rows are stored under. Recorded ahead of the seconds test below: a
    // book opened and shut gets no row and is named all the same.
    for s in &sessions {
        if let Some(asin) = &s.asin {
            let key = identity
                .get(&s.end_position)
                .copied()
                .unwrap_or(s.end_position);
            db::record_log_asin(conn, key, asin)?;
        }
    }

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
        // A session of no measurable duration is a book opened and shut. A
        // judgement about the row alone: what its events witnessed is recorded
        // above.
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
            start_counter_ms: s.start_counter_ms,
            end_counter_ms: s.end_counter_ms,
            start_words: s.start_words,
            end_words: s.end_words,
            measure: s.measure,
            tz_offset_s: s.tz_offset_s,
        };
        // The hours the sitting's reading fell in, from the log's own intervals.
        // Written with the row: the two are one measurement. A continued run
        // rebuilds its whole distribution, and the hours are replaced.
        match db::insert_reading_session(conn, &row)? {
            db::Stored::Added => out.added += 1,
            db::Stored::Extended => out.extended += 1,
            db::Stored::Unchanged => continue,
        }
        db::record_session_hours(conn, &row, &s.hours)?;
    }
    out.attributed = db::resolve_reading_sessions(conn)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`collect_events`] takes every [`METRIC_MARKERS`] schema out of a file,
    /// and leaves `ereader_open_book_failure_backup`, whose name carries
    /// `ereader_open_book` as a prefix.
    #[test]
    fn a_reader_shell_record_is_collected_and_its_lookalike_is_not() {
        let dir = std::env::temp_dir().join(format!("rl-metrics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log_backup_260816181520.txt");
        let wanted = r#"260816:181520 fastmetrics[4576]: D fastmetrics:KindleFastMetricsPublisher:[9788.019459]: Emitting a new record. SchemaName[ereader_book_consume_content], Fields[{ "end_position" : 4133, "start_position" : 3227, "words_count" : 147 } ]. :"#;
        let lookalike = r#"260816:181521 fastmetrics[4576]: D fastmetrics:KindleFastMetricsPublisher:[9789.0]: Emitting a new record. SchemaName[ereader_open_book_failure_backup], Fields[{ "cde_key" : "B00QPFC59S" } ]. :"#;
        std::fs::write(&path, format!("{wanted}\n{lookalike}\n")).unwrap();

        let found = collect_events(&[&path], &Default::default(), &[], &mut super::job::ignore);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(found.events.len(), 1);
        assert!(found.events.iter().next().unwrap().contains("words_count"));
    }

    /// The lines a device pushed survive whether or not the library could make
    /// a session of them, and come back through the ordinary import.
    ///
    /// The case this exists for: a device pushes two books' events, one of
    /// which forms a session and one of which does not. The watermark the
    /// device purges against is per device, so it moves past both, and the
    /// device drops its own copy of both. What the parser could not use today
    /// has to still be here tomorrow.
    #[test]
    fn pushed_lines_are_kept_whether_or_not_they_became_a_session() {
        let root = tempfile::tempdir().expect("temp dir");
        let unmeasurable = "260803:100000 java[1]: I ReadingTimerController:Information::\
             BookInfo:BookInfo:Num words known in book:0:,\
             CurrentPos:YJPosition: AR4GAAAAAAAA:57,EndPos:YJPosition: BBB:442;"
            .to_string();
        let events: BTreeSet<String> = [
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            page("260803:100500", "NextPage", 148_207, 120_000, 220),
            unmeasurable.clone(),
        ]
        .into_iter()
        .collect();

        archive_pushed(root.path(), "DEV", &events).expect("archive the push");

        // Read back the way any archive is read, since that is what it is.
        let found = collect_events(
            &[root.path().join("reading-log").join("DEV")],
            &Default::default(),
            &[],
            &mut super::job::ignore,
        );
        assert!(
            found.events.contains(&unmeasurable),
            "the line no session was made of is the one that had to survive"
        );
        assert_eq!(found.events.len(), events.len());

        // A second push of the same day merges rather than replacing.
        let later: BTreeSet<String> =
            [page("260803:101500", "NextPage", 148_207, 180_000, 300)].into();
        archive_pushed(root.path(), "DEV", &later).expect("archive the second push");
        let found = collect_events(
            &[root.path().join("reading-log").join("DEV")],
            &Default::default(),
            &[],
            &mut super::job::ignore,
        );
        assert_eq!(found.events.len(), events.len() + 1);
    }

    /// A page event, shaped like the device's own. `end` is the book's last
    /// position; the second `EndPos` is the chapter's, which must be ignored.
    fn page(stamp: &str, kind: &str, end: i64, total_ms: i64, words: i64) -> String {
        format!(
            "{stamp} cvm[6144]: I ReadingTimerController:Information::{kind},\
             Title:<private>,Asin:<private>,IntervalTime:900,\
             TotalTime:{total_ms},TotalWords:{words},Total%:0.5,\
             CurrentPos:YJPosition: AR4GAAAAAAAA:12,EndPos:YJPosition: BBB:{end},\
             NextTOCEntryPosition:YJPosition: CCC:99,\
             CurrentPos:YJPosition: AR4GAAAAAAAA:12,EndPos:YJPosition: DDD:6612;"
        )
    }

    /// The same fields, from a reader that lost the head of the payload: no
    /// event name at all, the line beginning partway down the field list.
    fn headless(stamp: &str, end: i64, total_ms: i64, words: i64) -> String {
        format!(
            "{stamp} java[8437]: I ReadingTimerController:Information::\
             IntervalTime:5153,IntervalWords:349,Interval%:0.002,\
             TotalTime:{total_ms},TotalWords:{words},Total%:0.5,\
             CurrentPos:YJPosition: AR4GAAAAAAAA:12,EndPos:YJPosition: BBB:{end},\
             CurrentPos:YJPosition: AR4GAAAAAAAA:12,EndPos:YJPosition: DDD:6612;"
        )
    }

    /// A close where that reader usually puts it: after the tail of the
    /// preceding payload, rather than at the head of its own.
    fn buried_close(stamp: &str, end: i64, total_ms: i64, words: i64) -> String {
        format!(
            "{stamp} java[8437]: I ReadingTimerController:Information::\
             DataSufficient:YES,NewTimeLeft:1440,OldTimeLeft:1521,\
             TimeLeftInSectionString:24 mins left in chapter;\
             CloseBook,Title:<private>,Asin:<private>,IntervalTime:900,\
             TotalTime:{total_ms},TotalWords:{words},Total%:0.5,\
             CurrentPos:YJPosition: AR4GAAAAAAAA:12,EndPos:YJPosition: BBB:{end},\
             CurrentPos:YJPosition: AR4GAAAAAAAA:12,EndPos:YJPosition: DDD:6612;"
        )
    }

    /// A marker line carrying neither a counter nor a position — a third of a
    /// real archive's lines are these. Verbatim in shape from a Colorsoft dump.
    fn nameless(stamp: &str) -> String {
        format!(
            "{stamp} cvm[4799]: I ReadingTimerController:Information::Reading_Resumed,Reason:8;"
        )
    }

    /// `OpenBook`, stating the counter the book resumes from. `stored` is the
    /// device's literal text, thousands separators and all.
    fn open_book(stamp: &str, stored: &str) -> String {
        format!(
            "{stamp} java[8437]: I ReadingTimerController:Information::OpenBook,\
             CurrentVersionUsed:0,StoredBookData:{stored},\
             Title:<private>,Asin:<private>;"
        )
    }

    /// The event that states both of a book's end-of-book constants.
    fn book_end_position(stamp: &str, from_book: i64, last_word: i64) -> String {
        format!(
            "{stamp} cvm[6144]: I ReadingTimerController:Information::\
             BookEndPosition.FromBook:YJPosition: AAA:{from_book},\
             BookEndPosition.LastWordPos.override:YJPosition: BBB:{last_word},\
             CurrentPos:YJPosition: AAA:524,EndPos:YJPosition: BBB:{last_word};"
        )
    }

    #[test]
    fn a_session_measures_the_counter_delta_not_the_wall_clock() {
        let lines = [
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            page("260803:100500", "NextPage", 148_207, 120_000, 220),
            page("260803:101000", "CloseBook", 148_207, 180_000, 300),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str), None);
        assert_eq!(out.len(), 1);
        // Wall clock spans 10 minutes; the counter only moved 2, and the counter
        // is what the device actually measured as reading.
        assert_eq!(out[0].seconds, 120);
        assert_eq!(out[0].end_position, 148_207);
        assert_eq!(out[0].words, 200);
        assert_eq!(out[0].page_turns, 2);
        assert_eq!(out[0].started_at, "2026-08-03T10:00:00");
    }

    /// A sitting interrupted by a Sync is one sitting, not two, and none of its
    /// reading falls into the seam.
    ///
    /// This is the ordinary case, not an edge one: the reader taps Sync in the
    /// middle of a book and carries on. The second batch of events holds only
    /// what was logged after the first, so parsed on its own it starts a fresh
    /// run at its first line — and the counter advance *between* the batches,
    /// which is real reading, belongs to neither run and is dropped.
    #[test]
    fn a_sitting_interrupted_by_a_sync_keeps_its_start_and_all_of_its_time() {
        let first = [
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            page("260803:100500", "NextPage", 148_207, 120_000, 220),
        ];
        let stored = parse_sessions(first.iter().map(String::as_str), None);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].seconds, 60);

        let resume = Resume {
            started_at: stored[0].started_at.clone(),
            ended_at: stored[0].ended_at.clone(),
            end_position: stored[0].end_position,
            start_counter_ms: stored[0].start_counter_ms,
            end_counter_ms: stored[0].end_counter_ms,
            start_words: stored[0].start_words,
            end_words: stored[0].end_words,
            page_turns: stored[0].page_turns,
            hours: stored[0].hours.clone(),
            seconds: stored[0].seconds,
            measure: stored[0].measure,
        };

        // What the device logs after the Sync, and only that.
        let rest = [
            page("260803:101000", "NextPage", 148_207, 300_000, 400),
            page("260803:101500", "CloseBook", 148_207, 360_000, 500),
        ];
        let out = parse_sessions(rest.iter().map(String::as_str), Some(&resume));
        assert_eq!(out.len(), 1, "one sitting, not one per Sync");
        assert_eq!(
            out[0].started_at, "2026-08-03T10:00:00",
            "the row this updates is the one already stored"
        );
        // 60 s → 360 s on the device's counter. Parsed without the resume the
        // second batch would report 60 s, and the 180 s the reader spent across
        // the Sync would be in no session at all.
        assert_eq!(out[0].seconds, 300);
        assert_eq!(out[0].page_turns, 3);
        assert_eq!(out[0].words, 400);
        assert_eq!(out[0].hours.iter().map(|(_, s)| s).sum::<i64>(), 300);

        let alone = parse_sessions(rest.iter().map(String::as_str), None);
        assert_eq!(alone[0].seconds, 60, "the seam, measured");
    }

    /// A run offered to a batch that does not continue it says nothing. The
    /// stored row is already right, and re-reporting it would have every Sync
    /// claim a sitting it did nothing to.
    #[test]
    fn a_sitting_the_next_events_do_not_continue_is_left_alone() {
        let resume = Resume {
            started_at: "2026-08-03T10:00:00".into(),
            ended_at: "2026-08-03T10:05:00".into(),
            end_position: 148_207,
            start_counter_ms: Some(60_000),
            end_counter_ms: Some(120_000),
            start_words: Some(100),
            end_words: Some(220),
            page_turns: 1,
            hours: vec![(10, 60)],
            seconds: 60,
            measure: Default::default(),
        };

        // Nothing at all since.
        assert!(parse_sessions(std::iter::empty(), Some(&resume)).is_empty());

        // The reader came back hours later: a new sitting, and the old one
        // untouched.
        let later = [
            page("260803:160000", "NextPage", 148_207, 180_000, 300),
            page("260803:160500", "CloseBook", 148_207, 240_000, 400),
        ];
        let out = parse_sessions(later.iter().map(String::as_str), Some(&resume));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].started_at, "2026-08-03T16:00:00");
        assert_eq!(out[0].seconds, 60);

        // A different book, straight away: likewise its own sitting.
        let other = [
            page("260803:100600", "NextPage", 99_000, 5_000, 10),
            page("260803:101000", "CloseBook", 99_000, 65_000, 90),
        ];
        let out = parse_sessions(other.iter().map(String::as_str), Some(&resume));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].end_position, 99_000);
        assert_eq!(out[0].seconds, 60);
    }

    /// Events already folded into a run can arrive again — the dumps overlap by
    /// design — and must not be counted twice.
    ///
    /// They cannot be: every figure is read from the device's own absolute
    /// counters, so a line already seen restates a value the run already holds.
    #[test]
    fn replayed_events_do_not_lengthen_the_run_they_are_already_in() {
        let all = [
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            page("260803:100500", "NextPage", 148_207, 120_000, 220),
        ];
        let stored = parse_sessions(all.iter().map(String::as_str), None);
        let resume = Resume {
            started_at: stored[0].started_at.clone(),
            ended_at: stored[0].ended_at.clone(),
            end_position: stored[0].end_position,
            start_counter_ms: stored[0].start_counter_ms,
            end_counter_ms: stored[0].end_counter_ms,
            start_words: stored[0].start_words,
            end_words: stored[0].end_words,
            page_turns: stored[0].page_turns,
            hours: stored[0].hours.clone(),
            seconds: stored[0].seconds,
            measure: stored[0].measure,
        };
        // The last line of the run, sent a second time.
        let again = [all[1].clone()];
        let out = parse_sessions(again.iter().map(String::as_str), Some(&resume));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seconds, stored[0].seconds);
        assert_eq!(out[0].words, stored[0].words);

        // A whole archive replayed against the run it already contains is not a
        // continuation of it at all: the run is dropped and rebuilt from the
        // events themselves.
        let whole = parse_sessions(all.iter().map(String::as_str), Some(&resume));
        assert_eq!(whole, stored);
    }

    #[test]
    fn the_first_endpos_is_the_book_not_the_chapter() {
        // The chapter's EndPos (6612) trails the book's on every page line; a
        // last-match scan would file every book under one fingerprint.
        let line = page("260803:100000", "NextPage", 148_207, 1000, 1);
        assert_eq!(book_position(&line), Some(148_207));
    }

    #[test]
    fn a_chapter_left_leading_a_cut_payload_is_not_taken_for_the_book() {
        // The head of this payload is gone, taking the book's EndPos with it and
        // leaving the chapter's first. Reading it as the book's would key the
        // session to a position no book has.
        let cut = "260803:100000 java[1]: I ReadingTimerController:Information::\
                   PosLeft:919720,NextTOCEntryPosition:YJPosition: AAA:17008,\
                   NextTOCEntryLength:43,CurrentPos:YJPosition: BBB:16969,\
                   EndPos:YJPosition: AAA:17008,PosLeft:39;";
        assert_eq!(book_position(cut), None);

        // The cut can also land inside the TOC group, leaving a later field of
        // it as the only sign that what follows is a chapter.
        let mid_group = "260803:100000 java[1]: I ReadingTimerController:Information::\
                         NextTOCEntryLevel:0,NextTOCEntryType:null,\
                         CurrentPos:YJPosition: BBB:78,EndPos:YJPosition: AAA:189;";
        assert_eq!(book_position(mid_group), None);

        // The close that follows on the same line does state it, and that is the
        // one the session must use.
        let both = format!(
            "{cut}CloseBook,Title:<private>,TotalTime:6491,\
                            CurrentPos:YJPosition: BBB:16969,\
                            EndPos:YJPosition: CCC:936689,PosLeft:919720,\
                            NextTOCEntryPosition:YJPosition: AAA:17008,\
                            CurrentPos:YJPosition: BBB:16969,\
                            EndPos:YJPosition: AAA:17008;"
        );
        assert_eq!(book_position(&both), Some(936_689));
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
        let out = parse_sessions(doubled.iter().map(String::as_str), None);
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
        let out = parse_sessions(lines.iter().map(String::as_str), None);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].seconds, out[1].seconds), (60, 60));
    }

    #[test]
    fn a_line_inside_the_gap_does_not_bridge_it() {
        // A `nameless` line inside the gap carries no counter and no position.
        // It ends no session and bridges none.
        let lines = [
            page("260803:080000", "NextPage", 148_207, 60_000, 100),
            page("260803:080100", "NextPage", 148_207, 120_000, 200),
            nameless("260803:225900"),
            page("260803:230000", "NextPage", 148_207, 300_000, 400),
            page("260803:230100", "NextPage", 148_207, 360_000, 500),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str), None);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].seconds, out[1].seconds), (60, 60));
    }

    #[test]
    fn a_session_does_not_run_past_midnight() {
        // A session is grouped by the day it began. The day change is noticed
        // on whichever line crosses it — here one that observes nothing.
        let lines = [
            page("260803:234500", "NextPage", 148_207, 60_000, 100),
            nameless("260804:000100"),
            page("260804:000200", "NextPage", 148_207, 120_000, 200),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str), None);
        assert_eq!(out.len(), 2);
        assert_eq!(&out[0].started_at[..10], "2026-08-03");
        assert_eq!(&out[1].started_at[..10], "2026-08-04");
        // Cut on the device's own local clock, the one the syslog prefix
        // carries: `260804:000200` is two minutes past the reader's midnight at
        // any offset to UTC.
        assert_eq!(out[1].started_at, "2026-08-04T00:00:00");
    }

    #[test]
    fn the_interval_that_spans_midnight_is_divided_between_the_two_days() {
        // Reading straight through midnight: events five minutes apart, then a
        // ten-minute interval that straddles the boundary with half its wall
        // clock either side.
        let lines = [
            page("260803:235000", "NextPage", 148_207, 3_000_000, 1000),
            page("260803:235500", "NextPage", 148_207, 3_300_000, 1200),
            page("260804:000500", "NextPage", 148_207, 3_900_000, 1600),
            page("260804:001000", "NextPage", 148_207, 4_200_000, 1800),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str), None);
        assert_eq!(out.len(), 2);

        // Half the crossing interval belongs to each day, so each gets its own
        // five minutes plus five of the ten that straddled the boundary.
        assert_eq!((out[0].seconds, out[1].seconds), (600, 600));
        assert_eq!((out[0].words, out[1].words), (400, 400));
        // And nothing falls between them: the counter advanced 1200 s in all.
        assert_eq!(out[0].seconds + out[1].seconds, 1200);

        // The rows meet at the boundary rather than at the events either side
        // of it — the reader was reading at 23:59 and at 00:01, and no minute
        // between the two is stranded outside both.
        assert_eq!(out[0].started_at, "2026-08-03T23:50:00");
        assert_eq!(out[0].ended_at, "2026-08-03T23:59:59");
        assert_eq!(out[1].started_at, "2026-08-04T00:00:00");
        assert_eq!(out[1].ended_at, "2026-08-04T00:10:00");
        // The clock agrees with the calendar: the boundary divides the hours as
        // it divides the days, with nothing booked to an hour on the far side.
        assert_eq!(out[0].hours, vec![(23, 600)]);
        assert_eq!(out[1].hours, vec![(0, 600)]);
    }

    #[test]
    fn a_sitting_books_its_reading_to_the_hours_it_was_actually_read_in() {
        let lines = [
            page("260803:200000", "NextPage", 148_207, 0, 0),
            page("260803:201500", "NextPage", 148_207, 900_000, 200),
            page("260803:203000", "NextPage", 148_207, 1_800_000, 400),
            // The reader stops here. The page still turns now and then, but the
            // device credits five seconds for each of the next two stretches —
            // it counts reading, and there is almost none.
            page("260803:205500", "NextPage", 148_207, 1_805_000, 405),
            page("260803:212000", "NextPage", 148_207, 1_810_000, 410),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str), None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seconds, 1810);
        // Half an hour of reading in the 20:00 hour and four seconds in the
        // 21:00 hour. Spread evenly across the 80-minute window — all a stored
        // row supports — 21:00 reports 452 s.
        assert_eq!(out[0].hours, vec![(20, 1806), (21, 4)]);
        assert_eq!(
            out[0].hours.iter().map(|(_, s)| s).sum::<i64>(),
            out[0].seconds,
            "the hours of a session must add up to the session",
        );
    }

    #[test]
    fn a_night_asleep_over_midnight_is_still_a_break_and_bridges_nothing() {
        // The same day change, across an eight-hour absence. The break is taken
        // where it happened and the interval across it belongs to neither day.
        let lines = [
            page("260803:225000", "NextPage", 148_207, 3_000_000, 1000),
            page("260803:225500", "NextPage", 148_207, 3_300_000, 1200),
            page("260804:070000", "NextPage", 148_207, 3_900_000, 1600),
            page("260804:071000", "NextPage", 148_207, 4_200_000, 1800),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str), None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].ended_at, "2026-08-03T22:55:00");
        assert_eq!(out[1].started_at, "2026-08-04T07:00:00");
        assert_eq!((out[0].seconds, out[1].seconds), (300, 300));
    }

    #[test]
    fn an_absence_across_midnight_is_measured_in_real_elapsed_time() {
        // 50 minutes, of which the clock reads 10 before midnight and 40 after.
        // Seconds-into-the-day run backwards there, so measuring the gap on them
        // reads this as no time at all and bridges a break that happened.
        let lines = [
            page("260803:235000", "NextPage", 148_207, 3_000_000, 1000),
            page("260804:004000", "NextPage", 148_207, 3_300_000, 1200),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str), None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].ended_at, "2026-08-03T23:50:00");
        assert_eq!(out[1].started_at, "2026-08-04T00:40:00");
    }

    #[test]
    fn switching_books_closes_the_previous_session() {
        let lines = [
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            page("260803:100100", "NextPage", 148_207, 120_000, 200),
            page("260803:100200", "NextPage", 764_576, 5_000, 10),
            page("260803:100300", "NextPage", 764_576, 65_000, 90),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str), None);
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
        // A cut payload can begin on any field, so the first one has the
        // `Information::` prefix in front of it rather than a comma.
        let cut = "260803:100000 java[1]: I ReadingTimerController:\
                   Information::TotalTime:900,Total%:0.5;";
        assert_eq!(field(cut, "TotalTime"), Some(900));
    }

    #[test]
    fn a_position_handle_carries_the_element_id_the_sidecars_use() {
        // Verbatim from a device: the handle beside coordinate 39799, whose
        // book's sidecar recorded eid 1566 offset 0 at that same coordinate.
        assert_eq!(decode_handle("AR4GAAAAAAAA"), Some((1566, 0)));
        // A non-zero offset, to pin the field order — the offset is the second
        // little-endian u32, not part of the element id.
        assert_eq!(decode_handle("AQ3PAQAhAAAA"), Some((118_541, 33)));
        // Too short to hold both fields, rather than silently reading zeros.
        assert_eq!(decode_handle("AQA="), None);
    }

    #[test]
    fn every_line_that_states_where_the_reader_stood_gives_up_its_point() {
        let stood = Location {
            eid: 1566,
            offset: 0,
            linear_pos: 12,
        };
        let lines = [
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            page("260803:100500", "CloseBook", 148_207, 120_000, 220),
        ];
        // `page` writes the same handle on both lines, so the two collapse: the
        // points are the distinct places visited, not one per event.
        assert_eq!(
            positions_seen(lines.iter().map(String::as_str)),
            vec![(148_207, stood)],
        );

        // Gathered from the lines, not the sittings made of them.
        // `BookEndPosition` is no observation and a book opened and shut gets no
        // row; both state a point, once each.
        let opened_and_shut = ["260803:095956 cvm[6144]: I ReadingTimerController:\
             Information::BookEndPosition.FromBook:YJPosition: AR4GAAAAAAAA:148213,\
             BookEndPosition.LastWordPos.override:YJPosition: AR4GAAAAAAAA:148207,\
             CurrentPos:YJPosition: AR4GAAAAAAAA:524,\
             EndPos:YJPosition: AR4GAAAAAAAA:148207;"];
        // A run forms on the line stating where the reader stood. One
        // observation spans nothing, and [`store_events`] keeps no row.
        assert!(
            parse_sessions(opened_and_shut.iter().copied(), None)
                .iter()
                .all(|s| s.seconds == 0)
        );
        assert_eq!(
            positions_seen(opened_and_shut.iter().copied()),
            vec![(
                148_207,
                Location {
                    eid: 1566,
                    offset: 0,
                    linear_pos: 524
                }
            )],
        );
    }

    /// Two Syncs in one sitting leave one row, holding all of it.
    ///
    /// The whole round trip: the first push stores a partial sitting, the second
    /// reads it back as the run under way and measures the same sitting further.
    /// A device never sends an event twice, so the second push carries only what
    /// was logged after the first — and without the stored row to continue, the
    /// sitting would be two rows and the reading between the pushes would be in
    /// neither.
    #[test]
    fn a_second_sync_lengthens_the_sitting_instead_of_starting_another() {
        let dir = std::env::temp_dir().join(format!("sidle-rl-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::open(&dir.join("library.db")).unwrap();

        let push = |lines: &[String], conn: &Connection| {
            let events: BTreeSet<String> = lines.iter().cloned().collect();
            let resume = Resume::newest(conn, "DEV").unwrap();
            store_events(conn, &events, 0, "DEV", resume.as_ref(), &mut job::ignore).unwrap()
        };

        let first = push(
            &[
                page("260803:100000", "NextPage", 148_207, 60_000, 100),
                page("260803:100500", "NextPage", 148_207, 120_000, 220),
            ],
            &conn,
        );
        assert_eq!((first.added, first.extended), (1, 0));

        let second = push(
            &[
                page("260803:101000", "NextPage", 148_207, 300_000, 400),
                page("260803:101500", "CloseBook", 148_207, 360_000, 500),
            ],
            &conn,
        );
        assert_eq!(
            (second.added, second.extended),
            (0, 1),
            "the same sitting, carried further — not a second one"
        );

        let rows: Vec<(String, String, i64, i64)> = conn
            .prepare(
                "SELECT started_at, ended_at, seconds, page_turns
                   FROM reading_sessions ORDER BY started_at",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![(
                "2026-08-03T10:00:00".to_string(),
                "2026-08-03T10:15:00".to_string(),
                300,
                3
            )]
        );

        // The hours are the sitting's own, replaced whole rather than added to a
        // stale slice of themselves.
        let hours: i64 = conn
            .query_row("SELECT SUM(seconds) FROM reading_session_hours", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(hours, 300);

        // A third Sync with nothing new says nothing and changes nothing.
        let idle = push(&[], &conn);
        assert_eq!((idle.added, idle.extended, idle.sessions), (0, 0, 0));
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM reading_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sitting whose row has since been re-keyed is still the same sitting.
    ///
    /// A log line names the book by its last-word position;
    /// [`db::resolve_reading_sessions`] moves the row to the book's last position
    /// the moment an event states the pairing. The device knows nothing of that
    /// and keeps sending the same fingerprint, so the run has to be offered back
    /// under the name the events use — otherwise the next Sync reads the
    /// continuation as a switch to a different book and files it separately.
    #[test]
    fn a_re_keyed_sitting_is_still_continued_by_the_events_that_name_it() {
        let dir = std::env::temp_dir().join(format!("sidle-rl-rekey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::open(&dir.join("library.db")).unwrap();

        let push = |lines: &[String], conn: &Connection| {
            let events: BTreeSet<String> = lines.iter().cloned().collect();
            let resume = Resume::newest(conn, "DEV").unwrap();
            store_events(conn, &events, 0, "DEV", resume.as_ref(), &mut job::ignore).unwrap()
        };

        // The first push both starts the sitting and states the pairing, so the
        // row lands under the book's last position (148213) while every page
        // event goes on naming the last-word one (148207).
        let first = push(
            &[
                book_end_position("260803:095900", 148_213, 148_207),
                page("260803:100000", "NextPage", 148_207, 60_000, 100),
                page("260803:100500", "NextPage", 148_207, 120_000, 220),
            ],
            &conn,
        );
        assert_eq!((first.added, first.extended), (1, 0));
        let key: i64 = conn
            .query_row("SELECT end_position FROM reading_sessions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(key, 148_213, "stored under the key the library joins on");

        let second = push(
            &[page("260803:101000", "CloseBook", 148_207, 300_000, 400)],
            &conn,
        );
        assert_eq!(
            (second.added, second.extended),
            (0, 1),
            "the same sitting under a name the device never sees"
        );
        let rows: Vec<(i64, i64)> = conn
            .prepare("SELECT end_position, seconds FROM reading_sessions")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows, vec![(148_213, 240)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sitting judged not worth a row must not take its evidence with it.
    ///
    /// The regression this guards is a one-line `continue`: skipping a session
    /// with no measurable duration used to skip recording where the reader stood
    /// in it, and a book opened and shut is precisely when the device states
    /// that. The lines are offered once, so the loss was permanent and silent —
    /// it showed up only as a session that could never be named.
    #[test]
    fn a_book_opened_and_shut_still_gives_up_where_the_reader_stood() {
        let dir = std::env::temp_dir().join(format!("sidle-rl-points-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::open(&dir.join("library.db")).unwrap();

        let events: BTreeSet<String> = ["260803:095956 cvm[6144]: I ReadingTimerController:\
             Information::BookEndPosition.FromBook:YJPosition: AR4GAAAAAAAA:148213,\
             BookEndPosition.LastWordPos.override:YJPosition: AR4GAAAAAAAA:148207,\
             CurrentPos:YJPosition: AR4GAAAAAAAA:524,\
             EndPos:YJPosition: AR4GAAAAAAAA:148207;"
            .to_string()]
        .into_iter()
        .collect();
        let out = store_events(&conn, &events, 1, "DEV", None, &mut job::ignore).unwrap();
        assert_eq!(out.added, 0, "opening and shutting a book is not reading");

        // Filed under the book's *last* position, the one the library joins on,
        // because the same line stated the pairing.
        let point: (i64, i64, i64) = conn
            .query_row(
                r#"SELECT end_position, eid, linear_pos FROM reading_log_points"#,
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("the point survives the session that was not stored");
        assert_eq!(point, (148_213, 1566, 524));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A latency record naming `cde_key`, in the shape the reader shell writes.
    fn named(stamp: &str, key: &str) -> String {
        format!(
            r#"{stamp} fastmetrics[10393]: D fastmetrics:KindleFastMetricsPublisher:[1.0]: Emitting a new record. SchemaName[ereader_reader_page_turn_latency_ops], Fields[{{ 	"action" : "PageTurnTotalTime", 	"book_category" : "MAGZ", 	"cde_key" : "{key}", 	"latency" : 132 }} ]. :"#
        )
    }

    /// A catalog key names a book the position axis cannot.
    ///
    /// The library holds this title at `max_position` 999999, so no session
    /// ending at 148213 joins it — a different build of the same book, which is
    /// what the axis is right to refuse. The key settles it outright.
    #[test]
    fn a_catalog_key_names_a_book_whose_axis_does_not_match() {
        let dir = std::env::temp_dir().join(format!("sidle-rl-asin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::open(&dir.join("library.db")).unwrap();
        conn.execute(
            "INSERT INTO books (sha256, title, author, language, file_size, imported_at,
                                amazon_asin, max_position)
             VALUES ('sha', 'A Magazine', 'Author', 'en', 0, 't0', 'B00QPFC59S', 999999)",
            [],
        )
        .unwrap();

        let events: BTreeSet<String> = [
            book_end_position("260803:095900", 148_213, 148_207),
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            named("260803:100001", "B00QPFC59S"),
            page("260803:100500", "NextPage", 148_207, 120_000, 220),
        ]
        .into_iter()
        .collect();
        let out = store_events(&conn, &events, 1, "DEV", None, &mut job::ignore).unwrap();

        assert_eq!(out.added, 1);
        assert_eq!(out.attributed, 1, "the key named it; the axis could not");
        let (position, book): (i64, Option<i64>) = conn
            .query_row(
                "SELECT end_position, book_id FROM reading_sessions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(position, 148_213);
        assert!(book.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A catalog row names a book on a firmware that logs no key.
    ///
    /// The extent and key are a real pair off a KOA2's own `/var/local/cc.db`:
    /// `p_contentSize` 416436 against `p_cdeKey`
    /// `L7P5OOJTFVDRFUJ2OFMKCAP7JYEACNDZ`, the content id Sidle baked into that
    /// book. The library here ends the title at a different position, which is
    /// the case the axis is right to refuse.
    #[test]
    fn a_catalog_row_names_a_book_the_reading_lines_never_do() {
        let dir = std::env::temp_dir().join(format!("sidle-rl-cat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::open(&dir.join("library.db")).unwrap();
        conn.execute(
            "INSERT INTO books (sha256, title, author, language, file_size, imported_at,
                                asin, max_position)
             VALUES ('sha', 'The Hangman''s Handyman', 'Author', 'en', 0, 't0',
                     'L7P5OOJTFVDRFUJ2OFMKCAP7JYEACNDZ', 999999)",
            [],
        )
        .unwrap();

        let events: BTreeSet<String> = [
            book_end_position("260803:095900", 416_436, 416_430),
            page("260803:100000", "NextPage", 416_430, 60_000, 100),
            page("260803:100500", "NextPage", 416_430, 120_000, 220),
            "260803:100600 sidle-native: I SidleCatalog:extent=416436,\
             cde_key=L7P5OOJTFVDRFUJ2OFMKCAP7JYEACNDZ,cde_type=PDOC;"
                .to_string(),
        ]
        .into_iter()
        .collect();
        let out = store_events(&conn, &events, 1, "DEV", None, &mut job::ignore).unwrap();

        assert_eq!(out.added, 1);
        assert_eq!(
            out.attributed, 1,
            "the catalog named it; the axis could not"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A catalog row for a book this batch holds no reading for is still kept:
    /// the sitting it answers may already be stored.
    #[test]
    fn a_catalog_row_is_kept_without_a_sitting_beside_it() {
        let dir = std::env::temp_dir().join(format!("sidle-rl-catonly-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::open(&dir.join("library.db")).unwrap();

        let events: BTreeSet<String> =
            ["260803:100600 sidle-native: I SidleCatalog:extent=416436,\
             cde_key=L7P5OOJTFVDRFUJ2OFMKCAP7JYEACNDZ,cde_type=PDOC;"
                .to_string()]
            .into_iter()
            .collect();
        store_events(&conn, &events, 1, "DEV", None, &mut job::ignore).unwrap();

        let (pos, key): (i64, String) = conn
            .query_row(
                "SELECT end_position, asin FROM reading_log_asins",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(pos, 416_436);
        assert_eq!(key, "L7P5OOJTFVDRFUJ2OFMKCAP7JYEACNDZ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A key the library holds no book for names nothing, and the axis is left
    /// to answer.
    #[test]
    fn a_catalog_key_for_a_book_the_library_lacks_settles_nothing() {
        let dir = std::env::temp_dir().join(format!("sidle-rl-asin-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::open(&dir.join("library.db")).unwrap();

        let events: BTreeSet<String> = [
            book_end_position("260803:095900", 148_213, 148_207),
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            named("260803:100001", "B0NOTHELD0"),
            page("260803:100500", "NextPage", 148_207, 120_000, 220),
        ]
        .into_iter()
        .collect();
        let out = store_events(&conn, &events, 1, "DEV", None, &mut job::ignore).unwrap();

        assert_eq!(out.added, 1);
        assert_eq!(out.attributed, 0);
        // Kept as evidence: the book may arrive next month.
        let held: String = conn
            .query_row("SELECT asin FROM reading_log_asins", [], |r| r.get(0))
            .unwrap();
        assert_eq!(held, "B0NOTHELD0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_close_that_is_not_first_on_its_line_still_ends_the_session() {
        let lines = [
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            buried_close("260803:100500", 148_207, 120_000, 220),
            page("260803:100600", "NextPage", 148_207, 130_000, 240),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str), None);
        // Same book with no gap, so only the close can be splitting these.
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].seconds, out[1].seconds), (60, 0));
    }

    #[test]
    fn a_payload_that_lost_its_name_is_still_an_observation() {
        let lines = [
            headless("260803:100000", 936_689, 10_000, 50),
            headless("260803:100500", 936_689, 70_000, 150),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str), None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seconds, 60);
        assert_eq!(out[0].words, 100);
        assert_eq!(out[0].end_position, 936_689);
        // Nothing named a page turn, so none is claimed. Counting an
        // observation as a turn would inflate the count on the reader that
        // names them.
        assert_eq!(out[0].page_turns, 0);
    }

    #[test]
    fn a_session_starts_at_the_counter_the_book_was_opened_at() {
        let lines = [
            open_book("260803:100000", "TimeRead:1,000 sec. WPM:430.27. Version:0"),
            // First event logged three minutes in, by which time the counter
            // has already moved with the reader.
            page("260803:100300", "NextPage", 148_207, 1_180_000, 100),
            page("260803:100400", "CloseBook", 148_207, 1_240_000, 200),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str), None);
        assert_eq!(out.len(), 1);
        // 1240 - 1000, not 1240 - 1180: those three minutes were read too.
        assert_eq!(out[0].seconds, 240);
        // And they belong inside the session, not before it.
        assert_eq!(out[0].started_at, "2026-08-03T10:00:00");
    }

    #[test]
    fn an_open_is_not_credited_to_a_book_it_cannot_belong_to() {
        let lines = [
            open_book("260803:100000", "null"),
            // Ten seconds later, a book already an hour into its counter. It
            // cannot be the book that just opened from zero.
            page("260803:100010", "NextPage", 148_207, 3_600_000, 100),
            page("260803:100110", "CloseBook", 148_207, 3_660_000, 200),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str), None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seconds, 60);
    }

    #[test]
    fn a_book_that_states_no_end_position_does_not_borrow_one() {
        let lines = [
            book_end_position("260803:095956", 148_213, 148_207),
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            // A second book opens and never states its own.
            open_book("260803:101000", "null"),
            page("260803:101100", "NextPage", 764_576, 5_000, 10),
        ];
        let map = frombook_map(lines.iter().map(String::as_str));
        assert_eq!(map.get(&148_207), Some(&148_213));
        // Unnamed, rather than named as the book before it.
        assert_eq!(map.get(&764_576), None);
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

    /// A truncated dump gives up its prefix but is **not** recorded as read.
    ///
    /// The two rules are load-bearing together. Snapshot names are immutable, so
    /// recording a half-decoded file as read would skip it unopened forever —
    /// including the complete copy pulled later under the same name. A real
    /// 31-file archive had 5 truncated, so this is the common case, not a corner.
    #[test]
    fn a_truncated_dump_gives_up_its_prefix_without_counting_as_read() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("sidle-rl-cut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let whole = format!(
            "{}\n{}\n",
            page("260803:100000", "NextPage", 1, 60_000, 10),
            page("260803:100500", "NextPage", 1, 120_000, 20),
        );
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(whole.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();

        // Cut the archive short, exactly as an interrupted transfer or a dump
        // read mid-write leaves it.
        let name = "log_backup_260803101500.txt.gz";
        std::fs::write(dir.join(name), &gz[..gz.len() - 12]).unwrap();

        let cut = collect_events(&[&dir], &Default::default(), &[], &mut job::ignore);
        assert_eq!(cut.files, 1, "it was opened and decoded as far as it went");
        assert_eq!(
            cut.truncated, 1,
            "and the shortfall is reported, not hidden"
        );
        assert!(
            cut.read.is_empty(),
            "a prefix is not a read file: recording it would make the skip permanent"
        );

        // The other way a transfer fails: nothing arrives at all. That is not an
        // empty log — it must not be counted, and above all not marked read.
        std::fs::write(dir.join(name), b"").unwrap();
        let nothing = collect_events(&[&dir], &Default::default(), &[], &mut job::ignore);
        assert_eq!((nothing.files, nothing.truncated), (0, 0));
        assert!(nothing.read.is_empty());

        // The complete file, later. Same name — so had the truncated pass
        // recorded it, this would find nothing at all.
        std::fs::write(dir.join(name), &gz).unwrap();
        let full = collect_events(&[&dir], &Default::default(), &[], &mut job::ignore);
        assert_eq!(full.truncated, 0);
        assert_eq!(full.read, vec![name.to_string()]);
        assert_eq!(
            full.events.len(),
            2,
            "both events, now that the tail is here"
        );
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
