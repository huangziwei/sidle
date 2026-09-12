//! The library search bar — one widget, drawn in both the grid view and the
//! keyboard overlay. Only the right edge differs, by `with_button`.

use crate::eink::fb::Framebuffer;
use crate::ui::grid;
use crate::ui::scale::Scale;
use crate::ui::text::TextRenderer;

/// Geometry — the single source of truth for the bar in every view, in design
/// pixels at `scale::DESIGN_DPI`. [`metrics`] puts them on the panel.
const TOP: u32 = 16;
const HEIGHT: u32 = 88;
const MARGIN_X: u32 = 40;
/// Right-hand zone that clears the query (only active when a query is set).
const CLEAR_W: u32 = 150;
/// Diameter of each round action button — a circle inscribed in the bar height,
/// so Sync and Update sit as two discs flush to the right margin (search field
/// left, action buttons right — the stock Kindle layout).
const BTN_D: u32 = HEIGHT;
/// Gap before the first button and between the two buttons.
const BUTTON_GAP: u32 = 24;

/// The bar's geometry on an `xres`-wide panel.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    pub top: u32,
    pub height: u32,
    pub margin_x: u32,
    pub clear_w: u32,
    pub btn_d: u32,
    pub button_gap: u32,
}

pub fn metrics(xres: u32) -> Metrics {
    let s = Scale::of_width(xres);
    Metrics {
        top: s.u(TOP),
        height: s.u(HEIGHT),
        margin_x: s.u(MARGIN_X),
        clear_w: s.u(CLEAR_W),
        btn_d: s.u(BTN_D),
        button_gap: s.u(BUTTON_GAP),
    }
}

/// The first row below the bar — where a view's own content starts.
pub fn below(xres: u32) -> u32 {
    let m = metrics(xres);
    m.top + m.height
}

/// Search-field pill width for a given view. Grid view (`with_button`): the row
/// between the side margins minus the two round buttons and the two gaps (field↔
/// Sync, Sync↔right), so field + gaps + buttons together span `xres - 2·MARGIN_X`.
pub fn field_w(xres: u32, with_button: bool) -> u32 {
    let m = metrics(xres);
    if with_button {
        xres.saturating_sub(m.margin_x * 2 + 2 * m.button_gap + 2 * m.btn_d)
    } else {
        xres.saturating_sub(m.margin_x * 2)
    }
}

/// The **Update** button's rectangle `(x, y, w, h)` — the rightmost disc, flush to
/// the right margin.
pub fn update_button_rect(xres: u32) -> (u32, u32, u32, u32) {
    let m = metrics(xres);
    (
        xres.saturating_sub(m.margin_x + m.btn_d),
        m.top,
        m.btn_d,
        m.btn_d,
    )
}

/// The **Sync** button's rectangle `(x, y, w, h)` — the disc left of Update.
pub fn sync_button_rect(xres: u32) -> (u32, u32, u32, u32) {
    let m = metrics(xres);
    (
        xres.saturating_sub(m.margin_x + 2 * m.btn_d + m.button_gap),
        m.top,
        m.btn_d,
        m.btn_d,
    )
}

/// A tap on the bar.
pub enum Tap {
    /// The field — open the keyboard (a no-op when already open).
    Open,
    /// The `✕` zone — clear the query.
    Clear,
    /// The **Sync** button — push this device's reading-state sidecars to
    /// sidle-server (the LAN twin of a USB annotation sync). Drawn only in the
    /// grid view.
    Sync,
    /// The right-hand button in the **library** view: pull the picker's next
    Update,
    /// The right-hand button in the **DRM** view: decrypt every on-device
    DecryptAll,
}

/// Hit-test the bar. `query_active` enables the `✕` zone; `with_button` must match
/// what [`draw`] was called with for this view, and `drm` selects that button.
pub fn hit(
    tx: u32,
    ty: u32,
    xres: u32,
    query_active: bool,
    with_button: bool,
    drm: bool,
) -> Option<Tap> {
    let m = metrics(xres);
    if !(m.top..m.top + m.height).contains(&ty) {
        return None;
    }
    // Action buttons — the two right-hand discs, checked first (they sit outside
    // the field's x-span). Only present in the grid view.
    if with_button {
        let (ux, _, ud, _) = update_button_rect(xres);
        if (ux..ux + ud).contains(&tx) {
            return Some(if drm { Tap::DecryptAll } else { Tap::Update });
        }
        let (sx, _, sd, _) = sync_button_rect(xres);
        if (sx..sx + sd).contains(&tx) {
            return Some(Tap::Sync);
        }
    }
    // Search field pill (left of the buttons in the grid; full width otherwise).
    let x = m.margin_x;
    let w = field_w(xres, with_button);
    if !(x..x + w).contains(&tx) {
        return None;
    }
    if query_active && tx >= x + w - m.clear_w {
        return Some(Tap::Clear);
    }
    Some(Tap::Open)
}

/// Draw the search field: rounded pill, magnifier, placeholder or query, plus an
/// `✕` when a query is set. `with_button` sets the width; the left edge is fixed.
pub fn draw(fb: &mut Framebuffer, renderer: &mut TextRenderer, query: &str, with_button: bool) {
    let xres = fb.var.xres;
    let sc = Scale::of_width(xres);
    let m = metrics(xres);
    let x = m.margin_x;
    let w = field_w(xres, with_button);
    let cy = (m.top + m.height / 2) as i32;
    let baseline = (m.top + m.height * 62 / 100) as i32;

    // Pill frame + magnifier just inside the left rounded end.
    grid::stroke_round_rect(
        fb,
        x as i32,
        m.top as i32,
        w,
        m.height,
        m.height / 2,
        sc.u(3),
        0x00,
    );
    let mr = sc.u(18);
    let mcx = (x + m.height / 2 + sc.u(6)) as i32;
    grid::draw_magnifier(fb, mcx, cy, mr, 0x00);
    let text_x = mcx + mr as i32 + sc.px(24);

    if query.trim().is_empty() {
        renderer.draw(fb, text_x, baseline, "Search by romaji", false);
        return;
    }
    // Active: query text (tail shown when it overflows) + the clear button.
    let right_limit = (x + w).saturating_sub(m.clear_w) as i32;
    let avail = (right_limit - text_x).max(0) as u32;
    let shown = clamp_tail(renderer, query, avail);
    renderer.draw(fb, text_x, baseline, &shown, false);
    let clear_cx = (x + w).saturating_sub(m.clear_w / 2) as i32;
    grid::draw_x(fb, clear_cx, cy, sc.px(15), 0x00);
}

/// Draw the two round action buttons flush to the right margin — **Sync** (left)
pub fn draw_buttons(fb: &mut Framebuffer, drm: bool) {
    let xres = fb.var.xres;
    let sc = Scale::of_width(xres);
    let rule = sc.u(3);
    // Left disc: Sync — same slot and glyph in both sources.
    let (sx, sy, sd, _) = sync_button_rect(xres);
    grid::stroke_round_rect(fb, sx as i32, sy as i32, sd, sd, sd / 2, rule, 0x00);
    grid::draw_sync_glyph(
        fb,
        (sx + sd / 2) as i32,
        (sy + sd / 2) as i32,
        sc.px(20),
        0x00,
    );

    // Right disc: Update (library) or Decrypt-All (DRM).
    let (ux, uy, ud, _) = update_button_rect(xres);
    grid::stroke_round_rect(fb, ux as i32, uy as i32, ud, ud, ud / 2, rule, 0x00);
    let (ucx, ucy) = ((ux + ud / 2) as i32, (uy + ud / 2) as i32);
    if drm {
        grid::draw_key_glyph(fb, ucx, ucy, sc.px(20), 0x00);
    } else {
        grid::draw_download_glyph(fb, ucx, ucy, sc.px(18), 0x00);
    }
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
