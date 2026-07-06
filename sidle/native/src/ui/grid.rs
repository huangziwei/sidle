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
pub fn fit_rect(
    box_x: i32,
    box_y: i32,
    box_w: u32,
    box_h: u32,
    iw: u32,
    ih: u32,
) -> (i32, i32, u32, u32) {
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
pub fn blit_fit(
    fb: &mut Framebuffer,
    box_x: i32,
    box_y: i32,
    box_w: u32,
    box_h: u32,
    img: &DynamicImage,
) -> (i32, i32, u32, u32) {
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
            fb.put_pixel_rgb(
                ox + dx as i32,
                oy + dy as i32,
                [raw[p], raw[p + 1], raw[p + 2]],
            );
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
pub fn outline_rect(
    fb: &mut Framebuffer,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    thickness: u32,
    shade: u8,
) {
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

/// Stroke a **rounded-rectangle** border (`thickness` px, corner `radius`) in
/// `shade` — the search field's pill/box frame. `radius == h/2` gives a full
/// pill (Amazon-style); a smaller radius gives rounded corners. The straight
/// edges are `fill_rect`s between the four quarter-circle corner arcs.
#[allow(clippy::too_many_arguments)] // positional geometry; a struct just moves the list
pub fn stroke_round_rect(
    fb: &mut Framebuffer,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    radius: u32,
    thickness: u32,
    shade: u8,
) {
    if x < 0 || y < 0 || w == 0 || h == 0 {
        return;
    }
    let (xu, yu) = (x as u32, y as u32);
    let r = radius.min(w / 2).min(h / 2);
    let t = thickness.min(w).min(h).max(1);
    let mid_w = w.saturating_sub(2 * r);
    let mid_h = h.saturating_sub(2 * r);
    if mid_w > 0 {
        fb.fill_rect(yu, xu + r, mid_w, t, shade); // top
        fb.fill_rect(yu + h - t, xu + r, mid_w, t, shade); // bottom
    }
    if mid_h > 0 {
        fb.fill_rect(yu + r, xu, t, mid_h, shade); // left
        fb.fill_rect(yu + r, xu + w - t, t, mid_h, shade); // right
    }
    let (cl, cr) = (x + r as i32, x + w as i32 - 1 - r as i32);
    let (ct, cb) = (y + r as i32, y + h as i32 - 1 - r as i32);
    corner_arc(fb, cl, ct, r, t, shade, -1, -1);
    corner_arc(fb, cr, ct, r, t, shade, 1, -1);
    corner_arc(fb, cl, cb, r, t, shade, -1, 1);
    corner_arc(fb, cr, cb, r, t, shade, 1, 1);
}

/// One quarter-circle arc of the rounded-rect border: pixels in the `r×r` corner
/// box whose distance from the arc center `(cx, cy)` falls in `[r-t, r]`. `(sx,
/// sy) ∈ {-1, 1}` selects the quadrant (which way the arc bulges).
#[allow(clippy::too_many_arguments)] // positional geometry; a struct just moves the list
fn corner_arc(fb: &mut Framebuffer, cx: i32, cy: i32, r: u32, t: u32, shade: u8, sx: i32, sy: i32) {
    let rf = r as f32;
    let inner = r.saturating_sub(t) as f32;
    for dy in 0..=r as i32 {
        for dx in 0..=r as i32 {
            let dist = ((dx * dx + dy * dy) as f32).sqrt();
            if dist >= inner && dist <= rf {
                fb.put_pixel(cx + sx * dx, cy + sy * dy, shade);
            }
        }
    }
}

/// Draw a magnifier glyph (a ring + a lower-right diagonal handle) centered at
/// `(cx, cy)` with lens radius `r`. The font has no 🔍, so we draw it.
pub fn draw_magnifier(fb: &mut Framebuffer, cx: i32, cy: i32, r: u32, shade: u8) {
    const T: u32 = 3;
    let rf = r as f32;
    let inner = r.saturating_sub(T) as f32;
    for dy in -(r as i32)..=r as i32 {
        for dx in -(r as i32)..=r as i32 {
            let dist = ((dx * dx + dy * dy) as f32).sqrt();
            if dist >= inner && dist <= rf {
                fb.put_pixel(cx + dx, cy + dy, shade);
            }
        }
    }
    // Handle: a short thick diagonal off the lens's lower-right.
    let start = rf * 0.78;
    let len = (rf * 0.95) as i32;
    for i in 0..len {
        let d = start + i as f32;
        let (hx, hy) = (cx + (d * 0.707) as i32, cy + (d * 0.707) as i32);
        for by in 0..T as i32 {
            for bx in 0..T as i32 {
                fb.put_pixel(hx + bx, hy + by, shade);
            }
        }
    }
}

/// Draw an `✕` (two diagonals) centered at `(cx, cy)`, half-extent `size` — the
/// search field's clear button.
pub fn draw_x(fb: &mut Framebuffer, cx: i32, cy: i32, size: i32, shade: u8) {
    for i in -size..=size {
        for k in 0..2 {
            fb.put_pixel(cx + i, cy + i + k, shade);
            fb.put_pixel(cx + i, cy - i + k, shade);
        }
    }
}

/// Draw a **sync** glyph — two arced arrows chasing round a circle (the refresh
/// pair) — centered at `(cx, cy)` with ring radius `r`. Two arcs with a gap at
/// each end, each gap capped by a small tangential arrowhead so the ring reads as
/// rotating. The font has no 🔄, so we draw it. Top-bar Sync button.
pub fn draw_sync_glyph(fb: &mut Framebuffer, cx: i32, cy: i32, r: i32, shade: u8) {
    const T: i32 = 5;
    let rf = r as f32;
    let inner = (r - T).max(1) as f32;
    // Ring pixels minus two gaps (screen coords, +y down): near 10° and near
    // 190°, leaving two arcs spanning roughly [38°,162°] and [218°,342°].
    for dy in -r..=r {
        for dx in -r..=r {
            let dist = ((dx * dx + dy * dy) as f32).sqrt();
            if dist < inner || dist > rf {
                continue;
            }
            let mut a = (dy as f32).atan2(dx as f32).to_degrees();
            if a < 0.0 {
                a += 360.0;
            }
            if !(38.0..=162.0).contains(&a) && !(218.0..=342.0).contains(&a) {
                continue;
            }
            fb.put_pixel(cx + dx, cy + dy, shade);
        }
    }
    // Cap each arc's low end with an arrowhead pointing along the clockwise
    // tangent, so the pair reads as rotation rather than a broken ring.
    sync_arrowhead(fb, cx, cy, rf, 38.0, shade);
    sync_arrowhead(fb, cx, cy, rf, 218.0, shade);
}

/// Small filled triangle capping a [`draw_sync_glyph`] arc at angle `deg`,
/// pointing along the clockwise tangent (screen `+y` down).
fn sync_arrowhead(fb: &mut Framebuffer, cx: i32, cy: i32, r: f32, deg: f32, shade: u8) {
    let a = deg.to_radians();
    let (c, s) = (a.cos(), a.sin());
    let (px, py) = (cx as f32 + r * c, cy as f32 + r * s);
    let (tx, ty) = (s, -c); // clockwise tangent
    let tip = (px + tx * 13.0, py + ty * 13.0);
    let b1 = (px + c * 9.0, py + s * 9.0);
    let b2 = (px - c * 9.0, py - s * 9.0);
    fill_tri(fb, tip, b1, b2, shade);
}

/// Fill the triangle `(a, b, c)` (float screen coords) via a barycentric test
/// over its bounding box. Small glyph triangles only — not a general rasterizer.
fn fill_tri(fb: &mut Framebuffer, a: (f32, f32), b: (f32, f32), c: (f32, f32), shade: u8) {
    let area = (b.0 - a.0) * (c.1 - a.1) - (c.0 - a.0) * (b.1 - a.1);
    if area.abs() < 1e-3 {
        return;
    }
    let minx = a.0.min(b.0).min(c.0).floor() as i32;
    let maxx = a.0.max(b.0).max(c.0).ceil() as i32;
    let miny = a.1.min(b.1).min(c.1).floor() as i32;
    let maxy = a.1.max(b.1).max(c.1).ceil() as i32;
    for y in miny..=maxy {
        for x in minx..=maxx {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let w0 = ((b.0 - px) * (c.1 - py) - (c.0 - px) * (b.1 - py)) / area;
            let w1 = ((c.0 - px) * (a.1 - py) - (a.0 - px) * (c.1 - py)) / area;
            if w0 >= 0.0 && w1 >= 0.0 && w0 + w1 <= 1.0 {
                fb.put_pixel(x, y, shade);
            }
        }
    }
}

/// Draw a **download** glyph — a vertical stem, a solid down-arrowhead, and a
/// tray line beneath — centered at `(cx, cy)`, scale `s`. The font has no ⤓, so
/// we draw it. Top-bar Update button (pull the next picker binary over the LAN).
pub fn draw_download_glyph(fb: &mut Framebuffer, cx: i32, cy: i32, s: i32, shade: u8) {
    const T: i32 = 5;
    let stem_h = s + s / 4;
    let head_h = s * 3 / 4;
    let tray_gap = s / 2;
    // Center the whole glyph (stem + head + gap + tray) on `cy`.
    let total = stem_h + head_h + tray_gap + T;
    let stem_top = cy - total / 2;
    let x0 = (cx - T / 2).max(0) as u32;
    fb.fill_rect(stem_top.max(0) as u32, x0, T as u32, stem_h as u32, shade);
    // Arrowhead: a downward filled triangle, flat top at the stem's neck.
    let head_top = stem_top + stem_h;
    for row in 0..=head_h {
        let wpx = ((head_h - row) * 2 + 1) as u32;
        let left = (cx - (head_h - row)).max(0) as u32;
        fb.fill_rect((head_top + row).max(0) as u32, left, wpx, 1, shade);
    }
    // Tray: a short horizontal base under the arrow.
    let base_y = head_top + head_h + tray_gap;
    fb.fill_rect(
        base_y.max(0) as u32,
        (cx - s).max(0) as u32,
        (s * 2) as u32,
        T as u32,
        shade,
    );
}

/// Draw a **key** glyph — a ring bow on the left, a horizontal shaft, and two
/// teeth dropping off its tip — centered at `(cx, cy)`, scale `s`. The font has
/// no 🔑, so we draw it. The DRM view's right-hand action button (decrypt every
/// purchase); the library view draws [`draw_download_glyph`] in that slot.
pub fn draw_key_glyph(fb: &mut Framebuffer, cx: i32, cy: i32, s: i32, shade: u8) {
    const T: i32 = 5;
    // Bow: a ring on the left, a hair inside where the shaft meets it.
    let bow_cx = cx - s / 2;
    let bow_r = s * 3 / 5;
    let inner = (bow_r - T).max(1) as f32;
    let rf = bow_r as f32;
    for dy in -bow_r..=bow_r {
        for dx in -bow_r..=bow_r {
            let dist = ((dx * dx + dy * dy) as f32).sqrt();
            if dist >= inner && dist <= rf {
                fb.put_pixel(bow_cx + dx, cy + dy, shade);
            }
        }
    }
    // Shaft: a horizontal bar from the bow's right edge to the key's tip.
    let shaft_x0 = bow_cx + bow_r - T / 2;
    let tip_x = cx + s;
    let shaft_w = (tip_x - shaft_x0).max(1);
    let shaft_y = cy - T / 2;
    fb.fill_rect(
        shaft_y.max(0) as u32,
        shaft_x0.max(0) as u32,
        shaft_w as u32,
        T as u32,
        shade,
    );
    // Teeth: two short prongs hanging below the shaft near the tip.
    let tooth_h = s / 2;
    for off in [0, s * 2 / 5] {
        let tx = tip_x - off - T;
        fb.fill_rect(
            (shaft_y + T).max(0) as u32,
            tx.max(0) as u32,
            T as u32,
            tooth_h as u32,
            shade,
        );
    }
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
    fb.fill_rect(
        badge_y.max(cell_y) as u32,
        badge_x.max(cell_x) as u32,
        badge_w,
        badge_h,
        0x00,
    );
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
