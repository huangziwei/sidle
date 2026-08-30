//! CSS declarations, typed and raw.

use cssparser::Parser;

use super::parse::border::{
    BorderSide, parse_border_shorthand, parse_border_side_shorthand,
    parse_border_style_shorthand_values, parse_border_width_value, parse_color_shorthand_values,
};
use super::parse::box_model::{parse_box_shorthand_values, parse_box_shorthand_with};
use super::parse::font::{
    parse_font_family, parse_font_shorthand, parse_font_size, parse_font_weight, parse_line_height,
};
use super::parse::keywords::{
    parse_border_collapse, parse_border_style_value, parse_box_sizing, parse_break_inside,
    parse_break_value, parse_clear, parse_decoration_style, parse_display, parse_float,
    parse_font_style, parse_font_variant, parse_hyphens, parse_line_break,
    parse_list_style_position, parse_list_style_shorthand, parse_list_style_type,
    parse_overflow_wrap, parse_text_align, parse_text_align_last, parse_text_combine_upright,
    parse_text_emphasis_position, parse_text_emphasis_style, parse_text_orientation,
    parse_text_transform, parse_vertical_align, parse_visibility, parse_white_space,
    parse_word_break, parse_writing_mode,
};
use super::parse::values::{
    Axis, parse_background_position, parse_background_position_axis, parse_background_repeat,
    parse_background_shorthand, parse_background_size, parse_color, parse_integer, parse_length,
    parse_length_or_normal, parse_text_decoration, parse_url_value,
};
use super::properties::*;

/// A parsed CSS declaration (property: value).
///
/// This "fat enum" combines property identity and value in one type.
#[derive(Debug, Clone)]
pub enum Declaration {
    // Colors
    Color(Color),
    BackgroundColor(Color),

    // Background image. The payload is the `url()` target as written; the
    // importer resolves it against the stylesheet's own location.
    BackgroundImage(String),
    BackgroundRepeat(BackgroundRepeat),
    BackgroundSize(BackgroundSize),
    BackgroundPositionX(Length),
    BackgroundPositionY(Length),

    // Font properties
    FontFamily(String),
    FontSize(Length),
    FontWeight(FontWeight),
    FontStyle(FontStyle),
    FontVariant(FontVariant),

    // Text properties
    TextAlign(TextAlign),
    TextAlignLast(TextAlignLast),
    TextIndent(Length),
    LineHeight(Length),
    LetterSpacing(Length),
    WordSpacing(Length),
    TextTransform(TextTransform),
    Hyphens(Hyphens),
    WhiteSpace(WhiteSpace),
    VerticalAlign(VerticalAlign),
    WritingMode(WritingMode),
    TextOrientation(TextOrientation),
    LineBreak(LineBreak),
    TextCombineUpright(TextCombineUpright),
    TextEmphasisStyle(TextEmphasisStyle),
    TextEmphasisColor(Color),
    TextEmphasisPosition(TextEmphasisPosition),

    // Text decoration
    TextDecoration(super::parse::TextDecorationValue),
    TextDecorationStyle(DecorationStyle),
    TextDecorationColor(Color),

    // Box model - margins
    Margin(Length),
    MarginTop(Length),
    MarginRight(Length),
    MarginBottom(Length),
    MarginLeft(Length),

    // Box model - padding
    Padding(Length),
    PaddingTop(Length),
    PaddingRight(Length),
    PaddingBottom(Length),
    PaddingLeft(Length),

    // Dimensions
    Width(Length),
    Height(Length),
    MaxWidth(Length),
    MaxHeight(Length),
    MinWidth(Length),
    MinHeight(Length),

    // Display & positioning
    Display(Display),
    Float(Float),
    Clear(Clear),
    Visibility(Visibility),
    BoxSizing(BoxSizing),

    // Pagination control
    Orphans(u32),
    Widows(u32),

    // Text wrapping
    WordBreak(WordBreak),
    OverflowWrap(OverflowWrap),

    // Page breaks
    BreakBefore(BreakValue),
    BreakAfter(BreakValue),
    BreakInside(BreakValue),

    // Border style
    BorderStyle(BorderStyle),
    BorderTopStyle(BorderStyle),
    BorderRightStyle(BorderStyle),
    BorderBottomStyle(BorderStyle),
    BorderLeftStyle(BorderStyle),

    // Border width
    BorderWidth(Length),
    BorderTopWidth(Length),
    BorderRightWidth(Length),
    BorderBottomWidth(Length),
    BorderLeftWidth(Length),

    // Border color
    BorderColor(Color),
    BorderTopColor(Color),
    BorderRightColor(Color),
    BorderBottomColor(Color),
    BorderLeftColor(Color),

    // Border radius
    BorderRadius(Length),
    BorderTopLeftRadius(Length),
    BorderTopRightRadius(Length),
    BorderBottomLeftRadius(Length),
    BorderBottomRightRadius(Length),

    // List properties
    ListStyleType(ListStyleType),
    ListStylePosition(ListStylePosition),

    // Table properties
    BorderCollapse(BorderCollapse),
    BorderSpacing(Length),

    /// CSS-wide keyword (`inherit` | `initial` | `unset` | `revert`) — the
    UniversalKeyword {
        property: String,
        keyword: UniversalKeyword,
    },
}

/// One of the four CSS-wide keywords that can be set on any property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UniversalKeyword {
    Inherit,
    Initial,
    Unset,
    Revert,
}

impl Declaration {
    /// Parse a CSS declaration from a property name and value parser.
    pub fn parse(name: &str, input: &mut Parser<'_, '_>) -> Vec<Self> {
        // Try shorthand properties first (they expand to multiple declarations)
        if let Some(decls) = Self::parse_shorthand(name, input) {
            return decls;
        }

        // Single-value properties
        Self::parse_single(name, input).into_iter().collect()
    }

    /// Parse shorthand properties that expand to multiple declarations.
    fn parse_shorthand(name: &str, input: &mut Parser<'_, '_>) -> Option<Vec<Self>> {
        Some(match name {
            "margin" => Self::parse_length_rect(input, |t, r, b, l| {
                [
                    Self::MarginTop(t),
                    Self::MarginRight(r),
                    Self::MarginBottom(b),
                    Self::MarginLeft(l),
                ]
            }),
            "padding" => Self::parse_length_rect(input, |t, r, b, l| {
                [
                    Self::PaddingTop(t),
                    Self::PaddingRight(r),
                    Self::PaddingBottom(b),
                    Self::PaddingLeft(l),
                ]
            }),
            "border-width" => parse_box_shorthand_with(input, parse_border_width_value)
                .map(|(t, r, b, l)| {
                    vec![
                        Self::BorderTopWidth(t),
                        Self::BorderRightWidth(r),
                        Self::BorderBottomWidth(b),
                        Self::BorderLeftWidth(l),
                    ]
                })
                .unwrap_or_default(),
            "border-style" => parse_border_style_shorthand_values(input)
                .map(|(t, r, b, l)| {
                    vec![
                        Self::BorderTopStyle(t),
                        Self::BorderRightStyle(r),
                        Self::BorderBottomStyle(b),
                        Self::BorderLeftStyle(l),
                    ]
                })
                .unwrap_or_default(),
            "border-color" => parse_color_shorthand_values(input)
                .map(|(t, r, b, l)| {
                    vec![
                        Self::BorderTopColor(t),
                        Self::BorderRightColor(r),
                        Self::BorderBottomColor(b),
                        Self::BorderLeftColor(l),
                    ]
                })
                .unwrap_or_default(),
            "border" => parse_border_shorthand(input),
            "background" => {
                let bg = parse_background_shorthand(input);
                let mut decls = Vec::with_capacity(4);
                if let Some(c) = bg.color {
                    decls.push(Self::BackgroundColor(c));
                }
                if let Some(url) = bg.image {
                    decls.push(Self::BackgroundImage(url));
                }
                if let Some(r) = bg.repeat {
                    decls.push(Self::BackgroundRepeat(r));
                }
                if let Some(sz) = bg.size {
                    decls.push(Self::BackgroundSize(sz));
                }
                if let Some(x) = bg.position_x {
                    decls.push(Self::BackgroundPositionX(x));
                }
                if let Some(y) = bg.position_y {
                    decls.push(Self::BackgroundPositionY(y));
                }
                decls
            }
            "background-position" => {
                let (x, y) = parse_background_position(input);
                let mut decls = Vec::with_capacity(2);
                if let Some(x) = x {
                    decls.push(Self::BackgroundPositionX(x));
                }
                if let Some(y) = y {
                    decls.push(Self::BackgroundPositionY(y));
                }
                decls
            }
            "font" => parse_font_shorthand(input),
            "border-top" => parse_border_side_shorthand(input, BorderSide::Top),
            "border-right" => parse_border_side_shorthand(input, BorderSide::Right),
            "border-bottom" => parse_border_side_shorthand(input, BorderSide::Bottom),
            "border-left" => parse_border_side_shorthand(input, BorderSide::Left),
            "list-style" => parse_list_style_shorthand(input),
            _ => return None,
        })
    }

    /// Parse a 1-4 value Length rect shorthand (margin, padding, border-width).
    fn parse_length_rect<F>(input: &mut Parser<'_, '_>, make_decls: F) -> Vec<Self>
    where
        F: FnOnce(Length, Length, Length, Length) -> [Self; 4],
    {
        parse_box_shorthand_values(input)
            .map(|(t, r, b, l)| make_decls(t, r, b, l).into())
            .unwrap_or_default()
    }

    /// Parse single-value properties.
    fn parse_single(name: &str, input: &mut Parser<'_, '_>) -> Option<Self> {
        // CSS-wide keywords apply to any property. Recognise them once here
        // so e.g. `text-indent: inherit` doesn't fall through to the
        // property-specific parser (which would reject "inherit").
        if let Ok(uk) = input.try_parse(|i| {
            let ident = i.expect_ident_cloned()?;
            match ident.as_ref() {
                "inherit" => {
                    Ok::<UniversalKeyword, cssparser::ParseError<'_, ()>>(UniversalKeyword::Inherit)
                }
                "initial" => Ok(UniversalKeyword::Initial),
                "unset" => Ok(UniversalKeyword::Unset),
                "revert" => Ok(UniversalKeyword::Revert),
                _ => Err(i.new_custom_error(())),
            }
        }) {
            return Some(Self::UniversalKeyword {
                property: name.to_string(),
                keyword: uk,
            });
        }
        match name {
            // Colors
            "color" => parse_color(input).map(Self::Color),
            "background-color" => parse_color(input).map(Self::BackgroundColor),
            "background-image" => parse_url_value(input).map(Self::BackgroundImage),
            "background-repeat" => parse_background_repeat(input).map(Self::BackgroundRepeat),
            "background-size" => parse_background_size(input).map(Self::BackgroundSize),
            "background-position-x" => {
                parse_background_position_axis(input, Axis::Horizontal).map(Self::BackgroundPositionX)
            }
            "background-position-y" => {
                parse_background_position_axis(input, Axis::Vertical).map(Self::BackgroundPositionY)
            }

            // Font properties
            "font-family" => parse_font_family(input).map(Self::FontFamily),
            "font-size" => parse_font_size(input).map(Self::FontSize),
            "font-weight" => parse_font_weight(input).map(Self::FontWeight),
            "font-style" => parse_font_style(input).map(Self::FontStyle),
            "font-variant" | "font-variant-caps" => {
                parse_font_variant(input).map(Self::FontVariant)
            }

            // Text properties
            "text-align" => parse_text_align(input).map(Self::TextAlign),
            "text-align-last" => parse_text_align_last(input).map(Self::TextAlignLast),
            "text-indent" => parse_length(input).map(Self::TextIndent),
            "line-height" => parse_line_height(input).map(Self::LineHeight),
            "letter-spacing" => parse_length_or_normal(input).map(Self::LetterSpacing),
            "word-spacing" => parse_length_or_normal(input).map(Self::WordSpacing),
            "text-transform" => parse_text_transform(input).map(Self::TextTransform),
            "hyphens" | "-epub-hyphens" | "-webkit-hyphens" => {
                parse_hyphens(input).map(Self::Hyphens)
            }
            "white-space" => parse_white_space(input).map(Self::WhiteSpace),
            "vertical-align" => parse_vertical_align(input).map(Self::VerticalAlign),
            "writing-mode" | "-webkit-writing-mode" | "-epub-writing-mode" => {
                parse_writing_mode(input).map(Self::WritingMode)
            }
            "text-orientation" | "-webkit-text-orientation" | "-epub-text-orientation" => {
                parse_text_orientation(input).map(Self::TextOrientation)
            }
            // CSS 3 spec name + legacy `text-combine` (epub3 + draft) — same
            // semantics for `none` / `all`; we ignore the `digits N` legacy form.
            "text-combine-upright"
            | "-webkit-text-combine-upright"
            | "text-combine"
            | "-webkit-text-combine"
            | "-epub-text-combine" => {
                parse_text_combine_upright(input).map(Self::TextCombineUpright)
            }
            "line-break" | "-webkit-line-break" | "-epub-line-break" => {
                parse_line_break(input).map(Self::LineBreak)
            }
            "text-emphasis-style" | "-webkit-text-emphasis-style"
            | "-epub-text-emphasis-style"
            // The `text-emphasis` shorthand sets style (+ optional color). Aozora
            | "text-emphasis" | "-webkit-text-emphasis" | "-epub-text-emphasis" => {
                parse_text_emphasis_style(input).map(Self::TextEmphasisStyle)
            }
            "text-emphasis-color" | "-webkit-text-emphasis-color"
            | "-epub-text-emphasis-color" => {
                parse_color(input).map(Self::TextEmphasisColor)
            }
            "text-emphasis-position" | "-webkit-text-emphasis-position"
            | "-epub-text-emphasis-position" => {
                parse_text_emphasis_position(input).map(Self::TextEmphasisPosition)
            }

            // Text decoration
            "text-decoration" | "text-decoration-line" => {
                parse_text_decoration(input).map(Self::TextDecoration)
            }
            "text-decoration-style" => parse_decoration_style(input).map(Self::TextDecorationStyle),
            "text-decoration-color" => parse_color(input).map(Self::TextDecorationColor),

            // Box model - margins (individual)
            "margin-top" => parse_length(input).map(Self::MarginTop),
            "margin-right" => parse_length(input).map(Self::MarginRight),
            "margin-bottom" => parse_length(input).map(Self::MarginBottom),
            "margin-left" => parse_length(input).map(Self::MarginLeft),

            // Box model - padding (individual)
            "padding-top" => parse_length(input).map(Self::PaddingTop),
            "padding-right" => parse_length(input).map(Self::PaddingRight),
            "padding-bottom" => parse_length(input).map(Self::PaddingBottom),
            "padding-left" => parse_length(input).map(Self::PaddingLeft),

            // Dimensions
            "width" => parse_length(input).map(Self::Width),
            "height" => parse_length(input).map(Self::Height),
            "max-width" => parse_length(input).map(Self::MaxWidth),
            "max-height" => parse_length(input).map(Self::MaxHeight),
            "min-width" => parse_length(input).map(Self::MinWidth),
            "min-height" => parse_length(input).map(Self::MinHeight),

            // Display & positioning
            "display" => parse_display(input).map(Self::Display),
            "float" => parse_float(input).map(Self::Float),
            "clear" => parse_clear(input).map(Self::Clear),
            "visibility" => parse_visibility(input).map(Self::Visibility),
            "box-sizing" => parse_box_sizing(input).map(Self::BoxSizing),

            // Pagination control
            "orphans" => parse_integer(input).map(Self::Orphans),
            "widows" => parse_integer(input).map(Self::Widows),

            // Text wrapping
            "word-break" | "-epub-word-break" | "-webkit-word-break" => {
                parse_word_break(input).map(Self::WordBreak)
            }
            // `word-wrap` is the legacy alias for `overflow-wrap` per CSS3.
            "overflow-wrap" | "word-wrap" => parse_overflow_wrap(input).map(Self::OverflowWrap),

            // Page breaks
            "break-before" | "page-break-before" => parse_break_value(input).map(Self::BreakBefore),
            "break-after" | "page-break-after" => parse_break_value(input).map(Self::BreakAfter),
            "break-inside" | "page-break-inside" => {
                parse_break_inside(input).map(Self::BreakInside)
            }

            // Border style (individual sides)
            "border-top-style" => parse_border_style_value(input).map(Self::BorderTopStyle),
            "border-right-style" => parse_border_style_value(input).map(Self::BorderRightStyle),
            "border-bottom-style" => parse_border_style_value(input).map(Self::BorderBottomStyle),
            "border-left-style" => parse_border_style_value(input).map(Self::BorderLeftStyle),

            // Border width (individual sides). Uses the keyword-aware parser so
            // `thin`/`medium`/`thick` keywords are honoured per CSS spec.
            "border-top-width" => parse_border_width_value(input).map(Self::BorderTopWidth),
            "border-right-width" => parse_border_width_value(input).map(Self::BorderRightWidth),
            "border-bottom-width" => parse_border_width_value(input).map(Self::BorderBottomWidth),
            "border-left-width" => parse_border_width_value(input).map(Self::BorderLeftWidth),

            // Border color (individual sides)
            "border-top-color" => parse_color(input).map(Self::BorderTopColor),
            "border-right-color" => parse_color(input).map(Self::BorderRightColor),
            "border-bottom-color" => parse_color(input).map(Self::BorderBottomColor),
            "border-left-color" => parse_color(input).map(Self::BorderLeftColor),

            // Border radius
            "border-radius" => parse_length(input).map(Self::BorderRadius),
            "border-top-left-radius" => parse_length(input).map(Self::BorderTopLeftRadius),
            "border-top-right-radius" => parse_length(input).map(Self::BorderTopRightRadius),
            "border-bottom-left-radius" => parse_length(input).map(Self::BorderBottomLeftRadius),
            "border-bottom-right-radius" => parse_length(input).map(Self::BorderBottomRightRadius),

            // List properties
            "list-style-type" => parse_list_style_type(input).map(Self::ListStyleType),
            "list-style-position" => parse_list_style_position(input).map(Self::ListStylePosition),

            // Table properties
            "border-collapse" => parse_border_collapse(input).map(Self::BorderCollapse),
            "border-spacing" => parse_length(input).map(Self::BorderSpacing),

            // Unknown properties
            _ => {
                while input.next().is_ok() {}
                None
            }
        }
    }
}

/// A small CSS rule body: ordered property/value pairs. Used when emitting
/// either an inline `style="..."` attribute or a stylesheet rule.
#[derive(Debug, Default, Clone)]
pub struct CssDecl {
    pub items: Vec<(String, String)>,
}

impl CssDecl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let n = name.into();
        // Last write wins.
        if let Some(slot) = self.items.iter_mut().find(|(k, _)| *k == n) {
            slot.1 = value.into();
        } else {
            self.items.push((n, value.into()));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The value set for `name`, if any.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.items
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Remove `name`, returning what it was set to. For a marker property
    /// that resolves into other declarations rather than surviving into CSS.
    pub fn take(&mut self, name: &str) -> Option<String> {
        let i = self.items.iter().position(|(k, _)| k == name)?;
        Some(self.items.remove(i).1)
    }

    pub fn to_inline(&self) -> String {
        let mut s = String::new();
        for (i, (k, v)) in self.items.iter().enumerate() {
            if i > 0 {
                s.push_str("; ");
            }
            s.push_str(k);
            s.push_str(": ");
            s.push_str(v);
        }
        s
    }
}

/// Parse a serialized inline declaration (`"k: v; k2: v2"`) back into a
/// [`CssDecl`]. Inverse of [`CssDecl::to_inline`]; also tolerant of plain
/// `style="..."` attribute text.
pub fn parse_inline_decl(s: &str) -> CssDecl {
    let mut decl = CssDecl::new();
    for chunk in s.split(';') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        if let Some(colon) = chunk.find(':') {
            let k = chunk[..colon].trim();
            let v = chunk[colon + 1..].trim();
            decl.set(k, v);
        }
    }
    decl
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::properties::BackgroundRepeat;

    /// Parse one declaration the way a stylesheet rule body would.
    fn parse_decl(name: &str, value: &str) -> Vec<Declaration> {
        let mut input = cssparser::ParserInput::new(value);
        let mut parser = Parser::new(&mut input);
        Declaration::parse(name, &mut parser)
    }

    #[test]
    fn parse_inline_decl_round_trip() {
        let decl = parse_inline_decl(" width: 100% ; ; text-align: center ");
        assert_eq!(decl.to_inline(), "width: 100%; text-align: center");
    }

    /// The section-break ornament idiom: publishers paint an `<hr>` with a
    /// picture and hide the rule. Every part of that has to survive parsing —
    /// the url most of all, since it is the only place the image is named.
    #[test]
    fn background_shorthand_keeps_the_image() {
        let decls = parse_decl(
            "background",
            "url('../images/asterisks.jpg') no-repeat center",
        );
        let mut image = None;
        let mut repeat = None;
        let mut x = None;
        let mut y = None;
        for d in &decls {
            match d {
                Declaration::BackgroundImage(s) => image = Some(s.clone()),
                Declaration::BackgroundRepeat(r) => repeat = Some(*r),
                Declaration::BackgroundPositionX(l) => x = Some(*l),
                Declaration::BackgroundPositionY(l) => y = Some(*l),
                _ => {}
            }
        }
        assert_eq!(image.as_deref(), Some("../images/asterisks.jpg"));
        assert_eq!(repeat, Some(BackgroundRepeat::NoRepeat));
        // A lone `center` centres both axes.
        assert_eq!(x, Some(Length::Percent(50.0)));
        assert_eq!(y, Some(Length::Percent(50.0)));
    }

    #[test]
    fn background_shorthand_still_reads_the_color() {
        let decls = parse_decl("background", "#ff0000 url(bg.png) repeat-x");
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::BackgroundColor(_)))
        );
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::BackgroundImage(s) if s == "bg.png"))
        );
        assert!(decls.iter().any(
            |d| matches!(d, Declaration::BackgroundRepeat(r) if *r == BackgroundRepeat::RepeatX)
        ));
    }

    /// The size follows the `/` and must not be mistaken for the position
    /// that precedes it — nor swallow it.
    #[test]
    fn background_size_does_not_displace_the_position() {
        let decls = parse_decl("background", "url(bg.png) no-repeat left top / cover");
        let x = decls.iter().find_map(|d| match d {
            Declaration::BackgroundPositionX(l) => Some(*l),
            _ => None,
        });
        let y = decls.iter().find_map(|d| match d {
            Declaration::BackgroundPositionY(l) => Some(*l),
            _ => None,
        });
        assert_eq!(x, Some(Length::Percent(0.0)));
        assert_eq!(y, Some(Length::Percent(0.0)));
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::BackgroundSize(BackgroundSize::Cover)))
        );
    }

    /// `background-size` in all three forms it is written in: the keywords,
    /// one length (the other axis stays proportional), and an explicit pair.
    #[test]
    fn background_size_parses_keywords_and_lengths() {
        let size_of = |name: &str, value: &str| {
            parse_decl(name, value).into_iter().find_map(|d| match d {
                Declaration::BackgroundSize(s) => Some(s),
                _ => None,
            })
        };
        assert_eq!(
            size_of("background-size", "cover"),
            Some(BackgroundSize::Cover)
        );
        assert_eq!(
            size_of("background-size", "contain"),
            Some(BackgroundSize::Contain)
        );
        assert_eq!(
            size_of("background-size", "50%"),
            Some(BackgroundSize::Explicit(
                Length::Percent(50.0),
                Length::Auto
            ))
        );
        assert_eq!(
            size_of("background-size", "10px 20px"),
            Some(BackgroundSize::Explicit(Length::Px(10.0), Length::Px(20.0)))
        );
        // Also reachable through the shorthand's post-slash slot.
        assert_eq!(
            size_of("background", "url(a.png) center / 100% 100%"),
            Some(BackgroundSize::Explicit(
                Length::Percent(100.0),
                Length::Percent(100.0)
            ))
        );
    }

    /// Named axes bind by name, not by order — `bottom left` is y then x.
    #[test]
    fn background_position_named_axes_bind_by_name() {
        let decls = parse_decl("background-position", "bottom left");
        let x = decls.iter().find_map(|d| match d {
            Declaration::BackgroundPositionX(l) => Some(*l),
            _ => None,
        });
        let y = decls.iter().find_map(|d| match d {
            Declaration::BackgroundPositionY(l) => Some(*l),
            _ => None,
        });
        assert_eq!(x, Some(Length::Percent(0.0)));
        assert_eq!(y, Some(Length::Percent(100.0)));
    }

    /// `border: none` on an element whose renderer draws one by default is a
    /// statement, not silence: it must not land on the same value as "no
    /// border declared", or the rule comes back on the far side.
    #[test]
    fn declared_border_none_differs_from_undeclared() {
        for value in ["none", "0", "0 none", "hidden"] {
            let decls = parse_decl("border", value);
            assert!(
                decls
                    .iter()
                    .any(|d| matches!(d, Declaration::BorderTopStyle(BorderStyle::None))),
                "`border: {value}` must state that no border is drawn"
            );
        }
        assert_ne!(BorderStyle::None, BorderStyle::default());
    }

    /// A shorthand that names a style keeps it; the fill-in is only for the
    /// component the author left out.
    #[test]
    fn border_shorthand_keeps_a_named_style() {
        let decls = parse_decl("border", "1px solid black");
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::BorderTopStyle(BorderStyle::Solid)))
        );
        let decls = parse_decl("border-bottom", "2px dashed");
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::BorderBottomStyle(BorderStyle::Dashed)))
        );
    }
}
