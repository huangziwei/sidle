//! PDF page rasterization via pdfium (Chrome's PDF engine).
//!
//! Used by the PDF→KFX path to render page 1 as a JPEG cover, and (later) by the
//! Sidle desktop reader to paint PDF pages. pdfium is a C++ library bound at
//! runtime; no mature pure-Rust PDF *renderer* exists (lopdf only parses
//! structure). The emit path ([`crate::export::pdf_to_kfx`]) stays pure — it
//! takes the rendered cover bytes as an argument — so this module is the *only*
//! place that depends on pdfium, and a failure here just means "no cover", never
//! a failed conversion.
//!
//! Native builds bind to a `libpdfium.{dylib,so,dll}` found via, in order: the
//! `BOKO_PDFIUM_LIB` env var (Sidle.app and the test harness stage the bundled
//! binary here), alongside the running executable, then the system library path.
//! The wasm/boko.html cover path (pdfium.wasm + JS glue) is a documented P2
//! follow-up; there the render returns [`RenderError::Unavailable`] and the
//! cover is simply omitted. See `.claude/plans/pdf-to-kfx.md`.

use std::fmt;

/// Default cover render width in pixels. Page 1 is rendered this wide (height
/// follows the page aspect), which is ample for the library tile and the
/// PDOC sleep-screen art while keeping the embedded JPEG small.
pub const COVER_TARGET_WIDTH_PX: u32 = 1000;

/// JPEG quality (1..=100) for the rendered cover.
pub const COVER_JPEG_QUALITY: u8 = 85;

/// Why a render didn't produce an image. Callers treat every variant as
/// "proceed without a cover", so this never aborts a conversion.
#[derive(Debug, Clone)]
pub enum RenderError {
    /// No usable `libpdfium` could be loaded in this process/build.
    Unavailable(String),
    /// pdfium loaded but failed on this document/page.
    Render(String),
    /// The rendered bitmap couldn't be JPEG-encoded.
    Encode(String),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::Unavailable(e) => write!(f, "pdfium unavailable: {e}"),
            RenderError::Render(e) => write!(f, "pdf render failed: {e}"),
            RenderError::Encode(e) => write!(f, "cover JPEG encode failed: {e}"),
        }
    }
}

impl std::error::Error for RenderError {}

/// Render one PDF page to a JPEG, scaled to `target_width_px` wide (height
/// follows the page aspect ratio). `page_index` is 0-based; `quality` is the
/// JPEG quality 1..=100.
///
/// Returns the JPEG bytes, or a [`RenderError`] the caller can downgrade to
/// "no cover". The whole PDF is passed by slice; only the one page is rendered.
#[cfg(not(target_arch = "wasm32"))]
pub fn render_pdf_page_jpeg(
    pdf_bytes: &[u8],
    page_index: usize,
    target_width_px: u32,
    quality: u8,
) -> Result<Vec<u8>, RenderError> {
    use pdfium_render::prelude::*;

    let pdfium = native::pdfium()?;
    let doc = pdfium
        .load_pdf_from_byte_slice(pdf_bytes, None)
        .map_err(|e| RenderError::Render(e.to_string()))?;

    // `PdfPageIndex` (and `Pixels`) are `c_int` (i32) in pdfium-render.
    let index: i32 = page_index
        .try_into()
        .map_err(|_| RenderError::Render(format!("page index {page_index} out of range")))?;
    let page = doc
        .pages()
        .get(index)
        .map_err(|e| RenderError::Render(format!("page {page_index}: {e}")))?;

    let width: i32 = target_width_px.try_into().unwrap_or(i32::MAX);
    let config = PdfRenderConfig::new()
        .set_target_width(width)
        .set_format(PdfBitmapFormat::BGRA)
        .render_form_data(false);
    let bitmap = page
        .render_with_config(&config)
        .map_err(|e| RenderError::Render(e.to_string()))?;

    let w = bitmap.width();
    let h = bitmap.height();
    if w <= 0 || h <= 0 || w > u16::MAX as i32 || h > u16::MAX as i32 {
        return Err(RenderError::Render(format!("unusable bitmap size {w}x{h}")));
    }
    let (w, h) = (w as usize, h as usize);

    // Normalize to true R,G,B,A. pdfium-render reverses byte order during
    // render, so `as_raw_bytes()` with a BGRA format actually yields RGBA —
    // encoding that as BGRA swaps red/blue (a blue cover came out brown).
    // `as_rgba_bytes()` accounts for the format + reverse flag authoritatively,
    // so colors are always correct.
    let raw = bitmap.as_rgba_bytes();

    // The buffer is normally tightly packed (stride == w*4), but strip any row
    // padding defensively so we read exact rows.
    let row = w * 4;
    let expected = row * h;
    let rgba = if raw.len() == expected {
        raw
    } else {
        if h == 0 || raw.len() / h < row {
            return Err(RenderError::Render(format!(
                "short bitmap buffer: {} bytes for {w}x{h}",
                raw.len()
            )));
        }
        let stride = raw.len() / h;
        let mut packed = Vec::with_capacity(expected);
        for y in 0..h {
            let start = y * stride;
            packed.extend_from_slice(&raw[start..start + row]);
        }
        packed
    };

    // Composite over white and drop alpha. pdfium leaves unpainted areas
    // transparent (straight alpha); JPEG has no alpha, so without this a PDF
    // whose page background isn't explicitly painted would get black margins.
    let mut rgb: Vec<u8> = Vec::with_capacity(w * h * 3);
    for px in rgba.chunks_exact(4) {
        let a = px[3] as u32;
        let over_white = |c: u8| (((c as u32) * a + 255 * (255 - a)) / 255) as u8;
        rgb.push(over_white(px[0]));
        rgb.push(over_white(px[1]));
        rgb.push(over_white(px[2]));
    }

    let mut out: Vec<u8> = Vec::with_capacity(128 * 1024);
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder
        .encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| RenderError::Encode(e.to_string()))?;
    Ok(out)
}

/// wasm stub: the browser/boko.html cover path needs pdfium.wasm loaded via JS
/// glue (a documented P2 follow-up). Until then wasm produces a cover-less KFX.
#[cfg(target_arch = "wasm32")]
pub fn render_pdf_page_jpeg(
    _pdf_bytes: &[u8],
    _page_index: usize,
    _target_width_px: u32,
    _quality: u8,
) -> Result<Vec<u8>, RenderError> {
    Err(RenderError::Unavailable(
        "wasm pdfium (pdfium.wasm) not yet wired — see pdf-to-kfx.md".into(),
    ))
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::RenderError;
    use pdfium_render::prelude::*;
    use std::sync::{Mutex, OnceLock};

    // pdfium keeps its bindings in a process-global (`Pdfium::new` sets a global
    // `OnceCell`), so we bind once and reuse. With the `thread_safe` feature
    // `Pdfium` is `Sync`, so it can live in a static; the bindings are
    // mutex-guarded for the Sidle worker's threads.
    static PDFIUM: OnceLock<Pdfium> = OnceLock::new();
    static BIND_LOCK: Mutex<()> = Mutex::new(());

    pub(super) fn pdfium() -> Result<&'static Pdfium, RenderError> {
        if let Some(p) = PDFIUM.get() {
            return Ok(p);
        }
        // Bind under a lock so `Pdfium::new` (which sets pdfium's process-global
        // bindings, and panics if set twice) runs at most once. We cache only
        // SUCCESS — pdfium's global is set only when binding fully succeeds, so a
        // failed attempt leaves it unset and a later call retries. That means
        // staging the dylib after a cold failure works without restarting the
        // process (the dev `cargo run -p sidle` footgun).
        let _guard = BIND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = PDFIUM.get() {
            return Ok(p);
        }
        let pdfium = bind().map_err(RenderError::Unavailable)?;
        let _ = PDFIUM.set(pdfium);
        Ok(PDFIUM.get().expect("just set"))
    }

    /// Bind to `libpdfium`, trying in order: `BOKO_PDFIUM_LIB`, a copy beside the
    /// running executable, then the system library path. Runs once (OnceLock).
    fn bind() -> Result<Pdfium, String> {
        let mut errors = Vec::new();

        if let Ok(path) = std::env::var("BOKO_PDFIUM_LIB") {
            match Pdfium::bind_to_library(&path) {
                Ok(b) => return Ok(Pdfium::new(b)),
                Err(e) => errors.push(format!("BOKO_PDFIUM_LIB={path}: {e}")),
            }
        }

        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            let candidate = Pdfium::pdfium_platform_library_name_at_path(dir);
            match Pdfium::bind_to_library(&candidate) {
                Ok(b) => return Ok(Pdfium::new(b)),
                Err(e) => errors.push(format!("{}: {e}", candidate.display())),
            }
        }

        match Pdfium::bind_to_system_library() {
            Ok(b) => Ok(Pdfium::new(b)),
            Err(e) => {
                errors.push(format!("system: {e}"));
                Err(errors.join("; "))
            }
        }
    }
}
