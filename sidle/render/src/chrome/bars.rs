//! The bar above a page and the one below it, stated against a panel
//! [`REFERENCE`] dots tall and scaled to the one in hand.

use super::{Action, Canvas, Chrome, Overlay, Position, text::Align};
use crate::geom::Rect;
use crate::settings::Progress;

/// The panel height every measurement in the chrome is stated against.
pub const REFERENCE: f32 = 1696.0;

/// Height of the toolbar, the title band under it, and the two together.
pub const TOOLBAR: f32 = 130.0;
pub const TITLE_BAND: f32 = 92.0;
pub const HEADER: f32 = TOOLBAR + TITLE_BAND;

/// Height of the bar below a page.
pub const FOOTER: f32 = 150.0;

/// Draw the toolbar, the title band, and the bar below the page.
/// `leftward` states which side of the page carries on.
pub fn draw(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    at: &Position,
    mode: Progress,
    leftward: bool,
) {
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
        icon(canvas, tool, box_, unit);
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
        (side, (TOOLBAR + 24.0) * unit),
        Align::Left,
    );
    canvas.rule(0.0, panel.width, HEADER * unit, 2.0 * unit, theme.ink);

    // Below the page: the chapter title, then `mode`, then the percentage.
    let foot = panel.height - FOOTER * unit;
    canvas.fill(Rect::new(0.0, foot, panel.width, FOOTER * unit), theme.page);
    canvas.rule(0.0, panel.width, foot, 2.0 * unit, theme.ink);
    canvas.text(
        &at.chapter_title,
        36.0 * unit,
        theme.ink,
        false,
        (panel.width / 2.0, foot + 26.0 * unit),
        Align::Center,
    );
    let progress = at.progress(mode);
    if !progress.is_empty() {
        canvas.text(
            &progress,
            30.0 * unit,
            theme.ink,
            false,
            (side, foot + 82.0 * unit),
            Align::Left,
        );
    }
    canvas.text(
        &format!("{}%", at.percent),
        30.0 * unit,
        theme.ink,
        false,
        (panel.width - side, foot + 82.0 * unit),
        Align::Right,
    );

    // One third of the page turns one way and the rest the other.
    let page = Rect::new(0.0, HEADER * unit, panel.width, foot - HEADER * unit);
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
            _ => None,
        }
    }
}

fn back_arrow(canvas: &mut Canvas<'_, '_>, box_: Rect) {
    let ink = canvas.theme.ink;
    let mid = box_.y + box_.height / 2.0;
    let weight = box_.height * 0.09;
    canvas.fill(
        Rect::new(box_.x, mid - weight / 2.0, box_.width, weight),
        ink,
    );
    // The head, as two short bars meeting at the shaft's end.
    let arm = box_.height * 0.3;
    for step in 0..(arm as usize).max(1) {
        let t = step as f32;
        canvas.fill(Rect::new(box_.x + t, mid - t - weight, weight, weight), ink);
        canvas.fill(Rect::new(box_.x + t, mid + t, weight, weight), ink);
    }
}

/// One [`Tool`]'s mark, drawn from filled rectangles and circles.
fn icon(canvas: &mut Canvas<'_, '_>, tool: Tool, box_: Rect, unit: f32) {
    let ink = canvas.theme.ink;
    let cx = box_.x + box_.width / 2.0;
    let cy = box_.y + box_.height / 2.0;
    let weight = 4.0 * unit;

    match tool {
        Tool::Aa => {
            canvas.text(
                "Aa",
                40.0 * unit,
                ink,
                false,
                (cx, cy - 24.0 * unit),
                Align::Center,
            );
        }
        Tool::Contents => {
            for (n, width) in [26.0, 20.0, 32.0].into_iter().enumerate() {
                let y = cy - 14.0 * unit + n as f32 * 14.0 * unit;
                canvas.fill(Rect::new(cx - 22.0 * unit, y, width * unit, weight), ink);
            }
            for n in 0..3 {
                let y = cy - 14.0 * unit + n as f32 * 14.0 * unit;
                canvas.fill(Rect::new(cx + 16.0 * unit, y, weight, weight), ink);
            }
        }
        Tool::Notes => {
            canvas.stroke(
                Rect::new(cx - 18.0 * unit, cy - 22.0 * unit, 36.0 * unit, 44.0 * unit),
                ink,
                weight,
            );
            canvas.fill(
                Rect::new(cx - 6.0 * unit, cy - 22.0 * unit, weight, 44.0 * unit),
                ink,
            );
        }
        Tool::Bookmark => {
            canvas.stroke(
                Rect::new(cx - 16.0 * unit, cy - 22.0 * unit, 32.0 * unit, 44.0 * unit),
                ink,
                weight,
            );
            canvas.fill(
                Rect::new(cx - 4.0 * unit, cy + 4.0 * unit, 8.0 * unit, 18.0 * unit),
                canvas.theme.page,
            );
        }
        Tool::Search => {
            canvas.circle((cx - 3.0 * unit, cy - 5.0 * unit), 15.0 * unit, ink, false);
            canvas.fill(
                Rect::new(cx + 7.0 * unit, cy + 6.0 * unit, 14.0 * unit, weight),
                ink,
            );
        }
        Tool::More => {
            for n in 0..3 {
                let y = cy - 20.0 * unit + n as f32 * 18.0 * unit;
                canvas.circle((cx, y), 4.0 * unit, ink, true);
            }
        }
    }
}
