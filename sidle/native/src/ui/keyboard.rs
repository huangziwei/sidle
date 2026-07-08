//! On-screen Latin keyboard — the romaji search overlay.
//!
//! A full CJK IME is infeasible on the device, but search needs only the ~26
//! Latin letters a book's romaji is spelled with (the romanization happens once
//! on the desktop; see `sidle_core::library::romaji`). So this is a plain QWERTY
//! grid: tap letters/digits, watch a **live match count** update, hit `Done` to
//! filter the grid.
//!
//! Same blocking-sub-loop shape as [`crate::ui::filtermenu`] / [`crate::ui::sortmenu`]:
//! it owns input while open, full GC16 on open / page / rotate, and a single-band
//! DU on a keystroke so typing doesn't flash the whole panel. All key labels are
//! ASCII (`Del`/`Clear`/`Done`/`space`) — no glyph-coverage risk, same discipline
//! as `ui::diag`.

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

/// Letter/digit rows. The action row (space / Del / Clear / Done) is laid out
/// separately because its keys span multiple column units.
const ROWS: [&str; 4] = ["1234567890", "qwertyuiop", "asdfghjkl", "zxcvbnm"];

/// Gap between keys, and the panel margin.
const GAP: i32 = 8;
const MARGIN: i32 = 20;

#[derive(Clone, Copy)]
enum Key {
    Char(char),
    Space,
    Backspace,
    Clear,
    Done,
}

struct KeyButton {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    key: Key,
    label: String,
}

/// Top of the key grid — the band above it holds the title, query, and count.
/// Bottom of the top band (the shared search bar + the match count below it). A
/// keystroke refreshes only `[0, band_bottom]`, leaving the keyboard untouched.
fn band_bottom(lh: u32) -> u32 {
    searchbar::TOP + searchbar::HEIGHT + lh + 24
}

/// Keyboard metrics for the panel: `(unit, key_w, key_h, keys_top)`. Keys are a
/// touch taller than wide (comfortable tap targets — not the old domino
/// proportions), and the 5-row block is **bottom-anchored** like a real soft
/// keyboard, so the search field sits up top and the keys rest at the bottom.
fn metrics(xres: u32, yres: u32) -> (i32, i32, i32, i32) {
    let unit = ((xres as i32 - 2 * MARGIN) / 10).max(1);
    let key_w = (unit - GAP).max(1);
    let key_h = (key_w * 13 / 10).max(1);
    let block_h = 5 * key_h + 4 * GAP;
    let keys_top = (yres as i32 - MARGIN - block_h).max(MARGIN);
    (unit, key_w, key_h, keys_top)
}

/// Lay out every key. Letter/digit rows use a fixed column `unit` (10 wide),
/// centered; the action row fills the same width with weighted keys (space ×4,
/// the rest ×2 → 10 units).
fn layout(xres: u32, yres: u32) -> Vec<KeyButton> {
    let (unit, key_w, key_h, top) = metrics(xres, yres);
    let stride = key_h + GAP;
    let mut out = Vec::new();
    for (r, row) in ROWS.iter().enumerate() {
        let n = row.chars().count() as i32;
        let start_x = (xres as i32 - n * unit) / 2;
        let y = top + r as i32 * stride;
        for (i, c) in row.chars().enumerate() {
            out.push(KeyButton {
                x: start_x + i as i32 * unit,
                y,
                w: key_w as u32,
                h: key_h as u32,
                key: Key::Char(c),
                label: c.to_string(),
            });
        }
    }

    // Action row: weights sum to 10 units, centered like a full letter row.
    let y = top + 4 * stride;
    let actions = [
        (Key::Space, "space", 4i32),
        (Key::Backspace, "Del", 2),
        (Key::Clear, "Clear", 2),
        (Key::Done, "Done", 2),
    ];
    let mut ax = (xres as i32 - 10 * unit) / 2;
    for (key, label, weight) in actions {
        out.push(KeyButton {
            x: ax,
            y,
            w: (weight * unit - GAP).max(1) as u32,
            h: key_h as u32,
            key,
            label: label.to_string(),
        });
        ax += weight * unit;
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

fn draw_key(fb: &mut Framebuffer, renderer: &mut TextRenderer, kb: &KeyButton) {
    outline_rect(fb, kb.x, kb.y, kb.w, kb.h, 2, 0x00);
    let lw = renderer.measure_width(&kb.label);
    let tx = kb.x + ((kb.w as i32 - lw as i32) / 2).max(0);
    let baseline = kb.y + (kb.h * 62 / 100) as i32;
    renderer.draw(fb, tx, baseline, &kb.label, false);
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
        draw_key(fb, renderer, kb);
    }
}

fn hit(keys: &[KeyButton], tx: u32, ty: u32) -> Option<Key> {
    let (tx, ty) = (tx as i32, ty as i32);
    keys.iter()
        .find(|k| tx >= k.x && tx < k.x + k.w as i32 && ty >= k.y && ty < k.y + k.h as i32)
        .map(|k| k.key)
}

/// Run the keyboard. Returns the final query on `Done` (the caller filters the
/// grid by it). `initial` pre-fills the box so re-opening edits the current
/// search rather than starting over.
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
                        None => {}
                    }
                }
            }
            InputEvent::Touch(TouchEvent::Down { .. }) => {}
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

    #[test]
    fn layout_keys_are_disjoint_and_cover_all_letters() {
        let keys = layout(1264, 1680);
        // 10 + 10 + 9 + 7 letters/digits + 4 action keys.
        assert_eq!(keys.len(), 36 + 4);
        // Every a-z and 0-9 present.
        for c in "abcdefghijklmnopqrstuvwxyz0123456789".chars() {
            assert!(
                keys.iter().any(|k| matches!(k.key, Key::Char(x) if x == c)),
                "missing key {c}"
            );
        }
    }

    #[test]
    fn hit_finds_the_tapped_key() {
        let keys = layout(1264, 1680);
        // Tap the center of the first key and confirm it resolves to that key.
        let k0 = &keys[0];
        let cx = (k0.x + k0.w as i32 / 2) as u32;
        let cy = (k0.y + k0.h as i32 / 2) as u32;
        assert!(matches!(hit(&keys, cx, cy), Some(Key::Char('1'))));
        // A gap between rows resolves to nothing.
        assert!(hit(&keys, 0, 0).is_none());
    }
}
