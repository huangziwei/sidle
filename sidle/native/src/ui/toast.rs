//! Modal status overlay.
//!
//! Black banner centered on the panel with a single line of white text —
//! used during downloads ("Downloading…", "Downloaded", "Failed").
//! Returns the dirty rect so the caller can refresh just that area.
//!
//! [`draw_download`] is the taller live variant: a title line, a
//! `transferred / total` progress line, and a tappable Cancel button.

use crate::eink::fb::{Framebuffer, MxcfbRect};
use crate::ui::text::TextRenderer;

const BANNER_HEIGHT: u32 = 140;
const BANNER_MARGIN_X: u32 = 80;

/// Taller banner for the live download overlay — fits title + progress + the
/// Cancel button with breathing room.
const DL_BANNER_HEIGHT: u32 = 300;
/// Cancel button footprint. Sized for a comfortable finger target on a
/// ~300 DPI panel (the stock reader's tap targets are in this range).
const CANCEL_W: u32 = 320;
const CANCEL_H: u32 = 84;

pub fn draw(fb: &mut Framebuffer, renderer: &mut TextRenderer, message: &str) -> MxcfbRect {
    let banner_w = fb.var.xres.saturating_sub(BANNER_MARGIN_X * 2);
    let banner_x = (fb.var.xres - banner_w) / 2;
    let banner_y = (fb.var.yres - BANNER_HEIGHT) / 2;

    fb.fill_rect(banner_y, banner_x, banner_w, BANNER_HEIGHT, 0x00);

    let text_w = renderer.measure_width(message);
    let text_x = banner_x as i32 + ((banner_w as i32 - text_w as i32) / 2).max(0);
    // Baseline ~ 70% down the banner — leaves headroom for ascenders +
    // a little descender clearance.
    let baseline = (banner_y + (BANNER_HEIGHT * 70 / 100)) as i32;
    renderer.draw(fb, text_x, baseline, message, true);

    MxcfbRect {
        top: banner_y,
        left: banner_x,
        width: banner_w,
        height: BANNER_HEIGHT,
    }
}

/// Live download overlay: a `title` line, a `progress` line
/// (`transferred / total`), and a white Cancel button below them. Returns the
/// banner's dirty rect (send it to the panel) **and** the Cancel button's
/// absolute-coordinate hit rect, so the caller can test a tap against it while
/// the transfer runs. The button is white-on-black-banner (inverted from the
/// banner) so it reads as a tappable control.
pub fn draw_download(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    title: &str,
    progress: &str,
) -> (MxcfbRect, MxcfbRect) {
    let banner_w = fb.var.xres.saturating_sub(BANNER_MARGIN_X * 2);
    let banner_x = (fb.var.xres - banner_w) / 2;
    let banner_y = (fb.var.yres.saturating_sub(DL_BANNER_HEIGHT)) / 2;

    fb.fill_rect(banner_y, banner_x, banner_w, DL_BANNER_HEIGHT, 0x00);

    // Title + progress, white-on-black, stacked in the upper half.
    let centered = |renderer: &mut TextRenderer, s: &str| -> i32 {
        let w = renderer.measure_width(s);
        banner_x as i32 + ((banner_w as i32 - w as i32) / 2).max(0)
    };
    let tx = centered(renderer, title);
    renderer.draw(fb, tx, (banner_y + 74) as i32, title, true);
    let px = centered(renderer, progress);
    renderer.draw(fb, px, (banner_y + 150) as i32, progress, true);

    // Cancel button: filled white box with black label, near the bottom.
    let cancel_x = banner_x + (banner_w.saturating_sub(CANCEL_W)) / 2;
    let cancel_y = banner_y + DL_BANNER_HEIGHT - CANCEL_H - 34;
    fb.fill_rect(cancel_y, cancel_x, CANCEL_W, CANCEL_H, 0xFF);
    let label = "Cancel";
    let lw = renderer.measure_width(label);
    let lx = cancel_x as i32 + ((CANCEL_W as i32 - lw as i32) / 2).max(0);
    let lbaseline = (cancel_y + CANCEL_H * 66 / 100) as i32;
    renderer.draw(fb, lx, lbaseline, label, false);

    let banner_rect = MxcfbRect {
        top: banner_y,
        left: banner_x,
        width: banner_w,
        height: DL_BANNER_HEIGHT,
    };
    let cancel_rect = MxcfbRect {
        top: cancel_y,
        left: cancel_x,
        width: CANCEL_W,
        height: CANCEL_H,
    };
    (banner_rect, cancel_rect)
}
