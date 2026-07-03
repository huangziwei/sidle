//! PDF page rasterization + text extraction via **Apple PDFKit / Core Graphics**
//! — the system engine Preview uses. No bundled `libpdfium`: we call the OS.
//!
//! Two jobs, one engine:
//! - **Cover** ([`render_pdf_page_jpeg`]): `PDFPage.draw(with:to:)` into a Core
//!   Graphics bitmap context → JPEG. This is exactly Preview's "export page as
//!   image", and the one thing we do that Amazon's cover-less PDOC doesn't.
//! - **Text layer** ([`extract_pdf_text`]): `PDFPage.string` +
//!   `characterBounds(at:)` give Unicode + per-glyph boxes — Apple's Core Text
//!   does the font/encoding/CMap work — which the KFX emit path turns into the
//!   invisible, selectable text storylines.
//!
//! macOS-only by design (`cfg(target_os = "macos")`): the wasm / Kindle builds
//! don't rasterize (the Kindle never *converts* — conversion is always Mac-side),
//! and a future Linux build would slot Poppler (`poppler_page_get_text_layout` =
//! the same per-char boxes) behind these same two functions. Every other target
//! gets an `Unavailable` stub, and the emit path treats that as "no cover / no
//! text layer", never a failed conversion. See `.claude/plans/pdf-to-kfx.md`.

use std::fmt;

/// Default cover render width in pixels. Page 1 is rendered this wide (height
/// follows the page aspect), which is ample for the library tile and the
/// PDOC sleep-screen art while keeping the embedded JPEG small.
pub const COVER_TARGET_WIDTH_PX: u32 = 1000;

/// JPEG quality (1..=100) for the rendered cover.
pub const COVER_JPEG_QUALITY: u8 = 85;

/// One PDF page's extracted text as positioned runs — the **invisible,
/// selectable overlay** the device draws over the rendered page image. This is
/// how Amazon's PDF→KFX makes a "fixed-layout" page's text *live*
/// (select / search / dictionary / highlight) rather than a flat picture: each
/// run is laid out at a fixed position with `visibility: false`, so the reader
/// hit-tests against it while showing the crisp PDF glyphs underneath.
///
/// All geometry is in **points × 100** (the KFX fixed-layout unit), with a
/// **top-left origin and Y increasing downward** — the same space as the page
/// storyline's `fixed_width`/`fixed_height`. PDFKit's native page space (points,
/// bottom-left origin, Y up) is flipped during extraction.
#[derive(Debug, Clone, Default)]
pub struct PageText {
    /// Text runs (≈ visual lines), in reading order.
    pub runs: Vec<TextRun>,
}

/// One run of text — roughly a visual line fragment — placed on the page.
#[derive(Debug, Clone)]
pub struct TextRun {
    /// The run's text in reading order (UTF-8).
    pub content: String,
    /// Left edge, pt×100 from the page's left.
    pub left: i64,
    /// Top edge, pt×100 from the page's top.
    pub top: i64,
    /// Width, pt×100.
    pub width: i64,
    /// Height, pt×100.
    pub height: i64,
    /// Baseline distance from the page top, pt×100 (Amazon's `text_baseline`).
    pub baseline: i64,
    /// Word / inter-word-space segmentation of `content`, in order — drives the
    /// custom word iterator (double-tap-to-select-word, dictionary lookup).
    pub words: Vec<StyleSeg>,
}

/// One `style_events` entry: a `[offset, offset+length)` slice of the run's
/// `content` with its rendered `width` (pt×100). Offsets/lengths are **UTF-16
/// code units** (KFX's string-indexing convention — and PDFKit's native unit).
/// `is_word` marks a word (Amazon's `model: word`) versus an inter-word run of
/// whitespace (no model).
#[derive(Debug, Clone)]
pub struct StyleSeg {
    pub offset: usize,
    pub length: usize,
    pub width: i64,
    pub is_word: bool,
}

/// Why a render/extract didn't produce output. Callers treat every variant as
/// "proceed without a cover / text layer", so this never aborts a conversion.
#[derive(Debug, Clone)]
pub enum RenderError {
    /// No usable PDF engine in this build/platform (non-macOS today).
    Unavailable(String),
    /// The engine loaded but failed on this document/page.
    Render(String),
    /// The rendered bitmap couldn't be JPEG-encoded.
    Encode(String),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::Unavailable(e) => write!(f, "PDF engine unavailable: {e}"),
            RenderError::Render(e) => write!(f, "pdf render failed: {e}"),
            RenderError::Encode(e) => write!(f, "cover JPEG encode failed: {e}"),
        }
    }
}

impl std::error::Error for RenderError {}

/// Render one PDF page to a JPEG, scaled to `target_width_px` wide (height
/// follows the page aspect). `page_index` is 0-based; `quality` is 1..=100.
/// Returns the JPEG bytes, or a [`RenderError`] the caller downgrades to
/// "no cover".
#[cfg(target_os = "macos")]
pub fn render_pdf_page_jpeg(
    pdf_bytes: &[u8],
    page_index: usize,
    target_width_px: u32,
    quality: u8,
) -> Result<Vec<u8>, RenderError> {
    macos::render_page_jpeg(pdf_bytes, page_index, target_width_px, quality)
}

/// Non-macOS stub: rasterization needs a platform PDF engine (PDFKit on macOS;
/// Poppler on a future Linux build). The wasm/Kindle builds produce a
/// visual-only KFX (no cover).
#[cfg(not(target_os = "macos"))]
pub fn render_pdf_page_jpeg(
    _pdf_bytes: &[u8],
    _page_index: usize,
    _target_width_px: u32,
    _quality: u8,
) -> Result<Vec<u8>, RenderError> {
    Err(RenderError::Unavailable(
        "PDF rasterization needs macOS PDFKit — see pdf-to-kfx.md".into(),
    ))
}

/// Extract every page's selectable text layer from the PDF, in page order.
///
/// Returns one [`PageText`] per page (empty `runs` for a page with no
/// extractable text — e.g. a scanned/image-only page). A whole-document
/// [`RenderError`] lets the caller ship an embed-only, visual-only KFX.
#[cfg(target_os = "macos")]
pub fn extract_pdf_text(pdf_bytes: &[u8]) -> Result<Vec<PageText>, RenderError> {
    macos::extract_text(pdf_bytes)
}

/// Non-macOS stub: text extraction needs a platform PDF engine. The wasm/Kindle
/// builds produce a visual-only KFX (no selectable text).
#[cfg(not(target_os = "macos"))]
pub fn extract_pdf_text(_pdf_bytes: &[u8]) -> Result<Vec<PageText>, RenderError> {
    Err(RenderError::Unavailable(
        "PDF text extraction needs macOS PDFKit — see pdf-to-kfx.md".into(),
    ))
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{PageText, RenderError, StyleSeg, TextRun};
    use core::ffi::c_void;
    use objc2::AnyThread; // brings `PDFDocument::alloc()` into scope
    use objc2::rc::{Retained, autoreleasepool};
    use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo};
    use objc2_foundation::NSData;
    use objc2_pdf_kit::{PDFDisplayBox, PDFDocument, PDFPage};

    /// KFX fixed-layout unit: points × 100.
    const SCALE: f32 = 100.0;

    /// Parse the in-memory PDF into a `PDFDocument` (a copy — PDFKit owns it).
    fn load(pdf_bytes: &[u8]) -> Result<Retained<PDFDocument>, RenderError> {
        let data = NSData::with_bytes(pdf_bytes);
        // SAFETY: `data` outlives the init call; PDFKit copies what it needs.
        unsafe { PDFDocument::initWithData(PDFDocument::alloc(), &data) }
            .ok_or_else(|| RenderError::Render("PDFKit could not parse the PDF".into()))
    }

    pub(super) fn render_page_jpeg(
        pdf_bytes: &[u8],
        page_index: usize,
        target_width_px: u32,
        quality: u8,
    ) -> Result<Vec<u8>, RenderError> {
        autoreleasepool(|_| {
            let doc = load(pdf_bytes)?;
            let page = unsafe { doc.pageAtIndex(page_index) }
                .ok_or_else(|| RenderError::Render(format!("no page {page_index}")))?;
            // The MediaBox is the page a viewer shows (= Preview): its origin may be
            // non-zero, and content/marks outside it (a press PDF's bleed + trim
            // marks) are clipped, which is what we want. Amazon's KFX text layer
            // measures from the MediaBox origin too (verified: a glyph at absolute
            // x=72 on a page with MediaBox `[9 9 441 657]` is stored at 63), so the
            // overlay lines up when we draw the MediaBox origin-relative.
            let bounds = unsafe { page.boundsForBox(PDFDisplayBox::MediaBox) };
            let (pw, ph) = (bounds.size.width, bounds.size.height);
            if pw <= 0.0 || ph <= 0.0 {
                return Err(RenderError::Render(format!("unusable page size {pw}x{ph}")));
            }
            let scale = target_width_px as f64 / pw;
            let out_w = target_width_px as usize;
            let out_h = ((ph * scale).round() as usize).max(1);
            if out_w > u16::MAX as usize || out_h > u16::MAX as usize {
                return Err(RenderError::Render(format!(
                    "bitmap too large {out_w}x{out_h}"
                )));
            }

            // RGBA bitmap we own; PDFKit draws into it. Premultiplied alpha — we
            // composite over white below (pages don't paint their paper).
            let bytes_per_row = out_w * 4;
            let mut buf = vec![0u8; bytes_per_row * out_h];
            let cs = CGColorSpace::new_device_rgb()
                .ok_or_else(|| RenderError::Render("no RGB colorspace".into()))?;
            let ctx_owned = unsafe {
                CGBitmapContextCreate(
                    buf.as_mut_ptr() as *mut c_void,
                    out_w,
                    out_h,
                    8,
                    bytes_per_row,
                    Some(&cs),
                    CGImageAlphaInfo::PremultipliedLast.0,
                )
            }
            .ok_or_else(|| RenderError::Render("CGBitmapContext create failed".into()))?;
            let ctx: &CGContext = &ctx_owned;

            // Scale points → pixels. `drawWithBox(MediaBox)` already maps the
            // MediaBox's lower-left to the context origin (so a non-(0,0) MediaBox
            // origin is handled by PDFKit) and clips to the MediaBox — exactly
            // Preview's output, so a press PDF's bleed/trim marks outside the
            // MediaBox don't render. Do NOT also translate by the origin: that
            // double-subtracts it, shifting the page left/up off the bitmap (the
            // original bug — Street's gutter clipped, Peter offset 9 pt).
            // `extract_text` measures from the same MediaBox origin, so the overlay
            // spans line up with the rendered glyphs.
            CGContext::scale_ctm(Some(ctx), scale, scale);
            unsafe { page.drawWithBox_toContext(PDFDisplayBox::MediaBox, ctx) };
            drop(ctx_owned); // flush + release before we read `buf`

            // Composite premultiplied-RGBA over white, drop alpha → RGB. For
            // premultiplied colour `sv = c*a/255`, over white = `sv + (255 - a)`.
            let mut rgb: Vec<u8> = Vec::with_capacity(out_w * out_h * 3);
            for px in buf.chunks_exact(4) {
                let a = px[3] as u32;
                let over_white = |sv: u8| ((sv as u32) + (255 - a)).min(255) as u8;
                rgb.push(over_white(px[0]));
                rgb.push(over_white(px[1]));
                rgb.push(over_white(px[2]));
            }

            let mut out: Vec<u8> = Vec::with_capacity(128 * 1024);
            jpeg_encoder::Encoder::new(&mut out, quality)
                .encode(
                    &rgb,
                    out_w as u16,
                    out_h as u16,
                    jpeg_encoder::ColorType::Rgb,
                )
                .map_err(|e| RenderError::Encode(e.to_string()))?;
            Ok(out)
        })
    }

    pub(super) fn extract_text(pdf_bytes: &[u8]) -> Result<Vec<PageText>, RenderError> {
        autoreleasepool(|_| {
            let doc = load(pdf_bytes)?;
            let n = unsafe { doc.pageCount() };
            let mut pages = Vec::with_capacity(n);
            for i in 0..n {
                let page = unsafe { doc.pageAtIndex(i) };
                pages.push(match page {
                    Some(p) => extract_page(&p),
                    None => PageText::default(),
                });
            }
            Ok(pages)
        })
    }

    /// One UTF-16 unit with its page-space box (points, bottom-left origin).
    struct Unit {
        unit: u16,
        left: f32,
        right: f32,
        top: f32,
        bottom: f32,
        boxed: bool,
    }

    /// True if a lone UTF-16 unit is whitespace. Surrogate halves (astral chars)
    /// aren't whitespace, so testing per-unit is safe.
    fn is_space(u: u16) -> bool {
        char::from_u32(u as u32).is_some_and(|c| c.is_whitespace())
    }

    fn extract_page(page: &PDFPage) -> PageText {
        let bounds = unsafe { page.boundsForBox(PDFDisplayBox::MediaBox) };
        // KFX positions are measured from the **MediaBox** top-left, Y down —
        // matching Amazon's S2K text layer (verified: on a page with MediaBox
        // `[9 9 441 657]`, a glyph at absolute x=72 is stored at 63 = 72−9, and a
        // line at absolute y=384 at top 263 = (9+648)−384). The renderer draws the
        // MediaBox origin-relative too, so glyph boxes and the image line up.
        let box_left = bounds.origin.x as f32;
        let box_top = (bounds.origin.y + bounds.size.height) as f32;

        let text = match unsafe { page.string() } {
            Some(s) => s.to_string(),
            None => return PageText::default(),
        };
        // PDFKit's `string` inserts line/fragment separators (\n, \r) that the
        // `characterBoundsAtIndex` index space does NOT contain — leaving them in
        // drifts the glyph↔bounds alignment by one per line. Drop them so
        // `utf16[i]` lines up with `characterBoundsAtIndex(i)`; visual lines are
        // recovered from geometry below, and real inter-word spaces are kept.
        let utf16: Vec<u16> = text
            .encode_utf16()
            .filter(|&u| u != 0x000A && u != 0x000D)
            .collect();
        // `numberOfCharacters` is the glyph (bounds) count; clamp defensively.
        let n = utf16.len().min(unsafe { page.numberOfCharacters() });
        if n == 0 {
            return PageText::default();
        }

        let mut units: Vec<Unit> = Vec::with_capacity(n);
        for (i, &u) in utf16.iter().enumerate().take(n) {
            let r = unsafe { page.characterBoundsAtIndex(i as isize) };
            let w = r.size.width as f32;
            let h = r.size.height as f32;
            let left = r.origin.x as f32;
            let bottom = r.origin.y as f32;
            units.push(Unit {
                unit: u,
                left,
                right: left + w,
                top: bottom + h,
                bottom,
                boxed: w > 0.0 && h > 0.0,
            });
        }

        // Turn one slice of units (a visual line) into a TextRun, or None if it
        // has no boxed, non-whitespace content.
        fn finish(us: &[Unit], box_left: f32, box_top: f32) -> Option<TextRun> {
            let raw: Vec<u16> = us.iter().map(|u| u.unit).collect();
            let content = String::from_utf16_lossy(&raw);
            if content.trim().is_empty() {
                return None;
            }
            let boxed: Vec<&Unit> = us.iter().filter(|u| u.boxed).collect();
            if boxed.is_empty() {
                return None;
            }
            let min_left = boxed.iter().map(|u| u.left).fold(f32::INFINITY, f32::min);
            let max_right = boxed
                .iter()
                .map(|u| u.right)
                .fold(f32::NEG_INFINITY, f32::max);
            let max_top = boxed
                .iter()
                .map(|u| u.top)
                .fold(f32::NEG_INFINITY, f32::max);
            let min_bottom = boxed.iter().map(|u| u.bottom).fold(f32::INFINITY, f32::min);

            let left = ((min_left - box_left) * SCALE).round() as i64;
            let top = ((box_top - max_top) * SCALE).round() as i64;
            let width = ((max_right - min_left) * SCALE).round() as i64;
            let height = ((max_top - min_bottom) * SCALE).round() as i64;
            let baseline = ((box_top - min_bottom) * SCALE).round() as i64;

            // Contiguous word / whitespace segments, each `[start, start+len)` in
            // UTF-16 units, with the left/right ink extent of its boxed chars
            // (a whitespace run carries no ink box, so its extent stays empty).
            struct Seg {
                start: usize,
                len: usize,
                space: bool,
                l: f32,
                r: f32,
            }
            let mut segs: Vec<Seg> = Vec::new();
            let mut i = 0usize;
            while i < us.len() {
                let space = is_space(us[i].unit);
                let start = i;
                let mut l = f32::INFINITY;
                let mut r = f32::NEG_INFINITY;
                while i < us.len() && is_space(us[i].unit) == space {
                    if us[i].boxed {
                        l = l.min(us[i].left);
                        r = r.max(us[i].right);
                    }
                    i += 1;
                }
                segs.push(Seg {
                    start,
                    len: i - start,
                    space,
                    l,
                    r,
                });
            }

            // A word's width is its own ink extent; a space's width is the gap
            // between the neighbouring words (PDFKit gives spaces no ink box, so
            // their advance has to come from the surrounding glyphs).
            let words = segs
                .iter()
                .enumerate()
                .map(|(k, s)| {
                    let width = if !s.space && s.r > s.l {
                        ((s.r - s.l) * SCALE).round() as i64
                    } else if s.space {
                        let prev_r = segs[..k]
                            .iter()
                            .rev()
                            .find_map(|p| p.r.is_finite().then_some(p.r));
                        let next_l = segs[k + 1..]
                            .iter()
                            .find_map(|p| p.l.is_finite().then_some(p.l));
                        match (prev_r, next_l) {
                            (Some(pr), Some(nl)) if nl > pr => ((nl - pr) * SCALE).round() as i64,
                            _ => 0,
                        }
                    } else {
                        0
                    };
                    StyleSeg {
                        offset: s.start,
                        length: s.len,
                        width,
                        is_word: !s.space,
                    }
                })
                .collect();

            Some(TextRun {
                content,
                left,
                top,
                width,
                height,
                baseline,
                words,
            })
        }

        // Group units into visual lines by VERTICAL OVERLAP: a char whose whole
        // box sits below the current line's lowest point starts a new (lower)
        // line. This is robust to descenders (which dip the bottom but keep their
        // top within the line) where a baseline-jump threshold over-splits. Run
        // granularity only needs to track lines — selection/search read the
        // per-word geometry above, not run boundaries.
        let mut runs: Vec<TextRun> = Vec::new();
        let mut cur: Vec<Unit> = Vec::new();
        let mut line_bottom: Option<f32> = None;
        for u in units {
            if u.boxed {
                match line_bottom {
                    Some(lb) if u.top < lb && !cur.is_empty() => {
                        if let Some(r) = finish(&cur, box_left, box_top) {
                            runs.push(r);
                        }
                        cur = Vec::new();
                        line_bottom = Some(u.bottom);
                    }
                    Some(lb) => line_bottom = Some(lb.min(u.bottom)),
                    None => line_bottom = Some(u.bottom),
                }
            }
            cur.push(u);
        }
        if let Some(r) = finish(&cur, box_left, box_top) {
            runs.push(r);
        }

        PageText { runs }
    }
}
