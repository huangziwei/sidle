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

pub const COLS: usize = 3;
pub const CELL_W: u32 = 360;
pub const CELL_H: u32 = 440;
pub const COL_GAP: u32 = 32;
pub const ROW_GAP: u32 = 20;

/// Decode a JPEG/PNG byte buffer and resize to fit inside `CELL_W × CELL_H`,
/// preserving aspect. Returns the resized grayscale image.
pub fn decode_resize(bytes: &[u8]) -> Result<DynamicImage> {
    let img = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .decode()?;
    Ok(img.resize(CELL_W, CELL_H, FilterType::Triangle))
}

/// Center-align `(cell_x, cell_y)` of `(CELL_W, CELL_H)` to fit the resized
/// image, then blit the image's luma channel to fb.
pub fn blit_cell(fb: &mut Framebuffer, cell_x: i32, cell_y: i32, img: &DynamicImage) {
    let gray = img.to_luma8();
    let iw = gray.width() as i32;
    let ih = gray.height() as i32;
    let x = cell_x + (CELL_W as i32 - iw) / 2;
    let y = cell_y + (CELL_H as i32 - ih) / 2;
    blit_luma(fb, x, y, gray.width(), gray.height(), gray.as_raw());
}

/// Solid-color cell, used as a placeholder when a cover fails to load.
pub fn blit_placeholder(fb: &mut Framebuffer, cell_x: i32, cell_y: i32, shade: u8) {
    if cell_x < 0 || cell_y < 0 {
        return;
    }
    fb.fill_rect(cell_y as u32, cell_x as u32, CELL_W, CELL_H, shade);
}

/// Frame the selected cell with a black border so the user knows which is
/// armed for download (M7). 4-pixel border, inset by `BORDER_PAD` so it
/// doesn't visually touch the cover edge.
pub fn outline_cell(fb: &mut Framebuffer, cell_x: i32, cell_y: i32, selected: bool) {
    if cell_x < 0 || cell_y < 0 {
        return;
    }
    let shade = if selected { 0x00 } else { 0xFF };
    let thickness = 6u32;
    let x = cell_x as u32;
    let y = cell_y as u32;
    // top + bottom
    fb.fill_rect(y, x, CELL_W, thickness, shade);
    fb.fill_rect(y + CELL_H - thickness, x, CELL_W, thickness, shade);
    // left + right
    fb.fill_rect(y, x, thickness, CELL_H, shade);
    fb.fill_rect(y, x + CELL_W - thickness, thickness, CELL_H, shade);
}

fn blit_luma(fb: &mut Framebuffer, x: i32, y: i32, w: u32, h: u32, raw: &[u8]) {
    if w == 0 || h == 0 {
        return;
    }
    let line_length = fb.fix.line_length as usize;
    let bpp = (fb.var.bits_per_pixel / 8).max(1) as usize;
    let fb_w = fb.var.xres as i32;
    let fb_h = fb.var.yres as i32;
    let pixels = fb.pixels_mut();
    let wu = w as usize;
    for row in 0..h as i32 {
        let py = y + row;
        if py < 0 || py >= fb_h {
            continue;
        }
        let src_row_base = row as usize * wu;
        let row_base = py as usize * line_length;
        for col in 0..w as i32 {
            let px = x + col;
            if px < 0 || px >= fb_w {
                continue;
            }
            let idx = row_base + px as usize * bpp;
            if idx < pixels.len() {
                pixels[idx] = raw[src_row_base + col as usize];
            }
        }
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
