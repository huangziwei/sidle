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
/// preserving aspect. Returns the resized image in its source color (the cover
/// thumbnail is a color JPEG; [`blit_fit`] samples its RGB).
pub fn decode_resize(bytes: &[u8]) -> Result<DynamicImage> {
    let img = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .decode()?;
    Ok(img.resize(CELL_W, CELL_H, FilterType::Triangle))
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
/// blit its RGB (placement from [`fit_rect`]). Returns the painted rect. A cover
/// already resized to ≤ a cell by [`decode_resize`] is copied 1:1 and centered,
/// while a smaller box (a tile's cover region) gets a nearest-neighbor downscale
/// (cheap, fine on the panel; no extra `image::resize` allocation per repaint).
/// Color reaches the Colorsoft; the grayscale KOA2 collapses it to luma in
/// `send_update`.
pub fn blit_fit(fb: &mut Framebuffer, box_x: i32, box_y: i32, box_w: u32, box_h: u32, img: &DynamicImage) -> (i32, i32, u32, u32) {
    let rgb = img.to_rgb8();
    let (iw, ih) = (rgb.width(), rgb.height());
    let rect = fit_rect(box_x, box_y, box_w, box_h, iw, ih);
    let (ox, oy, dw, dh) = rect;
    if dw == 0 || dh == 0 {
        return rect;
    }
    let scale = dw as f32 / iw as f32;
    let raw = rgb.as_raw();
    for dy in 0..dh {
        let sy = ((dy as f32 / scale) as u32).min(ih - 1);
        let src_row = (sy * iw) as usize;
        for dx in 0..dw {
            let sx = ((dx as f32 / scale) as u32).min(iw - 1);
            let p = (src_row + sx as usize) * 3;
            fb.put_pixel_rgb(ox + dx as i32, oy + dy as i32, [raw[p], raw[p + 1], raw[p + 2]]);
        }
    }
    rect
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

/// Clear the cell to white, aspect-fit the cover into the region between
/// `top_inset` and the bottom name band, then draw that band with `label`
/// (single line, centered, ellipsized). Returns the painted cover rect so a
/// caller can overlay chrome on it. Shared by [`draw_book_cell`] and
/// [`draw_series_cell`]: `top_inset` is 0 for a standalone book (the cover uses
/// the full height above the band) and `BAR_STRIP_H` for a series (leaving room
/// for the stack bars). Off-screen cells no-op with a zero-size rect.
fn draw_cover_tile(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    cell_x: i32,
    cell_y: i32,
    top_inset: u32,
    cover: Option<&DynamicImage>,
    label: &str,
) -> (i32, i32, u32, u32) {
    if cell_x < 0 || cell_y < 0 {
        return (cell_x, cell_y, 0, 0);
    }
    fb.fill_rect(cell_y as u32, cell_x as u32, CELL_W, CELL_H, 0xFF);

    // Cover region: full cell width (edge-to-edge, no inset card or frame),
    // between the optional top inset and the bottom name band. The cover
    // aspect-fits exactly like a standalone book cover.
    let region_y = cell_y + top_inset as i32;
    let region_h = CELL_H - NAME_BAND_H - top_inset;
    let rect = match cover {
        Some(img) => blit_fit(fb, cell_x, region_y, CELL_W, region_h, img),
        None => {
            // No cover yet: a light fill spanning the region width.
            fb.fill_rect(region_y as u32, cell_x as u32, CELL_W, region_h, 0xDD);
            (cell_x, region_y, CELL_W, region_h)
        }
    };

    // Name band: a 2px separator then the label, centered and clamped to one
    // ellipsized line so a long title can't overrun the cell.
    let band_top = cell_y as u32 + (CELL_H - NAME_BAND_H);
    fb.fill_rect(band_top, cell_x as u32, CELL_W, 2, 0x00);
    const PAD: u32 = 16;
    let lines = renderer.wrap_and_clamp(label, CELL_W.saturating_sub(PAD * 2), 1);
    if let Some(line) = lines.first() {
        let lw = renderer.measure_width(line);
        let lx = cell_x + ((CELL_W as i32 - lw as i32) / 2).max(0);
        let baseline = band_top as i32 + (NAME_BAND_H * 62 / 100) as i32;
        renderer.draw(fb, lx, baseline, line, false);
    }
    rect
}

/// Render a standalone book tile: the cover (aspect-fit, full-width, no frame)
/// above a name band carrying the book title — the same layout as a series tile
/// minus the stack bars and count badge, so books and collections line up in
/// the grid. A missing cover falls back to a light placeholder + the title.
/// Self-contained (clears its own cell) for both the initial paint and the
/// per-cover refresh in `main.rs`.
pub fn draw_book_cell(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    cell_x: i32,
    cell_y: i32,
    cover: Option<&DynamicImage>,
    title: &str,
) {
    draw_cover_tile(fb, renderer, cell_x, cell_y, 0, cover, title);
}

/// Render a series-collection tile: the shared cover tile (see
/// [`draw_cover_tile`]) with the series name in the band, plus two **book-edge
/// bars** stacked just above the cover (narrower as they recede, lighter the
/// further back — a "stack of volumes" hint) and a solid dark **count badge**
/// (light number = available-to-download members) at the cover's bottom-left.
/// Self-contained for both the placeholder paint and the per-cover refresh.
pub fn draw_series_cell(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    cell_x: i32,
    cell_y: i32,
    cover: Option<&DynamicImage>,
    count: usize,
    name: &str,
) {
    // Series reserve BAR_STRIP_H above the cover for the stack bars; the cover
    // is otherwise identical to a book's, so the two line up in the grid.
    let (cov_x, cov_y, cov_w, cov_h) =
        draw_cover_tile(fb, renderer, cell_x, cell_y, BAR_STRIP_H, cover, name);
    if cov_w == 0 {
        return; // off-screen cell
    }

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
