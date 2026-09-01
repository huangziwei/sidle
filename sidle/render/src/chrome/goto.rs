//! `Go To`: a card over the page listing the book's own contents, under the
//! fixed rows every book has, and the screen one of those rows opens for a
//! page or location number.

use bokai::style::Color;

use super::{Action, Canvas, Chrome, Overlay, icon, text::Align};
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
    /// The spine entry the row's own place falls in.
    pub chapter: usize,
    /// The location the row starts at, as the list shows it.
    pub location: i64,
    /// How far the title is indented.
    pub depth: usize,
}

/// The row a place falls in: the last one its location reaches, else the last
/// one at or before its chapter.
pub fn here(entries: &[Entry], location: i64, chapter: usize) -> Option<usize> {
    entries
        .iter()
        .rposition(|entry| entry.location > 0 && entry.location <= location)
        .or_else(|| entries.iter().rposition(|entry| entry.chapter <= chapter))
}

/// Draw the card and as many entries as fit, marking the one in hand.
pub fn draw(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    fixed: &[(Fixed, Option<usize>)],
    entries: &[Entry],
    here: Option<usize>,
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
    canvas.icon(icon::CLOSE, cross, theme.ink);
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
        match (row, chapter) {
            (Fixed::PageOrLocation, _) => {
                arrow(canvas, (right - 20.0 * unit, y + 40.0 * unit), 16.0 * unit);
                chrome.add(area, Action::Open(Overlay::PageOrLocation));
            }
            (_, Some(chapter)) => chrome.add(area, Action::GoToChapter(*chapter)),
            (_, None) => {}
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
    for (n, entry) in entries.iter().enumerate() {
        let area = Rect::new(left, y, right - left, height);
        y += height;
        if !area.intersects(&list) {
            continue;
        }
        let chosen = here == Some(n);
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
        chrome.add(area.intersection(&list), Action::GoToEntry(n));
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

/// Whether a number typed into [`Fixed::PageOrLocation`] names one of the
/// book's printed pages or one of its locations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Numbering {
    Page,
    #[default]
    Location,
}

impl Numbering {
    pub fn label(self) -> &'static str {
        match self {
            Numbering::Page => "Page number",
            Numbering::Location => "Location number",
        }
    }

    /// What the screen says of a number the book does not reach.
    fn missing(self) -> &'static str {
        match self {
            Numbering::Page => "This page number doesn’t exist. Please try again.",
            Numbering::Location => "This location number doesn’t exist. Please try again.",
        }
    }
}

/// The screen [`Fixed::PageOrLocation`] opens: what has been typed into it,
/// and how far the book's two scales run.
pub struct Jump {
    /// The digits typed into the field.
    pub typed: String,
    pub numbering: Numbering,
    /// The last page the book numbers, absent where it numbers none.
    pub pages: Option<i64>,
    /// The last location.
    pub locations: i64,
}

impl Jump {
    /// The last number the numbering in hand names.
    pub fn last(&self) -> i64 {
        match self.numbering {
            Numbering::Page => self.pages.unwrap_or(0),
            Numbering::Location => self.locations,
        }
    }

    /// The number typed, where the book reaches it.
    pub fn number(&self) -> Option<i64> {
        let typed: i64 = self.typed.parse().ok()?;
        (typed >= 1 && typed <= self.last()).then_some(typed)
    }

    /// What the screen says about what has been typed, if anything.
    pub fn error(&self) -> Option<&'static str> {
        match self.typed.is_empty() || self.number().is_some() {
            true => None,
            false => Some(self.numbering.missing()),
        }
    }

    /// What the screen says under its title.
    fn description(&self) -> String {
        match self.pages {
            Some(pages) if pages > 0 => format!(
                "Enter a page number (1-{pages}) or location number (1-{}).",
                self.locations
            ),
            Some(_) => format!(
                "Enter a page number or location number (1-{}).",
                self.locations
            ),
            None => format!("Enter a location number (1-{}).", self.locations),
        }
    }

    /// What the empty field reads.
    fn hint(&self) -> &'static str {
        match self.pages {
            Some(_) => "Page number or location number",
            None => "Location number",
        }
    }
}

/// Draw the screen a page or location number is typed into, over the card
/// that opened it. Its boxes are stated in the artwork dots [`Canvas::art`]
/// scales, its type in the panel dots [`Canvas::unit`] does.
pub fn jump(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, jump: &Jump) {
    let panel = canvas.panel;
    let unit = canvas.unit();
    let art = canvas.art();
    let theme = canvas.theme;

    // Anything outside the screen leaves it for the card behind it.
    chrome.add(
        Rect::new(0.0, 0.0, panel.width, panel.height),
        Action::Open(Overlay::GoTo),
    );

    let pad = 24.0 * art;
    let head = 32.0 * art;
    let width = (600.0 * art).min(panel.width - 10.0 * art);
    let error = jump.error();
    let field = 60.0 * art;
    let radio = 44.0 * art;
    let button = 52.0 * art;
    let described = wrapped(canvas, &jump.description(), 36.0 * unit, width - 2.0 * pad);
    let body = canvas.line_of(44.0 * unit, true)
        + 10.0 * art
        + described.len() as f32 * canvas.line_of(36.0 * unit, false)
        + 10.0 * art
        + field
        + canvas.line_of(36.0 * unit, false)
        + 2.0 * radio
        + button
        + head
        + 2.0 * pad;
    let height = body.max(300.0 * art);
    let card = Rect::new(
        (panel.width - width) / 2.0,
        (panel.height - height) / 2.0,
        width,
        height,
    );
    canvas.round_fill(card, 4.0 * art, theme.page);
    canvas.round_stroke(card, 4.0 * art, theme.ink, 2.0 * art);

    let left = card.x + pad;
    let right = card.right() - pad;
    let mut y = card.y + head;
    y = canvas
        .text(
            "Go to Page or Location",
            44.0 * unit,
            theme.ink,
            true,
            (left, y),
            Align::Left,
        )
        .bottom()
        + 10.0 * art;
    for line in &described {
        y = canvas
            .text(line, 36.0 * unit, theme.ink, false, (left, y), Align::Left)
            .bottom();
    }
    y += 10.0 * art;

    // The field, holding the digits or the hint standing in for them.
    let box_ = Rect::new(left, y, right - left, field);
    canvas.stroke(box_, theme.ink, 3.0 * unit);
    let (shown, ink) = match jump.typed.is_empty() {
        true => (jump.hint(), theme.faint),
        false => (jump.typed.as_str(), theme.ink),
    };
    let size = 40.0 * unit;
    let lead = (field - canvas.line_of(size, false)) / 2.0;
    canvas.text(
        shown,
        size,
        ink,
        false,
        (box_.x + 16.0 * art, box_.y + lead),
        Align::Left,
    );
    y = box_.bottom();
    if let Some(error) = error {
        canvas.text(error, 36.0 * unit, theme.ink, false, (left, y), Align::Left);
    }
    y += canvas.line_of(36.0 * unit, false);

    // The two numberings, the one in hand marked, each offered only where
    // the book carries the scale it names.
    let dot = 20.0 * unit;
    for numbering in [Numbering::Page, Numbering::Location] {
        let offered = match numbering {
            Numbering::Page => jump.pages.is_some_and(|pages| pages > 0),
            Numbering::Location => jump.locations > 0,
        };
        let ink = match offered {
            true => theme.ink,
            false => theme.faint,
        };
        let middle = y + radio / 2.0;
        canvas.circle((left + dot, middle), dot, ink, false);
        if numbering == jump.numbering {
            canvas.circle((left + dot, middle), 11.0 * unit, ink, true);
        }
        let size = 36.0 * unit;
        let half = canvas.line_of(size, false) / 2.0;
        canvas.text(
            numbering.label(),
            size,
            ink,
            false,
            (left + 56.0 * unit, middle - half),
            Align::Left,
        );
        if offered {
            chrome.add(
                Rect::new(left, y, right - left, radio),
                Action::Numbering(numbering),
            );
        }
        y += radio;
    }

    // Cancel, then Go, which stands ready only once the field names a place.
    let wide = 120.0 * art;
    let go = Rect::new(right - wide, card.bottom() - pad - button, wide, button);
    let cancel = Rect::new(go.x - 12.0 * art - wide, go.y, wide, button);
    canvas.round_stroke(cancel, 4.0 * art, theme.ink, 3.0 * unit);
    label(canvas, "Cancel", cancel, theme.ink, false);
    chrome.add(cancel, Action::Open(Overlay::GoTo));
    match jump.number() {
        Some(_) => {
            canvas.round_fill(go, 4.0 * art, theme.ink);
            label(canvas, "Go", go, theme.page, true);
            chrome.add(go, Action::GoToNumber);
        }
        None => {
            canvas.round_stroke(go, 4.0 * art, theme.faint, 3.0 * unit);
            label(canvas, "Go", go, theme.faint, true);
        }
    }
}

/// One button's own word, in the middle of it.
fn label(canvas: &mut Canvas<'_, '_>, word: &str, box_: Rect, color: Color, bold: bool) {
    let size = 36.0 * canvas.unit();
    let line = canvas.line_of(size, bold);
    canvas.text(
        word,
        size,
        color,
        bold,
        (
            box_.x + box_.width / 2.0,
            box_.y + (box_.height - line) / 2.0,
        ),
        Align::Center,
    );
}

/// `content` broken into lines no wider than `room`, at its spaces.
fn wrapped(canvas: &mut Canvas<'_, '_>, content: &str, size: f32, room: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in content.split(' ') {
        let longer = match line.is_empty() {
            true => word.to_string(),
            false => format!("{line} {word}"),
        };
        if !line.is_empty() && canvas.width_of(&longer, size, false) > room {
            lines.push(std::mem::take(&mut line));
            line = word.to_string();
            continue;
        }
        line = longer;
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A contents list whose last three rows share one section.
    fn contents() -> Vec<Entry> {
        [(0, 1), (1, 100), (2, 200), (2, 300), (2, 400)]
            .into_iter()
            .enumerate()
            .map(|(n, (chapter, location))| Entry {
                title: format!("Entry {}", n + 1),
                chapter,
                location,
                depth: 0,
            })
            .collect()
    }

    /// `here` names the last row the location reaches, one row of a shared
    /// section as readily as a row holding a section of its own.
    #[test]
    fn a_row_of_a_shared_section_stands_for_its_own_place() {
        let entries = contents();
        assert_eq!(here(&entries, 1, 0), Some(0));
        assert_eq!(here(&entries, 150, 1), Some(1));
        assert_eq!(here(&entries, 250, 2), Some(2));
        assert_eq!(here(&entries, 300, 2), Some(3));
        assert_eq!(here(&entries, 900, 2), Some(4));
        // `unlocated` numbers no location, and an empty list holds no row.
        let unlocated: Vec<Entry> = entries
            .into_iter()
            .map(|entry| Entry {
                location: 0,
                ..entry
            })
            .collect();
        assert_eq!(here(&unlocated, 600, 1), Some(1));
        assert_eq!(here(&[], 600, 1), None);
    }

    fn screen(typed: &str, numbering: Numbering, pages: Option<i64>) -> Jump {
        Jump {
            typed: typed.to_string(),
            numbering,
            pages,
            locations: 5289,
        }
    }

    /// The field takes a number the book reaches and refuses one past its
    /// end, each numbering against its own scale.
    #[test]
    fn a_number_the_book_does_not_reach_is_refused() {
        let located = |typed| screen(typed, Numbering::Location, Some(320));
        assert_eq!(located("").number(), None);
        assert_eq!(located("").error(), None);
        assert_eq!(located("1").number(), Some(1));
        assert_eq!(located("5289").number(), Some(5289));
        assert_eq!(located("0").number(), None);
        assert_eq!(
            located("5290").error(),
            Some("This location number doesn’t exist. Please try again.")
        );
        // The same number against the page scale, which stops far earlier.
        let paged = |typed| screen(typed, Numbering::Page, Some(320));
        assert_eq!(paged("320").number(), Some(320));
        assert_eq!(
            paged("321").error(),
            Some("This page number doesn’t exist. Please try again.")
        );
        // A book that numbers no pages reaches no page number at all.
        assert_eq!(screen("1", Numbering::Page, None).number(), None);
    }

    /// The screen states the scales the book carries, and the field says
    /// which numbers it takes.
    #[test]
    fn the_screen_states_the_scales_the_book_carries() {
        let numbered = screen("", Numbering::Page, Some(320));
        assert_eq!(
            numbered.description(),
            "Enter a page number (1-320) or location number (1-5289)."
        );
        assert_eq!(numbered.hint(), "Page number or location number");
        let unnumbered = screen("", Numbering::Location, None);
        assert_eq!(
            unnumbered.description(),
            "Enter a location number (1-5289)."
        );
        assert_eq!(unnumbered.hint(), "Location number");
        // A book whose page list carries no numbered page states neither a
        // last page nor a range for one.
        let empty = screen("", Numbering::Location, Some(0));
        assert_eq!(
            empty.description(),
            "Enter a page number or location number (1-5289)."
        );
    }
}
