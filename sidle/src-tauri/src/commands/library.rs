//! Tauri commands for library operations.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::oneshot;

use crate::cover_fetch;
use crate::library::db::{self, BookRow};
use crate::library::epub_cover;
use crate::library::import::{self, ImportOutcome};
use crate::library::kfx_cover;
use crate::library::{LibraryPaths, backup, merge, relocate};
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

/// A live tick from an import in flight. Keyed by the source path (there is no
/// book row yet — that is the last thing an import does), with `index`/`total`
/// placing this file in a multi-file drop. `fraction` and `label` mean what
/// they do for a conversion: how full the bar is, and the step it is on.
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

        // Three phases, two of them short-lived under the database lock and the
        // slow one — the azw3/mobi/aozora conversion, minutes on a big book —
        // outside it entirely. Holding the app's one connection across that
        // would stall every reader and every running conversion behind a single
        // drop.
        let result = tokio::task::spawn_blocking(move || {
            let kind = import::detect_kind(&path)?;
            let identity = import::identify_file(&path)?;
            {
                let conn = db_handle.blocking_lock();
                if let Some(existing) = db::find_by_sha(&conn, &identity.0)? {
                    return Ok(ImportOutcome::Duplicate(existing));
                }
            }

            // Every file opens with a tick, whatever its format: it names the
            // file being worked on, and it clears what the file before it left
            // on the bar — a fast `.epub` behind a slow `.azw3` would otherwise
            // sit under the azw3's finished bar for as long as it took.
            emit_import_progress(&app_handle, &raw, index, file_count, 0.0, "");

            let pipeline = crate::progress::import_pipeline(kind);
            let throttle = crate::progress::Throttle::new();
            let on_progress = |phase: &str, cur: usize, total: usize, label: &str| {
                // The formats stored as they arrive land too fast for the steps
                // to be worth naming; their opening tick is the whole report.
                let Some(pipeline) = pipeline else { return };
                let fraction = crate::progress::fraction(pipeline, phase, cur, total);
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
    apply_metadata_patch(&app, &state, book_id, patch).await
}

/// Canonicalize, validate, persist, and file-rename a full metadata patch, then
/// emit `library:row-updated` and return the refreshed row. Shared by the
/// metadata modal (`library_update_metadata`) and the book editor's metadata
/// panel (`editor_save_metadata`), which layers a surgical KFX write on top so
/// the edit is durable in the artifact, not just the library row.
pub(crate) async fn apply_metadata_patch(
    app: &AppHandle,
    state: &AppState,
    book_id: i64,
    mut patch: db::MetadataPatch,
) -> Result<BookRow, String> {
    // Trim text fields.
    patch.title = patch.title.trim().to_string();
    // Canonicalize authors: split the field on `&`/「、」 (never a plain comma —
    // that's the intra-name "Surname, Given" separator), flip Western names to
    // natural order, and re-join with the unambiguous display separator.
    patch.author =
        crate::library::authors::join_display(&crate::library::authors::parse_input(&patch.author));
    // Harmonize to a canonical code (en-US → en, eng → en, zh-TW → zh-Hant) so a
    // hand-edit stays consistent with what import stores.
    patch.language = crate::library::lang::normalize(&patch.language);
    // Page progression direction: only "rtl"/"ltr" are meaningful; blank or
    // anything else clears it to None (Auto = device/source default).
    patch.ppd = match patch.ppd.take().map(|s| s.trim().to_ascii_lowercase()) {
        Some(s) if s == "rtl" || s == "ltr" => Some(s),
        _ => None,
    };
    // Reading layout / writing mode: canonicalize to one of the four
    // primary-writing-mode values (or None = Auto). When set it's authoritative
    // for the page direction — a `-rl` layout turns right-to-left — so derive
    // `ppd` from it, keeping the two columns in agreement.
    patch.writing_mode = normalize_writing_mode(patch.writing_mode.take());
    if let Some(wm) = &patch.writing_mode {
        patch.ppd = Some(if wm.ends_with("-rl") { "rtl" } else { "ltr" }.to_string());
    }
    // Romaji: trim + lowercase the editable search fields. A blank field
    // self-heals by re-rendering from the (now-canonicalized) title/author via
    // the engine — so clearing it regenerates a sensible value instead of wiping
    // the book out of search. `title`/`author`/`language` are finalized above.
    // The yomi isn't available here (no source file), so a regenerate is
    // engine-only — the user then hand-corrects an ambiguous name.
    patch.title_romaji = normalize_romaji(&patch.title_romaji, &patch.title, &patch.language);
    patch.author_romaji = normalize_romaji(&patch.author_romaji, &patch.author, &patch.language);
    if let Some(s) = &mut patch.publisher {
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            patch.publisher = None;
        } else {
            *s = trimmed;
        }
    }
    if let Some(s) = &mut patch.published_at {
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            patch.published_at = None;
        } else {
            *s = trimmed;
        }
    }
    if let Some(s) = &mut patch.series_name {
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            patch.series_name = None;
        } else {
            *s = trimmed;
        }
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
        // Rename the on-disk files to match the edited `[Author] Title (Year)`
        // (best-effort; returns the refreshed row with any new paths). Keeps the
        // library folder and a future force-reconvert's derived basename in sync
        // with the metadata.
        crate::library::rename::rename_book_files(&conn, &state.paths, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("book {book_id} not found"))?
    };

    let _ = app.emit("library:row-updated", &updated);
    Ok(updated)
}

/// Canonicalize a reading-layout / writing-mode value to one of the four
/// `primary-writing-mode` strings (hyphenated, lowercase), or `None` (Auto) for
/// anything else. The UI only offers valid options, so an unknown value clears
/// to Auto rather than erroring.
fn normalize_writing_mode(v: Option<String>) -> Option<String> {
    let v = v?.trim().to_ascii_lowercase().replace('_', "-");
    match v.as_str() {
        "horizontal-lr" | "horizontal-rl" | "vertical-rl" | "vertical-lr" => Some(v),
        _ => None,
    }
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

/// Normalize an edited romaji field: trim + lowercase, and self-heal a blank one
/// by re-rendering from its source (title/author) via the engine — so clearing
/// the field regenerates rather than blanking the book out of search.
fn normalize_romaji(value: &str, source: &str, language: &str) -> String {
    let trimmed = value.trim().to_lowercase();
    if trimmed.is_empty() {
        crate::library::romaji::romanize_field(source, None, language)
    } else {
        trimmed
    }
}

/// Render romaji for a piece of text — backs the metadata editor's "regenerate"
/// (`↻`) buttons. Engine-only (no yomi available client-side); the user reviews
/// and corrects the result before saving.
#[tauri::command]
pub fn library_romanize(text: String, language: String) -> String {
    crate::library::romaji::romanize_field(&text, None, &language)
}

/// Set a book's ASIN to a real 10-character Amazon catalogue id.
///
/// Deliberately separate from `library_update_metadata` (the full-replacement
/// patch). That command sends every field on every save, so validating ASIN
/// there would reject saves on books that still carry their fabricated 32-char
/// bokai id. A dedicated command validates only when the user actually changes
/// the ASIN, and keeps the edit a distinct action — it has device-side
/// consequences (the `_<ASIN>.sdr` cleanup scan in `device::push`).
///
/// Rejects empty / free-text values (clearing isn't a use case — the fabricated
/// id is the resting state) and an ASIN already held by another book (the
/// per-book unique-id invariant; see `db::book_id_with_asin`).
#[tauri::command]
pub async fn library_set_asin(
    app: AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
    asin: String,
) -> Result<BookRow, String> {
    let asin = asin.trim().to_string();
    if !cover_fetch::looks_like_real_amazon_asin(&asin) {
        return Err("ASIN must be a real 10-character Amazon id (A–Z, 0–9).".into());
    }

    let updated = {
        let conn = state.db.lock().await;
        if let Some(other) =
            db::book_id_with_asin(&conn, &asin, book_id).map_err(|e| e.to_string())?
        {
            return Err(format!(
                "ASIN {asin} is already used by another book (id {other})."
            ));
        }
        db::set_asin(&conn, book_id, &asin).map_err(|e| e.to_string())?;
        // A user ASIN edit is curation, so move `updated_at` forward (the bump
        // can't live in `db::set_asin` — bootstrap and the conversion worker call
        // it mechanically). Merge's newest-wins then sees this edit.
        db::set_book_updated_at(&conn, book_id, &db::now_iso()).map_err(|e| e.to_string())?;
        db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("book {book_id} not found"))?
    };

    let _ = app.emit("library:row-updated", &updated);
    Ok(updated)
}

/// Open the user's browser to an Amazon search for this book, so they can find
/// its real ASIN to paste into the editor. The marketplace is chosen from the
/// book's language — the same language→store proxy `cover_fetch` uses to pick
/// the cover locale. Scoped to the Kindle store (`i=digital-text`).
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

/// Minimal `application/x-www-form-urlencoded` encoding for a search query, so
/// we don't pull in a `url`/`urlencoding` crate for one call site. Spaces →
/// `+`; RFC 3986 unreserved chars pass through; everything else (including all
/// multi-byte UTF-8, e.g. CJK titles) is percent-encoded per byte.
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

/// Trim an optional string in place; a now-empty value collapses to `None`
/// ("leave unchanged" in bulk semantics).
fn normalize_opt(s: &mut Option<String>) {
    if let Some(v) = s {
        let t = v.trim().to_string();
        if t.is_empty() {
            *s = None;
        } else {
            *v = t;
        }
    }
}

/// Bulk-edit metadata across many books. Sparse semantics: only the fields the
/// user filled in change; tags are additive. See [`db::BulkMetadataPatch`].
///
/// Validates / normalizes once, then applies per book under a single DB lock.
/// Unlike the single-book commands it does **not** emit `library:row-updated`
/// per book — the gallery's subscriber re-renders on every event, so a bulk
/// emit would mean one full render per book. The caller merges the returned
/// Vec and renders once.
#[tauri::command]
pub async fn library_bulk_update_metadata(
    state: State<'_, AppState>,
    book_ids: Vec<i64>,
    patch: db::BulkMetadataPatch,
) -> Result<Vec<BookRow>, String> {
    let mut patch = patch;
    normalize_opt(&mut patch.author);
    // Canonicalize the bulk author the same way as a single edit (split on
    // `&`/「、」, flip Western names, re-join); empty result clears it back to None.
    if let Some(a) = patch.author.take() {
        let canon =
            crate::library::authors::join_display(&crate::library::authors::parse_input(&a));
        patch.author = (!canon.is_empty()).then_some(canon);
    }
    normalize_opt(&mut patch.language);
    // Harmonize to a canonical code, same as a single edit / import.
    if let Some(l) = patch.language.take() {
        let canon = crate::library::lang::normalize(&l);
        patch.language = (!canon.is_empty()).then_some(canon);
    }
    // Page direction: lowercase + validate. Bulk can only set rtl/ltr (the
    // sparse "leave unchanged" semantics can't express "clear to Auto").
    if let Some(p) = patch.ppd.take() {
        let p = p.trim().to_ascii_lowercase();
        patch.ppd = match p.as_str() {
            "rtl" | "ltr" => Some(p),
            "" => None,
            _ => return Err("page direction must be 'rtl' or 'ltr'".into()),
        };
    }
    // Reading layout / writing mode: canonicalize like a single edit; when set,
    // it's authoritative for the page direction, so derive `ppd` from it.
    patch.writing_mode = normalize_writing_mode(patch.writing_mode.take());
    if let Some(wm) = &patch.writing_mode {
        patch.ppd = Some(if wm.ends_with("-rl") { "rtl" } else { "ltr" }.to_string());
    }
    normalize_opt(&mut patch.publisher);
    normalize_opt(&mut patch.published_at);
    normalize_opt(&mut patch.series_name);

    if let Some(idx) = patch.series_index
        && (!idx.is_finite() || idx < 0.0)
    {
        return Err("series_index must be a non-negative number".into());
    }
    patch.add_tags = canonicalize_tags(std::mem::take(&mut patch.add_tags));
    patch.remove_tags = canonicalize_tags(std::mem::take(&mut patch.remove_tags));

    let updated = {
        let conn = state.db.lock().await;
        let mut rows = Vec::with_capacity(book_ids.len());
        for id in &book_ids {
            db::apply_bulk_patch(&conn, *id, &patch).map_err(|e| e.to_string())?;
            if let Some(r) = db::get_book(&conn, *id).map_err(|e| e.to_string())? {
                rows.push(r);
            }
        }
        rows
    };
    Ok(updated)
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

/// Compact the library DB file (`VACUUM`), reclaiming the disk space freed by
/// removals. Deleting a book frees its rows but not the file's pages (SQLite
/// keeps them on a free-list), so the gallery calls this once per delete
/// *operation* — after a single remove, and after a bulk remove finishes. A
/// multi-select delete therefore pays one VACUUM, not one per book. Best-effort
/// from the caller's side: a transient failure here doesn't undo the deletes.
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
                // Reveal the first candidate that actually EXISTS on disk (KFX
                // first), so a stale DB path — e.g. an EPUB removed from the
                // folder, or a KFX import whose EPUB hasn't converted yet —
                // falls through to the file that's really there instead of
                // failing to reveal anything.
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

/// Summary of a multi-book export to an external folder, shown in a toast.
#[derive(Debug, Serialize)]
pub struct ExportSummary {
    /// Files actually copied.
    pub exported: usize,
    /// Books with no file of the requested format on disk (or a copy error).
    pub skipped: usize,
    /// The destination folder (echoed back for the toast).
    pub dest: String,
    /// First few human-readable skip reasons (capped), for the toast/console.
    pub errors: Vec<String>,
}

/// Write the chosen `format` of every book in `book_ids` directly into
/// `dest_dir` as a flat folder of files (`<dest>/<filename>`, no per-author
/// subfolders).
///
/// `"epub"` | `"pdf"` | `"kfx"` copy the stored file verbatim — each keeps its
/// basename (already `[Author] Title (Year)`). `"txt"` has no stored file: it is
/// generated on demand by converting the book's content to Markdown (the EPUB
/// when present — closest to the source text — else the universal KFX side),
/// written as `<basename>.txt`.
///
/// A name collision in the destination is disambiguated with a ` (n)` suffix so
/// two same-named files never clobber each other. A book with no usable source
/// on disk — the companion side hasn't converted yet, or the file was deleted —
/// is skipped and counted; the export never aborts on a single failure.
#[tauri::command]
pub async fn library_export_books(
    state: State<'_, AppState>,
    book_ids: Vec<i64>,
    format: String,
    dest_dir: String,
) -> Result<ExportSummary, String> {
    if !matches!(format.as_str(), "epub" | "pdf" | "kfx" | "txt") {
        return Err(format!("unknown export format: {format}"));
    }
    let dest_root = Path::new(&dest_dir);
    if !dest_root.is_dir() {
        return Err(format!("{} is not a folder", dest_root.display()));
    }

    let books = {
        let conn = state.db.lock().await;
        let mut v = Vec::with_capacity(book_ids.len());
        for &id in &book_ids {
            if let Some(b) = db::get_book(&conn, id).map_err(|e| e.to_string())? {
                v.push(b);
            }
        }
        v
    };

    let mut exported = 0usize;
    let mut skipped = 0usize;
    let mut errors: Vec<String> = Vec::new();
    let note = |errors: &mut Vec<String>, msg: String| {
        if errors.len() < 8 {
            errors.push(msg);
        }
    };

    for book in books {
        // The source file to read. The copy formats name that exact file; `txt`
        // is generated from the best available content source — the EPUB if it's
        // on disk, else the universal KFX side.
        let src: Option<&Path> = match format.as_str() {
            "kfx" => book
                .kfx_path
                .as_deref()
                .map(Path::new)
                .filter(|p| p.exists()),
            "epub" => book
                .epub_path
                .as_deref()
                .map(Path::new)
                .filter(|p| p.exists()),
            "pdf" => book
                .pdf_path
                .as_deref()
                .map(Path::new)
                .filter(|p| p.exists()),
            "txt" => [book.epub_path.as_deref(), book.kfx_path.as_deref()]
                .into_iter()
                .flatten()
                .map(Path::new)
                .find(|p| p.exists()),
            _ => unreachable!("format validated above"),
        };
        let Some(src) = src else {
            skipped += 1;
            let what = if format == "txt" {
                "no EPUB or KFX source on disk".to_string()
            } else {
                format!("no {} file on disk", format.to_uppercase())
            };
            note(&mut errors, format!("{}: {what}", book.title));
            continue;
        };

        // Target filename. Copy formats keep the source's name verbatim; `txt`
        // swaps the source's extension for `.txt` (both companion sides share
        // the same `[Author] Title (Year)` stem, so the source choice doesn't
        // change the output name).
        let target_name = if format == "txt" {
            match src.file_stem() {
                Some(stem) => {
                    let mut n = stem.to_os_string();
                    n.push(".txt");
                    n
                }
                None => {
                    skipped += 1;
                    continue;
                }
            }
        } else {
            match src.file_name() {
                Some(name) => name.to_os_string(),
                None => {
                    skipped += 1;
                    continue;
                }
            }
        };

        let target = crate::library::paths::dedup_path(dest_root.join(target_name));
        let outcome = if format == "txt" {
            // KFX decode + IR walk is CPU-heavy; keep it off the async runtime.
            let src = src.to_path_buf();
            let target = target.clone();
            tokio::task::spawn_blocking(move || export_book_as_txt(&src, &target))
                .await
                .unwrap_or_else(|e| Err(format!("task panicked: {e}")))
        } else {
            std::fs::copy(src, &target)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        match outcome {
            Ok(()) => exported += 1,
            Err(e) => {
                skipped += 1;
                note(&mut errors, format!("{}: {e}", book.title));
            }
        }
    }

    Ok(ExportSummary {
        exported,
        skipped,
        dest: dest_dir,
        errors,
    })
}

/// Convert a book file (EPUB or KFX, auto-detected by extension) to Markdown and
/// write it to `target`. Backs the `txt` export, which — unlike the copy formats
/// — has no stored file. Call on a blocking thread: bokai's KFX decode and IR
/// walk are CPU-bound.
fn export_book_as_txt(src: &Path, target: &Path) -> Result<(), String> {
    let mut book = bokai::Book::open(src).map_err(|e| format!("open: {e}"))?;
    let mut file = std::fs::File::create(target).map_err(|e| format!("create: {e}"))?;
    book.export(bokai::Format::Markdown, &mut file)
        .map_err(|e| format!("convert: {e}"))?;
    Ok(())
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

/// Internal outcome of re-fetching one book's cover. Mirrors `RecrawlResult`
/// but stays inside Rust: the single-book command maps it to the tagged
/// `RecrawlResult` the frontend toasts on, the bulk command just tallies it.
enum RecrawlOutcome {
    Updated { cover_path: String },
    NoAsin,
    Failed { error: String },
}

/// Embed `bytes` as the KFX cover, returning the new sha256 (for `kfx_sha256`)
/// or `None` on failure. Prefers an in-place swap; if the KFX declares no cover
/// (an EPUB import whose source EPUB had none), `ensure_cover` has already
/// inserted a cover into that EPUB, so we reconvert the KFX from it — giving the
/// on-device tile / sleep-screen a real cover. Best-effort: failures log under
/// `[sidle/<tag>]` and yield `None`. Shared by re-fetch and "Change cover…".
fn swap_or_insert_kfx_cover(book: &BookRow, kfx: &str, bytes: &[u8], tag: &str) -> Option<String> {
    let kfx_path = std::path::Path::new(kfx);
    match kfx_cover::replace_cover(kfx_path, bytes) {
        Ok(sha) => Some(sha),
        Err(e) => {
            // The KFX may be rebuilt from its EPUB only when the EPUB is the
            // SOURCE (`kind == "epub_to_kfx"`). Conversion runs source→target
            // only, never the reverse: a KFX-sourced book's KFX is authoritative
            // and must never be regenerated from its derived EPUB. So a swap
            // failure on a KFX-sourced book is just logged, not "healed".
            let epub_is_source = book.kind.as_deref() == Some("epub_to_kfx");
            let Some(epub) = book.epub_path.as_deref().filter(|_| epub_is_source) else {
                eprintln!(
                    "[sidle/{tag}] book {} kfx cover swap failed: {e:#}",
                    book.id
                );
                return None;
            };
            let reconvert =
                kfx_cover::reconvert_from_epub(std::path::Path::new(epub), kfx_path, |src| {
                    crate::queue::worker::book_metadata_override(src, book)
                });
            match reconvert {
                Ok(sha) => {
                    eprintln!(
                        "[sidle/{tag}] book {} kfx was coverless; cover inserted via reconvert",
                        book.id
                    );
                    Some(sha)
                }
                Err(e2) => {
                    eprintln!(
                        "[sidle/{tag}] book {} kfx cover swap failed ({e:#}); reconvert failed: {e2:#}",
                        book.id
                    );
                    None
                }
            }
        }
    }
}

/// Re-fetch one book's color cover from Amazon and replace it everywhere we
/// keep a copy: the cover sidecar (what the gallery shows), the picker
/// thumbnail, the embedded EPUB cover (for external readers), and the embedded
/// KFX cover (the on-device home tile / sleep-screen art). The EPUB/KFX swaps
/// are best-effort — logged and skipped on failure, since the sidecar is
/// authoritative for the sidle UI. Shared by `library_recrawl_cover` (single)
/// and `library_recrawl_covers` (bulk).
async fn recrawl_one(state: &AppState, book: &BookRow) -> RecrawlOutcome {
    let Some(asin) = book.asin.as_deref() else {
        return RecrawlOutcome::NoAsin;
    };
    // Treat fabricated bokai ASINs the same as missing — neither can resolve
    // to a real `/images/P/` cover, so "no ASIN" is more honest than "failed".
    if !cover_fetch::looks_like_real_amazon_asin(asin) {
        return RecrawlOutcome::NoAsin;
    }
    let Some(bytes) = cover_fetch::fetch_color_cover(asin, &book.language).await else {
        return RecrawlOutcome::Failed {
            error: "no cover returned (404, placeholder, or network error \
                    — see [sidle/cover-fetch] log lines)"
                .into(),
        };
    };
    let out = state.paths.cover(&book.sha256, "jpg");
    if let Err(e) = std::fs::write(&out, &bytes) {
        return RecrawlOutcome::Failed {
            error: format!("write failed: {e}"),
        };
    }
    let out_str = out.to_string_lossy().to_string();
    {
        let conn = state.db.lock().await;
        let _ = db::set_cover_path(&conn, book.id, &out_str);
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
    // Also swap the cover inside the EPUB so external readers see the color
    // version. `ensure_cover` regenerates the EPUB from the KFX when it's the
    // derived side and has no cover slot, else inserts one. Best-effort: log to
    // stderr and continue on failure — the sidecar is what the sidle gallery
    // uses, so a failed EPUB swap doesn't invalidate the user's action.
    if let Some(epub) = book.epub_path.as_deref() {
        match epub_cover::ensure_cover(
            std::path::Path::new(epub),
            book.kfx_path.as_deref().map(std::path::Path::new),
            &bytes,
            "jpg",
            book.kind.as_deref() == Some("kfx_to_epub"),
        ) {
            Ok(()) => {}
            Err(e) => eprintln!(
                "[sidle/recrawl] book {} epub cover swap failed: {e:#}",
                book.id
            ),
        }
    }
    // And into the imported KFX — that's the copy we push to the Kindle, and
    // its embedded cover drives the home tile / sleep-screen art. Rewriting it
    // changes the bytes, but `kfx_sha256` is the book's frozen identity (the
    // on-device filename infix), so `set_kfx_path_and_sha` preserves it — the
    // new-cover KFX reaches the device under the same, stable filename.
    if let Some(kfx) = book.kfx_path.as_deref()
        && let Some(new_sha) = swap_or_insert_kfx_cover(book, kfx, &bytes, "recrawl")
    {
        let conn = state.db.lock().await;
        let _ = db::set_kfx_path_and_sha(&conn, book.id, kfx, &new_sha);
    }
    RecrawlOutcome::Updated {
        cover_path: out_str,
    }
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
    Ok(match recrawl_one(state.inner(), &book).await {
        RecrawlOutcome::Updated { cover_path } => RecrawlResult::Updated { cover_path },
        RecrawlOutcome::NoAsin => RecrawlResult::NoAsin,
        RecrawlOutcome::Failed { error } => RecrawlResult::Failed { error },
    })
}

/// Per-book progress for a bulk cover re-fetch, emitted as
/// `library:recrawl-progress`. The run is one slow Amazon round-trip (plus an
/// EPUB/KFX rewrite) per book, so without this the selection bar would look
/// frozen for minutes on a large batch.
#[derive(Clone, Serialize)]
struct RecrawlProgress {
    done: usize,
    total: usize,
}

/// Tally returned by `library_recrawl_covers`. `updated` got a fresh cover;
/// `failed` returned no usable cover (404 / placeholder / network); `no_asin`
/// had no real Amazon ASIN to fetch from and was skipped.
#[derive(Debug, Serialize)]
pub struct RecrawlBulkSummary {
    updated: usize,
    failed: usize,
    no_asin: usize,
}

/// Re-fetch covers for a set of books — the selection bar's "Re-fetch covers"
/// action (and the multi-select / whole-series context menus). Sequential, to
/// match `library_import` and to hold one cheap DB lock at a time; emits
/// `library:recrawl-progress` after each book. The frontend does a single
/// `library_list` refresh when this returns rather than re-rendering per book.
#[tauri::command]
pub async fn library_recrawl_covers(
    app: AppHandle,
    state: State<'_, AppState>,
    book_ids: Vec<i64>,
) -> Result<RecrawlBulkSummary, String> {
    let total = book_ids.len();
    let mut summary = RecrawlBulkSummary {
        updated: 0,
        failed: 0,
        no_asin: 0,
    };
    for (i, id) in book_ids.iter().enumerate() {
        let book = {
            let conn = state.db.lock().await;
            db::get_book(&conn, *id).map_err(|e| e.to_string())?
        };
        match book {
            Some(book) => match recrawl_one(state.inner(), &book).await {
                RecrawlOutcome::Updated { .. } => summary.updated += 1,
                RecrawlOutcome::NoAsin => summary.no_asin += 1,
                RecrawlOutcome::Failed { error } => {
                    summary.failed += 1;
                    eprintln!("[sidle/recrawl] book {} failed: {error}", book.id);
                }
            },
            // Row vanished mid-run (e.g. removed by a parallel action). Count
            // it as failed and keep going rather than aborting the batch.
            None => summary.failed += 1,
        }
        let _ = app.emit(
            "library:recrawl-progress",
            RecrawlProgress { done: i + 1, total },
        );
    }
    Ok(summary)
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
    // `ensure_cover` regenerates from the KFX only when the EPUB is derived,
    // else inserts. Best-effort: failure logs and doesn't fail the command (the
    // gallery reads from the sidecar, not the EPUB).
    if let Some(epub) = book.epub_path.as_deref() {
        match epub_cover::ensure_cover(
            std::path::Path::new(epub),
            book.kfx_path.as_deref().map(std::path::Path::new),
            &bytes,
            ext,
            book.kind.as_deref() == Some("kfx_to_epub"),
        ) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("[sidle/set-cover] book {book_id} epub cover swap failed: {e:#}")
            }
        }
    }

    // And into the imported KFX (bokai normalizes png/webp → jpeg). This is the
    // copy pushed to the Kindle; the rewrite changes the bytes but the frozen
    // `kfx_sha256` identity is preserved by `set_kfx_path_and_sha`, so the
    // on-device filename is unchanged. A cover-less KFX (EPUB import with no
    // source cover) is healed by reconvert.
    if let Some(kfx) = book.kfx_path.as_deref()
        && let Some(new_sha) = swap_or_insert_kfx_cover(&book, kfx, &bytes, "set-cover")
    {
        let conn = state.db.lock().await;
        let _ = db::set_kfx_path_and_sha(&conn, book_id, kfx, &new_sha);
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
pub(crate) fn sniff_image_format(bytes: &[u8]) -> Option<&'static str> {
    // JPEG: FF D8 FF
    if bytes.len() >= 3 && bytes[0..3] == [0xFF, 0xD8, 0xFF] {
        return Some("jpg");
    }
    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if bytes.len() >= 8 && bytes[0..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
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
        .add_filter("Ebooks", &["epub", "kfx", "kfx-zip", "mobi", "azw3", "pdf"])
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
/// other root entry — `books/`, `notebooks/`, `device-dist/`, `.server-token` —
/// (rename when same-volume, else copy), repoint, delete the old remnants, and
/// relaunch — nothing is left behind except the tiny `config.json` pointer in the
/// app state dir. Refuses when a conversion is in flight (its
/// output would be stranded), when `dest` is already the current root, or when
/// `dest` is non-empty — except the default location, which always holds
/// `config.json` and is refused only if it already contains a library.
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
    // Moving back to it must therefore be allowed; we only refuse when it
    // already holds a library we'd clobber. Any other destination must be empty.
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
        // output into the old root and be stranded. (Relaunch is why no queue
        // *pause* is needed — §6.)
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

/// Per-unit progress for a long file operation (backup / restore / merge),
/// emitted as `library:fileop-progress` so the footer shows "Backing up library
/// — 62%" instead of going silent for the duration. `op` is the verb the
/// frontend renders; `done`/`total` count archived dirs (backup) or extracted
/// zip entries (restore / merge). The frontend clears the line when the command
/// resolves — restore relaunches, so its final tick just vanishes with the old
/// process.
#[derive(Clone, Serialize)]
struct FileopProgress {
    op: &'static str,
    done: u64,
    total: u64,
}

/// Write a full backup of the current library to `dest`. Holds the DB lock only
/// for the consistent snapshot, then zips the `books/` tree lock-free so a large
/// backup doesn't stall the app. Non-destructive — nothing here touches the live
/// library, so no queue gate or relaunch is needed.
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

    // Zip on a blocking thread — lock-free (it reads only the snapshot file + the
    // on-disk book tree) AND off the async runtime, so a large backup doesn't
    // stall other commands or freeze the UI. The snapshot guard moves in and
    // cleans up there.
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

/// Restore a `.sidlebak` over the current library, then relaunch. Replaces the
/// library; the pre-restore copy is kept at `<root>.bak-<timestamp>` as the undo
/// (the confirm UI states this). Refuses while a conversion is in flight (the
/// swap would strand its output), validates + verifies before the swap (so a bad
/// archive leaves the target untouched), then relaunches onto the restored files
/// — the same restore-then-relaunch path as relocate (H5), since the live
/// `Connection` can't be repointed in place.
#[tauri::command]
pub async fn library_restore(
    app: AppHandle,
    state: State<'_, AppState>,
    src: String,
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
    let outcome = backup::restore_with_progress(&src, &dest_root, db::SCHEMA_VERSION, &on_progress)
        .map_err(|e| format!("{e:#}"))?;
    eprintln!(
        "[sidle/backup] restored {} books; previous library kept at {}",
        outcome.books,
        outcome.safety_copy.display()
    );

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
/// library. **Additive** — only inserts rows + copies new files, never deletes or
/// overwrites — so, unlike restore, there's no swap and no relaunch; the UI just
/// re-lists. Validation + extraction + the (potentially large) file copy run on a
/// blocking thread with no DB lock held; only the row transaction takes the lock,
/// and it's metadata-only (fast). Duplicate books (same content sha) keep the
/// newer side's metadata; everything else unions by its content key.
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
    use super::{canonicalize_tags, percent_encode_query, sniff_image_format};

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
        let got = canonicalize_tags(vec!["bbb".into(), "aaa".into(), "BBB".into()]);
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
