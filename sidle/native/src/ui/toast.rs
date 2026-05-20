//! Modal status overlay.
//!
//! Black banner centered on the panel with a single line of white text —
//! used during downloads ("Downloading…", "Downloaded", "Failed").
//! Returns the dirty rect so the caller can refresh just that area.

use crate::eink::fb::{Framebuffer, MxcfbRect};
use crate::ui::text::TextRenderer;

const BANNER_HEIGHT: u32 = 140;
const BANNER_MARGIN_X: u32 = 80;

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
