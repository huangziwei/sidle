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
//! counters and no absolute stamp. Other lines in the same log embed *UTC*
//! stamps, and the two differ by exactly the device's offset (measured +02:00 to
//! the second on two devices, 2026-08-11): local is the one this wants, since
//! reading at 23:00 means 23:00 where the reader was, not 21:00 in Greenwich.
//! The offset itself is recorded nowhere, so a device carried across timezones
//! stamps each day in whatever local time it was on and nothing afterwards can
//! reconcile them.
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

/// The tag every event line carries; the cheap prefilter before any parsing.
const MARKER: &str = "ReadingTimerController";

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
    /// The counter the run began at and the last value seen for it. Both are
    /// required: without them the run has no measurable length to continue.
    pub start_counter_ms: i64,
    pub end_counter_ms: i64,
    pub start_words: Option<i64>,
    pub end_words: Option<i64>,
    pub page_turns: i64,
    /// The hours already booked against the run, so a continued session rebuilds
    /// the whole distribution rather than an incremental slice of one.
    pub hours: Vec<(u8, i64)>,
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
        // A row stored before sessions carried their counters cannot be
        // continued — its length is known but not where on the counter it sits,
        // so a later event states no measurable advance from it.
        let (Some(start_counter_ms), Some(end_counter_ms)) =
            (row.start_counter_ms, row.end_counter_ms)
        else {
            return Ok(None);
        };
        let hours = db::session_hours(conn, &row)?;
        Ok(Some(Self {
            started_at: row.started_at,
            ended_at: row.ended_at,
            end_position: row.end_position,
            start_counter_ms,
            end_counter_ms,
            start_words: row.start_words,
            end_words: row.end_words,
            page_turns: row.page_turns,
            hours,
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
            // Only a file decoded to its end is recorded as read. A truncated
            // one still contributes its prefix, but claiming it was read would
            // make the skip permanent: the names are immutable, so a complete
            // copy pulled later carries the same name and would be skipped
            // unopened, losing the tail for good.
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
            for line in file.text.lines().filter(|l| l.contains(MARKER)) {
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
    // An empty file is a backup that produced nothing, not a log with nothing in
    // it. Two of a real archive's 31 files were 0 bytes, both on dates where the
    // device's own backup script started and never finished. Reading it as valid
    // empty text would count it as a file and record its name as read.
    if bytes.is_empty() {
        return None;
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        // A truncated member yields Err *after* writing what it decoded, so the
        // buffer is kept either way; only a header-level failure yields nothing.
        // The error is not discarded: it is the sole evidence that what came out
        // is a prefix, and reading a prefix must not count as having read the
        // file.
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

/// `YYYY-MM-DDTHH:MM:SS` as the parser's own instant, for a stored session being
/// read back — the round trip of what [`stamp`] produced when it was written.
fn moment(iso: &str) -> Option<Moment> {
    stamp(&log_stamp(iso)?)
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
    let mut out = Vec::new();
    let mut open: Option<Open> = Open::resume(resume);
    let mut prev_abs: Option<i64> = open.as_ref().map(|cur| cur.last.abs);
    let mut opened: Option<Opened> = None;
    // A break, once noticed, waits here until an observation can act on it.
    let mut gapped = false;
    let mut first = true;

    for line in events {
        let Some(now) = stamp(line) else {
            continue;
        };
        if std::mem::take(&mut first) && open.as_ref().is_some_and(|cur| now.abs < cur.last.abs) {
            // Events from before the run they were offered against — a whole
            // archive replayed host-side, say, which already includes the run.
            // Folding them into it would count them a second time, so the run is
            // dropped rather than continued; it stays stored exactly as it is.
            open = None;
            prev_abs = None;
        }
        // The gap is measured against the previous *event of any kind*, so a
        // stretch of non-page activity still breaks a session.
        //
        // It is *held* rather than read on the spot, because the line that
        // notices a break is usually not one that can end a session: a third of
        // the marker lines carry no counter and no position — `OpenBook`,
        // `Reading_Resumed`, `TapOnFooter` — and the reader coming back after a
        // night's sleep produces exactly such a line first. Read locally, that
        // line consumed the break and re-anchored the clock, so the observation
        // behind it saw no gap at all and the two sittings merged. Measured on
        // one device's archive: 6 breaks taken, 116 swallowed, sessions running
        // to 48 h of wall clock and booking a whole night's reading to the day
        // before it.
        gapped |= prev_abs.is_some_and(|prev| now.abs - prev > SESSION_GAP_SECS);
        prev_abs = Some(now.abs);

        if let Some(counter_ms) = opened_at_counter(line) {
            opened = Some(Opened {
                counter_ms,
                at: now.clone(),
            });
        }

        let Some(obs) = observation(line) else {
            continue;
        };
        // Consumed here and only here: whatever happened between two
        // observations is decided at the second of them.
        let gapped = std::mem::take(&mut gapped);

        // Consumed at the first observation after the open, whether or not it is
        // used: an open belongs to the session it precedes and to no later one.
        let mut seed = opened.take().and_then(|o| o.vouch(&now, obs.total_ms));

        match open
            .as_ref()
            .and_then(|cur| cur.broken_by(&now, &obs, gapped))
        {
            None => {}
            Some(Break::Left) => out.extend(open.take().and_then(Open::emit)),
            Some(Break::Midnight(boundary)) => {
                out.extend(open.take().map(|cur| cur.finish_at(&boundary)));
                // The run was already under way, so nothing that happened after
                // midnight can be its start: the boundary is, and an `OpenBook`
                // seen in between describes a book that was already open.
                seed = Some(boundary);
            }
        }
        let cur = open.get_or_insert_with(|| Open::new(obs.position, &now, seed));
        cur.observe(&now, &obs);
        if obs.closes {
            out.extend(open.take().and_then(Open::emit));
        }
    }
    out.extend(open.and_then(Open::emit));
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
struct Start {
    counter_ms: i64,
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
                counter_ms: self.counter_ms,
                words: None,
                at: self.at,
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
            Some(s) => (Some(s.counter_ms), s.words, s.at),
            None => (None, None, now.clone()),
        };
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
            time_lo: Some(r.start_counter_ms),
            time_hi: r.end_counter_ms,
            words_lo,
            words_hi,
            page_turns: r.page_turns,
            last,
            last_time: Some(r.end_counter_ms),
            last_words: r.end_words,
            hours_ms,
            touched: false,
        })
    }

    /// The run as a session, unless it is a resumed one this batch never
    /// continued — that is the stored row unchanged, and re-offering it would
    /// have every sync report a sitting it did nothing to.
    fn emit(self) -> Option<Session> {
        self.touched.then(|| self.finish())
    }

    /// Book an interval's counted reading against the clock hours it ran
    /// through, `from` and `to` being seconds into the day.
    ///
    /// Evenly across the interval, which is the whole of what the log says: the
    /// device credits an interval in full at its far end and states nothing
    /// about where inside it the reading fell. An interval is a page turn wide,
    /// so that assumption spans minutes — where spreading a *session* spans
    /// hours, and is the guess this exists to avoid.
    fn credit(&mut self, from: i64, to: i64, advance_ms: i64) {
        if advance_ms <= 0 {
            return;
        }
        let hour = |secs: i64| ((secs / 3600) as usize).min(23);
        let span = to - from;
        if span <= 0 {
            self.hours_ms[hour(from)] += advance_ms;
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
            self.hours_ms[hour(h * 3600)] += share;
        }
        // The division's remainder to the hour the interval began in, so the
        // hours of a session add back up to the session.
        self.hours_ms[hour(from)] += advance_ms - placed;
    }

    fn observe(&mut self, now: &Moment, obs: &Observation) {
        self.touched = true;
        if let (Some(from), Some(to)) = (self.last_time, obs.total_ms) {
            self.credit(self.last.secs, now.secs, to - from);
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

    /// Whether this observation ends the run, and how. See [`Break`].
    fn broken_by(&self, now: &Moment, obs: &Observation, gapped: bool) -> Option<Break> {
        if self.end_position != obs.position || gapped {
            return Some(Break::Left);
        }
        if self.last.day == now.day {
            return None;
        }
        // Mid-run, over midnight. Both counters are read at the boundary by
        // straight-line interpolation between the observations either side of
        // it; without a counter on both sides there is nothing to divide, and
        // the day change is then no better than an ordinary break.
        let (Some(from), Some(to)) = (self.last_time, obs.total_ms) else {
            return Some(Break::Left);
        };
        let elapsed = now.abs - self.last.abs;
        let before = now.abs - now.secs - self.last.abs;
        Some(Break::Midnight(Start {
            counter_ms: from + share(to - from, elapsed, before),
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
        if let Some(from) = self.last_time {
            // Midnight is 86400 seconds into *this* day; the same instant is
            // second zero of the next, where the run resumes.
            self.credit(self.last.secs, 86_400, boundary.counter_ms - from);
        }
        self.time_hi = self.time_hi.max(boundary.counter_ms);
        if let Some(w) = boundary.words {
            self.words_hi = self.words_hi.max(w);
        }
        self.ended_at = format!("{}T23:59:59", self.last.day);
        self.finish()
    }

    fn finish(self) -> Session {
        let seconds = (self.time_hi - self.time_lo.unwrap_or(self.time_hi)) / 1000;
        Session {
            hours: hours_in_seconds(&self.hours_ms, seconds),
            started_at: self.started_at,
            ended_at: self.ended_at,
            end_position: self.end_position,
            seconds,
            page_turns: self.page_turns,
            words: self.words_hi - self.words_lo.unwrap_or(self.words_hi),
            // Both ends of the counter, not just their difference: a later batch
            // of events continues this run from where it stopped counting, and
            // only the values say where that is.
            start_counter_ms: self.time_lo,
            end_counter_ms: self.time_lo.map(|_| self.time_hi),
            start_words: self.words_lo,
            end_words: self.words_lo.map(|_| self.words_hi),
        }
    }
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
    // Sessions are grouped by the fingerprint every page event carries, then
    // rekeyed to the one the library can actually be joined against.
    //
    // What this archive reveals is remembered, and what earlier ones revealed is
    // applied: a book states its last position only occasionally, so the archive
    // holding a sitting often is not the one that can name it.
    let mut identity = frombook_map(events.iter().map(String::as_str));
    let learned: Vec<(i64, i64)> = identity.iter().map(|(k, v)| (*k, *v)).collect();
    db::record_book_ends(conn, &learned)?;
    for (last_word, from_book) in db::known_book_ends(conn)? {
        identity.entry(last_word).or_insert(from_book);
    }
    // Where the reader was seen standing, before a single session is looked at.
    //
    // First, and unconditionally, because this is the evidence that names a book
    // and these lines are never offered again: the device sends only what is
    // newer than the newest session stored. Anything downstream may decide a
    // sitting is not worth a row — a book opened and shut is not reading — but
    // that decision must not reach back and discard what its events witnessed.
    // The `.yjr` sidecar that names the book may not arrive for another sync.
    for (fingerprint, at) in positions_seen(events.iter().map(String::as_str)) {
        let key = identity.get(&fingerprint).copied().unwrap_or(fingerprint);
        db::record_log_points(conn, key, &[(at.eid, at.offset, at.linear_pos)])?;
    }
    // A stored row may be keyed by either of the book's two end constants: the
    // log line states the last-word position, and [`db::resolve_reading_sessions`]
    // re-keys the row to the last position once the pairing is known. The run is
    // therefore offered under the name the *events* use, so the parser sees the
    // continuation as the same book and not as a switch to another one.
    let resume = resume.map(|r| Resume {
        end_position: identity
            .iter()
            .find(|(_, key)| **key == r.end_position)
            .map_or(r.end_position, |(raw, _)| *raw),
        ..r.clone()
    });
    let sessions = parse_sessions(events.iter().map(String::as_str), resume.as_ref());

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
        // not reading; a row for it would litter the calendar with empty days.
        //
        // A judgement about the row and about nothing else. Everything those
        // events witnessed was recorded above, before this decided the sitting
        // was not worth keeping — a book opened and shut is exactly when the
        // device states where the reader stood in it, and that is the evidence
        // that names the book later.
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
        };
        // The hours the sitting's reading actually fell in — from the log's own
        // intervals, which exist nowhere else and are never sent twice.
        //
        // Written with the row and only with the row. The two are one
        // measurement: hours from a second, longer parse against totals from the
        // first would have the clock report a day the calendar beside it does
        // not. A continued run rebuilds its whole distribution, so the hours are
        // replaced rather than added to.
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
            start_counter_ms: stored[0].start_counter_ms.unwrap(),
            end_counter_ms: stored[0].end_counter_ms.unwrap(),
            start_words: stored[0].start_words,
            end_words: stored[0].end_words,
            page_turns: stored[0].page_turns,
            hours: stored[0].hours.clone(),
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
            start_counter_ms: 60_000,
            end_counter_ms: 120_000,
            start_words: Some(100),
            end_words: Some(220),
            page_turns: 1,
            hours: vec![(10, 60)],
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
            start_counter_ms: stored[0].start_counter_ms.unwrap(),
            end_counter_ms: stored[0].end_counter_ms.unwrap(),
            start_words: stored[0].start_words,
            end_words: stored[0].end_words,
            page_turns: stored[0].page_turns,
            hours: stored[0].hours.clone(),
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
        // The reader comes back at night and the device says so before it says
        // anything about a book. That line carries no counter and no position,
        // so it can end no session — but it must not stand in for the silence
        // either, or the evening's reading joins the morning's.
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
        // Every figure on the page is grouped by the day a session began, so a
        // session that spans two days books the second day's reading to the
        // first. The day change is noticed on whichever line crosses it — here
        // one that observes nothing.
        let lines = [
            page("260803:234500", "NextPage", 148_207, 60_000, 100),
            nameless("260804:000100"),
            page("260804:000200", "NextPage", 148_207, 120_000, 200),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str), None);
        assert_eq!(out.len(), 2);
        assert_eq!(&out[0].started_at[..10], "2026-08-03");
        assert_eq!(&out[1].started_at[..10], "2026-08-04");
        // The clock the cut is made on is the device's own local one, the only
        // clock the syslog prefix carries: `260804:000200` is two minutes past
        // the reader's midnight whatever the offset to UTC happens to be. The
        // same line under UTC would be the 3rd still, and a night's reading
        // would land a day early.
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
        // Half an hour of solid reading in the 20:00 hour and four seconds in
        // the 21:00 hour. Spreading the session's total across its 80-minute
        // window instead — all a stored row can support — would report 452 s of
        // reading at 21:00, and the difference is not recoverable later.
        assert_eq!(out[0].hours, vec![(20, 1806), (21, 4)]);
        assert_eq!(
            out[0].hours.iter().map(|(_, s)| s).sum::<i64>(),
            out[0].seconds,
            "the hours of a session must add up to the session",
        );
    }

    #[test]
    fn a_night_asleep_over_midnight_is_still_a_break_and_bridges_nothing() {
        // The same day change, but the reader left. Splitting at the boundary
        // would credit both days with a share of eight hours the device counted
        // while nobody was reading, so this break is taken where it happened and
        // the interval across it belongs to neither day.
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

        // The evidence is gathered from the lines, not from the sittings the
        // parser makes of them — so it survives a line no session covers.
        // `BookEndPosition` is no observation, and a book opened and shut has no
        // measurable duration and gets no row at all; both state a point, and the
        // log states each of them exactly once.
        let opened_and_shut = ["260803:095956 cvm[6144]: I ReadingTimerController:\
             Information::BookEndPosition.FromBook:YJPosition: AR4GAAAAAAAA:148213,\
             BookEndPosition.LastWordPos.override:YJPosition: AR4GAAAAAAAA:148207,\
             CurrentPos:YJPosition: AR4GAAAAAAAA:524,\
             EndPos:YJPosition: AR4GAAAAAAAA:148207;"];
        assert!(parse_sessions(opened_and_shut.iter().copied(), None).is_empty());
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
