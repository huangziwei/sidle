//! The library search bar — **one** widget, drawn in both the grid view and the
//! keyboard overlay. Its left edge, top, and height are constant across views, so
//! tapping to open the keyboard never shifts the field under your finger. Only
//! the RIGHT edge differs: the grid view leaves room for the Sync + action
//! buttons (`with_button = true`), while the keyboard overlay has none, so the
//! field stretches to full width (`with_button = false`) — reclaiming that space
//! rather than leaving it blank.

use crate::eink::fb::Framebuffer;
use crate::ui::grid;
use crate::ui::text::TextRenderer;

/// Geometry — the single source of truth for the bar in every view.
pub const TOP: u32 = 16;
pub const HEIGHT: u32 = 88;
pub const MARGIN_X: u32 = 40;
/// Right-hand zone that clears the query (only active when a query is set).
pub const CLEAR_W: u32 = 150;
/// Diameter of each round action button — a circle inscribed in the bar height,
/// so Sync and Update sit as two discs flush to the right margin (search field
/// left, action buttons right — the stock Kindle layout).
pub const BTN_D: u32 = HEIGHT;
/// Gap before the first button and between the two buttons.
pub const BUTTON_GAP: u32 = 24;

/// Search-field pill width for a given view. Grid view (`with_button`): the row
/// between the side margins minus the two round buttons and the two gaps (field↔
/// Sync, Sync↔right), so field + gaps + buttons together span `xres - 2·MARGIN_X`.
/// Keyboard overlay (no buttons): the full span between the margins, so the field
/// reclaims their space instead of leaving it blank. The left edge (`MARGIN_X`) is
/// the same either way — only the right edge moves.
pub fn field_w(xres: u32, with_button: bool) -> u32 {
    if with_button {
        xres.saturating_sub(MARGIN_X * 2 + 2 * BUTTON_GAP + 2 * BTN_D)
    } else {
        xres.saturating_sub(MARGIN_X * 2)
    }
}

/// The **Update** button's rectangle `(x, y, w, h)` — the rightmost disc, flush to
/// the right margin.
pub fn update_button_rect(xres: u32) -> (u32, u32, u32, u32) {
    (xres.saturating_sub(MARGIN_X + BTN_D), TOP, BTN_D, BTN_D)
}

/// The **Sync** button's rectangle `(x, y, w, h)` — the disc left of Update.
pub fn sync_button_rect(xres: u32) -> (u32, u32, u32, u32) {
    (
        xres.saturating_sub(MARGIN_X + 2 * BTN_D + BUTTON_GAP),
        TOP,
        BTN_D,
        BTN_D,
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
    /// binary from sidle-server (the LAN self-update). Drawn only in the grid
    /// view; the keyboard overlay leaves the slot
    /// empty, so a tap there is a harmless no-op (its handler acts only on
    /// `Clear`).
    Update,
    /// The right-hand button in the **DRM** view: decrypt every on-device
    /// purchase and push each to the desktop. Occupies the same slot as `Update`
    /// (the library view's self-update, useless while browsing DRM books), and is
    /// returned there in place of it when `drm` is set (see [`hit`]).
    DecryptAll,
}

/// Hit-test the bar. `query_active` enables the right-hand `✕` (clear) zone,
/// which is only drawn when a query is set. `with_button` must match the value
/// [`draw`] was called with for this view: in the grid view the right-hand pill
/// is an action button; in the full-width keyboard overlay it's part of the
/// field, so no action tap is ever returned there. `drm` selects that button's
/// meaning — [`Tap::DecryptAll`] in the DRM view, [`Tap::Update`] in the library
/// view — matching the glyph [`draw_buttons`] drew for the same view.
pub fn hit(
    tx: u32,
    ty: u32,
    xres: u32,
    query_active: bool,
    with_button: bool,
    drm: bool,
) -> Option<Tap> {
    if !(TOP..TOP + HEIGHT).contains(&ty) {
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
/// selects the width — shorter in the grid view (leaving room for the Sync +
/// Update buttons, drawn separately via [`draw_buttons`]) or full width in the
/// keyboard overlay. The left edge is the same either way, so the field never jumps.
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

/// Draw the two round action buttons flush to the right margin — **Sync** (left)
/// and the source-dependent right disc — each a circle with a hand-drawn glyph
/// (the font has none of 🔄 / ⤓ / 🔑). Only the grid view draws them; the
/// keyboard overlay leaves the slot empty so nothing competes with typing. Sync
/// pushes to sidle-server (annotations in the library, decrypted books in DRM).
/// The right disc is the library view's **Update** (download glyph — pull the
/// picker's next binary) or, when `drm` is set, **Decrypt-All** (key glyph —
/// decrypt every purchase); [`hit`] must be called with the same `drm` so taps
/// resolve to the glyph shown.
pub fn draw_buttons(fb: &mut Framebuffer, drm: bool) {
    let xres = fb.var.xres;
    // Left disc: Sync — same slot and glyph in both sources.
    let (sx, sy, sd, _) = sync_button_rect(xres);
    grid::stroke_round_rect(fb, sx as i32, sy as i32, sd, sd, sd / 2, 3, 0x00);
    grid::draw_sync_glyph(fb, (sx + sd / 2) as i32, (sy + sd / 2) as i32, 20, 0x00);

    // Right disc: Update (library) or Decrypt-All (DRM).
    let (ux, uy, ud, _) = update_button_rect(xres);
    grid::stroke_round_rect(fb, ux as i32, uy as i32, ud, ud, ud / 2, 3, 0x00);
    let (ucx, ucy) = ((ux + ud / 2) as i32, (uy + ud / 2) as i32);
    if drm {
        grid::draw_key_glyph(fb, ucx, ucy, 20, 0x00);
    } else {
        grid::draw_download_glyph(fb, ucx, ucy, 18, 0x00);
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
