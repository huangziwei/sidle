//! Cross-process live-repaint after a LAN annotation sync.
//!
//! The standalone `sidle-server` daemon ingests a Kindle's pushed `.yjr` and, on a
//! changed import, atomically writes `<data-dir>/.sync-pulse.json` (`{ts,
//! device_serial, report}`). It runs as a separate process, so it can't emit a
//! Tauri event into the app — instead we watch that file and re-emit the **same**
//! `annotations:sync-done` event the USB sync path emits (`device/monitor.rs`). The
//! frontend's existing handler (`web/library.js`) then toasts "Synced N…" and
//! repaints an open reader in place. See sidle-reader.md P3.

use std::sync::mpsc;

use notify::{Event, RecursiveMode, Watcher};
use tauri::{AppHandle, Emitter};

use crate::library::LibraryPaths;

/// The sidecar the daemon writes (atomically: temp + rename) on a changed import.
const PULSE_FILE: &str = ".sync-pulse.json";

/// Spawn the pulse watcher on a dedicated thread (it blocks on a channel recv, so
/// it can't share the async runtime). Lives for the app's lifetime; a watch setup
/// error just logs and ends the thread — LAN live-repaint stops, but nothing else
/// is affected (reopening the reader still shows the synced rows).
pub fn spawn(app: AppHandle, paths: LibraryPaths) {
    std::thread::spawn(move || {
        if let Err(e) = run(&app, &paths) {
            eprintln!("[sidle/sync_pulse] watcher stopped: {e:#}");
        }
    });
}

fn run(app: &AppHandle, paths: &LibraryPaths) -> anyhow::Result<()> {
    let root = paths.root.clone();
    let pulse_path = root.join(PULSE_FILE);

    // Watch the parent dir, not the file itself: the daemon swaps the pulse in by
    // atomic rename, which replaces the inode out from under a file-level watch.
    // NonRecursive — only the pulse (+ pid/log siblings) live at this root.
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(&root, RecursiveMode::NonRecursive)?;

    // Dedup: a single write fans out into several FS events; only act when the
    // pulse's `ts` advances. (Re-emit is idempotent anyway — `reloadAnnotations`
    // just refetches — so this only avoids churn.)
    let mut last_ts: Option<String> = None;

    for res in rx {
        let Ok(event) = res else { continue };
        // Ignore the `.tmp` write and the `server.pid`/`server.log` siblings.
        if !event.paths.iter().any(|p| p == &pulse_path) {
            continue;
        }
        // A read during the rename window may miss or partial-parse; skip and wait
        // for the settle event rather than emit a half-written report.
        let Ok(bytes) = std::fs::read(&pulse_path) else {
            continue;
        };
        let Some((ts, report)) = parse_pulse(&bytes) else {
            continue;
        };

        if ts.is_some() && ts == last_ts {
            continue;
        }
        last_ts = ts;

        // Re-emit the USB-path event + payload so the frontend handler toasts and
        // repaints unchanged. `report` is the same `DeviceImportReport` shape the
        // USB sync emits.
        let _ = app.emit("annotations:sync-done", &report);
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::parse_pulse;

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
    fn parse_pulse_rejects_garbage_and_missing_report() {
        assert!(parse_pulse(b"not json at all").is_none());
        assert!(parse_pulse(br#"{"ts":"x","device_serial":"y"}"#).is_none());
    }
}
