//! The reading settings a Kindle offers for a KFX book, and what each stop is
//! worth.
//!
//! A [`Panel`] holds the stops one device offers: font sizes in points by
//! script, line-spacing multipliers, embolden weights, and a margin ladder per
//! reading direction. [`Panel::parse`] reads one from a profile the caller
//! supplies; this crate carries none.
//!
//! [`Settings`] picks a stop from each ladder, and turns the pair into the
//! [`Viewport`] a page is laid out into.

use std::collections::HashMap;
use std::path::Path;
use std::{fs, io};

use crate::flow::Viewport;
use crate::geom::{Edges, Size};
use crate::units::Metrics;

/// Line height as a multiple of the em, before the line-spacing stop
/// multiplies it.
pub const BASE_LINE_HEIGHT: f32 = 1.2;

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
    /// Line-spacing multipliers, narrow to wide.
    pub line_spacings: Vec<f32>,
    /// The same for the languages [`Script::wide_line_spacing`] names.
    pub wide_line_spacings: Vec<f32>,
    /// Embolden weights.
    pub boldness: Vec<f32>,
    /// Which embolden weight a book opens at.
    pub default_boldness: usize,
    /// Three widths of the four margins, in dots, for a book that reads
    /// across the page.
    pub margins_horizontal: Vec<Edges>,
    /// Three widths for one that reads down it. The vertical ladder is on the
    /// block axis and carries its own numbers, with the sides pinned at their
    /// narrow value.
    pub margins_vertical: Vec<Edges>,
}

impl Panel {
    /// Read a profile.
    ///
    /// One `key value…` per line, `#` to end of line a comment. Keys:
    /// `panel` (width height dpi), `color` and `columns` (0 or 1),
    /// `font_size_default` / `font_size_cjk` / `font_size_indic` (points),
    /// `default_font_size` and `default_boldness` (an index), `line_spacing`
    /// and `line_spacing_wide` (multipliers), `boldness` (weights), and
    /// `margins_horizontal` / `margins_vertical` (top right bottom left,
    /// three times over).
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

        Ok(Self {
            size: Size::new(width, height),
            dpi,
            color: flag("color")?,
            columns_offered: flag("columns")?,
            font_sizes,
            default_font_size: index("default_font_size")?,
            line_spacings: take("line_spacing")?,
            wide_line_spacings: take("line_spacing_wide")?,
            boldness: take("boldness")?,
            default_boldness: index("default_boldness")?,
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
    pub margins: Stop,
    /// Which embolden weight.
    pub boldness: usize,
    pub justified: bool,
    pub hyphenate: bool,
    /// One or two, and two only where the panel offers it.
    pub columns: u8,
}

impl Settings {
    /// What a book opens at.
    pub fn default_for(panel: &Panel) -> Self {
        Self {
            font_size: panel.default_font_size,
            boldness: panel.default_boldness,
            line_spacing: Stop::Normal,
            margins: Stop::Narrow,
            justified: true,
            hyphenate: true,
            columns: 1,
        }
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
        let ladder = if Script::wide_line_spacing(language) {
            &panel.wide_line_spacings
        } else {
            &panel.line_spacings
        };
        at(ladder, self.line_spacing.index())
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
        self.em(panel, language) * BASE_LINE_HEIGHT * self.line_spacing(panel, language)
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
