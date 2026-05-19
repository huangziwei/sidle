//! Tauri commands for library operations.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::oneshot;

use crate::library::cover_fetch;
use crate::library::db::{self, BookRow};
use crate::library::epub_cover;
use crate::library::import::{self, ImportOutcome};
use crate::state::AppState;

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImportResult {
    /// `needs_enqueue` is true when the row was inserted with a pending job —
    /// either an EPUB-import awaiting EPUB→KFX, or a KFX-import awaiting
    /// KFX→EPUB. False on an idempotent re-import where the other side was
    /// already on disk.
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

#[tauri::command]
pub async fn library_import(
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> Result<Vec<ImportResult>, String> {
    let mut out = Vec::with_capacity(paths.len());

    for raw in paths {
        let path = PathBuf::from(&raw);
        let db_handle = state.db.clone();
        let paths_handle = state.paths.clone();
        let raw_for_err = raw.clone();

        let result = tokio::task::spawn_blocking(move || {
            let conn = db_handle.blocking_lock();
            import::import_file(&conn, &paths_handle, &path)
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
///
/// Validation + canonicalization happens here; `db::update_metadata` writes
/// the patch verbatim.
#[tauri::command]
pub async fn library_update_metadata(
    app: AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
    patch: db::MetadataPatch,
) -> Result<BookRow, String> {
    let mut patch = patch;

    // Trim text fields.
    patch.title = patch.title.trim().to_string();
    patch.author = patch.author.trim().to_string();
    patch.language = patch.language.trim().to_string();
    match &mut patch.series_name {
        Some(s) => {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                patch.series_name = None;
            } else {
                *s = trimmed;
            }
        }
        None => {}
    }

    // Validate.
    if patch.title.is_empty() {
        return Err("title cannot be empty".into());
    }
    if let Some(idx) = patch.series_index
        && (!idx.is_finite() || idx < 0.0)
    {
        return Err("series_index must be a non-negative number".into());
    }
    // Series index without a name has no meaning — drop it so the row
    // stays self-consistent.
    if patch.series_name.is_none() {
        patch.series_index = None;
    }

    // Canonicalize tags: trim, lowercase, drop empties, dedupe in-order.
    // Lowercasing is a no-op for CJK characters and gives consistent
    // grouping for ASCII tags ("Sci-Fi" and "sci-fi" merge).
    patch.tags = canonicalize_tags(std::mem::take(&mut patch.tags));

    let updated = {
        let conn = state.db.lock().await;
        db::update_metadata(&conn, book_id, &patch).map_err(|e| e.to_string())?;
        db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("book {book_id} not found"))?
    };

    let _ = app.emit("library:row-updated", &updated);
    Ok(updated)
}

fn canonicalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::with_capacity(tags.len());
    for t in tags {
        let t = t.trim().to_lowercase();
        if t.is_empty() {
            continue;
        }
        if seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out
}

#[tauri::command]
pub async fn library_remove(state: State<'_, AppState>, book_id: i64) -> Result<(), String> {
    let sha = {
        let conn = state.db.lock().await;
        db::remove_book(&conn, book_id).map_err(|e| e.to_string())?
    };
    if let Some(sha) = sha {
        state.paths.remove_sha(&sha);
    }
    Ok(())
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
                // Prefer the EPUB if it exists; fall back to KFX so books
                // imported as `.kfx` with a still-converting EPUB can still
                // be revealed by their `.kfx` file.
                b.epub_path.or(b.kfx_path)
            })
    };
    let Some(path) = path else {
        return Err("no file on disk for this book yet".into());
    };
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|e| e.to_string())
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
pub enum RecrawlResult {
    Updated { cover_path: String },
    NoAsin,
    Failed { error: String },
}

/// Re-pull the color cover for one book by hitting Amazon's `/images/P/`
/// endpoint with its ASIN. Same fetch path the kfx_to_epub worker tail uses;
/// this command is just the manual trigger from the right-click menu.
#[tauri::command]
pub async fn library_recrawl_cover(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<RecrawlResult, String> {
    let book = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id).map_err(|e| e.to_string())?
    };
    let Some(book) = book else {
        return Err("book not found".into());
    };
    let Some(asin) = book.asin.as_deref() else {
        return Ok(RecrawlResult::NoAsin);
    };
    let Some(bytes) = cover_fetch::fetch_color_cover(asin, &book.language).await else {
        return Ok(RecrawlResult::Failed {
            error: "no cover returned (404, placeholder, or network error \
                    — see [sidle/cover-fetch] log lines)"
                .into(),
        });
    };
    let out = state.paths.cover(&book.sha256, "jpg");
    if let Err(e) = std::fs::write(&out, &bytes) {
        return Ok(RecrawlResult::Failed {
            error: format!("write failed: {e}"),
        });
    }
    let out_str = out.to_string_lossy().to_string();
    {
        let conn = state.db.lock().await;
        let _ = db::set_cover_path(&conn, book_id, &out_str);
    }
    // If the previous cover lived at a different filename (e.g. cover.png
    // from a PNG-encoded resource), tidy it up so we don't leave both on
    // disk.
    if let Some(old) = book.cover_path.as_deref()
        && old != out_str.as_str()
    {
        let _ = std::fs::remove_file(old);
    }
    // Also swap the cover inside the EPUB so external readers see the
    // color version. Best-effort: log to stderr and continue if the swap
    // fails — the sidecar is what the sidle gallery uses, so a failed
    // EPUB swap doesn't invalidate the user's "Re-fetch cover" action.
    if let Some(epub) = book.epub_path.as_deref() {
        if let Err(e) = epub_cover::replace_cover(std::path::Path::new(epub), &bytes, "jpg") {
            eprintln!("[sidle/recrawl] book {book_id} epub cover swap failed: {e:#}");
        }
    }
    Ok(RecrawlResult::Updated {
        cover_path: out_str,
    })
}

/// Open the system file dialog and return selected ebook paths.
///
/// Accepts EPUB, KFX, and KFX-zip (the multi-container bundle Kindle DeDRM
/// produces) — the import pipeline dispatches on extension. Exposed from Rust
/// because vanilla-JS (no bundler) can't import the dialog plugin's JS module.
#[tauri::command]
pub async fn library_pick_files(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    let (tx, rx) = oneshot::channel();
    app.dialog()
        .file()
        .add_filter("Ebooks", &["epub", "kfx", "kfx-zip"])
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

#[cfg(test)]
mod tests {
    use super::canonicalize_tags;

    #[test]
    fn canonicalize_collapses_case_and_trims() {
        let got = canonicalize_tags(vec![
            "Sci-Fi".into(),
            "sci-fi".into(),
            "  Fantasy ".into(),
            "".into(),
            "  ".into(),
            "FANTASY".into(),
        ]);
        assert_eq!(got, vec!["sci-fi", "fantasy"]);
    }

    #[test]
    fn canonicalize_preserves_order_of_first_occurrence() {
        let got = canonicalize_tags(vec![
            "bbb".into(),
            "aaa".into(),
            "BBB".into(),
        ]);
        assert_eq!(got, vec!["bbb", "aaa"]);
    }

    #[test]
    fn canonicalize_passes_cjk_through_unchanged() {
        // CJK has no case, so lowercase is a no-op; trim still applies.
        let got = canonicalize_tags(vec![
            " 小説 ".into(),
            "ライトノベル".into(),
            "小説".into(), // duplicate after trim
        ]);
        assert_eq!(got, vec!["小説", "ライトノベル"]);
    }

    #[test]
    fn canonicalize_mixed_cjk_and_ascii_lowercases_only_ascii() {
        let got = canonicalize_tags(vec!["ライトSciFi".into(), "ライトscifi".into()]);
        assert_eq!(got, vec!["ライトscifi"]);
    }
}
