//! Book editor — the app-UI surface over boko-kai's KFX edit primitives.
//!
//! v1 targets **KFX-source** books (`kind == "kfx_to_epub"`), where the KFX is
//! both the source and the reader/device file. Every edit is a surgical
//! byte-passthrough rewrite of the KFX (via the boko `kfx::*_edit` primitives);
//! the derived EPUB is regenerated afterwards by the conversion queue. EPUB- and
//! PDF-source editing land in later phases — `editor_open` reports the source
//! format and a `kfx_editable` flag so the UI can gate its panels.
//!
//! The save seam ([`commit_edited_kfx`]) is shared by every mutating command:
//! back up the original source, atomically replace it, preserve the frozen
//! `kfx_sha256` device identity, then re-derive the EPUB and evict the reader
//! cache. It mirrors `library_set_cover`'s in-place KFX rewrite.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::oneshot;

use boko::kfx::image_extract;
use boko::kfx::metadata_edit::{self, MetadataPatch as KfxMetadataPatch};
use boko::kfx::toc_repair::{self, TocEntry};
use boko::validate::source::toc as toc_validate;

use crate::library::{authors, db, db::BookRow};
use crate::state::AppState;

/// Source format a book was imported *from*, from its conversion `kind`
/// (`"<source>_to_<target>"`): `"kfx"`, `"epub"`, or `"pdf"`. Mirrors the
/// frontend `sourceFormat()` helper.
fn source_format(kind: Option<&str>) -> String {
    kind.unwrap_or("epub_to_kfx")
        .split("_to_")
        .next()
        .unwrap_or("epub")
        .to_string()
}

/// TOC health from the source bytes — surfaced as the top-bar validate chip and
/// (later) the TOC panel header. Mirrors `validate::source::toc::TocAudit`.
#[derive(Serialize)]
pub struct EditorToc {
    /// `"OK"` | `"SUSPECT"` | `"SPARSE"`.
    pub verdict: String,
    pub nav_count: usize,
    pub nav_chapters: usize,
    pub contents_links: usize,
    pub headings: usize,
    pub section_heads: usize,
}

/// Current metadata for the metadata panel. `author` is the display string
/// (`"Jane Smith & John Doe"`), parsed back into an ordered list on save.
#[derive(Serialize)]
pub struct EditorMetadata {
    pub title: String,
    pub author: String,
    pub language: String,
    pub publisher: Option<String>,
    pub published_at: Option<String>,
    pub asin: Option<String>,
}

/// The editor's opening snapshot for one book — what the shell renders from.
#[derive(Serialize)]
pub struct EditorOpen {
    pub book_id: i64,
    /// Source format: `"kfx"` | `"epub"` | `"pdf"`.
    pub format: String,
    /// True only when the surgical KFX editor applies (KFX-source book with a
    /// KFX on disk). EPUB/PDF sources open the shell read-only for now.
    pub kfx_editable: bool,
    pub metadata: EditorMetadata,
    pub has_cover: bool,
    /// `None` when the TOC verdict couldn't be computed (non-KFX, or read error).
    pub toc: Option<EditorToc>,
}

/// Load the opening snapshot for the editor shell.
#[tauri::command]
pub async fn editor_open(state: State<'_, AppState>, book_id: i64) -> Result<EditorOpen, String> {
    let row = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no book with id {book_id}"))?
    };
    let format = source_format(row.kind.as_deref());
    let kfx_editable = format == "kfx" && row.kfx_path.is_some();

    // TOC verdict from the *source* bytes (KFX-source only in v1). Off the async
    // thread — it's a full container parse.
    let toc = match row.kfx_path.clone().filter(|_| kfx_editable) {
        Some(kfx_path) => tokio::task::spawn_blocking(move || compute_toc(&kfx_path))
            .await
            .map_err(|e| e.to_string())?,
        None => None,
    };

    Ok(EditorOpen {
        book_id,
        format,
        kfx_editable,
        metadata: EditorMetadata {
            title: row.title.clone(),
            author: row.author.clone(),
            language: row.language.clone(),
            publisher: row.publisher.clone(),
            published_at: row.published_at.clone(),
            asin: row.asin.clone(),
        },
        has_cover: row.cover_path.is_some(),
        toc,
    })
}

/// Fields the metadata panel submits. All are always present (the panel is
/// populated from the current values); an empty publisher/date/asin clears that
/// field.
#[derive(Deserialize)]
pub struct MetadataForm {
    pub title: String,
    /// Raw author line (`"A & B"`) — parsed and canonicalized backend-side.
    pub author: String,
    pub language: String,
    pub publisher: Option<String>,
    pub published_at: Option<String>,
    pub asin: Option<String>,
}

/// Write edited metadata into the KFX source *and* the library row.
///
/// The KFX write is the editor's durable value-add: a surgical
/// `book_metadata`/`metadata` patch, no whole-book re-encode. The row sync keeps
/// the library UI (title/author/…) in step and reuses the metadata modal's exact
/// canonicalize → persist → file-rename path via [`apply_metadata_patch`].
#[tauri::command]
pub async fn editor_save_metadata(
    app: AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
    form: MetadataForm,
) -> Result<BookRow, String> {
    let row = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no book with id {book_id}"))?
    };
    let kfx_path = require_kfx_source(&row)?;

    // Canonicalize the human fields once so the KFX artifact and the DB row agree
    // (author list flip/split, language code harmonize, empty → cleared).
    let title = form.title.trim().to_string();
    if title.is_empty() {
        return Err("title cannot be empty".into());
    }
    let author_names = authors::parse_input(&form.author);
    let language = crate::library::lang::normalize(&form.language);
    let publisher = trim_opt(form.publisher);
    let published_at = trim_opt(form.published_at);
    let asin = trim_opt(form.asin);

    // 1) Surgical KFX metadata write, then commit it in place via the save seam.
    let kfx_patch = KfxMetadataPatch {
        title: Some(title.clone()),
        authors: Some(author_names.clone()),
        language: Some(language.clone()),
        publisher: publisher.clone(),
        issue_date: published_at.clone(),
        asin: asin.clone(),
    };
    let src = kfx_path.clone();
    let new_bytes = tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&src).map_err(|e| format!("read {src}: {e}"))?;
        metadata_edit::edit_metadata(&bytes, &kfx_patch).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    commit_edited_kfx(&state, book_id, &kfx_path, new_bytes).await?;

    // 2) Sync the library row. ASIN isn't part of db::MetadataPatch, so set it
    //    separately; blank romaji triggers the modal path's self-heal regenerate
    //    (the title/author may have changed under it).
    if let Some(asin) = asin.as_deref() {
        let conn = state.db.lock().await;
        db::set_asin(&conn, book_id, asin).map_err(|e| e.to_string())?;
    }
    let patch = db::MetadataPatch {
        title,
        author: authors::join_display(&author_names),
        language,
        ppd: row.ppd.clone(),
        writing_mode: row.writing_mode.clone(),
        publisher,
        published_at,
        series_name: row.series_name.clone(),
        series_index: row.series_index,
        tags: row.tags.clone(),
        title_romaji: String::new(),
        author_romaji: String::new(),
    };
    let updated =
        crate::commands::library::apply_metadata_patch(&app, &state, book_id, patch).await?;

    // 3) Regenerate the derived EPUB from the edited KFX and drop the reader
    //    cache so a re-open reflects the change.
    let _ = state.queue.enqueue_reconvert(book_id).await;
    crate::commands::reader::evict_reader(&state, book_id).await;

    Ok(updated)
}

/// One TOC entry crossing the wire — the full `TocEntry` tree, nesting intact.
/// A repair mirrors the book's own structure: a flat Contents page yields a flat
/// TOC, a Part→chapter Contents page yields a nested one. We never flatten a
/// nested source or nest a flat one.
#[derive(Serialize, Deserialize, Clone)]
pub struct TocEntryDto {
    pub label: String,
    pub eid: i64,
    #[serde(default)]
    pub children: Vec<TocEntryDto>,
}

impl TocEntryDto {
    /// Convert a proposer `TocEntry` to the wire DTO, recursively.
    fn from_entry(e: &TocEntry) -> Self {
        Self {
            label: e.label.clone(),
            eid: e.eid,
            children: e.children.iter().map(Self::from_entry).collect(),
        }
    }

    /// Convert back to a `TocEntry` for `set_toc`, trimming labels, recursively.
    fn into_entry(self) -> TocEntry {
        TocEntry {
            label: self.label.trim().to_string(),
            eid: self.eid,
            children: self.children.into_iter().map(Self::into_entry).collect(),
        }
    }
}

/// True if any entry in the tree has a blank label — a rejected edit (`set_toc`
/// would emit an unlabeled nav unit).
fn any_blank_label(entries: &[TocEntryDto]) -> bool {
    entries
        .iter()
        .any(|e| e.label.trim().is_empty() || any_blank_label(&e.children))
}

/// Full state for the TOC panel: the current declared TOC, the verdict, and a
/// proposed chapter list derived from the book's own in-book Contents page.
#[derive(Serialize)]
pub struct EditorTocDetail {
    /// `"OK"` | `"SUSPECT"` | `"SPARSE"`.
    pub verdict: String,
    pub nav_count: usize,
    pub nav_chapters: usize,
    /// The currently-declared TOC entry labels (flattened) — what's wrong (or
    /// right) today.
    pub current: Vec<String>,
    /// Proposed TOC derived from the densest in-book Contents-page link cluster,
    /// in document order and with the book's own nesting preserved. Empty when
    /// none could be derived (see `note`).
    pub proposed: Vec<TocEntryDto>,
    /// Set when no proposal could be derived, explaining why.
    pub note: Option<String>,
}

/// Read the TOC panel state (verdict + current + proposal). Lazy — the frontend
/// calls it when the TOC panel is first opened, not on every editor open.
#[tauri::command]
pub async fn editor_toc(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<EditorTocDetail, String> {
    let kfx_path = {
        let conn = state.db.lock().await;
        let row = db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no book with id {book_id}"))?;
        require_kfx_source(&row)?
    };
    tokio::task::spawn_blocking(move || read_toc_detail(&kfx_path))
        .await
        .map_err(|e| e.to_string())?
}

/// Write a reviewed TOC (`entries`, in order) into the KFX and re-derive. The
/// user-facing path: the panel shows the proposal, the user tweaks/removes rows,
/// then applies. Returns the refreshed panel state (verdict should flip to OK).
#[tauri::command]
pub async fn editor_set_toc(
    state: State<'_, AppState>,
    book_id: i64,
    entries: Vec<TocEntryDto>,
) -> Result<EditorTocDetail, String> {
    let kfx_path = {
        let conn = state.db.lock().await;
        let row = db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no book with id {book_id}"))?;
        require_kfx_source(&row)?
    };
    if entries.is_empty() {
        return Err("a table of contents needs at least one entry".into());
    }
    if any_blank_label(&entries) {
        return Err("every table-of-contents entry needs a label".into());
    }
    // Preserve the tree exactly — no flatten, no re-nest.
    let toc_entries: Vec<TocEntry> = entries.into_iter().map(TocEntryDto::into_entry).collect();

    let src = kfx_path.clone();
    let new_bytes = tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&src).map_err(|e| format!("read {src}: {e}"))?;
        toc_repair::set_toc(&bytes, &toc_entries).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    commit_edited_kfx(&state, book_id, &kfx_path, new_bytes).await?;
    let _ = state.queue.enqueue_reconvert(book_id).await;
    crate::commands::reader::evict_reader(&state, book_id).await;

    let path = kfx_path.clone();
    tokio::task::spawn_blocking(move || read_toc_detail(&path))
        .await
        .map_err(|e| e.to_string())?
}

/// One-click repair: derive the chapter list and write it in a single pass
/// (`propose + set`), for a user who trusts the proposal without reviewing.
#[tauri::command]
pub async fn editor_repair_toc(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<EditorTocDetail, String> {
    let kfx_path = {
        let conn = state.db.lock().await;
        let row = db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no book with id {book_id}"))?;
        require_kfx_source(&row)?
    };

    let src = kfx_path.clone();
    let new_bytes = tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&src).map_err(|e| format!("read {src}: {e}"))?;
        toc_repair::repair_toc(&bytes).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    commit_edited_kfx(&state, book_id, &kfx_path, new_bytes).await?;
    let _ = state.queue.enqueue_reconvert(book_id).await;
    crate::commands::reader::evict_reader(&state, book_id).await;

    let path = kfx_path.clone();
    tokio::task::spawn_blocking(move || read_toc_detail(&path))
        .await
        .map_err(|e| e.to_string())?
}

// --- images ---------------------------------------------------------------

/// One embedded image for the Images panel: identity + declared dimensions + an
/// on-disk preview copy the webview loads through the asset protocol. The panel
/// offers extract/export only; KFX image *replacement* is a later tier.
#[derive(Serialize)]
pub struct EditorImage {
    /// Position in the container's image list — the stable key
    /// [`editor_export_image`] re-resolves against (the list is read-only and
    /// deterministically sorted by the extractor).
    pub index: usize,
    pub resource_name: String,
    /// Lowercase extension, no dot: `"jpg"`/`"png"`/`"gif"`/`"webp"`/`"bmp"`.
    pub ext: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// True for the book's declared cover (matched by backing bytes).
    pub is_cover: bool,
    pub byte_len: usize,
    /// Absolute path to a decoded preview copy on disk, for `convertFileSrc`.
    pub preview_path: String,
}

/// List every embedded image, writing a preview copy of each into a per-book
/// cache dir the webview can load. Read-only — the KFX is never touched.
#[tauri::command]
pub async fn editor_images(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<Vec<EditorImage>, String> {
    let kfx_path = {
        let conn = state.db.lock().await;
        let row = db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no book with id {book_id}"))?;
        require_kfx_source(&row)?
    };
    // A fresh dir per book keeps only the open book's previews on disk.
    let preview_dir = state
        .paths
        .root
        .join("editor-images")
        .join(book_id.to_string());
    tokio::task::spawn_blocking(move || extract_images_with_previews(&kfx_path, &preview_dir))
        .await
        .map_err(|e| e.to_string())?
}

/// Export one embedded image to a user-picked path (save dialog defaulting to
/// `<resource_name>.<ext>`). Returns the saved path, or `None` if cancelled.
#[tauri::command]
pub async fn editor_export_image(
    app: AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
    index: usize,
) -> Result<Option<String>, String> {
    let kfx_path = {
        let conn = state.db.lock().await;
        let row = db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no book with id {book_id}"))?;
        require_kfx_source(&row)?
    };
    // Re-extract and pick the one image by its stable index (the KFX is
    // unchanged between listing and export, so the sorted order is identical).
    let src = kfx_path.clone();
    let (bytes, default_name) =
        tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), String> {
            let data = std::fs::read(&src).map_err(|e| format!("read {src}: {e}"))?;
            let images = image_extract::kfx_extract_images(&data).map_err(|e| e.to_string())?;
            let img = images
                .get(index)
                .ok_or_else(|| "that image is no longer present".to_string())?;
            Ok((
                img.bytes.clone(),
                format!("{}.{}", sanitize_filename(&img.resource_name), img.ext),
            ))
        })
        .await
        .map_err(|e| e.to_string())??;

    let (tx, rx) = oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(default_name)
        .save_file(move |path| {
            let _ = tx.send(path);
        });
    let Some(dest) = rx.await.map_err(|e| e.to_string())? else {
        return Ok(None); // cancelled
    };
    let dest = PathBuf::from(dest.to_string());
    std::fs::write(&dest, &bytes).map_err(|e| format!("write {}: {e}", dest.display()))?;
    Ok(Some(dest.to_string_lossy().to_string()))
}

/// How many images an [`editor_export_images`] run wrote, and where.
#[derive(Serialize)]
pub struct ExportImagesResult {
    pub dir: String,
    pub count: usize,
}

/// Export every embedded image into `dir`, each named `<resource_name>.<ext>`
/// with a numeric suffix on any name collision.
#[tauri::command]
pub async fn editor_export_images(
    state: State<'_, AppState>,
    book_id: i64,
    dir: String,
) -> Result<ExportImagesResult, String> {
    let kfx_path = {
        let conn = state.db.lock().await;
        let row = db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no book with id {book_id}"))?;
        require_kfx_source(&row)?
    };
    let dest_dir = PathBuf::from(&dir);
    let count = tokio::task::spawn_blocking(move || -> Result<usize, String> {
        let data = std::fs::read(&kfx_path).map_err(|e| format!("read {kfx_path}: {e}"))?;
        let images = image_extract::kfx_extract_images(&data).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&dest_dir)
            .map_err(|e| format!("create {}: {e}", dest_dir.display()))?;
        let mut used: HashSet<String> = HashSet::new();
        for img in &images {
            let stem = sanitize_filename(&img.resource_name);
            let mut fname = format!("{stem}.{}", img.ext);
            let mut i = 1;
            while !used.insert(fname.clone()) {
                fname = format!("{stem}-{i}.{}", img.ext);
                i += 1;
            }
            let path = dest_dir.join(&fname);
            std::fs::write(&path, &img.bytes)
                .map_err(|e| format!("write {}: {e}", path.display()))?;
        }
        Ok(images.len())
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(ExportImagesResult { dir, count })
}

/// Extract every image and materialize a preview copy on disk. Sync (reads +
/// parses the container, writes files); call inside `spawn_blocking`.
fn extract_images_with_previews(
    kfx_path: &str,
    preview_dir: &Path,
) -> Result<Vec<EditorImage>, String> {
    let bytes = std::fs::read(kfx_path).map_err(|e| format!("read {kfx_path}: {e}"))?;
    let images = image_extract::kfx_extract_images(&bytes).map_err(|e| e.to_string())?;

    let _ = std::fs::remove_dir_all(preview_dir); // stale previews from a prior open
    std::fs::create_dir_all(preview_dir)
        .map_err(|e| format!("create {}: {e}", preview_dir.display()))?;

    let mut out = Vec::with_capacity(images.len());
    for (index, img) in images.iter().enumerate() {
        // Index-prefixed so two resources with the same sanitized name (e.g.
        // both non-ASCII) still land on distinct preview files.
        let fname = format!(
            "{index}-{}.{}",
            sanitize_filename(&img.resource_name),
            img.ext
        );
        let path = preview_dir.join(&fname);
        std::fs::write(&path, &img.bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
        out.push(EditorImage {
            index,
            resource_name: img.resource_name.clone(),
            ext: img.ext.to_string(),
            width: img.width,
            height: img.height,
            is_cover: img.is_cover,
            byte_len: img.bytes.len(),
            preview_path: path.to_string_lossy().to_string(),
        });
    }
    Ok(out)
}

/// Reduce a KFX resource name to a filesystem-safe stem: keep ASCII
/// alphanumerics plus `.`/`-`/`_`, collapse the rest to `_` (so
/// `resource/rsrc7` → `resource_rsrc7`). Falls back to `image` when nothing
/// printable survives.
fn sanitize_filename(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches('_');
    if trimmed.is_empty() {
        "image".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Validate-only TOC verdict for the top-bar chip (no proposal — cheaper than
/// [`read_toc_detail`], which also parses the Contents page). `None` on a read
/// or parse error. Sync — call inside `spawn_blocking`.
fn compute_toc(kfx_path: &str) -> Option<EditorToc> {
    let bytes = std::fs::read(kfx_path).ok()?;
    let audit = toc_validate::validate(&bytes).ok()?;
    Some(EditorToc {
        verdict: audit.verdict.as_str().to_string(),
        nav_count: audit.nav_count,
        nav_chapters: audit.nav_chapters,
        contents_links: audit.contents_links,
        headings: audit.headings,
        section_heads: audit.section_heads,
    })
}

/// Full TOC panel state — verdict + current labels + the proposal (nesting
/// preserved). Sync (reads + parses the container); call inside `spawn_blocking`.
fn read_toc_detail(kfx_path: &str) -> Result<EditorTocDetail, String> {
    let bytes = std::fs::read(kfx_path).map_err(|e| format!("read {kfx_path}: {e}"))?;
    let audit = toc_validate::validate(&bytes)?;
    let (proposed, note) = match toc_repair::propose_toc(&bytes) {
        Ok(entries) if !entries.is_empty() => {
            (entries.iter().map(TocEntryDto::from_entry).collect(), None)
        }
        Ok(_) => (
            Vec::new(),
            Some("No chapter list found on an in-book Contents page.".to_string()),
        ),
        Err(e) => (
            Vec::new(),
            Some(format!("Couldn't auto-derive chapters: {e}")),
        ),
    };
    Ok(EditorTocDetail {
        verdict: audit.verdict.as_str().to_string(),
        nav_count: audit.nav_count,
        nav_chapters: audit.nav_chapters,
        current: audit.nav_labels,
        proposed,
        note,
    })
}

/// The editor's v1 gate: succeed with the KFX path only for a KFX-source book
/// that has its KFX on disk.
fn require_kfx_source(row: &BookRow) -> Result<String, String> {
    let format = source_format(row.kind.as_deref());
    if format != "kfx" {
        return Err(format!(
            "the editor edits KFX-source books; this is a {format}-source book (coming soon)"
        ));
    }
    row.kfx_path
        .clone()
        .ok_or_else(|| "this book has no KFX file yet".to_string())
}

/// The save seam. Commit `new_bytes` to `kfx_path` in place: back up the
/// original, write atomically (temp + rename), preserve the frozen `kfx_sha256`
/// device identity, and drop the backup once the file is settled. Rolls the file
/// back from the backup if the replace fails partway. The caller handles the
/// library-row sync, the derived-EPUB reconvert, and the reader eviction.
async fn commit_edited_kfx(
    state: &AppState,
    book_id: i64,
    kfx_path: &str,
    new_bytes: Vec<u8>,
) -> Result<(), String> {
    let path = kfx_path.to_string();
    let new_sha = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let target = Path::new(&path);
        let backup = sibling(target, "editbak");
        let temp = sibling(target, "editing");

        // Back up the original — required for rollback, so a failure here aborts
        // before we've touched the live file.
        std::fs::copy(target, &backup).map_err(|e| format!("back up {}: {e}", target.display()))?;

        // Write the new bytes to a sibling temp, then atomically rename over the
        // target so a crash mid-write can't leave a truncated KFX.
        if let Err(e) = std::fs::write(&temp, &new_bytes) {
            let _ = std::fs::remove_file(&backup);
            return Err(format!("write {}: {e}", temp.display()));
        }
        if let Err(e) = std::fs::rename(&temp, target) {
            let _ = std::fs::remove_file(&temp);
            let _ = std::fs::copy(&backup, target); // best-effort restore
            let _ = std::fs::remove_file(&backup);
            return Err(format!("replace {}: {e}", target.display()));
        }

        let sha = sidle_core::library::import::sha256_of_file(target).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&backup); // settled — tidy the backup
        Ok(sha)
    })
    .await
    .map_err(|e| e.to_string())??;

    // COALESCE keeps the existing `kfx_sha256`, so the on-device filename and
    // annotation-sync identity stay stable even though the bytes changed.
    {
        let conn = state.db.lock().await;
        db::set_kfx_path_and_sha(&conn, book_id, kfx_path, &new_sha).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `path` with an extra dot-suffix, e.g. `book.kfx` + `"editbak"` →
/// `book.kfx.editbak`. Kept in the same directory as `path` so the temp→target
/// rename is atomic (same filesystem).
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

/// Trim a submitted optional string; an empty result clears the field (`None`).
fn trim_opt(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_format_from_kind() {
        assert_eq!(source_format(Some("kfx_to_epub")), "kfx");
        assert_eq!(source_format(Some("epub_to_kfx")), "epub");
        assert_eq!(source_format(Some("pdf_to_kfx")), "pdf");
        // Missing kind falls back to the EPUB-source default (matches the frontend).
        assert_eq!(source_format(None), "epub");
    }

    #[test]
    fn sibling_appends_suffix_in_same_dir() {
        let p = Path::new("/books/abc/book.kfx");
        assert_eq!(
            sibling(p, "editbak"),
            Path::new("/books/abc/book.kfx.editbak")
        );
        assert_eq!(
            sibling(p, "editing"),
            Path::new("/books/abc/book.kfx.editing")
        );
    }

    #[test]
    fn trim_opt_clears_blank() {
        assert_eq!(
            trim_opt(Some("  Shinchosha ".into())),
            Some("Shinchosha".into())
        );
        assert_eq!(trim_opt(Some("   ".into())), None);
        assert_eq!(trim_opt(Some(String::new())), None);
        assert_eq!(trim_opt(None), None);
    }

    #[test]
    fn toc_dto_round_trips_nesting_faithfully() {
        // A nested source (Part → chapters) must survive DTO → wire → TocEntry
        // with its shape intact: no flatten, no re-nest.
        let src = [
            TocEntry {
                label: "Part I".into(),
                eid: 1,
                children: vec![TocEntry::new("Ch 1", 2), TocEntry::new("Ch 2", 3)],
            },
            TocEntry::new("Afterword", 4),
        ];
        let dto: Vec<TocEntryDto> = src.iter().map(TocEntryDto::from_entry).collect();
        let back: Vec<TocEntry> = dto.into_iter().map(TocEntryDto::into_entry).collect();

        assert_eq!(back.len(), 2);
        assert_eq!(back[0].label, "Part I");
        assert_eq!(back[0].eid, 1);
        assert_eq!(back[0].children.len(), 2);
        assert_eq!(back[0].children[1].label, "Ch 2");
        assert_eq!(back[0].children[1].eid, 3);
        // The Afterword stays a top-level leaf.
        assert_eq!(back[1].label, "Afterword");
        assert!(back[1].children.is_empty());

        // A flat source stays flat.
        let flat = [TocEntry::new("One", 10), TocEntry::new("Two", 20)];
        let back: Vec<TocEntry> = flat
            .iter()
            .map(TocEntryDto::from_entry)
            .map(TocEntryDto::into_entry)
            .collect();
        assert!(back.iter().all(|e| e.children.is_empty()));
    }

    #[test]
    fn any_blank_label_finds_nested_blanks() {
        let ok = [TocEntry::new("A", 1), TocEntry::new("B", 2)];
        let ok_dto: Vec<TocEntryDto> = ok.iter().map(TocEntryDto::from_entry).collect();
        assert!(!any_blank_label(&ok_dto));

        // A blank label buried under a Part must still be caught.
        let bad = [TocEntry {
            label: "Part I".into(),
            eid: 1,
            children: vec![TocEntry::new("  ", 2)],
        }];
        let bad_dto: Vec<TocEntryDto> = bad.iter().map(TocEntryDto::from_entry).collect();
        assert!(any_blank_label(&bad_dto));
    }

    #[test]
    fn sanitize_filename_keeps_safe_chars() {
        assert_eq!(sanitize_filename("resource/rsrc7"), "resource_rsrc7");
        assert_eq!(sanitize_filename("eF"), "eF");
        assert_eq!(sanitize_filename("cover-1.2"), "cover-1.2");
        // Leading/trailing separators are trimmed; an all-symbol name falls back.
        assert_eq!(sanitize_filename("/leading"), "leading");
        assert_eq!(sanitize_filename("图片"), "image");
        assert_eq!(sanitize_filename(""), "image");
    }
}
