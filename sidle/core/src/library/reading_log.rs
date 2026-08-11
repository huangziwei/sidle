//! Recover reading sessions from a Kindle's own system logs.
//!
//! The device writes `/var/local/log/messages` and, at most once a day, gzips a
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
    /// Every distinct point the reader was seen at, ascending.
    ///
    /// Not stored — this is how the session gets a book. The log redacts the
    /// title, but a `.yjr` sidecar records where its own book was left, and the
    /// sync files that under a `book_id`. A point in both is the same reader in
    /// the same book.
    pub locations: Vec<Location>,
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
fn end_position(payload: &str) -> Option<i64> {
    let at = payload.find("EndPos:YJPosition: ")?;
    if payload.find("NextTOCEntry").is_some_and(|toc| toc < at) {
        return None;
    }
    let rest = &payload[at + "EndPos:YJPosition: ".len()..];
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
    /// Where the reader was standing. Names no book by itself, but a book's
    /// sidecar names the same point.
    at: Option<Location>,
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
        at: location(chosen, "CurrentPos"),
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
    let mut opened: Option<Opened> = None;

    for line in events {
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

        if let Some(counter_ms) = opened_at_counter(line) {
            opened = Some(Opened {
                counter_ms,
                day: day.clone(),
                secs,
                at: at.clone(),
            });
        }

        let Some(obs) = observation(line) else {
            continue;
        };

        // Consumed at the first observation after the open, whether or not it is
        // used: an open belongs to the session it precedes and to no later one.
        let seed = opened
            .take()
            .and_then(|o| o.vouch(&day, secs, obs.total_ms));

        if let Some(cur) = &open
            && (cur.end_position != obs.position || gapped)
        {
            out.extend(open.take().map(Open::finish));
        }
        let cur = open.get_or_insert_with(|| Open::new(obs.position, &at, seed));
        cur.observe(&at, &obs);
        if obs.closes {
            out.extend(open.take().map(Open::finish));
        }
    }
    out.extend(open.map(Open::finish));
    out
}

/// An `OpenBook` seen but not yet attached to a session.
struct Opened {
    counter_ms: i64,
    day: String,
    secs: i64,
    at: String,
}

/// Where a session begins: the counter it resumes from and the clock time it
/// started at, both taken from the `OpenBook` that vouched for them.
struct Start {
    counter_ms: i64,
    at: String,
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
    fn vouch(self, day: &str, secs: i64, first_total: Option<i64>) -> Option<Start> {
        let total = first_total?;
        let elapsed = secs.checked_sub(self.secs).filter(|e| *e >= 0)?;
        (self.day == day
            && self.counter_ms <= total
            && total - self.counter_ms <= (elapsed + SEED_SLACK_SECS) * 1000)
            .then_some(Start {
                counter_ms: self.counter_ms,
                at: self.at,
            })
    }
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
    locations: Vec<Location>,
}

impl Open {
    /// `start` is where the book was opened, where an `OpenBook` vouched for it.
    /// That is the session's true floor in both senses: the first *logged*
    /// observation can sit minutes past the open, so taking it as the floor
    /// discards the reading in between and reports a sitting that took no time
    /// at all. Without one, the first observation is the best available start.
    fn new(end_position: i64, at: &str, start: Option<Start>) -> Self {
        let (time_lo, started_at) = match start {
            Some(s) => (Some(s.counter_ms), s.at),
            None => (None, at.to_string()),
        };
        Self {
            end_position,
            started_at,
            ended_at: at.to_string(),
            time_lo,
            time_hi: time_lo.unwrap_or(0),
            words_lo: None,
            words_hi: 0,
            page_turns: 0,
            locations: Vec::new(),
        }
    }

    fn observe(&mut self, at: &str, obs: &Observation) {
        self.ended_at = at.to_string();
        if obs.page_turn {
            self.page_turns += 1;
        }
        if let Some(t) = obs.total_ms {
            self.time_lo = Some(self.time_lo.map_or(t, |lo| lo.min(t)));
            self.time_hi = self.time_hi.max(t);
        }
        if let Some(w) = obs.words {
            self.words_lo = Some(self.words_lo.map_or(w, |lo| lo.min(w)));
            self.words_hi = self.words_hi.max(w);
        }
        if let Some(at) = obs.at {
            self.locations.push(at);
        }
        // A book reopened after finishing restarts its counter; the run so far is
        // already banked because a fresh `Open` is made per run.
    }

    fn finish(mut self) -> Session {
        self.locations.sort_unstable();
        self.locations.dedup();
        Session {
            locations: self.locations,
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
    let mut out = store_events(conn, &found.events, found.files, device_serial, watch)?;
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

/// The single book every one of these points belongs to, or `None`.
///
/// One agreement is enough to name a session, because a point is specific
/// enough to belong to one book. Two *different* books agreeing is not a
/// stronger answer but a contradiction — one of the points is a coincidence and
/// nothing here can say which — so it names nothing.
fn names_a_book(
    locations: &[Location],
    positions: &std::collections::HashMap<(i64, i64, i64), i64>,
) -> Option<i64> {
    let mut found: Option<i64> = None;
    for at in locations {
        let Some(&book) = positions.get(&(at.eid, at.offset, at.linear_pos)) else {
            continue;
        };
        match found {
            Some(held) if held != book => return None,
            _ => found = Some(book),
        }
    }
    found
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
    let sessions = parse_sessions(events.iter().map(String::as_str));
    let positions = db::device_positions(conn, device_serial)?;

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
        // The log cannot name a book, but it can say where the reader stood,
        // and the sidecar sync already knows which book was left at that point.
        // One agreement names the fingerprint for good: `attribute_reading_position`
        // settles every unattributed session that stopped there, so a book only
        // has to be caught once for its whole history to follow.
        if let Some(book_id) = names_a_book(&s.locations, &positions) {
            db::attribute_reading_position(conn, row.end_position, book_id)?;
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
    fn a_session_collects_the_points_the_reader_stood_at() {
        let lines = [
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            page("260803:100500", "CloseBook", 148_207, 120_000, 220),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str));
        // `page` writes the same handle on both lines, so the two collapse:
        // the points are the distinct places visited, not one per event.
        assert_eq!(
            out[0].locations,
            vec![Location {
                eid: 1566,
                offset: 0,
                linear_pos: 12
            }]
        );
    }

    #[test]
    fn a_point_two_books_disagree_about_names_neither() {
        let a = Location {
            eid: 1566,
            offset: 0,
            linear_pos: 12,
        };
        let b = Location {
            eid: 99,
            offset: 0,
            linear_pos: 500,
        };
        let mut positions = std::collections::HashMap::new();
        positions.insert((a.eid, a.offset, a.linear_pos), 7_i64);
        assert_eq!(names_a_book(&[a, b], &positions), Some(7));

        // A second book claiming another of the same session's points is a
        // contradiction, not a tie-break: one of them is a coincidence and
        // nothing here can say which.
        positions.insert((b.eid, b.offset, b.linear_pos), 9_i64);
        assert_eq!(names_a_book(&[a, b], &positions), None);
        assert_eq!(names_a_book(&[], &positions), None);
    }

    #[test]
    fn a_close_that_is_not_first_on_its_line_still_ends_the_session() {
        let lines = [
            page("260803:100000", "NextPage", 148_207, 60_000, 100),
            buried_close("260803:100500", 148_207, 120_000, 220),
            page("260803:100600", "NextPage", 148_207, 130_000, 240),
        ];
        let out = parse_sessions(lines.iter().map(String::as_str));
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
        let out = parse_sessions(lines.iter().map(String::as_str));
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
        let out = parse_sessions(lines.iter().map(String::as_str));
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
        let out = parse_sessions(lines.iter().map(String::as_str));
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
