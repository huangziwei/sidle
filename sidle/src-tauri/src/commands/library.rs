//! Tauri commands for library operations.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::oneshot;

use crate::cover_fetch;
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
    match &mut patch.publisher {
        Some(s) => {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                patch.publisher = None;
            } else {
                *s = trimmed;
            }
        }
        None => {}
    }
    match &mut patch.published_at {
        Some(s) => {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                patch.published_at = None;
            } else {
                *s = trimmed;
            }
        }
        None => {}
    }
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
    // Look up the sha first so we can delete the files BEFORE the row.
    // If file deletion fails (Spotlight/Books.app holding a handle, perms),
    // the row stays put so the user sees the failure in the gallery rather
    // than ending up with an orphan `books/<sha>/` dir whose row is gone.
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
    // Treat fabricated boko ASINs the same as missing — neither can resolve
    // to a real `/images/P/` cover, so showing the user "no ASIN" is more
    // honest than "fetch failed".
    if !cover_fetch::looks_like_real_amazon_asin(asin) {
        return Ok(RecrawlResult::NoAsin);
    }
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
    // Refresh the picker thumbnail to match the re-fetched cover. Best-effort
    // (see library::thumbnail).
    let _ = crate::library::thumbnail::ensure_thumbnail(&state.paths, &book.sha256, &out);
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

/// Outcome of the user "Change cover…" action in the metadata editor.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SetCoverResult {
    Updated { cover_path: String },
    Failed { error: String },
}

/// Replace the cover for one book from a user-picked image file.
///
/// Modeled on `library_recrawl_cover` but takes its bytes from a local
/// file instead of Amazon's `/images/P/` endpoint. Immediate-apply
/// semantics: commits on file pick; the editor modal's Cancel doesn't
/// undo this.
///
/// Reads bytes, sniffs the format (JPG/PNG/WebP) via magic bytes so a
/// `.png` file mislabeled as `.jpg` still lands correctly, writes to
/// `paths.cover(&sha, ext)`, updates `cover_path` in the DB, removes the
/// previous file if the extension differs, best-effort EPUB embed via
/// `epub_cover::replace_cover`, and emits `library:row-updated` so the
/// rest of the UI refreshes.
#[tauri::command]
pub async fn library_set_cover(
    app: AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
    src_path: String,
) -> Result<SetCoverResult, String> {
    let bytes = match std::fs::read(&src_path) {
        Ok(b) => b,
        Err(e) => {
            return Ok(SetCoverResult::Failed {
                error: format!("read {src_path}: {e}"),
            });
        }
    };

    let ext = match sniff_image_format(&bytes) {
        Some(e) => e,
        None => {
            return Ok(SetCoverResult::Failed {
                error: "unsupported image format (expected JPG, PNG, or WebP)".into(),
            });
        }
    };

    let book = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id).map_err(|e| e.to_string())?
    };
    let Some(book) = book else {
        return Err("book not found".into());
    };

    let out = state.paths.cover(&book.sha256, ext);
    if let Err(e) = std::fs::write(&out, &bytes) {
        return Ok(SetCoverResult::Failed {
            error: format!("write {}: {e}", out.display()),
        });
    }
    let out_str = out.to_string_lossy().to_string();

    {
        let conn = state.db.lock().await;
        let _ = db::set_cover_path(&conn, book_id, &out_str);
    }

    // Refresh the picker thumbnail to match the user-picked cover. Best-effort
    // (see library::thumbnail).
    let _ = crate::library::thumbnail::ensure_thumbnail(&state.paths, &book.sha256, &out);

    // Old cover at a different filename (e.g. `cover.jpg` being replaced
    // by `cover.png`) — tidy up so we don't leave both on disk.
    if let Some(old) = book.cover_path.as_deref()
        && old != out_str.as_str()
    {
        let _ = std::fs::remove_file(old);
    }

    // Embed in the EPUB so external readers see the user-chosen image.
    // Best-effort: failure logs and doesn't fail the command (the gallery
    // reads from the sidecar, not the EPUB).
    if let Some(epub) = book.epub_path.as_deref()
        && let Err(e) =
            epub_cover::replace_cover(std::path::Path::new(epub), &bytes, ext)
    {
        eprintln!("[sidle/set-cover] book {book_id} epub cover swap failed: {e:#}");
    }

    let updated = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id).map_err(|e| e.to_string())?
    };
    if let Some(updated) = updated {
        let _ = app.emit("library:row-updated", &updated);
    }

    Ok(SetCoverResult::Updated {
        cover_path: out_str,
    })
}

/// Magic-byte sniff for the three image formats sidle accepts as covers.
/// Returns the canonical lowercase extension or None if no header matches.
fn sniff_image_format(bytes: &[u8]) -> Option<&'static str> {
    // JPEG: FF D8 FF
    if bytes.len() >= 3 && bytes[0..3] == [0xFF, 0xD8, 0xFF] {
        return Some("jpg");
    }
    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if bytes.len() >= 8 && bytes[0..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
    {
        return Some("png");
    }
    // WebP: "RIFF" .... "WEBP"
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    None
}

/// Open the system file dialog filtered to images and return one path.
///
/// Used by the metadata editor's "Change cover…" button. The filter is a
/// hint for the OS dialog; the actual format detection happens in
/// `library_set_cover` via magic-byte sniff, so a file with a wrong
/// extension still gets validated before write.
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
///
/// Accepts EPUB, KFX, KFX-zip (the multi-container bundle Kindle DeDRM
/// produces), and MOBI — the import pipeline dispatches on extension.
/// Exposed from Rust because vanilla-JS (no bundler) can't import the
/// dialog plugin's JS module.
#[tauri::command]
pub async fn library_pick_files(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    let (tx, rx) = oneshot::channel();
    app.dialog()
        .file()
        .add_filter("Ebooks", &["epub", "kfx", "kfx-zip", "mobi"])
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
    use super::{canonicalize_tags, sniff_image_format};

    #[test]
    fn sniff_detects_jpeg_png_webp() {
        // JPEG SOI + APP0 (typical EXIF/JFIF prefix).
        assert_eq!(
            sniff_image_format(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]),
            Some("jpg")
        );
        // PNG 8-byte signature.
        assert_eq!(
            sniff_image_format(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("png")
        );
        // WebP container: "RIFF" + 4 size bytes + "WEBP".
        assert_eq!(
            sniff_image_format(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("webp")
        );
    }

    #[test]
    fn sniff_rejects_non_image() {
        assert_eq!(sniff_image_format(b""), None);
        assert_eq!(sniff_image_format(b"PK\x03\x04"), None); // ZIP
        assert_eq!(sniff_image_format(b"hello"), None);
        // Too short to match anything.
        assert_eq!(sniff_image_format(&[0xFF, 0xD8]), None);
    }


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
