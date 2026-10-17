//! Tauri command for the built-in reader: KFX → renderable DOM.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

use crate::library::db::{self, AnnotationRow};
use crate::library::ingest;
use crate::library::notes;
use crate::state::AppState;

/// One spine document in reading order. `html` carries `data-eid` attributes —
/// or is `null` for a section outside the resume window of a windowed open
/// (large text book), in which case [`reader_fetch_sections`] streams it in.
#[derive(Debug, Serialize)]
pub struct ReaderSectionDto {
    pub href: String,
    pub html: Option<String>,
    /// Serialized HTML byte length, set whether or not `html` shipped.
    pub size: i64,
    /// Fixed-layout page pixel size `[width, height]`, or `null` for reflowable.
    pub viewport: Option<[u32; 2]>,
    /// `"page-spread-left"` / `"page-spread-right"` on a paired fixed-layout page.
    pub spread: Option<String>,
    /// Base-text char count, ruby-free and whitespace-collapsed.
    pub chars: i64,
    /// A full-page-image section (cover, full-bleed art).
    pub image_only: bool,
    /// Image hrefs this section references, in document order.
    pub image_hrefs: Vec<String>,
}

/// A non-spine asset the chapters reference by relative href.
#[derive(Debug, Serialize)]
pub struct ReaderResourceDto {
    pub href: String,
    pub mime: String,
    /// base64 (STANDARD, padded) of the resource bytes.
    pub data_base64: String,
}

/// Manifest entry for an image the frontend fetches on demand via
/// [`reader_fetch_resources`]. `width`/`height` let it reserve layout space.
#[derive(Debug, Serialize)]
pub struct ReaderImageDto {
    pub href: String,
    pub mime: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Table-of-contents entry; `href` points into a section (`"c5.xhtml#anchor"`).
#[derive(Debug, Serialize)]
pub struct ReaderTocDto {
    pub label: String,
    pub href: String,
    pub children: Vec<ReaderTocDto>,
}

/// What `reader_open` returns: a reflowable HTML book or a fixed-layout
/// PDF-backed one, tagged by `mode`.
#[derive(Debug, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ReaderOpen {
    Reflowable(ReaderBookDto),
    Pdf(ReaderPdfDto),
}

/// A PDF-backed book's structure for the fixed-layout view; [`reader_pdf_page`]
/// renders its pages.
#[derive(Debug, Serialize)]
pub struct ReaderPdfDto {
    pub page_count: usize,
    pub title: String,
    pub authors: Vec<String>,
    /// PDF outline → TOC, each entry pointing at a 0-based page index.
    pub toc: Vec<ReaderPdfTocDto>,
    /// Per-page point size (`width`/`height`) for the viewer's aspect ratio.
    pub pages: Vec<ReaderPdfPageDto>,
    /// Per-page display label (`/PageLabels`: "Cover", "i", "1", …).
    pub page_labels: Vec<String>,
    /// `"rtl"` / `"ltr"` page-turn direction, from the row's `ppd`.
    pub page_progression_direction: String,
}

/// One PDF page's display size in points plus its selectable text layer: the runs
/// to overlay as `data-eid` spans, and every eid anchored on the page.
#[derive(Debug, Serialize)]
pub struct ReaderPdfPageDto {
    pub width: f32,
    pub height: f32,
    pub words: Vec<ReaderPdfWordDto>,
    pub eids: Vec<i64>,
}

/// One text run over the page image; `left`/`top`/`width`/`height` are
/// fractions of the page box, top-left origin.
#[derive(Debug, Serialize)]
pub struct ReaderPdfWordDto {
    pub eid: i64,
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub text: String,
}

/// A PDF TOC entry, targeting a 0-based page index (not an eid/href).
#[derive(Debug, Serialize)]
pub struct ReaderPdfTocDto {
    pub label: String,
    pub page_index: usize,
    pub children: Vec<ReaderPdfTocDto>,
}

fn map_pdf_toc(items: &[bokai::formats::pdf::PdfOutlineItem]) -> Vec<ReaderPdfTocDto> {
    items
        .iter()
        .map(|it| ReaderPdfTocDto {
            label: it.title.clone(),
            page_index: it.page_index,
            children: map_pdf_toc(&it.children),
        })
        .collect()
}

/// The reader's view of a book, ready for the webview paginator.
#[derive(Debug, Serialize)]
pub struct ReaderBookDto {
    pub sections: Vec<ReaderSectionDto>,
    /// Eagerly-shipped assets (`style.css`); images are in `images`.
    pub resources: Vec<ReaderResourceDto>,
    /// Deferred-image manifest; bytes come from [`reader_fetch_resources`].
    pub images: Vec<ReaderImageDto>,
    pub toc: Vec<ReaderTocDto>,
    pub title: String,
    pub authors: Vec<String>,
    pub language: String,
    /// `"vertical-rl"` / `"horizontal-tb"` — drives the reader's writing mode.
    pub writing_mode: String,
    /// `"rtl"` / `"ltr"` — spine progression / page-turn direction.
    pub page_progression_direction: String,
    /// `[eid, location]` pairs, `location` being the device's Loc number.
    pub locations: Vec<(i64, i64)>,
    /// Location count — the denominator for whole-book % and "Loc N of M".
    pub max_location: i64,
    /// Image-based fixed-layout book (manga, comic) with pre-paginated pages.
    pub fixed_layout: bool,
}

fn map_toc(points: Vec<sidle_core::reader::ReaderTocEntry>) -> Vec<ReaderTocDto> {
    points
        .into_iter()
        .map(|p| ReaderTocDto {
            label: p.label,
            href: p.href,
            children: map_toc(p.children),
        })
        .collect()
}

/// Section-HTML bytes above which a reflowable open is windowed.
const SECTION_WINDOW_THRESHOLD: usize = 2 * 1024 * 1024;

/// The open DTO plus the store-side pieces a lazy open leaves behind.
struct BuiltReaderOpen {
    dto: ReaderBookDto,
    /// All sections `(href, html)` for `reader_fetch_sections`.
    sections: Vec<(String, String)>,
    eid_to_section: std::collections::HashMap<i64, usize>,
    /// True when the DTO withheld section HTML.
    withheld: bool,
}

/// Build the reader-open DTO, windowing large reflowable books around
/// `resume_eid`'s section: back 1 section, forward 2.
fn build_reader_open(
    b: sidle_core::reader::ReaderBook,
    resume_eid: Option<i64>,
) -> BuiltReaderOpen {
    let total_html: usize = b.sections.iter().map(|s| s.html.len()).sum();
    let windowed = !b.fixed_layout && total_html > SECTION_WINDOW_THRESHOLD;
    let n = b.sections.len();
    let (lo, hi) = if windowed {
        let idx = resume_eid
            .and_then(|e| b.sections.iter().position(|s| s.elements.contains(&e)))
            .unwrap_or(0);
        (idx.saturating_sub(1), (idx + 2).min(n.saturating_sub(1)))
    } else {
        (0, n.saturating_sub(1))
    };

    let eid_to_section = b.section_of_element();

    let sections_dto = b
        .sections
        .iter()
        .enumerate()
        .map(|(i, s)| ReaderSectionDto {
            href: s.href.clone(),
            html: if !windowed || (lo..=hi).contains(&i) {
                Some(s.html.clone())
            } else {
                None
            },
            size: s.html.len() as i64,
            viewport: s.viewport.map(|(w, h)| [w, h]),
            spread: s.spread.clone(),
            chars: s.chars as i64,
            image_only: s.image_only,
            image_hrefs: s.image_hrefs.clone(),
        })
        .collect();

    let dto = ReaderBookDto {
        sections: sections_dto,
        resources: b
            .resources
            .into_iter()
            .map(|r| ReaderResourceDto {
                href: r.href,
                mime: r.mime,
                data_base64: B64.encode(&r.data),
            })
            .collect(),
        images: b
            .images
            .into_iter()
            .map(|i| ReaderImageDto {
                href: i.href,
                mime: i.mime,
                width: i.width,
                height: i.height,
            })
            .collect(),
        toc: map_toc(b.toc),
        title: b.title,
        authors: b.authors,
        language: b.language,
        writing_mode: b.writing_mode,
        page_progression_direction: b.page_progression_direction,
        locations: b.locations,
        max_location: b.max_location,
        fixed_layout: b.fixed_layout,
    };
    BuiltReaderOpen {
        dto,
        sections: b.sections.into_iter().map(|s| (s.href, s.html)).collect(),
        eid_to_section,
        withheld: windowed,
    }
}

/// Open a library book for the reader.
#[tauri::command]
pub async fn reader_open(state: State<'_, AppState>, book_id: i64) -> Result<ReaderOpen, String> {
    // Snapshot paths, display metadata and the saved Sidle position under the lock,
    // then release it before the CPU-bound parse/render.
    let (kfx_path, pdf_path, title, author, ppd, resume_eid) = {
        let conn = state.db.lock().await;
        let row = db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no book with id {book_id}"))?;
        let kfx_path = row
            .kfx_path
            .ok_or_else(|| "this book has no KFX file yet".to_string())?;
        let resume_eid = db::list_reading_positions(&conn, book_id)
            .ok()
            .and_then(|rows| rows.into_iter().find(|p| p.source == "sidle"))
            .and_then(|p| p.eid);
        (
            kfx_path,
            row.pdf_path,
            row.title,
            row.author,
            row.ppd,
            resume_eid,
        )
    };

    let (open, store) = tokio::task::spawn_blocking(move || {
        let kfx = std::fs::read(&kfx_path).map_err(|e| format!("read {kfx_path}: {e}"))?;

        // A PDF-backed `kfx` serves the fixed-layout view.
        if pdf_path.is_some() || bokai::formats::kfx::pdf_container::kfx_is_pdf_backed(&kfx) {
            let rd = bokai::formats::kfx::pdf_pages::read_pages(&kfx)
                .map_err(|e| format!("read PDF KFX for reader: {e}"))?;
            let authors = crate::library::authors::split_display(&author);
            let dto = ReaderPdfDto {
                page_count: rd.pages.len(),
                title,
                authors,
                toc: map_pdf_toc(&rd.outline),
                pages: rd
                    .pages
                    .iter()
                    .map(|p| ReaderPdfPageDto {
                        width: p.box_w,
                        height: p.box_h,
                        words: p
                            .runs
                            .iter()
                            .map(|r| ReaderPdfWordDto {
                                eid: r.eid,
                                left: r.left,
                                top: r.top,
                                width: r.width,
                                height: r.height,
                                text: r.text.clone(),
                            })
                            .collect(),
                        eids: p.eids.clone(),
                    })
                    .collect(),
                page_labels: rd.page_labels,
                page_progression_direction: match ppd.as_deref() {
                    Some("rtl") => "rtl".to_string(),
                    _ => "ltr".to_string(),
                },
            };
            return Ok((ReaderOpen::Pdf(dto), None));
        }

        // Reflowable / fixed-layout KFX → DOM: image bytes deferred, large
        // text books windowed.
        let (book, images) = sidle_core::reader::ReaderBook::open(&kfx)
            .map_err(|e| format!("KFX→DOM render failed: {e}"))?;
        let built = build_reader_open(book, resume_eid);
        // Cache the store only when something is left to serve: images the
        // manifest promises, or sections the window withheld.
        let has_images = !built.dto.images.is_empty();
        let entry = (has_images || built.withheld).then_some(crate::state::ReaderStoreEntry {
            images,
            sections: built.sections,
            eid_to_section: built.eid_to_section,
        });
        Ok::<(ReaderOpen, Option<crate::state::ReaderStoreEntry>), String>((
            ReaderOpen::Reflowable(built.dto),
            entry,
        ))
    })
    .await
    .map_err(|e| format!("reader task join error: {e}"))??;

    // `store` replaces the previous book's fetch store; `None` clears it.
    {
        let mut cache = state.reader_store.lock().await;
        *cache = store.map(|s| (book_id, std::sync::Arc::new(s)));
    }
    Ok(open)
}

/// Fetch a batch of deferred images for the open book: each manifest href →
/// mime and base64 bytes, a failed href omitted.
#[tauri::command]
pub async fn reader_fetch_resources(
    state: State<'_, AppState>,
    book_id: i64,
    hrefs: Vec<String>,
) -> Result<Vec<ReaderResourceDto>, String> {
    let entry = reader_store_entry(&state, book_id).await?;
    tokio::task::spawn_blocking(move || {
        entry
            .images
            .fetch_many(&hrefs)
            .into_iter()
            .map(|img| ReaderResourceDto {
                href: img.href,
                mime: img.mime,
                data_base64: B64.encode(&img.bytes),
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| format!("reader fetch task join error: {e}"))
}

/// One streamed section of a windowed open.
#[derive(Debug, Serialize)]
pub struct ReaderSectionChunkDto {
    pub index: i64,
    pub html: String,
}

/// Stream built section HTML for a windowed open.
#[tauri::command]
pub async fn reader_fetch_sections(
    state: State<'_, AppState>,
    book_id: i64,
    indices: Vec<i64>,
) -> Result<Vec<ReaderSectionChunkDto>, String> {
    let entry = reader_store_entry(&state, book_id).await?;
    Ok(indices
        .into_iter()
        .filter_map(|i| {
            let section = entry.sections.get(usize::try_from(i).ok()?)?;
            Some(ReaderSectionChunkDto {
                index: i,
                html: section.1.clone(),
            })
        })
        .collect())
}

/// Resolve an eid to its section index — for jumps (annotation / resume /
/// search) into a section the webview hasn't streamed yet. `null` when the
/// eid isn't in the book (e.g. after a re-convert).
#[tauri::command]
pub async fn reader_eid_section(
    state: State<'_, AppState>,
    book_id: i64,
    eid: i64,
) -> Result<Option<i64>, String> {
    let entry = reader_store_entry(&state, book_id).await?;
    Ok(entry.eid_to_section.get(&eid).map(|&i| i as i64))
}

/// Drop the open book's fetch store.
#[tauri::command]
pub async fn reader_release(state: State<'_, AppState>, book_id: i64) -> Result<(), String> {
    evict_reader(&state, book_id).await;
    Ok(())
}

/// Drop the cached fetch store and search index held for `book_id`.
pub(crate) async fn evict_reader(state: &AppState, book_id: i64) {
    {
        let mut store = state.reader_store.lock().await;
        if matches!(&*store, Some((id, _)) if *id == book_id) {
            *store = None;
        }
    }
    {
        let mut search = state.reader_search_cache.lock().await;
        if matches!(&*search, Some((id, _)) if *id == book_id) {
            *search = None;
        }
    }
}

/// The cached fetch store for `book_id`, or the "reopen the book" error every
/// deferred-fetch command shares.
async fn reader_store_entry(
    state: &State<'_, AppState>,
    book_id: i64,
) -> Result<std::sync::Arc<crate::state::ReaderStoreEntry>, String> {
    let cache = state.reader_store.lock().await;
    match &*cache {
        Some((id, entry)) if *id == book_id => Ok(entry.clone()),
        _ => Err(format!(
            "no reader store for book {book_id} (was the reader closed?)"
        )),
    }
}

/// Render one page of a PDF-backed book to a JPEG, scaled to `width` device
/// pixels wide, returned base64 (data-URL payload) for the fixed-layout viewer.
#[tauri::command]
pub async fn reader_pdf_page(
    state: State<'_, AppState>,
    book_id: i64,
    page: usize,
    width: u32,
) -> Result<String, String> {
    let (kfx_path, pdf_path) = {
        let conn = state.db.lock().await;
        let row = db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no book with id {book_id}"))?;
        (row.kfx_path, row.pdf_path)
    };
    let width = width.clamp(200, 4000);

    tokio::task::spawn_blocking(move || {
        let bytes: Vec<u8> = match pdf_path.as_deref().map(std::fs::read) {
            Some(Ok(b)) => b,
            _ => {
                let kfx_path = kfx_path.ok_or("book has no KFX or PDF to render")?;
                let kfx = std::fs::read(&kfx_path).map_err(|e| format!("read {kfx_path}: {e}"))?;
                bokai::formats::kfx::pdf_container::kfx_extract_pdf(&kfx)
                    .map_err(|e| format!("extract embedded PDF: {e:?}"))?
            }
        };
        let jpeg = bokai::formats::pdf::render::render_pdf_page_jpeg(
            &bytes,
            page,
            width,
            bokai::formats::pdf::render::COVER_JPEG_QUALITY,
        )
        .map_err(|e| format!("render page {page}: {e}"))?;
        Ok::<String, String>(B64.encode(&jpeg))
    })
    .await
    .map_err(|e| format!("reader render task join error: {e}"))?
}

/// One handwritten-ink overlay anchored on a host page: the transparent SVG to
/// composite over the rendered PDF page, tagged with the source ink page's
/// container id (stable per-page key, for the toggle/legend).
#[derive(Debug, Serialize)]
pub struct ReaderInkDto {
    pub container_id: String,
    pub host_page: i64,
    /// Transparent ink-only SVG, the cached overlay render; empty without its cache file.
    pub svg: String,
}

/// The handwritten-ink overlays anchored on one 0-based PDF page (usually one,
/// possibly more) — for the fixed-layout reader to composite over the rendered
/// page image behind a show/hide toggle. Empty when the page carries no ink.
#[tauri::command]
pub async fn reader_pdf_ink(
    state: State<'_, AppState>,
    book_id: i64,
    page: i64,
) -> Result<Vec<ReaderInkDto>, String> {
    let (sha, rows) = {
        let conn = state.db.lock().await;
        let sha = db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no book with id {book_id}"))?
            .sha256;
        let rows = db::list_book_ink_on_page(&conn, book_id, page).map_err(|e| e.to_string())?;
        (sha, rows)
    };
    let svg_for = |asin: &str, cid: &str| {
        std::fs::read_to_string(state.paths.book_ink_overlay_svg(&sha, asin, cid))
            .unwrap_or_default()
    };
    Ok(rows
        .into_iter()
        .map(|r| ReaderInkDto {
            host_page: r.host_page.unwrap_or(page),
            svg: svg_for(&r.asin, &r.container_id),
            container_id: r.container_id,
        })
        .collect())
}

/// The 0-based PDF pages carrying anchored ink for a book; [`reader_pdf_ink`]
/// serves each page's SVGs.
#[tauri::command]
pub async fn reader_pdf_ink_pages(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<Vec<i64>, String> {
    let conn = state.db.lock().await;
    db::book_ink_host_pages(&conn, book_id).map_err(|e| e.to_string())
}

/// One ink page for the annotations panel — enough to list, jump to, and delete.
#[derive(Debug, Serialize)]
pub struct InkPageDto {
    pub id: i64,
    pub host_page: Option<i64>,
    pub host_linear: Option<i64>,
    pub container_id: String,
    pub hidden: bool,
}

/// List a book's handwritten-ink pages for the annotations panel (one row per
/// drawn page), ordered by reading position.
#[tauri::command]
pub async fn book_ink_for_book(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<Vec<InkPageDto>, String> {
    let conn = state.db.lock().await;
    let rows = db::list_book_ink(&conn, book_id).map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|r| InkPageDto {
            id: r.id,
            host_page: r.host_page,
            host_linear: r.host_linear,
            container_id: r.container_id,
            hidden: r.hidden,
        })
        .collect())
}

/// Delete one handwritten-ink page — the row and its cached SVGs — and record
/// the deletion for device sync.
#[tauri::command]
pub async fn book_ink_delete(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    let conn = state.db.lock().await;
    crate::library::ink::delete_ink_page(&conn, &state.paths, id).map_err(|e| e.to_string())
}

/// Hide / unhide one annotation in the reader — kept in the backup, just not
/// painted or listed by default. Reversible; never touches the device.
#[tauri::command]
pub async fn annotation_set_hidden(
    state: State<'_, AppState>,
    id: i64,
    hidden: bool,
) -> Result<(), String> {
    let conn = state.db.lock().await;
    db::set_annotation_hidden(&conn, id, hidden).map_err(|e| e.to_string())
}

/// Hide / unhide one handwritten-ink page in the reader (kept in the backup).
#[tauri::command]
pub async fn book_ink_set_hidden(
    state: State<'_, AppState>,
    id: i64,
    hidden: bool,
) -> Result<(), String> {
    let conn = state.db.lock().await;
    db::set_book_ink_hidden(&conn, id, hidden).map_err(|e| e.to_string())
}

/// One stored annotation, shaped for the reader's painter + sidebar.
#[derive(Debug, Serialize)]
pub struct AnnotationDto {
    pub id: i64,
    /// `"highlight"` | `"note"` | `"bookmark"` | other.
    pub kind: String,
    pub eid_start: Option<i64>,
    pub off_start: Option<i64>,
    pub eid_end: Option<i64>,
    pub off_end: Option<i64>,
    pub loc_start: Option<i64>,
    pub loc_end: Option<i64>,
    /// Highlighted text (or bookmark/element preview).
    pub text: String,
    pub note_body: Option<String>,
    /// CSS color hint, if the source carried one.
    pub color: Option<String>,
    /// `"yjr"` | `"clippings"` — provenance.
    pub source: String,
    /// Reversible "hidden from the reader" flag (kept in the backup).
    pub hidden: bool,
    /// For a `note`, the id of the highlight it annotates, when one encloses it.
    pub attached_to: Option<i64>,
}

impl From<AnnotationRow> for AnnotationDto {
    fn from(a: AnnotationRow) -> Self {
        AnnotationDto {
            id: a.id,
            kind: a.kind,
            eid_start: a.eid_start,
            off_start: a.off_start,
            eid_end: a.eid_end,
            off_end: a.off_end,
            loc_start: a.loc_start,
            loc_end: a.loc_end,
            text: a.text,
            note_body: a.note_body,
            color: a.color,
            source: a.source,
            hidden: a.hidden,
            attached_to: None,
        }
    }
}

/// Annotation rows as the reader wants them: each note carrying the id of the
/// highlight it annotates.
fn with_attachments(rows: Vec<db::AnnotationRow>) -> Vec<AnnotationDto> {
    let attached: std::collections::HashMap<i64, i64> =
        notes::attachments(&rows).into_iter().collect();
    rows.into_iter()
        .map(|row| {
            let parent = attached.get(&row.id).copied();
            AnnotationDto {
                attached_to: parent,
                ..AnnotationDto::from(row)
            }
        })
        .collect()
}

/// List a book's stored annotations, ordered by reading position.
#[tauri::command]
pub async fn annotations_for_book(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<Vec<AnnotationDto>, String> {
    let conn = state.db.lock().await;
    let rows = db::list_annotations_for_book(&conn, book_id).map_err(|e| e.to_string())?;
    Ok(with_attachments(rows))
}

/// One stored last-read position: `source` is `"sidle"` or `"device"`,
/// `(eid, offset)` its anchor and `linear_pos` its Location.
#[derive(Debug, Serialize)]
pub struct ReadingPositionDto {
    pub eid: Option<i64>,
    pub offset: Option<i64>,
    pub linear_pos: Option<i64>,
    pub source: String,
    pub device_serial: String,
    pub updated_at: String,
}

/// A book's saved positions: the reader's own (`'sidle'`) plus one per Kindle
/// that synced it (`'device'`, keyed by serial).
#[tauri::command]
pub async fn reading_position_get(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<Vec<ReadingPositionDto>, String> {
    let conn = state.db.lock().await;
    let rows = db::list_reading_positions(&conn, book_id).map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|p| ReadingPositionDto {
            eid: p.eid,
            offset: p.offset,
            linear_pos: p.linear_pos,
            source: p.source,
            device_serial: p.device_serial,
            updated_at: p.updated_at,
        })
        .collect())
}

/// Save the reader's own last-read position (`source='sidle'`).
#[tauri::command]
pub async fn reading_position_set(
    state: State<'_, AppState>,
    book_id: i64,
    eid: Option<i64>,
    offset: Option<i64>,
    linear_pos: Option<i64>,
) -> Result<(), String> {
    let conn = state.db.lock().await;
    db::set_reading_position(&conn, book_id, eid, offset, linear_pos, "sidle", "")
        .map_err(|e| e.to_string())
}

/// One search hit. `off_end` is the **inclusive** last-char index of the match,
/// as in an annotation's `(eid_start, off_start, eid_end, off_end)`.
#[derive(Debug, Serialize)]
pub struct SearchMatchDto {
    pub eid: i64,
    pub off_start: i64,
    pub off_end: i64,
    pub linear_pos: i64,
    pub preview_before: String,
    pub preview_match: String,
    pub preview_after: String,
}

impl From<sidle_core::library::anchor::SearchMatch> for SearchMatchDto {
    fn from(m: sidle_core::library::anchor::SearchMatch) -> Self {
        SearchMatchDto {
            eid: m.eid,
            off_start: m.off_start as i64,
            off_end: m.off_end as i64,
            linear_pos: m.linear_pos,
            preview_before: m.preview_before,
            preview_match: m.preview_match,
            preview_after: m.preview_after,
        }
    }
}

/// In-book full-text search: strict char match, ASCII case-insensitive,
/// intra-eid — see [`BookIndex::search`](sidle_core::library::anchor::BookIndex::search).
#[tauri::command]
pub async fn book_search(
    state: State<'_, AppState>,
    book_id: i64,
    query: String,
) -> Result<Vec<SearchMatchDto>, String> {
    use std::sync::Arc;

    let cached: Option<Arc<sidle_core::library::anchor::BookIndex>> = {
        let guard = state.reader_search_cache.lock().await;
        match &*guard {
            Some((id, idx)) if *id == book_id => Some(idx.clone()),
            _ => None,
        }
    };

    let index = match cached {
        Some(i) => i,
        None => {
            // Resolve KFX path under the db lock, then release it before the
            // CPU-bound parse — same shape as reader_open.
            let kfx_path = {
                let conn = state.db.lock().await;
                db::get_book(&conn, book_id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("no book with id {book_id}"))?
                    .kfx_path
                    .ok_or_else(|| "this book has no KFX file yet".to_string())?
            };
            let built = tokio::task::spawn_blocking(move || {
                let bytes =
                    std::fs::read(&kfx_path).map_err(|e| format!("read {kfx_path}: {e}"))?;
                sidle_core::library::anchor::BookIndex::from_kfx(&bytes)
                    .ok_or_else(|| format!("could not read {kfx_path} as KFX"))
            })
            .await
            .map_err(|e| format!("search task join error: {e}"))??;
            let arc = Arc::new(built);
            // Single-entry: `book_id`'s index replaces any other book's.
            let mut guard = state.reader_search_cache.lock().await;
            *guard = Some((book_id, arc.clone()));
            arc
        }
    };

    // `index.search` scans the whole corpus on a large book (~1M chars).
    let matches = tokio::task::spawn_blocking(move || index.search(&query))
        .await
        .map_err(|e| format!("search task join error: {e}"))?;

    Ok(matches.into_iter().map(SearchMatchDto::from).collect())
}

// ---------------------------------------------------------------------------
// Native annotations: create, edit and delete the reader's own.

/// Create a native annotation, salting the shared content dedup hash with the
/// book's title key. Returns the stored row, or the existing one.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn annotation_create(
    state: State<'_, AppState>,
    book_id: i64,
    kind: String,
    eid_start: Option<i64>,
    off_start: Option<i64>,
    eid_end: Option<i64>,
    off_end: Option<i64>,
    loc_start: Option<i64>,
    linear_pos: Option<i64>,
    text: String,
    note_body: Option<String>,
    color: Option<String>,
) -> Result<AnnotationDto, String> {
    let conn = state.db.lock().await;
    let title = db::get_book(&conn, book_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no book with id {book_id}"))?
        .title;
    let book_key = ingest::book_match_key(&title);
    let hash = ingest::annotation_dedup_hash(
        &book_key,
        &kind,
        eid_start,
        off_start,
        eid_end,
        off_end,
        &text,
        note_body.as_deref().unwrap_or(""),
    );
    let now = db::now_iso();
    let row = db::NewAnnotation {
        dedup_hash: &hash,
        book_id: Some(book_id),
        kind: &kind,
        eid_start,
        off_start,
        eid_end,
        off_end,
        loc_start,
        loc_end: loc_start, // a single-point Location
        linear_pos,
        text: &text,
        note_body: note_body.as_deref(),
        color: color.as_deref(),
        clip_title: None,
        clip_author: None,
        added_at: Some(&now),
        added_raw: None,
        imported_at: &now,
        source: ingest::SOURCE_SIDLE,
    };
    // `clear_deletion` lets device sync carry the recreated `hash`.
    db::clear_deletion(&conn, db::DELETION_ANNOTATION, &hash).map_err(|e| e.to_string())?;
    db::insert_annotation(&conn, &row).map_err(|e| e.to_string())?;
    // `hash` names the stored row, fresh or pre-existing.
    let stored = db::get_annotation_by_hash(&conn, &hash)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "annotation missing after insert".to_string())?;
    Ok(AnnotationDto::from(stored))
}

/// Edit a native annotation's `kind` / `note_body` / `color`, recomputing the
/// content hash that folds them in.
#[tauri::command]
pub async fn annotation_update(
    state: State<'_, AppState>,
    id: i64,
    kind: String,
    note_body: Option<String>,
    color: Option<String>,
) -> Result<AnnotationDto, String> {
    let conn = state.db.lock().await;
    let row = db::get_annotation(&conn, id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no annotation with id {id}"))?;
    let book_key = match row.book_id {
        Some(bid) => db::get_book(&conn, bid)
            .map_err(|e| e.to_string())?
            .map(|b| ingest::book_match_key(&b.title))
            .unwrap_or_default(),
        None => String::new(),
    };
    let hash = ingest::annotation_dedup_hash(
        &book_key,
        &kind,
        row.eid_start,
        row.off_start,
        row.eid_end,
        row.off_end,
        &row.text,
        note_body.as_deref().unwrap_or(""),
    );
    db::update_annotation(
        &conn,
        id,
        &kind,
        note_body.as_deref(),
        color.as_deref(),
        &hash,
    )
    .map_err(|e| e.to_string())?;
    let updated = db::get_annotation(&conn, id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "annotation missing after update".to_string())?;
    Ok(AnnotationDto::from(updated))
}

/// Write the note attached to `highlight_id`, creating, replacing or removing it.
#[tauri::command]
pub async fn annotation_set_note(
    state: State<'_, AppState>,
    highlight_id: i64,
    note_id: Option<i64>,
    body: Option<String>,
) -> Result<Vec<AnnotationDto>, String> {
    let conn = state.db.lock().await;
    let hl = db::get_annotation(&conn, highlight_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no annotation with id {highlight_id}"))?;
    let book_id = hl
        .book_id
        .ok_or_else(|| "annotation is not linked to a book".to_string())?;

    if let Some(id) = note_id {
        db::delete_annotation(&conn, id).map_err(|e| e.to_string())?;
    }

    let body = body.unwrap_or_default();
    let body = body.trim();
    if !body.is_empty() {
        let title = db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .map(|b| b.title)
            .unwrap_or_default();
        let book_key = ingest::book_match_key(&title);
        let hash = ingest::annotation_dedup_hash(
            &book_key,
            "note",
            hl.eid_start,
            hl.off_start,
            hl.eid_end,
            hl.off_end,
            &hl.text,
            body,
        );
        let now = db::now_iso();
        // `clear_deletion` lets device sync carry this exact note again.
        db::clear_deletion(&conn, db::DELETION_ANNOTATION, &hash).map_err(|e| e.to_string())?;
        db::insert_annotation(
            &conn,
            &db::NewAnnotation {
                dedup_hash: &hash,
                book_id: Some(book_id),
                kind: "note",
                eid_start: hl.eid_start,
                off_start: hl.off_start,
                eid_end: hl.eid_end,
                off_end: hl.off_end,
                loc_start: hl.loc_start,
                loc_end: hl.loc_end,
                linear_pos: hl.linear_pos,
                text: &hl.text,
                note_body: Some(body),
                color: None,
                clip_title: None,
                clip_author: None,
                added_at: Some(&now),
                added_raw: None,
                imported_at: &now,
                source: ingest::SOURCE_SIDLE,
            },
        )
        .map_err(|e| e.to_string())?;
    }

    let rows = db::list_annotations_for_book(&conn, book_id).map_err(|e| e.to_string())?;
    Ok(with_attachments(rows))
}

/// Delete a native annotation by id.
#[tauri::command]
pub async fn annotation_delete(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    let conn = state.db.lock().await;
    db::delete_annotation(&conn, id).map_err(|e| e.to_string())?;
    Ok(())
}

/// Hand an external book link off to the OS default handler.
#[tauri::command]
pub async fn open_external_url(app: AppHandle, url: String) -> Result<(), String> {
    let lower = url.trim_start().to_ascii_lowercase();
    let scheme_ok = lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:");
    if !scheme_ok {
        return Err(format!("refusing to open non-web URL: {url}"));
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}
