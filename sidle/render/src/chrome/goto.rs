//! `Go To`: a card over the page listing the book's own contents, under the
//! fixed rows every book has.

use super::{Action, Canvas, Chrome, text::Align};
use crate::geom::Rect;

/// A row every book carries, above its own contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fixed {
    Cover,
    FrontMatter,
    Beginning,
    End,
}

impl Fixed {
    pub fn label(self) -> &'static str {
        match self {
            Fixed::Cover => "Cover",
            Fixed::FrontMatter => "Front Matter",
            Fixed::Beginning => "Beginning",
            Fixed::End => "End",
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
    fixed: &[(Fixed, usize)],
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

    let tabs = card.y + 108.0 * unit;
    canvas.rule(card.x, card.right(), tabs, 3.0 * unit, theme.ink);
    canvas.text(
        "Contents",
        38.0 * unit,
        theme.ink,
        true,
        (card.x + card.width * 0.28, tabs + 22.0 * unit),
        Align::Center,
    );
    canvas.text(
        "Popular Highlights",
        38.0 * unit,
        theme.faint,
        false,
        (card.x + card.width * 0.72, tabs + 22.0 * unit),
        Align::Center,
    );
    canvas.rule(
        card.x,
        card.right(),
        tabs + 90.0 * unit,
        3.0 * unit,
        theme.ink,
    );

    let height = 84.0 * unit;
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
        chrome.add(area, Action::GoToChapter(*chapter));
        canvas.rule(left, right, area.bottom(), 2.0 * unit, theme.faint);
        y += height;
    }
    y += 20.0 * unit;

    for entry in entries {
        if y + height > card.bottom() - 20.0 * unit {
            break;
        }
        let chosen = entry.chapter == here;
        canvas.text(
            &entry.title,
            36.0 * unit,
            theme.ink,
            chosen,
            (left + entry.depth as f32 * 28.0 * unit, y + 16.0 * unit),
            Align::Left,
        );
        if entry.location > 0 {
            canvas.text(
                &entry.location.to_string(),
                36.0 * unit,
                theme.ink,
                chosen,
                (right, y + 16.0 * unit),
                Align::Right,
            );
        }
        let area = Rect::new(left, y, right - left, height);
        chrome.add(area, Action::GoToChapter(entry.chapter));
        canvas.rule(left, right, area.bottom(), 2.0 * unit, theme.faint);
        y += height;
    }
}
