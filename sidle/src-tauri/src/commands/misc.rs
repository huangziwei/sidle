//! Tauri commands backing the "Misc." tab — the screenshots + picker logs pulled
//! off the Kindle on Sync (see [`sidle_core::library::device::misc`]). Read-only surface over
//! the on-disk `device-backup/<serial>/{screenshots,logs}/` tree: list them,
//! read a log's text for the in-app viewer, reveal one in Finder.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

use crate::state::AppState;

/// One backed-up misc file, for the Misc tab list.
#[derive(Debug, Serialize)]
pub struct MiscFile {
    /// `"screenshot"` or `"log"` — drives the tab's grid-vs-list rendering.
    pub kind: String,
    /// Filename, e.g. `screenshot_1719430000.png` or `sidle-native.log`.
    pub name: String,
    /// Absolute local path — the frontend feeds it to `convertFileSrc` (images)
    /// or the read/reveal commands below.
    pub path: String,
    pub size: u64,
    /// Local-filesystem mtime as a naive ISO string (`YYYY-MM-DDTHH:MM:SS`), or
    /// `None` if unreadable. Sorts chronologically as a plain string.
    pub modified: Option<String>,
    /// The device serial this file was pulled from (its `<serial>/` dir name).
    pub device: String,
}

/// Filesystem mtime → naive local-wall-clock ISO, matching the shape the device
/// transports produce for `TEntry::modified`.
fn mtime_iso(meta: &std::fs::Metadata) -> Option<String> {
    let t = meta.modified().ok()?;
    Some(
        chrono::DateTime::<chrono::Utc>::from(t)
            .with_timezone(&chrono::Local)
            .naive_local()
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string(),
    )
}

/// List every backed-up screenshot + log across all devices, newest first.
/// Cheap local-fs scan of `device-backup/<serial>/{screenshots,logs}/`.
#[tauri::command]
pub async fn misc_list(state: State<'_, AppState>) -> Result<Vec<MiscFile>, String> {
    let root = state.paths.device_backup_dir();
    let mut out = Vec::new();

    // <serial>/ per device.
    let serial_dirs = match std::fs::read_dir(&root) {
        Ok(rd) => rd,
        // No backups yet → empty tab, not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(format!("read {}: {e}", root.display())),
    };
    for serial_entry in serial_dirs.flatten() {
        if !serial_entry
            .file_type()
            .map(|t| t.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let device = serial_entry.file_name().to_string_lossy().into_owned();
        for (kind, subdir) in [("screenshot", "screenshots"), ("log", "logs")] {
            let dir = serial_entry.path().join(subdir);
            let files = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => continue, // subdir may not exist for this device
            };
            for file in files.flatten() {
                let name = file.file_name().to_string_lossy().into_owned();
                // Skip dotfiles: our backups are `screenshot_*.png` / `*.log`,
                // never hidden, so anything starting with `.` is macOS junk
                // (`.DS_Store`, `._*` resource forks) that Finder dropped here —
                // e.g. after a "Reveal in Finder". Don't surface it as a screenshot.
                if name.starts_with('.') {
                    continue;
                }
                let meta = match file.metadata() {
                    Ok(m) if m.is_file() => m,
                    _ => continue,
                };
                out.push(MiscFile {
                    kind: kind.to_string(),
                    name,
                    path: file.path().to_string_lossy().into_owned(),
                    size: meta.len(),
                    modified: mtime_iso(&meta),
                    device: device.clone(),
                });
            }
        }
    }

    // Newest first (None mtimes sort last). ISO strings compare chronologically.
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(out)
}

/// Cap on how much of a log the in-app viewer loads: logs grow unbounded, and a
/// huge one would freeze the webview. The tail is what matters, so we read the
/// last chunk rather than the first.
const LOG_VIEW_CAP: u64 = 2 * 1024 * 1024;

/// Read a backed-up log's text for the in-app viewer. Files larger than
/// [`LOG_VIEW_CAP`] are tailed (last N bytes) with a truncation banner. Lossy
/// UTF-8 so a stray byte never fails the read.
#[tauri::command]
pub async fn misc_read_text(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let p = guard_in_backup(&state, &path)?;
    let len = std::fs::metadata(&p)
        .map_err(|e| format!("stat {}: {e}", p.display()))?
        .len();
    let mut f = std::fs::File::open(&p).map_err(|e| format!("open {}: {e}", p.display()))?;
    if len > LOG_VIEW_CAP {
        f.seek(SeekFrom::Start(len - LOG_VIEW_CAP))
            .map_err(|e| e.to_string())?;
        let mut buf = Vec::with_capacity(LOG_VIEW_CAP as usize);
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        Ok(format!(
            "… truncated — showing the last {} KB of {} KB …\n\n{}",
            LOG_VIEW_CAP / 1024,
            len / 1024,
            String::from_utf8_lossy(&buf)
        ))
    } else {
        let mut buf = Vec::with_capacity(len as usize);
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

/// Reveal a backed-up file in Finder.
#[tauri::command]
pub async fn misc_reveal(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let p = guard_in_backup(&state, &path)?;
    app.opener()
        .reveal_item_in_dir(p)
        .map_err(|e| e.to_string())
}

/// Delete one backed-up screenshot / log copy. Local only — this removes Sidle's
/// backup, not anything on the Kindle (which the picker already cleared for
/// screenshots on Sync). `NotFound` is treated as success (idempotent).
#[tauri::command]
pub async fn misc_delete(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let p = guard_in_backup(&state, &path)?;
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("delete {}: {e}", p.display())),
    }
}

/// Resolve `path` and confirm it lives inside the `device-backup/` tree, so
/// these commands can't be repurposed to read/reveal arbitrary files. Returns
/// the canonicalized path on success.
fn guard_in_backup(state: &AppState, path: &str) -> Result<PathBuf, String> {
    let base = state
        .paths
        .device_backup_dir()
        .canonicalize()
        .map_err(|e| format!("no device backups yet: {e}"))?;
    let p = Path::new(path)
        .canonicalize()
        .map_err(|e| format!("resolve {path}: {e}"))?;
    if p.starts_with(&base) {
        Ok(p)
    } else {
        Err("path is outside the device-backup directory".to_string())
    }
}
