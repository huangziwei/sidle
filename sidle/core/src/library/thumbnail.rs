//! Cover thumbnails — small color derivatives served to the Kindle picker.
//!
//! The library stores full-resolution source art (median ~520KB, up to ~1MB,
//! ~1443×2048px). Shipping those over the Kindle's slow 2.4GHz radio and
//! decoding them on its armv7l CPU was the ~1s/cover the picker felt.
//!
//! A thumbnail is a derived library asset, produced once when a book is
//! imported (and whenever its cover is replaced) — never per request. The LAN
//! server stays a dumb file-server: it just serves `cover.thumb.jpg` when the
//! picker asks for `?thumb=1`, and falls back to the full-res cover if the
//! thumbnail isn't there yet.
//!
//! Spec: **color** JPEG, downscaled to fit within [`THUMB_W`]×[`THUMB_H`]
//! preserving aspect, quality [`THUMB_QUALITY`]. ~30–50KB — still a big shrink
//! that keeps the on-device decode cheap. The thumbnail is color regardless of
//! the target panel: the Colorsoft renders it in color, and the grayscale KOA2
//! collapses it to luma at blit time, so one asset serves both devices.
//!
//! [`THUMB_FORMAT_VERSION`] gates a one-time rebuild: when the output format
//! changes (here, grayscale → color), the boot backfill force-regenerates every
//! existing thumbnail once, since the mtime freshness check can't see a pure
//! format flip (the old thumb is newer than its unchanged cover).

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
/// JPEG quality. 80 keeps a color e-ink thumbnail visually clean while holding
/// the payload to ~30–50KB.
const THUMB_QUALITY: u8 = 80;

/// On-disk thumbnail format generation. Bump whenever [`make_thumbnail`]'s
/// output format changes so the boot backfill rebuilds every existing
/// `cover.thumb.jpg` once (see [`backfill_thumbnails`]). 1 = grayscale (the
/// original); 2 = color RGB.
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
///
/// Best-effort by convention: the import / cover-replace call sites `let _ =`
/// the result — a thumbnail failure must never fail an import or a cover swap
/// (the full-res cover still works, and the server falls back to serving it).
/// It returns `Result` only so the boot backfill can log which book tripped.
pub fn ensure_thumbnail(paths: &LibraryPaths, sha: &str, cover_path: &Path) -> Result<bool> {
    ensure_thumbnail_inner(paths, sha, cover_path, false)
}

/// As [`ensure_thumbnail`], but `force` rebuilds even when the existing
/// thumbnail looks mtime-fresh — used by the backfill to reconvert thumbnails
/// after a format change the mtime check can't detect (see
/// [`THUMB_FORMAT_VERSION`]).
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
///
/// When the on-disk format version is behind [`THUMB_FORMAT_VERSION`] (e.g. the
/// grayscale→color flip), every thumbnail is rebuilt once regardless of mtime,
/// then the version marker is advanced so subsequent boots are warm again.
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
            eprintln!("[sidle/thumbnail] write format marker {}: {e:#}", marker.display());
        }
    }
    Ok(generated)
}

/// True when the library's recorded thumbnail format is older than
/// [`THUMB_FORMAT_VERSION`] (or unrecorded — a pre-marker or fresh library), so
/// the backfill should force a one-time rebuild. An unreadable/garbage marker
/// reads as version 0 (force), which is safe: a rebuild is idempotent.
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
        assert!(thumb.len() < src.len(), "thumb {} >= src {}", thumb.len(), src.len());
    }

    #[test]
    fn make_thumbnail_rejects_non_image_bytes() {
        assert!(make_thumbnail(b"this is not an image").is_err());
    }
}
