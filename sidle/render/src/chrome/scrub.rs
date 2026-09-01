//! The pages of a chapter, reached over a slider: one page previewed at a
//! time, or nine at once, each tagged with the location it opens at.

use tiny_skia::Pixmap;

use super::{Action, Canvas, Chrome, bars, icon, text::Align};
use crate::geom::Rect;

/// How much of the panel the bar below the pages takes.
const BAR: f32 = 300.0;

/// The pages nine at a time, across and down.
const GRID: usize = 3;

/// One page the scrubber offers.
pub struct Leaf<'a> {
    /// The page, at the size it was drawn.
    pub sheet: &'a Pixmap,
    /// The location it opens at.
    pub location: i64,
    /// Which page of the chapter it is.
    pub page: usize,
}

/// Everything the scrubber states about where it stands.
pub struct Scrub<'a> {
    pub chapter_title: String,
    /// The pages it offers: one where the pages show one at a time, nine
    /// where they show nine.
    pub leaves: &'a [Leaf<'a>],
    /// Which page the chapter is open at.
    pub here: usize,
    pub pages: usize,
    pub locations: i64,
    /// Whether the pages carry on leftward.
    pub leftward: bool,
}

/// Draw the pages, then the bar that moves between them.
pub fn draw(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, scrub: &Scrub<'_>) {
    let panel = canvas.panel;
    let unit = canvas.unit();
    let theme = canvas.theme;
    let foot = panel.height - BAR * unit;

    canvas.fill(Rect::new(0.0, 0.0, panel.width, foot), theme.page);
    if chrome.grid {
        grid(chrome, canvas, scrub, foot);
    } else {
        preview(chrome, canvas, scrub, foot);
    }
    bar(chrome, canvas, scrub, foot);
}

/// One page in a card, with a page either side of it.
fn preview(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, scrub: &Scrub<'_>, foot: f32) {
    let panel = canvas.panel;
    let unit = canvas.unit();
    let theme = canvas.theme;
    let Some(leaf) = scrub.leaves.first() else {
        return;
    };

    let card = Rect::new(
        panel.width * 0.08,
        panel.height * 0.04,
        panel.width * 0.84,
        foot - panel.height * 0.08,
    );
    canvas.fill(card, theme.page);
    canvas.stroke(card, theme.ink, 4.0 * unit);
    // The page shown is the page opened.
    chrome.add(card, Action::GoToPage(leaf.page));
    // The page, clear of the band its location is stated in.
    canvas.picture(
        Rect::new(
            card.x + 6.0 * unit,
            card.y + 6.0 * unit,
            card.width - 12.0 * unit,
            card.height - 100.0 * unit,
        ),
        leaf.sheet,
    );

    cross(
        chrome,
        canvas,
        (card.right() - 46.0 * unit, card.y + 46.0 * unit),
        unit,
    );
    canvas.text(
        &format!("Loc {} of {}", leaf.location, scrub.locations),
        30.0 * unit,
        theme.ink,
        true,
        (card.x + card.width / 2.0, card.bottom() - 60.0 * unit),
        Align::Center,
    );

    // A page either side of the one showing.
    let middle = card.y + card.height / 2.0;
    for (n, by) in [-1isize, 1].into_iter().enumerate() {
        let x = card.x + 40.0 * unit + n as f32 * (card.width - 80.0 * unit);
        chevron(canvas, (x, middle), 34.0 * unit, by < 0);
        let step = leaf.page as isize + by;
        if (0..scrub.pages as isize).contains(&step) {
            chrome.add(
                Rect::new(
                    x - 40.0 * unit,
                    middle - 60.0 * unit,
                    80.0 * unit,
                    120.0 * unit,
                ),
                Action::Scrub(step as usize),
            );
        }
    }
}

/// Nine pages at once, the one the chapter is open at in a heavier frame.
fn grid(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, scrub: &Scrub<'_>, foot: f32) {
    let panel = canvas.panel;
    let unit = canvas.unit();
    let theme = canvas.theme;
    let margin = panel.width * 0.06;
    let gap = 24.0 * unit;
    let width = (panel.width - margin * 2.0 - gap * (GRID as f32 - 1.0)) / GRID as f32;
    let height = (foot - margin * 2.0 - gap * (GRID as f32 - 1.0)) / GRID as f32;

    for (n, leaf) in scrub.leaves.iter().take(GRID * GRID).enumerate() {
        let column = if scrub.leftward {
            GRID - 1 - n % GRID
        } else {
            n % GRID
        };
        let cell = Rect::new(
            margin + column as f32 * (width + gap),
            margin + (n / GRID) as f32 * (height + gap),
            width,
            height,
        );
        canvas.fill(cell, theme.page);
        canvas.picture(cell.inset_by(3.0 * unit), leaf.sheet);
        canvas.stroke(
            cell,
            theme.ink,
            if leaf.page == scrub.here { 6.0 } else { 2.0 } * unit,
        );

        // The location the page opens at, in a tag at its own corner.
        let tag = format!("{}", leaf.location);
        let wide = 26.0 * unit * tag.len() as f32 * 0.62 + 20.0 * unit;
        let box_ = Rect::new(cell.right() - wide, cell.y, wide, 44.0 * unit);
        canvas.fill(box_, theme.ink);
        canvas.text(
            &tag,
            28.0 * unit,
            theme.page,
            true,
            (box_.x + box_.width / 2.0, box_.y + 6.0 * unit),
            Align::Center,
        );
        chrome.add(cell, Action::GoToPage(leaf.page));
    }
}

/// The bar below the pages: a chapter either side, the chapter's own name, a
/// slider over its pages, and the two ways the pages show.
fn bar(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, scrub: &Scrub<'_>, foot: f32) {
    let panel = canvas.panel;
    let unit = canvas.unit();
    let theme = canvas.theme;
    let side = panel.width * 0.05;

    canvas.fill(
        Rect::new(0.0, foot, panel.width, panel.height - foot),
        theme.page,
    );
    canvas.rule(0.0, panel.width, foot, 2.0 * unit, theme.ink);

    let row = foot + 60.0 * unit;
    for (n, by) in [-1isize, 1].into_iter().enumerate() {
        let x = side + n as f32 * (panel.width - side * 2.0);
        jump(canvas, (x, row), 30.0 * unit, by < 0);
        chrome.add(
            Rect::new(x - 44.0 * unit, row - 44.0 * unit, 88.0 * unit, 88.0 * unit),
            Action::Jump(by),
        );
    }
    canvas.text(
        &scrub.chapter_title,
        36.0 * unit,
        theme.ink,
        false,
        (panel.width / 2.0, row - 22.0 * unit),
        Align::Center,
    );

    // The slider, filled to the page the scrubber stands at.
    let track = Rect::new(
        side * 2.0,
        row + 78.0 * unit,
        panel.width - side * 4.0,
        5.0 * unit,
    );
    canvas.fill(track, theme.ink);
    let along = scrub.here as f32 / scrub.pages.max(1).saturating_sub(1).max(1) as f32;
    let along = if scrub.leftward { 1.0 - along } else { along };
    let knob = (
        track.x + track.width * along.clamp(0.0, 1.0),
        track.y + 2.5 * unit,
    );
    canvas.circle(knob, 30.0 * unit, theme.page, true);
    canvas.circle(knob, 30.0 * unit, theme.ink, false);
    for step in 0..scrub.pages {
        let x = track.x + track.width * step as f32 / scrub.pages.max(1) as f32;
        chrome.add(
            Rect::new(
                x,
                track.y - 40.0 * unit,
                track.width / scrub.pages.max(1) as f32,
                80.0 * unit,
            ),
            Action::Scrub(step),
        );
    }

    views(chrome, canvas, foot + 216.0 * unit, chrome.grid);
}

/// The page view and the grid view, the one showing filled.
fn views(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, y: f32, grid: bool) {
    let panel = canvas.panel;
    let unit = canvas.unit();
    let theme = canvas.theme;
    let (width, height) = (156.0 * unit, 64.0 * unit);
    let left = panel.width / 2.0 - width;

    for (n, wanted) in [false, true].into_iter().enumerate() {
        let box_ = Rect::new(left + n as f32 * width, y, width, height);
        if wanted == grid {
            canvas.fill(box_, theme.ink);
        }
        canvas.stroke(box_, theme.ink, 3.0 * unit);
        let ink = if wanted == grid {
            theme.page
        } else {
            theme.ink
        };
        bars::view_mark(canvas, box_, wanted, ink, unit);
        chrome.add(box_, Action::Grid(wanted));
    }
}

/// The mark that closes the scrubber.
fn cross(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, at: (f32, f32), unit: f32) {
    let ink = canvas.theme.ink;
    let reach = 26.0 * unit;
    let box_ = Rect::new(at.0 - reach, at.1 - reach, reach * 2.0, reach * 2.0);
    canvas.icon(icon::CLOSE, box_, ink);
    chrome.add(box_, Action::Close);
}

/// The chevron a page arrow carries, its own artwork scaled to `size`.
fn chevron(canvas: &mut Canvas<'_, '_>, at: (f32, f32), size: f32, back: bool) {
    let art = size / super::CHEVRON_BOX;
    let ink = canvas.theme.ink;
    canvas.chevron((at.0 - size / 2.0, at.1 - size / 2.0), art, back, ink);
}

/// A chevron against a bar: the chapter either side of this one.
fn jump(canvas: &mut Canvas<'_, '_>, at: (f32, f32), size: f32, back: bool) {
    let ink = canvas.theme.ink;
    let turn = if back { 1.0 } else { -1.0 };
    chevron(canvas, (at.0 + turn * size * 0.2, at.1), size, back);
    canvas.fill(
        Rect::new(
            at.0 - turn * size * 0.62,
            at.1 - size * 0.72,
            size * 0.16,
            size * 1.44,
        ),
        ink,
    );
}
