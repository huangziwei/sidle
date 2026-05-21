//! Bottom-strip toolbar.
//!
//! Always-visible 80px strip at the bottom of the panel with three tap
//! zones, left-to-right:
//!
//! - `[Exit]`         (always shown, left third)
//! - `← Prev / N / Next →` (middle + right third, only when n_pages > 1)
//!
//! Replaces the earlier hidden top-left-corner quit gesture, which was
//! ambiguous with stray touch events near the panel edge.

use crate::eink::fb::Framebuffer;
use crate::ui::text::TextRenderer;

pub const STRIP_H: u32 = 80;
pub const PAGE_SIZE: usize = 9;

const EXIT_ZONE_W: u32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagerHit {
    Exit,
    Prev,
    Next,
}

pub fn n_pages(books: usize) -> usize {
    if books == 0 {
        1
    } else {
        (books + PAGE_SIZE - 1) / PAGE_SIZE
    }
}

pub fn strip_top(fb_yres: u32) -> u32 {
    fb_yres.saturating_sub(STRIP_H)
}

pub fn hit(tx: u32, ty: u32, fb_xres: u32, fb_yres: u32, total_pages: usize) -> Option<PagerHit> {
    if ty < strip_top(fb_yres) {
        return None;
    }
    // Exit takes the leftmost slice; the rest of the strip is split for
    // page nav only when there's somewhere to navigate to.
    if tx < EXIT_ZONE_W {
        return Some(PagerHit::Exit);
    }
    if total_pages <= 1 {
        return None;
    }
    if tx < fb_xres / 2 {
        Some(PagerHit::Prev)
    } else {
        Some(PagerHit::Next)
    }
}

pub fn draw(fb: &mut Framebuffer, renderer: &mut TextRenderer, page: usize, total_pages: usize) {
    let strip_y = strip_top(fb.var.yres);
    // 2px black divider, white strip body below.
    fb.fill_rect(strip_y, 0, fb.var.xres, 2, 0x00);
    fb.fill_rect(strip_y + 2, 0, fb.var.xres, STRIP_H - 2, 0xFF);

    let baseline = (strip_y + STRIP_H * 70 / 100) as i32;

    // Exit on the left. Always visible.
    renderer.draw(fb, 40, baseline, "✕ Exit", false);
    // Vertical separator after exit zone.
    fb.fill_rect(strip_y + 12, EXIT_ZONE_W - 2, 2, STRIP_H - 24, 0x00);

    if total_pages <= 1 {
        return;
    }

    let label_prev = "← Prev";
    let label_next = "Next →";
    let label_mid = format!("{} / {}", page + 1, total_pages);

    if page > 0 {
        renderer.draw(fb, EXIT_ZONE_W as i32 + 40, baseline, label_prev, false);
    }
    let mid_w = renderer.measure_width(&label_mid);
    let mid_x = (fb.var.xres as i32 - mid_w as i32) / 2;
    renderer.draw(fb, mid_x, baseline, &label_mid, false);
    if page + 1 < total_pages {
        let next_w = renderer.measure_width(label_next);
        let next_x = fb.var.xres as i32 - 80 - next_w as i32;
        renderer.draw(fb, next_x, baseline, label_next, false);
    }
}
