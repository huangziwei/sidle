//! Cover thumbnails — small color derivatives served to the Kindle picker.

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use image::{ExtendedColorType, ImageEncoder, codecs::jpeg::JpegEncoder, imageops::FilterType};

use super::LibraryPaths;
use super::import::write_bytes_atomic;

/// Thumbnail bounding box, a touch larger than the picker's cell so the device
/// downsamples rather than upscales, and decoupled from the exact cell dims.
pub const THUMB_W: u32 = 400;
pub const THUMB_H: u32 = 520;
/// JPEG quality. 80 keeps a color e-ink thumbnail visually clean while holding
/// the payload to ~30–50KB.
const THUMB_QUALITY: u8 = 80;

/// On-disk thumbnail format generation. Bump whenever [`make_thumbnail`]'s
pub const THUMB_FORMAT_VERSION: u32 = 2;

/// Decode a cover image, downscale to fit [`THUMB_W`]×[`THUMB_H`], and re-encode
/// as a color JPEG. Pure (bytes→bytes), so it's unit-testable without touching
/// the filesystem.
pub fn make_thumbnail(src: &[u8]) -> Result<Vec<u8>> {
    let img = image::load_from_memory(src).context("decode cover")?;
    // `resize` fits *within* the box preserving aspect; `into_rgb8` keeps color
    // for the Colorsoft panel (the grayscale KOA2 collapses to luma at blit).
    let rgb = img
        .resize(THUMB_W, THUMB_H, FilterType::Triangle)
        .into_rgb8();
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, THUMB_QUALITY)
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .context("encode thumbnail jpeg")?;
    Ok(buf)
}

/// Ensure `books/<sha>/cover.thumb.jpg` exists and is no older than the cover
/// it derives from. Returns `Ok(true)` if it (re)generated, `Ok(false)` if the
/// existing thumbnail was already fresh.
pub fn ensure_thumbnail(paths: &LibraryPaths, sha: &str, cover_path: &Path) -> Result<bool> {
    ensure_thumbnail_inner(paths, sha, cover_path, false)
}

/// As [`ensure_thumbnail`], but `force` rebuilds even when the existing
fn ensure_thumbnail_inner(
    paths: &LibraryPaths,
    sha: &str,
    cover_path: &Path,
    force: bool,
) -> Result<bool> {
    let thumb = paths.cover_thumb(sha);
    if !force && is_fresh(&thumb, cover_path) {
        return Ok(false);
    }
    let src = std::fs::read(cover_path)
        .with_context(|| format!("read cover {}", cover_path.display()))?;
    let bytes = make_thumbnail(&src)?;
    write_bytes_atomic(&thumb, &bytes)?;
    Ok(true)
}

/// True when `thumb` exists and its mtime is at or after the cover's, i.e. the
/// cover has not been replaced since. Anything unreadable reads as not fresh.
fn is_fresh(thumb: &Path, cover: &Path) -> bool {
    match (mtime(thumb), mtime(cover)) {
        (Ok(tm), Ok(cm)) => tm >= cm,
        _ => false,
    }
}

fn mtime(p: &Path) -> std::io::Result<SystemTime> {
    std::fs::metadata(p)?.modified()
}

/// Generate any missing or stale thumbnails across the whole library, in a
/// background task at startup. Idempotent and mtime-gated; a format version
/// behind [`THUMB_FORMAT_VERSION`] rebuilds every thumbnail once.
pub fn backfill_thumbnails(paths: &LibraryPaths) -> Result<usize> {
    let conn = super::db::open(&paths.db()).context("open library.db")?;
    let books = super::db::list_books(&conn).context("list books")?;
    let force = format_outdated(paths);
    let mut generated = 0;
    for b in books {
        let Some(cover) = b.cover_path.as_deref() else {
            continue;
        };
        match ensure_thumbnail_inner(paths, &b.sha256, Path::new(cover), force) {
            Ok(true) => generated += 1,
            Ok(false) => {}
            Err(e) => eprintln!("[sidle/thumbnail] book {} ({}): {e:#}", b.id, b.title),
        }
    }
    // Record the format we just produced so the forced rebuild runs only once.
    // Best-effort: a write failure simply re-forces next boot (idempotent).
    if force {
        let marker = paths.cover_thumb_format();
        if let Err(e) = write_bytes_atomic(&marker, THUMB_FORMAT_VERSION.to_string().as_bytes()) {
            eprintln!(
                "[sidle/thumbnail] write format marker {}: {e:#}",
                marker.display()
            );
        }
    }
    Ok(generated)
}

/// True when the library's recorded thumbnail format is behind
/// [`THUMB_FORMAT_VERSION`], so the backfill forces a one-time rebuild.
fn format_outdated(paths: &LibraryPaths) -> bool {
    let recorded = std::fs::read_to_string(paths.cover_thumb_format())
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    recorded < THUMB_FORMAT_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};
    use std::io::Cursor;

    /// A synthetic color cover at the given dimensions, so the test needs no
    /// fixture file.
    fn synth_cover(w: u32, h: u32) -> Vec<u8> {
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([(x % 256) as u8, (y % 256) as u8, 128]);
        }
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Jpeg)
            .unwrap();
        buf
    }

    #[test]
    fn thumbnail_fits_box_preserves_aspect_and_keeps_color() {
        let src = synth_cover(1443, 2048); // a real-world cover shape
        let thumb = make_thumbnail(&src).unwrap();
        let decoded = image::load_from_memory(&thumb).unwrap();

        // Fits within the box, and one dimension hits it exactly (aspect kept).
        assert!(decoded.width() <= THUMB_W && decoded.height() <= THUMB_H);
        assert!(decoded.width() == THUMB_W || decoded.height() == THUMB_H);
        // Portrait cover is height-bound: 520/2048 < 400/1443.
        assert_eq!(decoded.height(), THUMB_H);

        // Color is preserved: the synthetic cover has R≠G≠B regions, so at least
        // one thumbnail pixel must carry real chroma (not collapsed to gray).
        let rgb = decoded.to_rgb8();
        assert!(
            rgb.pixels().any(|p| p[0] != p[1] || p[1] != p[2]),
            "thumbnail should retain color, not collapse to grayscale"
        );

        // And a genuine shrink in bytes.
        assert!(
            thumb.len() < src.len(),
            "thumb {} >= src {}",
            thumb.len(),
            src.len()
        );
    }

    #[test]
    fn make_thumbnail_rejects_non_image_bytes() {
        assert!(make_thumbnail(b"this is not an image").is_err());
    }
}
