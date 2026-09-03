//! Tauri commands for library operations.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sidle_core::library::{cover, cover_fetch, export, metadata, progress};
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::oneshot;

use crate::library::db::{self, BookRow};
use crate::library::import::{self, ImportOutcome};
use crate::library::{LibraryPaths, backup, merge, relocate};
use crate::state::AppState;

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImportResult {
    /// `needs_enqueue` is true when the row was inserted with a pending job. False on
    /// an idempotent re-import where the other side was already on disk.
    Imported {
        book: BookRow,
        needs_enqueue: bool,
    },
    Duplicate {
        book: BookRow,
    },
    Failed {
        path: String,
        error: String,
    },
}

#[tauri::command]
pub async fn library_list(state: State<'_, AppState>) -> Result<Vec<BookRow>, String> {
    let conn = state.db.lock().await;
    db::list_books(&conn).map_err(|e| e.to_string())
}

/// A live tick from an import in flight.
#[derive(Clone, Serialize)]
struct ImportProgress<'a> {
    path: &'a str,
    index: usize,
    total: usize,
    fraction: f32,
    label: &'a str,
}

fn emit_import_progress(
    app: &AppHandle,
    path: &str,
    index: usize,
    total: usize,
    fraction: f32,
    label: &str,
) {
    let _ = app.emit(
        "library:import-progress",
        ImportProgress {
            path,
            index,
            total,
            fraction,
            label,
        },
    );
}

#[tauri::command]
pub async fn library_import(
    app: AppHandle,
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> Result<Vec<ImportResult>, String> {
    let file_count = paths.len();
    let mut out = Vec::with_capacity(file_count);

    for (index, raw) in paths.into_iter().enumerate() {
        let path = PathBuf::from(&raw);
        let db_handle = state.db.clone();
        let paths_handle = state.paths.clone();
        let raw_for_err = raw.clone();
        let app_handle = app.clone();

        // Three phases: two short-lived under the database lock, and the conversion —
        // minutes on a big book — outside it, so it stalls no reader.
        let result = tokio::task::spawn_blocking(move || {
            let kind = import::detect_kind(&path)?;
            let identity = import::identify_file(&path)?;
            {
                let conn = db_handle.blocking_lock();
                if let Some(existing) = db::find_by_sha(&conn, &identity.0)? {
                    return Ok(ImportOutcome::Duplicate(existing));
                }
            }

            // Every file opens with a tick, whatever its format: it names the file being
            // worked on and clears what the file before it left on the bar.
            emit_import_progress(&app_handle, &raw, index, file_count, 0.0, "");

            let pipeline = progress::import_pipeline(kind);
            let throttle = progress::Throttle::new();
            let on_progress = |phase: &str, cur: usize, total: usize, label: &str| {
                // The formats stored as they arrive land too fast for the steps
                // to be worth naming; their opening tick is the whole report.
                let Some(pipeline) = pipeline else { return };
                let fraction = progress::fraction(pipeline, phase, cur, total);
                if throttle.worth_emitting(fraction) {
                    emit_import_progress(&app_handle, &raw, index, file_count, fraction, label);
                }
            };
            let staged = import::stage_file(&paths_handle, &path, identity, &on_progress)?;

            let conn = db_handle.blocking_lock();
            import::record(&conn, staged)
        })
        .await
        .map_err(|e| e.to_string())?;

        match result {
            Ok(ImportOutcome::Imported {
                book,
                needs_enqueue,
            }) => {
                let book_id = book.id;
                if needs_enqueue {
                    let _ = state.queue.enqueue(book_id).await;
                }
                out.push(ImportResult::Imported {
                    book,
                    needs_enqueue,
                });
            }
            Ok(ImportOutcome::Duplicate(book)) => {
                out.push(ImportResult::Duplicate { book });
            }
            Err(e) => out.push(ImportResult::Failed {
                path: raw_for_err,
                error: format!("{e:#}"),
            }),
        }
    }

    Ok(out)
}

/// Edit the metadata for one book. The editor modal always submits every
/// field (it starts from the current row, the user edits in place), so the
/// patch is a full replacement — no "no-op" semantics to manage.
#[tauri::command]
pub async fn library_update_metadata(
    app: AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
    patch: db::MetadataPatch,
) -> Result<BookRow, String> {
    apply_metadata_patch(&app, &state, book_id, patch).await
}

/// Canonicalize, validate, persist, and file-rename a full metadata patch, then
pub(crate) async fn apply_metadata_patch(
    app: &AppHandle,
    state: &AppState,
    book_id: i64,
    patch: db::MetadataPatch,
) -> Result<BookRow, String> {
    let updated = {
        let conn = state.db.lock().await;
        metadata::apply(&conn, &state.paths, book_id, patch).map_err(|e| format!("{e:#}"))?
    };
    let _ = app.emit("library:row-updated", &updated);
    Ok(updated)
}

/// Render romaji for a piece of text — backs the metadata editor's "regenerate"
/// (`↻`) buttons. Engine-only (no yomi available client-side); the user reviews
/// and corrects the result before saving.
#[tauri::command]
pub fn library_romanize(text: String, language: String) -> String {
    crate::library::romaji::romanize_field(&text, None, &language)
}

/// Set — or clear — a book's Amazon catalogue id, the colour-cover key.
#[tauri::command]
pub async fn library_set_asin(
    app: AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
    asin: String,
) -> Result<BookRow, String> {
    let updated = {
        let conn = state.db.lock().await;
        metadata::set_amazon_asin(&conn, book_id, Some(&asin)).map_err(|e| format!("{e:#}"))?
    };
    let _ = app.emit("library:row-updated", &updated);
    Ok(updated)
}

/// Open the user's browser to an Amazon search for this book, so they can find
#[tauri::command]
pub async fn library_amazon_search(
    app: AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<(), String> {
    let book = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id).map_err(|e| e.to_string())?
    };
    let Some(book) = book else {
        return Err("book not found".into());
    };
    let domain = cover_fetch::amazon_search_domain(&book.language);
    let mut query = book.title.clone();
    if !book.author.is_empty() {
        query.push(' ');
        query.push_str(&book.author);
    }
    let url = format!(
        "https://www.{domain}/s?k={}&i=digital-text",
        percent_encode_query(query.trim())
    );
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// Minimal `application/x-www-form-urlencoded` encoding for a search query.
/// Spaces → `+`, RFC 3986 unreserved pass through, everything else per byte.
fn percent_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Bulk-edit metadata across many books. Sparse semantics: only the fields the
/// user filled in change; tags are additive. See [`db::BulkMetadataPatch`].
#[tauri::command]
pub async fn library_bulk_update_metadata(
    state: State<'_, AppState>,
    book_ids: Vec<i64>,
    patch: db::BulkMetadataPatch,
) -> Result<Vec<BookRow>, String> {
    let conn = state.db.lock().await;
    metadata::apply_bulk(&conn, &book_ids, patch).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn library_remove(state: State<'_, AppState>, book_id: i64) -> Result<(), String> {
    // Look up the sha first so we can delete the files BEFORE the row.
    let sha: Option<String> = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .map(|b| b.sha256)
    };
    let Some(sha) = sha else {
        // Already absent — treat as success so a double-click doesn't error.
        return Ok(());
    };

    state
        .paths
        .remove_sha(&sha)
        .map_err(|e| format!("could not remove files for {sha}: {e}"))?;

    let conn = state.db.lock().await;
    db::remove_book(&conn, book_id).map_err(|e| e.to_string())?;
    Ok(())
}

/// Compact the library DB file (`VACUUM`), reclaiming the disk space freed by
#[tauri::command]
pub async fn library_compact(state: State<'_, AppState>) -> Result<(), String> {
    let conn = state.db.lock().await;
    db::vacuum(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn library_open_in_finder(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<(), String> {
    let path = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .and_then(|b| {
                // Sidle is a KFX reader: the `.kfx` is the canonical book file;
                // the `.epub` is a derived artifact the user may have deleted.
                [b.kfx_path, b.epub_path]
                    .into_iter()
                    .flatten()
                    .find(|p| std::path::Path::new(p).exists())
            })
    };
    let Some(path) = path else {
        return Err("no file on disk for this book".into());
    };
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|e| e.to_string())
}

/// Write the chosen `format` of every book in `book_ids` directly into
/// `dest_dir` as a flat folder of files (`<dest>/<filename>`, no per-author
/// subfolders). See [`export::export_books`] for the per-format rules.
#[tauri::command]
pub async fn library_export_books(
    state: State<'_, AppState>,
    book_ids: Vec<i64>,
    format: String,
    dest_dir: String,
) -> Result<export::Summary, String> {
    let format = export::Format::parse(&format).map_err(|e| e.to_string())?;
    let db = state.db.clone();
    // KFX decode + IR walk (the `txt` route) is CPU-heavy; keep it off the async
    // runtime.
    tokio::task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        export::export_books(&conn, &book_ids, format, Path::new(&dest_dir))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn library_cover_path(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<Option<String>, String> {
    let conn = state.db.lock().await;
    Ok(db::get_book(&conn, book_id)
        .map_err(|e| e.to_string())?
        .and_then(|b| b.cover_path))
}

/// Outcome of a per-book "Re-fetch cover" action. The frontend uses the tag
/// to pick the right toast.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CoverResult {
    Updated { cover_path: String },
    NoAsin,
    Failed { error: String },
}

impl From<cover::Outcome> for CoverResult {
    fn from(outcome: cover::Outcome) -> Self {
        match outcome {
            cover::Outcome::Updated { cover_path } => CoverResult::Updated { cover_path },
            cover::Outcome::NoAsin => CoverResult::NoAsin,
            cover::Outcome::Failed { error } => CoverResult::Failed { error },
        }
    }
}

/// Re-pull the color cover for one book by hitting Amazon's `/images/P/`
/// endpoint with its ASIN. Same fetch path the import-time enrichment uses;
/// this command is just the manual trigger from the right-click menu.
#[tauri::command]
pub async fn library_recrawl_cover(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<CoverResult, String> {
    let db = state.db.clone();
    let paths = state.paths.clone();
    // One Amazon round-trip plus an EPUB/KFX rewrite: blocking IO and CPU both.
    tokio::task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        let book = db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "book not found".to_string())?;
        Ok(cover::refetch(&conn, &paths, &book).into())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Per-book progress for a bulk cover re-fetch, emitted as
#[derive(Clone, Serialize)]
struct RecrawlProgress {
    done: usize,
    total: usize,
}

/// Tally returned by `library_recrawl_covers`. `updated` got a fresh cover;
/// `failed` returned no usable cover (404 / placeholder / network); `no_asin`
/// had no real Amazon ASIN to fetch from and was skipped.
#[derive(Debug, Default, Serialize)]
pub struct RecrawlBulkSummary {
    updated: usize,
    failed: usize,
    no_asin: usize,
}

/// Re-fetch covers for a set of books — the selection bar's "Re-fetch covers"
/// action (and the multi-select / whole-series context menus). Sequential;
#[tauri::command]
pub async fn library_recrawl_covers(
    app: AppHandle,
    state: State<'_, AppState>,
    book_ids: Vec<i64>,
) -> Result<RecrawlBulkSummary, String> {
    let db = state.db.clone();
    let paths = state.paths.clone();
    tokio::task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        let total = book_ids.len();
        let mut summary = RecrawlBulkSummary::default();
        for (i, id) in book_ids.iter().enumerate() {
            match db::get_book(&conn, *id).map_err(|e| e.to_string())? {
                Some(book) => match cover::refetch(&conn, &paths, &book) {
                    cover::Outcome::Updated { .. } => summary.updated += 1,
                    cover::Outcome::NoAsin => summary.no_asin += 1,
                    cover::Outcome::Failed { error } => {
                        summary.failed += 1;
                        eprintln!("[sidle/recrawl] book {} failed: {error}", book.id);
                    }
                },
                // Row vanished mid-run (e.g. removed by a parallel action).
                // Count it as failed and keep going rather than aborting.
                None => summary.failed += 1,
            }
            let _ = app.emit(
                "library:recrawl-progress",
                RecrawlProgress { done: i + 1, total },
            );
        }
        Ok(summary)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Replace the cover for one book from a user-picked image file.
#[tauri::command]
pub async fn library_set_cover(
    app: AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
    src_path: String,
) -> Result<CoverResult, String> {
    let db = state.db.clone();
    let paths = state.paths.clone();
    let (outcome, updated) = tokio::task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        let book = db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "book not found".to_string())?;
        let outcome = cover::set_from_file(&conn, &paths, &book, Path::new(&src_path));
        let updated = db::get_book(&conn, book_id).map_err(|e| e.to_string())?;
        Ok::<_, String>((outcome, updated))
    })
    .await
    .map_err(|e| e.to_string())??;

    if let Some(updated) = updated {
        let _ = app.emit("library:row-updated", &updated);
    }
    Ok(outcome.into())
}

/// Open the system file dialog filtered to images and return one path.
#[tauri::command]
pub async fn library_pick_image(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = oneshot::channel();
    app.dialog()
        .file()
        .add_filter("Images", &["jpg", "jpeg", "png", "webp"])
        .pick_file(move |path| {
            let _ = tx.send(path);
        });
    let result = rx.await.map_err(|e| e.to_string())?;
    Ok(result.map(|p| p.to_string()))
}

/// Open the system file dialog and return selected ebook paths.
#[tauri::command]
pub async fn library_pick_files(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    let (tx, rx) = oneshot::channel();
    app.dialog()
        .file()
        .add_filter(
            "Ebooks",
            &["epub", "kfx", "kfx-zip", "mobi", "pobi", "azw3", "pdf"],
        )
        .pick_files(move |paths| {
            let _ = tx.send(paths);
        });
    let result = rx.await.map_err(|e| e.to_string())?;
    Ok(result
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.to_string())
        .collect())
}

/// Where the library currently lives, and whether that's the default location.
/// Shown in the Settings (⚙) panel.
#[derive(Debug, Serialize)]
pub struct LibraryLocation {
    pub root: String,
    pub is_default: bool,
}

#[tauri::command]
pub async fn library_location(state: State<'_, AppState>) -> Result<LibraryLocation, String> {
    let default = LibraryPaths::default_root()
        .map_err(|e| e.to_string())?
        .root;
    Ok(LibraryLocation {
        is_default: state.paths.root == default,
        root: state.paths.root.to_string_lossy().to_string(),
    })
}

/// Open a folder picker; returns the chosen directory, or `None` if cancelled.
/// Backs both Settings relocate actions. Exposed from Rust for the same reason
/// as `library_pick_files` — vanilla JS can't import the dialog plugin module.
#[tauri::command]
pub async fn library_pick_folder(app: AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = oneshot::channel();
    app.dialog().file().pick_folder(move |path| {
        let _ = tx.send(path);
    });
    let result = rx.await.map_err(|e| e.to_string())?;
    Ok(result.map(|p| p.to_string()))
}

/// Move the library to `dest`: snapshot + verify the DB there, relocate every
#[tauri::command]
pub async fn library_relocate_move(
    app: AppHandle,
    state: State<'_, AppState>,
    dest: String,
) -> Result<(), String> {
    let dest = PathBuf::from(dest);
    if dest == state.paths.root {
        return Err("That's already the current library location.".into());
    }
    // The default location *is* the app state dir, which always holds
    // `config.json` (the root pointer) and so can never be literally empty.
    let state_dir = LibraryPaths::state_dir().map_err(|e| e.to_string())?;
    if dest == state_dir {
        if dest.join("library.db").exists() || dest.join("books").is_dir() {
            return Err(format!(
                "{} already contains a library — pick a new or empty folder.",
                dest.display()
            ));
        }
    } else if dir_has_entries(&dest) {
        return Err(format!(
            "{} is not empty — pick a new or empty folder.",
            dest.display()
        ));
    }
    let src_root = state.paths.root.clone();
    let copied = {
        let conn = state.db.lock().await;
        // Gate on an idle queue: a conversion finishing mid-move would write its
        // output into the old root and be stranded. The relaunch afterwards is
        // why no queue *pause* is needed.
        if !db::pending_or_error_book_ids(&conn)
            .map_err(|e| e.to_string())?
            .is_empty()
        {
            return Err(
                "A conversion is still running — wait for the queue to finish, then try again."
                    .into(),
            );
        }
        relocate::move_library(&conn, &src_root, &dest).map_err(|e| format!("{e:#}"))?
    };
    // Repoint FIRST, then delete the old remnants — so the destructive cleanup
    // runs only once the new root is the live one; `finish_move` preserves the
    // state dir's `config.json` when the old root was the default location.
    LibraryPaths::set_root(&dest).map_err(|e| format!("{e:#}"))?;
    relocate::finish_move(&src_root, &state_dir, &copied);
    // Relaunch onto the new root; `restart` diverges, so nothing runs after it.
    app.restart()
}

/// Adopt a library that already exists at `dir`: validate it, repoint, relaunch.
/// Copies nothing — for a library moved by hand, or one on an external drive.
#[tauri::command]
pub async fn library_relocate_use(
    app: AppHandle,
    state: State<'_, AppState>,
    dir: String,
) -> Result<(), String> {
    let dir = PathBuf::from(dir);
    if dir == state.paths.root {
        return Err("That's already the current library location.".into());
    }
    relocate::validate_existing(&dir).map_err(|e| format!("{e:#}"))?;
    LibraryPaths::set_root(&dir).map_err(|e| format!("{e:#}"))?;
    app.restart()
}

/// Summary of a completed backup, shown in the Settings panel.
#[derive(Debug, Serialize)]
pub struct BackupSummary {
    pub books: i64,
    pub annotations: i64,
    pub path: String,
}

/// Pick a destination for a backup archive (save dialog). Defaults the name to
/// `sidle-library-<date>.sidlebak`. Returns the chosen path, or `None` if
/// cancelled. Exposed from Rust for the same reason as `library_pick_files`.
#[tauri::command]
pub async fn library_backup_pick_dest(app: AppHandle) -> Result<Option<String>, String> {
    let default_name = format!(
        "sidle-library-{}.sidlebak",
        chrono::Local::now().format("%Y-%m-%d")
    );
    let (tx, rx) = oneshot::channel();
    app.dialog()
        .file()
        .add_filter("Sidle backup", &["sidlebak"])
        .set_file_name(default_name)
        .save_file(move |path| {
            let _ = tx.send(path);
        });
    let result = rx.await.map_err(|e| e.to_string())?;
    Ok(result.map(|p| p.to_string()))
}

/// Per-unit progress for a long file operation, emitted as
/// `library:fileop-progress`. `done`/`total` count archived dirs or zip entries.
#[derive(Clone, Serialize)]
struct FileopProgress {
    op: &'static str,
    done: u64,
    total: u64,
}

/// Write a full backup of the current library to `dest`. Holds the DB lock only
/// for the snapshot, then zips `books/` lock-free. Non-destructive.
#[tauri::command]
pub async fn library_backup(
    app: AppHandle,
    state: State<'_, AppState>,
    dest: String,
) -> Result<BackupSummary, String> {
    let dest = PathBuf::from(dest);
    let dest_label = dest.to_string_lossy().to_string();
    let books_dir = state.paths.root.join("books");
    let source_root = state.paths.root.clone();

    // Snapshot under the lock (fast — just the metadata DB), then release.
    let (snapshot, db_user_version) = {
        let conn = state.db.lock().await;
        let snap = backup::snapshot(&conn).map_err(|e| format!("{e:#}"))?;
        let uv = db::user_version(&conn).map_err(|e| e.to_string())?;
        (snap, uv)
    };

    // Zip on a blocking thread: lock-free and off the async runtime, so a large
    // backup stalls no command and freezes no UI. The snapshot guard cleans up there.
    let manifest = tokio::task::spawn_blocking(move || {
        // Cap IPC chatter: emit only when the integer percentage changes (plus
        // the final tick). The book/notebook loop can run into the hundreds, and
        // restore/merge extract thousands of zip entries.
        let last_pct = std::cell::Cell::new(-1i32);
        let on_progress = |done: u64, total: u64| {
            let pct = (done * 100).checked_div(total).unwrap_or(0) as i32;
            if pct != last_pct.get() || done >= total {
                last_pct.set(pct);
                let _ = app.emit(
                    "library:fileop-progress",
                    FileopProgress {
                        op: "backup",
                        done,
                        total,
                    },
                );
            }
        };
        backup::create_archive_with_progress(
            &snapshot,
            &books_dir,
            &source_root,
            env!("CARGO_PKG_VERSION"),
            db_user_version,
            &dest,
            &on_progress,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))?;

    Ok(BackupSummary {
        books: manifest.counts.books,
        annotations: manifest.counts.annotations,
        path: dest_label,
    })
}

/// Pick a `.sidlebak` to restore (open dialog). Returns the path, or `None`.
#[tauri::command]
pub async fn library_restore_pick_src(app: AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = oneshot::channel();
    app.dialog()
        .file()
        .add_filter("Sidle backup", &["sidlebak"])
        .pick_file(move |path| {
            let _ = tx.send(path);
        });
    let result = rx.await.map_err(|e| e.to_string())?;
    Ok(result.map(|p| p.to_string()))
}

/// Restore a `.sidlebak` over the current library, then relaunch.
#[tauri::command]
pub async fn library_restore(
    app: AppHandle,
    state: State<'_, AppState>,
    src: String,
    keep_previous: bool,
) -> Result<(), String> {
    let src = PathBuf::from(src);
    let dest_root = state.paths.root.clone();

    // Gate on an idle queue before mutating anything (same reasoning as
    // relocate). Lock only for the check.
    {
        let conn = state.db.lock().await;
        if !db::pending_or_error_book_ids(&conn)
            .map_err(|e| e.to_string())?
            .is_empty()
        {
            return Err(
                "A conversion is still running — wait for the queue to finish, then try again."
                    .into(),
            );
        }
    }

    // Extraction + verify + swap. No live connection needed; the open DB handle
    // keeps working off the old (now set-aside) inode until we relaunch.
    let app_progress = app.clone();
    let last_pct = std::cell::Cell::new(-1i32);
    let on_progress = |done: u64, total: u64| {
        let pct = (done * 100).checked_div(total).unwrap_or(0) as i32;
        if pct != last_pct.get() || done >= total {
            last_pct.set(pct);
            let _ = app_progress.emit(
                "library:fileop-progress",
                FileopProgress {
                    op: "restore",
                    done,
                    total,
                },
            );
        }
    };
    let previous = if keep_previous {
        backup::PreviousLibrary::Keep
    } else {
        backup::PreviousLibrary::Discard
    };
    let outcome =
        backup::restore_with_progress(&src, &dest_root, db::SCHEMA_VERSION, previous, &on_progress)
            .map_err(|e| format!("{e:#}"))?;
    // The third case is the one worth logging loudly: the user asked for the
    // space back and did not get it, which no other surface will tell them
    // (the app restarts immediately after).
    match (&outcome.safety_copy, keep_previous) {
        (Some(p), true) => eprintln!(
            "[sidle/backup] restored {} books; previous library kept at {}",
            outcome.books,
            p.display()
        ),
        (Some(p), false) => eprintln!(
            "[sidle/backup] restored {} books; could NOT remove the previous library, left at {}",
            outcome.books,
            p.display()
        ),
        (None, _) => eprintln!(
            "[sidle/backup] restored {} books; previous library removed",
            outcome.books
        ),
    }

    // Relaunch onto the restored library; `restart` diverges.
    app.restart()
}

/// What a merge brought in, surfaced to the UI.
#[derive(Debug, Serialize)]
pub struct MergeSummary {
    pub books_added: i64,
    pub books_updated: i64,
    pub annotations_added: i64,
    pub ink_added: i64,
    pub notebooks_added: i64,
    pub path: String,
}

/// Pick a `.sidlebak` to merge (open dialog). Returns the path, or `None`.
#[tauri::command]
pub async fn library_merge_pick_src(app: AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = oneshot::channel();
    app.dialog()
        .file()
        .add_filter("Sidle backup", &["sidlebak"])
        .pick_file(move |path| {
            let _ = tx.send(path);
        });
    let result = rx.await.map_err(|e| e.to_string())?;
    Ok(result.map(|p| p.to_string()))
}

/// Merge a `.sidlebak`'s books, annotations, ink, and notebooks into the current
#[tauri::command]
pub async fn library_merge(
    app: AppHandle,
    state: State<'_, AppState>,
    src: String,
) -> Result<MergeSummary, String> {
    let src = PathBuf::from(src);
    let src_label = src.to_string_lossy().to_string();
    let dest_root = state.paths.root.clone();

    // Validate → extract → copy new files, off the runtime and lock-free (mirrors
    // backup's snapshot/zip split). Yields the in-memory inventory to apply.
    let prepared = tokio::task::spawn_blocking(move || {
        let last_pct = std::cell::Cell::new(-1i32);
        let on_progress = |done: u64, total: u64| {
            let pct = (done * 100).checked_div(total).unwrap_or(0) as i32;
            if pct != last_pct.get() || done >= total {
                last_pct.set(pct);
                let _ = app.emit(
                    "library:fileop-progress",
                    FileopProgress {
                        op: "merge",
                        done,
                        total,
                    },
                );
            }
        };
        merge::prepare_with_progress(&src, &dest_root, db::SCHEMA_VERSION, &on_progress)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))?;

    // Apply the rows in one transaction under the lock (fast — metadata only).
    let outcome = {
        let conn = state.db.lock().await;
        merge::commit(&conn, &prepared).map_err(|e| format!("{e:#}"))?
    };

    Ok(MergeSummary {
        books_added: outcome.books_added,
        books_updated: outcome.books_updated,
        annotations_added: outcome.annotations_added,
        ink_added: outcome.ink_added,
        notebooks_added: outcome.notebooks_added,
        path: src_label,
    })
}

/// True if `dir` exists and holds at least one entry. A non-existent dir is
/// fine (we create it); a non-empty one we refuse, to avoid clobbering.
fn dir_has_entries(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}
#[cfg(test)]
mod tests {
    use super::percent_encode_query;

    #[test]
    fn percent_encode_query_handles_spaces_and_cjk() {
        // Spaces become '+'; unreserved chars pass through.
        assert_eq!(
            percent_encode_query("Foundation Asimov"),
            "Foundation+Asimov"
        );
        assert_eq!(percent_encode_query("a-b_c.d~e"), "a-b_c.d~e");
        // CJK is percent-encoded per UTF-8 byte ("ノ" = E3 83 8E).
        assert_eq!(percent_encode_query("ノ"), "%E3%83%8E");
        // Reserved ASCII that could break the query string is escaped.
        assert_eq!(percent_encode_query("a&b=c"), "a%26b%3Dc");
    }
}
