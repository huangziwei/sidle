//! The bars over a page: the progress line a page states below it, and the
//! toolbar, title band and footer a tap reveals over it. Every measurement is
//! stated against a [`REFERENCE`]-dot panel and scaled to the one in hand.

use bokai::style::Color;

use super::{Action, Canvas, Chrome, Overlay, Position, icon, text::Align};
use crate::geom::Rect;
use crate::settings::Progress;

/// The panel height every measurement below is stated against.
pub const REFERENCE: f32 = 1696.0;

/// Height of the toolbar, the title band under it, and the two together.
pub const TOOLBAR: f32 = 112.0;
pub const TITLE_BAND: f32 = 113.0;
pub const HEADER: f32 = TOOLBAR + TITLE_BAND;

/// Height of the bar below the page while the bars are showing.
pub const FOOTER: f32 = 290.0;

/// The share of the page a tap on it reveals the bars from.
const REVEAL: f32 = 0.14;

/// Draw the progress line, the bars where they are showing, and the areas a
/// tap acts on. `mode` is the measure the page states below it, `None` for
/// none at all; `leftward` states which side of the page carries on.
pub fn draw(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    at: &Position,
    mode: Option<Progress>,
    leftward: bool,
) {
    if chrome.revealed {
        bars(chrome, canvas, at, mode);
    } else if let Some(mode) = mode {
        line(canvas, at, mode);
    }
    taps(chrome, canvas, leftward);
}

/// The line a page carries below it: the chosen measure, and how far in.
fn line(canvas: &mut Canvas<'_, '_>, at: &Position, mode: Progress) {
    let panel = canvas.panel;
    let unit = canvas.unit();
    let theme = canvas.theme;
    let side = panel.width * 0.05;
    let y = panel.height - 60.0 * unit;

    canvas.text(
        &at.progress(mode),
        30.0 * unit,
        theme.ink,
        true,
        (side, y),
        Align::Left,
    );
    canvas.text(
        &format!("{}%", at.percent),
        30.0 * unit,
        theme.ink,
        true,
        (panel.width - side, y),
        Align::Right,
    );
}

/// The toolbar, the title band and the footer, drawn over the page.
fn bars(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, at: &Position, mode: Option<Progress>) {
    let panel = canvas.panel;
    let unit = canvas.unit();
    let side = panel.width * 0.05;
    let theme = canvas.theme;

    canvas.fill(Rect::new(0.0, 0.0, panel.width, HEADER * unit), theme.page);
    let middle = TOOLBAR * unit / 2.0;

    // Back to the library.
    let arrow = Rect::new(side, middle - 22.0 * unit, 44.0 * unit, 44.0 * unit);
    back_arrow(canvas, arrow);
    let label = canvas.text(
        "Library",
        34.0 * unit,
        theme.ink,
        false,
        (side + 60.0 * unit, middle - 24.0 * unit),
        Align::Left,
    );
    chrome.add(
        Rect::new(arrow.x, arrow.y, label.right() - arrow.x, arrow.height),
        Action::Close,
    );

    // The tools, right to left.
    let mut x = panel.width - side;
    let step = 78.0 * unit;
    for tool in [
        Tool::More,
        Tool::Search,
        Tool::Bookmark,
        Tool::Notes,
        Tool::Contents,
        Tool::Aa,
    ] {
        let box_ = Rect::new(x - step, middle - 26.0 * unit, step, 52.0 * unit);
        icon(canvas, tool, box_);
        if let Some(action) = tool.action() {
            chrome.add(box_, action);
        }
        x -= step;
    }

    canvas.rule(0.0, panel.width, TOOLBAR * unit, 2.0 * unit, theme.faint);
    canvas.text(
        &at.title,
        36.0 * unit,
        theme.ink,
        true,
        (side, (TOOLBAR + 32.0) * unit),
        Align::Left,
    );
    canvas.rule(0.0, panel.width, HEADER * unit, 2.0 * unit, theme.ink);

    // Below the page: the chapter, everything the page states about where it
    // sits, and the two views a book opens in.
    let foot = panel.height - FOOTER * unit;
    canvas.fill(Rect::new(0.0, foot, panel.width, FOOTER * unit), theme.page);
    canvas.rule(0.0, panel.width, foot, 2.0 * unit, theme.ink);
    canvas.text(
        &at.chapter_title,
        36.0 * unit,
        theme.ink,
        false,
        (panel.width / 2.0, foot + 30.0 * unit),
        Align::Center,
    );
    if let Some(mode) = mode {
        canvas.text(
            &at.stated(mode),
            30.0 * unit,
            theme.ink,
            false,
            (panel.width / 2.0, foot + 92.0 * unit),
            Align::Center,
        );
    }
    views(chrome, canvas, foot + 176.0 * unit, unit);
}

/// The page view and the grid view, side by side below the footer's text.
fn views(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, y: f32, unit: f32) {
    let panel = canvas.panel;
    let ink = canvas.theme.ink;
    let (width, height) = (156.0 * unit, 64.0 * unit);
    let left = panel.width / 2.0 - width;

    for (n, grid) in [false, true].into_iter().enumerate() {
        let box_ = Rect::new(left + n as f32 * width, y, width, height);
        canvas.stroke(box_, ink, 3.0 * unit);
        view_mark(canvas, box_, grid, ink, unit);
        chrome.add(box_, Action::Grid(grid));
    }
}

/// One view's mark: a page between its neighbours, or nine of them.
pub fn view_mark(canvas: &mut Canvas<'_, '_>, box_: Rect, grid: bool, ink: Color, unit: f32) {
    let cx = box_.x + box_.width / 2.0;
    let cy = box_.y + box_.height / 2.0;
    if grid {
        for row in 0..3 {
            for column in 0..3 {
                canvas.fill(
                    Rect::new(
                        cx - 21.0 * unit + column as f32 * 16.0 * unit,
                        cy - 21.0 * unit + row as f32 * 16.0 * unit,
                        11.0 * unit,
                        11.0 * unit,
                    ),
                    ink,
                );
            }
        }
    } else {
        for (column, width) in [(0, 9.0), (1, 22.0), (2, 9.0)] {
            canvas.fill(
                Rect::new(
                    cx - 24.0 * unit + column as f32 * 17.0 * unit,
                    cy - 18.0 * unit,
                    width * unit,
                    36.0 * unit,
                ),
                ink,
            );
        }
    }
}

/// Where a tap turns a page, and where it takes the bars away or brings them
/// back.
fn taps(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, leftward: bool) {
    let panel = canvas.panel;
    let reveal = Rect::new(0.0, 0.0, panel.width, panel.height * REVEAL);

    if chrome.revealed {
        // The page between the bars takes them away again.
        let unit = canvas.unit();
        let top = HEADER * unit;
        let foot = panel.height - FOOTER * unit;
        chrome.add(
            Rect::new(0.0, top, panel.width, (foot - top).max(0.0)),
            Action::Reveal(false),
        );
        return;
    }

    // One third of the page turns one way and the rest the other.
    let page = Rect::new(0.0, 0.0, panel.width, panel.height);
    let (near, far) = if leftward { (1, -1) } else { (-1, 1) };
    chrome.add(
        Rect::new(page.x, page.y, page.width / 3.0, page.height),
        Action::TurnPage(near),
    );
    chrome.add(
        Rect::new(
            page.x + page.width / 3.0,
            page.y,
            page.width * 2.0 / 3.0,
            page.height,
        ),
        Action::TurnPage(far),
    );
    chrome.add(reveal, Action::Reveal(true));
}

/// One tool in the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Aa,
    Contents,
    Notes,
    Bookmark,
    Search,
    More,
}

impl Tool {
    fn action(self) -> Option<Action> {
        match self {
            Tool::Aa => Some(Action::Open(Overlay::Aa)),
            Tool::Contents => Some(Action::Open(Overlay::GoTo)),
            Tool::Search => Some(Action::Open(Overlay::Search)),
            _ => None,
        }
    }
}

/// The way back to the library, its own artwork in `box_`.
fn back_arrow(canvas: &mut Canvas<'_, '_>, box_: Rect) {
    let ink = canvas.theme.ink;
    canvas.icon(icon::BACK, box_, ink);
}

/// One [`Tool`]'s mark, its own artwork in `box_`.
fn icon(canvas: &mut Canvas<'_, '_>, tool: Tool, box_: Rect) {
    let ink = canvas.theme.ink;
    let art = match tool {
        Tool::Aa => icon::AA,
        Tool::Contents => icon::CONTENTS,
        Tool::Notes => icon::NOTES,
        Tool::Bookmark => icon::BOOKMARK,
        Tool::Search => icon::SEARCH,
        Tool::More => icon::MORE,
    };
    canvas.icon(art, box_, ink);
}
