//! Book editor — the app-UI surface over bokai's KFX, EPUB and PDF edit
//! primitives.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::oneshot;

use bokai::formats::epub::image_extract as epub_image;
use bokai::formats::epub::spine_repair as epub_spine;
use bokai::formats::epub::toc_repair as epub_toc;
use bokai::formats::kfx::image_extract;
use bokai::formats::kfx::toc_repair::{self, TocEntry as KfxTocEntry};
use bokai::formats::pdf::cover::{self as pdf_cover, CoverMode};
use bokai::formats::pdf::toc_repair as pdf_toc;
use bokai::import::pdf::PdfOutlineItem;
use bokai::model::TocEntry as EpubTocEntry;
use bokai::validate::source::toc as toc_validate;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use sidle_core::library::editor as text_editor;
use sidle_core::library::source::{self, Source as SourceKind};

use crate::commands::library::CoverResult;
use crate::library::{db, db::BookRow, metadata};
use crate::state::AppState;

/// Resolve a book's editable source: its format + on-disk path. Errors for an
/// unrecognized source format, or when the source file is missing.
fn require_editable_source(row: &BookRow) -> Result<(SourceKind, String), String> {
    source::of(row).map_err(|e| format!("{e:#}"))
}

/// Which editor panels an editable source can back; the rail follows it.
fn editor_panels(kind: SourceKind) -> Vec<String> {
    let mut panels: Vec<String> = ["metadata", "toc", "cover", "images"]
        .into_iter()
        .map(str::to_string)
        .collect();
    if kind == SourceKind::Epub {
        panels.push("spine".to_string());
        panels.push("text".to_string());
    }
    panels
}

/// Fetch a book row and resolve its editable source (kind + path) — the shared
/// preamble of every editor command.
async fn editor_source(
    state: &State<'_, AppState>,
    book_id: i64,
) -> Result<(SourceKind, String), String> {
    let conn = state.db.lock().await;
    let row = db::get_book(&conn, book_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no book with id {book_id}"))?;
    require_editable_source(&row)
}

/// Source format a book was imported *from*.
fn source_format(kind: Option<&str>) -> String {
    source::format_of(kind).to_string()
}

/// TOC health from the source bytes, mirroring `validate::source::toc::TocAudit`.
#[derive(Serialize)]
pub struct EditorToc {
    /// `"OK"` | `"SUSPECT"` | `"FLATTENED"` | `"SPARSE"`.
    pub verdict: String,
    pub nav_count: usize,
    pub nav_chapters: usize,
    pub contents_links: usize,
    pub headings: usize,
    pub section_heads: usize,
    /// On a `"FLATTENED"` verdict, the volumes the TOC lists at one depth; else 0.
    pub flattened_volumes: usize,
    /// On a `"FLATTENED"` verdict, the entries belonging under them; else 0.
    pub flattened_entries: usize,
}

/// Current metadata for the metadata panel. `author` is the author line
/// (`"Jane Smith & John Doe"`); a save replaces the authors with its names and
/// keeps every other credit.
#[derive(Serialize)]
pub struct EditorMetadata {
    pub title: String,
    pub author: String,
    /// Every credit with its roles; the panel lists those beside the authors.
    pub credits: Vec<crate::library::credits::Credit>,
    pub language: String,
    pub publisher: Option<String>,
    pub published_at: Option<String>,
    /// The catalogue ASIN, editable; it fetches the colour cover and stays in the row.
    pub amazon_asin: Option<String>,
    /// The identity the file carries as both `ASIN` and `content_id`; read-only.
    pub content_id: Option<String>,
}

/// The editor's opening snapshot for one book — what the shell renders from.
#[derive(Serialize)]
pub struct EditorOpen {
    pub book_id: i64,
    /// Source format: `"kfx"` | `"epub"` | `"pdf"`.
    pub format: String,
    /// True when the source file is on disk and its format has a write path.
    pub editable: bool,
    /// The panels this source can back, empty when not editable; see [`editor_panels`].
    pub panels: Vec<String>,
    pub metadata: EditorMetadata,
    pub has_cover: bool,
    /// `None` when the TOC verdict couldn't be computed (read or parse error).
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
    let source = require_editable_source(&row).ok();
    let editable = source.is_some();
    let panels = match &source {
        Some((kind, _)) => editor_panels(*kind),
        None => Vec::new(),
    };

    // TOC verdict from the *source* bytes. Off the async thread — it's a full
    // container/zip/PDF parse.
    let toc = match source {
        Some((kind, path)) => tokio::task::spawn_blocking(move || compute_toc(&path, kind))
            .await
            .map_err(|e| e.to_string())?,
        None => None,
    };

    Ok(EditorOpen {
        book_id,
        format,
        editable,
        panels,
        metadata: EditorMetadata {
            title: row.title.clone(),
            author: row.author.clone(),
            credits: row.credits.clone(),
            language: row.language.clone(),
            publisher: row.publisher.clone(),
            published_at: row.published_at.clone(),
            amazon_asin: row.amazon_asin.clone(),
            content_id: row.asin.clone(),
        },
        has_cover: row.cover_path.is_some(),
        toc,
    })
}

/// Fields the metadata panel submits; every field is present on every save.
#[derive(Deserialize)]
pub struct MetadataForm {
    pub title: String,
    /// Raw author line (`"A & B"`) — parsed and canonicalized backend-side.
    pub author: String,
    pub language: String,
    pub publisher: Option<String>,
    pub published_at: Option<String>,
    pub amazon_asin: Option<String>,
}

/// Write edited metadata into the library row and every file of the book. The
/// catalogue ASIN stays in the row.
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
    if form.title.trim().is_empty() {
        return Err("title cannot be empty".into());
    }
    {
        let conn = state.db.lock().await;
        let amazon_asin = metadata::check_amazon_asin(&conn, book_id, form.amazon_asin.as_deref())
            .map_err(|e| format!("{e:#}"))?;
        metadata::set_amazon_asin(&conn, book_id, amazon_asin.as_deref())
            .map_err(|e| format!("{e:#}"))?;
    }
    let patch = db::MetadataPatch {
        title: form.title,
        author: form.author,
        credits: None,
        language: form.language,
        ppd: row.ppd.clone(),
        writing_mode: row.writing_mode.clone(),
        publisher: form.publisher,
        published_at: form.published_at,
        series_name: row.series_name.clone(),
        series_index: row.series_index,
        tags: row.tags.clone(),
        title_romaji: String::new(),
        author_romaji: String::new(),
    };
    crate::commands::library::apply_metadata_patch(&app, &state, book_id, patch).await
}

// --- cover (PDF) ----------------------------------------------------------

/// Give a PDF-source book a cover page: the PDF analog of `library_set_cover`.
#[tauri::command]
pub async fn editor_set_pdf_cover(
    app: AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
    src_path: String,
    mode: String,
) -> Result<CoverResult, String> {
    let mode = match mode.as_str() {
        "replace" => CoverMode::Replace,
        "insert" => CoverMode::Insert,
        other => return Err(format!("unknown cover mode {other:?}")),
    };
    let row = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no book with id {book_id}"))?
    };
    let (kind, path) = require_editable_source(&row)?;
    if kind != SourceKind::Pdf {
        return Err("this command is for PDF-source books only".into());
    }

    let image = match std::fs::read(&src_path) {
        Ok(b) => b,
        Err(e) => {
            return Ok(CoverResult::Failed {
                error: format!("read {src_path}: {e}"),
            });
        }
    };
    let Some(ext) = sidle_core::library::cover::sniff_image_format(&image) else {
        return Ok(CoverResult::Failed {
            error: "unsupported image format (expected JPG, PNG, or WebP)".into(),
        });
    };

    // 1) Write the cover page into the PDF and commit it.
    let (src, img) = (path.clone(), image.clone());
    let edited = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let bytes = std::fs::read(&src).map_err(|e| format!("read {src}: {e}"))?;
        pdf_cover::set_cover_page(&bytes, &img, mode).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?;
    let new_bytes = match edited {
        Ok(b) => b,
        Err(error) => return Ok(CoverResult::Failed { error }),
    };
    commit_edited_source(&state, book_id, kind, &path, new_bytes).await?;

    // 2) Point the library at the same image; the tile shows it before the
    //    reconvert renders the page.
    let out = state.paths.cover(&row.sha256, ext);
    std::fs::write(&out, &image).map_err(|e| format!("write {}: {e}", out.display()))?;
    let out_str = out.to_string_lossy().to_string();
    {
        let conn = state.db.lock().await;
        db::set_cover_path(&conn, book_id, &out_str).map_err(|e| e.to_string())?;
    }
    let _ = crate::library::thumbnail::ensure_thumbnail(&state.paths, &row.sha256, &out);
    // Remove a previous cover under a different extension.
    if let Some(old) = row.cover_path.as_deref()
        && old != out_str.as_str()
    {
        let _ = std::fs::remove_file(old);
    }

    // 3) Re-derive the KFX from the edited PDF and refresh the UI.
    let _ = state.queue.enqueue_reconvert(book_id).await;
    crate::commands::reader::evict_reader(&state, book_id).await;

    if let Ok(Some(updated)) = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id)
    } {
        let _ = app.emit("library:row-updated", &updated);
    }
    Ok(CoverResult::Updated {
        cover_path: out_str,
    })
}

/// One TOC entry crossing the wire — the full `TocEntry` tree, nesting intact.
#[derive(Serialize, Deserialize, Clone)]
pub struct TocEntryDto {
    pub label: String,
    /// KFX target — the `$155` element id; `0` for an EPUB entry, which targets
    /// by `href`.
    #[serde(default)]
    pub eid: i64,
    /// EPUB target — an absolute zip-path href (e.g. `"OEBPS/c1.xhtml#ch1"`);
    /// empty for a KFX entry.
    #[serde(default)]
    pub href: String,
    /// PDF target — a 1-based page number.
    #[serde(default)]
    pub page: usize,
    #[serde(default)]
    pub children: Vec<TocEntryDto>,
}

impl TocEntryDto {
    /// KFX proposer `TocEntry` → wire DTO, recursively.
    fn from_kfx(e: &KfxTocEntry) -> Self {
        Self {
            label: e.label.clone(),
            eid: e.eid,
            href: String::new(),
            page: 0,
            children: e.children.iter().map(Self::from_kfx).collect(),
        }
    }

    /// Wire DTO → KFX `TocEntry` for `kfx::toc_repair::set_toc`, trimming labels.
    fn into_kfx(self) -> KfxTocEntry {
        KfxTocEntry {
            label: self.label.trim().to_string(),
            eid: self.eid,
            children: self.children.into_iter().map(Self::into_kfx).collect(),
        }
    }

    /// A PDF outline item → the read-only "currently declared" DTO. The page it
    /// jumps to goes in the label; this side has no page input.
    fn declared_pdf(e: &PdfOutlineItem) -> Self {
        Self {
            label: format!("{} — p.{}", e.title, e.page_index + 1),
            eid: 0,
            href: String::new(),
            page: 0,
            children: e.children.iter().map(Self::declared_pdf).collect(),
        }
    }

    /// A declared-TOC entry → wire DTO for the read-only "currently declared"
    /// side: its label and its shape, no target.
    fn label_only(e: &EpubTocEntry) -> Self {
        Self {
            label: e.title.clone(),
            eid: 0,
            href: String::new(),
            page: 0,
            children: e.children.iter().map(Self::label_only).collect(),
        }
    }

    /// EPUB proposer `TocEntry` (title + href) → wire DTO, recursively.
    fn from_epub(e: &EpubTocEntry) -> Self {
        Self {
            label: e.title.clone(),
            eid: 0,
            href: e.href.clone(),
            page: 0,
            children: e.children.iter().map(Self::from_epub).collect(),
        }
    }

    /// PDF outline item → wire DTO. `page_index` is 0-based; the panel shows and
    /// edits 1-based page numbers.
    fn from_pdf(e: &PdfOutlineItem) -> Self {
        Self {
            label: e.title.clone(),
            eid: 0,
            href: String::new(),
            page: e.page_index + 1,
            children: e.children.iter().map(Self::from_pdf).collect(),
        }
    }

    /// Wire DTO → `PdfOutlineItem` for `pdf::toc_repair::set_toc`. A `page` of 0
    /// (unset by the panel) clamps to page 1; the primitive rejects a page past
    /// the last one and names the entry.
    fn into_pdf(self) -> PdfOutlineItem {
        PdfOutlineItem {
            title: self.label.trim().to_string(),
            page_index: self.page.saturating_sub(1),
            children: self.children.into_iter().map(Self::into_pdf).collect(),
        }
    }

    /// Wire DTO → EPUB `TocEntry` for `epub::toc_repair::set_toc`, trimming labels.
    fn into_epub(self) -> EpubTocEntry {
        let mut t = EpubTocEntry::new(self.label.trim(), self.href);
        t.children = self.children.into_iter().map(Self::into_epub).collect();
        t
    }
}

/// True if any entry in the tree has a blank label; `set_toc` rejects it.
fn any_blank_label(entries: &[TocEntryDto]) -> bool {
    entries
        .iter()
        .any(|e| e.label.trim().is_empty() || any_blank_label(&e.children))
}

/// Full state for the TOC panel: the current declared TOC, the verdict, and a
/// proposed chapter list derived from the book's own in-book Contents page.
#[derive(Serialize)]
pub struct EditorTocDetail {
    /// `"OK"` | `"SUSPECT"` | `"FLATTENED"` | `"SPARSE"`.
    pub verdict: String,
    pub nav_count: usize,
    pub nav_chapters: usize,
    /// On a `"FLATTENED"` verdict, the volumes listed at one depth; else 0.
    pub flattened_volumes: usize,
    /// On a `"FLATTENED"` verdict, the entries the rebuild nests under them; else 0.
    pub flattened_entries: usize,
    /// The TOC the book declares, as a tree of labels.
    pub current: Vec<TocEntryDto>,
    /// The editable starting tree: for KFX/EPUB, the declared TOC merged with the Contents page.
    pub proposed: Vec<TocEntryDto>,
    /// Set when no proposal could be derived, explaining why.
    pub note: Option<String>,
    /// `Some(n)` for a PDF source, whose entries target pages `1..=n`; `None` for KFX/EPUB.
    pub page_count: Option<usize>,
    /// True when the format has an automatic proposer (`editor_repair_toc`).
    pub can_auto_repair: bool,
}

/// Read the TOC panel state: verdict, current TOC and proposal.
#[tauri::command]
pub async fn editor_toc(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<EditorTocDetail, String> {
    let (kind, path) = editor_source(&state, book_id).await?;
    tokio::task::spawn_blocking(move || read_toc_detail(&path, kind))
        .await
        .map_err(|e| e.to_string())?
}

/// Write a reviewed TOC (`entries`, in order) into the source and re-derive;
/// returns the refreshed panel state.
#[tauri::command]
pub async fn editor_set_toc(
    state: State<'_, AppState>,
    book_id: i64,
    entries: Vec<TocEntryDto>,
) -> Result<EditorTocDetail, String> {
    let (kind, path) = editor_source(&state, book_id).await?;
    if entries.is_empty() {
        return Err("a table of contents needs at least one entry".into());
    }
    if any_blank_label(&entries) {
        return Err("every table-of-contents entry needs a label".into());
    }

    // Preserve the tree exactly — no flatten, no re-nest — and write it via the
    // source format's own primitive (KFX targets by eid, EPUB by href).
    let src = path.clone();
    let new_bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let bytes = std::fs::read(&src).map_err(|e| format!("read {src}: {e}"))?;
        match kind {
            SourceKind::Kfx => {
                let toc: Vec<KfxTocEntry> =
                    entries.into_iter().map(TocEntryDto::into_kfx).collect();
                toc_repair::set_toc(&bytes, &toc).map_err(|e| e.to_string())
            }
            SourceKind::Epub => {
                let toc: Vec<EpubTocEntry> =
                    entries.into_iter().map(TocEntryDto::into_epub).collect();
                epub_toc::set_toc(&bytes, &toc).map_err(|e| e.to_string())
            }
            SourceKind::Pdf => {
                let toc: Vec<PdfOutlineItem> =
                    entries.into_iter().map(TocEntryDto::into_pdf).collect();
                pdf_toc::set_toc(&bytes, &toc).map_err(|e| e.to_string())
            }
        }
    })
    .await
    .map_err(|e| e.to_string())??;

    commit_edited_source(&state, book_id, kind, &path, new_bytes).await?;
    let _ = state.queue.enqueue_reconvert(book_id).await;
    crate::commands::reader::evict_reader(&state, book_id).await;

    let p = path.clone();
    tokio::task::spawn_blocking(move || read_toc_detail(&p, kind))
        .await
        .map_err(|e| e.to_string())?
}

/// One-click repair: derive the chapter list and write it in one pass
/// (`propose + set`).
#[tauri::command]
pub async fn editor_repair_toc(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<EditorTocDetail, String> {
    let (kind, path) = editor_source(&state, book_id).await?;

    let src = path.clone();
    let new_bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let bytes = std::fs::read(&src).map_err(|e| format!("read {src}: {e}"))?;
        match kind {
            SourceKind::Kfx => toc_repair::repair_toc(&bytes).map_err(|e| e.to_string()),
            SourceKind::Epub => epub_toc::repair_toc(&bytes).map_err(|e| e.to_string()),
            // No PDF proposer exists; the panel hides the button via
            // `EditorTocDetail::can_auto_repair`.
            SourceKind::Pdf => Err(
                "a PDF's table of contents can't be derived automatically — add \
                 the entries by hand"
                    .to_string(),
            ),
        }
    })
    .await
    .map_err(|e| e.to_string())??;

    commit_edited_source(&state, book_id, kind, &path, new_bytes).await?;
    let _ = state.queue.enqueue_reconvert(book_id).await;
    crate::commands::reader::evict_reader(&state, book_id).await;

    let p = path.clone();
    tokio::task::spawn_blocking(move || read_toc_detail(&p, kind))
        .await
        .map_err(|e| e.to_string())?
}

// --- reading order (spine) --------------------------------------------------

/// One spine document as the reading-order panel lists it.
#[derive(Serialize, Deserialize, Clone)]
pub struct SpineDocDto {
    /// The manifest id: the panel's handle, and all a write sends back.
    pub idref: String,
    /// The declared TOC's label for this document, else its filename.
    pub label: String,
    /// True when the label came from the TOC.
    pub named: bool,
}

/// Full state for the reading-order panel: what the spine reads today, what the
/// book's own navigation implies it should read, and the measurement between.
#[derive(Serialize)]
pub struct EditorSpineDetail {
    /// `"OK"` | `"MISORDERED"`.
    pub verdict: String,
    /// Places where the spine reads the declared TOC's entries out of order.
    pub descents: usize,
    /// How many documents the proposal moves.
    pub moved: usize,
    /// The spine is its own manifest sorted lexicographically, a packaging artifact.
    pub machine_sorted: bool,
    /// The first entry the spine reads late, for the panel's one-line summary.
    pub first_out_of_order: Option<String>,
    /// The spine as the book declares it.
    pub current: Vec<SpineDocDto>,
    /// The order the book's own navigation implies; equal to `current` when they agree.
    pub proposed: Vec<SpineDocDto>,
}

/// Read the reading-order panel state. EPUB-only; the rail offers the panel
/// for an EPUB source ([`editor_panels`]).
#[tauri::command]
pub async fn editor_spine(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<EditorSpineDetail, String> {
    let (kind, path) = editor_source(&state, book_id).await?;
    tokio::task::spawn_blocking(move || read_spine_detail(&path, kind))
        .await
        .map_err(|e| e.to_string())?
}

/// Write a reviewed reading order (`order`, a list of manifest ids) into the
/// source EPUB and re-derive the KFX.
#[tauri::command]
pub async fn editor_set_spine(
    state: State<'_, AppState>,
    book_id: i64,
    order: Vec<String>,
) -> Result<EditorSpineDetail, String> {
    let (kind, path) = editor_source(&state, book_id).await?;
    if kind != SourceKind::Epub {
        return Err(spine_unsupported(kind).to_string());
    }
    if order.is_empty() {
        return Err("a reading order needs at least one document".into());
    }

    let src = path.clone();
    let new_bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let bytes = std::fs::read(&src).map_err(|e| format!("read {src}: {e}"))?;
        epub_spine::set_spine(&bytes, &order).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;

    commit_edited_source(&state, book_id, kind, &path, new_bytes).await?;
    let _ = state.queue.enqueue_reconvert(book_id).await;
    crate::commands::reader::evict_reader(&state, book_id).await;

    let p = path.clone();
    tokio::task::spawn_blocking(move || read_spine_detail(&p, kind))
        .await
        .map_err(|e| e.to_string())?
}

/// Why a source format has no reading order to permute.
fn spine_unsupported(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Kfx => {
            "a KFX's reading order carries every reading position \
                            in the book with it, so reordering it is a rebuild \
                            rather than a reorder — not built yet"
        }
        SourceKind::Pdf => {
            "a PDF's reading order is its page order, which this \
                            editor doesn't rearrange"
        }
        SourceKind::Epub => "",
    }
}

fn read_spine_detail(source_path: &str, kind: SourceKind) -> Result<EditorSpineDetail, String> {
    if kind != SourceKind::Epub {
        return Err(spine_unsupported(kind).to_string());
    }
    let bytes = std::fs::read(source_path).map_err(|e| format!("read {source_path}: {e}"))?;
    let m = epub_spine::declared_spine_misordering(&bytes).map_err(|e| e.to_string())?;
    let dto = |d: epub_spine::SpineDoc| SpineDocDto {
        named: d.label.is_some(),
        label: d
            .label
            .unwrap_or_else(|| d.href.rsplit('/').next().unwrap_or(&d.href).to_string()),
        idref: d.idref,
    };
    Ok(EditorSpineDetail {
        verdict: if m.contradicts() { "MISORDERED" } else { "OK" }.to_string(),
        descents: m.descents,
        moved: m.moved,
        machine_sorted: m.machine_sorted,
        first_out_of_order: m.first_out_of_order.clone(),
        current: epub_spine::current_spine(&bytes)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(dto)
            .collect(),
        proposed: epub_spine::propose_spine(&bytes)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(dto)
            .collect(),
    })
}

// --- images ---------------------------------------------------------------

/// One embedded image for the Images panel (KFX/EPUB): identity + dimensions +
/// an on-disk preview copy the webview loads through the asset protocol. The
/// panel offers extract/export only; an image cannot be *replaced* through it.
#[derive(Serialize)]
pub struct EditorImage {
    /// Position in the source's sorted image list, the key [`editor_export_image`] takes.
    pub index: usize,
    /// Display name, in the source format's own terms — see [`RawImage`].
    pub resource_name: String,
    /// Lowercase extension, no dot: `"jpg"`/`"png"`/`"gif"`/`"webp"`/`"bmp"`.
    pub ext: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// True for the book's declared cover, matched by backing bytes.
    pub is_cover: bool,
    pub byte_len: usize,
    /// Absolute path to a decoded preview copy on disk, for `convertFileSrc`.
    pub preview_path: String,
}

/// List every embedded image, writing a preview copy of each into a per-book
/// cache dir the webview can load. Read-only — the source is never touched.
#[tauri::command]
pub async fn editor_images(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<Vec<EditorImage>, String> {
    let (kind, path) = editor_source(&state, book_id).await?;
    // A fresh dir per book keeps only the open book's previews on disk.
    let preview_dir = state
        .paths
        .root
        .join("editor-images")
        .join(book_id.to_string());
    tokio::task::spawn_blocking(move || extract_images_with_previews(&path, kind, &preview_dir))
        .await
        .map_err(|e| e.to_string())?
}

/// Export one embedded image through a save dialog defaulting to
/// `<resource_name>.<ext>`; returns the saved path, or `None` if cancelled.
#[tauri::command]
pub async fn editor_export_image(
    app: AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
    index: usize,
) -> Result<Option<String>, String> {
    let (kind, path) = editor_source(&state, book_id).await?;
    // Re-extract and pick the one image by its stable index; the sorted order
    // is the listing's.
    let src = path.clone();
    let (bytes, default_name) =
        tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), String> {
            let images = extract_images(&src, kind)?;
            let img = images
                .get(index)
                .ok_or_else(|| "that image is no longer present".to_string())?;
            Ok((
                img.bytes.clone(),
                format!("{}.{}", sanitize_filename(&img.name), img.ext),
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
    let (kind, path) = editor_source(&state, book_id).await?;
    let dest_dir = PathBuf::from(&dir);
    let count = tokio::task::spawn_blocking(move || -> Result<usize, String> {
        let images = extract_images(&path, kind)?;
        std::fs::create_dir_all(&dest_dir)
            .map_err(|e| format!("create {}: {e}", dest_dir.display()))?;
        let mut used: HashSet<String> = HashSet::new();
        for img in &images {
            let stem = sanitize_filename(&img.name);
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

/// One extracted image, normalized across the source formats that *have* an
/// embedded-image list — KFX and EPUB. `name` is a display / export stem: a KFX
/// `resource_name`, or an EPUB member's filename stem.
struct RawImage {
    name: String,
    ext: String,
    width: Option<u32>,
    height: Option<u32>,
    is_cover: bool,
    bytes: Vec<u8>,
}

/// Extract every embedded image from the source, normalized to [`RawImage`] and
/// deterministically ordered: an index is a stable handle across calls. Blocking.
fn extract_images(source_path: &str, kind: SourceKind) -> Result<Vec<RawImage>, String> {
    let bytes = std::fs::read(source_path).map_err(|e| format!("read {source_path}: {e}"))?;
    let out = match kind {
        SourceKind::Kfx => image_extract::kfx_extract_images(&bytes)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|i| RawImage {
                name: i.resource_name,
                ext: i.ext.to_string(),
                width: i.width,
                height: i.height,
                is_cover: i.is_cover,
                bytes: i.bytes,
            })
            .collect(),
        SourceKind::Epub => epub_image::epub_extract_images(&bytes)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|i| RawImage {
                name: image_stem(&i.path),
                ext: i.ext.to_string(),
                width: i.width,
                height: i.height,
                is_cover: i.is_cover,
                bytes: i.bytes,
            })
            .collect(),
        // A PDF's pages are its images; the panel takes [`editor_pdf_pages`]
        // and never calls this arm.
        SourceKind::Pdf => {
            return Err("a PDF's images are its pages — export them instead".to_string());
        }
    };
    Ok(out)
}

/// The filename stem of a zip member path — `"OEBPS/img/fig1.jpg"` → `"fig1"` —
/// used as an EPUB image's display / export name (the extension is added back).
fn image_stem(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(base)
        .to_string()
}

/// Extract every image and write a preview copy of each to disk. Blocking.
fn extract_images_with_previews(
    source_path: &str,
    kind: SourceKind,
    preview_dir: &Path,
) -> Result<Vec<EditorImage>, String> {
    let images = extract_images(source_path, kind)?;

    let _ = std::fs::remove_dir_all(preview_dir); // stale previews from a prior open
    std::fs::create_dir_all(preview_dir)
        .map_err(|e| format!("create {}: {e}", preview_dir.display()))?;

    let mut out = Vec::with_capacity(images.len());
    for (index, img) in images.iter().enumerate() {
        // Index-prefixed: same-named resources get distinct preview files.
        let fname = format!("{index}-{}.{}", sanitize_filename(&img.name), img.ext);
        let path = preview_dir.join(&fname);
        std::fs::write(&path, &img.bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
        out.push(EditorImage {
            index,
            resource_name: img.name.clone(),
            ext: img.ext.clone(),
            width: img.width,
            height: img.height,
            is_cover: img.is_cover,
            byte_len: img.bytes.len(),
            preview_path: path.to_string_lossy().to_string(),
        });
    }
    Ok(out)
}

// --- PDF page export ------------------------------------------------------

/// One page offered for export. Sizes are in PDF points (`/Rotate` applied);
/// the panel lays out an aspect-correct card from them.
#[derive(Serialize)]
pub struct EditorPdfPage {
    /// 1-based page number — the handle [`editor_export_pdf_page`] takes.
    pub page: usize,
    pub width_pt: f32,
    pub height_pt: f32,
}

/// List a PDF's pages for the Images panel. Thumbnails are not rendered here;
/// the panel asks `reader_pdf_page` for each one as its card scrolls into view.
#[tauri::command]
pub async fn editor_pdf_pages(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<Vec<EditorPdfPage>, String> {
    let (kind, path) = editor_source(&state, book_id).await?;
    if kind != SourceKind::Pdf {
        return Err("this book's source isn't a PDF".to_string());
    }
    tokio::task::spawn_blocking(move || -> Result<Vec<EditorPdfPage>, String> {
        Ok(probe_pdf_pages(&path)?
            .into_iter()
            .enumerate()
            .map(|(i, p)| EditorPdfPage {
                page: i + 1,
                width_pt: p.width,
                height_pt: p.height,
            })
            .collect())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Read a PDF's page geometry. Blocking.
fn probe_pdf_pages(path: &str) -> Result<Vec<bokai::import::pdf::PdfPage>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    bokai::import::pdf::probe_pdf(bytes)
        .map(|d| d.pages)
        .map_err(|e| e.to_string())
}

/// How a page is encoded on export: JPEG for scans, PNG for text and line art.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PageFormat {
    Jpeg,
    Png,
}

impl PageFormat {
    fn ext(self) -> &'static str {
        match self {
            PageFormat::Jpeg => "jpg",
            PageFormat::Png => "png",
        }
    }
}

/// JPEG quality for an exported page, above the library cover's 85.
const PAGE_EXPORT_JPEG_QUALITY: u8 = 90;

/// Pixel width for a page `width_pt` points wide rendered at `dpi`; a PDF point
/// is 1/72 inch.
fn export_width_px(width_pt: f32, dpi: u32) -> u32 {
    let px = width_pt * dpi as f32 / 72.0;
    if !px.is_finite() {
        return 1;
    }
    (px.round() as i64).clamp(1, 20_000) as u32
}

/// Render one page (`page` 1-based) to image bytes at `dpi`. Blocking.
fn render_page(
    pdf: &[u8],
    pages: &[bokai::import::pdf::PdfPage],
    page: usize,
    dpi: u32,
    format: PageFormat,
) -> Result<Vec<u8>, String> {
    let geom = pages
        .get(page.wrapping_sub(1))
        .ok_or_else(|| format!("this PDF has no page {page}"))?;
    let width_px = export_width_px(geom.width, dpi);
    let index = page - 1;
    match format {
        PageFormat::Jpeg => bokai::formats::pdf::render::render_pdf_page_jpeg(
            pdf,
            index,
            width_px,
            PAGE_EXPORT_JPEG_QUALITY,
        ),
        PageFormat::Png => bokai::formats::pdf::render::render_pdf_page_png(pdf, index, width_px),
    }
    .map_err(|e| format!("render page {page}: {e}"))
}

/// Export one page through a save dialog defaulting to `page-042.jpg`; returns
/// the saved path, or `None` if cancelled.
#[tauri::command]
pub async fn editor_export_pdf_page(
    app: AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
    page: usize,
    dpi: u32,
    format: PageFormat,
) -> Result<Option<String>, String> {
    let (kind, path) = editor_source(&state, book_id).await?;
    if kind != SourceKind::Pdf {
        return Err("this book's source isn't a PDF".to_string());
    }
    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let pdf = std::fs::read(&path).map_err(|e| format!("read {path}: {e}"))?;
        let pages = bokai::import::pdf::probe_pdf(pdf.clone())
            .map_err(|e| e.to_string())?
            .pages;
        render_page(&pdf, &pages, page, dpi, format)
    })
    .await
    .map_err(|e| e.to_string())??;

    let (tx, rx) = oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(format!("page-{page:03}.{}", format.ext()))
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

/// Export `pages` (1-based) into `dir`, each named `page-042.jpg`, zero-padded
/// to the book's page count.
#[tauri::command]
pub async fn editor_export_pdf_pages(
    state: State<'_, AppState>,
    book_id: i64,
    pages: Vec<usize>,
    dir: String,
    dpi: u32,
    format: PageFormat,
) -> Result<ExportImagesResult, String> {
    let (kind, path) = editor_source(&state, book_id).await?;
    if kind != SourceKind::Pdf {
        return Err("this book's source isn't a PDF".to_string());
    }
    if pages.is_empty() {
        return Err("no pages selected".to_string());
    }
    let dest_dir = PathBuf::from(&dir);
    let count = tokio::task::spawn_blocking(move || -> Result<usize, String> {
        let pdf = std::fs::read(&path).map_err(|e| format!("read {path}: {e}"))?;
        let geom = bokai::import::pdf::probe_pdf(pdf.clone())
            .map_err(|e| e.to_string())?
            .pages;
        std::fs::create_dir_all(&dest_dir)
            .map_err(|e| format!("create {}: {e}", dest_dir.display()))?;

        let width = geom.len().to_string().len(); // "570" -> page-001.jpg
        for &page in &pages {
            let bytes = render_page(&pdf, &geom, page, dpi, format)?;
            let out = dest_dir.join(format!("page-{page:0width$}.{}", format.ext()));
            std::fs::write(&out, &bytes).map_err(|e| format!("write {}: {e}", out.display()))?;
        }
        Ok(pages.len())
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(ExportImagesResult { dir, count })
}

/// Reduce an image's name to a filesystem-safe stem: keep ASCII alphanumerics
/// plus `.`/`-`/`_`, collapse the rest to `_` (`resource/rsrc7` →
/// `resource_rsrc7`). Falls back to `image` when nothing printable survives.
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

/// Validate-only TOC verdict for the top-bar chip, without a proposal.
fn compute_toc(source_path: &str, kind: SourceKind) -> Option<EditorToc> {
    let bytes = std::fs::read(source_path).ok()?;
    if kind == SourceKind::Pdf {
        return Some(pdf_toc_summary(&bytes)?.0);
    }
    let audit = toc_validate::validate(&bytes).ok()?;
    Some(EditorToc {
        verdict: audit.verdict.as_str().to_string(),
        nav_count: audit.nav_count,
        nav_chapters: audit.nav_chapters,
        contents_links: audit.contents_links,
        headings: audit.headings,
        section_heads: audit.section_heads,
        flattened_volumes: audit.flattened.volumes,
        flattened_entries: audit.flattened.misplaced,
    })
}

/// PDF TOC summary: the verdict chip plus the parsed outline and page count.
fn pdf_toc_summary(bytes: &[u8]) -> Option<(EditorToc, Vec<PdfOutlineItem>, usize)> {
    let doc = bokai::import::probe_pdf(bytes.to_vec()).ok()?;
    let count = count_outline(&doc.outline);
    Some((
        EditorToc {
            verdict: toc_verdict(count).to_string(),
            nav_count: count,
            nav_chapters: count,
            contents_links: 0,
            headings: 0,
            section_heads: 0,
            flattened_volumes: 0,
            flattened_entries: 0,
        },
        doc.outline,
        doc.pages.len(),
    ))
}

/// A PDF's TOC verdict is presence-based: an outline exists or it does not.
fn toc_verdict(count: usize) -> &'static str {
    if count == 0 { "SPARSE" } else { "OK" }
}

/// Total entries in an outline tree, all levels.
fn count_outline(items: &[PdfOutlineItem]) -> usize {
    items.iter().map(|i| 1 + count_outline(&i.children)).sum()
}

/// Full TOC panel state — verdict, current labels and the nested proposal — with
/// the format sniffed from the bytes. Blocking.
fn read_toc_detail(source_path: &str, kind: SourceKind) -> Result<EditorTocDetail, String> {
    let bytes = std::fs::read(source_path).map_err(|e| format!("read {source_path}: {e}"))?;

    // PDF: no proposer; the panel starts from the existing outline, or blank.
    if kind == SourceKind::Pdf {
        let (summary, outline, page_count) =
            pdf_toc_summary(&bytes).ok_or_else(|| "couldn't read the PDF".to_string())?;
        let note = outline.is_empty().then(|| {
            "This PDF has no table of contents. Add entries below — each one \
             needs a title and the page it jumps to."
                .to_string()
        });
        return Ok(EditorTocDetail {
            verdict: summary.verdict,
            nav_count: summary.nav_count,
            nav_chapters: summary.nav_chapters,
            flattened_volumes: 0,
            flattened_entries: 0,
            current: outline.iter().map(TocEntryDto::declared_pdf).collect(),
            proposed: outline.iter().map(TocEntryDto::from_pdf).collect(),
            note,
            page_count: Some(page_count),
            can_auto_repair: false,
        });
    }

    let audit = toc_validate::validate(&bytes)?;
    let no_contents = "This book declares no table of contents, and no chapter \
                       list could be found in its Contents page or headings.";
    let (proposed, note): (Vec<TocEntryDto>, Option<String>) = if bytes.starts_with(b"PK") {
        match epub_toc::propose_toc(&bytes) {
            Ok(entries) if !entries.is_empty() => {
                (entries.iter().map(TocEntryDto::from_epub).collect(), None)
            }
            Ok(_) => (Vec::new(), Some(no_contents.to_string())),
            Err(e) => (
                Vec::new(),
                Some(format!("Couldn't auto-derive chapters: {e}")),
            ),
        }
    } else {
        match toc_repair::propose_toc(&bytes) {
            Ok(entries) if !entries.is_empty() => {
                (entries.iter().map(TocEntryDto::from_kfx).collect(), None)
            }
            Ok(_) => (Vec::new(), Some(no_contents.to_string())),
            Err(e) => (
                Vec::new(),
                Some(format!("Couldn't auto-derive chapters: {e}")),
            ),
        }
    };
    Ok(EditorTocDetail {
        verdict: audit.verdict.as_str().to_string(),
        nav_count: audit.nav_count,
        nav_chapters: audit.nav_chapters,
        flattened_volumes: audit.flattened.volumes,
        flattened_entries: audit.flattened.misplaced,
        current: audit.nav_tree.iter().map(TocEntryDto::label_only).collect(),
        proposed,
        note,
        page_count: None,
        can_auto_repair: true,
    })
}

#[derive(Serialize)]
pub struct EditorTextOpen {
    pub opf_path: String,
    pub members: Vec<text_editor::MemberInfo>,
}

#[tauri::command]
pub async fn editor_text_open(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<EditorTextOpen, String> {
    let row = editor_row(&state, book_id).await?;
    tokio::task::spawn_blocking(move || -> Result<EditorTextOpen, String> {
        let session = text_editor::EpubSession::open(&row).map_err(|e| format!("{e:#}"))?;
        Ok(EditorTextOpen {
            opf_path: session.opf_path().map_err(|e| format!("{e:#}"))?,
            members: session.members().map_err(|e| format!("{e:#}"))?,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn editor_text_read(
    state: State<'_, AppState>,
    book_id: i64,
    member: String,
) -> Result<String, String> {
    let (kind, path) = editor_source(&state, book_id).await?;
    if kind != SourceKind::Epub {
        return Err("text editing writes to an EPUB source".into());
    }
    tokio::task::spawn_blocking(move || {
        text_editor::member_text(&path, &member).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn editor_text_read_bytes(
    state: State<'_, AppState>,
    book_id: i64,
    member: String,
) -> Result<String, String> {
    let (kind, path) = editor_source(&state, book_id).await?;
    if kind != SourceKind::Epub {
        return Err("text editing writes to an EPUB source".into());
    }
    tokio::task::spawn_blocking(move || {
        text_editor::member_bytes(&path, &member)
            .map(|b| B64.encode(b))
            .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Deserialize)]
pub struct TextEdit {
    pub member: String,
    pub text: String,
    pub media_type: Option<String>,
}

#[derive(Serialize)]
pub struct EditorTextSaved {
    pub written: Vec<String>,
    pub members: Vec<text_editor::MemberInfo>,
    pub toc: Option<EditorToc>,
    pub findings: Vec<text_editor::FindingInfo>,
}

fn apply_edits(
    session: &mut text_editor::EpubSession,
    edits: Vec<TextEdit>,
    removed: &[String],
) -> Result<(), String> {
    let existing: HashSet<String> = session
        .members()
        .map_err(|e| format!("{e:#}"))?
        .into_iter()
        .map(|m| m.path)
        .collect();
    for e in edits {
        if existing.contains(&e.member) {
            session
                .write_text(&e.member, &e.text)
                .map_err(|e| format!("{e:#}"))?;
        } else {
            let mt = e
                .media_type
                .or_else(|| text_editor::media_type_for(&e.member).map(str::to_string))
                .ok_or_else(|| format!("{}: unknown media type for a new member", e.member))?;
            session
                .add(&e.member, &mt, e.text.into_bytes())
                .map_err(|e| format!("{e:#}"))?;
        }
    }
    for path in removed {
        if existing.contains(path) {
            session.remove(path).map_err(|e| format!("{e:#}"))?;
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn editor_text_op(
    state: State<'_, AppState>,
    book_id: i64,
    edits: Vec<TextEdit>,
    removed: Option<Vec<String>>,
    op: text_editor::Operation,
) -> Result<text_editor::Outcome, String> {
    let row = editor_row(&state, book_id).await?;
    tokio::task::spawn_blocking(move || -> Result<text_editor::Outcome, String> {
        let mut session = text_editor::EpubSession::open(&row).map_err(|e| format!("{e:#}"))?;
        apply_edits(&mut session, edits, &removed.unwrap_or_default())?;
        session.apply(&op).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn editor_text_save(
    state: State<'_, AppState>,
    book_id: i64,
    edits: Vec<TextEdit>,
    removed: Option<Vec<String>>,
) -> Result<EditorTextSaved, String> {
    let row = editor_row(&state, book_id).await?;
    let db = state.db.clone();
    let saved = tokio::task::spawn_blocking(move || -> Result<EditorTextSaved, String> {
        let mut session = text_editor::EpubSession::open(&row).map_err(|e| format!("{e:#}"))?;
        apply_edits(&mut session, edits, &removed.unwrap_or_default())?;
        let written = {
            let conn = db.blocking_lock();
            session.save(&conn).map_err(|e| format!("{e:#}"))?
        };
        let path = session.path().to_string();
        Ok(EditorTextSaved {
            written,
            members: session.members().map_err(|e| format!("{e:#}"))?,
            toc: compute_toc(&path, SourceKind::Epub),
            findings: session.validate().map_err(|e| format!("{e:#}"))?,
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    if !saved.written.is_empty() {
        let _ = state.queue.enqueue_reconvert(book_id).await;
        crate::commands::reader::evict_reader(&state, book_id).await;
    }
    Ok(saved)
}

#[tauri::command]
pub async fn editor_text_validate(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<Vec<text_editor::FindingInfo>, String> {
    let (kind, path) = editor_source(&state, book_id).await?;
    if kind != SourceKind::Epub {
        return Err("text editing writes to an EPUB source".into());
    }
    tokio::task::spawn_blocking(move || {
        text_editor::validate_file(&path).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn editor_row(state: &State<'_, AppState>, book_id: i64) -> Result<BookRow, String> {
    let conn = state.db.lock().await;
    db::get_book(&conn, book_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no book with id {book_id}"))
}

async fn commit_edited_source(
    state: &AppState,
    book_id: i64,
    kind: SourceKind,
    path: &str,
    new_bytes: Vec<u8>,
) -> Result<(), String> {
    let db = state.db.clone();
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        source::commit(&conn, book_id, kind, &path, &new_bytes).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest PDF `read_toc_detail` accepts: one page, an `/Info` dictionary.
    const MINIMAL_INPUT: &[u8] = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\n4 0 obj\n<< /Title (Tiny Test PDF) /Author (A. Tester) >>\nendobj\nxref\n0 5\n0000000000 65535 f \n0000000015 00000 n \n0000000064 00000 n \n0000000121 00000 n \n0000000192 00000 n \ntrailer\n<< /Size 5 /Root 1 0 R /Info 4 0 R >>\nstartxref\n256\n%%EOF\n";

    #[test]
    fn source_format_from_kind() {
        assert_eq!(source_format(Some("kfx_to_epub")), "kfx");
        assert_eq!(source_format(Some("epub_to_kfx")), "epub");
        assert_eq!(source_format(Some("pdf_to_kfx")), "pdf");
        // A missing kind reads as the EPUB-source default.
        assert_eq!(source_format(None), "epub");
    }

    /// A PDF point is 1/72 inch: a 300-dpi render is the page's point width times 300/72.
    #[test]
    fn export_width_follows_dpi_from_the_page_size() {
        // US Letter, 612pt wide: the textbook cases.
        assert_eq!(export_width_px(612.0, 72), 612, "72 dpi is 1px per point");
        assert_eq!(export_width_px(612.0, 300), 2550);
        assert_eq!(export_width_px(612.0, 600), 5100);
        // A bunko page at 300 dpi — the real scanned-novel case.
        assert_eq!(export_width_px(342.7, 300), 1428);
    }

    /// The rasterizer refuses a bitmap wider than a `u16`: an absurd page or DPI
    /// is clamped, and a degenerate page renders a non-empty bitmap.
    #[test]
    fn export_width_is_clamped_at_both_ends() {
        assert_eq!(
            export_width_px(0.0, 300),
            1,
            "a zero-width page still renders"
        );
        assert_eq!(
            export_width_px(612.0, 0),
            1,
            "0 dpi floors rather than empties"
        );
        assert_eq!(export_width_px(f32::NAN, 300), 1);
        assert_eq!(
            export_width_px(1_000_000.0, 600),
            20_000,
            "clamped, not overflowed"
        );
        // A0 at 600 dpi stays under the cap — the bound is generous, not tight.
        assert!(export_width_px(2384.0, 600) < 20_000);
    }

    #[test]
    fn panels_cover_every_built_capability() {
        for kind in [SourceKind::Kfx, SourceKind::Epub, SourceKind::Pdf] {
            let p = editor_panels(kind);
            for want in ["metadata", "toc", "cover", "images"] {
                assert!(p.contains(&want.to_string()), "{want} panel is backed");
            }
            assert_eq!(p.contains(&"text".to_string()), kind == SourceKind::Epub);
        }
    }

    /// Reordering a reading order is a permutation only in EPUB; KFX and PDF
    /// name the reason.
    #[test]
    fn only_epub_backs_the_reading_order_panel() {
        assert!(editor_panels(SourceKind::Epub).contains(&"spine".to_string()));
        for kind in [SourceKind::Kfx, SourceKind::Pdf] {
            assert!(
                !editor_panels(kind).contains(&"spine".to_string()),
                "{kind:?} has no spine permutation behind the panel"
            );
            assert!(
                !spine_unsupported(kind).is_empty(),
                "{kind:?} must explain why, not just refuse"
            );
        }
    }

    /// The PDF DTO boundary: the panel speaks 1-based page numbers, the primitive
    /// speaks 0-based indices, and the tree survives a round-trip.
    #[test]
    fn pdf_toc_dto_roundtrips_page_numbers_and_nesting() {
        let outline = [PdfOutlineItem {
            title: "Part I".into(),
            page_index: 0,
            children: vec![PdfOutlineItem {
                title: "Chapter 1".into(),
                page_index: 41,
                children: vec![],
            }],
        }];
        let dto: Vec<TocEntryDto> = outline.iter().map(TocEntryDto::from_pdf).collect();
        assert_eq!(dto[0].page, 1, "0-based index 0 shows as page 1");
        assert_eq!(dto[0].children[0].page, 42);

        let back: Vec<PdfOutlineItem> = dto.into_iter().map(TocEntryDto::into_pdf).collect();
        assert_eq!(back[0].page_index, 0);
        assert_eq!(back[0].children[0].page_index, 41);
        assert_eq!(back[0].children[0].title, "Chapter 1");
    }

    /// A page of 0 (a panel row never touched) clamps to page 1.
    #[test]
    fn pdf_dto_page_zero_clamps_to_first_page() {
        let dto = TocEntryDto {
            label: "  Spacey  ".into(),
            eid: 0,
            href: String::new(),
            page: 0,
            children: vec![],
        };
        let item = dto.into_pdf();
        assert_eq!(item.page_index, 0);
        assert_eq!(item.title, "Spacey", "labels are trimmed at the boundary");
    }

    /// PDF's TOC verdict is presence-based: an outline exists or it does not.
    #[test]
    fn pdf_toc_verdict_reflects_presence() {
        assert_eq!(toc_verdict(0), "SPARSE");
        assert_eq!(toc_verdict(1), "OK");
        assert_eq!(toc_verdict(99), "OK", "no ceiling above OK for a PDF");
    }

    /// The panel's "Currently declared" side keeps the outline's nesting and
    /// names the page each entry jumps to.
    #[test]
    fn the_declared_pdf_outline_keeps_its_nesting_and_pages() {
        let outline = [PdfOutlineItem {
            title: "Part I".into(),
            page_index: 0,
            children: vec![PdfOutlineItem {
                title: "Chapter 1".into(),
                page_index: 4,
                children: vec![],
            }],
        }];
        let declared: Vec<TocEntryDto> = outline.iter().map(TocEntryDto::declared_pdf).collect();

        assert_eq!(declared.len(), 1, "one top-level entry, not a flat run");
        assert_eq!(declared[0].label, "Part I — p.1");
        assert_eq!(declared[0].children[0].label, "Chapter 1 — p.5");
        assert_eq!(count_outline(&outline), 2);
    }

    /// `read_toc_detail` puts a PDF panel into hand-authoring mode: page count
    /// present, auto-repair off, and a note when the book has no TOC.
    #[test]
    fn pdf_toc_detail_is_hand_authoring_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.pdf");
        std::fs::write(&path, MINIMAL_INPUT).expect("write");

        let detail = read_toc_detail(path.to_str().unwrap(), SourceKind::Pdf).expect("detail");
        assert_eq!(detail.page_count, Some(1));
        assert!(!detail.can_auto_repair, "PDF has no proposer");
        assert!(detail.proposed.is_empty());
        assert!(detail.note.is_some(), "explains that entries must be added");
        assert_eq!(detail.verdict, "SPARSE");
    }

    #[test]
    fn toc_dto_round_trips_nesting_faithfully() {
        // A nested source (Part → chapters) must survive DTO → wire → TocEntry
        // with its shape intact: no flatten, no re-nest.
        let src = [
            KfxTocEntry {
                label: "Part I".into(),
                eid: 1,
                children: vec![KfxTocEntry::new("Ch 1", 2), KfxTocEntry::new("Ch 2", 3)],
            },
            KfxTocEntry::new("Afterword", 4),
        ];
        let dto: Vec<TocEntryDto> = src.iter().map(TocEntryDto::from_kfx).collect();
        let back: Vec<KfxTocEntry> = dto.into_iter().map(TocEntryDto::into_kfx).collect();

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
        let flat = [KfxTocEntry::new("One", 10), KfxTocEntry::new("Two", 20)];
        let back: Vec<KfxTocEntry> = flat
            .iter()
            .map(TocEntryDto::from_kfx)
            .map(TocEntryDto::into_kfx)
            .collect();
        assert!(back.iter().all(|e| e.children.is_empty()));
    }

    /// The EPUB path of the DTO round-trips title↔label and href faithfully.
    #[test]
    fn toc_dto_round_trips_epub_href() {
        let src = [EpubTocEntry {
            title: "Part I".into(),
            href: "OEBPS/p1.xhtml".into(),
            children: vec![EpubTocEntry::new("Ch 1", "OEBPS/c1.xhtml#h1")],
            play_order: None,
            target: None,
        }];
        let dto: Vec<TocEntryDto> = src.iter().map(TocEntryDto::from_epub).collect();
        assert_eq!(dto[0].href, "OEBPS/p1.xhtml");
        assert_eq!(dto[0].eid, 0, "EPUB entries carry no eid");
        let back: Vec<EpubTocEntry> = dto.into_iter().map(TocEntryDto::into_epub).collect();
        assert_eq!(back[0].title, "Part I");
        assert_eq!(back[0].children[0].href, "OEBPS/c1.xhtml#h1");
    }

    #[test]
    fn any_blank_label_finds_nested_blanks() {
        let ok = [KfxTocEntry::new("A", 1), KfxTocEntry::new("B", 2)];
        let ok_dto: Vec<TocEntryDto> = ok.iter().map(TocEntryDto::from_kfx).collect();
        assert!(!any_blank_label(&ok_dto));

        // A blank label buried under a Part is caught.
        let bad = [KfxTocEntry {
            label: "Part I".into(),
            eid: 1,
            children: vec![KfxTocEntry::new("  ", 2)],
        }];
        let bad_dto: Vec<TocEntryDto> = bad.iter().map(TocEntryDto::from_kfx).collect();
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

#[derive(Serialize)]
pub struct EditorStylesRestored {
    pub report: sidle_core::library::editor::RestoreReport,
    pub members: Vec<text_editor::MemberInfo>,
    pub toc: Option<EditorToc>,
    pub findings: Vec<text_editor::FindingInfo>,
}

#[tauri::command]
pub async fn editor_style_candidates(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<Vec<sidle_core::library::editor::Candidate>, String> {
    let row = editor_row(&state, book_id).await?;
    let db = state.db.clone();
    tokio::task::spawn_blocking(move || {
        let conn = db.blocking_lock();
        sidle_core::library::editor::candidates(&conn, &row).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn editor_restore_styles(
    state: State<'_, AppState>,
    book_id: i64,
    reference_id: i64,
    force: bool,
) -> Result<EditorStylesRestored, String> {
    let row = editor_row(&state, book_id).await?;
    let reference = editor_row(&state, reference_id).await?;
    let db = state.db.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<EditorStylesRestored, String> {
        let report = {
            let conn = db.blocking_lock();
            sidle_core::library::editor::restore(&conn, &row, &reference, true, force, None)
                .map_err(|e| format!("{e:#}"))?
        };
        let session = text_editor::EpubSession::open(&row).map_err(|e| format!("{e:#}"))?;
        let path = session.path().to_string();
        Ok(EditorStylesRestored {
            report,
            members: session.members().map_err(|e| format!("{e:#}"))?,
            toc: compute_toc(&path, SourceKind::Epub),
            findings: session.validate().map_err(|e| format!("{e:#}"))?,
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    if out.report.written {
        let _ = state.queue.enqueue_reconvert(book_id).await;
        crate::commands::reader::evict_reader(&state, book_id).await;
    }
    Ok(out)
}
