//! Tauri commands backing the Files tab — what a Sync brings off the Kindle

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;
use sidle_core::library::device_backup::{self, SyncCollections};
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

use crate::state::AppState;

/// One backed-up file, for the Files tab list.
#[derive(Debug, Serialize)]
pub struct MiscFile {
    /// Which collection's folder it came off the device in.
    pub collection: String,
    /// Path relative to that collection's dir — a bare filename
    /// (`screenshot_1719430000.png`) or a nested one (`2026/draft.md`).
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

/// A group heading in the Files tab.
#[derive(Debug, Serialize)]
pub struct MiscGroup {
    pub id: String,
    pub label: String,
}

/// Everything the Files tab renders: the groups, in the order the config lists
/// them, and every file across every device.
#[derive(Debug, Serialize)]
pub struct MiscListing {
    pub groups: Vec<MiscGroup>,
    pub files: Vec<MiscFile>,
}

/// How deep the local scan descends. Matches the device-side walk's cap, so a
/// file that could be backed up can always be listed.
const MAX_DEPTH: usize = 5;

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

/// List every backed-up file across all devices, newest first, plus the groups
/// to render them under. Cheap local-fs scan of
/// `device-backup/<serial>/<collection>/`.
#[tauri::command]
pub async fn misc_list(state: State<'_, AppState>) -> Result<MiscListing, String> {
    let config = SyncCollections::load(&state.paths).unwrap_or(SyncCollections {
        collections: Vec::new(),
    });
    let root = state.paths.device_backup_dir();
    let mut files = Vec::new();
    let mut seen_ids = Vec::new();

    // <serial>/ per device.
    let serial_dirs = match std::fs::read_dir(&root) {
        Ok(rd) => rd,
        // No backups yet → empty tab, not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MiscListing {
                groups: Vec::new(),
                files,
            });
        }
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
        let collection_dirs = match std::fs::read_dir(serial_entry.path()) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for collection_entry in collection_dirs.flatten() {
            if !collection_entry
                .file_type()
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let id = collection_entry.file_name().to_string_lossy().into_owned();
            if id.starts_with('.') {
                continue;
            }
            if !seen_ids.contains(&id) {
                seen_ids.push(id.clone());
            }
            collect(&collection_entry.path(), "", &id, &device, &mut files, 0);
        }
    }

    // Newest first (None mtimes sort last). ISO strings compare chronologically.
    files.sort_by(|a, b| b.modified.cmp(&a.modified));

    // Configured collections first, in the order the user put them in; then any
    // leftover dir whose collection is gone.
    let mut groups: Vec<MiscGroup> = config
        .collections
        .iter()
        .map(|c| MiscGroup {
            id: c.id.clone(),
            label: c.label.clone(),
        })
        .collect();
    for id in seen_ids {
        if config.get(&id).is_none() {
            groups.push(MiscGroup {
                id: id.clone(),
                label: id,
            });
        }
    }
    Ok(MiscListing { groups, files })
}

/// Walk one collection dir, keeping each file's path relative to it.
fn collect(
    dir: &Path,
    rel: &str,
    collection: &str,
    device: &str,
    out: &mut Vec<MiscFile>,
    depth: usize,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skip dotfiles: our backups are never hidden, so anything starting with
        // `.` is macOS junk (`.DS_Store`, `._*` resource forks) that Finder
        // dropped here — e.g. after a "Reveal in Finder".
        if name.starts_with('.') {
            continue;
        }
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if depth + 1 < MAX_DEPTH {
                collect(
                    &entry.path(),
                    &child_rel,
                    collection,
                    device,
                    out,
                    depth + 1,
                );
            }
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        out.push(MiscFile {
            collection: collection.to_string(),
            name: child_rel,
            path: entry.path().to_string_lossy().into_owned(),
            size: meta.len(),
            modified: mtime_iso(&meta),
            device: device.to_string(),
        });
    }
}

/// The library's sync-collection config, for the settings editor.
#[tauri::command]
pub async fn misc_collections_get(state: State<'_, AppState>) -> Result<SyncCollections, String> {
    SyncCollections::load(&state.paths).map_err(|e| e.to_string())
}

/// Replace the library's sync-collection config. Takes effect on the next Sync:
/// the picker fetches this list before it scans, and the desktop's own USB pull
/// reads it directly.
#[tauri::command]
pub async fn misc_collections_set(
    state: State<'_, AppState>,
    config: SyncCollections,
    renames: Option<Vec<(String, String)>>,
) -> Result<SyncCollections, String> {
    let mut failed = Vec::new();
    for (old, new) in renames.unwrap_or_default() {
        if let Err(e) = device_backup::rename_collection_storage(&state.paths, &old, &new) {
            failed.push(format!("{old} → {new}: {e}"));
        }
    }
    config.save(&state.paths).map_err(|e| e.to_string())?;
    if !failed.is_empty() {
        return Err(format!(
            "saved, but these folders' existing files could not be moved — {}",
            failed.join("; ")
        ));
    }
    // Return what actually landed — `save` normalizes ids and drops any that
    // reduce to nothing, and the editor should show the result, not the input.
    SyncCollections::load(&state.paths).map_err(|e| e.to_string())
}

/// Cap on how much of a file the in-app viewer loads: a log grows unbounded, and
/// a huge one would freeze the webview. The tail is what matters for a log, so
/// we read the last chunk rather than the first.
const LOG_VIEW_CAP: u64 = 2 * 1024 * 1024;

/// Read a backed-up file's text for the in-app viewer. Files larger than
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

/// Delete one backed-up copy. Local only — this removes Sidle's backup, not
/// anything on the Kindle (which the picker already cleared for the collections
/// configured that way). `NotFound` is treated as success (idempotent).
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
