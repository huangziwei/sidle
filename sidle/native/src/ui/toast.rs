//! Modal status overlay.
//!
//! Black banner centered on the panel with white text — used during downloads
//! ("Downloading…", "Downloaded", "Failed"). A message is laid out one row per
//! `\n`-delimited line: callers compose results by joining clauses with `\n`,
//! and a newline handed to the renderer instead would draw as the
//! missing-glyph box (no face has U+000A). Returns the dirty rect so the
//! caller can refresh just that area.
//!
//! [`draw_download`] is the taller live variant: a title line, a
//! `transferred / total` progress line, and a tappable Cancel button.
//! [`draw_progress`] is the batch-step variant: a title, an `n / total` count,
//! and a filled progress bar. [`draw_progress_stop`] adds a Stop button to it —
//! the DRM Decrypt-All step indicator, which can be told to end after the book
//! it is on.

use crate::eink::fb::{Framebuffer, MxcfbRect};
use crate::ui::text::TextRenderer;

const BANNER_HEIGHT: u32 = 140;
const BANNER_MARGIN_X: u32 = 80;
/// Breathing room above and below the text block, and so the least height a
/// banner may take beyond the block it holds. Only a message taller than
/// [`BANNER_HEIGHT`] is affected: everything shorter keeps the one-line
/// footprint, which is what lets one plain toast overwrite another exactly.
const BANNER_PAD_Y: u32 = 20;

/// Taller banner for the live download overlay — fits title + progress + the
/// Cancel button with breathing room.
const DL_BANNER_HEIGHT: u32 = 300;

/// Banner for the batch-progress overlay ([`draw_progress`]) — fits a title, an
/// `n / total` count, the progress bar, and the Stop button
/// [`draw_progress_stop`] adds under it. Same footprint as the download
/// overlay, so the two never leave each other's edges behind.
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
    // At least the one-line footprint, so the ordinary toast still overwrites
    // the overlay it replaces exactly; taller only when the text needs it,
    // which beats spilling white rows outside the black box.
    let banner_h = BANNER_HEIGHT.max(block_height(renderer, message) + BANNER_PAD_Y * 2);
    let banner_w = fb.var.xres.saturating_sub(BANNER_MARGIN_X * 2);
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
/// line, each row centered on its own width. White-on-black, matching every
/// banner here. Shared so the plain toast and the download overlay's terminal
/// banner lay text out identically and differ only in footprint.
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

/// Terminal state of the live download overlay. Reuses [`draw_download`]'s
/// banner footprint (same width, height, position) so it paints directly over
/// the running progress/Cancel overlay and leaves none of its edges behind —
/// the "Downloaded"/"Failed" result and the live overlay are one banner in one
/// place, not a smaller toast stacked inside the taller one. `message` is
/// centered as a block, one row per `\n`-delimited line (so multi-line results
/// like the token-mismatch hint read as real lines, not a stray glyph), with no
/// Cancel button. Returns the banner's dirty rect.
pub fn draw_download_done(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    message: &str,
) -> MxcfbRect {
    let banner_w = fb.var.xres.saturating_sub(BANNER_MARGIN_X * 2);
    let banner_x = (fb.var.xres - banner_w) / 2;
    let banner_y = (fb.var.yres.saturating_sub(DL_BANNER_HEIGHT)) / 2;

    fb.fill_rect(banner_y, banner_x, banner_w, DL_BANNER_HEIGHT, 0x00);
    draw_message_block(
        fb,
        renderer,
        banner_x,
        banner_y,
        banner_w,
        DL_BANNER_HEIGHT,
        message,
    );

    MxcfbRect {
        top: banner_y,
        left: banner_x,
        width: banner_w,
        height: DL_BANNER_HEIGHT,
    }
}

/// Batch-progress overlay: a `title` line, an `n / total` count line, and a
/// progress bar filled to `done / total`. White-on-black to match the other
/// banners, and with no button — [`draw_progress_stop`] is the variant that
/// offers one, and painting this over it is what erases it. Returns the
/// banner's dirty rect. `total == 0` draws an empty track (no divide-by-zero).
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
///
/// The label carries the contract, because the engine itself cannot be
/// interrupted: a decrypt already in flight runs to its end, and the batch is
/// what stops. The caller polls for the tap between the engine's exit checks.
pub fn draw_progress_stop(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    title: &str,
    done: usize,
    total: usize,
) -> (MxcfbRect, MxcfbRect) {
    let banner = progress_body(fb, renderer, title, done, total);

    // Filled white box with a black label, mirroring the download Cancel.
    let stop_x = banner.left + (banner.width.saturating_sub(STOP_W)) / 2;
    let stop_y = banner.top + PROGRESS_BANNER_HEIGHT - STOP_H - 12;
    fb.fill_rect(stop_y, stop_x, STOP_W, STOP_H, 0xFF);

    let label = "Stop after this book";
    let lw = renderer.measure_width(label);
    let lx = stop_x as i32 + ((STOP_W as i32 - lw as i32) / 2).max(0);
    renderer.draw(fb, lx, (stop_y + STOP_H * 66 / 100) as i32, label, false);

    let stop_rect = MxcfbRect {
        top: stop_y,
        left: stop_x,
        width: STOP_W,
        height: STOP_H,
    };
    (banner, stop_rect)
}

/// The banner both progress variants share: title, count, bar. The bar hangs
/// off the top of the banner, leaving the space beneath it for the Stop button
/// [`draw_progress_stop`] adds there — so painting a [`draw_progress`] over a
/// [`draw_progress_stop`] blacks the button out and moves nothing else.
fn progress_body(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    title: &str,
    done: usize,
    total: usize,
) -> MxcfbRect {
    let banner_w = fb.var.xres.saturating_sub(BANNER_MARGIN_X * 2);
    let banner_x = (fb.var.xres - banner_w) / 2;
    let banner_y = (fb.var.yres.saturating_sub(PROGRESS_BANNER_HEIGHT)) / 2;

    fb.fill_rect(banner_y, banner_x, banner_w, PROGRESS_BANNER_HEIGHT, 0x00);

    let centered = |renderer: &mut TextRenderer, s: &str| -> i32 {
        let w = renderer.measure_width(s);
        banner_x as i32 + ((banner_w as i32 - w as i32) / 2).max(0)
    };

    // Title + count, white-on-black, stacked in the upper half.
    let tx = centered(renderer, title);
    renderer.draw(fb, tx, (banner_y + 66) as i32, title, true);
    let count = format!("{done} / {total}");
    let cx = centered(renderer, &count);
    renderer.draw(fb, cx, (banner_y + 126) as i32, &count, true);

    // Progress track: a white outline, filled white to `done / total`.
    let bar_x = banner_x + PROGRESS_BAR_INSET;
    let bar_w = banner_w.saturating_sub(PROGRESS_BAR_INSET * 2);
    let bar_y = banner_y + 156;
    const T: u32 = 3;
    fb.fill_rect(bar_y, bar_x, bar_w, T, 0xFF); // top
    fb.fill_rect(bar_y + PROGRESS_BAR_H - T, bar_x, bar_w, T, 0xFF); // bottom
    fb.fill_rect(bar_y, bar_x, T, PROGRESS_BAR_H, 0xFF); // left
    fb.fill_rect(bar_y, bar_x + bar_w - T, T, PROGRESS_BAR_H, 0xFF); // right
    if total > 0 {
        let inner_w = bar_w.saturating_sub(T * 2);
        // u64 math: inner_w·done can overflow u32 on a wide panel / many books.
        let fill_w = (inner_w as u64 * done as u64 / total as u64) as u32;
        if fill_w > 0 {
            fb.fill_rect(bar_y + T, bar_x + T, fill_w, PROGRESS_BAR_H - T * 2, 0xFF);
        }
    }

    MxcfbRect {
        top: banner_y,
        left: banner_x,
        width: banner_w,
        height: PROGRESS_BANNER_HEIGHT,
    }
}
