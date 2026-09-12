//! The search overlay. [`crate::keyboard`] supplies the keys: a commit arrives
//! over [`crate::lipc`], every other key as a keysym on
//! [`crate::eink::fb::Pump`].

use anyhow::Result;

use crate::api::Book;
use crate::eink::fb::{Framebuffer, MxcfbRect, WAVEFORM_MODE_DU, WAVEFORM_MODE_GC16};
use crate::eink::input::{Input, InputEvent};
use crate::eink::keysym::{Typed, of_keysym};
use crate::eink::touch::TouchEvent;
use crate::lipc::Service;
use crate::orientation::Orientation;
use crate::search;
use crate::ui::filter::{self, Filters};
use crate::ui::scale::Scale;
use crate::ui::searchbar;
use crate::ui::text::TextRenderer;

/// The foot strip holding `[ Back ]` and `[ Search ]`, and the width of each
/// slot. Design pixels at `scale::DESIGN_DPI`.
const STRIP_H: u32 = 120;
const ZONE_W: u32 = 200;
const RULE: u32 = 2;
/// The gap under the match count.
const BAND_GAP: u32 = 24;

/// The query, and the run an IME is composing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Query {
    text: String,
    /// Drawn after `text` and matched on by nothing.
    preedit: String,
}

impl Query {
    /// Takes `said` onto `text`, clearing `preedit`.
    fn commit(&mut self, said: &str) {
        self.preedit.clear();
        self.text.push_str(said);
    }

    /// Takes `count` characters off the end of `text`.
    fn delete(&mut self, count: usize) {
        for _ in 0..count {
            self.text.pop();
        }
    }

    /// `keyboardCommit` carries the text; `keyboardSetPreeditString`
    /// `position:str`; `keyboardDelete` `before:after`; `keyboardReplace`
    /// `before:after:str`. Answers whether the query moved.
    fn set(&mut self, property: &str, value: &str) -> bool {
        match property {
            "keyboardCommit" => self.commit(value),
            "keyboardSetPreeditString" => {
                let (_, said) = value.split_once(':').unwrap_or(("", value));
                said.clone_into(&mut self.preedit);
            }
            "keyboardDelete" => {
                let (before, _) = value.split_once(':').unwrap_or((value, ""));
                self.delete(before.parse().unwrap_or(0));
            }
            "keyboardReplace" => {
                let mut parts = value.splitn(3, ':');
                let before = parts.next().unwrap_or_default().parse().unwrap_or(0);
                let said = parts.nth(1).unwrap_or_default().to_string();
                self.delete(before);
                self.commit(&said);
            }
            _ => return false,
        }
        true
    }

    /// `text` with `preedit` after it.
    fn shown(&self) -> String {
        format!("{}{}", self.text, self.preedit)
    }
}

/// Where the overlay's rows sit on an `xres` by `yres` panel.
#[derive(Debug, Clone, Copy)]
struct Layout {
    /// Bottom of the band holding the search bar and the match count. A
    /// keystroke refreshes `[0, band_bottom]` alone.
    band_bottom: u32,
    /// Top of the foot strip, and of the keyboard over it.
    strip_top: u32,
    keyboard_top: u32,
    strip_h: u32,
    zone_w: u32,
    rule: u32,
}

impl Layout {
    fn compute(lh: u32, xres: u32, yres: u32) -> Self {
        let s = Scale::of_width(xres);
        let strip_h = s.u(STRIP_H);
        let keyboard_h = crate::keyboard::height(yres as i32).clamp(0, yres as i32) as u32;
        Self {
            band_bottom: searchbar::below(xres) + lh + s.u(BAND_GAP),
            strip_top: yres.saturating_sub(strip_h),
            keyboard_top: yres.saturating_sub(keyboard_h.max(strip_h)),
            strip_h,
            zone_w: s.u(ZONE_W).min(xres / 5),
            rule: s.u(RULE),
        }
    }

    /// Which foot-strip slot `(x, y)` lands on.
    fn hit(&self, x: u32, y: u32, xres: u32) -> Option<Tap> {
        if y < self.strip_top {
            return None;
        }
        if x < self.zone_w {
            return Some(Tap::Back);
        }
        (x >= xres.saturating_sub(self.zone_w)).then_some(Tap::Search)
    }
}

/// A tap on the foot strip.
enum Tap {
    /// Answers the query the overlay opened with.
    Back,
    /// Answers what has been typed.
    Search,
}

/// What a batch of keysyms did.
enum Act {
    Search,
    Back,
    Moved,
    Nothing,
}

/// Takes `keysyms` into `query`. `Escape` and `Enter` drop the rest of them.
fn typed(query: &mut Query, keysyms: &[u32]) -> Act {
    let mut moved = false;
    for keysym in keysyms {
        let Some(said) = of_keysym(*keysym) else {
            continue;
        };
        match said {
            Typed::Char(said) => {
                query.text.push(said);
                moved = true;
            }
            Typed::Backspace => moved |= query.text.pop().is_some(),
            Typed::Enter => return Act::Search,
            Typed::Escape => return Act::Back,
        }
    }
    match moved {
        true => Act::Moved,
        false => Act::Nothing,
    }
}

/// Takes every [`Service::drain`] set into `query`, answering whether it
/// moved.
fn committed(service: Option<&mut Service>, query: &mut Query) -> bool {
    let Some(service) = service else {
        return false;
    };
    let mut moved = false;
    for set in service.drain() {
        moved |= query.set(&set.property, &set.value);
    }
    moved
}

/// The books `query` names, under the filters in force.
fn count_matches(all_books: &[Book], filters: &Filters, query: &str) -> usize {
    all_books
        .iter()
        .filter(|b| filter::matches(b, filters, None) && search::matches(b, query))
        .count()
}

/// `searchbar::draw` and the [`count_matches`] line below it. The caller
/// white-fills the band first.
fn draw_band(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    all_books: &[Book],
    filters: &Filters,
    query: &Query,
    lh: u32,
) {
    let xres = fb.var.xres;
    searchbar::draw(fb, renderer, &query.shown(), false);

    let n = count_matches(all_books, filters, &query.text);
    let count = if query.text.trim().is_empty() {
        format!("{n} books")
    } else if n == 0 {
        "no matches".to_string()
    } else if n == 1 {
        "1 match".to_string()
    } else {
        format!("{n} matches")
    };
    let cw = renderer.measure_width(&count);
    let cy = (searchbar::below(xres) + lh) as i32;
    renderer.draw(
        fb,
        ((xres as i32 - cw as i32) / 2).max(0),
        cy,
        &count,
        false,
    );
}

/// A rule, then `[ Back ]` at the left end and `[ Search ]` at the right.
fn draw_strip(fb: &mut Framebuffer, renderer: &mut TextRenderer, layout: &Layout) {
    let xres = fb.var.xres;
    let top = layout.strip_top;
    fb.fill_rect(top, 0, xres, layout.rule, 0x00);
    fb.fill_rect(
        top + layout.rule,
        0,
        xres,
        layout.strip_h - layout.rule,
        0xFF,
    );
    let baseline = (top + layout.strip_h * 62 / 100) as i32;
    for (label, from, to) in [
        ("[ Back ]", 0, layout.zone_w),
        ("[ Search ]", xres.saturating_sub(layout.zone_w), xres),
    ] {
        let w = renderer.measure_width(label);
        let x = from as i32 + ((to - from) as i32 - w as i32) / 2;
        renderer.draw(fb, x.max(from as i32), baseline, label, false);
    }
}

/// The band and the strip, over `[0, keyboard_top]`.
fn render(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    all_books: &[Book],
    filters: &Filters,
    query: &Query,
    layout: &Layout,
    lh: u32,
) {
    fb.fill_rect(0, 0, fb.var.xres, layout.keyboard_top, 0xFF);
    draw_band(fb, renderer, all_books, filters, query, lh);
    draw_strip(fb, renderer, layout);
}

fn band_rect(fb: &Framebuffer, layout: &Layout) -> MxcfbRect {
    MxcfbRect {
        top: 0,
        left: 0,
        width: fb.var.xres,
        height: layout.band_bottom,
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

/// Answers what was typed, on `[ Search ]` or Enter; `initial` on `[ Back ]` or
/// Escape. `initial` pre-fills the field.
pub fn run(
    fb: &mut Framebuffer,
    input: &mut Input,
    renderer: &mut TextRenderer,
    all_books: &[Book],
    filters: &Filters,
    initial: &str,
    orient: &mut Orientation,
) -> Result<String> {
    // A `Service` that will not open leaves the keysym path, which needs none
    // of it.
    let mut service = match Service::open(crate::keyboard::CLIENT) {
        Ok(service) => {
            eprintln!("lipc: {} is open", service.name());
            Some(service)
        }
        Err(err) => {
            eprintln!("?? lipc: {err:#} — Latin typing only");
            None
        }
    };
    // `EVIOCGRAB` is exclusive against the keyboard's own window.
    input.set_keyboard(true);
    crate::keyboard::open();
    let out = drive(
        fb,
        input,
        renderer,
        all_books,
        filters,
        &mut service,
        initial,
        orient,
    );
    crate::keyboard::close();
    // The socket closes with `service`; a descriptor left in `watched`
    // outlives it.
    input.watch([None, None]);
    input.set_keyboard(false);
    input.retake();
    out
}

#[allow(clippy::too_many_arguments)] // one overlay's state, positional
fn drive(
    fb: &mut Framebuffer,
    input: &mut Input,
    renderer: &mut TextRenderer,
    all_books: &[Book],
    filters: &Filters,
    service: &mut Option<Service>,
    initial: &str,
    orient: &mut Orientation,
) -> Result<String> {
    let lh = renderer.line_height().max(1);
    let mut query = Query {
        text: initial.to_string(),
        preedit: String::new(),
    };
    let mut layout = Layout::compute(lh, fb.var.xres, fb.var.yres);
    render(fb, renderer, all_books, filters, &query, &layout, lh);
    fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;

    // DU, not GC16: a keystroke must not flash the panel.
    macro_rules! refresh_band {
        () => {{
            fb.fill_rect(0, 0, fb.var.xres, layout.band_bottom, 0xFF);
            draw_band(fb, renderer, all_books, filters, &query, lh);
            fb.send_update(band_rect(fb, &layout), WAVEFORM_MODE_DU)?;
        }};
    }

    loop {
        // A `KeyPress` arrives on the X connection and a commit on the lipc
        // socket, neither of them an input device.
        input.watch([Some(fb.raw_fd()), service.as_ref().map(|s| s.raw_fd())]);
        match input.next()? {
            InputEvent::Touch(TouchEvent::Up { x, y }) => {
                // `keyboard_top` down belongs to the keyboard's own window.
                if y >= layout.keyboard_top {
                    continue;
                }
                let clearable = !query.text.is_empty();
                if let Some(searchbar::Tap::Clear) =
                    searchbar::hit(x, y, fb.var.xres, clearable, false, false)
                {
                    query = Query::default();
                    refresh_band!();
                    continue;
                }
                match layout.hit(x, y, fb.var.xres) {
                    Some(Tap::Search) => return Ok(query.text),
                    Some(Tap::Back) => return Ok(initial.to_string()),
                    None => {}
                }
            }
            InputEvent::Touch(TouchEvent::Down { .. }) => {}
            InputEvent::Touch(TouchEvent::Screenshot) => {
                let _ = crate::eink::screenshot::capture(fb);
            }
            InputEvent::Page(_) => {}
            InputEvent::Tick => {
                let pump = fb.pump_events();
                if let Some(covered) = pump.covered {
                    input.set_covered(covered);
                }
                input.retake();
                let moved = match typed(&mut query, &pump.typed) {
                    Act::Search => return Ok(query.text),
                    Act::Back => return Ok(initial.to_string()),
                    Act::Moved => true,
                    Act::Nothing => false,
                } | committed(service.as_mut(), &mut query);

                let turned = input.follow_orientation();
                if turned {
                    *orient = input.orientation();
                }
                if turned || pump.resized.is_some() {
                    layout = Layout::compute(lh, fb.var.xres, fb.var.yres);
                }
                if pump.covered == Some(true) {
                    continue;
                }
                if turned || pump.resized.is_some() || pump.repaint || pump.covered.is_some() {
                    render(fb, renderer, all_books, filters, &query, &layout, lh);
                    fb.send_update(full_rect(fb), WAVEFORM_MODE_GC16)?;
                } else if moved {
                    refresh_band!();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shipped framebuffer.
    const PANELS: [(u32, u32); 4] = [(600, 800), (758, 1024), (1264, 1680), (1860, 2480)];

    /// [`Query::set`] over the four properties `kb` sets.
    #[test]
    fn the_candidate_engine_commits_over_the_service() {
        let mut q = Query::default();
        assert!(q.set("keyboardSetPreeditString", "0:ゆめ"));
        assert_eq!((q.text.as_str(), q.preedit.as_str()), ("", "ゆめ"));
        assert_eq!(q.shown(), "ゆめ", "a composing run draws after the text");
        assert!(q.set("keyboardCommit", "夢遊"));
        assert_eq!((q.text.as_str(), q.preedit.as_str()), ("夢遊", ""));
    }

    /// `keyboardDelete` takes `before` characters off the end.
    #[test]
    fn a_delete_takes_characters_off_the_end() {
        let mut q = Query::default();
        q.set("keyboardCommit", "夢遊病者");
        assert!(q.set("keyboardDelete", "2:0"));
        assert_eq!(q.text, "夢遊");
        // `x` parses as no count.
        assert!(q.set("keyboardDelete", "x:0"));
        assert_eq!(q.text, "夢遊");
    }

    /// `keyboardReplace` takes `before` off and commits the third field.
    #[test]
    fn a_replace_swaps_the_tail() {
        let mut q = Query::default();
        q.set("keyboardCommit", "むえ");
        assert!(q.set("keyboardReplace", "2:0:夢絵"));
        assert_eq!(q.text, "夢絵");
    }

    /// [`Query::set`] answers false for a property it does not read.
    #[test]
    fn an_unknown_property_moves_nothing() {
        let mut q = Query::default();
        assert!(!q.set("keyboardBounds", "0:0"));
        assert_eq!(q, Query::default());
    }

    /// [`typed`] reaches `Query` with no `Service` open.
    #[test]
    fn keysyms_reach_the_query_without_a_service() {
        let mut q = Query::default();
        // 'a', 'b', BackSpace, 'c'.
        assert!(matches!(
            typed(&mut q, &[0x61, 0x62, 0xFF08, 0x63]),
            Act::Moved
        ));
        assert_eq!(q.text, "ac");
        assert!(matches!(typed(&mut q, &[0xFF0D]), Act::Search));
        assert!(matches!(typed(&mut q, &[0xFF1B]), Act::Back));
        // Shift_L names no character.
        assert!(matches!(typed(&mut q, &[0xFFE1]), Act::Nothing));
    }

    /// [`Query::commit`] takes a whole run; a following Backspace takes one
    /// character of it.
    #[test]
    fn a_committed_run_survives_a_backspace_over_x() {
        let mut q = Query::default();
        q.set("keyboardCommit", "夢遊病者");
        assert!(matches!(typed(&mut q, &[0xFF08]), Act::Moved));
        assert_eq!(
            q.text, "夢遊病",
            "backspace takes one character, not one byte"
        );
    }

    /// [`Layout::hit`] answers `Back` and `Search` at the two ends.
    #[test]
    fn back_and_search_take_the_two_ends() {
        for (w, h) in PANELS {
            let layout = Layout::compute(40, w, h);
            let row = h - 1;
            assert!(matches!(layout.hit(1, row, w), Some(Tap::Back)), "{w}x{h}");
            assert!(
                matches!(layout.hit(w - 1, row, w), Some(Tap::Search)),
                "{w}x{h}"
            );
            assert!(layout.hit(w / 2, row, w).is_none(), "{w}x{h}: dead middle");
            // Above `strip_top` belongs to the band.
            assert!(layout.hit(1, layout.strip_top - 1, w).is_none(), "{w}x{h}");
        }
    }

    /// `band_bottom` stays above `keyboard_top`.
    #[test]
    fn the_band_stays_clear_of_the_keyboard() {
        for (w, h) in PANELS {
            let layout = Layout::compute(40, w, h);
            assert!(
                layout.band_bottom < layout.keyboard_top,
                "{w}x{h}: band ends at {}, keyboard starts at {}",
                layout.band_bottom,
                layout.keyboard_top
            );
        }
    }
}
