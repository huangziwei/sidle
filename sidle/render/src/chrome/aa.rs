//! The `Aa` panel: a tab strip over Themes, Font, Layout and More.

use super::{AaTab, Action, Canvas, Chrome, Ladder, text::Align};
use crate::geom::Rect;
use crate::settings::Stop;

/// The share of the panel the sheet covers.
const SHEET: f32 = 0.6;

/// Draw the sheet and everything on the tab in hand.
pub fn draw(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, ladder: &Ladder) {
    let panel = canvas.panel;
    let unit = panel.height / 1696.0;
    let theme = canvas.theme;
    let top = panel.height * (1.0 - SHEET);

    canvas.fill(
        Rect::new(0.0, top, panel.width, panel.height - top),
        theme.page,
    );
    canvas.rule(0.0, panel.width, top, 4.0 * unit, theme.ink);

    // Anything outside the sheet closes it.
    chrome.add(Rect::new(0.0, 0.0, panel.width, top), Action::Close);

    let strip = top + 62.0 * unit;
    let mut x = panel.width * 0.05;
    for tab in AaTab::ALL {
        let chosen = tab == chrome.tab;
        let label = canvas.text(
            tab.label(),
            36.0 * unit,
            theme.ink,
            chosen,
            (x, strip - 22.0 * unit),
            Align::Left,
        );
        if chosen {
            canvas.rule(
                label.x,
                label.right(),
                strip + 34.0 * unit,
                6.0 * unit,
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
        strip + 36.0 * unit,
        2.0 * unit,
        theme.faint,
    );

    let body = strip + 90.0 * unit;
    match chrome.tab {
        AaTab::Themes => themes(chrome, canvas, body, unit),
        AaTab::Font => font(chrome, canvas, body, unit, ladder),
        AaTab::Layout => layout(chrome, canvas, body, unit, ladder),
        AaTab::More => more(canvas, body, unit),
    }
}

fn themes(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, top: f32, unit: f32) {
    let panel = canvas.panel;
    let theme = canvas.theme;
    let left = panel.width * 0.05;
    let right = panel.width * 0.95;

    canvas.text(
        "Page Color",
        36.0 * unit,
        theme.ink,
        false,
        (left, top),
        Align::Left,
    );
    let pale = (right - 84.0 * unit, top + 18.0 * unit);
    let dark = (right - 18.0 * unit, top + 18.0 * unit);
    canvas.circle(pale, 26.0 * unit, theme.ink, false);
    canvas.circle(pale, 18.0 * unit, theme.page, true);
    canvas.circle(dark, 26.0 * unit, theme.ink, true);
    chrome.add(
        Rect::new(
            pale.0 - 30.0 * unit,
            top - 8.0 * unit,
            60.0 * unit,
            60.0 * unit,
        ),
        Action::PageColor(false),
    );
    chrome.add(
        Rect::new(
            dark.0 - 30.0 * unit,
            top - 8.0 * unit,
            60.0 * unit,
            60.0 * unit,
        ),
        Action::PageColor(true),
    );
    canvas.rule(left, right, top + 62.0 * unit, 2.0 * unit, theme.ink);

    let names = ["Custom", "Compact", "Standard", "Large"];
    for (n, name) in names.into_iter().enumerate() {
        let column = (n % 2) as f32;
        let row = (n / 2) as f32;
        let x = left + column * (panel.width * 0.45);
        let y = top + 110.0 * unit + row * 100.0 * unit;
        canvas.stroke(
            Rect::new(x, y, 56.0 * unit, 56.0 * unit),
            theme.ink,
            3.0 * unit,
        );
        for line in 0..4 {
            let weight = if n == 3 { 5.0 } else { 3.0 };
            canvas.fill(
                Rect::new(
                    x + 12.0 * unit,
                    y + 12.0 * unit + line as f32 * 10.0 * unit,
                    32.0 * unit,
                    weight * unit,
                ),
                theme.ink,
            );
        }
        canvas.text(
            name,
            34.0 * unit,
            theme.ink,
            n == 0,
            (x + 76.0 * unit, y + 6.0 * unit),
            Align::Left,
        );
    }
}

fn font(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, top: f32, unit: f32, ladder: &Ladder) {
    let panel = canvas.panel;
    let theme = canvas.theme;
    let left = panel.width * 0.05;
    let right = panel.width * 0.95;

    for (n, family) in ladder.families.iter().enumerate() {
        let column = (n % 2) as f32;
        let row = (n / 2) as f32;
        let x = left + column * (panel.width * 0.45);
        let y = top + row * 74.0 * unit;
        let dot = (x + 18.0 * unit, y + 18.0 * unit);
        canvas.circle(dot, 18.0 * unit, theme.ink, false);
        if n == ladder.family {
            canvas.circle(dot, 10.0 * unit, theme.ink, true);
        }
        canvas.text(
            family,
            34.0 * unit,
            theme.ink,
            false,
            (x + 46.0 * unit, y - 4.0 * unit),
            Align::Left,
        );
        chrome.add(
            Rect::new(x, y - 8.0 * unit, panel.width * 0.4, 56.0 * unit),
            Action::Family(n),
        );
    }

    let rows = top + (ladder.families.len().div_ceil(2)) as f32 * 74.0 * unit + 30.0 * unit;
    canvas.rule(left, right, rows, 2.0 * unit, theme.faint);
    slider(
        chrome,
        canvas,
        Slider {
            label: "Bold",
            y: rows + 60.0 * unit,
            unit,
            at: ladder.bold,
            stops: ladder.bolds,
            bold: true,
        },
    );
    canvas.rule(left, right, rows + 130.0 * unit, 2.0 * unit, theme.faint);
    slider(
        chrome,
        canvas,
        Slider {
            label: "Size",
            y: rows + 190.0 * unit,
            unit,
            at: ladder.font_size,
            stops: ladder.font_sizes,
            bold: false,
        },
    );
}

/// A row of stops with a `−` and a `+`, and where the chosen one sits.
struct Slider<'a> {
    label: &'a str,
    y: f32,
    unit: f32,
    at: usize,
    stops: usize,
    /// Which `Action` a stop asks for.
    bold: bool,
}

/// Draw one `Slider`, the stops up to its own filled.
fn slider(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, s: Slider<'_>) {
    let Slider {
        label,
        y,
        unit,
        at,
        stops,
        bold,
    } = s;
    let panel = canvas.panel;
    let theme = canvas.theme;
    let left = panel.width * 0.05;
    canvas.text(
        label,
        34.0 * unit,
        theme.ink,
        false,
        (left, y - 20.0 * unit),
        Align::Left,
    );

    let track_left = panel.width * 0.36;
    let track_right = panel.width * 0.88;
    let step = (track_right - track_left) / stops.max(1) as f32;
    let choose = |n: usize| {
        if bold {
            Action::Bold(n)
        } else {
            Action::FontSize(n)
        }
    };

    canvas.fill(
        Rect::new(
            track_left - 44.0 * unit,
            y - 2.0 * unit,
            26.0 * unit,
            5.0 * unit,
        ),
        theme.ink,
    );
    chrome.add(
        Rect::new(
            track_left - 64.0 * unit,
            y - 26.0 * unit,
            60.0 * unit,
            52.0 * unit,
        ),
        choose(at.saturating_sub(1)),
    );
    for n in 0..stops {
        let box_ = Rect::new(
            track_left + n as f32 * step,
            y - 9.0 * unit,
            step * 0.8,
            18.0 * unit,
        );
        if n <= at {
            canvas.fill(box_, theme.ink);
        } else {
            canvas.stroke(box_, theme.ink, 2.0 * unit);
        }
        chrome.add(box_, choose(n));
    }
    canvas.text(
        &format!("{}", at + 1),
        26.0 * unit,
        theme.ink,
        false,
        (track_left + (at as f32 + 0.4) * step, y - 46.0 * unit),
        Align::Center,
    );
    canvas.fill(
        Rect::new(
            track_right + 20.0 * unit,
            y - 2.0 * unit,
            26.0 * unit,
            5.0 * unit,
        ),
        theme.ink,
    );
    canvas.fill(
        Rect::new(
            track_right + 30.0 * unit,
            y - 12.0 * unit,
            5.0 * unit,
            26.0 * unit,
        ),
        theme.ink,
    );
    chrome.add(
        Rect::new(
            track_right + 6.0 * unit,
            y - 26.0 * unit,
            60.0 * unit,
            52.0 * unit,
        ),
        choose((at + 1).min(stops.saturating_sub(1))),
    );
}

fn layout(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, top: f32, unit: f32, ladder: &Ladder) {
    let panel = canvas.panel;
    let theme = canvas.theme;
    let left = panel.width * 0.06;
    let mid = panel.width * 0.43;

    canvas.text(
        "Orientation",
        34.0 * unit,
        theme.ink,
        false,
        (left, top),
        Align::Left,
    );
    for (n, vertical) in [true, false].into_iter().enumerate() {
        let x = left + n as f32 * 84.0 * unit;
        let y = top + 54.0 * unit;
        let box_ = Rect::new(x, y, 68.0 * unit, 88.0 * unit);
        canvas.stroke(
            box_,
            theme.ink,
            if vertical == ladder.vertical {
                5.0
            } else {
                2.0
            } * unit,
        );
        rules_in(canvas, box_, vertical, unit);
        chrome.add(box_, Action::Vertical(vertical));
    }

    canvas.text(
        "Margins",
        34.0 * unit,
        theme.ink,
        false,
        (mid, top - 12.0 * unit),
        Align::Left,
    );
    for (n, stop) in [Stop::Narrow, Stop::Normal, Stop::Wide]
        .into_iter()
        .enumerate()
    {
        let x = mid + n as f32 * 84.0 * unit;
        let y = top + 42.0 * unit;
        let box_ = Rect::new(x, y, 68.0 * unit, 88.0 * unit);
        canvas.stroke(
            box_,
            theme.ink,
            if stop == ladder.margins { 5.0 } else { 2.0 } * unit,
        );
        rules_in(canvas, box_, true, unit);
        chrome.add(box_, Action::Margins(stop));
    }

    let second = top + 180.0 * unit;
    canvas.text(
        "Alignment",
        34.0 * unit,
        theme.ink,
        false,
        (left, second),
        Align::Left,
    );
    for (n, justified) in [true, false].into_iter().enumerate() {
        let x = left + n as f32 * 100.0 * unit;
        let y = second + 54.0 * unit;
        let box_ = Rect::new(x, y, 84.0 * unit, 56.0 * unit);
        canvas.stroke(
            box_,
            theme.ink,
            if justified == ladder.justified {
                5.0
            } else {
                2.0
            } * unit,
        );
        for line in 0..3 {
            let width = if justified || line < 2 { 60.0 } else { 40.0 };
            canvas.fill(
                Rect::new(
                    x + 12.0 * unit,
                    y + 12.0 * unit + line as f32 * 14.0 * unit,
                    width * unit,
                    3.0 * unit,
                ),
                theme.ink,
            );
        }
        chrome.add(box_, Action::Justified(justified));
    }

    canvas.text(
        "Spacing",
        34.0 * unit,
        theme.ink,
        false,
        (mid, second - 12.0 * unit),
        Align::Left,
    );
    for (n, stop) in [Stop::Narrow, Stop::Normal, Stop::Wide]
        .into_iter()
        .enumerate()
    {
        let x = mid + n as f32 * 84.0 * unit;
        let y = second + 42.0 * unit;
        let box_ = Rect::new(x, y, 68.0 * unit, 88.0 * unit);
        canvas.stroke(
            box_,
            theme.ink,
            if stop == ladder.spacing { 5.0 } else { 2.0 } * unit,
        );
        rules_in(canvas, box_, true, unit);
        chrome.add(box_, Action::Spacing(stop));
    }
}

/// The little ruled page inside a Layout button.
fn rules_in(canvas: &mut Canvas<'_, '_>, box_: Rect, vertical: bool, unit: f32) {
    let ink = canvas.theme.ink;
    for n in 0..5 {
        if vertical {
            let x = box_.x + 12.0 * unit + n as f32 * 10.0 * unit;
            canvas.fill(
                Rect::new(
                    x,
                    box_.y + 12.0 * unit,
                    3.0 * unit,
                    box_.height - 24.0 * unit,
                ),
                ink,
            );
        } else {
            let y = box_.y + 14.0 * unit + n as f32 * 13.0 * unit;
            canvas.fill(
                Rect::new(
                    box_.x + 10.0 * unit,
                    y,
                    box_.width - 20.0 * unit,
                    3.0 * unit,
                ),
                ink,
            );
        }
    }
}

fn more(canvas: &mut Canvas<'_, '_>, top: f32, unit: f32) {
    let panel = canvas.panel;
    let theme = canvas.theme;
    let left = panel.width * 0.05;
    for (n, row) in ["Reading Ruler", "About This Book", "Reading Progress"]
        .into_iter()
        .enumerate()
    {
        let y = top + n as f32 * 84.0 * unit;
        canvas.text(row, 34.0 * unit, theme.faint, false, (left, y), Align::Left);
        canvas.rule(
            left,
            panel.width * 0.95,
            y + 56.0 * unit,
            2.0 * unit,
            theme.faint,
        );
    }
}
