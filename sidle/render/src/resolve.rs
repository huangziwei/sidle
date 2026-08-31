//! [`Resolver`] turns a [`Length`] into device dots at one reading setting.

use bokai::style::{Length, ROOT_FONT_SIZE_PX};

use crate::units::Metrics;

/// Multiplies `font_size` where `line_height` is `Length::Auto`.
pub const NORMAL_LINE_HEIGHT: f32 = 1.2;

/// One reading setting, as `length`, `font_size` and `line_height` read it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resolver {
    pub metrics: Metrics,
    /// The em `Length::Rem` and a `Length::Px` font size resolve against.
    pub root_font_size: f32,
    /// Scales an `Em` and an `Auto` `line_height`.
    pub line_spacing: f32,
    /// Feeds `embolden`.
    pub embolden_weight: f32,
    /// Added after every character, in ems.
    pub character_spacing: f32,
    /// Added at every word break, in ems.
    pub word_spacing: f32,
    /// Added before a paragraph, in ems.
    pub paragraph_spacing: f32,
}

/// Divides `font_size * embolden_weight` into the dots `embolden` returns.
const EMBOLDEN_DIVISOR: f32 = 1700.0;

impl Default for Resolver {
    fn default() -> Self {
        Self {
            metrics: Metrics::default(),
            root_font_size: ROOT_FONT_SIZE_PX,
            line_spacing: 1.0,
            embolden_weight: 0.0,
            character_spacing: 0.0,
            word_spacing: 0.0,
            paragraph_spacing: 0.0,
        }
    }
}

impl Resolver {
    /// `value` in device dots, `None` for `Length::Auto`.
    /// `available` is the containing block's inline size.
    pub fn length(&self, value: Length, available: f32, font_size: f32) -> Option<f32> {
        match value {
            Length::Auto => None,
            Length::Px(px) => Some(self.metrics.length(px)),
            Length::Em(em) => Some(em * font_size),
            Length::Rem(rem) => Some(rem * self.root_font_size),
            Length::Percent(percent) => Some(available * percent / 100.0),
        }
    }

    /// The em `declared` sets. `Length::Px` counts multiples of
    /// `ROOT_FONT_SIZE_PX`, never dots.
    pub fn font_size(&self, declared: Length, parent: f32) -> f32 {
        match declared {
            Length::Auto => parent,
            Length::Px(px) => px / ROOT_FONT_SIZE_PX * self.root_font_size,
            Length::Em(em) => em * parent,
            Length::Rem(rem) => rem * self.root_font_size,
            Length::Percent(percent) => parent * percent / 100.0,
        }
    }

    /// The distance from one baseline to the next. `Em` and `Auto` scale by
    /// `line_spacing`; `Percent` and `Px` do not.
    pub fn line_height(&self, declared: Length, font_size: f32) -> f32 {
        match declared {
            Length::Auto => font_size * NORMAL_LINE_HEIGHT * self.line_spacing,
            Length::Em(factor) => factor * font_size * self.line_spacing,
            Length::Rem(factor) => factor * self.root_font_size * self.line_spacing,
            Length::Percent(percent) => font_size * percent / 100.0,
            Length::Px(px) => self.metrics.length(px),
        }
    }

    /// `line_height` of `Length::Auto` at `font_size`.
    pub fn normal_line_height(&self, font_size: f32) -> f32 {
        self.line_height(Length::Auto, font_size)
    }

    /// Dots `embolden_weight` adds to one glyph advance at `font_size`.
    pub fn embolden(&self, font_size: f32) -> f32 {
        (font_size * self.embolden_weight / EMBOLDEN_DIVISOR).round()
    }

    /// Dots `character_spacing` adds after one glyph at `font_size`.
    pub fn tracking(&self, font_size: f32) -> f32 {
        font_size * self.character_spacing
    }

    /// Dots `word_spacing` adds to one word break at `font_size`.
    pub fn word_gap(&self, font_size: f32) -> f32 {
        font_size * self.word_spacing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::KINDLE_PANEL_DPI;

    /// `root_font_size` 44, `line_spacing` 1.35.
    fn kindle() -> Resolver {
        Resolver {
            metrics: Metrics::kindle(),
            root_font_size: 44.0,
            line_spacing: 1.35,
            ..Resolver::default()
        }
    }

    #[test]
    fn an_absolute_length_is_a_point_at_the_panels_resolution() {
        assert_eq!(kindle().length(Length::Px(24.0), 0.0, 44.0), Some(45.0));
    }

    #[test]
    fn a_percentage_resolves_against_the_containing_blocks_inline_size() {
        assert_eq!(
            kindle().length(Length::Percent(10.0), 1108.0, 44.0),
            Some(110.8)
        );
    }

    #[test]
    fn a_declared_font_size_is_a_multiple_of_the_root_not_a_length() {
        assert_eq!(kindle().font_size(Length::Px(20.0), 44.0), 55.0);
        assert_eq!(kindle().font_size(Length::Percent(150.0), 44.0), 66.0);
        assert_eq!(kindle().font_size(Length::Em(1.5), 44.0), 66.0);
    }

    #[test]
    fn a_silent_line_height_is_the_reading_settings_own() {
        let pitch = kindle().line_height(Length::Auto, 44.0);

        assert!((pitch - 71.28).abs() < 0.01, "{pitch}");
    }

    #[test]
    fn an_em_line_height_multiplies_the_reading_setting() {
        let one = kindle().line_height(Length::Em(NORMAL_LINE_HEIGHT), 44.0);
        let two = kindle().line_height(Length::Em(2.0 * NORMAL_LINE_HEIGHT), 44.0);

        assert!((one - 71.28).abs() < 0.01, "{one}");
        assert!((two - 142.56).abs() < 0.01, "{two}");
    }

    #[test]
    fn a_percent_line_height_replaces_the_reading_setting() {
        assert_eq!(kindle().line_height(Length::Percent(150.0), 44.0), 66.0);
    }

    #[test]
    fn an_absolute_line_height_is_itself() {
        assert_eq!(kindle().line_height(Length::Px(40.0), 44.0), 75.0);
    }

    #[test]
    fn the_two_line_height_grammars_are_not_interchangeable() {
        let resolver = kindle();
        let em = resolver.line_height(Length::Em(1.5 * NORMAL_LINE_HEIGHT), 44.0);
        let percent = resolver.line_height(Length::Percent(150.0), 44.0);

        assert!((em - 106.92).abs() < 0.01, "{em}");
        assert_eq!(percent, 66.0);
    }

    #[test]
    fn a_resolver_at_no_line_spacing_reads_line_heights_as_css_does() {
        let plain = Resolver {
            metrics: Metrics::kfx(KINDLE_PANEL_DPI),
            root_font_size: 44.0,
            line_spacing: 1.0,
            ..Resolver::default()
        };

        assert_eq!(plain.line_height(Length::Em(2.0), 44.0), 88.0);
        assert!((plain.line_height(Length::Auto, 44.0) - 52.8).abs() < 0.01);
    }

    #[test]
    fn an_embolden_stop_widens_every_glyph() {
        // Six stops at two ems.
        let ladder = [
            (44.0, 0.0, 0.0),
            (44.0, 20.0, 1.0),
            (44.0, 40.0, 1.0),
            (44.0, 60.0, 2.0),
            (44.0, 80.0, 2.0),
            (44.0, 100.0, 3.0),
            (71.0, 0.0, 0.0),
            (71.0, 20.0, 1.0),
            (71.0, 40.0, 2.0),
            (71.0, 60.0, 3.0),
            (71.0, 80.0, 3.0),
            (71.0, 100.0, 4.0),
        ];
        for (em, weight, dots) in ladder {
            let resolver = Resolver {
                root_font_size: em,
                embolden_weight: weight,
                ..kindle()
            };
            assert_eq!(resolver.embolden(em), dots, "em {em}, weight {weight}");
        }
    }
}
