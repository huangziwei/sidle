//! Reading-session event lines selected out of the Kindle's own logs for sync.

use std::io::Read;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};

/// The tags on a line that states reading.
const READING_MARKERS: [&str; 9] = [
    "ReadingTimerController",
    "SchemaName[ereader_open_book]",
    "SchemaName[ereader_close_book]",
    "SchemaName[ereader_book_consume_content]",
    "SchemaName[ereader_book_page_turn]",
    "SchemaName[ereader_book_linear_page_actions]",
    "SchemaName[ereader_content_point]",
    "SchemaName[ereader_reader_latency_ops]",
    "SchemaName[ereader_reader_page_turn_latency_ops]",
];

/// The tags on a line that states whether the device was awake, the measure
/// left for a book `ReadingTimerController` never times.
const POWER_MARKERS: [&str; 4] = [
    "ereader_powerd_state_change",
    "lipc:evts:name=outOfScreenSaver, origin=com.lab126.powerd",
    "lipc:evts:name=goingToScreenSaver, origin=com.lab126.powerd",
    "lipc:evts:name=suspending, origin=com.lab126.powerd",
];

/// The tag on a [`take_catalog`] line, marking it this collector's statement
/// and not the firmware's.
const CATALOG_TAG: &str = "SidleCatalog";

/// Every tag [`take_events`] selects on; the prefilter ahead of everything
/// else. The desktop's own set names the same tags, the two crates building
/// apart.
const MARKERS: [&str; 14] = {
    let (r, p) = (READING_MARKERS, POWER_MARKERS);
    [
        r[0],
        r[1],
        r[2],
        r[3],
        r[4],
        r[5],
        r[6],
        r[7],
        r[8],
        p[0],
        p[1],
        p[2],
        p[3],
        CATALOG_TAG,
    ]
};

/// The live syslog, on the root filesystem's tmpfs and not under `/mnt/us`.
/// Every file in [`DUMP_DIR`] is a snapshot of it. No file of this name sits in
/// [`LOG_DIR`] beside the chunks rotated out of it.
const LIVE_LOG: &str = "/var/log/messages";

/// The directory `tinyrot` gzips [`LIVE_LOG`]'s rotated chunks into, on flash.
const LOG_DIR: &str = "/var/local/log";

/// What a rotated chunk's name begins with:
/// `messages_00000807_20260807101501.gz`. The trailing `_` separates it from
/// `messages`.
const CHUNK_PREFIX: &str = "messages_";

/// Where the firmware keeps its daily snapshots, relative to `/mnt/us`.
const DUMP_DIR: &str = "system/logbackup";

/// An archive of [`MARKERS`] lines, relative to `/mnt/us`, kept past the 30
/// daily dumps [`DUMP_DIR`] holds. A measured month is 1.1 MB gzipped against
/// 92 MB of dumps.
const ARCHIVE_DIR: &str = "extensions/sidle/readinglog";

/// What an archive file is called: `rl_<YYMMDDHHMMSS>.txt.gz`, stamped with the
/// newest line it holds — the shape [`DUMP_DIR`]'s names take. [`archive_files`]
const ARCHIVE_PREFIX: &str = "rl_";

/// The newest line [`archive`] has written, in [`ARCHIVE_DIR`] beside the
/// files. [`purge_archive`] deletes every [`ARCHIVE_PREFIX`] file the library
/// confirms and leaves this 13-byte stamp, which is what [`archive_watermark`]
const ARCHIVE_MARK: &str = "mark";

/// What one collection pass looked at, for the sync log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Collected {
    /// Distinct event lines newer than the watermark.
    pub lines: Vec<String>,
    /// Names of the dumps that decoded to the end, for the desktop to record
    /// and skip next time. A dump counted in `truncated` is absent from this.
    pub read: Vec<String>,
    /// Dumps skipped on their name, without being opened.
    pub skipped: usize,
    /// Dumps that decoded partway. Their prefix went into `lines`.
    pub truncated: usize,
    /// Event lines each source offered, ahead of the de-duplication across them.
    /// The sources overlap, and these do not add up to `lines`.
    pub from: Sources,
}

/// Event lines taken from each of the four sources, for the sync log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Sources {
    pub dumps: usize,
    pub live: usize,
    /// Whether `LIVE_LOG` opened. An absent file and a file with nothing past
    /// the watermark both leave `live` at 0.
    pub live_read: bool,
    pub chunks: usize,
    pub archive: usize,
    /// Catalog rows named, from [`take_catalog`]. Not a log source: these lines
    /// are this collector's own statement about the books on the device.
    pub catalog: usize,
}

/// The `YYMMDD:HHMMSS` a dump's name encodes, matching the prefix its lines
/// carry so the two compare directly.
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

/// Every [`MARKERS`] line newer than `watermark`, across four sources:
/// [`DUMP_DIR`], `LIVE_LOG`, [`LOG_DIR`]'s chunks, and [`ARCHIVE_DIR`].
pub fn collect(us_root: &Path, watermark: &str, seen: &[String]) -> Collected {
    let mut out = Collected::default();
    // A plain Vec plus a sort. The desktop de-duplicates, and a set of every
    // line holds the selection twice on a device with 512 MB.
    for (name, path) in dumps(us_root, watermark, seen, &mut out) {
        if let Some(dump) = read_maybe_gzip(&path) {
            // Both cases give up their lines; `read` takes the complete ones.
            if dump.complete {
                out.read.push(name);
            } else {
                out.truncated += 1;
            }
            out.from.dumps += take_events(&dump.text, watermark, &mut out.lines);
        }
    }
    // `LIVE_LOG` ahead of `LOG_DIR`, without tinyrot's lock: a rotation between
    // the two reads then duplicates lines into a de-duplicated selection.
    take_live(Path::new(LIVE_LOG), watermark, &mut out);
    for path in chunks(Path::new(LOG_DIR), watermark, &mut out) {
        // A chunk pruned between the listing and the read yields nothing, and
        // one caught mid-rotation yields its intact prefix.
        if let Some(chunk) = read_maybe_gzip(&path) {
            out.from.chunks += take_events(&chunk.text, watermark, &mut out.lines);
        }
    }
    // `ARCHIVE_DIR` last: the one copy of events past `DUMP_DIR`'s month and
    // `LOG_DIR`'s pruning.
    for path in archive_files(us_root, watermark, &mut out) {
        if let Some(old) = read_maybe_gzip(&path) {
            out.from.archive += take_events(&old.text, watermark, &mut out.lines);
        }
    }
    // Last: `take_catalog` stamps its rows at the newest line in `out.lines`.
    out.from.catalog = take_catalog(&catalog_paths(), &mut out.lines);
    out.lines.sort();
    out.lines.dedup();
    out
}

/// The content catalog, newest firmware first. `/var/local` is a symlink to
/// `/var/base-local`, and one device answers to more than one of these;
/// [`take_catalog`] takes the first that exists.
fn catalog_paths() -> Vec<PathBuf> {
    [
        "/var/base-local/metadata/cc.db",
        "/var/local/metadata/cc.db",
        "/var/local/cc.db",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

/// Append one [`CATALOG_TAG`] line per book on the device to `lines`, and
/// answer with how many.
fn take_catalog(paths: &[PathBuf], lines: &mut Vec<String>) -> usize {
    let Some(stamp) = lines.iter().filter_map(|l| line_stamp(l)).max() else {
        return 0;
    };
    let stamp = stamp.to_string();
    let Some(db) = paths.iter().find(|p| p.exists()) else {
        return 0;
    };
    // Plain, never `mode=ro` or `immutable=1`: on a WAL `cc.db` the first fails
    // for want of a -shm file and the second reads pre-WAL state. A SELECT
    // writes nothing.
    let Ok(sql) = std::process::Command::new("sqlite3")
        .arg("-separator")
        .arg("\u{1}")
        .arg(db)
        .arg(CATALOG_QUERY)
        .output()
    else {
        return 0;
    };
    let mut n = 0;
    for row in String::from_utf8_lossy(&sql.stdout).lines() {
        let mut f = row.split('\u{1}');
        let (Some(extent), Some(key)) = (f.next(), f.next()) else {
            continue;
        };
        if extent.is_empty() || key.is_empty() {
            continue;
        }
        let cde_type = f.next().unwrap_or_default();
        lines.push(format!(
            "{stamp} sidle-native: I {CATALOG_TAG}:extent={extent},cde_key={key},cde_type={cde_type};"
        ));
        n += 1;
    }
    n
}

/// `p_location` is non-empty on a book the device holds, empty on a cloud row.
/// `p_contentState` is 1 on a store book and 0 on a sideload.
/// `p_cdeKey` beginning `*` is a loose file: a scriptlet, `My Clippings.txt`.
const CATALOG_QUERY: &str = "select p_contentSize, p_cdeKey, p_cdeType from Entries \
     where p_location is not null and p_location <> '' \
       and p_contentSize is not null \
       and p_cdeKey is not null and p_cdeKey not like '*%' \
       and p_cdeType in ('EBOK', 'PDOC', 'MAGZ')";

/// Take `LIVE_LOG`'s new events into `out.lines`, and set `out.from.live_read`.
fn take_live(path: &Path, watermark: &str, out: &mut Collected) {
    let Some(live) = read_maybe_gzip(path) else {
        return;
    };
    out.from.live_read = true;
    out.from.live += take_events(&live.text, watermark, &mut out.lines);
}

/// Whether `lines` holds any reading.
pub fn has_reading(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|l| READING_MARKERS.iter().any(|m| l.contains(m)))
}

/// The newest archived event as the `YYMMDD:HHMMSS` a log line carries, empty
/// for an untouched [`ARCHIVE_DIR`].
pub fn archive_watermark(us_root: &Path) -> String {
    let marked =
        std::fs::read_to_string(us_root.join(ARCHIVE_DIR).join(ARCHIVE_MARK)).unwrap_or_default();
    let marked = marked.trim().to_string();
    let Ok(entries) = std::fs::read_dir(us_root.join(ARCHIVE_DIR)) else {
        return marked;
    };
    entries
        .flatten()
        .filter_map(|e| archive_stamp(&e.file_name().to_string_lossy()))
        .chain(std::iter::once(marked))
        .max()
        .unwrap_or_default()
}

/// How often the archiver runs, matching stock `tinyrot`'s cadence: `tinyrot`
/// rotates `LIVE_LOG`, and this rate stays within one rotation of it.
pub const ARCHIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// The running archiver's pid and build stamp, read by [`archiver`]. On the
/// user partition, the writable one.
const DAEMON_PID: &str = "/mnt/us/extensions/sidle/archive.pid";

/// The flag that turns this binary into the archiver, and what [`classify`]
/// matches a `/proc` cmdline against.
pub const DAEMON_FLAG: &str = "--archive-daemon";

/// The archiver's own copy of this binary, named to share no text with the
/// picker's `bin/sidle`.
const DAEMON_BIN: &str = "/mnt/us/extensions/sidle/bin/readinglogd";

/// What the pidfile and `/proc` together say about the archiver.
#[derive(Debug, PartialEq, Eq)]
pub enum Archiver {
    /// No archiver is running.
    Absent,
    /// This build's archiver is running.
    Running,
    /// An archiver from a different build is running, carrying its pid. Its
    /// [`DAEMON_PID`] is what reports an archiver as up, and a self-update
    /// swaps `bin/sidle` underneath it.
    Outdated(u32),
}

/// [`DAEMON_PID`] read against `/proc`.
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

/// A pid from [`DAEMON_PID`], the build stamp beside it, and the cmdline
/// `/proc` reports for that pid. A `None` stamp matches no build.
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

/// Signal an archiver and delete [`DAEMON_PID`]. `pid` comes from
/// [`archiver`], which confirms it against `/proc`.
pub fn stop_archiver(pid: u32) {
    // SAFETY: `kill` sends a signal to a pid and touches nothing in this
    // process. SIGTERM: the archiver holds no lock.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let _ = std::fs::remove_file(DAEMON_PID);
}

/// Start an archiver and answer with its pid.
pub fn start_archiver() -> std::io::Result<u32> {
    let bin = Path::new(DAEMON_BIN);
    stage_binary(&std::env::current_exe()?, bin)?;
    let mut cmd = std::process::Command::new(bin);
    cmd.arg(DAEMON_FLAG);
    // `setsid`, for a life independent of the launcher's session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    Ok(cmd.spawn()?.id())
}

/// Copy `src` over `dst` through a `.new` scratch name beside it. `dst` is a
/// file a process may be executing, and the rename stays within one FAT mount.
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

/// Delete [`ARCHIVE_DIR`] files stamped at or before `watermark`, and report
/// how many went.
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

/// The [`ARCHIVE_DIR`] files whose stamp is past `watermark`. [`archive`]
/// stamps a name with the newest line inside, making the test exact where
/// [`chunks`]'s is loose.
fn archive_files(us_root: &Path, watermark: &str, out: &mut Collected) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(us_root.join(ARCHIVE_DIR)) else {
        return Vec::new();
    };
    let mut keep = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // A `.part` is an unfinished write, stamped for more than it holds.
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

/// Add `lines` to [`ARCHIVE_DIR`], merged into the day's file, and answer with
/// that file's name. `None` for lines carrying no stamp.
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

    // The scratch name carries the pid: two overlapping runs interleave their
    // writes into one shared path.
    let tmp = dir.join(format!("{name}.{}.part", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, dir.join(&name))?;
    if let Some(old) = existing
        && old != dir.join(&name)
    {
        let _ = std::fs::remove_file(old);
    }
    // Written after the file it describes, and forwards only: a mark ahead of
    // the disk skips lines this pass is the last to see.
    let mark = us_root.join(ARCHIVE_DIR).join(ARCHIVE_MARK);
    if std::fs::read_to_string(&mark).unwrap_or_default().trim() < newest {
        let _ = std::fs::write(&mark, newest);
    }
    Ok(Some(name))
}

/// The `YYMMDD:HHMMSS` a rotated chunk's name encodes, from its 14-digit
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

/// The [`LOG_DIR`] chunks worth opening, oldest first.
fn chunks(dir: &Path, watermark: &str, out: &mut Collected) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dated: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // An unparseable name is read, at the cost of a gunzip.
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

/// The [`DUMP_DIR`] files worth opening. Two tests on the name alone: `seen`
/// holds it, or [`dump_stamp`] is at or before `watermark` — a snapshot taken
/// at T holds nothing after T.
fn dumps(
    us_root: &Path,
    watermark: &str,
    seen: &[String],
    out: &mut Collected,
) -> Vec<(String, PathBuf)> {
    let dir = us_root.join(DUMP_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        // An unreadable `DUMP_DIR` leaves `LIVE_LOG` and `LOG_DIR`.
        return Vec::new();
    };
    let mut keep = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if seen.contains(&name) {
            out.skipped += 1;
            continue;
        }
        // An unparseable name is read, at the cost of a gunzip.
        match dump_stamp(&name) {
            Some(stamp) if !watermark.is_empty() && stamp.as_str() <= watermark => out.skipped += 1,
            _ => keep.push((name, entry.path())),
        }
    }
    keep.sort();
    keep
}

/// Append every event line in `text` that is newer than `watermark`, and answer
/// with how many that was.
fn take_events(text: &str, watermark: &str, out: &mut Vec<String>) -> usize {
    let before = out.len();
    for line in text.lines() {
        if !MARKERS.iter().any(|m| line.contains(m)) {
            continue;
        }
        // A line with no [`line_stamp`] is kept; the desktop's parser drops it.
        match line_stamp(line) {
            Some(stamp) if !watermark.is_empty() && stamp <= watermark => continue,
            _ => out.push(line.to_string()),
        }
    }
    out.len() - before
}

/// A decoded log file, and whether it decoded to the end.
struct Decoded {
    text: String,
    /// False on a truncated decode, leaving `text` a prefix. Such a file goes
    /// to [`Collected::truncated`] and not to [`Collected::read`].
    complete: bool,
}

/// Decode a log file, gunzipping a gzipped one, and keep a truncated decode's
/// intact prefix.
fn read_maybe_gzip(path: &Path) -> Option<Decoded> {
    let bytes = std::fs::read(path).ok()?;
    // An empty file is a created-and-unwritten dump.
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
        // Two dumps, one stamped before the watermark and one after.
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
        // An empty watermark skips nothing.
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
        // The line at the watermark, the line past it, and a line carrying no
        // MARKERS tag.
        assert_eq!(lines, vec![event("260809:120000")]);
    }

    #[test]
    fn a_line_the_desktop_already_holds_costs_nothing_on_the_wire() {
        let text = event("260701:090000");
        let mut lines = Vec::new();
        take_events(&text, "260809:000000", &mut lines);
        assert!(lines.is_empty());
    }

    /// Records copied unedited from a device log, one per [`READING_MARKERS`]
    /// entry past `ReadingTimerController`.
    const METRICS: &[&str] = &[
        r#"260816:141126.136 fastmetrics[10393]: D fastmetrics:KindleFastMetricsPublisher:[6562.272921]: Emitting a new record. SchemaName[ereader_open_book], Fields[{ 	"book_category" : "MAGZ", 	"book_format" : "mobi7", 	"is_downloaded" : 0, 	"is_opened_by_kpp_reader" : "Yes", 	"language" : "en", 	"load_method" : "catalog", 	"start_reading_position" : 184 } ]. :"#,
        r#"260816:141151.131 fastmetrics[10393]: D fastmetrics:KindleFastMetricsPublisher:[6587.267540]: Emitting a new record. SchemaName[ereader_close_book], Fields[{ 	"close_method" : "Navigation", 	"close_position" : 5070, 	"close_timestamp" : 1786882311101, 	"is_opened_by_kpp_reader" : "Yes" } ]. :"#,
        r#"260816:181520 fastmetrics[4576]: D fastmetrics:KindleFastMetricsPublisher:[9788.019459]: Emitting a new record. SchemaName[ereader_book_consume_content], Fields[{ 	"context" : "Book:Reading:MainContent", 	"end_position" : 4133, 	"span_type" : "Text", 	"start_position" : 3227, 	"words_count" : 147 } ]. :"#,
        r#"260816:141128.481 fastmetrics[10393]: D fastmetrics:KindleFastMetricsPublisher:[6564.618216]: Emitting a new record. SchemaName[ereader_book_page_turn], Fields[{ 	"action_id" : "NextPageTurnWithGESTURE_TAP_SWIPES", 	"action_start_time" : 1786882288349, 	"book_category" : "MAGZ", 	"book_format" : "mobi7", 	"failure_key" : "NextPageTurn.SUCCESS", 	"is_opened_by_kpp_reader" : "Yes", 	"is_virtual_focus_location_used" : "No" } ]. :"#,
        r#"260816:181520 fastmetrics[4576]: D fastmetrics:KindleFastMetricsPublisher:[9788.000965]: Emitting a new record. SchemaName[ereader_book_linear_page_actions], Fields[{ 	"action_id" : "NextPageWithSwipe", 	"context" : "Book:Reading:MainContent" } ]. :"#,
        r#"260816:141139.179 fastmetrics[10393]: D fastmetrics:KindleFastMetricsPublisher:[6575.315703]: Emitting a new record. SchemaName[ereader_content_point], Fields[{ 	"context" : "Book:Reading:MainContent", 	"point_type" : "ChapterStart", 	"position" : 184 } ]. :"#,
        r#"260816:141341.957 fastmetrics[10393]: D fastmetrics:KindleFastMetricsPublisher:[6778.093472]: Emitting a new record. SchemaName[ereader_reader_latency_ops], Fields[{ 	"action" : "OpenBookTotalTime", 	"book_category" : "MAGZ", 	"book_format" : "mobi7", 	"cde_key" : "B00QPFC59S", 	"is_kpp_open_book_cache_hit" : "No", 	"is_opened_by_kpp_reader" : "Yes", 	"latency" : 851 } ]. :"#,
        r#"260816:141128.482 fastmetrics[10393]: D fastmetrics:KindleFastMetricsPublisher:[6564.619104]: Emitting a new record. SchemaName[ereader_reader_page_turn_latency_ops], Fields[{ 	"action" : "PageTurnTotalTime", 	"book_category" : "MAGZ", 	"book_format" : "mobi7", 	"cde_key" : "B00QPFC59S", 	"is_opened_by_kpp_reader" : "Yes", 	"latency" : 132 } ]. :"#,
    ];

    /// A record `log_backup.sh` writes whose schema no marker names.
    const UNWANTED: &str = r#"260816:141126.000 fastmetrics[10393]: D fastmetrics:KindleFastMetricsPublisher:[6562.000000]: Emitting a new record. SchemaName[ereader_open_book_failure_backup], Fields[{ 	"book_category" : "MAGZ", 	"cde_key" : "B00QPFC59S" } ]. :"#;

    /// Every reader-shell record is selected, and a schema outside the set is
    /// left behind. `ereader_open_book_failure_backup` shares a prefix with
    /// `ereader_open_book`.
    #[test]
    fn the_reader_shell_records_are_selected_and_their_lookalikes_are_not() {
        let mut text = METRICS.join("\n");
        text.push('\n');
        text.push_str(UNWANTED);

        let mut lines = Vec::new();
        take_events(&text, "", &mut lines);
        assert_eq!(lines.len(), METRICS.len());
        assert!(lines.iter().all(|l| !l.contains("failure_backup")));
    }

    /// `METRICS` alone, with no `ReadingTimerController` line, is reading.
    #[test]
    fn a_batch_of_reader_shell_records_alone_is_reading() {
        let lines: Vec<String> = METRICS.iter().map(|l| l.to_string()).collect();
        assert!(has_reading(&lines));
    }

    /// `take_catalog` finds no path here, and `lines` is left as it was.
    #[test]
    fn a_catalog_row_is_stamped_at_the_batch_it_names() {
        let mut lines = vec![event("260814:112035"), event("260814:113240")];
        let missing = [PathBuf::from("/nonexistent/cc.db")];
        assert_eq!(take_catalog(&missing, &mut lines), 0);
        assert_eq!(lines.len(), 2, "a missing catalog adds nothing");
    }

    /// An empty `lines` has no stamp to give a catalog row, and takes none.
    #[test]
    fn a_quiet_batch_names_no_books() {
        let mut lines: Vec<String> = Vec::new();
        assert_eq!(
            take_catalog(&[PathBuf::from("/nonexistent")], &mut lines),
            0
        );
        assert!(lines.is_empty());
    }

    /// A `SidleCatalog` line passes `take_events` and fails `has_reading`.
    #[test]
    fn a_catalog_row_is_carried_but_is_not_a_sitting() {
        let row = "260814:112035 sidle-native: I SidleCatalog:extent=416436,\
                   cde_key=L7P5OOJTFVDRFUJ2OFMKCAP7JYEACNDZ,cde_type=PDOC;"
            .to_string();
        let mut taken = Vec::new();
        take_events(&row, "", &mut taken);
        assert_eq!(taken.len(), 1, "carried");
        assert!(!has_reading(&taken), "and not a sitting");
    }

    /// Power lines alone are not reading: a Kindle sleeps and wakes dozens of
    /// times a day.
    #[test]
    fn power_lines_alone_are_not_reading() {
        let lines = vec![
            "260814:111900 powerd[4213]: I lipc:evts:name=outOfScreenSaver, origin=com.lab126.powerd, fparam=2:Event sent".to_string(),
            r#"260814:113500.549 fastmetrics[9842]: D fastmetrics:KindleFastMetricsPublisher:[26548.733985]: Emitting a new record. SchemaName[ereader_powerd_state_change], Fields[{ 	"curr_state" : "SCREEN SAVER", 	"prev_state" : "ACTIVE" } ]. :"#.to_string(),
        ];
        assert!(!has_reading(&lines));
        let mut taken = Vec::new();
        take_events(&lines.join("\n"), "", &mut taken);
        assert_eq!(taken.len(), 2, "collected, and not counted as reading");
    }

    /// A dump caught mid-write lands in `truncated` with its prefix in `lines`,
    /// and stays out of `read`. `log_backup.sh` gzips a snapshot under a name
    /// that never changes.
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

    /// `LIVE_LOG` sits outside [`LOG_DIR`]. `tinyrot` rotates
    /// `/var/log/messages`, on the tmpfs, into `/var/local/log`, on flash under
    /// `/var/base-local`, and no `messages` exists there.
    #[test]
    fn the_live_log_is_not_looked_for_beside_its_own_chunks() {
        assert!(
            !LIVE_LOG.starts_with(LOG_DIR),
            "{LIVE_LOG:?} is where the chunks are, not where the firmware writes"
        );
    }

    /// `live_read` separates an absent `LIVE_LOG` from one with nothing past
    /// the watermark. Both leave `live` at 0.
    #[test]
    fn an_unread_live_log_is_not_reported_as_a_quiet_one() {
        let dir = tempdir();
        let live = dir.join("messages");

        let mut missing = Collected::default();
        take_live(&live, "", &mut missing);
        assert!(!missing.from.live_read, "there was no file to read");
        assert_eq!(missing.from.live, 0);

        // Present, holding only lines at or before the watermark.
        std::fs::write(&live, format!("{}\n", event("260809:100000"))).unwrap();
        let mut quiet = Collected::default();
        take_live(&live, "260809:120000", &mut quiet);
        assert!(quiet.from.live_read, "the file was read");
        assert_eq!(quiet.from.live, 0, "and had nothing past the watermark");

        // Present, holding a line past the watermark.
        let mut reading = Collected::default();
        take_live(&live, "260809:090000", &mut reading);
        assert!(reading.from.live_read);
        assert_eq!(reading.lines, vec![event("260809:100000")]);
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

    /// [`chunks`] keeps the newest chunk stamped at or before the watermark,
    /// the one whose content straddles it.
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

        // An empty watermark keeps every chunk.
        let mut fresh = Collected::default();
        assert_eq!(chunks(&dir, "", &mut fresh).len(), 4);
        assert_eq!(fresh.skipped, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [`archive`] names a file for the newest line it holds, and
    /// [`archive_watermark`] reads that name.
    #[test]
    fn the_archive_states_its_own_newest_event_in_its_name() {
        let dir = tempdir();
        assert_eq!(archive_watermark(&dir), "", "nothing archived yet");

        let lines = vec![event("260809:100000"), event("260809:120000")];
        let name = archive(&dir, &lines).unwrap().unwrap();
        assert_eq!(name, "rl_260809120000.txt.gz", "named for the newest line");
        assert_eq!(archive_watermark(&dir), "260809:120000");

        // An empty slice writes no file.
        assert_eq!(archive(&dir, &[]).unwrap(), None);
        assert_eq!(archive_watermark(&dir), "260809:120000");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each [`ARCHIVE_INTERVAL`] run folds into the day's one file, renamed
    /// forward as it grows, with the previous name removed.
    #[test]
    fn a_days_runs_fold_into_a_single_file() {
        let dir = tempdir();
        archive(&dir, &[event("260809:100000")]).unwrap();
        archive(&dir, &[event("260809:103000")]).unwrap();
        let name = archive(&dir, &[event("260809:110000")]).unwrap().unwrap();
        assert_eq!(name, "rl_260809110000.txt.gz");

        assert_eq!(archived(&dir), vec!["rl_260809110000.txt.gz".to_string()]);

        // Both runs' lines survive the fold.
        let found = collect(&dir, "", &[]);
        assert_eq!(
            found.lines,
            vec![
                event("260809:100000"),
                event("260809:103000"),
                event("260809:110000")
            ]
        );

        // A new `YYMMDD` starts its own file.
        archive(&dir, &[event("260810:090000")]).unwrap();
        assert_eq!(
            archived(&dir),
            vec![
                "rl_260809110000.txt.gz".to_string(),
                "rl_260810090000.txt.gz".to_string()
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [`collect`] reads [`ARCHIVE_DIR`], the only copy left once [`DUMP_DIR`]
    /// and [`LOG_DIR`] have both been pruned.
    #[test]
    fn a_sync_after_a_long_gap_gets_its_history_from_the_archive() {
        let dir = tempdir();
        let old = vec![event("260701:090000"), event("260715:093000")];
        archive(&dir, &old).unwrap();

        // An empty watermark takes the archived lines, with no dump or chunk.
        let found = collect(&dir, "", &[]);
        assert_eq!(found.lines, old);

        // A watermark past the stamp opens nothing, on the filename alone.
        let mut caught_up = collect(&dir, "260715:093000", &[]);
        assert!(caught_up.lines.is_empty());
        assert_eq!(caught_up.skipped, 1);

        // A `.part` file is skipped: its stamp promises a tail it lacks.
        std::fs::write(
            dir.join(ARCHIVE_DIR).join("rl_260801000000.txt.gz.part"),
            b"junk",
        )
        .unwrap();
        caught_up = collect(&dir, "260715:093000", &[]);
        assert!(caught_up.lines.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [`purge_archive`] deletes up to what the library stored, and no further.
    #[test]
    fn a_confirmed_sync_clears_the_archive_it_covers() {
        let dir = tempdir();
        archive(&dir, &[event("260701:090000")]).unwrap();
        archive(&dir, &[event("260715:093000")]).unwrap();
        archive(&dir, &[event("260809:120000")]).unwrap();

        // A watermark of 260715 leaves the file stamped past it.
        assert_eq!(purge_archive(&dir, "260715:093000"), 2);
        assert_eq!(archive_watermark(&dir), "260809:120000");
        assert_eq!(collect(&dir, "", &[]).lines, vec![event("260809:120000")]);

        // An empty watermark confirms nothing and deletes nothing.
        assert_eq!(purge_archive(&dir, ""), 0);
        assert_eq!(archive_watermark(&dir), "260809:120000");

        assert_eq!(purge_archive(&dir, "260809:120000"), 1);
        assert!(archived(&dir).is_empty(), "nothing left to keep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [`ARCHIVE_MARK`] survives a [`purge_archive`] that deletes every
    /// [`ARCHIVE_PREFIX`] file, holding [`archive_watermark`] where it was.
    #[test]
    fn a_purge_does_not_reset_how_far_the_archiver_has_read() {
        let dir = tempdir();
        archive(&dir, &[event("260809:100000"), event("260809:120000")]).unwrap();
        assert_eq!(archive_watermark(&dir), "260809:120000");

        // A watermark past every stamp takes every file.
        assert_eq!(purge_archive(&dir, "260809:120000"), 1);
        assert!(
            collect(&dir, "", &[]).lines.is_empty(),
            "the files are gone"
        );
        assert_eq!(
            archive_watermark(&dir),
            "260809:120000",
            "but the archiver still knows where it got to"
        );

        // Filenames alone answer for an archive with no ARCHIVE_MARK.
        std::fs::remove_file(dir.join(ARCHIVE_DIR).join(ARCHIVE_MARK)).unwrap();
        archive(&dir, &[event("260810:090000")]).unwrap();
        std::fs::write(dir.join(ARCHIVE_DIR).join(ARCHIVE_MARK), "260701:000000").unwrap();
        assert_eq!(archive_watermark(&dir), "260810:090000");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [`classify`] answers [`Archiver::Absent`] for a cmdline carrying no
    /// [`DAEMON_FLAG`]. A [`DAEMON_PID`] outlives the process that wrote it.
    #[test]
    fn a_pid_that_is_no_longer_an_archiver_never_blocks_one_from_starting() {
        // A pid the kernel reissued to another command.
        assert_eq!(
            classify(999, Some("0"), b"/usr/bin/something\0--else\0"),
            Archiver::Absent
        );
        // An empty cmdline: no process under that pid.
        assert_eq!(classify(999, Some("0"), b""), Archiver::Absent);
    }

    /// [`DAEMON_BIN`]'s name shares no text with `sidle`, which `pidof sidle`
    /// matches against the executable behind a process.
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

    /// [`stage_binary`] leaves the old file or the whole new one, and no `.new`.
    #[test]
    fn staging_the_archivers_binary_replaces_it_whole() {
        let dir = tempdir();
        let src = dir.join("sidle");
        let dst = dir.join("bin/readinglogd");
        std::fs::write(&src, b"v1").unwrap();

        stage_binary(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"v1");

        // A copy at `dst` is replaced in full.
        std::fs::write(&src, b"v2-and-longer").unwrap();
        stage_binary(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"v2-and-longer");
        assert!(!dst.with_extension("new").exists(), "scratch left behind");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A build stamp other than this one's gives [`Archiver::Outdated`], which
    /// [`stop_archiver`] takes. Its [`DAEMON_PID`] is what reports one as up.
    #[test]
    fn an_archiver_from_another_build_is_replaced() {
        let cmdline = b"sidle-archive\0--archive-daemon\0";
        // A pidfile carrying no build stamp.
        assert_eq!(classify(42, None, cmdline), Archiver::Outdated(42));
        assert_eq!(classify(42, Some("1"), cmdline), Archiver::Outdated(42));

        let mine = crate::selfupdate::self_build_ts().to_string();
        assert_eq!(classify(42, Some(&mine), cmdline), Archiver::Running);
    }

    /// The [`ARCHIVE_PREFIX`] files under `us_root`, sorted. [`ARCHIVE_MARK`]
    /// sits beside them and is none of them.
    fn archived(us_root: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(us_root.join(ARCHIVE_DIR))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| archive_stamp(n).is_some())
            .collect();
        names.sort();
        names
    }

    /// A unique scratch dir. These tests run on the host, and the crate carries
    /// no tempfile dev-dependency.
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
