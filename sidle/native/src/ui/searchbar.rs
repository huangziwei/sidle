//! The library search bar — **one** widget, drawn in both the grid view and the
//! keyboard overlay. Its left edge, top, and height are constant across views, so
//! tapping to open the keyboard never shifts the field under your finger. Only
//! the RIGHT edge differs: the grid view leaves room for the Update button
//! (`with_button = true`), while the keyboard overlay has no button, so the field
//! stretches to full width (`with_button = false`) — reclaiming that space rather
//! than leaving it blank.

use crate::eink::fb::Framebuffer;
use crate::ui::grid;
use crate::ui::text::TextRenderer;

/// Geometry — the single source of truth for the bar in every view.
pub const TOP: u32 = 16;
pub const HEIGHT: u32 = 88;
pub const MARGIN_X: u32 = 40;
/// Right-hand zone that clears the query (only active when a query is set).
pub const CLEAR_W: u32 = 150;
/// Width of the **Update** action button — the pill flush to the right margin,
/// echoing the stock Kindle layout (search field left, action buttons right).
pub const BUTTON_W: u32 = 190;
/// Gap between the search field and the Update button.
pub const BUTTON_GAP: u32 = 24;

/// Search-field pill width for a given view. Grid view (`with_button`): the row
/// between the side margins minus the Update button and the gap before it, so
/// field + gap + button together span `xres - 2·MARGIN_X`. Keyboard overlay (no
/// button): the full span between the margins, so the field reclaims the button's
/// space instead of leaving it blank. The left edge (`MARGIN_X`) is the same
/// either way — only the right edge moves.
pub fn field_w(xres: u32, with_button: bool) -> u32 {
    if with_button {
        xres.saturating_sub(MARGIN_X * 2 + BUTTON_GAP + BUTTON_W)
    } else {
        xres.saturating_sub(MARGIN_X * 2)
    }
}

/// The Update button's rectangle `(x, y, w, h)` — a pill flush to the right
/// margin, vertically aligned with the field.
pub fn button_rect(xres: u32) -> (u32, u32, u32, u32) {
    (
        xres.saturating_sub(MARGIN_X + BUTTON_W),
        TOP,
        BUTTON_W,
        HEIGHT,
    )
}

/// A tap on the bar.
pub enum Tap {
    /// The field — open the keyboard (a no-op when already open).
    Open,
    /// The `✕` zone — clear the query.
    Clear,
    /// The **Update** button — pull the picker's next binary from sidle-server
    /// (the LAN self-update that used to be its own KUAL tile). Drawn only in
    /// the grid view; the keyboard overlay leaves the slot empty, so a tap there
    /// is a harmless no-op (its handler acts only on `Clear`).
    Update,
}

/// Hit-test the bar. `query_active` enables the right-hand `✕` (clear) zone,
/// which is only drawn when a query is set. `with_button` must match the value
/// [`draw`] was called with for this view: in the grid view the right-hand pill
/// is the Update button; in the full-width keyboard overlay it's part of the
/// field, so no `Tap::Update` is ever returned there.
pub fn hit(tx: u32, ty: u32, xres: u32, query_active: bool, with_button: bool) -> Option<Tap> {
    if !(TOP..TOP + HEIGHT).contains(&ty) {
        return None;
    }
    // Update button — the right-hand pill, checked first (it sits outside the
    // field's x-span). Only present in the grid view.
    if with_button {
        let (bx, _, bw, _) = button_rect(xres);
        if (bx..bx + bw).contains(&tx) {
            return Some(Tap::Update);
        }
    }
    // Search field pill (left of the button in the grid; full width otherwise).
    let x = MARGIN_X;
    let w = field_w(xres, with_button);
    if !(x..x + w).contains(&tx) {
        return None;
    }
    if query_active && tx >= x + w - CLEAR_W {
        return Some(Tap::Clear);
    }
    Some(Tap::Open)
}

/// Draw the search field: a rounded pill + magnifier glyph + the
/// placeholder/query, plus an `✕` clear button when a query is set. `with_button`
/// selects the width — shorter in the grid view (leaving room for the Update
/// button, drawn separately via [`draw_update`]) or full width in the keyboard
/// overlay. The left edge is the same either way, so the field never jumps.
pub fn draw(fb: &mut Framebuffer, renderer: &mut TextRenderer, query: &str, with_button: bool) {
    let xres = fb.var.xres;
    let x = MARGIN_X;
    let w = field_w(xres, with_button);
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

/// Draw the **Update** button — a pill flush to the right margin, echoing the
/// search field's rounded style, labelled "Update". Only the grid view draws it
/// (the keyboard overlay leaves the slot empty so nothing competes with typing);
/// a tap on it pulls the picker's next binary from sidle-server (see
/// `crate::selfupdate`), the LAN self-update that used to be a separate KUAL tile.
pub fn draw_update(fb: &mut Framebuffer, renderer: &mut TextRenderer) {
    let (x, y, w, h) = button_rect(fb.var.xres);
    grid::stroke_round_rect(fb, x as i32, y as i32, w, h, h / 2, 3, 0x00);
    let label = "Update";
    let lw = renderer.measure_width(label);
    let tx = x as i32 + ((w as i32 - lw as i32) / 2).max(0);
    let baseline = (y + h * 62 / 100) as i32;
    renderer.draw(fb, tx, baseline, label, false);
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
