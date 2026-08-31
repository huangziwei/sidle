//! The `Aa` sheet: a tab strip over Themes, Font, Layout and More, each tab
//! a grid of icon buttons, sliders and rows.

use super::{AaPane, AaTab, Action, Canvas, Chrome, Reading, text::Align};
use crate::geom::Rect;
use crate::settings::{Device, Orientation, Preset, Progress, Stop};

/// The share of the panel the sheet covers.
const SHEET: f32 = 0.6;

/// Where a tab's own content starts below the sheet's top edge, and how far
/// apart two rows of controls sit, in reference dots.
const BODY: f32 = 155.0;
const GROUP: f32 = 250.0;

/// The corner a plate is rounded by, and the padding around it: `TILE` sets
/// 41 in a box of 61.
const PLATE_CORNER: f32 = 6.0;
const PADDING: f32 = 10.0;

/// The share of the sheet the Layout tab's two blocks take.
const BLOCK_LEFT: f32 = 0.41;

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

    let body = match chrome.pane {
        AaPane::Tab => tabs(chrome, canvas, top, unit),
        AaPane::ReadingProgress => heading(chrome, canvas, top, unit),
    };
    let page = Page {
        left: panel.width * 0.05,
        right: panel.width * 0.95,
        body,
        unit,
    };

    match chrome.pane {
        AaPane::Tab => match chrome.tab {
            AaTab::Themes => themes(chrome, canvas, &page, reading),
            AaTab::Font => font(chrome, canvas, &page, reading),
            AaTab::Layout => layout(chrome, canvas, &page, reading),
            AaTab::More => more(chrome, canvas, &page, reading),
        },
        AaPane::ReadingProgress => reading_progress(chrome, canvas, &page, reading),
    }
}

/// Where a tab's controls go: the inset edges, the first row, and one
/// reference dot in dots of the panel in hand.
struct Page {
    left: f32,
    right: f32,
    body: f32,
    unit: f32,
}

impl Page {
    fn width(&self) -> f32 {
        self.right - self.left
    }

    /// The second of the two columns a tab lays its groups out in.
    fn column(&self) -> f32 {
        self.left + self.width() * 0.46
    }

    /// `dots` reference dots below the first row.
    fn at(&self, dots: f32) -> f32 {
        self.body + dots * self.unit
    }
}

/// The tab strip, returning where the body starts.
fn tabs(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, top: f32, unit: f32) -> f32 {
    let panel = canvas.panel;
    let theme = canvas.theme;
    let mut x = panel.width * 0.05;

    for tab in AaTab::ALL {
        let chosen = tab == chrome.tab;
        let label = canvas.text(
            tab.label(),
            36.0 * unit,
            theme.ink,
            chosen,
            (x, top + 40.0 * unit),
            Align::Left,
        );
        if chosen {
            canvas.rule(
                label.x - 4.0 * unit,
                label.right() + 4.0 * unit,
                top + 96.0 * unit,
                8.0 * unit,
                theme.ink,
            );
        }
        chrome.add(
            Rect::new(
                label.x - 14.0 * unit,
                top,
                label.width + 28.0 * unit,
                100.0 * unit,
            ),
            Action::Tab(tab),
        );
        x = label.right() + 56.0 * unit;
    }
    canvas.rule(
        0.0,
        panel.width,
        top + 100.0 * unit,
        2.0 * unit,
        theme.faint,
    );
    top + BODY * unit
}

/// A screen's back arrow and title, returning where its body starts.
fn heading(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, top: f32, unit: f32) -> f32 {
    let panel = canvas.panel;
    let theme = canvas.theme;
    let left = panel.width * 0.05;

    chevron(
        canvas,
        (left + 14.0 * unit, top + 58.0 * unit),
        18.0 * unit,
        true,
    );
    canvas.text(
        chrome.pane.title(),
        38.0 * unit,
        theme.ink,
        true,
        (left + 54.0 * unit, top + 34.0 * unit),
        Align::Left,
    );
    chrome.add(
        Rect::new(0.0, top, panel.width * 0.5, 100.0 * unit),
        Action::Pane(AaPane::Tab),
    );
    canvas.rule(
        0.0,
        panel.width,
        top + 100.0 * unit,
        2.0 * unit,
        theme.faint,
    );
    top + BODY * unit
}

fn themes(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, page: &Page, reading: &Reading<'_>) {
    let unit = page.unit;
    let theme = canvas.theme;
    let dark = chrome.dark;

    canvas.text(
        "Page Color",
        36.0 * unit,
        theme.ink,
        false,
        (page.left, page.body),
        Align::Left,
    );
    for (n, night) in [false, true].into_iter().enumerate() {
        let centre = (
            page.right - (84.0 - n as f32 * 66.0) * unit,
            page.body + 18.0 * unit,
        );
        canvas.circle(centre, 26.0 * unit, theme.ink, night);
        if night == dark {
            canvas.circle(centre, 34.0 * unit, theme.ink, false);
        }
        chrome.add(
            Rect::new(
                centre.0 - 34.0 * unit,
                page.body - 10.0 * unit,
                68.0 * unit,
                68.0 * unit,
            ),
            Action::PageColor(night),
        );
    }
    canvas.rule(page.left, page.right, page.at(62.0), 2.0 * unit, theme.ink);

    // The theme in hand, then the three `Preset::OFFERED` names.
    let held = reading.settings.matches(reading.panel);
    tile(chrome, canvas, page, 0, "Custom", held.is_none(), None);
    for (n, preset) in Preset::OFFERED.into_iter().enumerate() {
        tile(
            chrome,
            canvas,
            page,
            n + 1,
            preset.label(),
            held == Some(preset),
            Some(preset),
        );
    }

    let save = Rect::new(page.left, page.at(430.0), page.width(), 76.0 * unit);
    canvas.round_stroke(save, 6.0 * unit, theme.faint, 2.0 * unit);
    canvas.text(
        "Save Current Settings",
        34.0 * unit,
        theme.faint,
        false,
        (page.left + page.width() / 2.0, page.at(452.0)),
        Align::Center,
    );

    canvas.rule(
        page.left,
        page.right,
        page.at(545.0),
        2.0 * unit,
        theme.faint,
    );
    canvas.text(
        "Manage Themes",
        34.0 * unit,
        theme.faint,
        false,
        (page.left, page.at(575.0)),
        Align::Left,
    );
    chevron(
        canvas,
        (page.right - 18.0 * unit, page.at(596.0)),
        16.0 * unit,
        false,
    );
}

/// One theme tile: a ruled page in a box, its name beside it, and the tick
/// where it is the theme in hand.
fn tile(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    page: &Page,
    slot: usize,
    name: &str,
    held: bool,
    preset: Option<Preset>,
) {
    let unit = page.unit;
    let art = canvas.art();
    let theme = canvas.theme;
    let x = if slot.is_multiple_of(2) {
        page.left
    } else {
        page.column()
    };
    let y = page.at(110.0 + (slot / 2) as f32 * 150.0);

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
            control(chrome, canvas, &button, inside, art);
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
    let box_ = 61.0 * art;
    canvas.text(
        name,
        34.0 * unit,
        theme.ink,
        held,
        (x + box_ + 16.0 * art, y + 16.0 * art),
        Align::Left,
    );
    if held {
        canvas.text(
            "Current Theme",
            30.0 * unit,
            theme.faint,
            false,
            (x + box_ + 16.0 * art, y + 38.0 * art),
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

fn font(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, page: &Page, reading: &Reading<'_>) {
    let unit = page.unit;
    let theme = canvas.theme;
    let settings = reading.settings;
    let panel = reading.panel;

    for (n, family) in reading.families.iter().enumerate() {
        let x = if n % 2 == 0 { page.left } else { page.column() };
        let y = page.at((n / 2) as f32 * 112.0);
        let dot = (x + 18.0 * unit, y + 18.0 * unit);
        canvas.circle(dot, 20.0 * unit, theme.ink, false);
        if n == settings.family {
            canvas.circle(dot, 11.0 * unit, theme.ink, true);
        }
        canvas.text(
            family,
            34.0 * unit,
            theme.ink,
            false,
            (x + 52.0 * unit, y - 4.0 * unit),
            Align::Left,
        );
        chrome.add(
            Rect::new(x, y - 10.0 * unit, page.width() * 0.45, 66.0 * unit),
            Action::Family(n),
        );
    }

    let rows = reading.families.len().div_ceil(2) as f32 * 112.0 + 30.0;
    canvas.rule(
        page.left,
        page.right,
        page.at(rows),
        2.0 * unit,
        theme.faint,
    );
    slider(
        chrome,
        canvas,
        page,
        Ladder {
            label: "Bold",
            y: page.at(rows + 80.0),
            at: settings.boldness,
            stops: panel.boldness.len(),
        },
        &Action::Bold,
    );
    canvas.rule(
        page.left,
        page.right,
        page.at(rows + 160.0),
        2.0 * unit,
        theme.faint,
    );
    slider(
        chrome,
        canvas,
        page,
        Ladder {
            label: "Size",
            y: page.at(rows + 250.0),
            at: settings.font_size,
            stops: panel.font_size_stops(),
        },
        &Action::FontSize,
    );
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
    let track_left = page.left + page.width() * 0.30;
    let track_right = page.right - 60.0 * unit;
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

fn layout(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, page: &Page, reading: &Reading<'_>) {
    let settings = reading.settings;
    let down = reading.vertical;
    let right = page.left + page.width() * BLOCK_LEFT;

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
    group(
        chrome,
        canvas,
        page,
        "Orientation",
        page.left,
        page.body,
        &orientation,
    );

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
    group(chrome, canvas, page, "Margins", right, page.body, &margins);

    // `down` denies the alignment group.
    let second = page.at(GROUP);
    if down {
        denied(canvas, page, "Alignment", page.left, second);
    } else {
        let alignment = [true, false].map(|justified| Button {
            art: plate(ALIGNMENT_ART[usize::from(!justified)], false),
            chosen: justified == settings.justified,
            action: Action::Justified(justified),
        });
        group(
            chrome,
            canvas,
            page,
            "Alignment",
            page.left,
            second,
            &alignment,
        );
    }

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
    group(chrome, canvas, page, "Spacing", right, second, &spacing);

    let third = page.at(GROUP * 2.0);
    if reading.panel.columns_offered {
        let columns = [1u8, 2].map(|count| Button {
            art: plate(COLUMN_ART[usize::from(count == 2)], down),
            chosen: count == settings.columns,
            action: Action::Columns(count),
        });
        group(chrome, canvas, page, "Column", page.left, third, &columns);
    }
    if reading.hyphenates {
        toggle(
            chrome,
            canvas,
            page,
            "Hyphenate words that extend beyond the margin",
            page.at(GROUP * 2.0 + 200.0),
            settings.hyphenate,
            &Action::Hyphenate,
        );
    }
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

/// A labelled row of icon buttons, `chosen` in the heavier border.
fn group(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    page: &Page,
    label: &str,
    x: f32,
    y: f32,
    buttons: &[Button],
) {
    let art = canvas.art();
    canvas.text(
        label,
        34.0 * page.unit,
        canvas.theme.ink,
        false,
        (x, y),
        Align::Left,
    );
    let tallest = buttons
        .iter()
        .map(|button| button.art.size().1)
        .fold(0.0f32, f32::max);
    let mut at = x;
    for button in buttons {
        let (width, height) = button.art.size();
        let top = y + 54.0 * page.unit + (tallest - height) / 2.0 * art;
        control(chrome, canvas, button, (at, top), art);
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
) {
    let ink = canvas.theme.ink;
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

/// A group the book denies, greyed under what the catalogue calls it.
fn denied(canvas: &mut Canvas<'_, '_>, page: &Page, label: &str, x: f32, y: f32) {
    let unit = page.unit;
    let faint = canvas.theme.faint;
    canvas.text(label, 34.0 * unit, faint, false, (x, y), Align::Left);
    canvas.text(
        "Not Available",
        30.0 * unit,
        faint,
        false,
        (x, y + 40.0 * unit),
        Align::Left,
    );
}

/// A row with a box at its right, checked where the setting is on.
fn toggle(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    page: &Page,
    label: &str,
    y: f32,
    on: bool,
    action: &dyn Fn(bool) -> Action,
) {
    let unit = page.unit;
    let theme = canvas.theme;
    canvas.text(
        label,
        32.0 * unit,
        theme.ink,
        false,
        (page.left, y),
        Align::Left,
    );
    let box_ = Rect::new(
        page.right - 44.0 * unit,
        y - 4.0 * unit,
        44.0 * unit,
        44.0 * unit,
    );
    canvas.round_stroke(box_, 4.0 * unit, theme.ink, 3.0 * unit);
    if on {
        check(canvas, box_.inset_by(9.0 * unit));
    }
    chrome.add(
        Rect::new(page.left, y - 12.0 * unit, page.width(), 60.0 * unit),
        action(!on),
    );
}

fn more(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, page: &Page, reading: &Reading<'_>) {
    let unit = page.unit;
    let theme = canvas.theme;

    // The screen the emulator draws, which a Kindle has in hardware.
    let mut y = page.body;
    canvas.text(
        "Screen",
        34.0 * unit,
        theme.ink,
        false,
        (page.left, y),
        Align::Left,
    );
    for (n, device) in Device::ALL.into_iter().enumerate() {
        let box_ = Rect::new(
            page.right - (2 - n) as f32 * 200.0 * unit + 10.0 * unit,
            y - 10.0 * unit,
            190.0 * unit,
            56.0 * unit,
        );
        let chosen = reading.device == Some(device);
        canvas.round_stroke(
            box_,
            6.0 * unit,
            theme.ink,
            if chosen { 5.0 } else { 2.0 } * unit,
        );
        canvas.text(
            device.name(),
            30.0 * unit,
            theme.ink,
            chosen,
            (box_.x + box_.width / 2.0, box_.y + 12.0 * unit),
            Align::Center,
        );
        chrome.add(box_, Action::Screen(device));
    }
    canvas.rule(
        page.left,
        page.right,
        y + 56.0 * unit,
        2.0 * unit,
        theme.faint,
    );

    y += 84.0 * unit;
    canvas.text(
        "Reading Progress",
        34.0 * unit,
        theme.ink,
        false,
        (page.left, y),
        Align::Left,
    );
    canvas.text(
        reading.settings.progress.label(),
        30.0 * unit,
        theme.faint,
        false,
        (page.right - 40.0 * unit, y + 4.0 * unit),
        Align::Right,
    );
    chevron(
        canvas,
        (page.right - 14.0 * unit, y + 20.0 * unit),
        16.0 * unit,
        false,
    );
    chrome.add(
        Rect::new(page.left, y - 14.0 * unit, page.width(), 70.0 * unit),
        Action::Pane(AaPane::ReadingProgress),
    );
    canvas.rule(
        page.left,
        page.right,
        y + 56.0 * unit,
        2.0 * unit,
        theme.faint,
    );

    for row in [
        "About This Book",
        "Book Mentions",
        "Highlight Menu",
        "Page Turn Animation",
        "Popular Highlights",
        "Show Clock While Reading",
        "Word Wise",
    ] {
        y += 84.0 * unit;
        canvas.text(
            row,
            34.0 * unit,
            theme.faint,
            false,
            (page.left, y),
            Align::Left,
        );
        canvas.rule(
            page.left,
            page.right,
            y + 56.0 * unit,
            2.0 * unit,
            theme.faint,
        );
    }
}

/// The screen behind More's `Reading Progress` row: what the bar below the
/// page states.
fn reading_progress(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    page: &Page,
    reading: &Reading<'_>,
) {
    let unit = page.unit;
    let theme = canvas.theme;
    let chosen = reading.settings.progress;

    for (n, mode) in Progress::ALL
        .into_iter()
        .filter(|mode| mode.offered(reading.numbered, reading.chaptered))
        .enumerate()
    {
        let y = page.at(n as f32 * 84.0);
        let dot = (page.left + 18.0 * unit, y + 18.0 * unit);
        canvas.circle(dot, 20.0 * unit, theme.ink, false);
        if mode == chosen {
            canvas.circle(dot, 11.0 * unit, theme.ink, true);
        }
        canvas.text(
            mode.label(),
            34.0 * unit,
            theme.ink,
            false,
            (page.left + 56.0 * unit, y - 4.0 * unit),
            Align::Left,
        );
        chrome.add(
            Rect::new(page.left, y - 14.0 * unit, page.width(), 70.0 * unit),
            Action::Progress(mode),
        );
        canvas.rule(
            page.left,
            page.right,
            y + 56.0 * unit,
            2.0 * unit,
            theme.faint,
        );
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
