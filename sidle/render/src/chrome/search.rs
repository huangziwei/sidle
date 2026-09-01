//! `Search`: a card over the page holding the phrase being looked for and
//! every place in the book it occurs.

use super::{Action, Canvas, Chrome, icon, text::Align};
use crate::geom::Rect;

/// How tall one result stands, against the panel [`super::bars::REFERENCE`]
/// states. A list scrolls by this much at a time.
pub const ROW: f32 = 128.0;

/// What the field reads while nothing has been typed.
const HINT: &str = "Search in this book";

/// One place the phrase occurs, as the list states it.
pub struct Found {
    /// The text before the match on the line it sits on.
    pub before: String,
    /// The phrase itself, as the book spells it.
    pub found: String,
    /// The text after the match on the same line.
    pub after: String,
    /// The location the match falls in.
    pub location: i64,
}

/// Everything the card shows.
pub struct Search<'a> {
    /// The phrase as it has been typed so far.
    pub query: &'a str,
    /// What an input method is composing after it, drawn underlined and
    /// carried into `query` on a commit.
    pub composing: &'a str,
    /// Where it was found, in reading order.
    pub found: &'a [Found],
    /// Whether the book has been searched for the phrase in hand.
    pub searched: bool,
}

/// Draw the card, the field and as many results as fit.
pub fn draw(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, search: &Search<'_>) {
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

    // The field: the glass, the phrase or the hint standing in for it, and
    // the composition an input method holds after them.
    let glass = Rect::new(left, card.y + 26.0 * unit, 44.0 * unit, 44.0 * unit);
    canvas.icon(icon::SEARCH, glass, theme.ink);
    let size = 40.0 * unit;
    let at = (glass.right() + 20.0 * unit, card.y + 28.0 * unit);
    let empty = search.query.is_empty() && search.composing.is_empty();
    let (typed, ink) = match empty {
        true => (HINT, theme.faint),
        false => (search.query, theme.ink),
    };
    let drawn = canvas.text(typed, size, ink, false, at, Align::Left);
    if !search.composing.is_empty() {
        let x = match empty {
            true => at.0,
            false => drawn.right(),
        };
        let composed = canvas.text(
            search.composing,
            size,
            theme.ink,
            false,
            (x, at.1),
            Align::Left,
        );
        canvas.rule(
            composed.x,
            composed.right(),
            composed.bottom() - 6.0 * unit,
            3.0 * unit,
            theme.ink,
        );
    }
    // The field's own box, where an input method puts its candidates.
    chrome.field = Some(Rect::new(
        at.0,
        at.1,
        right - 60.0 * unit - at.0,
        canvas.line_of(size, false),
    ));
    let cross = Rect::new(
        right - 44.0 * unit,
        card.y + 26.0 * unit,
        44.0 * unit,
        44.0 * unit,
    );
    canvas.icon(icon::CLOSE, cross, theme.ink);
    chrome.add(cross, Action::Close);
    canvas.rule(
        card.x,
        card.right(),
        card.y + 92.0 * unit,
        3.0 * unit,
        theme.ink,
    );

    // What the search came to, above the results themselves.
    canvas.text(
        &stated(search),
        36.0 * unit,
        theme.ink,
        true,
        (left, card.y + 112.0 * unit),
        Align::Left,
    );

    let height = ROW * unit;
    let head = card.y + 176.0 * unit;
    canvas.rule(card.x, card.right(), head, 3.0 * unit, theme.ink);
    let list = Rect::new(
        card.x,
        head,
        card.width,
        (card.bottom() - 2.0 * unit - head).max(0.0),
    );
    let lead = 20.0 * unit;
    let reach = (search.found.len() as f32 * height + 2.0 * lead - list.height).max(0.0);
    chrome.scroll = chrome.scroll.clamp(0.0, reach);

    canvas.clip_to(list);
    let mut y = list.y + lead - chrome.scroll;
    for (n, found) in search.found.iter().enumerate() {
        let area = Rect::new(left, y, right - left, height);
        y += height;
        if !area.intersects(&list) {
            continue;
        }
        line(canvas, found, area, unit);
        canvas.text(
            &format!("Loc {}", found.location),
            32.0 * unit,
            theme.faint,
            false,
            (left, area.y + 74.0 * unit),
            Align::Left,
        );
        canvas.rule(left, right, area.bottom(), 2.0 * unit, theme.faint);
        chrome.add(area.intersection(&list), Action::GoToFound(n));
    }
    canvas.unclip();
}

/// One result's own line: the text either side of the phrase, with the phrase
/// itself in bold between them, cut where the row ends.
fn line(canvas: &mut Canvas<'_, '_>, found: &Found, area: Rect, unit: f32) {
    let ink = canvas.theme.ink;
    let size = 34.0 * unit;
    let mut x = area.x;
    for (part, bold) in [
        (found.before.as_str(), false),
        (found.found.as_str(), true),
        (found.after.as_str(), false),
    ] {
        if part.is_empty() || x >= area.right() {
            continue;
        }
        let part = fitted(canvas, part, size, bold, area.right() - x);
        let drawn = canvas.text(
            &part,
            size,
            ink,
            bold,
            (x, area.y + 16.0 * unit),
            Align::Left,
        );
        x = drawn.right();
    }
}

/// As much of `content` as `room` holds.
fn fitted(canvas: &mut Canvas<'_, '_>, content: &str, size: f32, bold: bool, room: f32) -> String {
    if canvas.width_of(content, size, bold) <= room {
        return content.to_string();
    }
    let mut fits = String::new();
    for character in content.chars() {
        fits.push(character);
        if canvas.width_of(&fits, size, bold) > room {
            fits.pop();
            break;
        }
    }
    fits
}

/// What the card says above the results.
fn stated(search: &Search<'_>) -> String {
    match (search.searched, search.found.len()) {
        (false, _) => "Text in this book".to_string(),
        (true, 0) => "No results were found.".to_string(),
        (true, 1) => "1 result found".to_string(),
        (true, count) => format!("{count} results found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn searched(found: usize) -> Vec<Found> {
        (0..found)
            .map(|n| Found {
                before: String::new(),
                found: "cat".to_string(),
                after: String::new(),
                location: n as i64,
            })
            .collect()
    }

    #[test]
    fn the_head_counts_what_a_finished_search_found() {
        let none: Vec<Found> = Vec::new();
        let one = searched(1);
        let many = searched(7);
        let card = |found: &[Found], searched| {
            stated(&Search {
                query: "cat",
                composing: "",
                found,
                searched,
            })
        };
        assert_eq!(card(&none, false), "Text in this book");
        assert_eq!(card(&none, true), "No results were found.");
        assert_eq!(card(&one, true), "1 result found");
        assert_eq!(card(&many, true), "7 results found");
    }
}
