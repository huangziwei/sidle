//! Bottom-strip toolbar.
//!
//! Always-visible 80px strip at the bottom of the panel with tap zones,
//! left-to-right:
//!
//! - `Exit` — always shown, fixed-width left zone.
//! - `Filter` — always shown, fixed-width zone; opens the filter & sort menu,
//!   shows `(N)` when N facets are active.
//! - `Sync` — always shown, fixed-width zone; pushes this device's
//!   `.yjr`/`.yjf` + My Clippings.txt to sidle-server (the LAN twin of a USB
//!   annotation sync).
//! - `← Prev / N / Next →` — the remaining width, split in half: left half pages
//!   back, right half pages forward (shown only when n_pages > 1). Touch nav is
//!   essential on the Paperwhite, which has no bezel page buttons.
//!
//! Replaces the earlier hidden top-left-corner quit gesture, which was
//! ambiguous with stray touch events near the panel edge.

use crate::eink::fb::Framebuffer;
use crate::ui::text::TextRenderer;

pub const STRIP_H: u32 = 80;
pub const PAGE_SIZE: usize = 9;

const EXIT_ZONE_W: u32 = 200;
/// Filter zone sits immediately right of Exit, same fixed-width pattern.
const FILTER_ZONE_W: u32 = 220;
/// Sync zone sits right of Filter, same pattern. The page nav
/// (Prev/mid/Next) gets whatever width is left.
const SYNC_ZONE_W: u32 = 200;
/// Left edge of the Sync zone (right after Exit + Filter).
const SYNC_LEFT: u32 = EXIT_ZONE_W + FILTER_ZONE_W;
/// Left edge of the page-nav region (after Exit + Filter + Sync).
const NAV_LEFT: u32 = EXIT_ZONE_W + FILTER_ZONE_W + SYNC_ZONE_W;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagerHit {
    Exit,
    Filter,
    /// Push this device's reading-state sidecars to sidle-server.
    Sync,
    /// Page back / forward — the nav region's left / right half (see `hit`).
    /// Touch nav is the only paging on the Paperwhite (no bezel buttons).
    Prev,
    Next,
}

pub fn n_pages(books: usize) -> usize {
    // `.max(1)` keeps an empty library on a single (empty) page.
    books.div_ceil(PAGE_SIZE).max(1)
}

pub fn strip_top(fb_yres: u32) -> u32 {
    fb_yres.saturating_sub(STRIP_H)
}

pub fn hit(tx: u32, ty: u32, fb_xres: u32, fb_yres: u32, total_pages: usize) -> Option<PagerHit> {
    if ty < strip_top(fb_yres) {
        return None;
    }
    // Exit, Filter, and Sync take the three leftmost fixed slices; the rest of
    // the strip is the page-nav zone, live only when there's somewhere to go.
    if tx < EXIT_ZONE_W {
        return Some(PagerHit::Exit);
    }
    if tx < SYNC_LEFT {
        return Some(PagerHit::Filter);
    }
    if tx < NAV_LEFT {
        return Some(PagerHit::Sync);
    }
    if total_pages <= 1 {
        return None;
    }
    // Split the NAV REGION (NAV_LEFT..xres) in half: left = Prev, right = Next.
    // The bug was splitting at `fb_xres / 2` (~632px) — the whole *screen's*
    // midpoint, which sits just left of NAV_LEFT (620) on the 1264px panel, so
    // the Prev zone was a ~12px sliver and every nav tap fell through to Next.
    // Splitting the region itself gives two real halves on any panel width.
    let nav_mid = (NAV_LEFT + fb_xres) / 2;
    if tx < nav_mid {
        Some(PagerHit::Prev)
    } else {
        Some(PagerHit::Next)
    }
}

pub fn draw(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    page: usize,
    total_pages: usize,
    filter_count: usize,
) {
    let strip_y = strip_top(fb.var.yres);
    // 2px black divider, white strip body below.
    fb.fill_rect(strip_y, 0, fb.var.xres, 2, 0x00);
    fb.fill_rect(strip_y + 2, 0, fb.var.xres, STRIP_H - 2, 0xFF);

    let baseline = (strip_y + STRIP_H * 70 / 100) as i32;

    // Exit on the left. Always visible.
    renderer.draw(fb, 40, baseline, "Exit", false);
    // Vertical separator after exit zone.
    fb.fill_rect(strip_y + 12, EXIT_ZONE_W - 2, 2, STRIP_H - 24, 0x00);

    // Filter zone, right of Exit. Always visible (filter/sort work on a single
    // page too). Shows `(N)` when N facets are active so a filtered state is
    // obvious; the active sort key/dir lives in the grid header.
    let filter_label = if filter_count > 0 {
        format!("Filter ({filter_count})")
    } else {
        "Filter".to_string()
    };
    renderer.draw(fb, EXIT_ZONE_W as i32 + 40, baseline, &filter_label, false);
    fb.fill_rect(strip_y + 12, SYNC_LEFT - 2, 2, STRIP_H - 24, 0x00);

    // Sync zone, right of Filter. Always visible — pushes annotations to the Mac.
    renderer.draw(fb, SYNC_LEFT as i32 + 40, baseline, "Sync", false);
    fb.fill_rect(strip_y + 12, NAV_LEFT - 2, 2, STRIP_H - 24, 0x00);

    if total_pages <= 1 {
        return;
    }

    let label_prev = "← Prev";
    let label_next = "Next →";
    let label_mid = format!("{} / {}", page + 1, total_pages);

    // Prev = left half of the nav region, Next = right half (see `hit`). Show
    // each label only when that direction exists, so a dead edge reads as dead.
    if page > 0 {
        renderer.draw(fb, NAV_LEFT as i32 + 40, baseline, label_prev, false);
    }
    // Center "N / M" in the nav region (`NAV_LEFT`..xres, NOT the whole screen —
    // screen-centering shoved it left against the Sync separator once Sync
    // widened the fixed zones).
    let mid_w = renderer.measure_width(&label_mid);
    let mid_x = (NAV_LEFT as i32 + fb.var.xres as i32) / 2 - mid_w as i32 / 2;
    renderer.draw(fb, mid_x, baseline, &label_mid, false);
    if page + 1 < total_pages {
        let next_w = renderer.measure_width(label_next);
        let next_x = fb.var.xres as i32 - 80 - next_w as i32;
        renderer.draw(fb, next_x, baseline, label_next, false);
    }
}
