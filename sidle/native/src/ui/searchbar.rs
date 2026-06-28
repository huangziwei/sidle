//! The library search bar — **one** widget, drawn pixel-identically in the grid
//! view and the keyboard overlay. Both views call [`draw`] with the same
//! constants, so tapping the bar to open the keyboard never moves or restyles
//! it: the keyboard simply appears below the unchanged bar.

use crate::eink::fb::Framebuffer;
use crate::ui::grid;
use crate::ui::text::TextRenderer;

/// Geometry — the single source of truth for the bar in every view.
pub const TOP: u32 = 16;
pub const HEIGHT: u32 = 88;
pub const MARGIN_X: u32 = 40;
/// Right-hand zone that clears the query (only active when a query is set).
pub const CLEAR_W: u32 = 150;

/// A tap on the bar.
pub enum Tap {
    /// The field — open the keyboard (a no-op when already open).
    Open,
    /// The `✕` zone — clear the query.
    Clear,
}

/// Hit-test the bar. `query_active` enables the right-hand `✕` (clear) zone,
/// which is only drawn when a query is set.
pub fn hit(tx: u32, ty: u32, xres: u32, query_active: bool) -> Option<Tap> {
    let x = MARGIN_X;
    let w = xres.saturating_sub(MARGIN_X * 2);
    if !(TOP..TOP + HEIGHT).contains(&ty) || !(x..x + w).contains(&tx) {
        return None;
    }
    if query_active && tx >= x + w - CLEAR_W {
        return Some(Tap::Clear);
    }
    Some(Tap::Open)
}

/// Draw the bar: a rounded pill + magnifier glyph + the placeholder/query, plus
/// an `✕` clear button when a query is set. Identical in every view.
pub fn draw(fb: &mut Framebuffer, renderer: &mut TextRenderer, query: &str) {
    let xres = fb.var.xres;
    let x = MARGIN_X;
    let w = xres.saturating_sub(MARGIN_X * 2);
    let cy = (TOP + HEIGHT / 2) as i32;
    let baseline = (TOP + HEIGHT * 62 / 100) as i32;

    // Pill frame + magnifier just inside the left rounded end.
    grid::stroke_round_rect(fb, x as i32, TOP as i32, w, HEIGHT, HEIGHT / 2, 3, 0x00);
    let mr = 18u32;
    let mcx = (x + HEIGHT / 2 + 6) as i32;
    grid::draw_magnifier(fb, mcx, cy, mr, 0x00);
    let text_x = mcx + mr as i32 + 24;

    if query.trim().is_empty() {
        renderer.draw(fb, text_x, baseline, "Search by romaji", false);
        return;
    }
    // Active: query text (tail shown when it overflows) + the clear button.
    let right_limit = (x + w).saturating_sub(CLEAR_W) as i32;
    let avail = (right_limit - text_x).max(0) as u32;
    let shown = clamp_tail(renderer, query, avail);
    renderer.draw(fb, text_x, baseline, &shown, false);
    let clear_cx = (x + w).saturating_sub(CLEAR_W / 2) as i32;
    grid::draw_x(fb, clear_cx, cy, 15, 0x00);
}

/// Trailing substring of `s` that fits `max_width`, so a long query scrolls to
/// keep the most recently typed characters visible.
fn clamp_tail(renderer: &mut TextRenderer, s: &str, max_width: u32) -> String {
    if renderer.measure_width(s) <= max_width {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut start = 0;
    while start < chars.len() {
        let tail: String = chars[start..].iter().collect();
        if renderer.measure_width(&tail) <= max_width {
            return tail;
        }
        start += 1;
    }
    String::new()
}
