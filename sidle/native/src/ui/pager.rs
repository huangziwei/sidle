//! Bottom-strip toolbar.

use crate::eink::fb::Framebuffer;
use crate::ui::scale::Scale;
use crate::ui::text::TextRenderer;

/// Design pixels at `scale::DESIGN_DPI`; [`strip_h`] puts them on the panel.
const STRIP_H: u32 = 80;

/// The strip's height on an `xres`-wide panel.
pub fn strip_h(xres: u32) -> u32 {
    Scale::of_width(xres).u(STRIP_H)
}

/// The inset a strip label stands at, and the gap around a separator rule.
const LABEL_INSET: i32 = 40;
const RULE_INSET: u32 = 12;

const EXIT_ZONE_W: u32 = 200;
/// Filter zone sits immediately right of Exit, same fixed-width pattern.
const FILTER_ZONE_W: u32 = 220;
/// Source (library-switch) zone sits right of Filter, same pattern. The page nav
/// (Prev/mid/Next) gets whatever width is left.
const SOURCE_ZONE_W: u32 = 200;
/// Left edge of the Source zone (right after Exit + Filter).
const SOURCE_LEFT: u32 = EXIT_ZONE_W + FILTER_ZONE_W;
/// Left edge of the page-nav region (after Exit + Filter + Source).
const NAV_LEFT: u32 = EXIT_ZONE_W + FILTER_ZONE_W + SOURCE_ZONE_W;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagerHit {
    Exit,
    Filter,
    /// Pop a series drill-in back to the grouped top level. Occupies the same
    /// strip slot as `Filter` (which is moot inside one series), swapped in when
    /// drilled in — bezel buttons are Prev/Next only, so Back must be on-screen.
    Back,
    /// Library-switch button — toggle between the LAN library and on-device DRM
    /// books.
    Source,
    /// Page back / forward — the nav region's left / right half (see `hit`).
    /// Touch nav is the only paging on the Paperwhite (no bezel buttons).
    Prev,
    Next,
}

pub fn n_pages(books: usize, page_size: usize) -> usize {
    // `.max(1)` keeps an empty library on a single (empty) page; the inner one
    // also guards the divide against a degenerate layout.
    books.div_ceil(page_size.max(1)).max(1)
}

pub fn strip_top(fb_xres: u32, fb_yres: u32) -> u32 {
    fb_yres.saturating_sub(strip_h(fb_xres))
}

pub fn hit(
    tx: u32,
    ty: u32,
    fb_xres: u32,
    fb_yres: u32,
    total_pages: usize,
    drilled: bool,
) -> Option<PagerHit> {
    if ty < strip_top(fb_xres, fb_yres) {
        return None;
    }
    let s = Scale::of_width(fb_xres);
    // Exit, Filter/Back, and Source take the three leftmost fixed slices; the rest
    // of the strip is the page-nav zone, live only when there's somewhere to go.
    if tx < s.u(EXIT_ZONE_W) {
        return Some(PagerHit::Exit);
    }
    if tx < s.u(SOURCE_LEFT) {
        // Inside a drilled-in series this slot is Back (Filter is moot — you're
        // already scoped to one series); at the top level it opens the filter menu.
        return Some(if drilled {
            PagerHit::Back
        } else {
            PagerHit::Filter
        });
    }
    if tx < s.u(NAV_LEFT) {
        return Some(PagerHit::Source);
    }
    if total_pages <= 1 {
        return None;
    }
    // Split the NAV REGION (NAV_LEFT..xres) in half: left = Prev, right = Next.
    let nav_mid = (s.u(NAV_LEFT) + fb_xres) / 2;
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
    drilled: bool,
    drm_active: bool,
) {
    let s = Scale::of_width(fb.var.xres);
    let strip_h = s.u(STRIP_H);
    let rule = s.u(2);
    let inset = s.u(RULE_INSET);
    let label_inset = s.px(LABEL_INSET);
    let (exit_w, source_left, nav_left) = (s.u(EXIT_ZONE_W), s.u(SOURCE_LEFT), s.u(NAV_LEFT));
    let strip_y = strip_top(fb.var.xres, fb.var.yres);
    // A black divider, white strip body below.
    fb.fill_rect(strip_y, 0, fb.var.xres, rule, 0x00);
    fb.fill_rect(strip_y + rule, 0, fb.var.xres, strip_h - rule, 0xFF);

    let baseline = (strip_y + strip_h * 70 / 100) as i32;
    // A separator rule stands clear of the strip's edges at both ends.
    let rule_h = strip_h - inset * 2;

    // Exit on the left. Always visible.
    renderer.draw(fb, label_inset, baseline, "Exit", false);
    // Vertical separator after exit zone.
    fb.fill_rect(strip_y + inset, exit_w - rule, rule, rule_h, 0x00);

    // Filter/Back zone, right of Exit. Drilled into a series → "← Back" (reusing
    // the proven `←` glyph rather than a `‹` that may be absent from the font).
    let filter_label = if drilled {
        "← Back".to_string()
    } else if filter_count > 0 {
        format!("Filter ({filter_count})")
    } else {
        "Filter".to_string()
    };
    renderer.draw(
        fb,
        exit_w as i32 + label_inset,
        baseline,
        &filter_label,
        false,
    );
    fb.fill_rect(strip_y + inset, source_left - rule, rule, rule_h, 0x00);

    // Source (library-switch) zone, right of Filter. Always visible — toggles the
    // LAN library ↔ on-device DRM books; the label names where a tap goes.
    let source_label = if drm_active { "Library" } else { "DRM" };
    renderer.draw(
        fb,
        source_left as i32 + label_inset,
        baseline,
        source_label,
        false,
    );
    fb.fill_rect(strip_y + inset, nav_left - rule, rule, rule_h, 0x00);

    if total_pages <= 1 {
        return;
    }

    let label_prev = "← Prev";
    let label_next = "Next →";
    let label_mid = format!("{} / {}", page + 1, total_pages);

    // Prev = left half of the nav region, Next = right half (see `hit`). Show
    // each label only when that direction exists, so a dead edge reads as dead.
    if page > 0 {
        renderer.draw(
            fb,
            nav_left as i32 + label_inset,
            baseline,
            label_prev,
            false,
        );
    }
    // Center "N / M" in the nav region (`NAV_LEFT`..xres, NOT the whole screen —
    // screen-centering shoved it left against the Sync separator once Sync
    // widened the fixed zones).
    let mid_w = renderer.measure_width(&label_mid);
    let mid_x = (nav_left as i32 + fb.var.xres as i32) / 2 - mid_w as i32 / 2;
    renderer.draw(fb, mid_x, baseline, &label_mid, false);
    if page + 1 < total_pages {
        let next_w = renderer.measure_width(label_next);
        let next_x = fb.var.xres as i32 - s.px(80) - next_w as i32;
        renderer.draw(fb, next_x, baseline, label_next, false);
    }
}
