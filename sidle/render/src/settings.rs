//! [`Panel`] holds one screen's ladders, either a [`Device`]'s own or read by
//! [`Panel::parse`] from a profile. [`Settings`] picks a stop from each and
//! turns the pair into a [`Viewport`].

use std::collections::HashMap;
use std::path::Path;
use std::{fs, io};

use crate::flow::Viewport;
use bokai::style::TextAlign;

use crate::geom::{Edges, Size};
use crate::resolve::NORMAL_LINE_HEIGHT;
use crate::units::Metrics;

/// Line-spacing multipliers, five stops, narrow to wide. One ladder covers
/// every panel and every language.
pub const FINE_LINE_SPACINGS: [f32; 5] = [1.14, 1.35, 1.54, 1.74, 1.94];

/// Which [`FINE_LINE_SPACINGS`] stop a book opens at.
pub const DEFAULT_FINE_LINE_SPACING: usize = 1;

/// Extra space before a paragraph, in ems.
pub const PARAGRAPH_SPACINGS: [f32; 5] = [0.0, 1.0, 2.0, 3.0, 4.0];

/// Extra space at every word break, in ems.
pub const WORD_SPACINGS: [f32; 5] = [0.0, 0.22, 0.44, 0.66, 0.88];

/// Extra space between two characters, in ems.
pub const CHARACTER_SPACINGS: [f32; 5] = [0.0, 0.056, 0.112, 0.168, 0.224];

/// A screen a book is laid out for, and the [`Panel`] it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Device {
    #[default]
    Colorsoft,
    Scribe,
}

/// Font-size ladders in points, one per [`Script`], for a 300 dpi panel.
const FONT_SIZES_DEFAULT: [f32; 14] = [
    6.96, 7.92, 8.4, 9.36, 10.56, 11.52, 12.48, 13.92, 15.36, 17.04, 19.2, 22.08, 25.2, 29.28,
];
const FONT_SIZES_CJK: [f32; 14] = [
    6.96, 7.92, 8.4, 9.36, 10.56, 11.52, 12.48, 13.92, 15.36, 17.04, 19.2, 22.08, 25.2, 29.04,
];
const FONT_SIZES_INDIC: [f32; 14] = [
    8.4, 9.36, 10.56, 11.52, 12.48, 12.96, 13.44, 14.64, 15.84, 17.04, 19.68, 22.56, 25.92, 29.28,
];

impl Device {
    pub const ALL: [Device; 2] = [Device::Colorsoft, Device::Scribe];

    pub fn name(self) -> &'static str {
        match self {
            Device::Colorsoft => "Colorsoft",
            Device::Scribe => "Scribe",
        }
    }

    /// This screen's ladders.
    pub fn panel(self) -> Panel {
        let font_sizes = HashMap::from([
            (Script::Default, FONT_SIZES_DEFAULT.to_vec()),
            (Script::Cjk, FONT_SIZES_CJK.to_vec()),
            (Script::Indic, FONT_SIZES_INDIC.to_vec()),
        ]);
        let shared = Panel {
            size: Size::new(0.0, 0.0),
            dpi: 300.0,
            color: false,
            columns_offered: false,
            font_sizes,
            default_font_size: 4,
            font_size_presets: vec![2, 4, 6, 10],
            line_spacings: vec![1.14, 1.35, 1.54],
            wide_line_spacings: vec![1.3523, 1.51, 1.635],
            boldness: vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0],
            default_boldness: 0,
            boldness_presets: vec![0, 0, 1, 4],
            margins_horizontal: Vec::new(),
            margins_vertical: Vec::new(),
        };
        match self {
            Device::Colorsoft => Panel {
                size: Size::new(1272.0, 1696.0),
                color: true,
                default_boldness: 1,
                boldness_presets: vec![0, 1, 1, 4],
                margins_horizontal: vec![
                    Edges::new(65.0, 82.0, 0.0, 82.0),
                    Edges::new(65.0, 164.0, 0.0, 164.0),
                    Edges::new(65.0, 246.0, 0.0, 246.0),
                ],
                margins_vertical: vec![
                    Edges::new(82.0, 82.0, 17.0, 82.0),
                    Edges::new(164.0, 82.0, 99.0, 82.0),
                    Edges::new(246.0, 82.0, 181.0, 82.0),
                ],
                ..shared
            },
            Device::Scribe => Panel {
                size: Size::new(1860.0, 2480.0),
                columns_offered: true,
                margins_horizontal: vec![
                    Edges::new(64.0, 158.0, 8.0, 158.0),
                    Edges::new(64.0, 200.0, 8.0, 200.0),
                    Edges::new(64.0, 244.0, 8.0, 244.0),
                ],
                margins_vertical: vec![
                    Edges::new(158.0, 158.0, 102.0, 158.0),
                    Edges::new(242.0, 158.0, 186.0, 158.0),
                    Edges::new(363.0, 158.0, 307.0, 158.0),
                ],
                ..shared
            },
        }
    }
}

/// One panel's ladders.
#[derive(Debug, Clone, PartialEq)]
pub struct Panel {
    /// The whole screen, in dots.
    pub size: Size,
    pub dpi: f32,
    /// Whether the panel is colour.
    pub color: bool,
    /// Whether two columns are offered.
    pub columns_offered: bool,
    /// Font sizes in points, one ladder per [`Script`].
    pub font_sizes: HashMap<Script, Vec<f32>>,
    /// Which font-size stop a book opens at.
    pub default_font_size: usize,
    /// Which font-size stop each [`Preset`] sets.
    pub font_size_presets: Vec<usize>,
    /// Line-spacing multipliers, narrow to wide.
    pub line_spacings: Vec<f32>,
    /// The same for the languages [`Script::wide_line_spacing`] names.
    pub wide_line_spacings: Vec<f32>,
    /// Embolden weights.
    pub boldness: Vec<f32>,
    /// Which embolden weight a book opens at.
    pub default_boldness: usize,
    /// Which embolden weight each [`Preset`] sets.
    pub boldness_presets: Vec<usize>,
    /// Three widths of the four margins, in dots, for a book that reads
    /// across the page.
    pub margins_horizontal: Vec<Edges>,
    /// Three widths for one that reads down it. The vertical ladder is on the
    /// block axis and carries its own numbers, with the sides pinned at their
    /// narrow value.
    pub margins_vertical: Vec<Edges>,
}

impl Panel {
    /// Read a profile: one `key value…` per line, `#` to end of line a
    /// comment, the keys `take`, `flag`, `index`, `ladder` and `stops` name
    /// below. Each `margins_*` line states top right bottom left three times.
    pub fn parse(profile: &str) -> Result<Self, String> {
        let mut fields: HashMap<&str, Vec<f32>> = HashMap::new();
        for line in profile.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let mut parts = line.split_whitespace();
            let Some(key) = parts.next() else { continue };
            let values = parts
                .map(|v| v.parse::<f32>().map_err(|_| format!("{key}: {v}")))
                .collect::<Result<Vec<f32>, String>>()?;
            fields.insert(key, values);
        }

        let take = |key: &str| -> Result<Vec<f32>, String> {
            fields
                .get(key)
                .cloned()
                .ok_or_else(|| format!("no {key} in the profile"))
        };
        let flag = |key: &str| -> Result<bool, String> {
            Ok(take(key)?.first().copied().unwrap_or(0.0) != 0.0)
        };
        let index = |key: &str| -> Result<usize, String> {
            Ok(take(key)?.first().copied().unwrap_or(0.0) as usize)
        };
        let ladder = |key: &str| -> Result<Vec<Edges>, String> {
            let values = take(key)?;
            if values.len() % 4 != 0 {
                return Err(format!("{key}: not a whole number of edges"));
            }
            Ok(values
                .chunks_exact(4)
                .map(|e| Edges::new(e[0], e[1], e[2], e[3]))
                .collect())
        };

        let [width, height, dpi] = take("panel")?[..] else {
            return Err("panel: width height dpi".to_string());
        };
        let font_sizes = HashMap::from([
            (Script::Default, take("font_size_default")?),
            (Script::Cjk, take("font_size_cjk")?),
            (Script::Indic, take("font_size_indic")?),
        ]);

        let stops = |key: &str, fallback: Vec<usize>| -> Vec<usize> {
            fields
                .get(key)
                .map(|values| values.iter().map(|v| *v as usize).collect())
                .unwrap_or(fallback)
        };
        let built_in = Device::Colorsoft.panel();

        Ok(Self {
            size: Size::new(width, height),
            dpi,
            color: flag("color")?,
            columns_offered: flag("columns")?,
            font_sizes,
            default_font_size: index("default_font_size")?,
            font_size_presets: stops("font_size_presets", built_in.font_size_presets),
            line_spacings: take("line_spacing")?,
            wide_line_spacings: take("line_spacing_wide")?,
            boldness: take("boldness")?,
            default_boldness: index("default_boldness")?,
            boldness_presets: stops("boldness_presets", built_in.boldness_presets),
            margins_horizontal: ladder("margins_horizontal")?,
            margins_vertical: ladder("margins_vertical")?,
        })
    }

    /// Read a profile from a file.
    pub fn read(path: impl AsRef<Path>) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        Self::parse(&text).map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))
    }

    /// How this panel turns a KFX book's declared values into dots.
    pub fn metrics(&self) -> Metrics {
        Metrics::kfx(self.dpi)
    }
}

/// Which way the book reads, which the margin ladder keys on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Horizontal,
    Vertical,
}

/// A named stop for every field of [`Settings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Compact,
    Standard,
    Large,
    LowVision,
}

impl Preset {
    pub const ALL: [Preset; 4] = [
        Preset::Compact,
        Preset::Standard,
        Preset::Large,
        Preset::LowVision,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Compact => "Compact",
            Preset::Standard => "Standard",
            Preset::Large => "Large",
            Preset::LowVision => "Low Vision",
        }
    }

    fn index(self) -> usize {
        match self {
            Preset::Compact => 0,
            Preset::Standard => 1,
            Preset::Large => 2,
            Preset::LowVision => 3,
        }
    }
}

/// Which measure of progress the bar below the page states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Progress {
    PageNumber,
    TimeLeftInChapter,
    TimeLeft,
    #[default]
    Location,
    None,
}

impl Progress {
    pub const ALL: [Progress; 5] = [
        Progress::PageNumber,
        Progress::TimeLeftInChapter,
        Progress::TimeLeft,
        Progress::Location,
        Progress::None,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Progress::PageNumber => "Page in book",
            Progress::TimeLeftInChapter => "Time left in chapter",
            Progress::TimeLeft => "Time left in book",
            Progress::Location => "Location in book",
            Progress::None => "None",
        }
    }

    /// Whether a book with `pages` and `chapters` offers this mode.
    pub fn offered(self, pages: bool, chapters: bool) -> bool {
        match self {
            Progress::PageNumber => pages,
            Progress::TimeLeftInChapter => chapters,
            _ => true,
        }
    }

    /// What a book with these two opens showing.
    pub fn default_for(pages: bool, _chapters: bool) -> Self {
        if pages {
            Progress::PageNumber
        } else {
            Progress::Location
        }
    }
}

/// One of the three stops a ladder with a narrow, a normal and a wide setting
/// offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stop {
    #[default]
    Narrow,
    Normal,
    Wide,
}

impl Stop {
    fn index(self) -> usize {
        match self {
            Stop::Narrow => 0,
            Stop::Normal => 1,
            Stop::Wide => 2,
        }
    }
}

/// The script a book is set in, which chooses its font-size ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Script {
    /// Latin, Arabic, and every tag [`Script::of`] does not name.
    #[default]
    Default,
    /// Chinese, Japanese and Korean.
    Cjk,
    /// Hindi, Gujarati, Marathi, Tamil and Malayalam.
    Indic,
}

impl Script {
    /// The script a BCP 47 tag reads as.
    pub fn of(language: &str) -> Self {
        match primary_subtag(language).as_str() {
            "zh" | "ja" | "ko" => Script::Cjk,
            "hi" | "gu" | "mr" | "ta" | "ml" => Script::Indic,
            _ => Script::Default,
        }
    }

    /// Whether `language` takes [`Panel::wide_line_spacings`].
    pub fn wide_line_spacing(language: &str) -> bool {
        matches!(primary_subtag(language).as_str(), "ar" | "ja")
    }
}

/// The reading fonts a book in `language` is offered, the first the one it
/// opens in. `Publisher Font` heads every list and leaves the book's own
/// faces alone.
pub fn reading_families(language: &str) -> &'static [&'static str] {
    let tag = language.to_ascii_lowercase().replace('_', "-");
    let script = tag.split('-').nth(1).unwrap_or("");
    match (primary_subtag(&tag).as_str(), script) {
        ("ja", _) => &["Publisher Font", "Mincho", "Gothic", "Droid Serif"],
        ("zh", "hant") | ("zh", "tw") | ("zh", "hk") => &["Publisher Font", "System"],
        ("zh", _) => &["Publisher Font", "Song", "Hei", "Droid Serif", "System"],
        ("hi" | "mr", _) => &["Publisher Font", "Devanagari Serif"],
        ("gu", _) => &["Publisher Font", "Gujarati Serif"],
        ("ta", _) => &["Publisher Font", "Tamil Serif"],
        ("ml", _) => &["Publisher Font", "Malayalam Serif"],
        ("ar" | "fa", _) => &["Publisher Font", "Sakkal Kitab", "Muna"],
        ("he", _) => &["Publisher Font", "Noto Sans Hebrew"],
        _ => &[
            "Publisher Font",
            "Bookerly",
            "Amazon Ember Bold",
            "Baskerville",
            "Caecilia",
            "Droid Serif",
            "Georgia",
            "Helvetica",
            "Lucida",
            "OpenDyslexic",
            "Palatino",
        ],
    }
}

fn primary_subtag(language: &str) -> String {
    language
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// One reading configuration: where each setting sits on its ladder.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Which font-size stop.
    pub font_size: usize,
    pub line_spacing: Stop,
    /// Which [`FINE_LINE_SPACINGS`] stop `fine_spacing` takes.
    pub fine_line_spacing: usize,
    /// Whether line spacing comes off the five-stop ladder.
    pub fine_spacing: bool,
    /// Which [`PARAGRAPH_SPACINGS`] stop.
    pub paragraph_spacing: usize,
    /// Which [`WORD_SPACINGS`] stop.
    pub word_spacing: usize,
    /// Which [`CHARACTER_SPACINGS`] stop.
    pub character_spacing: usize,
    pub margins: Stop,
    /// Which embolden weight.
    pub boldness: usize,
    pub justified: bool,
    pub hyphenate: bool,
    /// One or two, and two only where the panel offers it.
    pub columns: u8,
    /// Which family of the book's own script list.
    pub family: usize,
    pub progress: Progress,
}

impl Settings {
    /// What a book opens at.
    pub fn default_for(panel: &Panel) -> Self {
        Self {
            font_size: panel.default_font_size,
            boldness: panel.default_boldness,
            line_spacing: Stop::Normal,
            fine_line_spacing: DEFAULT_FINE_LINE_SPACING,
            fine_spacing: false,
            paragraph_spacing: 0,
            word_spacing: 0,
            character_spacing: 0,
            margins: Stop::Narrow,
            justified: true,
            hyphenate: true,
            columns: 1,
            family: 0,
            progress: Progress::default(),
        }
    }

    /// The same settings with every stop [`Preset`] names.
    pub fn preset(&self, panel: &Panel, preset: Preset) -> Self {
        let stop = preset.index();
        Self {
            font_size: at(&panel.font_size_presets, stop),
            boldness: at(&panel.boldness_presets, stop),
            line_spacing: if preset == Preset::Compact {
                Stop::Narrow
            } else {
                Stop::Normal
            },
            fine_line_spacing: if preset == Preset::Compact {
                0
            } else {
                DEFAULT_FINE_LINE_SPACING
            },
            paragraph_spacing: 0,
            word_spacing: 0,
            character_spacing: 0,
            margins: Stop::Narrow,
            justified: preset != Preset::LowVision,
            hyphenate: preset != Preset::LowVision,
            ..self.clone()
        }
    }

    /// Which [`Preset`] these settings are, where they are one.
    pub fn matches(&self, panel: &Panel) -> Option<Preset> {
        Preset::ALL
            .into_iter()
            .find(|preset| self.preset(panel, *preset) == *self)
    }

    /// The chosen font size, in points. A script the panel names no ladder
    /// for takes [`Script::Default`]'s.
    pub fn font_size_pt(&self, panel: &Panel, script: Script) -> f32 {
        panel
            .font_sizes
            .get(&script)
            .or_else(|| panel.font_sizes.get(&Script::Default))
            .and_then(|stops| stops.get(self.font_size.min(stops.len().saturating_sub(1))))
            .copied()
            .unwrap_or_default()
    }

    /// The chosen line-spacing multiplier.
    pub fn line_spacing(&self, panel: &Panel, language: &str) -> f32 {
        if self.fine_spacing {
            return at(&FINE_LINE_SPACINGS, self.fine_line_spacing);
        }
        let ladder = if Script::wide_line_spacing(language) {
            &panel.wide_line_spacings
        } else {
            &panel.line_spacings
        };
        at(ladder, self.line_spacing.index())
    }

    /// The chosen extra space before a paragraph, in ems.
    pub fn paragraph_spacing(&self) -> f32 {
        at(&PARAGRAPH_SPACINGS, self.paragraph_spacing)
    }

    /// The chosen extra space at a word break, in ems.
    pub fn word_spacing(&self) -> f32 {
        at(&WORD_SPACINGS, self.word_spacing)
    }

    /// The chosen extra space between two characters, in ems.
    pub fn character_spacing(&self) -> f32 {
        at(&CHARACTER_SPACINGS, self.character_spacing)
    }

    /// The chosen embolden weight.
    pub fn embolden_weight(&self, panel: &Panel) -> f32 {
        at(&panel.boldness, self.boldness)
    }

    /// The chosen margins, in dots.
    pub fn margins(&self, panel: &Panel, direction: Direction) -> Edges {
        let ladder = match direction {
            Direction::Horizontal => &panel.margins_horizontal,
            Direction::Vertical => &panel.margins_vertical,
        };
        at(ladder, self.margins.index())
    }

    /// How many columns this lays out in. A panel that offers one column lays
    /// out in one at every setting.
    pub fn columns(&self, panel: &Panel) -> u8 {
        if panel.columns_offered {
            self.columns.clamp(1, 2)
        } else {
            1
        }
    }

    /// The em a line of `language` sits on, in dots.
    pub fn em(&self, panel: &Panel, language: &str) -> f32 {
        panel
            .metrics()
            .points(self.font_size_pt(panel, Script::of(language)))
    }

    /// The distance between two baselines, in dots.
    pub fn line_height(&self, panel: &Panel, language: &str) -> f32 {
        self.em(panel, language) * NORMAL_LINE_HEIGHT * self.line_spacing(panel, language)
    }

    /// The area a page is laid out into: the whole panel, the margins the
    /// ladder gives it, and the em `rem` resolves against.
    pub fn viewport(&self, panel: &Panel, language: &str, direction: Direction) -> Viewport {
        Viewport {
            size: panel.size,
            margins: self.margins(panel, direction),
            root_font_size: self.em(panel, language),
            language: Some(language.to_string()),
            metrics: panel.metrics(),
            line_spacing: self.line_spacing(panel, language),
            embolden_weight: self.embolden_weight(panel),
            character_spacing: self.character_spacing(),
            word_spacing: self.word_spacing(),
            paragraph_spacing: self.paragraph_spacing(),
            align: if self.justified {
                TextAlign::Justify
            } else {
                TextAlign::Start
            },
        }
    }
}

/// A ladder's `index`th stop, clamped to the ladder's own last one.
fn at<T: Copy + Default>(ladder: &[T], index: usize) -> T {
    ladder
        .get(index.min(ladder.len().saturating_sub(1)))
        .copied()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round numbers in the shape a profile takes, each ladder's stops far
    /// enough apart to tell which one an answer came from.
    const PROFILE: &str = "\
panel 1000 2000 300     # width height dpi
color 1
columns 0
font_size_default 10 20 30 40
font_size_cjk 11 21 31 41
font_size_indic 12 22 32 42
default_font_size 2
line_spacing 1.0 1.5 2.0
line_spacing_wide 1.1 1.6 2.1
boldness 0 20 40
default_boldness 1
margins_horizontal 1 2 3 4  11 12 13 14  21 22 23 24
margins_vertical 5 6 7 8  15 16 17 18  25 26 27 28
";

    fn panel() -> Panel {
        Panel::parse(PROFILE).expect("the profile parses")
    }

    #[test]
    fn a_profile_states_every_ladder() {
        let panel = panel();

        assert_eq!(panel.size, Size::new(1000.0, 2000.0));
        assert_eq!(panel.dpi, 300.0);
        assert!(panel.color);
        assert!(!panel.columns_offered);
        assert_eq!(panel.font_sizes[&Script::Cjk], [11.0, 21.0, 31.0, 41.0]);
        assert_eq!(
            panel.margins_horizontal[2],
            Edges::new(21.0, 22.0, 23.0, 24.0)
        );
        assert_eq!(panel.margins_vertical[0], Edges::new(5.0, 6.0, 7.0, 8.0));
    }

    #[test]
    fn a_profile_missing_a_ladder_names_it() {
        let error = Panel::parse("panel 1 2 3").expect_err("an incomplete profile is refused");

        assert!(error.contains("font_size_default"), "{error}");
    }

    #[test]
    fn a_comment_and_a_blank_line_are_skipped() {
        let panel = Panel::parse(&PROFILE.replace("color 1", "\n# color 0\ncolor 1"))
            .expect("the profile parses");

        assert!(panel.color);
    }

    #[test]
    fn a_book_opens_on_the_stop_the_profile_names() {
        let panel = panel();
        let settings = Settings::default_for(&panel);

        assert_eq!(settings.font_size, 2);
        assert_eq!(settings.font_size_pt(&panel, Script::Default), 30.0);
        assert_eq!(settings.font_size_pt(&panel, Script::Cjk), 31.0);
        assert_eq!(settings.embolden_weight(&panel), 20.0);
        assert_eq!(
            settings.margins(&panel, Direction::Horizontal),
            Edges::new(1.0, 2.0, 3.0, 4.0)
        );
    }

    #[test]
    fn a_script_the_profile_has_no_ladder_for_takes_the_default_one() {
        let mut panel = panel();
        panel.font_sizes.remove(&Script::Indic);
        let settings = Settings::default_for(&panel);

        assert_eq!(settings.font_size_pt(&panel, Script::Indic), 30.0);
    }

    #[test]
    fn two_columns_are_only_offered_where_the_profile_says_so() {
        let mut panel = panel();
        let two = Settings {
            columns: 2,
            ..Settings::default_for(&panel)
        };

        assert_eq!(two.columns(&panel), 1);
        panel.columns_offered = true;
        assert_eq!(two.columns(&panel), 2);
    }

    #[test]
    fn arabic_and_japanese_take_the_wide_line_spacing_ladder() {
        let panel = panel();
        let settings = Settings::default_for(&panel);

        assert_eq!(settings.line_spacing(&panel, "ja"), 1.6);
        assert_eq!(settings.line_spacing(&panel, "ar"), 1.6);
        assert_eq!(settings.line_spacing(&panel, "en"), 1.5);
        assert_eq!(settings.line_spacing(&panel, "zh-Hans"), 1.5);
    }

    #[test]
    fn each_device_carries_its_own_screen_and_ladders() {
        let colorsoft = Device::Colorsoft.panel();
        let scribe = Device::Scribe.panel();

        assert_eq!(colorsoft.size, Size::new(1272.0, 1696.0));
        assert_eq!(scribe.size, Size::new(1860.0, 2480.0));
        assert!(colorsoft.color && !scribe.color);
        // Two columns resolve on the larger panel alone.
        assert!(scribe.columns_offered && !colorsoft.columns_offered);
        // A colour screen opens one stop bolder.
        assert_eq!(colorsoft.default_boldness, 1);
        assert_eq!(scribe.default_boldness, 0);
        // The vertical ladder moves onto the block axis, sides pinned narrow.
        assert_eq!(
            scribe.margins_vertical[2],
            Edges::new(363.0, 158.0, 307.0, 158.0)
        );
        assert_eq!(
            scribe.margins_horizontal[2],
            Edges::new(64.0, 244.0, 8.0, 244.0)
        );
    }

    #[test]
    fn a_preset_sets_every_setting_at_once() {
        let panel = Device::Scribe.panel();
        let opened = Settings::default_for(&panel);

        let large = opened.preset(&panel, Preset::Large);
        assert_eq!(large.font_size, 6);
        assert_eq!(large.boldness, 1);

        let low = opened.preset(&panel, Preset::LowVision);
        assert_eq!(low.font_size, 10);
        assert_eq!(low.boldness, 4);
        assert!(!low.justified);
        assert!(!low.hyphenate);

        let compact = opened.preset(&panel, Preset::Compact);
        assert_eq!(compact.line_spacing, Stop::Narrow);
    }

    #[test]
    fn a_preset_recognises_itself() {
        let panel = Device::Colorsoft.panel();
        let opened = Settings::default_for(&panel);
        let large = opened.preset(&panel, Preset::Large);

        assert_eq!(large.matches(&panel), Some(Preset::Large));
        assert_eq!(
            Settings {
                font_size: 13,
                ..large
            }
            .matches(&panel),
            None
        );
    }

    #[test]
    fn the_fine_ladder_takes_over_line_spacing_when_it_is_chosen() {
        let panel = Device::Scribe.panel();
        let mut settings = Settings::default_for(&panel);

        // Japanese takes its own three-stop ladder until the fine one is set.
        assert_eq!(settings.line_spacing(&panel, "ja"), 1.51);
        settings.fine_spacing = true;
        assert_eq!(settings.line_spacing(&panel, "ja"), 1.35);
        settings.fine_line_spacing = 4;
        assert_eq!(settings.line_spacing(&panel, "ja"), 1.94);
    }

    #[test]
    fn a_progress_mode_a_book_cannot_show_is_not_offered() {
        assert!(!Progress::PageNumber.offered(false, true));
        assert!(Progress::PageNumber.offered(true, true));
        assert!(!Progress::TimeLeftInChapter.offered(true, false));
        assert!(Progress::Location.offered(false, false));
        assert_eq!(Progress::default_for(false, true), Progress::Location);
        assert_eq!(Progress::default_for(true, true), Progress::PageNumber);
    }

    #[test]
    fn a_script_takes_the_font_list_its_own_language_offers() {
        assert_eq!(reading_families("ja")[1], "Mincho");
        assert_eq!(reading_families("zh-Hans")[1], "Song");
        assert_eq!(reading_families("zh-Hant")[1], "System");
        assert_eq!(reading_families("en-GB")[1], "Bookerly");
        // Every list leaves the book's own faces reachable.
        for tag in ["ja", "zh", "he", "ar", "ta", "en"] {
            assert_eq!(reading_families(tag)[0], "Publisher Font", "{tag}");
        }
    }

    #[test]
    fn a_language_tag_reads_down_to_its_primary_subtag() {
        assert_eq!(Script::of("zh-Hant"), Script::Cjk);
        assert_eq!(Script::of("ja"), Script::Cjk);
        assert_eq!(Script::of("en-GB"), Script::Default);
        assert_eq!(Script::of("ar"), Script::Default);
        assert_eq!(Script::of("ta"), Script::Indic);
        assert_eq!(Script::of(""), Script::Default);
    }

    #[test]
    fn the_em_is_the_stop_taken_against_the_panel() {
        let panel = panel();
        let settings = Settings::default_for(&panel);

        // 30 pt at 300 dpi is 125 dots, and 1.2 × 1.5 of that is 225.
        assert_eq!(settings.em(&panel, "en"), 125.0);
        assert_eq!(settings.line_height(&panel, "en"), 225.0);
    }

    #[test]
    fn the_viewport_is_the_panel_with_the_margins_it_chose() {
        let panel = panel();
        let viewport = Settings::default_for(&panel).viewport(&panel, "en", Direction::Vertical);

        assert_eq!(viewport.size, panel.size);
        assert_eq!(viewport.margins, Edges::new(5.0, 6.0, 7.0, 8.0));
        assert_eq!(viewport.root_font_size, 125.0);
    }
}
