//! Sort-picker overlay.
//!
//! Full-screen list of the seven sort keys (tap to select) + a Direction toggle
//! row + a `[ Done ]` strip. Blocking sub-loop in the `ui/diag.rs` mold: a pure
//! `hit` geometry fn, a `render` that paints the whole panel, and a `run` that
//! owns input until the user taps Done — at which point the caller rebuilds the
//! view and repaints the grid.
//!
//! Refresh discipline mirrors the grid: one full GC16 on open (and on a
//! detected rotation, to clear the blank the X server leaves), but in-menu
//! selection changes repaint only the list region with a fast DU partial — no
//! full-screen flash per tap.
//!
//! Rotation is handled here, not by the main loop: while this sub-loop owns
//! input, the main loop's `Tick` re-orient path can't run, so on a `Tick` we
//! re-detect orientation, re-orient the input devices, and repaint ourselves.

use crate::eink::fb::{Framebuffer, MxcfbRect, WAVEFORM_MODE_DU, WAVEFORM_MODE_GC16};
use crate::eink::input::{Input, InputEvent};
use crate::eink::touch::TouchEvent;
use crate::orientation::Orientation;
use crate::ui::sort::{SortKey, SortState};
use crate::ui::text::TextRenderer;

/// Bottom `[ Done ]` strip height — matches `ui/diag.rs`'s generous button row.
const STRIP_H: u32 = 120;
/// Left inset for the title and row labels.
const MARGIN_X: u32 = 60;

/// What a tap resolved to.
enum Tap {
    Key(SortKey),
    Direction,
    Done,
}

/// Precomputed vertical geometry. Stable across KOA2's Up/Down (both portrait,
/// same `xres`/`yres`), but recomputed on rotation anyway in case a future
/// device reports different dims.
struct Layout {
    lh: u32,
    rows_top: u32,
    row_h: u32,
    strip_top: u32,
}

impl Layout {
    fn compute(renderer: &TextRenderer, yres: u32) -> Self {
        let lh = renderer.line_height().max(1);
        Layout {
            lh,
            rows_top: lh * 3,
            // Generous tap targets — 96px floor regardless of font size.
            row_h: (lh * 2).max(96),
            strip_top: yres.saturating_sub(STRIP_H),
        }
    }

    /// Map a finger-up to an action. The Done strip spans the full bottom width;
    /// above it, rows are `row_h` tall starting at `rows_top` — the first
    /// `SortKey::ALL.len()` are key rows, the next is the Direction toggle.
    fn hit(&self, ty: u32) -> Option<Tap> {
        if ty >= self.strip_top {
            return Some(Tap::Done);
        }
        if ty < self.rows_top {
            return None;
        }
        let row = ((ty - self.rows_top) / self.row_h) as usize;
        if row < SortKey::ALL.len() {
            Some(Tap::Key(SortKey::ALL[row]))
        } else if row == SortKey::ALL.len() {
            Some(Tap::Direction)
        } else {
            None
        }
    }

    /// The list region (key rows + direction row), refreshed with DU on an
    /// in-menu change so a tap doesn't flash the whole screen.
    fn rows_rect(&self, xres: u32) -> MxcfbRect {
        MxcfbRect {
            top: self.rows_top,
            left: 0,
            width: xres,
            height: self.strip_top.saturating_sub(self.rows_top),
        }
    }
}

fn full_rect(fb: &Framebuffer) -> MxcfbRect {
    MxcfbRect {
        top: 0,
        left: 0,
        width: fb.var.xres,
        height: fb.var.yres,
    }
}

/// Paint the whole panel into the framebuffer (no refresh — caller decides the
/// rect + waveform). White background; the selected key row is inverted
/// (black fill, white text) so the highlight needs no glyph coverage.
fn render(fb: &mut Framebuffer, renderer: &mut TextRenderer, state: SortState, layout: &Layout) {
    let xres = fb.var.xres;
    fb.fill_rect(0, 0, xres, fb.var.yres, 0xFF);

    // Centered title in the top gap above the rows.
    let title = "Sort by";
    let tw = renderer.measure_width(title);
    let tx = ((xres as i32 - tw as i32) / 2).max(0);
    renderer.draw(fb, tx, (layout.lh * 2) as i32, title, false);

    // Key rows.
    for (i, key) in SortKey::ALL.iter().enumerate() {
        let row_top = layout.rows_top + i as u32 * layout.row_h;
        let selected = state.key == *key;
        if selected {
            fb.fill_rect(row_top, 0, xres, layout.row_h, 0x00);
        }
        let baseline = (row_top + layout.row_h * 60 / 100) as i32;
        renderer.draw(fb, MARGIN_X as i32, baseline, key.label(), selected);
    }

    // Direction toggle row, set off by a divider so it reads as separate from
    // the key list.
    let dir_top = layout.rows_top + SortKey::ALL.len() as u32 * layout.row_h;
    fb.fill_rect(
        dir_top,
        MARGIN_X,
        xres.saturating_sub(MARGIN_X * 2),
        2,
        0x00,
    );
    let baseline = (dir_top + layout.row_h * 60 / 100) as i32;
    let dir_text = format!("Direction:  {} {}", state.dir.word(), state.dir.arrow());
    renderer.draw(fb, MARGIN_X as i32, baseline, &dir_text, false);

    draw_done(fb, renderer, layout);
}

/// Full-width `[ Done ]` strip at the bottom, `ui/diag.rs` style.
fn draw_done(fb: &mut Framebuffer, renderer: &mut TextRenderer, layout: &Layout) {
    let xres = fb.var.xres;
    let top = layout.strip_top;
    fb.fill_rect(top, 0, xres, 2, 0x00); // top divider
    fb.fill_rect(top + 2, 0, xres, STRIP_H - 2, 0xFF); // white body
    let label = "[ Done ]";
    let w = renderer.measure_width(label);
    let x = ((xres as i32 - w as i32) / 2).max(0);
    let baseline = (top + STRIP_H * 60 / 100) as i32;
    renderer.draw(fb, x, baseline, label, false);
}

/// Draw the sort picker seeded with `initial`, then block until Done. Returns
/// the chosen `SortState` (equal to `initial` if nothing was changed — the
/// caller no-ops a same-state result rather than rebuilding the view). `orient`
/// is kept in sync so the caller's rotation tracking stays correct.
pub fn run(
    fb: &mut Framebuffer,
    input: &mut Input,
    renderer: &mut TextRenderer,
    initial: SortState,
    orient: &mut Orientation,
) -> anyhow::Result<SortState> {
    let mut state = initial;
    let mut layout = Layout::compute(renderer, fb.var.yres);
    render(fb, renderer, state, &layout);
    fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;

    loop {
        match input.next()? {
            InputEvent::Touch(TouchEvent::Up { y, .. }) => match layout.hit(y) {
                Some(Tap::Key(k)) if state.key != k => {
                    state.key = k;
                    render(fb, renderer, state, &layout);
                    let rect = layout.rows_rect(fb.var.xres);
                    fb.send_update(rect, WAVEFORM_MODE_DU)?;
                }
                Some(Tap::Direction) => {
                    state.dir = state.dir.toggled();
                    render(fb, renderer, state, &layout);
                    let rect = layout.rows_rect(fb.var.xres);
                    fb.send_update(rect, WAVEFORM_MODE_DU)?;
                }
                Some(Tap::Done) => return Ok(state),
                // Tapping the already-selected key / no hit — nothing to do.
                Some(Tap::Key(_)) | None => {}
            },
            InputEvent::Touch(TouchEvent::Down { .. }) => {}
            InputEvent::Touch(TouchEvent::Screenshot) => {
                let _ = crate::eink::screenshot::capture(fb);
            }
            // Eight rows fit on one screen — nothing to page.
            InputEvent::Page(_) => {}
            InputEvent::Tick => {
                let o = Orientation::detect();
                if o != *orient {
                    *orient = o;
                    input.set_orientation(o);
                    layout = Layout::compute(renderer, fb.var.yres);
                    render(fb, renderer, state, &layout);
                    fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;
                }
            }
        }
    }
}
