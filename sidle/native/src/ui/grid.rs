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
/// Per-card offset for the two "stack" outlines behind the lead cover. Small —
/// just enough to read as a stack on the 16-shade panel, not a fan.
const STACK_OFFSET: i32 = 12;
/// Margin between the cell edge and the front card (cover) of a series tile.
const CARD_INSET: u32 = 28;
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

/// Aspect-fit `img` into the box `(box_x, box_y, box_w × box_h)`, centered, and
/// blit its luma channel. Never upscales (`scale` clamped to ≤ 1.0), so a cover
/// already resized to ≤ a cell by [`decode_resize`] is a 1:1 centered copy —
/// the original `blit_cell` behavior — while a series tile's smaller front-card
/// box gets a nearest-neighbor downscale (cheap, fine on a 16-shade panel; no
/// extra `image::resize` allocation per repaint).
pub fn blit_fit(fb: &mut Framebuffer, box_x: i32, box_y: i32, box_w: u32, box_h: u32, img: &DynamicImage) {
    let gray = img.to_luma8();
    let (iw, ih) = (gray.width(), gray.height());
    if iw == 0 || ih == 0 || box_w == 0 || box_h == 0 {
        return;
    }
    let scale = (box_w as f32 / iw as f32)
        .min(box_h as f32 / ih as f32)
        .min(1.0);
    let dw = ((iw as f32 * scale).round() as u32).max(1);
    let dh = ((ih as f32 * scale).round() as u32).max(1);
    let ox = box_x + (box_w as i32 - dw as i32) / 2;
    let oy = box_y + (box_h as i32 - dh as i32) / 2;
    let raw = gray.as_raw();
    for dy in 0..dh {
        let sy = ((dy as f32 / scale) as u32).min(ih - 1);
        let src_row = (sy * iw) as usize;
        for dx in 0..dw {
            let sx = ((dx as f32 / scale) as u32).min(iw - 1);
            fb.put_pixel(ox + dx as i32, oy + dy as i32, raw[src_row + sx as usize]);
        }
    }
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
/// `(x, y)`) in `shade`. Shared by [`outline_cell`] and the series-tile stack
/// hint. Negative origin / zero size no-ops.
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
/// - the lead cover (or a light placeholder) as the **front card**, framed;
/// - two offset outline rectangles **behind** it peeking bottom-right — a
///   "stack" hint, no alpha needed (gray outlines on the 16-shade panel);
/// - a solid dark **count badge** (light number) at the front card's top-right
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

    // Cover region = the cell above the name band. The front card leaves a
    // CARD_INSET margin and reserves 2×STACK_OFFSET at the bottom-right for the
    // stack outlines to peek out from behind the cover.
    let region_h = CELL_H - NAME_BAND_H;
    let card_x = cell_x + CARD_INSET as i32;
    let card_y = cell_y + CARD_INSET as i32;
    let card_w = CELL_W.saturating_sub(CARD_INSET * 2 + STACK_OFFSET as u32 * 2);
    let card_h = region_h.saturating_sub(CARD_INSET * 2 + STACK_OFFSET as u32 * 2);

    // Stack outlines first (drawn behind: the front card paints over their
    // top-left, leaving the bottom-right edges showing). Lighter the further back.
    outline_rect(fb, card_x + STACK_OFFSET * 2, card_y + STACK_OFFSET * 2, card_w, card_h, 3, 0x99);
    outline_rect(fb, card_x + STACK_OFFSET, card_y + STACK_OFFSET, card_w, card_h, 3, 0x66);

    // Front card: lead cover, or a light fill when it hasn't arrived. Framed so
    // it reads as a card even with a pale or missing cover.
    match cover {
        Some(img) => blit_fit(fb, card_x, card_y, card_w, card_h, img),
        None => fb.fill_rect(card_y as u32, card_x as u32, card_w, card_h, 0xDD),
    }
    outline_rect(fb, card_x, card_y, card_w, card_h, 3, 0x00);

    // Count badge: solid black rect + white (inverted) number, top-right of the
    // front card. Sized to the number so 1- and 2-digit counts both fit.
    let badge_text = count.to_string();
    let lh = renderer.line_height().max(1);
    let tw = renderer.measure_width(&badge_text);
    let badge_w = tw + BADGE_PAD * 2;
    let badge_h = lh + BADGE_PAD;
    let badge_x = (card_x + card_w as i32 - badge_w as i32).max(card_x);
    let badge_y = card_y;
    fb.fill_rect(badge_y as u32, badge_x as u32, badge_w, badge_h, 0x00);
    let text_x = badge_x + ((badge_w as i32 - tw as i32) / 2).max(0);
    let text_baseline = badge_y + (badge_h * 70 / 100) as i32;
    renderer.draw(fb, text_x, text_baseline, &badge_text, true);

    // Name band: a 2px separator then the series name, centered and clamped to
    // one ellipsized line so a long name can't overrun the cell.
    let band_top = cell_y as u32 + region_h;
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
