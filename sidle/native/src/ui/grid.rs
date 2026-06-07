//! Cover grid: layout + cell hit-test + image blit.
//!
//! 3×N grid centered on the panel, each cell is a fixed `CELL_W × CELL_H`
//! bounding box. Covers fit inside via aspect-preserving resize (image
//! crate's `Triangle` filter — bilinear, fast enough on armv7l, and we
//! don't need Lanczos-quality on a 16-shade eink panel). Missing covers
//! get a placeholder rect with the title text.

use anyhow::Result;
use image::{DynamicImage, ImageReader, imageops::FilterType};
use std::io::Cursor;

use crate::eink::fb::Framebuffer;
use crate::ui::text::TextRenderer;

pub const COLS: usize = 3;
pub const CELL_W: u32 = 360;
pub const CELL_H: u32 = 440;
pub const COL_GAP: u32 = 32;
pub const ROW_GAP: u32 = 20;

// ---- Series-collection tile geometry (see `draw_series_cell`) ----
/// Bottom band of a series tile, reserved for the series name (book covers
/// don't draw titles, but a collection must — its art is just the lead cover).
pub const NAME_BAND_H: u32 = 64;
/// Top strip above the lead cover, holding the two stacked book-edge bars.
const BAR_STRIP_H: u32 = 22;
/// Thickness of each book-edge bar.
const BAR_H: u32 = 6;
/// Inset of the count badge from the lead cover's bottom-left corner.
const BADGE_MARGIN: u32 = 8;
/// Padding inside the count badge around its number.
const BADGE_PAD: u32 = 12;

/// Decode a JPEG/PNG byte buffer and resize to fit inside `CELL_W × CELL_H`,
/// preserving aspect. Returns the resized grayscale image.
pub fn decode_resize(bytes: &[u8]) -> Result<DynamicImage> {
    let img = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .decode()?;
    Ok(img.resize(CELL_W, CELL_H, FilterType::Triangle))
}

/// Blit a cover into the full cell box, aspect-fit + centered (the common
/// case). Thin wrapper over [`blit_fit`].
pub fn blit_cell(fb: &mut Framebuffer, cell_x: i32, cell_y: i32, img: &DynamicImage) {
    blit_fit(fb, cell_x, cell_y, CELL_W, CELL_H, img);
}

/// The aspect-fit placement of an `iw × ih` image inside the box — the rect
/// [`blit_fit`] actually paints. Never upscales (`scale` clamped to ≤ 1.0).
/// Returns `(ox, oy, dw, dh)`; lets callers position chrome (the series tile's
/// stack bars + count badge) against the displayed cover, not the letterbox box.
pub fn fit_rect(box_x: i32, box_y: i32, box_w: u32, box_h: u32, iw: u32, ih: u32) -> (i32, i32, u32, u32) {
    if iw == 0 || ih == 0 || box_w == 0 || box_h == 0 {
        return (box_x, box_y, 0, 0);
    }
    let scale = (box_w as f32 / iw as f32)
        .min(box_h as f32 / ih as f32)
        .min(1.0);
    let dw = ((iw as f32 * scale).round() as u32).max(1);
    let dh = ((ih as f32 * scale).round() as u32).max(1);
    let ox = box_x + (box_w as i32 - dw as i32) / 2;
    let oy = box_y + (box_h as i32 - dh as i32) / 2;
    (ox, oy, dw, dh)
}

/// Aspect-fit `img` into the box `(box_x, box_y, box_w × box_h)`, centered, and
/// blit its luma channel (placement from [`fit_rect`]). Returns the painted
/// rect. A cover already resized to ≤ a cell by [`decode_resize`] is a 1:1
/// centered copy — the original `blit_cell` behavior — while a smaller box gets
/// a nearest-neighbor downscale (cheap, fine on a 16-shade panel; no extra
/// `image::resize` allocation per repaint).
pub fn blit_fit(fb: &mut Framebuffer, box_x: i32, box_y: i32, box_w: u32, box_h: u32, img: &DynamicImage) -> (i32, i32, u32, u32) {
    let gray = img.to_luma8();
    let (iw, ih) = (gray.width(), gray.height());
    let rect = fit_rect(box_x, box_y, box_w, box_h, iw, ih);
    let (ox, oy, dw, dh) = rect;
    if dw == 0 || dh == 0 {
        return rect;
    }
    let scale = dw as f32 / iw as f32;
    let raw = gray.as_raw();
    for dy in 0..dh {
        let sy = ((dy as f32 / scale) as u32).min(ih - 1);
        let src_row = (sy * iw) as usize;
        for dx in 0..dw {
            let sx = ((dx as f32 / scale) as u32).min(iw - 1);
            fb.put_pixel(ox + dx as i32, oy + dy as i32, raw[src_row + sx as usize]);
        }
    }
    rect
}

/// Solid-color cell, used as a placeholder when a cover fails to load.
pub fn blit_placeholder(fb: &mut Framebuffer, cell_x: i32, cell_y: i32, shade: u8) {
    if cell_x < 0 || cell_y < 0 {
        return;
    }
    fb.fill_rect(cell_y as u32, cell_x as u32, CELL_W, CELL_H, shade);
}

/// Frame the selected cell with a black border so the user knows which is
/// armed (download for a book, drill-in for a series). 6px border.
pub fn outline_cell(fb: &mut Framebuffer, cell_x: i32, cell_y: i32, selected: bool) {
    let shade = if selected { 0x00 } else { 0xFF };
    outline_rect(fb, cell_x, cell_y, CELL_W, CELL_H, 6, shade);
}

/// Draw a `thickness`-px outline rectangle (the four edges of `w × h` at
/// `(x, y)`) in `shade`. Used by [`outline_cell`] for the selection frame.
/// Negative origin / zero size no-ops.
pub fn outline_rect(fb: &mut Framebuffer, x: i32, y: i32, w: u32, h: u32, thickness: u32, shade: u8) {
    if x < 0 || y < 0 || w == 0 || h == 0 {
        return;
    }
    let (xu, yu) = (x as u32, y as u32);
    let t = thickness.min(w).min(h);
    fb.fill_rect(yu, xu, w, t, shade); // top
    fb.fill_rect(yu + h - t, xu, w, t, shade); // bottom
    fb.fill_rect(yu, xu, t, h, shade); // left
    fb.fill_rect(yu, xu + w - t, t, h, shade); // right
}

/// Render a series-collection tile into the cell at `(cell_x, cell_y)`:
///
/// - the lead cover, aspect-fit **full-width** (edge-to-edge left/right, no
///   inset or frame) exactly like a standalone book cover — only shorter, to
///   leave room for the bars above and the name band below;
/// - two **book-edge bars** stacked just above the cover (narrower as they
///   recede, lighter the further back) — a "stack of volumes" hint;
/// - a solid dark **count badge** (light number) at the cover's bottom-left
///   = available-to-download members;
/// - a bottom **name band** with the series name (single line, ellipsized).
///
/// Self-contained (clears its own cell first) so it's reused both for the
/// placeholder paint and the per-cover refresh in `main.rs`.
pub fn draw_series_cell(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    cell_x: i32,
    cell_y: i32,
    cover: Option<&DynamicImage>,
    count: usize,
    name: &str,
) {
    if cell_x < 0 || cell_y < 0 {
        return;
    }
    fb.fill_rect(cell_y as u32, cell_x as u32, CELL_W, CELL_H, 0xFF);

    // Cover region: the full cell width (edge-to-edge like a standalone book),
    // between the top bar strip and the bottom name band.
    let region_y = cell_y + BAR_STRIP_H as i32;
    let region_h = CELL_H - NAME_BAND_H - BAR_STRIP_H;

    // Lead cover, aspect-fit into the region just like a standalone cover — no
    // inset card, no frame. `cov_*` is the actually-painted rect, so the bars
    // and badge track the cover rather than the letterbox margins. A missing
    // cover falls back to a light fill spanning the region width.
    let (cov_x, cov_y, cov_w, cov_h) = match cover {
        Some(img) => blit_fit(fb, cell_x, region_y, CELL_W, region_h, img),
        None => {
            fb.fill_rect(region_y as u32, cell_x as u32, CELL_W, region_h, 0xDD);
            (cell_x, region_y, CELL_W, region_h)
        }
    };

    // Stack hint: two book-edge bars centered above the cover, the nearer (lower,
    // wider) bar darker than the farther (higher, narrower) one.
    let cx = cov_x + cov_w as i32 / 2;
    let bar_lo_w = cov_w * 86 / 100;
    let bar_hi_w = cov_w * 66 / 100;
    fb.fill_rect(
        (cov_y - (BAR_H as i32 + 4)).max(cell_y) as u32,
        (cx - bar_lo_w as i32 / 2).max(cell_x) as u32,
        bar_lo_w,
        BAR_H,
        0x66,
    );
    fb.fill_rect(
        (cov_y - (BAR_H as i32 * 2 + 6)).max(cell_y) as u32,
        (cx - bar_hi_w as i32 / 2).max(cell_x) as u32,
        bar_hi_w,
        BAR_H,
        0x99,
    );

    // Count badge: solid black rect + white (inverted) number, bottom-left of the
    // cover. Sized to the number so 1- and 2-digit counts both fit.
    let badge_text = count.to_string();
    let lh = renderer.line_height().max(1);
    let tw = renderer.measure_width(&badge_text);
    let badge_w = tw + BADGE_PAD * 2;
    let badge_h = lh + BADGE_PAD;
    let badge_x = cov_x + BADGE_MARGIN as i32;
    let badge_y = cov_y + cov_h as i32 - badge_h as i32 - BADGE_MARGIN as i32;
    fb.fill_rect(badge_y.max(cell_y) as u32, badge_x.max(cell_x) as u32, badge_w, badge_h, 0x00);
    let text_x = badge_x + ((badge_w as i32 - tw as i32) / 2).max(0);
    let text_baseline = badge_y + (badge_h * 70 / 100) as i32;
    renderer.draw(fb, text_x, text_baseline, &badge_text, true);

    // Name band: a 2px separator then the series name, centered and clamped to
    // one ellipsized line so a long name can't overrun the cell.
    let band_top = cell_y as u32 + (CELL_H - NAME_BAND_H);
    fb.fill_rect(band_top, cell_x as u32, CELL_W, 2, 0x00);
    const PAD: u32 = 16;
    let lines = renderer.wrap_and_clamp(name, CELL_W.saturating_sub(PAD * 2), 1);
    if let Some(line) = lines.first() {
        let lw = renderer.measure_width(line);
        let lx = cell_x + ((CELL_W as i32 - lw as i32) / 2).max(0);
        let baseline = band_top as i32 + (NAME_BAND_H * 62 / 100) as i32;
        renderer.draw(fb, lx, baseline, line, false);
    }
}

/// Grid origin computed to center the grid on the panel.
pub fn grid_origin(fb_xres: u32, top_margin: u32) -> (i32, i32) {
    let grid_w = COLS as u32 * CELL_W + (COLS as u32 - 1) * COL_GAP;
    let left = ((fb_xres as i32) - grid_w as i32) / 2;
    (left, top_margin as i32)
}

pub fn cell_xy(grid_left: i32, grid_top: i32, idx: usize) -> (i32, i32) {
    let col = idx % COLS;
    let row = idx / COLS;
    let x = grid_left + col as i32 * (CELL_W + COL_GAP) as i32;
    let y = grid_top + row as i32 * (CELL_H + ROW_GAP) as i32;
    (x, y)
}

pub fn cell_at_tap(
    tx: u32,
    ty: u32,
    grid_left: i32,
    grid_top: i32,
    n_books: usize,
) -> Option<usize> {
    if (tx as i32) < grid_left || (ty as i32) < grid_top {
        return None;
    }
    let local_x = (tx as i32 - grid_left) as u32;
    let local_y = (ty as i32 - grid_top) as u32;
    let stride_x = CELL_W + COL_GAP;
    let stride_y = CELL_H + ROW_GAP;
    let col = (local_x / stride_x) as usize;
    let row = (local_y / stride_y) as usize;
    if col >= COLS {
        return None;
    }
    // Reject taps that land in the gap between cells (improves accuracy
    // — otherwise a tap right between two covers picks the left one).
    if local_x % stride_x >= CELL_W {
        return None;
    }
    if local_y % stride_y >= CELL_H {
        return None;
    }
    let idx = row * COLS + col;
    if idx < n_books { Some(idx) } else { None }
}
