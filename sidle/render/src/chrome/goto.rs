//! `Go To`: a card over the page listing the book's own contents, under the
//! fixed rows every book has.

use super::{Action, Canvas, Chrome, text::Align};
use crate::geom::Rect;

/// How tall one row stands, against the panel [`super::bars::REFERENCE`]
/// states. A list scrolls by this much at a time.
pub const ROW: f32 = 84.0;

/// A row the card carries above the book's own contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fixed {
    Beginning,
    /// The screen a number is typed into, marked with a chevron of its own.
    PageOrLocation,
}

impl Fixed {
    pub fn label(self) -> &'static str {
        match self {
            Fixed::Beginning => "Beginning",
            Fixed::PageOrLocation => "Page or Location",
        }
    }
}

/// One row of the contents list.
pub struct Entry {
    pub title: String,
    /// Which spine entry the row opens.
    pub chapter: usize,
    /// The location the row starts at, as the list shows it.
    pub location: i64,
    /// How far the title is indented.
    pub depth: usize,
}

/// Draw the card and as many entries as fit, marking the one in hand.
pub fn draw(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    fixed: &[(Fixed, Option<usize>)],
    entries: &[Entry],
    here: usize,
) {
    let panel = canvas.panel;
    let unit = canvas.unit();
    let theme = canvas.theme;

    // Anything outside the card closes it.
    chrome.add(
        Rect::new(0.0, 0.0, panel.width, panel.height),
        Action::Close,
    );

    let card = Rect::new(
        panel.width * 0.06,
        panel.height * 0.03,
        panel.width * 0.88,
        panel.height * 0.92,
    );
    canvas.fill(card, theme.page);
    canvas.stroke(card, theme.ink, 4.0 * unit);

    let left = card.x + 40.0 * unit;
    let right = card.right() - 40.0 * unit;
    canvas.text(
        "Go To",
        44.0 * unit,
        theme.ink,
        true,
        (left, card.y + 34.0 * unit),
        Align::Left,
    );

    // The close cross.
    let cross = Rect::new(
        right - 44.0 * unit,
        card.y + 30.0 * unit,
        44.0 * unit,
        44.0 * unit,
    );
    for step in 0..(44.0 * unit) as usize {
        let t = step as f32;
        canvas.fill(
            Rect::new(cross.x + t, cross.y + t, 5.0 * unit, 5.0 * unit),
            theme.ink,
        );
        canvas.fill(
            Rect::new(
                cross.right() - t - 5.0 * unit,
                cross.y + t,
                5.0 * unit,
                5.0 * unit,
            ),
            theme.ink,
        );
    }
    chrome.add(cross, Action::Close);

    // The tab in hand carries into the list below it; the other sits in a
    // box of its own.
    let tabs = card.y + 108.0 * unit;
    let divide = card.x + card.width * 0.5;
    canvas.rule(card.x, card.right(), tabs, 3.0 * unit, theme.ink);
    canvas.stroke(
        Rect::new(divide, tabs, card.right() - divide, 90.0 * unit),
        theme.ink,
        3.0 * unit,
    );
    canvas.text(
        "Contents",
        38.0 * unit,
        theme.ink,
        true,
        (card.x + card.width * 0.25, tabs + 22.0 * unit),
        Align::Center,
    );
    canvas.text(
        "Popular Highlights",
        38.0 * unit,
        theme.faint,
        false,
        (card.x + card.width * 0.75, tabs + 22.0 * unit),
        Align::Center,
    );
    canvas.rule(card.x, divide, tabs + 90.0 * unit, 3.0 * unit, theme.ink);

    let height = ROW * unit;
    let mut y = tabs + 108.0 * unit;
    for (row, chapter) in fixed {
        canvas.text(
            row.label(),
            38.0 * unit,
            theme.ink,
            false,
            (left, y + 14.0 * unit),
            Align::Left,
        );
        let area = Rect::new(left, y, right - left, height);
        match chapter {
            Some(chapter) => chrome.add(area, Action::GoToChapter(*chapter)),
            None => arrow(canvas, (right - 20.0 * unit, y + 40.0 * unit), 16.0 * unit),
        }
        canvas.rule(left, right, area.bottom(), 2.0 * unit, theme.faint);
        y += height;
    }
    // The list scrolls under the rows above it, inside the card's border.
    canvas.rule(card.x, card.right(), y, 3.0 * unit, theme.ink);
    let lead = 20.0 * unit;
    let list = Rect::new(
        card.x,
        y,
        card.width,
        (card.bottom() - 2.0 * unit - y).max(0.0),
    );
    let reach = (entries.len() as f32 * height + 2.0 * lead - list.height).max(0.0);
    chrome.scroll = chrome.scroll.clamp(0.0, reach);

    canvas.clip_to(list);
    let mut y = list.y + lead - chrome.scroll;
    for entry in entries {
        let area = Rect::new(left, y, right - left, height);
        y += height;
        if !area.intersects(&list) {
            continue;
        }
        let chosen = entry.chapter == here;
        canvas.text(
            &entry.title,
            36.0 * unit,
            theme.ink,
            chosen,
            (
                left + entry.depth as f32 * 28.0 * unit,
                area.y + 16.0 * unit,
            ),
            Align::Left,
        );
        if entry.location > 0 {
            canvas.text(
                &entry.location.to_string(),
                36.0 * unit,
                theme.ink,
                chosen,
                (right, area.y + 16.0 * unit),
                Align::Right,
            );
        }
        canvas.rule(left, right, area.bottom(), 2.0 * unit, theme.faint);
        // A row `list` cuts takes a click only where it shows.
        chrome.add(area.intersection(&list), Action::GoToChapter(entry.chapter));
    }
    canvas.unclip();
}

/// The solid mark a row carrying a screen of its own ends with.
fn arrow(canvas: &mut Canvas<'_, '_>, at: (f32, f32), size: f32) {
    let ink = canvas.theme.ink;
    let steps = (size * 1.2) as usize;
    for step in 0..steps.max(1) {
        let t = step as f32;
        let reach = (size - t * 0.8).max(0.0);
        canvas.fill(
            Rect::new(at.0 - size + t * 0.8, at.1 - reach, 1.5, reach * 2.0),
            ink,
        );
    }
}
