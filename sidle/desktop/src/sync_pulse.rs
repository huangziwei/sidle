//! Cross-process live-apply after a LAN sync — annotations and books.
//!
//! The standalone `sidle-server` daemon runs as a separate process, so it can't
//! emit a Tauri event into the app. Instead it atomically writes sidecar pulse
//! files at the data-dir root, and this watcher reacts:
//!
//! - `.sync-pulse.json` (`{ts, device_serial, report}`) after a changed
//!   annotation import → re-emit the **same** `annotations:sync-done` event the
//!   USB sync path emits (`device/monitor.rs`); the frontend toasts and repaints
//!   an open reader.
//! - `.book-pulse.json` (`{ts, books:[{id, needs_enqueue}]}`) after a WiFi book
//!   import (`POST /sync/book`) → enqueue the pending `kfx_to_epub` conversion and
//!   re-emit `device:autopull-done` so the shelf refreshes, exactly as the USB
//!   `/dedrm` auto-pull does.
//! - `.reading-pulse.json` (`{ts, device_serial, added, extended, attributed}`)
//!   after a `POST /sync/reading-log` that stored rows → emit
//!   `reading-log:changed`; the Reading Log page refetches.

use std::sync::mpsc;

use notify::{Event, RecursiveMode, Watcher};
use tauri::{AppHandle, Emitter};

use crate::library::LibraryPaths;
use crate::queue::QueueHandle;

/// The annotation sidecar the daemon writes (atomically: temp + rename) on a
/// changed annotation import.
const PULSE_FILE: &str = ".sync-pulse.json";
/// The book sidecar the daemon writes after a WiFi book import (`POST /sync/book`):
/// `{ts, books:[{id, needs_enqueue}]}`. We enqueue the pending conversion and
/// refresh the shelf, mirroring the USB `/dedrm` auto-pull.
const BOOK_PULSE_FILE: &str = ".book-pulse.json";
/// The reading-log sidecar the daemon writes after a push that stored sessions.
const READING_PULSE_FILE: &str = ".reading-pulse.json";

/// Spawn the pulse watcher on a dedicated thread (it blocks on a channel recv, so
/// it can't share the async runtime). Lives for the app's lifetime; a watch setup
/// error just logs and ends the thread — LAN live-repaint stops, but nothing else
/// is affected (reopening the reader still shows the synced rows).
pub fn spawn(app: AppHandle, paths: LibraryPaths, queue: QueueHandle) {
    std::thread::spawn(move || {
        if let Err(e) = run(&app, &paths, &queue) {
            eprintln!("[sidle/sync_pulse] watcher stopped: {e:#}");
        }
    });
}

fn run(app: &AppHandle, paths: &LibraryPaths, queue: &QueueHandle) -> anyhow::Result<()> {
    let root = paths.root.clone();
    let anno_path = root.join(PULSE_FILE);
    let book_path = root.join(BOOK_PULSE_FILE);
    let reading_path = root.join(READING_PULSE_FILE);

    // Watch the parent dir, not the files: the daemon swaps each pulse in by
    // atomic rename, which replaces the inode out from under a file-level watch.
    // NonRecursive — only the pulses (+ pid/log siblings) live at this root.
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(&root, RecursiveMode::NonRecursive)?;

    // Dedup per file: a single write fans out into several FS events; only act
    // when that pulse's `ts` advances. (Re-emit is idempotent anyway — the
    // frontend just refetches — so this only avoids churn.)
    let mut last_anno_ts: Option<String> = None;
    let mut last_book_ts: Option<String> = None;
    let mut last_reading_ts: Option<String> = None;

    for res in rx {
        let Ok(event) = res else { continue };
        // Ignore the `.tmp` writes and the `server.pid`/`server.log` siblings.
        if event.paths.iter().any(|p| p == &anno_path) {
            // A read during the rename window may miss or partial-parse; skip and
            // wait for the settle event rather than emit a half-written report.
            let Ok(bytes) = std::fs::read(&anno_path) else {
                continue;
            };
            let Some((ts, report)) = parse_pulse(&bytes) else {
                continue;
            };
            if ts.is_some() && ts == last_anno_ts {
                continue;
            }
            last_anno_ts = ts;
            // Re-emit the USB-path event + payload so the frontend handler toasts
            // and repaints unchanged (same `DeviceImportReport` shape).
            let _ = app.emit("annotations:sync-done", &report);
        } else if event.paths.iter().any(|p| p == &book_path) {
            let Ok(bytes) = std::fs::read(&book_path) else {
                continue;
            };
            let Some((ts, books)) = parse_book_pulse(&bytes) else {
                continue;
            };
            if ts.is_some() && ts == last_book_ts {
                continue;
            }
            last_book_ts = ts;
            apply_book_pulse(app, queue, &books);
        } else if event.paths.iter().any(|p| p == &reading_path) {
            let Ok(bytes) = std::fs::read(&reading_path) else {
                continue;
            };
            let Some(ts) = parse_reading_pulse(&bytes) else {
                continue;
            };
            if ts.is_some() && ts == last_reading_ts {
                continue;
            }
            last_reading_ts = ts;
            let _ = app.emit("reading-log:changed", ());
        }
    }
    Ok(())
}

/// One imported book from a `.book-pulse.json`.
struct BookEntry {
    id: i64,
    /// The book has a pending `kfx_to_epub` job the app must enqueue (false for a
    /// side that was already on disk).
    needs_enqueue: bool,
}

/// Enqueue each pending conversion, then refresh the shelf by re-emitting the USB
/// auto-pull's `device:autopull-done` (the frontend toasts "N imported" and
/// refetches). Enqueue is fire-and-forget on Tauri's runtime — this watcher runs
/// on a plain thread with no async context of its own.
fn apply_book_pulse(app: &AppHandle, queue: &QueueHandle, books: &[BookEntry]) {
    for b in books {
        if b.needs_enqueue {
            let q = queue.clone();
            let id = b.id;
            tauri::async_runtime::spawn(async move {
                let _ = q.enqueue(id).await;
            });
        }
    }
    let _ = app.emit(
        "device:autopull-done",
        serde_json::json!({ "imported": books.len(), "duplicate": 0, "failed": 0 }),
    );
}

/// Extract `(ts, report)` from a pulse blob — the bits [`run`] emits. Split out so
/// the JSON contract with the daemon's `sidle_server` `write_sync_pulse`
/// (`{ts, device_serial, report}`) is unit-testable: a field-name drift between the
/// writer and this reader would otherwise fail silently (no live repaint). Returns
/// `None` for unparseable bytes or a pulse missing its `report`.
fn parse_pulse(bytes: &[u8]) -> Option<(Option<String>, serde_json::Value)> {
    let pulse: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let ts = pulse.get("ts").and_then(|v| v.as_str()).map(str::to_owned);
    let report = pulse.get("report")?.clone();
    Some((ts, report))
}

/// Extract `ts` from a `.reading-pulse.json` blob. `None` for unparseable bytes;
/// the counts on it are the daemon's own log, and the event carries no payload.
fn parse_reading_pulse(bytes: &[u8]) -> Option<Option<String>> {
    let pulse: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    Some(pulse.get("ts").and_then(|v| v.as_str()).map(str::to_owned))
}

/// Extract `(ts, books)` from a `.book-pulse.json` blob (`{ts, books:[{id,
/// needs_enqueue}]}`). `None` for unparseable bytes or a pulse missing its
/// `books` array; an entry missing `id` is skipped.
fn parse_book_pulse(bytes: &[u8]) -> Option<(Option<String>, Vec<BookEntry>)> {
    let pulse: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let ts = pulse.get("ts").and_then(|v| v.as_str()).map(str::to_owned);
    let books = pulse.get("books")?.as_array()?;
    let entries = books
        .iter()
        .filter_map(|b| {
            Some(BookEntry {
                id: b.get("id")?.as_i64()?,
                needs_enqueue: b
                    .get("needs_enqueue")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })
        })
        .collect();
    Some((ts, entries))
}

#[cfg(test)]
mod tests {
    use super::{parse_book_pulse, parse_pulse, parse_reading_pulse};

    #[test]
    fn parse_pulse_extracts_ts_and_report() {
        // Mirrors `sidle_server::write_sync_pulse`'s shape: {ts, device_serial, report}.
        let blob = br#"{"ts":"2026-05-27T20:00:00Z","device_serial":"G000X",
            "report":{"yjr_books":1,"matched":1,"annotations":{"inserted":2}}}"#;
        let (ts, report) = parse_pulse(blob).expect("valid pulse parses");
        assert_eq!(ts.as_deref(), Some("2026-05-27T20:00:00Z"));
        // The emitted payload carries the report the frontend handler reads.
        assert_eq!(report["annotations"]["inserted"], 2);
    }

    #[test]
    fn parse_reading_pulse_extracts_ts() {
        // Mirrors `sidle_server::write_reading_pulse`.
        let blob = br#"{"ts":"2026-08-30T09:00:00Z","device_serial":"G000X",
            "added":3,"extended":1,"attributed":4}"#;
        let ts = parse_reading_pulse(blob).expect("valid pulse parses");
        assert_eq!(ts.as_deref(), Some("2026-08-30T09:00:00Z"));
    }

    #[test]
    fn parse_reading_pulse_rejects_garbage() {
        assert!(parse_reading_pulse(b"not json at all").is_none());
    }

    #[test]
    fn parse_pulse_rejects_garbage_and_missing_report() {
        assert!(parse_pulse(b"not json at all").is_none());
        assert!(parse_pulse(br#"{"ts":"x","device_serial":"y"}"#).is_none());
    }

    #[test]
    fn parse_book_pulse_extracts_ts_and_books() {
        // Mirrors `sidle_server::write_book_pulse`'s shape: {ts, books:[…]}.
        let blob = br#"{"ts":"2026-07-05T10:00:00Z",
            "books":[{"id":42,"needs_enqueue":true}]}"#;
        let (ts, books) = parse_book_pulse(blob).expect("valid book pulse parses");
        assert_eq!(ts.as_deref(), Some("2026-07-05T10:00:00Z"));
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].id, 42);
        assert!(books[0].needs_enqueue);
    }

    #[test]
    fn parse_book_pulse_rejects_garbage_and_missing_books() {
        assert!(parse_book_pulse(b"not json").is_none());
        assert!(parse_book_pulse(br#"{"ts":"x"}"#).is_none());
        // An entry missing `id` is skipped, not fatal.
        let (_, books) =
            parse_book_pulse(br#"{"ts":"x","books":[{"needs_enqueue":true}]}"#).unwrap();
        assert!(books.is_empty());
    }
}
