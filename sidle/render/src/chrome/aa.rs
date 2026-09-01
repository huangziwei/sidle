//! The `Aa` sheet: a tab strip over Themes, Font, Layout and More, each tab
//! a grid of icon buttons, sliders and rows.

use super::{AaPane, AaTab, Action, Canvas, Chrome, Reading, text::Align};
use crate::geom::Rect;
use crate::settings::{Device, Orientation, Preset, Progress, Stop};

/// The share of the panel the sheet covers, which the reader sets outside the
/// sheet's own layout.
const SHEET: f32 = 0.6;

/// The tab strip's own height, in stated dots.
const TAB_STRIP: f32 = 60.0;

/// The padding a row of settings holds above and below what it carries.
const ROW_PAD: f32 = 19.0;

/// The block a list of font names takes, by the rows of names in it.
const FAMILY_BLOCK: [f32; 2] = [92.0, 161.0];

/// A ladder's own row: how deep it stands, the share of it left for the
/// ladder beside its label, the share of that the ladder fills, and the room
/// the `-` before its stops and the `+` after them stand in.
const LADDER_ROW: f32 = 92.0;
const LADDER_VIEW: f32 = 0.78;
const LADDER_TRACK: f32 = 0.95;
const LADDER_END: f32 = 58.0;

/// The row one font's name stands in.
const FAMILY_ROW: f32 = 63.0;

/// The padding a group of controls holds above and below its row of them.
const GROUP_PAD: f32 = 13.0;

/// How deep the box one control in a group sits in stands. A taller control
/// grows it, which is what sets one group in a row against another.
const CONTROL_BOX: f32 = 60.0;

/// The share of the More tab one of its rows takes, which stands four of them
/// on a page.
const MORE_ROW: f32 = 0.25;

/// A theme tile's own row, and the padding the rows of them sit in.
const TILE_ROW: f32 = 83.0;
const TILES_PAD: f32 = 19.0;

/// The padding the Layout tab's own rows sit under.
const LAYOUT_PAD: f32 = 24.0;

/// The corner a plate is rounded by, and the padding around it: `TILE` sets
/// 41 in a box of 61.
const PLATE_CORNER: f32 = 6.0;
const PADDING: f32 = 10.0;

/// The share of the sheet the Layout tab's two blocks take.
const BLOCK_LEFT: f32 = 0.41;

/// One row of the More tab on a panel `height` deep at `dpi`, which is the
/// step a scrolling list of settings takes.
pub fn row_of(height: f32, dpi: f32) -> f32 {
    (height * SHEET - dpi / super::ARTWORK_DPI * TAB_STRIP) * MORE_ROW
}

/// Draw the sheet and whichever tab or screen is showing.
pub fn draw(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, reading: &Reading<'_>) {
    let panel = canvas.panel;
    let unit = canvas.unit();
    let theme = canvas.theme;
    let top = panel.height * (1.0 - SHEET);

    canvas.fill(
        Rect::new(0.0, top, panel.width, panel.height - top),
        theme.page,
    );
    canvas.rule(0.0, panel.width, top, 4.0 * unit, theme.ink);

    // Anything above the sheet closes it.
    chrome.add(Rect::new(0.0, 0.0, panel.width, top), Action::Close);

    let strip = canvas.art() * TAB_STRIP;
    match chrome.pane {
        AaPane::Tab => tabs(chrome, canvas, top, strip),
        AaPane::ReadingProgress => heading(chrome, canvas, top, strip),
    }
    let mut page = Page {
        left: panel.width * 0.05,
        right: panel.width * 0.95,
        body: top + strip,
        unit,
        art: canvas.art(),
    };

    match chrome.pane {
        AaPane::Tab => match chrome.tab {
            AaTab::Themes => themes(chrome, canvas, &mut page, reading, panel.height),
            AaTab::Font => font(chrome, canvas, &mut page, reading),
            AaTab::Layout => layout(chrome, canvas, &mut page, reading),
            AaTab::More => more(chrome, canvas, &mut page, reading, panel.height),
        },
        AaPane::ReadingProgress => reading_progress(chrome, canvas, &mut page, reading),
    }
}

/// Where a tab lays its bands out: the inset edges, the top of the band to
/// come, and one reference and one stated dot in dots of the panel in hand.
struct Page {
    left: f32,
    right: f32,
    body: f32,
    unit: f32,
    art: f32,
}

impl Page {
    fn width(&self) -> f32 {
        self.right - self.left
    }

    /// The second of the two columns a tab lays its groups out in.
    fn column(&self) -> f32 {
        self.left + self.width() * 0.46
    }

    /// `dots` stated dots, in dots of the panel in hand.
    fn dp(&self, dots: f32) -> f32 {
        dots * self.art
    }

    /// Take a band of `dots` stated dots, returning its top edge.
    fn band(&mut self, dots: f32) -> f32 {
        let top = self.body;
        self.body += dots * self.art;
        top
    }
}

/// The tab strip, filling a band `strip` deep from `top`.
fn tabs(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, top: f32, strip: f32) {
    let panel = canvas.panel;
    let unit = canvas.unit();
    let theme = canvas.theme;
    let mut x = panel.width * 0.05;
    let size = 36.0 * unit;
    let middle = top + strip / 2.0;

    for tab in AaTab::ALL {
        let chosen = tab == chrome.tab;
        let line = middle - canvas.line_of(size, chosen) / 2.0;
        let label = canvas.text(tab.label(), size, theme.ink, chosen, (x, line), Align::Left);
        if chosen {
            canvas.rule(
                label.x - 4.0 * unit,
                label.right() + 4.0 * unit,
                top + strip - 4.0 * unit,
                8.0 * unit,
                theme.ink,
            );
        }
        chrome.add(
            Rect::new(label.x - 14.0 * unit, top, label.width + 28.0 * unit, strip),
            Action::Tab(tab),
        );
        x = label.right() + 56.0 * unit;
    }
    canvas.rule(0.0, panel.width, top + strip, 2.0 * unit, theme.faint);
}

/// A screen's back arrow and title, filling a band `strip` deep from `top`.
fn heading(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, top: f32, strip: f32) {
    let panel = canvas.panel;
    let unit = canvas.unit();
    let theme = canvas.theme;
    let left = panel.width * 0.05;
    let size = 38.0 * unit;
    let middle = top + strip / 2.0;

    chevron(canvas, (left + 14.0 * unit, middle), 18.0 * unit, true);
    let line = middle - canvas.line_of(size, true) / 2.0;
    canvas.text(
        chrome.pane.title(),
        size,
        theme.ink,
        true,
        (left + 54.0 * unit, line),
        Align::Left,
    );
    chrome.add(
        Rect::new(0.0, top, panel.width * 0.5, strip),
        Action::Pane(AaPane::Tab),
    );
    canvas.rule(0.0, panel.width, top + strip, 2.0 * unit, theme.faint);
}

fn themes(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    page: &mut Page,
    reading: &Reading<'_>,
    bottom: f32,
) {
    let unit = page.unit;
    let theme = canvas.theme;
    let dark = chrome.dark;

    // Page colour: a labelled row, as tall as the taller of its label and its
    // two swatches, in the padding a row of settings holds. The ring a chosen
    // swatch carries stands outside the row, as its own artwork states it.
    let size = 36.0 * unit;
    let label = canvas.line_of(size, false);
    let swatch = 26.0 * unit;
    let ring = 34.0 * unit;
    let row = page.dp(ROW_PAD * 2.0) + label.max(swatch * 2.0);
    let top = page.band(0.0);
    let middle = top + row / 2.0;
    canvas.text(
        "Page Color",
        size,
        theme.ink,
        false,
        (page.left, middle - label / 2.0),
        Align::Left,
    );
    for (n, night) in [false, true].into_iter().enumerate() {
        let centre = (page.right - (84.0 - n as f32 * 66.0) * unit, middle);
        canvas.circle(centre, swatch, theme.ink, night);
        if night == dark {
            canvas.circle(centre, ring, theme.ink, false);
        }
        chrome.add(
            Rect::new(centre.0 - ring, middle - ring, ring * 2.0, ring * 2.0),
            Action::PageColor(night),
        );
    }
    page.body = top + row;
    canvas.rule(page.left, page.right, page.body, 2.0 * unit, theme.ink);

    // Manage Themes and Save Current Settings sit at the foot of the sheet,
    // and the tiles take what is left between them.
    let manage = bottom - page.dp(TAB_STRIP);
    let save = manage - page.dp(LADDER_ROW);
    let tiles = page.body + page.dp(TILES_PAD);

    // The theme in hand, then the three `Preset::OFFERED` names.
    let held = reading.settings.matches(reading.panel);
    tile(
        chrome,
        canvas,
        page,
        Tile {
            top: tiles,
            slot: 0,
            name: "Custom",
            held: held.is_none(),
            preset: None,
        },
    );
    for (n, preset) in Preset::OFFERED.into_iter().enumerate() {
        tile(
            chrome,
            canvas,
            page,
            Tile {
                top: tiles,
                slot: n + 1,
                name: preset.label(),
                held: held == Some(preset),
                preset: Some(preset),
            },
        );
    }

    let button = Rect::new(
        page.left,
        save + page.dp(ROW_PAD),
        page.width(),
        page.dp(LADDER_ROW - ROW_PAD * 2.0),
    );
    canvas.round_stroke(button, 6.0 * unit, theme.faint, 2.0 * unit);
    let size = 34.0 * unit;
    let line = canvas.line_of(size, false);
    canvas.text(
        "Save Current Settings",
        size,
        theme.faint,
        false,
        (
            page.left + page.width() / 2.0,
            button.y + (button.height - line) / 2.0,
        ),
        Align::Center,
    );

    canvas.rule(page.left, page.right, manage, 2.0 * unit, theme.faint);
    let middle = manage + (bottom - manage) / 2.0;
    canvas.text(
        "Manage Themes",
        size,
        theme.faint,
        false,
        (page.left, middle - line / 2.0),
        Align::Left,
    );
    chevron(
        canvas,
        (page.right - 18.0 * unit, middle),
        16.0 * unit,
        false,
    );
}

/// One theme tile: where the tiles start, which slot it takes, what it is
/// called, whether it is the theme in hand, and the preset it sets.
struct Tile<'a> {
    top: f32,
    slot: usize,
    name: &'a str,
    held: bool,
    preset: Option<Preset>,
}

/// One theme tile: a ruled page in a box, its name beside it, and the tick
/// where it is the theme in hand.
fn tile(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, page: &Page, tile: Tile<'_>) {
    let Tile {
        top,
        slot,
        name,
        held,
        preset,
    } = tile;
    let unit = page.unit;
    let art = canvas.art();
    let theme = canvas.theme;
    let x = if slot.is_multiple_of(2) {
        page.left
    } else {
        page.column()
    };
    // The tile's box sits in the middle of its own row.
    let row = page.dp(TILE_ROW);
    let box_ = 61.0 * art;
    let y = top + (slot / 2) as f32 * row + (row - box_) / 2.0;

    // The plate sits inside its own touch box, ruled at the pitch the theme
    // sets. A theme with no ruled page of its own carries its initial.
    let inside = (x + PADDING * art, y + PADDING * art);
    match preset.and_then(|preset| Preset::OFFERED.iter().position(|it| *it == preset)) {
        Some(n) => {
            let button = Button {
                art: Art {
                    plate: TILE,
                    rules: TILE_ART[n],
                    turned: false,
                },
                chosen: false,
                action: Action::Preset(Preset::OFFERED[n]),
            };
            let ink = canvas.theme.ink;
            control(chrome, canvas, &button, inside, art, ink);
        }
        None => {
            canvas.round_stroke(
                Rect::new(inside.0, inside.1, TILE.0 * art, TILE.1 * art),
                PLATE_CORNER * art,
                theme.ink,
                2.0 * art,
            );
            let size = A_SIZE * art;
            let top = y + A_BASELINE * art - canvas.baseline_of(size, false);
            canvas.text(
                "A",
                size,
                theme.ink,
                false,
                (inside.0 + TILE.0 * art / 2.0, top),
                Align::Center,
            );
        }
    }

    if held {
        canvas.polygon(&bite(), (x, y), art, theme.page);
        canvas.polygon(&TICK, (x, y), art, theme.ink);
    }
    // The name, and under it what a held tile is called, the pair of them in
    // the middle of the box beside them.
    let size = 34.0 * unit;
    let said = 30.0 * unit;
    let name_line = canvas.line_of(size, held);
    let lines = name_line
        + if held {
            canvas.line_of(said, false)
        } else {
            0.0
        };
    let beside = x + box_ + 16.0 * art;
    let top = y + (box_ - lines) / 2.0;
    canvas.text(name, size, theme.ink, held, (beside, top), Align::Left);
    if held {
        canvas.text(
            "Current Theme",
            said,
            theme.faint,
            false,
            (beside, top + name_line),
            Align::Left,
        );
    }
    if let Some(preset) = preset {
        chrome.add(
            Rect::new(x, y, page.width() * 0.45, box_),
            Action::Preset(preset),
        );
    }
}

/// The [`TICK`] in the middle of `box_`, as large as it fits.
fn check(canvas: &mut Canvas<'_, '_>, box_: Rect) {
    let ink = canvas.theme.ink;
    let art = (box_.width / TICK_BOX.0).min(box_.height / TICK_BOX.1);
    let at = (
        box_.x + (box_.width - TICK_BOX.0 * art) / 2.0,
        box_.y + (box_.height - TICK_BOX.1 * art) / 2.0,
    );
    canvas.polygon(&TICK, at, art, ink);
}

fn font(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, page: &mut Page, reading: &Reading<'_>) {
    let unit = page.unit;
    let theme = canvas.theme;
    let settings = reading.settings;
    let panel = reading.panel;

    // The names take a block of their own, stated by how many rows they run
    // to, with the rows in the middle of it.
    let rows = reading.families.len().div_ceil(2);
    let block = FAMILY_BLOCK[usize::from(rows > 1).min(FAMILY_BLOCK.len() - 1)];
    let name = 34.0 * unit;
    let line = canvas.line_of(name, false);
    let dot = 22.0 * unit;
    let row = page.dp(FAMILY_ROW);
    let top = page.band(block.max(rows as f32 * row / page.art));
    let first = top + (page.dp(block) - rows as f32 * row).max(0.0) / 2.0;

    for (n, family) in reading.families.iter().enumerate() {
        let x = if n % 2 == 0 { page.left } else { page.column() };
        let middle = first + (n / 2) as f32 * row + row / 2.0;
        canvas.circle((x + dot, middle), 20.0 * unit, theme.ink, false);
        if n == settings.family {
            canvas.circle((x + dot, middle), 11.0 * unit, theme.ink, true);
        }
        canvas.text(
            family,
            name,
            theme.ink,
            false,
            (x + 52.0 * unit, middle - line / 2.0),
            Align::Left,
        );
        chrome.add(
            Rect::new(x, middle - row / 2.0, page.width() * 0.45, row),
            Action::Family(n),
        );
    }

    // Then one ladder to a row, each under a rule.
    for ladder in [
        (
            "Bold",
            settings.boldness,
            panel.boldness.len(),
            &Action::Bold as &dyn Fn(usize) -> Action,
        ),
        (
            "Size",
            settings.font_size,
            panel.font_size_stops(),
            &Action::FontSize,
        ),
    ] {
        let (label, at, stops, action) = ladder;
        canvas.rule(page.left, page.right, page.body, 2.0 * unit, theme.faint);
        let band = page.band(LADDER_ROW);
        slider(
            chrome,
            canvas,
            page,
            Ladder {
                label,
                y: band + page.dp(LADDER_ROW) / 2.0,
                at,
                stops,
            },
            action,
        );
    }
}

/// One labelled ladder: where it sits, which stop it is at, and how many it
/// carries.
struct Ladder<'a> {
    label: &'a str,
    y: f32,
    at: usize,
    stops: usize,
}

/// A labelled row of stops with a `−` and a `+`, the stops up to the chosen
/// one filled and its number above it.
fn slider(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    page: &Page,
    ladder: Ladder<'_>,
    action: &dyn Fn(usize) -> Action,
) {
    let Ladder {
        label,
        y,
        at,
        stops,
    } = ladder;
    let unit = page.unit;
    let theme = canvas.theme;
    // The label takes the head of the row and the ladder the rest, which the
    // `-` opens and the `+` closes.
    let track_right = page.right - page.dp(LADDER_END);
    let track_left = page.right - page.width() * LADDER_VIEW * LADDER_TRACK + page.dp(LADDER_END);
    let step = (track_right - track_left) / stops.max(1) as f32;

    canvas.text(
        label,
        34.0 * unit,
        theme.ink,
        false,
        (page.left, y - 20.0 * unit),
        Align::Left,
    );

    // The minus, then the stops, then the plus.
    canvas.fill(
        Rect::new(
            track_left - 56.0 * unit,
            y - 3.0 * unit,
            30.0 * unit,
            6.0 * unit,
        ),
        theme.ink,
    );
    chrome.add(
        Rect::new(
            track_left - 76.0 * unit,
            y - 30.0 * unit,
            70.0 * unit,
            60.0 * unit,
        ),
        action(at.saturating_sub(1)),
    );
    for n in 0..stops {
        let box_ = Rect::new(
            track_left + n as f32 * step,
            y - 9.0 * unit,
            step * 0.78,
            18.0 * unit,
        );
        if n <= at {
            canvas.round_fill(box_, 4.0 * unit, theme.ink);
        } else {
            canvas.round_stroke(box_, 4.0 * unit, theme.ink, 2.0 * unit);
        }
        chrome.add(box_, action(n));
    }
    canvas.text(
        &format!("{}", at + 1),
        28.0 * unit,
        theme.ink,
        false,
        (track_left + (at as f32 + 0.39) * step, y - 52.0 * unit),
        Align::Center,
    );
    canvas.fill(
        Rect::new(
            track_right + 26.0 * unit,
            y - 3.0 * unit,
            30.0 * unit,
            6.0 * unit,
        ),
        theme.ink,
    );
    canvas.fill(
        Rect::new(
            track_right + 38.0 * unit,
            y - 15.0 * unit,
            6.0 * unit,
            30.0 * unit,
        ),
        theme.ink,
    );
    chrome.add(
        Rect::new(
            track_right + 6.0 * unit,
            y - 30.0 * unit,
            70.0 * unit,
            60.0 * unit,
        ),
        action((at + 1).min(stops.saturating_sub(1))),
    );
}

fn layout(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    page: &mut Page,
    reading: &Reading<'_>,
) {
    let settings = reading.settings;
    let down = reading.vertical;
    let right = page.left + page.width() * BLOCK_LEFT;
    page.band(LAYOUT_PAD);

    // `down` takes the other plate, turned: its rules draw as columns.
    let orientation = Orientation::ALL.map(|held| {
        let n = usize::from(held == Orientation::Landscape);
        let stated = if down { 1 - n } else { n };
        Button {
            art: Art {
                plate: ORIENTATION_ART[stated],
                rules: ORIENTATION_RULES[stated],
                turned: down,
            },
            chosen: held == settings.orientation,
            action: Action::Orient(held),
        }
    });

    let stops = [Stop::Narrow, Stop::Normal, Stop::Wide];
    let margins = stops.map(|stop| Button {
        art: plate(
            if down {
                MARGIN_ART_TURNED[stop.index()]
            } else {
                MARGIN_ART[stop.index()]
            },
            down,
        ),
        chosen: stop == settings.margins,
        action: Action::Margins(stop),
    });
    row(
        chrome,
        canvas,
        page,
        [
            Group::new("Orientation", page.left, &orientation),
            Group::new("Margins", right, &margins),
        ],
    );

    // `down` denies the alignment group and greys its controls.
    let alignment = [true, false].map(|justified| Button {
        art: plate(ALIGNMENT_ART[usize::from(!justified)], false),
        chosen: !down && justified == settings.justified,
        action: Action::Justified(justified),
    });
    let spacing = stops.map(|stop| Button {
        art: plate(
            if down {
                SPACING_ART_TURNED[stop.index()]
            } else {
                SPACING_ART[stop.index()]
            },
            down,
        ),
        chosen: stop == settings.line_spacing,
        action: Action::Spacing(stop),
    });
    let mut left = Group::new("Alignment", page.left, &alignment);
    left.denied = down;
    row(
        chrome,
        canvas,
        page,
        [left, Group::new("Spacing", right, &spacing)],
    );

    let columns = [1u8, 2].map(|count| Button {
        art: plate(COLUMN_ART[usize::from(count == 2)], down),
        chosen: count == settings.columns,
        action: Action::Columns(count),
    });
    if reading.panel.columns_offered {
        row(
            chrome,
            canvas,
            page,
            [Group::new("Column", page.left, &columns)],
        );
    }
    if reading.hyphenates {
        toggle(
            chrome,
            canvas,
            page,
            "Hyphenate words that extend beyond the margin",
            settings.hyphenate,
            &Action::Hyphenate,
        );
    }
}

/// One labelled group of controls in a row of them.
struct Group<'a> {
    label: &'a str,
    x: f32,
    buttons: &'a [Button],
    /// Whether the book denies the setting, which greys the group and states
    /// so under its label.
    denied: bool,
}

impl<'a> Group<'a> {
    fn new(label: &'a str, x: f32, buttons: &'a [Button]) -> Self {
        Self {
            label,
            x,
            buttons,
            denied: false,
        }
    }

    /// What the group stands: its label, what a denied one says under it, the
    /// box its controls sit in, and the padding either side of that box.
    fn stands(&self, canvas: &mut Canvas<'_, '_>, page: &Page) -> f32 {
        let mut label = canvas.line_of(34.0 * page.unit, false);
        if self.denied {
            label += canvas.line_of(30.0 * page.unit, false);
        }
        label + page.dp(GROUP_PAD * 2.0) + self.box_of(page)
    }

    /// The box one row of its controls sits in: the stated one, grown to the
    /// tallest control in it.
    fn box_of(&self, page: &Page) -> f32 {
        let tallest = self
            .buttons
            .iter()
            .map(|button| button.art.size().1)
            .fold(0.0f32, f32::max);
        page.dp(CONTROL_BOX.max(tallest))
    }
}

/// A row of groups, each in the middle of the tallest of them, over the
/// padding a row of controls carries below it.
fn row<const N: usize>(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    page: &mut Page,
    groups: [Group<'_>; N],
) {
    let stands: [f32; N] = std::array::from_fn(|n| groups[n].stands(canvas, page));
    let tallest = stands.iter().fold(0.0f32, |tallest, it| tallest.max(*it));
    let top = page.body;
    for (group, stands) in groups.iter().zip(stands) {
        draw_group(chrome, canvas, page, group, top + (tallest - stands) / 2.0);
    }
    page.body = top + tallest + page.dp(GROUP_PAD);
}

/// One control's artwork: the plate it is drawn on and the rules inside it,
/// in the units the artwork states. `turned` puts the plate on its side.
struct Art {
    plate: (f32, f32),
    rules: &'static [(f32, f32, f32)],
    turned: bool,
}

impl Art {
    /// The plate as it is drawn, which a quarter turn puts on its side.
    fn size(&self) -> (f32, f32) {
        match self.turned {
            true => (self.plate.1, self.plate.0),
            false => self.plate,
        }
    }

    /// One rule as a rectangle on the drawn plate.
    fn rule(&self, (x0, x1, y): (f32, f32, f32), weight: f32) -> Rect {
        match self.turned {
            // A quarter turn anticlockwise: `(x, y)` lands at `(y, W - x)`.
            true => Rect::new(y - weight / 2.0, self.plate.0 - x1, weight, x1 - x0),
            false => Rect::new(x0, y - weight / 2.0, x1 - x0, weight),
        }
    }
}

/// The plate `Margins`, `Spacing`, `Alignment` and `Column` share.
const PLATE: (f32, f32) = (78.0, 44.0);

/// A theme tile's plate, inside a touch box of 61 by 61.
const TILE: (f32, f32) = (41.0, 41.0);

/// The tick a chosen control carries, as its own artwork states it, and the
/// box that artwork draws it in.
const TICK: [(f32, f32); 6] = [
    (25.159, 0.0),
    (9.818, 16.129),
    (1.841, 7.742),
    (0.0, 9.677),
    (9.818, 20.0),
    (27.0, 1.935),
];

/// The `A` a tile with no ruled page of its own carries: the size its
/// artwork sets, and where that artwork puts its baseline in the box of 61.
const A_SIZE: f32 = 27.0;
const A_BASELINE: f32 = 40.0;

/// The box `TICK` is drawn in, and the arc it clears a plate's corner
/// along: centre, then radius.
const TICK_BOX: (f32, f32) = (27.0, 20.0);
const TICK_BITE: ((f32, f32), f32) = ((26.0, 25.0), 16.5);

/// The corner a tick clears, in the tile's box of 61: the arc from the
/// plate's top edge round to its left side, closed at the corner itself.
fn bite() -> Vec<(f32, f32)> {
    let ((cx, cy), radius) = TICK_BITE;
    let mut corner: Vec<(f32, f32)> = (0..=8)
        .map(|step| {
            let turn = (-90.0 - step as f32 * 90.0 / 8.0f32).to_radians();
            (cx + radius * turn.cos(), cy + radius * turn.sin())
        })
        .collect();
    corner.push((PADDING - 1.0, PADDING - 1.0));
    corner
}

const MARGIN_ART: [&[(f32, f32, f32)]; 3] = [
    &[
        (6.0, 72.0, 10.5),
        (6.0, 72.0, 16.5),
        (6.0, 72.0, 22.5),
        (6.0, 72.0, 28.5),
        (6.0, 72.0, 34.5),
    ],
    &[
        (13.0, 66.0, 10.5),
        (13.0, 66.0, 16.5),
        (13.0, 66.0, 22.5),
        (13.0, 66.0, 28.5),
        (13.0, 66.0, 34.5),
    ],
    &[
        (22.0, 57.0, 10.5),
        (22.0, 57.0, 16.5),
        (22.0, 57.0, 22.5),
        (22.0, 57.0, 28.5),
        (22.0, 57.0, 34.5),
    ],
];

/// `MARGIN_ART` as a vertical book states it.
const MARGIN_ART_TURNED: [&[(f32, f32, f32)]; 3] = [
    &[
        (7.0, 71.0, 10.5),
        (7.0, 71.0, 16.5),
        (7.0, 71.0, 22.5),
        (7.0, 71.0, 28.5),
        (7.0, 71.0, 34.5),
    ],
    &[
        (15.0, 63.0, 10.5),
        (15.0, 63.0, 16.5),
        (15.0, 63.0, 22.5),
        (15.0, 63.0, 28.5),
        (15.0, 63.0, 34.5),
    ],
    &[
        (22.0, 56.0, 10.5),
        (22.0, 56.0, 16.5),
        (22.0, 56.0, 22.5),
        (22.0, 56.0, 28.5),
        (22.0, 56.0, 34.5),
    ],
];

const SPACING_ART: [&[(f32, f32, f32)]; 3] = [
    &[
        (13.0, 66.0, 13.5),
        (13.0, 66.0, 19.5),
        (13.0, 66.0, 25.5),
        (13.0, 66.0, 31.5),
    ],
    &[
        (13.0, 66.0, 10.5),
        (13.0, 66.0, 18.5),
        (13.0, 66.0, 26.5),
        (13.0, 66.0, 34.5),
    ],
    &[(13.0, 66.0, 12.5), (13.0, 66.0, 22.5), (13.0, 66.0, 33.5)],
];

const SPACING_ART_TURNED: [&[(f32, f32, f32)]; 3] = [
    SPACING_ART[0],
    SPACING_ART[1],
    &[(13.0, 66.0, 12.5), (13.0, 66.0, 22.5), (13.0, 66.0, 32.5)],
];

/// Justified, then ragged, whose rules run short.
const ALIGNMENT_ART: [&[(f32, f32, f32)]; 2] = [
    &[
        (13.0, 65.0, 13.5),
        (13.0, 65.0, 19.5),
        (13.0, 65.0, 25.5),
        (13.0, 65.0, 31.5),
    ],
    &[
        (13.0, 66.0, 13.5),
        (13.0, 61.5, 19.5),
        (13.0, 55.5, 25.5),
        (13.0, 66.0, 31.5),
    ],
];

const COLUMN_ART: [&[(f32, f32, f32)]; 2] = [
    &[
        (12.66, 67.89, 13.44),
        (12.66, 67.89, 19.62),
        (12.66, 67.89, 26.38),
        (12.66, 67.89, 32.56),
    ],
    &[
        (14.0, 38.0, 14.0),
        (14.0, 38.0, 20.0),
        (14.0, 38.0, 26.0),
        (14.0, 38.0, 32.0),
        (43.0, 67.0, 14.0),
        (43.0, 67.0, 20.0),
        (43.0, 67.0, 26.0),
        (43.0, 67.0, 32.0),
    ],
];

/// The plate upright, then on its side.
const ORIENTATION_ART: [(f32, f32); 2] = [(49.0, 66.0), (66.0, 49.0)];
const ORIENTATION_RULES: [&[(f32, f32, f32)]; 2] = [
    &[
        (10.0, 40.0, 11.5),
        (10.0, 40.0, 20.5),
        (10.0, 40.0, 29.5),
        (10.0, 40.0, 38.5),
        (10.0, 40.0, 47.5),
        (10.0, 40.0, 56.5),
    ],
    &[
        (11.5, 56.5, 11.0),
        (11.5, 56.5, 18.0),
        (11.5, 56.5, 25.0),
        (11.5, 56.5, 32.0),
        (11.5, 56.5, 39.0),
    ],
];

/// Compact, Standard and Large, the pitch each rules its page at.
const TILE_ART: [&[(f32, f32, f32)]; 3] = [
    &[
        (8.0, 33.0, 8.5),
        (8.0, 33.0, 13.5),
        (8.0, 33.0, 18.5),
        (8.0, 33.0, 23.5),
        (8.0, 33.0, 28.5),
        (8.0, 33.0, 33.5),
    ],
    &[
        (8.0, 33.0, 10.5),
        (8.0, 33.0, 17.5),
        (8.0, 33.0, 24.5),
        (8.0, 33.0, 31.5),
    ],
    &[(8.0, 33.0, 11.5), (8.0, 33.0, 21.5), (8.0, 33.0, 31.5)],
];

/// One control in a group of icon buttons.
struct Button {
    art: Art,
    chosen: bool,
    action: Action,
}

/// A control on `PLATE`, `turned` for a vertical book.
fn plate(rules: &'static [(f32, f32, f32)], turned: bool) -> Art {
    Art {
        plate: PLATE,
        rules,
        turned,
    }
}

/// One group: its label, its controls in a row below, and what a denied one
/// says between the two.
fn draw_group(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    page: &Page,
    group: &Group<'_>,
    top: f32,
) {
    let art = canvas.art();
    let unit = page.unit;
    let ink = if group.denied {
        canvas.theme.faint
    } else {
        canvas.theme.ink
    };
    let size = 34.0 * unit;
    let mut label = canvas.line_of(size, false);
    canvas.text(group.label, size, ink, false, (group.x, top), Align::Left);
    if group.denied {
        canvas.text(
            "Not Available",
            30.0 * unit,
            ink,
            false,
            (group.x, top + label),
            Align::Left,
        );
        label += canvas.line_of(30.0 * unit, false);
    }

    // Each control in the middle of the box the row of them stands in.
    let box_ = group.box_of(page);
    let row = top + label + page.dp(GROUP_PAD);
    let mut at = group.x;
    for button in group.buttons {
        let (width, height) = button.art.size();
        control(
            chrome,
            canvas,
            button,
            (at, row + (box_ - height * art) / 2.0),
            art,
            ink,
        );
        at += (width + PADDING) * art;
    }
}

/// Draw a control: the plate, its rules, and the area a click covers.
fn control(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    button: &Button,
    at: (f32, f32),
    art: f32,
    ink: crate::chrome::Color,
) {
    let (width, height) = button.art.size();
    let (inset, weight) = if button.chosen {
        (1.5, 3.0)
    } else {
        (0.5, 1.0)
    };
    canvas.round_stroke(
        Rect::new(
            at.0 + inset * art,
            at.1 + inset * art,
            (width - inset * 2.0) * art,
            (height - inset * 2.0) * art,
        ),
        PLATE_CORNER * art,
        ink,
        weight * art,
    );
    for stated in button.art.rules {
        let rule = button.art.rule(*stated, 1.0);
        canvas.fill(
            Rect::new(
                at.0 + rule.x * art,
                at.1 + rule.y * art,
                rule.width * art,
                rule.height * art,
            ),
            ink,
        );
    }
    chrome.add(
        Rect::new(at.0, at.1, width * art, height * art),
        button.action.clone(),
    );
}

/// A row with a box at its right, checked where the setting is on.
fn toggle(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    page: &mut Page,
    label: &str,
    on: bool,
    action: &dyn Fn(bool) -> Action,
) {
    let unit = page.unit;
    let theme = canvas.theme;
    let size = 32.0 * unit;
    let line = canvas.line_of(size, false);
    let check_box = 44.0 * unit;
    let height = page.dp(ROW_PAD * 2.0) + line.max(check_box);
    let top = page.band(height / page.art);
    let middle = top + height / 2.0;

    canvas.text(
        label,
        size,
        theme.ink,
        false,
        (page.left, middle - line / 2.0),
        Align::Left,
    );
    let box_ = Rect::new(
        page.right - check_box,
        middle - check_box / 2.0,
        check_box,
        check_box,
    );
    canvas.round_stroke(box_, 4.0 * unit, theme.ink, 3.0 * unit);
    if on {
        check(canvas, box_.inset_by(9.0 * unit));
    }
    chrome.add(Rect::new(page.left, top, page.width(), height), action(!on));
}

fn more(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    page: &mut Page,
    reading: &Reading<'_>,
    bottom: f32,
) {
    let unit = page.unit;
    let theme = canvas.theme;

    // Four rows to a page, and more rows than that, so they scroll inside the
    // tab.
    let button = 56.0 * unit;
    let line = canvas.line_of(34.0 * unit, false);
    let list = Rect::new(0.0, page.body, page.right + page.left, bottom - page.body);
    let row = (list.height * MORE_ROW).max(page.dp(ROW_PAD * 2.0) + line.max(button));
    let reach = (row * MORE_ROWS as f32 - list.height).max(0.0);
    chrome.scroll = chrome.scroll.clamp(0.0, reach);
    canvas.clip_to(list);
    page.body -= chrome.scroll;

    // The screen the emulator draws, which a Kindle has in hardware.
    let size = 34.0 * unit;
    let (_, middle) = settings_row(canvas, page, button, row);
    canvas.text(
        "Screen",
        size,
        theme.ink,
        false,
        (page.left, middle - line / 2.0),
        Align::Left,
    );
    for (n, device) in Device::ALL.into_iter().enumerate() {
        let box_ = Rect::new(
            page.right - (2 - n) as f32 * 200.0 * unit + 10.0 * unit,
            middle - button / 2.0,
            190.0 * unit,
            button,
        );
        let chosen = reading.device == Some(device);
        canvas.round_stroke(
            box_,
            6.0 * unit,
            theme.ink,
            if chosen { 5.0 } else { 2.0 } * unit,
        );
        let size = 30.0 * unit;
        let line = canvas.line_of(size, chosen);
        canvas.text(
            device.name(),
            size,
            theme.ink,
            chosen,
            (box_.x + box_.width / 2.0, middle - line / 2.0),
            Align::Center,
        );
        chrome.add(box_, Action::Screen(device));
    }
    rule_under(canvas, page);

    // What the bar below the page states, then the rows a Kindle carries that
    // this reader has nothing behind.
    let (top, middle) = settings_row(canvas, page, 0.0, row);
    canvas.text(
        "Reading Progress",
        size,
        theme.ink,
        false,
        (page.left, middle - line / 2.0),
        Align::Left,
    );
    let said = canvas.line_of(30.0 * unit, false);
    canvas.text(
        reading.settings.progress.label(),
        30.0 * unit,
        theme.faint,
        false,
        (page.right - 40.0 * unit, middle - said / 2.0),
        Align::Right,
    );
    chevron(
        canvas,
        (page.right - 14.0 * unit, middle),
        16.0 * unit,
        false,
    );
    chrome.add(
        Rect::new(page.left, top, page.width(), page.body - top),
        Action::Pane(AaPane::ReadingProgress),
    );
    rule_under(canvas, page);

    for inert in INERT {
        let (_, middle) = settings_row(canvas, page, 0.0, row);
        canvas.text(
            inert,
            size,
            theme.faint,
            false,
            (page.left, middle - line / 2.0),
            Align::Left,
        );
        rule_under(canvas, page);
    }
    canvas.unclip();
}

/// The rows a Kindle carries that this reader has nothing behind.
const INERT: [&str; 7] = [
    "About This Book",
    "Book Mentions",
    "Highlight Menu",
    "Page Turn Animation",
    "Popular Highlights",
    "Show Clock While Reading",
    "Word Wise",
];

/// How many rows the More tab holds beside the one naming the screen.
const MORE_ROWS: usize = INERT.len() + 1;

/// Take a row of settings `deep` dots deep, or as deep as the padding around
/// what it holds makes it, and report its top edge and its middle. `tall` is
/// what the row carries beside its label.
fn settings_row(canvas: &mut Canvas<'_, '_>, page: &mut Page, tall: f32, deep: f32) -> (f32, f32) {
    let line = canvas.line_of(34.0 * page.unit, false);
    let height = deep.max(page.dp(ROW_PAD * 2.0) + line.max(tall));
    let top = page.band(height / page.art);
    (top, top + height / 2.0)
}

/// The rule closing the row in hand.
fn rule_under(canvas: &mut Canvas<'_, '_>, page: &Page) {
    let faint = canvas.theme.faint;
    canvas.rule(page.left, page.right, page.body, 2.0 * page.unit, faint);
}

/// The screen behind More's `Reading Progress` row: what the bar below the
/// page states.
fn reading_progress(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    page: &mut Page,
    reading: &Reading<'_>,
) {
    let unit = page.unit;
    let theme = canvas.theme;
    let chosen = reading.settings.progress;
    let size = 34.0 * unit;
    let line = canvas.line_of(size, false);
    let dot = 22.0 * unit;

    for mode in Progress::ALL
        .into_iter()
        .filter(|mode| mode.offered(reading.numbered, reading.chaptered))
    {
        let (top, middle) = settings_row(canvas, page, dot * 2.0, 0.0);
        canvas.circle((page.left + dot, middle), 20.0 * unit, theme.ink, false);
        if mode == chosen {
            canvas.circle((page.left + dot, middle), 11.0 * unit, theme.ink, true);
        }
        canvas.text(
            mode.label(),
            size,
            theme.ink,
            false,
            (page.left + 56.0 * unit, middle - line / 2.0),
            Align::Left,
        );
        chrome.add(
            Rect::new(page.left, top, page.width(), page.body - top),
            Action::Progress(mode),
        );
        rule_under(canvas, page);
    }
}

/// A chevron, pointing back where `back` and on where not.
fn chevron(canvas: &mut Canvas<'_, '_>, at: (f32, f32), size: f32, back: bool) {
    let ink = canvas.theme.ink;
    let weight = size * 0.22;
    let steps = (size * 0.9) as usize;
    let turn = if back { 1.0 } else { -1.0 };
    for step in 0..steps.max(1) {
        let t = step as f32 * 0.6;
        canvas.fill(
            Rect::new(at.0 + turn * t - weight / 2.0, at.1 - t, weight, weight),
            ink,
        );
        canvas.fill(
            Rect::new(at.0 + turn * t - weight / 2.0, at.1 + t, weight, weight),
            ink,
        );
    }
}
