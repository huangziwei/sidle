//! On-screen Latin keyboard — the romaji search overlay.
//!
//! A full CJK IME is infeasible on the device, but search needs only the ~26
//! Latin letters a book's romaji is spelled with (the romanization happens once
//! on the desktop; see `sidle_core::library::romaji`). So this is a plain QWERTY
//! grid: tap letters/digits, watch a **live match count** update, hit
//! `[ Search ]` to filter the grid or `[ Back ]` to leave it as it was.
//!
//! Layout follows a physical keyboard rather than a phone: there is a number
//! row, so `Del` sits at its right end where a keyboard puts Backspace. The
//! screen-level commands are not keys — they live in a bottom strip with the
//! same geometry and grammar as [`crate::ui::filtermenu`] and
//! [`crate::ui::pager`].
//!
//! Two of those commands leave the overlay and they are not the same thing.
//! `[ Search ]` is this keyboard's Enter: it submits, and sits at the far right
//! where a keyboard puts Enter. `[ Back ]` abandons, returning the query the
//! overlay opened with, and takes the leftmost slot — the one the gallery gives
//! `Exit`. Without it the only way out committed a query, so an overlay opened
//! by accident had to be paid for with one.
//!
//! Same blocking-sub-loop shape as [`crate::ui::filtermenu`] / [`crate::ui::sortmenu`]:
//! it owns input while open, full GC16 on open / page / rotate, and a single-band
//! DU on a keystroke so typing doesn't flash the whole panel. All key labels are
//! ASCII (`Del`/`Clear`/`space`/`[ Back ]`/`[ Search ]`) — no glyph-coverage
//! risk, same discipline as `ui::diag`.

use crate::api::Book;
use crate::eink::fb::{Framebuffer, MxcfbRect, WAVEFORM_MODE_DU, WAVEFORM_MODE_GC16};
use crate::eink::input::{Input, InputEvent};
use crate::eink::touch::TouchEvent;
use crate::orientation::Orientation;
use crate::search;
use crate::ui::filter::{self, Filters};
use crate::ui::grid::outline_rect;
use crate::ui::searchbar;
use crate::ui::text::TextRenderer;

/// Letter/digit rows. Row 0 carries `Del` in an eleventh cell at its right end,
/// appended by [`layout`].
const ROWS: [&str; 4] = ["1234567890", "qwertyuiop", "asdfghjkl", "zxcvbnm"];

/// Gap between key faces, and the panel margin.
const GAP: i32 = 8;
const MARGIN: i32 = 20;

/// Bottom command strip: `[ Back ] | Clear | space | [ Search ]`. Height matches
/// `ui/filtermenu.rs` and `ui/sortmenu.rs`; `ZONE_W` matches the fixed left slots
/// in `ui/pager.rs` and `ui/filtermenu.rs`, so `[ Back ]` lands where `Exit` and
/// the menus' own leave action do.
const STRIP_H: u32 = 120;
const ZONE_W: u32 = 200;
/// Thickness of the strip's divider rules, matching the other bottom strips.
const RULE: u32 = 2;

#[derive(Clone, Copy)]
enum Key {
    Char(char),
    Space,
    Backspace,
    Clear,
    /// Leave without applying, returning the query the overlay opened with, so
    /// an accidental open costs nothing. Distinct from [`Key::Done`], which is
    /// this keyboard's Enter.
    Back,
    /// Submit: filter the grid by what has been typed.
    Done,
}

/// How a key is drawn. Both kinds hit-test the same way.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Style {
    /// A key in the grid: an outlined face inset from its cell.
    Face,
    /// A slot in the bottom strip: a label only, with the strip drawing the
    /// dividers around it.
    Zone,
}

/// `x`/`y`/`w`/`h` is the **cell** — the whole tappable area, not the drawn
/// face. A `Face` key draws inset by half a gap on each side, so the gutters
/// between keys still belong to a key and a tap there registers.
struct KeyButton {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    key: Key,
    label: String,
    style: Style,
}

/// Top of the key grid — the band above it holds the title, query, and count.
/// Bottom of the top band (the shared search bar + the match count below it). A
/// keystroke refreshes only `[0, band_bottom]`, leaving the keyboard untouched.
fn band_bottom(lh: u32) -> u32 {
    searchbar::TOP + searchbar::HEIGHT + lh + 24
}

fn strip_top(yres: u32) -> u32 {
    yres.saturating_sub(STRIP_H)
}

/// Keyboard metrics: `(unit, unit_digits, key_h, keys_top)`.
///
/// Letter rows divide the span into ten columns. The digit row spans the same
/// width but needs an eleventh cell for `Del`, so it divides that span into
/// eleven — the two rows line up at their edges and not in between, which is
/// what a physical keyboard's stagger does anyway.
///
/// Key faces are square: a row is as tall as a letter key is wide. Nothing
/// physical or on-screen uses keys taller than they are wide, and the panel has
/// vertical room to spare, so height follows width rather than stretching it.
/// One row height serves every row, so the digit row's slightly narrower cells
/// stay aligned with the letters above and below.
///
/// The four-row block is bottom-anchored above the command strip, so the search
/// field stays at the top and the keys rest near the thumb.
fn metrics(xres: u32, yres: u32) -> (i32, i32, i32, i32) {
    let span = (xres as i32 - 2 * MARGIN).max(1);
    let unit = (span / 10).max(1);
    let unit_digits = (span / 11).max(1);
    let key_h = (unit - GAP).max(1);
    let block_h = 4 * key_h + 3 * GAP;
    let keys_top = (yres as i32 - STRIP_H as i32 - MARGIN - block_h).max(MARGIN);
    (unit, unit_digits, key_h, keys_top)
}

/// Lay out every key: the four letter/digit rows, then the four strip slots.
fn layout(xres: u32, yres: u32) -> Vec<KeyButton> {
    let (unit, unit_digits, key_h, top) = metrics(xres, yres);
    let stride = key_h + GAP;
    let mut out = Vec::new();

    for (r, row) in ROWS.iter().enumerate() {
        let digits = r == 0;
        let u = if digits { unit_digits } else { unit };
        // The digit row reserves one extra column for `Del`.
        let n = row.chars().count() as i32 + i32::from(digits);
        let start_x = (xres as i32 - n * u) / 2;
        let y = top + r as i32 * stride;
        for (i, c) in row.chars().enumerate() {
            out.push(KeyButton {
                x: start_x + i as i32 * u,
                y,
                w: u as u32,
                h: stride as u32,
                key: Key::Char(c),
                label: c.to_string(),
                style: Style::Face,
            });
        }
        if digits {
            out.push(KeyButton {
                x: start_x + (n - 1) * u,
                y,
                w: u as u32,
                h: stride as u32,
                key: Key::Backspace,
                label: "Del".to_string(),
                style: Style::Face,
            });
        }
    }

    // Command strip. `[ Back ]` takes the leftmost slot, the one `ui/pager.rs`
    // gives `Exit` on the gallery and `ui/filtermenu.rs` gives its own leave
    // action — leaving a screen is the same gesture wherever you are.
    // `[ Search ]` is this keyboard's Enter, so it sits at the far right where a
    // keyboard puts it, with the wide `space` between it and `Clear` so a
    // mis-tap cannot wipe the query and submit in one slip.
    let sy = strip_top(yres) as i32;
    let side = ZONE_W.min(xres / 5);
    for (x, w, key, label) in [
        (0, side, Key::Back, "[ Back ]"),
        (side as i32, side, Key::Clear, "Clear"),
        (
            (side * 2) as i32,
            xres.saturating_sub(side * 3),
            Key::Space,
            "space",
        ),
        (
            xres.saturating_sub(side) as i32,
            side,
            Key::Done,
            "[ Search ]",
        ),
    ] {
        out.push(KeyButton {
            x,
            y: sy,
            w,
            h: STRIP_H,
            key,
            label: label.to_string(),
            style: Style::Zone,
        });
    }
    out
}

fn full_rect(fb: &Framebuffer) -> MxcfbRect {
    MxcfbRect {
        top: 0,
        left: 0,
        width: fb.var.xres,
        height: fb.var.yres,
    }
}

/// The query+count band at the top — its own rect so a keystroke refreshes only
/// this with a fast DU instead of the whole panel.
fn band_rect(fb: &Framebuffer, lh: u32) -> MxcfbRect {
    MxcfbRect {
        top: 0,
        left: 0,
        width: fb.var.xres,
        height: band_bottom(lh),
    }
}

/// The drawn face of a key: for a grid key the cell inset by half a gap; for a
/// strip slot the cell inset past the strip's rules on its top and left edges.
///
/// That inset is what keeps a press from erasing the chrome. The rules are drawn
/// *inside* the slot rects — the top rule along the strip's first rows, each
/// vertical rule at a slot's own left edge — so a face covering the whole cell
/// paints over them when it fills, and the restore leaves them gone.
fn face(kb: &KeyButton) -> (i32, i32, u32, u32) {
    match kb.style {
        Style::Zone => (
            kb.x + RULE as i32,
            kb.y + RULE as i32,
            kb.w.saturating_sub(RULE).max(1),
            kb.h.saturating_sub(RULE).max(1),
        ),
        Style::Face => {
            let inset = GAP / 2;
            (
                kb.x + inset,
                kb.y + inset,
                kb.w.saturating_sub(GAP as u32).max(1),
                kb.h.saturating_sub(GAP as u32).max(1),
            )
        }
    }
}

fn key_rect(kb: &KeyButton) -> MxcfbRect {
    let (x, y, w, h) = face(kb);
    MxcfbRect {
        top: y.max(0) as u32,
        left: x.max(0) as u32,
        width: w,
        height: h,
    }
}

/// Books passing the active facets **and** the typed query — the same predicate
/// the grid will use, so the count never lies.
fn count_matches(all_books: &[Book], filters: &Filters, query: &str) -> usize {
    let cq = search::canon(query);
    all_books
        .iter()
        .filter(|b| filter::matches(b, filters, None) && search::matches(b, &cq))
        .count()
}

/// Draw the **shared** search bar (identical to the grid view — same position,
/// size, style) and the live match count directly below it. Caller white-fills
/// the band first.
fn draw_band(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    all_books: &[Book],
    filters: &Filters,
    query: &str,
    lh: u32,
) {
    let xres = fb.var.xres;
    searchbar::draw(fb, renderer, query, false);

    // Match count, centered, directly below the bar.
    let n = count_matches(all_books, filters, query);
    let count = if query.trim().is_empty() {
        format!("{n} books")
    } else if n == 0 {
        "no matches".to_string()
    } else if n == 1 {
        "1 match".to_string()
    } else {
        format!("{n} matches")
    };
    let cw = renderer.measure_width(&count);
    let cy = (searchbar::TOP + searchbar::HEIGHT + lh) as i32;
    renderer.draw(
        fb,
        ((xres as i32 - cw as i32) / 2).max(0),
        cy,
        &count,
        false,
    );
}

/// Draw one key. `pressed` inverts it — filled black with a white label — which
/// is the only acknowledgement a tap gets while the finger is still down: the
/// band refresh lands later and at the far end of the screen, so without this a
/// tap that registered looks exactly like one that missed.
fn draw_key(fb: &mut Framebuffer, renderer: &mut TextRenderer, kb: &KeyButton, pressed: bool) {
    let (x, y, w, h) = face(kb);
    let (top, left) = (y.max(0) as u32, x.max(0) as u32);
    fb.fill_rect(top, left, w, h, if pressed { 0x00 } else { 0xFF });
    if !pressed && kb.style == Style::Face {
        outline_rect(fb, x, y, w, h, 2, 0x00);
    }
    let lw = renderer.measure_width(&kb.label);
    let tx = x + ((w as i32 - lw as i32) / 2).max(0);
    let baseline = y + (h * 62 / 100) as i32;
    renderer.draw(fb, tx, baseline, &kb.label, pressed);
}

/// The strip's chrome: the rule above it and the slot separators, drawn the same
/// way `ui/filtermenu.rs` and `ui/pager.rs` draw theirs. [`face`] insets a slot
/// past these, so pressing one leaves them intact.
fn draw_strip_chrome(fb: &mut Framebuffer, keys: &[KeyButton]) {
    let xres = fb.var.xres;
    let top = strip_top(fb.var.yres);
    fb.fill_rect(top, 0, xres, RULE, 0x00);
    for kb in keys.iter().filter(|k| k.style == Style::Zone) {
        if kb.x > 0 {
            fb.fill_rect(top + 12, kb.x.max(0) as u32, RULE, STRIP_H - 24, 0x00);
        }
    }
}

fn render_all(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    all_books: &[Book],
    filters: &Filters,
    keys: &[KeyButton],
    query: &str,
    lh: u32,
) {
    fb.fill_rect(0, 0, fb.var.xres, fb.var.yres, 0xFF);
    draw_band(fb, renderer, all_books, filters, query, lh);
    for kb in keys {
        draw_key(fb, renderer, kb, false);
    }
    draw_strip_chrome(fb, keys);
}

/// Index of the key under a touch. Cells tile their row, so a tap in a gutter
/// lands on a neighbouring key rather than resolving to nothing.
fn hit_index(keys: &[KeyButton], tx: u32, ty: u32) -> Option<usize> {
    let (tx, ty) = (tx as i32, ty as i32);
    keys.iter()
        .position(|k| tx >= k.x && tx < k.x + k.w as i32 && ty >= k.y && ty < k.y + k.h as i32)
}

fn hit(keys: &[KeyButton], tx: u32, ty: u32) -> Option<Key> {
    hit_index(keys, tx, ty).map(|i| keys[i].key)
}

/// Run the keyboard. Returns the typed query on `[ Search ]`, or `initial`
/// unchanged on `[ Back ]` — the caller acts only when the two differ, so
/// abandoning costs nothing. `initial` also pre-fills the box, so re-opening
/// edits the current search rather than starting over.
pub fn run(
    fb: &mut Framebuffer,
    input: &mut Input,
    renderer: &mut TextRenderer,
    all_books: &[Book],
    filters: &Filters,
    initial: &str,
    orient: &mut Orientation,
) -> anyhow::Result<String> {
    let lh = renderer.line_height().max(1);
    let mut query = initial.to_string();
    let mut keys = layout(fb.var.xres, fb.var.yres);
    // The key currently held down, so it can be un-inverted on release.
    let mut pressed: Option<usize> = None;

    render_all(fb, renderer, all_books, filters, &keys, &query, lh);
    fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;

    // Refresh just the query+count band after a keystroke (fast DU, no flash).
    macro_rules! refresh_band {
        () => {{
            fb.fill_rect(0, 0, fb.var.xres, band_bottom(lh), 0xFF);
            draw_band(fb, renderer, all_books, filters, &query, lh);
            fb.send_update(band_rect(fb, lh), WAVEFORM_MODE_DU)?;
        }};
    }

    loop {
        match input.next()? {
            InputEvent::Touch(TouchEvent::Up { x, y }) => {
                if let Some(i) = pressed.take() {
                    draw_key(fb, renderer, &keys[i], false);
                    fb.send_update(key_rect(&keys[i]), WAVEFORM_MODE_DU)?;
                }
                // The search bar stays live in the overlay — its `✕` clears,
                // consistent with the grid view; a field tap is a no-op (already
                // open). Otherwise resolve a key.
                // `with_button = false` here (the overlay draws no action discs),
                // so `drm` is moot — only `Clear`/`Open` come back.
                if let Some(tap) =
                    searchbar::hit(x, y, fb.var.xres, !query.is_empty(), false, false)
                {
                    if matches!(tap, searchbar::Tap::Clear) {
                        query.clear();
                        refresh_band!();
                    }
                } else {
                    match hit(&keys, x, y) {
                        Some(Key::Char(c)) => {
                            query.push(c);
                            refresh_band!();
                        }
                        Some(Key::Space) => {
                            // Harmless to matching (`canon` drops it) but lets the
                            // user separate words visually.
                            query.push(' ');
                            refresh_band!();
                        }
                        Some(Key::Backspace) => {
                            query.pop();
                            refresh_band!();
                        }
                        Some(Key::Clear) => {
                            query.clear();
                            refresh_band!();
                        }
                        Some(Key::Done) => return Ok(query),
                        // Hand back what the overlay opened with. The caller
                        // compares against its current query, so an unchanged
                        // return is a no-op and nothing is re-filtered.
                        Some(Key::Back) => return Ok(initial.to_string()),
                        None => {}
                    }
                }
            }
            InputEvent::Touch(TouchEvent::Down { x, y }) => {
                if searchbar::hit(x, y, fb.var.xres, !query.is_empty(), false, false).is_none()
                    && let Some(i) = hit_index(&keys, x, y)
                {
                    draw_key(fb, renderer, &keys[i], true);
                    fb.send_update(key_rect(&keys[i]), WAVEFORM_MODE_DU)?;
                    pressed = Some(i);
                }
            }
            InputEvent::Touch(TouchEvent::Screenshot) => {
                let _ = crate::eink::screenshot::capture(fb);
            }
            InputEvent::Page(_) => {}
            InputEvent::Tick => {
                let o = Orientation::detect();
                if o != *orient {
                    *orient = o;
                    input.set_orientation(o);
                    keys = layout(fb.var.xres, fb.var.yres);
                    pressed = None;
                    render_all(fb, renderer, all_books, filters, &keys, &query, lh);
                    fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XRES: u32 = 1264;
    const YRES: u32 = 1680;

    fn find(keys: &[KeyButton], c: char) -> &KeyButton {
        keys.iter()
            .find(|k| matches!(k.key, Key::Char(x) if x == c))
            .expect("key present")
    }

    #[test]
    fn layout_covers_every_letter_and_digit() {
        let keys = layout(XRES, YRES);
        // 11 (digits + Del) + 10 + 9 + 7 faces, plus the four strip slots.
        assert_eq!(keys.len(), 37 + 4);
        for c in "abcdefghijklmnopqrstuvwxyz0123456789".chars() {
            assert!(
                keys.iter().any(|k| matches!(k.key, Key::Char(x) if x == c)),
                "missing key {c}"
            );
        }
    }

    #[test]
    fn del_sits_at_the_right_end_of_the_number_row() {
        let keys = layout(XRES, YRES);
        let zero = find(&keys, '0');
        let del = keys
            .iter()
            .find(|k| matches!(k.key, Key::Backspace))
            .expect("Del present");
        assert_eq!(del.y, zero.y, "Del shares the number row");
        assert!(del.x > zero.x, "Del sits right of 0");
        assert_eq!(del.style, Style::Face);
        // The digit row spans the same width as a letter row, within a column.
        let q = find(&keys, 'q');
        let p = find(&keys, 'p');
        let one = find(&keys, '1');
        let letter_span = p.x + p.w as i32 - q.x;
        let digit_span = del.x + del.w as i32 - one.x;
        assert!(
            (letter_span - digit_span).abs() <= del.w as i32,
            "rows span the same width: letters {letter_span}, digits {digit_span}"
        );
    }

    #[test]
    fn back_takes_the_leftmost_slot_and_search_the_rightmost() {
        let keys = layout(XRES, YRES);
        let back = keys
            .iter()
            .find(|k| matches!(k.key, Key::Back))
            .expect("Back present");
        assert_eq!(back.x, 0, "Back is flush to the left edge, as Exit is");
        assert_eq!(back.y, strip_top(YRES) as i32);
        assert_eq!(back.style, Style::Zone);
        // Left to right: Back, Clear, space, Search — the submit at the far
        // right, and the space bar between Clear and Search so a mis-tap cannot
        // wipe the query and submit in one slip.
        let row = YRES - 10;
        assert!(matches!(hit(&keys, 10, row), Some(Key::Back)));
        assert!(matches!(hit(&keys, ZONE_W + 10, row), Some(Key::Clear)));
        assert!(matches!(hit(&keys, XRES / 2, row), Some(Key::Space)));
        assert!(matches!(hit(&keys, XRES - 10, row), Some(Key::Done)));
    }

    #[test]
    fn pressing_a_strip_slot_cannot_erase_the_chrome() {
        // A slot's face must stay inside its cell. A face covering the whole
        // cell paints over the top rule and over the vertical rule at its own
        // left edge when filled on press, and the restore draws no chrome, so
        // both stay missing. The space bar, the widest slot, shows it worst.
        let keys = layout(XRES, YRES);
        let strip = strip_top(YRES) as i32;
        for kb in keys.iter().filter(|k| k.style == Style::Zone) {
            let (fx, fy, _, _) = face(kb);
            assert!(
                fy >= strip + RULE as i32,
                "{} face starts at y={fy}, inside the top rule at {strip}",
                kb.label
            );
            if kb.x > 0 {
                assert!(
                    fx >= kb.x + RULE as i32,
                    "{} face starts at x={fx}, inside its own rule at {}",
                    kb.label,
                    kb.x
                );
            }
        }
    }

    #[test]
    fn back_abandons_the_edit() {
        // `Back` hands the caller the query the overlay opened with, so the
        // caller's "did it change?" guard makes it a no-op. Guarding the wiring
        // rather than the loop: `run` needs a framebuffer to drive.
        let keys = layout(XRES, YRES);
        assert!(matches!(hit(&keys, 10, YRES - 10), Some(Key::Back)));
        assert!(
            keys.iter().filter(|k| matches!(k.key, Key::Done)).count() == 1,
            "exactly one submit key"
        );
    }

    #[test]
    fn a_tap_in_a_gutter_still_lands_on_a_key() {
        let keys = layout(XRES, YRES);
        let a = find(&keys, 'a');
        let s = find(&keys, 's');
        // The seam between two neighbouring keys belongs to one of them.
        let seam = (a.x + a.w as i32) as u32;
        assert!(seam <= s.x as u32 + 1, "cells tile the row");
        let mid = a.y + a.h as i32 / 2;
        assert!(hit(&keys, seam.saturating_sub(1), mid as u32).is_some());
        assert!(hit(&keys, seam, mid as u32).is_some());
        // The drawn face is inset, so it is narrower than the cell it fills.
        let (_, _, fw, _) = face(a);
        assert!(fw < a.w, "face {fw} is inset within cell {}", a.w);
    }

    #[test]
    fn letter_key_faces_are_square() {
        let keys = layout(XRES, YRES);
        let (_, _, fw, fh) = face(find(&keys, 'a'));
        assert_eq!(fw, fh, "a letter face is {fw}x{fh}, not square");
    }

    #[test]
    fn hit_finds_the_tapped_key() {
        let keys = layout(XRES, YRES);
        let k0 = &keys[0];
        let cx = (k0.x + k0.w as i32 / 2) as u32;
        let cy = (k0.y + k0.h as i32 / 2) as u32;
        assert!(matches!(hit(&keys, cx, cy), Some(Key::Char('1'))));
        // Above the key block there is nothing to tap.
        assert!(hit(&keys, 0, 0).is_none());
    }
}
