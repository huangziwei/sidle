//! Cover thumbnails — small grayscale derivatives served to the Kindle picker.
//!
//! The native picker renders covers in a 360×440 *grayscale* cell, but the
//! library stores full-resolution *color* source art (median ~520KB, up to
//! ~1MB, ~1443×2048px). Shipping those over the Kindle's slow 2.4GHz radio
//! and decoding them on its armv7l CPU was the ~1s/cover the picker felt.
//!
//! A thumbnail is a derived library asset, produced once when a book is
//! imported (and whenever its cover is replaced) — never per request. The LAN
//! server stays a dumb file-server: it just serves `cover.thumb.jpg` when the
//! picker asks for `?thumb=1`, and falls back to the full-res cover if the
//! thumbnail isn't there yet.
//!
//! Spec: grayscale JPEG, downscaled to fit within [`THUMB_W`]×[`THUMB_H`]
//! preserving aspect, quality [`THUMB_QUALITY`]. ~20KB — a ~25× shrink that
//! also makes the on-device decode trivial.

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use image::{ExtendedColorType, ImageEncoder, codecs::jpeg::JpegEncoder, imageops::FilterType};

use super::LibraryPaths;
use super::import::write_bytes_atomic;

/// Thumbnail bounding box. A touch larger than the picker's 360×440 cell so
/// the device downsamples (sharper) rather than upscales, and deliberately
/// decoupled from the exact cell dims so a cell-size tweak doesn't invalidate
/// every thumbnail.
pub const THUMB_W: u32 = 400;
pub const THUMB_H: u32 = 520;
/// JPEG quality. 80 is visually lossless for a grayscale e-ink thumbnail and
/// keeps the payload around 20KB.
const THUMB_QUALITY: u8 = 80;

/// Decode a cover image, downscale to fit [`THUMB_W`]×[`THUMB_H`], drop chroma,
/// and re-encode as a grayscale JPEG. Pure (bytes→bytes), so it's unit-testable
/// without touching the filesystem.
pub fn make_thumbnail(src: &[u8]) -> Result<Vec<u8>> {
    let img = image::load_from_memory(src).context("decode cover")?;
    // `resize` fits *within* the box preserving aspect; `into_luma8` drops the
    // chroma channels — the device is grayscale, so color is wasted bytes.
    let luma = img
        .resize(THUMB_W, THUMB_H, FilterType::Triangle)
        .into_luma8();
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, THUMB_QUALITY)
        .write_image(
            luma.as_raw(),
            luma.width(),
            luma.height(),
            ExtendedColorType::L8,
        )
        .context("encode thumbnail jpeg")?;
    Ok(buf)
}

/// Ensure `books/<sha>/cover.thumb.jpg` exists and is no older than the cover
/// it derives from. Returns `Ok(true)` if it (re)generated, `Ok(false)` if the
/// existing thumbnail was already fresh.
///
/// Best-effort by convention: the import / cover-replace call sites `let _ =`
/// the result — a thumbnail failure must never fail an import or a cover swap
/// (the full-res cover still works, and the server falls back to serving it).
/// It returns `Result` only so the boot backfill can log which book tripped.
pub fn ensure_thumbnail(paths: &LibraryPaths, sha: &str, cover_path: &Path) -> Result<bool> {
    let thumb = paths.cover_thumb(sha);
    if is_fresh(&thumb, cover_path) {
        return Ok(false);
    }
    let src = std::fs::read(cover_path)
        .with_context(|| format!("read cover {}", cover_path.display()))?;
    let bytes = make_thumbnail(&src)?;
    write_bytes_atomic(&thumb, &bytes)?;
    Ok(true)
}

/// True when `thumb` exists and its mtime is at or after the cover's — i.e. the
/// cover hasn't been replaced since the thumbnail was built. A recrawl /
/// set-cover rewrites the cover (bumping its mtime), which makes this false and
/// triggers a rebuild. A missing thumb (or any unreadable mtime) reads as not
/// fresh, so we (re)build rather than risk serving nothing.
fn is_fresh(thumb: &Path, cover: &Path) -> bool {
    match (mtime(thumb), mtime(cover)) {
        (Ok(tm), Ok(cm)) => tm >= cm,
        _ => false,
    }
}

fn mtime(p: &Path) -> std::io::Result<SystemTime> {
    std::fs::metadata(p)?.modified()
}

/// Generate any missing or stale thumbnails across the whole library. Run in a
/// background task at server startup so books imported before this feature
/// shipped get thumbnails without a manual step. Idempotent and mtime-gated, so
/// it's a near-instant no-op once warm. Returns the count (re)generated.
pub fn backfill_thumbnails(paths: &LibraryPaths) -> Result<usize> {
    let conn = super::db::open(&paths.db()).context("open library.db")?;
    let books = super::db::list_books(&conn).context("list books")?;
    let mut generated = 0;
    for b in books {
        let Some(cover) = b.cover_path.as_deref() else {
            continue;
        };
        match ensure_thumbnail(paths, &b.sha256, Path::new(cover)) {
            Ok(true) => generated += 1,
            Ok(false) => {}
            Err(e) => eprintln!("[sidle/thumbnail] book {} ({}): {e:#}", b.id, b.title),
        }
    }
    Ok(generated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};
    use std::io::Cursor;

    /// A synthetic color cover at the given dimensions — keeps the test off any
    /// gitignored fixture under `books/`.
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
    fn thumbnail_fits_box_preserves_aspect_and_is_grayscale() {
        let src = synth_cover(1443, 2048); // a real-world cover shape
        let thumb = make_thumbnail(&src).unwrap();
        let decoded = image::load_from_memory(&thumb).unwrap();

        // Fits within the box, and one dimension hits it exactly (aspect kept).
        assert!(decoded.width() <= THUMB_W && decoded.height() <= THUMB_H);
        assert!(decoded.width() == THUMB_W || decoded.height() == THUMB_H);
        // Portrait cover is height-bound: 520/2048 < 400/1443.
        assert_eq!(decoded.height(), THUMB_H);

        // Grayscale: every pixel has R == G == B.
        let rgb = decoded.to_rgb8();
        assert!(rgb.pixels().all(|p| p[0] == p[1] && p[1] == p[2]));

        // And a genuine shrink in bytes.
        assert!(thumb.len() < src.len(), "thumb {} >= src {}", thumb.len(), src.len());
    }

    #[test]
    fn make_thumbnail_rejects_non_image_bytes() {
        assert!(make_thumbnail(b"this is not an image").is_err());
    }
}
