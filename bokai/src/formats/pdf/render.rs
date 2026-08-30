//! PDF page rasterization + text extraction via **Apple PDFKit / Core Graphics**
//! — the system engine Preview uses. No bundled `libpdfium`: we call the OS.

use std::fmt;

/// Default cover render width in pixels. Page 1 is rendered this wide (height
/// follows the page aspect), which is ample for the library tile and the
/// PDOC sleep-screen art while keeping the embedded JPEG small.
pub const COVER_TARGET_WIDTH_PX: u32 = 1000;

/// JPEG quality (1..=100) for the rendered cover.
pub const COVER_JPEG_QUALITY: u8 = 85;

/// One PDF page's extracted text as positioned runs — the **invisible,
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
#[cfg(target_os = "macos")]
pub fn render_pdf_page_jpeg(
    pdf_bytes: &[u8],
    page_index: usize,
    target_width_px: u32,
    quality: u8,
) -> Result<Vec<u8>, RenderError> {
    macos::render_page_jpeg(pdf_bytes, page_index, target_width_px, quality)
}

/// Render one PDF page to a PNG — the lossless counterpart of
/// [`render_pdf_page_jpeg`], sharing its rasterizer.
#[cfg(target_os = "macos")]
pub fn render_pdf_page_png(
    pdf_bytes: &[u8],
    page_index: usize,
    target_width_px: u32,
) -> Result<Vec<u8>, RenderError> {
    macos::render_page_png(pdf_bytes, page_index, target_width_px)
}

/// Non-macOS stub — see [`render_pdf_page_jpeg`].
#[cfg(not(target_os = "macos"))]
pub fn render_pdf_page_png(
    _pdf_bytes: &[u8],
    _page_index: usize,
    _target_width_px: u32,
) -> Result<Vec<u8>, RenderError> {
    Err(RenderError::Unavailable(
        "PDF rasterization needs macOS PDFKit".into(),
    ))
}

/// Non-macOS stub: rasterization needs a platform PDF engine (PDFKit on macOS;
/// Poppler on a future Linux build). Non-macOS builds produce a visual-only KFX
/// (no cover).
#[cfg(not(target_os = "macos"))]
pub fn render_pdf_page_jpeg(
    _pdf_bytes: &[u8],
    _page_index: usize,
    _target_width_px: u32,
    _quality: u8,
) -> Result<Vec<u8>, RenderError> {
    Err(RenderError::Unavailable(
        "PDF rasterization needs macOS PDFKit; this build has no PDF renderer".into(),
    ))
}

/// Extract every page's selectable text layer from the PDF, in page order.
#[cfg(target_os = "macos")]
pub fn extract_pdf_text(pdf_bytes: &[u8]) -> Result<Vec<PageText>, RenderError> {
    macos::extract_text(pdf_bytes)
}

/// Non-macOS stub: text extraction needs a platform PDF engine, so these builds
/// produce a visual-only KFX (no selectable text).
#[cfg(not(target_os = "macos"))]
pub fn extract_pdf_text(_pdf_bytes: &[u8]) -> Result<Vec<PageText>, RenderError> {
    Err(RenderError::Unavailable(
        "PDF text extraction needs macOS PDFKit; this build has no PDF text layer".into(),
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

    /// A page's `/Rotate` as quarter turns clockwise (0..=3).
    fn quarter_turns(page: &PDFPage) -> u8 {
        (((unsafe { page.rotation() } as f64 / 90.0).round() as i64).rem_euclid(4)) as u8
    }

    /// Parse the in-memory PDF into a `PDFDocument` (a copy — PDFKit owns it).
    fn load(pdf_bytes: &[u8]) -> Result<Retained<PDFDocument>, RenderError> {
        let data = NSData::with_bytes(pdf_bytes);
        // SAFETY: `data` outlives the init call; PDFKit copies what it needs.
        unsafe { PDFDocument::initWithData(PDFDocument::alloc(), &data) }
            .ok_or_else(|| RenderError::Render("PDFKit could not parse the PDF".into()))
    }

    /// Rasterize one page to packed RGB8 (no alpha), returning `(rgb, w, h)`.
    /// The shared core of every page-image path — what gets encoded on top of it
    /// is the caller's choice.
    pub(super) fn render_page_rgb(
        pdf_bytes: &[u8],
        page_index: usize,
        target_width_px: u32,
    ) -> Result<(Vec<u8>, u32, u32), RenderError> {
        autoreleasepool(|_| {
            let doc = load(pdf_bytes)?;
            let page = unsafe { doc.pageAtIndex(page_index) }
                .ok_or_else(|| RenderError::Render(format!("no page {page_index}")))?;
            // The MediaBox is the page a viewer shows (= Preview): its origin may be
            let bounds = unsafe { page.boundsForBox(PDFDisplayBox::MediaBox) };
            // `boundsForBox` reports the page's own box, *un*rotated, while
            let (mut pw, mut ph) = (bounds.size.width, bounds.size.height);
            if quarter_turns(&page) % 2 == 1 {
                std::mem::swap(&mut pw, &mut ph);
            }
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

            Ok((rgb, out_w as u32, out_h as u32))
        })
    }

    pub(super) fn render_page_jpeg(
        pdf_bytes: &[u8],
        page_index: usize,
        target_width_px: u32,
        quality: u8,
    ) -> Result<Vec<u8>, RenderError> {
        let (rgb, w, h) = render_page_rgb(pdf_bytes, page_index, target_width_px)?;
        let mut out: Vec<u8> = Vec::with_capacity(128 * 1024);
        jpeg_encoder::Encoder::new(&mut out, quality)
            .encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
            .map_err(|e| RenderError::Encode(e.to_string()))?;
        Ok(out)
    }

    pub(super) fn render_page_png(
        pdf_bytes: &[u8],
        page_index: usize,
        target_width_px: u32,
    ) -> Result<Vec<u8>, RenderError> {
        use image::codecs::png::PngEncoder;
        use image::{ExtendedColorType, ImageEncoder};

        let (rgb, w, h) = render_page_rgb(pdf_bytes, page_index, target_width_px)?;
        let mut out: Vec<u8> = Vec::with_capacity(256 * 1024);
        PngEncoder::new(&mut out)
            .write_image(&rgb, w, h, ExtendedColorType::Rgb8)
            .map_err(|e| RenderError::Encode(e.to_string()))?;
        Ok(out)
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
        let turns = quarter_turns(page);
        let (x0, y0) = (bounds.origin.x as f32, bounds.origin.y as f32);
        let (bw, bh) = (bounds.size.width as f32, bounds.size.height as f32);
        // The displayed page, origin at its bottom-left, Y up — the same
        // convention `Unit` and `finish` already use.
        let box_left = 0.0f32;
        let box_top = if turns % 2 == 1 { bw } else { bh };

        let text = match unsafe { page.string() } {
            Some(s) => s.to_string(),
            None => return PageText::default(),
        };
        // PDFKit's `string` inserts line/fragment separators (\n, \r) that the
        let utf16: Vec<u16> = text
            .encode_utf16()
            .filter(|&u| u != 0x000A && u != 0x000D)
            .collect();
        // `numberOfCharacters` is the glyph (bounds) count; clamp defensively.
        let n = utf16.len().min(unsafe { page.numberOfCharacters() });
        if n == 0 {
            return PageText::default();
        }

        // Page space → displayed space, both origin-at-bottom-left and Y up. The
        // quarter turn is clockwise, so the page's bottom-left corner becomes the
        // displayed top-left at one turn, the bottom-right at two, and so on.
        let map = |x: f32, y: f32| -> (f32, f32) {
            match turns {
                1 => (y - y0, (x0 + bw) - x),
                2 => ((x0 + bw) - x, (y0 + bh) - y),
                3 => ((y0 + bh) - y, x - x0),
                _ => (x - x0, y - y0),
            }
        };

        let mut units: Vec<Unit> = Vec::with_capacity(n);
        for (i, &u) in utf16.iter().enumerate().take(n) {
            let r = unsafe { page.characterBoundsAtIndex(i as isize) };
            let w = r.size.width as f32;
            let h = r.size.height as f32;
            let x = r.origin.x as f32;
            let y = r.origin.y as f32;
            // Map opposite corners, then re-order: a turn can swap which is which.
            let (ax, ay) = map(x, y);
            let (bx, by) = map(x + w, y + h);
            units.push(Unit {
                unit: u,
                left: ax.min(bx),
                right: ax.max(bx),
                top: ay.max(by),
                bottom: ay.min(by),
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
            let mut words: Vec<StyleSeg> = Vec::with_capacity(segs.len());
            for (k, s) in segs.iter().enumerate() {
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
                words.push(StyleSeg {
                    offset: s.start,
                    length: s.len,
                    width,
                    is_word: !s.space,
                });
            }

            // Amazon ends every run with a space, folded into the final word's
            let mut content = content;
            if !content.ends_with(char::is_whitespace) {
                content.push(' ');
                if let Some(last) = words.last_mut() {
                    last.length += 1;
                }
            }

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

#[cfg(test)]
mod tests {
    /// Assemble a one-page PDF with the given MediaBox and `/Rotate`, drawing a
    fn one_page_pdf(media: [i32; 4], rotate: i64) -> Vec<u8> {
        let content = "BT /F1 24 Tf 40 30 Td (Hi) Tj ET\n";
        let objects: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [{} {} {} {}] /Rotate {rotate} \
                 /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
                media[0], media[1], media[2], media[3]
            ),
            format!(
                "<< /Length {} >>\nstream\n{content}endstream",
                content.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];

        let mut out: Vec<u8> = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    /// `/Rotate` reaches the probed page as quarter turns, with the extents
    /// already swapped for the odd ones. Values off the quarter grid, and the
    /// negative and past-360 spellings real PDFs use, all fold into 0..=3.
    #[test]
    fn page_geometry_reports_quarter_turns() {
        use crate::formats::pdf::doc::{load_pdf, page_geometry};

        for (rotate, want) in [
            (0, (400.0, 200.0, 0)),
            (90, (200.0, 400.0, 1)),
            (180, (400.0, 200.0, 2)),
            (270, (200.0, 400.0, 3)),
            (-90, (200.0, 400.0, 3)),
            (450, (200.0, 400.0, 1)),
            (75, (200.0, 400.0, 1)),
        ] {
            let pdf = one_page_pdf([0, 0, 400, 200], rotate);
            let doc = load_pdf(&pdf).unwrap();
            let page_id = *doc.get_pages().values().next().unwrap();
            assert_eq!(page_geometry(&doc, page_id), want, "/Rotate {rotate}");
        }
    }

    /// The rendered page and the text overlay must agree on a rotated page:
    #[cfg(target_os = "macos")]
    #[test]
    fn rotated_page_overlay_lands_on_the_rendered_ink() {
        use crate::formats::pdf::doc::{load_pdf, page_geometry};

        const TARGET_W: u32 = 400;

        for rotate in [0, 90, 180, 270] {
            let pdf = one_page_pdf([0, 0, 400, 200], rotate);

            let doc = load_pdf(&pdf).unwrap();
            let page_id = *doc.get_pages().values().next().unwrap();
            let (pw, ph, _) = page_geometry(&doc, page_id);

            // The raster carries the displayed page's aspect, not the stored box.
            let (rgb, rw, rh) = super::macos::render_page_rgb(&pdf, 0, TARGET_W).unwrap();
            let scale = TARGET_W as f32 / pw;
            assert_eq!(rw, TARGET_W, "/Rotate {rotate} raster width");
            assert_eq!(
                rh,
                (ph * scale).round() as u32,
                "/Rotate {rotate} raster height"
            );

            // Bounding box of the drawn glyphs.
            let (mut l, mut t, mut r, mut b) = (u32::MAX, u32::MAX, 0u32, 0u32);
            for y in 0..rh {
                for x in 0..rw {
                    if rgb[((y * rw + x) * 3) as usize] < 128 {
                        l = l.min(x);
                        t = t.min(y);
                        r = r.max(x);
                        b = b.max(y);
                    }
                }
            }
            assert!(l <= r && t <= b, "/Rotate {rotate} rendered no ink");

            // The same glyphs as the overlay places them, in raster pixels.
            let pages = super::macos::extract_text(&pdf).unwrap();
            let run = pages[0].runs.first().expect("one run");
            let px = |v: i64| v as f32 / 100.0 * scale;
            for (name, want, got) in [
                ("left", l as f32, px(run.left)),
                ("top", t as f32, px(run.top)),
                ("right", r as f32, px(run.left + run.width)),
                ("bottom", b as f32, px(run.top + run.height)),
            ] {
                assert!(
                    (want - got).abs() <= 2.0,
                    "/Rotate {rotate} {name}: ink {want} vs overlay {got}"
                );
            }
        }
    }
}
