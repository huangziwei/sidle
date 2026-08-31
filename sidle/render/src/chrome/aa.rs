//! The `Aa` sheet: a tab strip over Themes, Font, Layout and More, and the
//! screens a row opens over one of them.

use super::{AaPane, AaTab, Action, Canvas, Chrome, Reading, text::Align};
use crate::geom::Rect;
use crate::settings::{
    CHARACTER_SPACINGS, Device, FINE_LINE_SPACINGS, PARAGRAPH_SPACINGS, Preset, Progress, Stop,
    WORD_SPACINGS,
};

/// The share of the panel the sheet covers.
const SHEET: f32 = 0.62;

/// Height of one row, and of one carrying a slider, in reference dots.
const ROW: f32 = 84.0;
const SLIDER_ROW: f32 = 112.0;

/// Draw the sheet and whichever screen is showing.
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

    // Anything outside the sheet closes it.
    chrome.add(Rect::new(0.0, 0.0, panel.width, top), Action::Close);

    let body = if chrome.pane == AaPane::Tab {
        tabs(chrome, canvas, top, unit)
    } else {
        heading(chrome, canvas, top, unit)
    };

    let mut rows = Rows {
        y: body,
        left: panel.width * 0.05,
        right: panel.width * 0.95,
        unit,
    };
    match chrome.pane {
        AaPane::Tab => match chrome.tab {
            AaTab::Themes => themes(chrome, canvas, &mut rows, reading),
            AaTab::Font => font(chrome, canvas, &mut rows, reading),
            AaTab::Layout => layout(chrome, canvas, &mut rows, reading),
            AaTab::More => more(chrome, canvas, &mut rows),
        },
        AaPane::FontList => font_list(chrome, canvas, &mut rows, reading),
        AaPane::Spacing => spacing(chrome, canvas, &mut rows, reading),
        AaPane::ReadingProgress => reading_progress(chrome, canvas, &mut rows, reading),
        AaPane::Screen => screens(chrome, canvas, &mut rows, reading),
    }
}

/// The tab strip, returning where the body starts.
fn tabs(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, top: f32, unit: f32) -> f32 {
    let panel = canvas.panel;
    let theme = canvas.theme;
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
    strip + 90.0 * unit
}

/// A sub-screen's back arrow and title, returning where its body starts.
fn heading(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, top: f32, unit: f32) -> f32 {
    let panel = canvas.panel;
    let theme = canvas.theme;
    let left = panel.width * 0.05;
    let strip = top + 62.0 * unit;

    chevron(canvas, (left + 14.0 * unit, strip), 18.0 * unit, false);
    canvas.text(
        chrome.pane.title(),
        38.0 * unit,
        theme.ink,
        true,
        (left + 54.0 * unit, strip - 24.0 * unit),
        Align::Left,
    );
    chrome.add(
        Rect::new(0.0, top, panel.width, 100.0 * unit),
        Action::Pane(AaPane::Tab),
    );
    canvas.rule(
        0.0,
        panel.width,
        strip + 36.0 * unit,
        2.0 * unit,
        theme.faint,
    );
    strip + 90.0 * unit
}

/// Where the next row goes, and how wide a row is.
struct Rows {
    y: f32,
    left: f32,
    right: f32,
    unit: f32,
}

impl Rows {
    /// The area of a row `height` reference dots tall, advancing past it.
    fn take(&mut self, height: f32) -> Rect {
        let rect = Rect::new(
            self.left,
            self.y,
            self.right - self.left,
            height * self.unit,
        );
        self.y += height * self.unit;
        rect
    }

    fn rule(&self, canvas: &mut Canvas<'_, '_>) {
        let faint = canvas.theme.faint;
        canvas.rule(self.left, self.right, self.y, 2.0 * self.unit, faint);
    }
}

/// A row whose label sits left and whose value sits right, with a chevron.
fn link(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    rows: &mut Rows,
    label: &str,
    value: &str,
    pane: AaPane,
) {
    let unit = rows.unit;
    let theme = canvas.theme;
    let row = rows.take(ROW);
    canvas.text(
        label,
        34.0 * unit,
        theme.ink,
        false,
        (row.x, row.y + 16.0 * unit),
        Align::Left,
    );
    canvas.text(
        value,
        32.0 * unit,
        theme.faint,
        false,
        (row.right() - 40.0 * unit, row.y + 18.0 * unit),
        Align::Right,
    );
    chevron(
        canvas,
        (row.right() - 16.0 * unit, row.y + 30.0 * unit),
        16.0 * unit,
        true,
    );
    chrome.add(row, Action::Pane(pane));
    rows.rule(canvas);
}

/// A row of prose with a switch at its right end.
fn toggle(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    rows: &mut Rows,
    label: &str,
    on: bool,
    action: Option<Action>,
) {
    let unit = rows.unit;
    let theme = canvas.theme;
    let row = rows.take(ROW);
    let ink = if action.is_some() {
        theme.ink
    } else {
        theme.faint
    };
    canvas.text(
        label,
        34.0 * unit,
        ink,
        false,
        (row.x, row.y + 16.0 * unit),
        Align::Left,
    );

    let track = Rect::new(
        row.right() - 76.0 * unit,
        row.y + 14.0 * unit,
        76.0 * unit,
        40.0 * unit,
    );
    canvas.stroke(track, ink, 3.0 * unit);
    let knob = if on {
        track.right() - 20.0 * unit
    } else {
        track.x + 20.0 * unit
    };
    canvas.circle((knob, track.y + track.height / 2.0), 14.0 * unit, ink, on);
    if let Some(action) = action {
        chrome.add(row, action);
    }
    rows.rule(canvas);
}

/// A row of buttons, the one in force marked.
fn choices<T: PartialEq + Copy>(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    rows: &mut Rows,
    label: &str,
    options: &[(T, &str)],
    chosen: T,
    action: impl Fn(T) -> Action,
) {
    let unit = rows.unit;
    let theme = canvas.theme;
    let row = rows.take(ROW);
    canvas.text(
        label,
        34.0 * unit,
        theme.ink,
        false,
        (row.x, row.y + 16.0 * unit),
        Align::Left,
    );

    let width = 128.0 * unit;
    let mut x = row.right() - width * options.len() as f32;
    for (value, name) in options {
        let box_ = Rect::new(x, row.y + 8.0 * unit, width - 8.0 * unit, 52.0 * unit);
        let marked = *value == chosen;
        canvas.stroke(box_, theme.ink, if marked { 5.0 } else { 2.0 } * unit);
        canvas.text(
            name,
            28.0 * unit,
            theme.ink,
            marked,
            (box_.x + box_.width / 2.0, box_.y + 10.0 * unit),
            Align::Center,
        );
        chrome.add(box_, action(*value));
        x += width;
    }
    rows.rule(canvas);
}

/// A list of options with a dot beside the one in force.
fn radios<T: PartialEq + Copy>(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    rows: &mut Rows,
    options: &[(T, String)],
    chosen: T,
    action: impl Fn(T) -> Action,
) {
    let unit = rows.unit;
    let theme = canvas.theme;
    for (value, name) in options {
        let row = rows.take(ROW);
        let dot = (row.x + 18.0 * unit, row.y + 30.0 * unit);
        canvas.circle(dot, 18.0 * unit, theme.ink, false);
        if *value == chosen {
            canvas.circle(dot, 10.0 * unit, theme.ink, true);
        }
        canvas.text(
            name,
            34.0 * unit,
            theme.ink,
            false,
            (row.x + 56.0 * unit, row.y + 12.0 * unit),
            Align::Left,
        );
        chrome.add(row, action(*value));
        rows.rule(canvas);
    }
}

/// A row of stops with a `−` and a `+`, the stops up to the chosen one filled.
fn slider(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    rows: &mut Rows,
    label: &str,
    at: usize,
    stops: usize,
    action: impl Fn(usize) -> Action,
) {
    let unit = rows.unit;
    let theme = canvas.theme;
    let row = rows.take(SLIDER_ROW);
    let y = row.y + 66.0 * unit;
    canvas.text(
        label,
        34.0 * unit,
        theme.ink,
        false,
        (row.x, row.y + 4.0 * unit),
        Align::Left,
    );

    let track_left = row.x + 76.0 * unit;
    let track_right = row.right() - 76.0 * unit;
    let step = (track_right - track_left) / stops.max(1) as f32;

    canvas.fill(
        Rect::new(row.x, y - 2.0 * unit, 30.0 * unit, 5.0 * unit),
        theme.ink,
    );
    chrome.add(
        Rect::new(
            row.x - 10.0 * unit,
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
            canvas.fill(box_, theme.ink);
        } else {
            canvas.stroke(box_, theme.ink, 2.0 * unit);
        }
        chrome.add(
            Rect::new(box_.x, y - 30.0 * unit, step, 60.0 * unit),
            action(n),
        );
    }

    let plus = row.right() - 30.0 * unit;
    canvas.fill(
        Rect::new(plus, y - 2.0 * unit, 30.0 * unit, 5.0 * unit),
        theme.ink,
    );
    canvas.fill(
        Rect::new(plus + 12.0 * unit, y - 14.0 * unit, 5.0 * unit, 30.0 * unit),
        theme.ink,
    );
    chrome.add(
        Rect::new(
            plus - 20.0 * unit,
            y - 30.0 * unit,
            70.0 * unit,
            60.0 * unit,
        ),
        action((at + 1).min(stops.saturating_sub(1))),
    );
    rows.rule(canvas);
}

fn themes(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    rows: &mut Rows,
    reading: &Reading<'_>,
) {
    let unit = rows.unit;
    let theme = canvas.theme;
    let dark = chrome.dark;
    let row = rows.take(ROW);
    canvas.text(
        "Page color",
        34.0 * unit,
        theme.ink,
        false,
        (row.x, row.y + 16.0 * unit),
        Align::Left,
    );
    for (n, night) in [false, true].into_iter().enumerate() {
        let centre = (
            row.right() - 90.0 * unit + n as f32 * 72.0 * unit,
            row.y + 30.0 * unit,
        );
        canvas.circle(centre, 28.0 * unit, theme.ink, night);
        if night == dark {
            canvas.circle(centre, 34.0 * unit, theme.ink, false);
        }
        chrome.add(
            Rect::new(centre.0 - 36.0 * unit, row.y, 72.0 * unit, row.height),
            Action::PageColor(night),
        );
    }
    rows.rule(canvas);

    toggle(chrome, canvas, rows, "Use system theme", false, None);

    let here = reading.settings.matches(reading.panel);
    let presets: Vec<(Option<Preset>, String)> = Preset::ALL
        .into_iter()
        .map(|preset| (Some(preset), preset.label().to_string()))
        .collect();
    radios(chrome, canvas, rows, &presets, here, |preset| {
        Action::Preset(preset.unwrap_or(Preset::Standard))
    });

    let screen = reading.device.map_or("Custom", Device::name);
    link(chrome, canvas, rows, "Device", screen, AaPane::Screen);
}

fn font(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, rows: &mut Rows, reading: &Reading<'_>) {
    let settings = reading.settings;
    let panel = reading.panel;
    let family = reading
        .families
        .get(settings.family)
        .map_or("Publisher Font", String::as_str)
        .to_string();
    link(
        chrome,
        canvas,
        rows,
        "Font family",
        &family,
        AaPane::FontList,
    );

    let sizes = panel
        .font_sizes
        .values()
        .map(Vec::len)
        .max()
        .unwrap_or(1)
        .max(1);
    slider(
        chrome,
        canvas,
        rows,
        "Size",
        settings.font_size,
        sizes,
        Action::FontSize,
    );
    slider(
        chrome,
        canvas,
        rows,
        "Bold",
        settings.boldness,
        panel.boldness.len().max(1),
        Action::Bold,
    );
    link(chrome, canvas, rows, "Spacing", "", AaPane::Spacing);
}

fn layout(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    rows: &mut Rows,
    reading: &Reading<'_>,
) {
    let settings = reading.settings;
    choices(
        chrome,
        canvas,
        rows,
        "Orientation",
        &[(false, "Portrait"), (true, "Landscape")],
        reading.vertical,
        Action::Vertical,
    );
    if reading.panel.columns_offered {
        choices(
            chrome,
            canvas,
            rows,
            "Column",
            &[(1u8, "One"), (2u8, "Two")],
            settings.columns,
            Action::Columns,
        );
    }
    choices(
        chrome,
        canvas,
        rows,
        "Margins",
        &[
            (Stop::Narrow, "Narrow"),
            (Stop::Normal, "Normal"),
            (Stop::Wide, "Wide"),
        ],
        settings.margins,
        Action::Margins,
    );
    choices(
        chrome,
        canvas,
        rows,
        "Alignment",
        &[(true, "Justified"), (false, "Ragged")],
        settings.justified,
        Action::Justified,
    );
    if !settings.fine_spacing {
        choices(
            chrome,
            canvas,
            rows,
            "Spacing",
            &[
                (Stop::Narrow, "Narrow"),
                (Stop::Normal, "Normal"),
                (Stop::Wide, "Wide"),
            ],
            settings.line_spacing,
            Action::Spacing,
        );
    }
    toggle(
        chrome,
        canvas,
        rows,
        "Hyphenation",
        settings.hyphenate,
        Some(Action::Hyphenate(!settings.hyphenate)),
    );
}

fn more(chrome: &mut Chrome, canvas: &mut Canvas<'_, '_>, rows: &mut Rows) {
    for label in [
        "About this book",
        "Book mentions",
        "Word wise",
        "Popular highlights",
        "Vocabulary builder",
        "Assistive reader",
        "Annotation menu",
        "Page turn animations",
        "Show clock while reading",
    ] {
        toggle(chrome, canvas, rows, label, false, None);
    }
    link(
        chrome,
        canvas,
        rows,
        "Reading progress",
        "",
        AaPane::ReadingProgress,
    );
}

fn font_list(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    rows: &mut Rows,
    reading: &Reading<'_>,
) {
    let options: Vec<(usize, String)> = reading
        .families
        .iter()
        .enumerate()
        .map(|(n, family)| (n, family.clone()))
        .collect();
    radios(
        chrome,
        canvas,
        rows,
        &options,
        reading.settings.family,
        Action::Family,
    );
}

fn spacing(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    rows: &mut Rows,
    reading: &Reading<'_>,
) {
    let settings = reading.settings;
    slider(
        chrome,
        canvas,
        rows,
        "Line spacing",
        settings.fine_line_spacing,
        FINE_LINE_SPACINGS.len(),
        Action::FineLineSpacing,
    );
    slider(
        chrome,
        canvas,
        rows,
        "Paragraph spacing",
        settings.paragraph_spacing,
        PARAGRAPH_SPACINGS.len(),
        Action::ParagraphSpacing,
    );
    slider(
        chrome,
        canvas,
        rows,
        "Word spacing",
        settings.word_spacing,
        WORD_SPACINGS.len(),
        Action::WordSpacing,
    );
    slider(
        chrome,
        canvas,
        rows,
        "Character spacing",
        settings.character_spacing,
        CHARACTER_SPACINGS.len(),
        Action::CharacterSpacing,
    );

    let unit = rows.unit;
    let ink = canvas.theme.ink;
    let row = rows.take(ROW);
    canvas.text(
        "Reset to default",
        34.0 * unit,
        ink,
        false,
        (row.x, row.y + 16.0 * unit),
        Align::Left,
    );
    chrome.add(row, Action::Spacing(Stop::Normal));
}

fn reading_progress(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    rows: &mut Rows,
    reading: &Reading<'_>,
) {
    let options: Vec<(Progress, String)> = Progress::ALL
        .into_iter()
        .filter(|mode| mode.offered(reading.numbered, reading.chaptered))
        .map(|mode| (mode, mode.label().to_string()))
        .collect();
    radios(
        chrome,
        canvas,
        rows,
        &options,
        reading.settings.progress,
        Action::Progress,
    );
}

fn screens(
    chrome: &mut Chrome,
    canvas: &mut Canvas<'_, '_>,
    rows: &mut Rows,
    reading: &Reading<'_>,
) {
    let options: Vec<(Device, String)> = Device::ALL
        .into_iter()
        .map(|device| {
            let panel = device.panel();
            (
                device,
                format!(
                    "{}  {}×{}",
                    device.name(),
                    panel.size.width as u32,
                    panel.size.height as u32
                ),
            )
        })
        .collect();
    let here = reading.device.unwrap_or(Device::Colorsoft);
    radios(chrome, canvas, rows, &options, here, Action::Screen);
}

/// A chevron pointing right, or back the way a heading's does.
fn chevron(canvas: &mut Canvas<'_, '_>, at: (f32, f32), size: f32, forward: bool) {
    let ink = canvas.theme.ink;
    let weight = size * 0.22;
    let steps = (size / weight).max(2.0) as usize;
    for step in 0..steps {
        let t = step as f32 / steps as f32 * size;
        let x = if forward {
            at.0 - size + t
        } else {
            at.0 + size - t
        };
        canvas.fill(Rect::new(x, at.1 - t - weight, weight, weight), ink);
        canvas.fill(Rect::new(x, at.1 + t, weight, weight), ink);
    }
}
