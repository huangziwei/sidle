//! Modal status overlay.

use crate::eink::fb::{Framebuffer, MxcfbRect};
use crate::ui::scale::Scale;
use crate::ui::text::TextRenderer;

/// Every length here is a design pixel at `scale::DESIGN_DPI`; each drawing
/// function puts them on its own panel through [`Scale`].
const BANNER_HEIGHT: u32 = 140;
const BANNER_MARGIN_X: u32 = 80;
/// Breathing room above and below the text block, and the least height a banner
/// may take beyond the block it holds.
const BANNER_PAD_Y: u32 = 20;

/// Taller banner for the live download overlay — fits title + progress + the
/// Cancel button with breathing room.
const DL_BANNER_HEIGHT: u32 = 300;

/// Banner for the batch-progress overlay: title, `n / total`, bar, and the Stop
/// button [`draw_progress_stop`] adds. Same footprint as the download overlay.
const PROGRESS_BANNER_HEIGHT: u32 = DL_BANNER_HEIGHT;
/// Stop button footprint in [`draw_progress_stop`]. Wider than [`CANCEL_W`]
/// because the label is a sentence, not a word.
const STOP_W: u32 = 460;
const STOP_H: u32 = 76;
/// Horizontal inset of the progress bar from the banner's side edges.
const PROGRESS_BAR_INSET: u32 = 60;
/// Progress-bar track height.
const PROGRESS_BAR_H: u32 = 44;
/// Cancel button footprint. Sized for a comfortable finger target on a
/// ~300 DPI panel (the stock reader's tap targets are in this range).
const CANCEL_W: u32 = 320;
const CANCEL_H: u32 = 84;

pub fn draw(fb: &mut Framebuffer, renderer: &mut TextRenderer, message: &str) -> MxcfbRect {
    let s = Scale::of_width(fb.var.xres);
    // At least the one-line footprint, so the ordinary toast still overwrites
    // the overlay it replaces exactly; taller only when the text needs it,
    // which beats spilling white rows outside the black box.
    let banner_h = s
        .u(BANNER_HEIGHT)
        .max(block_height(renderer, message) + s.u(BANNER_PAD_Y) * 2);
    let banner_w = fb.var.xres.saturating_sub(s.u(BANNER_MARGIN_X) * 2);
    let banner_x = (fb.var.xres - banner_w) / 2;
    let banner_y = fb.var.yres.saturating_sub(banner_h) / 2;

    fb.fill_rect(banner_y, banner_x, banner_w, banner_h, 0x00);
    draw_message_block(
        fb, renderer, banner_x, banner_y, banner_w, banner_h, message,
    );

    MxcfbRect {
        top: banner_y,
        left: banner_x,
        width: banner_w,
        height: banner_h,
    }
}

/// Height of `message` laid out one row per line, never less than one row so
/// an empty message still reserves the line it would have drawn on.
fn block_height(renderer: &TextRenderer, message: &str) -> u32 {
    renderer.line_height() * message.lines().count().max(1) as u32
}

/// Center `message` as a block inside the banner, one row per `\n`-delimited
fn draw_message_block(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    banner_x: u32,
    banner_y: u32,
    banner_w: u32,
    banner_h: u32,
    message: &str,
) {
    let lh = renderer.line_height();
    let block_top = banner_y + banner_h.saturating_sub(block_height(renderer, message)) / 2;
    for (i, line) in message.lines().enumerate() {
        let text_w = renderer.measure_width(line);
        let text_x = banner_x as i32 + ((banner_w as i32 - text_w as i32) / 2).max(0);
        // Baseline ~72% down each line's slot — headroom for ascenders, and a
        // little descender clearance.
        let baseline = (block_top + lh * i as u32 + lh * 72 / 100) as i32;
        renderer.draw(fb, text_x, baseline, line, true);
    }
}

/// Live download overlay: a `title` line, a `progress` line
pub fn draw_download(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    title: &str,
    progress: &str,
) -> (MxcfbRect, MxcfbRect) {
    let s = Scale::of_width(fb.var.xres);
    let banner_w = fb.var.xres.saturating_sub(s.u(BANNER_MARGIN_X) * 2);
    let banner_x = (fb.var.xres - banner_w) / 2;
    let banner_y = (fb.var.yres.saturating_sub(s.u(DL_BANNER_HEIGHT))) / 2;

    fb.fill_rect(banner_y, banner_x, banner_w, s.u(DL_BANNER_HEIGHT), 0x00);

    // Title + progress, white-on-black, stacked in the upper half.
    let centered = |renderer: &mut TextRenderer, text: &str| -> i32 {
        let w = renderer.measure_width(text);
        banner_x as i32 + ((banner_w as i32 - w as i32) / 2).max(0)
    };
    let tx = centered(renderer, title);
    renderer.draw(fb, tx, (banner_y + s.u(74)) as i32, title, true);
    let px = centered(renderer, progress);
    renderer.draw(fb, px, (banner_y + s.u(150)) as i32, progress, true);

    // Cancel button: filled white box with black label, near the bottom.
    let cancel_x = banner_x + (banner_w.saturating_sub(s.u(CANCEL_W))) / 2;
    let cancel_y = banner_y + s.u(DL_BANNER_HEIGHT) - s.u(CANCEL_H) - s.u(34);
    fb.fill_rect(cancel_y, cancel_x, s.u(CANCEL_W), s.u(CANCEL_H), 0xFF);
    let label = "Cancel";
    let lw = renderer.measure_width(label);
    let lx = cancel_x as i32 + ((s.u(CANCEL_W) as i32 - lw as i32) / 2).max(0);
    let lbaseline = (cancel_y + s.u(CANCEL_H) * 66 / 100) as i32;
    renderer.draw(fb, lx, lbaseline, label, false);

    let banner_rect = MxcfbRect {
        top: banner_y,
        left: banner_x,
        width: banner_w,
        height: s.u(DL_BANNER_HEIGHT),
    };
    let cancel_rect = MxcfbRect {
        top: cancel_y,
        left: cancel_x,
        width: s.u(CANCEL_W),
        height: s.u(CANCEL_H),
    };
    (banner_rect, cancel_rect)
}

/// Terminal state of the live download overlay. Reuses [`draw_download`]'s
pub fn draw_download_done(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    message: &str,
) -> MxcfbRect {
    let s = Scale::of_width(fb.var.xres);
    let banner_w = fb.var.xres.saturating_sub(s.u(BANNER_MARGIN_X) * 2);
    let banner_x = (fb.var.xres - banner_w) / 2;
    let banner_y = (fb.var.yres.saturating_sub(s.u(DL_BANNER_HEIGHT))) / 2;

    fb.fill_rect(banner_y, banner_x, banner_w, s.u(DL_BANNER_HEIGHT), 0x00);
    draw_message_block(
        fb,
        renderer,
        banner_x,
        banner_y,
        banner_w,
        s.u(DL_BANNER_HEIGHT),
        message,
    );

    MxcfbRect {
        top: banner_y,
        left: banner_x,
        width: banner_w,
        height: s.u(DL_BANNER_HEIGHT),
    }
}

/// Batch-progress overlay: a `title` line, an `n / total` count, and a bar filled
/// to `done / total`. No button; `total == 0` draws an empty track.
pub fn draw_progress(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    title: &str,
    done: usize,
    total: usize,
) -> MxcfbRect {
    progress_body(fb, renderer, title, done, total)
}

/// [`draw_progress`] plus a Stop button. Returns the banner's dirty rect and
/// the button's hit rect.
pub fn draw_progress_stop(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    title: &str,
    done: usize,
    total: usize,
) -> (MxcfbRect, MxcfbRect) {
    let s = Scale::of_width(fb.var.xres);
    let banner = progress_body(fb, renderer, title, done, total);

    // Filled white box with a black label, mirroring the download Cancel.
    let stop_x = banner.left + (banner.width.saturating_sub(s.u(STOP_W))) / 2;
    let stop_y = banner.top + s.u(PROGRESS_BANNER_HEIGHT) - s.u(STOP_H) - s.u(12);
    fb.fill_rect(stop_y, stop_x, s.u(STOP_W), s.u(STOP_H), 0xFF);

    let label = "Stop after this book";
    let lw = renderer.measure_width(label);
    let lx = stop_x as i32 + ((s.u(STOP_W) as i32 - lw as i32) / 2).max(0);
    renderer.draw(
        fb,
        lx,
        (stop_y + s.u(STOP_H) * 66 / 100) as i32,
        label,
        false,
    );

    let stop_rect = MxcfbRect {
        top: stop_y,
        left: stop_x,
        width: s.u(STOP_W),
        height: s.u(STOP_H),
    };
    (banner, stop_rect)
}

/// The banner both progress variants share: title, count, bar. The bar hangs
fn progress_body(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    title: &str,
    done: usize,
    total: usize,
) -> MxcfbRect {
    let s = Scale::of_width(fb.var.xres);
    let banner_w = fb.var.xres.saturating_sub(s.u(BANNER_MARGIN_X) * 2);
    let banner_x = (fb.var.xres - banner_w) / 2;
    let banner_y = (fb.var.yres.saturating_sub(s.u(PROGRESS_BANNER_HEIGHT))) / 2;

    fb.fill_rect(
        banner_y,
        banner_x,
        banner_w,
        s.u(PROGRESS_BANNER_HEIGHT),
        0x00,
    );

    let centered = |renderer: &mut TextRenderer, text: &str| -> i32 {
        let w = renderer.measure_width(text);
        banner_x as i32 + ((banner_w as i32 - w as i32) / 2).max(0)
    };

    // Title + count, white-on-black, stacked in the upper half.
    let tx = centered(renderer, title);
    renderer.draw(fb, tx, (banner_y + s.u(66)) as i32, title, true);
    let count = format!("{done} / {total}");
    let cx = centered(renderer, &count);
    renderer.draw(fb, cx, (banner_y + s.u(126)) as i32, &count, true);

    // Progress track: a white outline, filled white to `done / total`.
    let bar_x = banner_x + s.u(PROGRESS_BAR_INSET);
    let bar_w = banner_w.saturating_sub(s.u(PROGRESS_BAR_INSET) * 2);
    let bar_y = banner_y + s.u(156);
    /// Progress-track stroke, in design pixels.
    const T_DESIGN: u32 = 3;
    let t = s.u(T_DESIGN);
    fb.fill_rect(bar_y, bar_x, bar_w, t, 0xFF); // top
    fb.fill_rect(bar_y + s.u(PROGRESS_BAR_H) - t, bar_x, bar_w, t, 0xFF); // bottom
    fb.fill_rect(bar_y, bar_x, t, s.u(PROGRESS_BAR_H), 0xFF); // left
    fb.fill_rect(bar_y, bar_x + bar_w - t, t, s.u(PROGRESS_BAR_H), 0xFF); // right
    if total > 0 {
        let inner_w = bar_w.saturating_sub(t * 2);
        // u64 math: inner_w·done can overflow u32 on a wide panel / many books.
        let fill_w = (inner_w as u64 * done as u64 / total as u64) as u32;
        if fill_w > 0 {
            fb.fill_rect(
                bar_y + t,
                bar_x + t,
                fill_w,
                s.u(PROGRESS_BAR_H) - t * 2,
                0xFF,
            );
        }
    }

    MxcfbRect {
        top: banner_y,
        left: banner_x,
        width: banner_w,
        height: s.u(PROGRESS_BANNER_HEIGHT),
    }
}
