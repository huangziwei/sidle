//! SVG rasterization for KFX bundling (EPUB→KFX).

#[cfg(feature = "svg")]
use std::sync::{Arc, OnceLock};

#[cfg(feature = "svg")]
use resvg::tiny_skia;
#[cfg(feature = "svg")]
use resvg::usvg;

/// Upper bound for either raster dimension. Amazon's pipeline keeps non-HDV
/// image resources — which JXR always is — within 1920px; kfxlib flags
/// larger JXR resources as out-of-spec ("HDV dimensions" warning).
#[cfg(feature = "svg")]
const MAX_DIM: f32 = 1920.0;

/// Supersample factor over the SVG's intrinsic CSS-px size. E-ink Kindles
/// are ~300 ppi vs CSS's 96 (~3.1× physical); round up for zoom headroom.
#[cfg(feature = "svg")]
const SCALE: f32 = 4.0;

/// Cache the system-font scan. `load_system_fonts()` walks every
#[cfg(feature = "svg")]
pub fn cached_fontdb() -> Arc<usvg::fontdb::Database> {
    static FONTDB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTDB
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone()
}

/// Quick sniff: do these bytes look like an SVG document? Checks for an
pub(crate) fn looks_like_svg(data: &[u8]) -> bool {
    let head = &data[..data.len().min(1024)];
    memchr::memmem::find(head, b"<svg").is_some()
}

/// Rasterize an SVG to an opaque RGB image, flattened over white.
#[cfg(feature = "svg")]
pub(crate) fn rasterize(data: &[u8]) -> Option<image::DynamicImage> {
    if !looks_like_svg(data) {
        return None;
    }
    let opts = usvg::Options {
        fontdb: cached_fontdb(),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(data, &opts).ok()?;
    let size = tree.size(); // usvg guarantees finite, > 0
    let scale = SCALE.min(MAX_DIM / size.width().max(size.height()));
    let w = (size.width() * scale).round().max(1.0) as u32;
    let h = (size.height() * scale).round().max(1.0) as u32;

    let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
    pixmap.fill(tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // Source-over onto an opaque white base leaves alpha at 255 everywhere,
    // so tiny-skia's premultiplied RGBA equals straight RGBA here.
    let rgba = image::RgbaImage::from_raw(w, h, pixmap.take())?;
    Some(image::DynamicImage::ImageRgb8(
        image::DynamicImage::ImageRgba8(rgba).to_rgb8(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10×10 SVG, left half black, transparent background.
    const HALF_BLACK: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10" viewBox="0 0 10 10"><rect x="0" y="0" width="5" height="10" fill="black"/></svg>"#;

    #[test]
    fn sniff_accepts_svg_rejects_raster() {
        assert!(looks_like_svg(HALF_BLACK.as_bytes()));
        assert!(looks_like_svg(
            "<?xml version='1.0'?>\n<!-- c -->\n<svg/>".as_bytes()
        ));
        assert!(!looks_like_svg(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0]));
        assert!(!looks_like_svg(b""));
    }

    #[cfg(feature = "svg")]
    #[test]
    fn rasterize_supersamples_and_flattens_white() {
        let img = rasterize(HALF_BLACK.as_bytes()).expect("svg rasterizes");
        // 10 CSS px × SCALE(4) = 40 device px.
        assert_eq!((img.width(), img.height()), (40, 40));
        let rgb = img.to_rgb8();
        // Transparent background → white, not black.
        assert_eq!(rgb.get_pixel(30, 20).0, [255, 255, 255]);
        // Painted rect stays black.
        assert_eq!(rgb.get_pixel(10, 20).0, [0, 0, 0]);
    }

    #[cfg(feature = "svg")]
    #[test]
    fn rasterize_caps_huge_intrinsic_size() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="4000" height="2000"><rect width="4000" height="2000" fill="red"/></svg>"#;
        let img = rasterize(svg.as_bytes()).expect("svg rasterizes");
        assert_eq!(img.width(), 1920);
        assert_eq!(img.height(), 960);
    }

    #[cfg(feature = "svg")]
    #[test]
    fn rasterize_rejects_non_svg() {
        assert!(rasterize(b"GIF89a not svg").is_none());
        assert!(rasterize(b"<svg garbage <<<").is_none());
    }
}
