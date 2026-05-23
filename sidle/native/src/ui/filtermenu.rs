//! Filter & sort overlay — the strip's entry point.
//!
//! Two blocking sub-loops in the `ui/sortmenu.rs` / `ui/diag.rs` mold:
//!
//! - [`run`] — the **menu**: a row per facet (showing its selected-count), a
//!   `Sort:` row, and a `[ Clear all | Done ]` strip. Tapping a facet row opens
//!   its value picker; tapping `Sort:` opens [`crate::ui::sortmenu`]. Mutates
//!   the caller's `Filters` + `SortState` in place.
//! - [`value_picker`] — a **paged checklist** of one facet's options
//!   (value + count), tap to toggle, `[ Back | Clear | ← Prev N/M Next → ]`
//!   strip + bezel paging for long lists (100+ authors).
//!
//! Refresh discipline matches the grid and sortmenu: full GC16 on open / page
//! turn / rotation; a single-row DU on a toggle so a tap doesn't flash the
//! screen. Each sub-loop handles its own rotation (`Tick`) — the main loop
//! isn't running while we own input.
//!
//! Markers are bracketed ASCII (`[x]` / `[ ]`, `>`) — no glyph-coverage risk,
//! same reasoning as `ui/diag.rs`.

use crate::eink::buttons::PageButton;
use crate::eink::fb::{Framebuffer, MxcfbRect, WAVEFORM_MODE_DU, WAVEFORM_MODE_GC16};
use crate::eink::input::{Input, InputEvent};
use crate::eink::touch::TouchEvent;
use crate::orientation::Orientation;
use crate::api::Book;
use crate::ui::filter::{self, Facet, Filters};
use crate::ui::sort::SortState;
use crate::ui::sortmenu;
use crate::ui::text::TextRenderer;

const STRIP_H: u32 = 120;
const MARGIN_X: u32 = 60;
/// Two fixed left zones (Back, Clear) in the value-picker strip, each this wide;
/// page nav fills the rest. Mirrors the gallery strip's Exit/Sort layout.
const ZONE_W: u32 = 200;

fn row_h(lh: u32) -> u32 {
    (lh * 2).max(96)
}
fn rows_top(lh: u32) -> u32 {
    lh * 3
}
fn strip_top(yres: u32) -> u32 {
    yres.saturating_sub(STRIP_H)
}
fn full_rect(fb: &Framebuffer) -> MxcfbRect {
    MxcfbRect { top: 0, left: 0, width: fb.var.xres, height: fb.var.yres }
}
fn row_rect(top: u32, xres: u32, rh: u32) -> MxcfbRect {
    MxcfbRect { top, left: 0, width: xres, height: rh }
}

/// Centered title in the top gap.
fn draw_title(fb: &mut Framebuffer, renderer: &mut TextRenderer, lh: u32, title: &str) {
    let tw = renderer.measure_width(title);
    let tx = ((fb.var.xres as i32 - tw as i32) / 2).max(0);
    renderer.draw(fb, tx, (lh * 2) as i32, title, false);
}

// ===========================================================================
// Menu (facet rows + Sort row + Clear all / Done)
// ===========================================================================

enum MenuTap {
    Facet(Facet),
    Sort,
    ClearAll,
    Done,
}

/// Menu rows: `Facet::ALL` then one Sort row.
fn menu_hit(tx: u32, ty: u32, xres: u32, yres: u32, lh: u32) -> Option<MenuTap> {
    if ty >= strip_top(yres) {
        return Some(if tx < xres / 2 { MenuTap::ClearAll } else { MenuTap::Done });
    }
    let rt = rows_top(lh);
    if ty < rt {
        return None;
    }
    let row = ((ty - rt) / row_h(lh)) as usize;
    if row < Facet::ALL.len() {
        Some(MenuTap::Facet(Facet::ALL[row]))
    } else if row == Facet::ALL.len() {
        Some(MenuTap::Sort)
    } else {
        None
    }
}

fn render_menu(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    filters: &Filters,
    sort: SortState,
    lh: u32,
) {
    let xres = fb.var.xres;
    fb.fill_rect(0, 0, xres, fb.var.yres, 0xFF);
    draw_title(fb, renderer, lh, "Filter & sort");

    let rh = row_h(lh);
    for (i, facet) in Facet::ALL.iter().enumerate() {
        let row_top = rows_top(lh) + i as u32 * rh;
        let baseline = (row_top + rh * 60 / 100) as i32;
        let count = filters.count(*facet);
        let text = if count > 0 {
            format!("{}  ({})  >", facet.label(), count)
        } else {
            format!("{}  >", facet.label())
        };
        renderer.draw(fb, MARGIN_X as i32, baseline, &text, false);
    }

    // Sort row, set off by a divider so it reads as separate from the facets.
    let sort_top = rows_top(lh) + Facet::ALL.len() as u32 * rh;
    fb.fill_rect(sort_top, MARGIN_X, xres.saturating_sub(MARGIN_X * 2), 2, 0x00);
    let baseline = (sort_top + rh * 60 / 100) as i32;
    renderer.draw(fb, MARGIN_X as i32, baseline, &format!("Sort:  {}", sort.header()), false);

    // [ Clear all | Done ] strip.
    let top = strip_top(fb.var.yres);
    let mid = xres / 2;
    fb.fill_rect(top, 0, xres, 2, 0x00);
    fb.fill_rect(top + 2, 0, xres, STRIP_H - 2, 0xFF);
    fb.fill_rect(top + 12, mid.saturating_sub(1), 2, STRIP_H - 24, 0x00);
    let baseline = (top + STRIP_H * 60 / 100) as i32;
    draw_centered_in(fb, renderer, "Clear all", 0, mid, baseline);
    draw_centered_in(fb, renderer, "[ Done ]", mid, xres, baseline);
}

/// Draw `s` horizontally centered within `[x0, x1)`.
fn draw_centered_in(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    s: &str,
    x0: u32,
    x1: u32,
    baseline: i32,
) {
    let w = renderer.measure_width(s);
    let span = x1.saturating_sub(x0);
    let x = x0 as i32 + ((span as i32 - w as i32) / 2).max(0);
    renderer.draw(fb, x, baseline, s, false);
}

/// Run the Filter & sort menu. Mutates `filters`/`sort` in place; the caller
/// snapshots them to decide whether to rebuild the view. `orient` is kept in
/// sync across nested overlays.
pub fn run(
    fb: &mut Framebuffer,
    input: &mut Input,
    renderer: &mut TextRenderer,
    all_books: &[Book],
    filters: &mut Filters,
    sort: &mut SortState,
    orient: &mut Orientation,
) -> anyhow::Result<()> {
    let lh = renderer.line_height().max(1);
    render_menu(fb, renderer, filters, *sort, lh);
    fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;

    loop {
        match input.next()? {
            InputEvent::Touch(TouchEvent::Up { x, y }) => {
                match menu_hit(x, y, fb.var.xres, fb.var.yres, lh) {
                    Some(MenuTap::Facet(f)) => {
                        value_picker(fb, input, renderer, all_books, filters, f, orient)?;
                        // The picker overwrote the screen; repaint the menu
                        // (the facet's count may have changed).
                        render_menu(fb, renderer, filters, *sort, lh);
                        fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;
                    }
                    Some(MenuTap::Sort) => {
                        *sort = sortmenu::run(fb, input, renderer, *sort, orient)?;
                        render_menu(fb, renderer, filters, *sort, lh);
                        fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;
                    }
                    Some(MenuTap::ClearAll) => {
                        filters.clear_all();
                        render_menu(fb, renderer, filters, *sort, lh);
                        fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;
                    }
                    Some(MenuTap::Done) => return Ok(()),
                    None => {}
                }
            }
            InputEvent::Touch(TouchEvent::Down { .. }) => {}
            InputEvent::Page(_) => {}
            InputEvent::Tick => {
                let o = Orientation::detect();
                if o != *orient {
                    *orient = o;
                    input.set_orientation(o);
                    render_menu(fb, renderer, filters, *sort, lh);
                    fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;
                }
            }
        }
    }
}

// ===========================================================================
// Value picker (paged checklist for one facet)
// ===========================================================================

enum PickTap {
    Toggle(usize), // index into `options`
    Back,
    Clear,
    Prev,
    Next,
}

fn per_page(lh: u32, yres: u32) -> usize {
    (strip_top(yres).saturating_sub(rows_top(lh)) / row_h(lh)).max(1) as usize
}

fn n_pages(n_options: usize, per_page: usize) -> usize {
    n_options.div_ceil(per_page).max(1)
}

fn pick_hit(
    tx: u32,
    ty: u32,
    xres: u32,
    yres: u32,
    lh: u32,
    page: usize,
    per_page: usize,
    n_options: usize,
    pages: usize,
) -> Option<PickTap> {
    if ty >= strip_top(yres) {
        if tx < ZONE_W {
            return Some(PickTap::Back);
        }
        if tx < ZONE_W * 2 {
            return Some(PickTap::Clear);
        }
        if pages <= 1 {
            return None;
        }
        return Some(if tx < xres / 2 { PickTap::Prev } else { PickTap::Next });
    }
    let rt = rows_top(lh);
    if ty < rt {
        return None;
    }
    let slot = ((ty - rt) / row_h(lh)) as usize;
    if slot >= per_page {
        return None;
    }
    let idx = page * per_page + slot;
    if idx < n_options {
        Some(PickTap::Toggle(idx))
    } else {
        None
    }
}

/// Text for one option row: `[x] value  (count)`.
fn row_text(filters: &Filters, facet: Facet, value: &str, count: usize) -> String {
    let mark = if filters.is_selected(facet, value) { "[x] " } else { "[ ] " };
    format!("{mark}{value}  ({count})")
}

fn render_pick_page(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    filters: &Filters,
    facet: Facet,
    options: &[(String, usize)],
    page: usize,
    per_page: usize,
    pages: usize,
    lh: u32,
) {
    let xres = fb.var.xres;
    fb.fill_rect(0, 0, xres, fb.var.yres, 0xFF);
    let count = filters.count(facet);
    let title = if count > 0 {
        format!("{}  ({} selected)", facet.label(), count)
    } else {
        facet.label().to_string()
    };
    draw_title(fb, renderer, lh, &title);

    let rh = row_h(lh);
    let start = page * per_page;
    let end = (start + per_page).min(options.len());
    for (slot, idx) in (start..end).enumerate() {
        let (value, c) = &options[idx];
        let row_top = rows_top(lh) + slot as u32 * rh;
        let baseline = (row_top + rh * 60 / 100) as i32;
        renderer.draw(fb, MARGIN_X as i32, baseline, &row_text(filters, facet, value, *c), false);
    }

    // [ Back | Clear | ← Prev  N/M  Next → ] strip.
    let top = strip_top(fb.var.yres);
    fb.fill_rect(top, 0, xres, 2, 0x00);
    fb.fill_rect(top + 2, 0, xres, STRIP_H - 2, 0xFF);
    fb.fill_rect(top + 12, ZONE_W - 2, 2, STRIP_H - 24, 0x00);
    fb.fill_rect(top + 12, ZONE_W * 2 - 2, 2, STRIP_H - 24, 0x00);
    let baseline = (top + STRIP_H * 60 / 100) as i32;
    draw_centered_in(fb, renderer, "[ Back ]", 0, ZONE_W, baseline);
    draw_centered_in(fb, renderer, "Clear", ZONE_W, ZONE_W * 2, baseline);
    if pages > 1 {
        if page > 0 {
            renderer.draw(fb, ZONE_W as i32 * 2 + 40, baseline, "← Prev", false);
        }
        let mid = format!("{} / {}", page + 1, pages);
        draw_centered_in(fb, renderer, &mid, ZONE_W * 2, xres, baseline);
        if page + 1 < pages {
            let next = "Next →";
            let nw = renderer.measure_width(next);
            renderer.draw(fb, xres as i32 - 80 - nw as i32, baseline, next, false);
        }
    }
}

/// Re-draw a single option row in place + DU refresh — a toggle shouldn't flash
/// the whole screen.
fn redraw_pick_row(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    filters: &Filters,
    facet: Facet,
    options: &[(String, usize)],
    idx: usize,
    page: usize,
    per_page: usize,
    lh: u32,
) -> anyhow::Result<()> {
    let slot = idx - page * per_page;
    let rh = row_h(lh);
    let row_top = rows_top(lh) + slot as u32 * rh;
    fb.fill_rect(row_top, 0, fb.var.xres, rh, 0xFF);
    let (value, c) = &options[idx];
    let baseline = (row_top + rh * 60 / 100) as i32;
    renderer.draw(fb, MARGIN_X as i32, baseline, &row_text(filters, facet, value, *c), false);
    fb.send_update(row_rect(row_top, fb.var.xres, rh), WAVEFORM_MODE_DU)?;
    Ok(())
}

/// Paged checklist for one facet. Mutates `filters` in place. The option list +
/// counts are computed once on entry and stay fixed while toggling: a facet's
/// own options are leave-one-out (independent of its own selections — see
/// `filter::facet_options`), so only the checkmarks change per tap.
fn value_picker(
    fb: &mut Framebuffer,
    input: &mut Input,
    renderer: &mut TextRenderer,
    all_books: &[Book],
    filters: &mut Filters,
    facet: Facet,
    orient: &mut Orientation,
) -> anyhow::Result<()> {
    let options = filter::facet_options(all_books, filters, facet);
    let lh = renderer.line_height().max(1);
    let pp = per_page(lh, fb.var.yres);
    let pages = n_pages(options.len(), pp);
    let mut page = 0usize;

    render_pick_page(fb, renderer, filters, facet, &options, page, pp, pages, lh);
    fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;

    loop {
        match input.next()? {
            InputEvent::Touch(TouchEvent::Up { x, y }) => {
                match pick_hit(x, y, fb.var.xres, fb.var.yres, lh, page, pp, options.len(), pages) {
                    Some(PickTap::Toggle(idx)) => {
                        filters.toggle(facet, &options[idx].0);
                        redraw_pick_row(fb, renderer, filters, facet, &options, idx, page, pp, lh)?;
                    }
                    Some(PickTap::Clear) => {
                        filters.clear_facet(facet);
                        render_pick_page(fb, renderer, filters, facet, &options, page, pp, pages, lh);
                        fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;
                    }
                    Some(PickTap::Prev) => {
                        if page > 0 {
                            page -= 1;
                            render_pick_page(fb, renderer, filters, facet, &options, page, pp, pages, lh);
                            fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;
                        }
                    }
                    Some(PickTap::Next) => {
                        if page + 1 < pages {
                            page += 1;
                            render_pick_page(fb, renderer, filters, facet, &options, page, pp, pages, lh);
                            fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;
                        }
                    }
                    Some(PickTap::Back) => return Ok(()),
                    None => {}
                }
            }
            InputEvent::Touch(TouchEvent::Down { .. }) => {}
            InputEvent::Page(pb) => {
                let new_page = match pb {
                    PageButton::Prev => page.saturating_sub(1),
                    PageButton::Next => (page + 1).min(pages.saturating_sub(1)),
                };
                if new_page != page {
                    page = new_page;
                    render_pick_page(fb, renderer, filters, facet, &options, page, pp, pages, lh);
                    fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;
                }
            }
            InputEvent::Tick => {
                let o = Orientation::detect();
                if o != *orient {
                    *orient = o;
                    input.set_orientation(o);
                    render_pick_page(fb, renderer, filters, facet, &options, page, pp, pages, lh);
                    fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;
                }
            }
        }
    }
}
