//! SVG → PDF rendering for notebook export.
//!
//! Scribe notebooks are stored as one cached `<svg>` per page (vector ink, with
//! occasional embedded raster `<image>` for pencil strokes). To export a
//! notebook as a single PDF we rasterize each page with `resvg` and assemble the
//! page images into a multi-page PDF with `lopdf`.
//!
//! Raster, not vector: the only clean vector route (`svg2pdf`) pins `usvg` 0.45
//! against the workspace's 0.47, which would fork the whole SVG stack into the
//! build. Rasterizing reuses the `resvg`/`tiny_skia`/`lopdf` already compiled for
//! the rest of the app and handles every primitive the page SVGs use (nested
//! `<svg>`, transforms, embedded images) for free. We render at a fixed
//! long-edge resolution (~device native) so handwriting stays crisp on screen
//! and in print.

use std::io::Write as _;

use anyhow::{Context, Result, anyhow};
use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};
use resvg::{tiny_skia, usvg};

/// Long edge of each rasterized page, in pixels. ~Kindle Scribe native (1860 ×
/// 2480), so the export is visually lossless without ballooning file size.
const MAX_LONG_EDGE_PX: f32 = 2200.0;

/// Long edge of each PDF page, in points (1/72"). 11" — a Letter-ish page; the
/// short edge follows the page's aspect ratio. The pixel resolution above is
/// independent, giving ~200 DPI of effective image detail.
const PAGE_LONG_PT: f32 = 792.0;

/// Render `svgs` (one document per page, in order) into a single multi-page PDF,
/// returning the serialized bytes. Errors if `svgs` is empty or a page fails to
/// parse / rasterize.
pub fn svgs_to_pdf(svgs: &[String]) -> Result<Vec<u8>> {
    if svgs.is_empty() {
        return Err(anyhow!("nothing to render: no pages"));
    }

    let opt = usvg::Options::default();
    let mut doc = Document::with_version("1.5");
    // Reserve the Pages node id up front so each Page can point its /Parent at it
    // before the Pages dict itself exists.
    let pages_id = doc.new_object_id();
    let mut page_ids: Vec<Object> = Vec::with_capacity(svgs.len());

    for (i, svg) in svgs.iter().enumerate() {
        let (rgb, pw, ph, aspect_w, aspect_h) =
            rasterize(svg, &opt).with_context(|| format!("render page {}", i + 1))?;

        // Flate-compress the raw RGB so a sparse handwriting page (mostly white)
        // stores compactly; the stream is written verbatim under /FlateDecode.
        let compressed = zlib(&rgb)?;
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => pw as i64,
                "Height" => ph as i64,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
                "Filter" => "FlateDecode",
            },
            compressed,
        ));

        // Page box keeps the SVG aspect ratio; the image fills it via the CTM.
        let (page_w, page_h) = if aspect_w >= aspect_h {
            (PAGE_LONG_PT, PAGE_LONG_PT * aspect_h / aspect_w)
        } else {
            (PAGE_LONG_PT * aspect_w / aspect_h, PAGE_LONG_PT)
        };

        let content = Content {
            operations: vec![
                Operation::new("q", vec![]),
                Operation::new(
                    "cm",
                    vec![
                        Object::Real(page_w),
                        Object::Real(0.0),
                        Object::Real(0.0),
                        Object::Real(page_h),
                        Object::Real(0.0),
                        Object::Real(0.0),
                    ],
                ),
                Operation::new("Do", vec![Object::Name(b"Im0".to_vec())]),
                Operation::new("Q", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(
            lopdf::Dictionary::new(),
            content.encode().context("encode page content")?,
        ));

        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(page_w),
                Object::Real(page_h),
            ],
            "Contents" => Object::Reference(content_id),
            "Resources" => dictionary! {
                "XObject" => dictionary! { "Im0" => Object::Reference(img_id) },
            },
        });
        page_ids.push(Object::Reference(page_id));
    }

    let count = page_ids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids,
            "Count" => count,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));

    let mut buf = Vec::new();
    doc.save_to(&mut buf).context("serialize PDF")?;
    Ok(buf)
}

/// Rasterize one SVG document to an opaque RGB buffer over a white page.
/// Returns `(rgb_bytes, width_px, height_px, svg_w, svg_h)` — the last two are
/// the SVG's intrinsic size, for the PDF page's aspect ratio.
fn rasterize(svg: &str, opt: &usvg::Options) -> Result<(Vec<u8>, u32, u32, f32, f32)> {
    let tree = usvg::Tree::from_str(svg, opt).map_err(|e| anyhow!("parse svg: {e}"))?;
    let size = tree.size();
    let (sw, sh) = (size.width(), size.height());
    let long = sw.max(sh).max(1.0);
    let scale = (MAX_LONG_EDGE_PX / long).clamp(0.05, 8.0);
    let pw = ((sw * scale).round() as u32).max(1);
    let ph = ((sh * scale).round() as u32).max(1);

    let mut pixmap =
        tiny_skia::Pixmap::new(pw, ph).ok_or_else(|| anyhow!("alloc pixmap {pw}x{ph}"))?;
    pixmap.fill(tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny_skia stores premultiplied RGBA. Flatten onto the white page (which is
    // also what the gallery shows): straight-over-white of a premultiplied pixel
    // is `channel + (255 - alpha)`. Fully-opaque ink is unchanged.
    let data = pixmap.data();
    let mut rgb = Vec::with_capacity((pw * ph * 3) as usize);
    for px in data.chunks_exact(4) {
        let add = 255 - px[3];
        rgb.push(px[0].saturating_add(add));
        rgb.push(px[1].saturating_add(add));
        rgb.push(px[2].saturating_add(add));
    }
    Ok((rgb, pw, ph, sw, sh))
}

/// zlib-compress (PDF `/FlateDecode` is a zlib stream).
fn zlib(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(bytes).context("flate write")?;
    enc.finish().context("flate finish")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_multipage_pdf() {
        let page = |fill: &str| {
            format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 400 600\">\
                 <rect x=\"0\" y=\"0\" width=\"100%\" height=\"100%\" fill=\"white\"/>\
                 <path d=\"M50 50 L350 550\" stroke=\"{fill}\" stroke-width=\"6\" fill=\"none\"/>\
                 </svg>"
            )
        };
        let svgs = vec![page("black"), page("blue")];
        let pdf = svgs_to_pdf(&svgs).expect("render");
        assert!(pdf.starts_with(b"%PDF-"), "should be a PDF");
        // Re-parse to confirm it's structurally valid and has both pages.
        let doc = Document::load_mem(&pdf).expect("reparse");
        assert_eq!(doc.get_pages().len(), 2);
    }

    #[test]
    fn empty_is_an_error() {
        assert!(svgs_to_pdf(&[]).is_err());
    }
}
