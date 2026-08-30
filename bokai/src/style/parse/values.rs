//! CSS value parsing functions.
//!
//! This module contains parsers for CSS values like colors, lengths, and integers.

use cssparser::{ParseError, Parser, Token};

use crate::style::properties::{BackgroundRepeat, BackgroundSize, Color, Length};

/// Text decoration value (can combine underline and line-through).
#[derive(Debug, Clone, Copy, Default)]
pub struct TextDecorationValue {
    pub underline: bool,
    pub line_through: bool,
    pub overline: bool,
}

pub(crate) fn parse_color(input: &mut Parser<'_, '_>) -> Option<Color> {
    // Try named colors first. CSS idents are ASCII-case-insensitive, so we
    // lowercase before matching to catch `currentColor`, `Red`, etc.
    if let Ok(token) = input.try_parse(|i| i.expect_ident_cloned()) {
        let lower = token.as_ref().to_ascii_lowercase();
        let color = match lower.as_str() {
            "black" => Color::BLACK,
            "white" => Color::WHITE,
            "red" => Color::rgb(255, 0, 0),
            "green" => Color::rgb(0, 128, 0),
            "blue" => Color::rgb(0, 0, 255),
            "yellow" => Color::rgb(255, 255, 0),
            "cyan" => Color::rgb(0, 255, 255),
            "magenta" => Color::rgb(255, 0, 255),
            "gray" | "grey" => Color::rgb(128, 128, 128),
            "transparent" => Color::TRANSPARENT,
            // `currentColor` means "the current `color` property value." We
            "currentcolor" => Color::BLACK,
            _ => return None,
        };
        return Some(color);
    }

    // Try ID token (which is how cssparser parses hex colors like #ff0000)
    // Or Hash token (for colors starting with digits like #222299)
    if let Ok(hash) = input.try_parse(|i| -> Result<_, ParseError<'_, ()>> {
        match i.next()? {
            Token::IDHash(h) | Token::Hash(h) => Ok(h.clone()),
            _ => Err(i.new_custom_error(())),
        }
    }) && let Some(color) = parse_hex_color(hash.as_ref())
    {
        return Some(color);
    }

    // Try rgb() or rgba()
    if let Ok(color) = input.try_parse(parse_rgb_function) {
        return Some(color);
    }

    None
}

/// The components of the CSS `background` shorthand that the IR models.
#[derive(Debug, Clone, Default)]
pub(crate) struct BackgroundShorthand {
    pub color: Option<Color>,
    /// The `url()` target, exactly as written — resolving it against the
    /// stylesheet's location is the importer's job, which is the only layer
    /// that knows where the rule was written.
    pub image: Option<String>,
    pub repeat: Option<BackgroundRepeat>,
    pub position_x: Option<Length>,
    pub position_y: Option<Length>,
    pub size: Option<BackgroundSize>,
}

/// Parse the CSS `background` shorthand.
pub(crate) fn parse_background_shorthand(input: &mut Parser<'_, '_>) -> BackgroundShorthand {
    let mut out = BackgroundShorthand::default();
    // Positional keywords bind to an axis by order, not by name: `center` on
    // its own means both axes, `center bottom` means x then y. Collect them
    // in order and resolve once the whole value is consumed.
    let mut positions: Vec<PositionComponent> = Vec::new();
    // Set once a `/` is seen — from there on, lengths are the size.
    let mut size_follows = false;

    // Try to parse each component in any order, like lightningcss does
    loop {
        // Try to parse a color if we haven't found one yet
        if out.color.is_none()
            && let Ok(c) =
                input.try_parse(|i| parse_color(i).ok_or(i.new_custom_error::<_, ()>(())))
        {
            out.color = Some(c);
            continue;
        }

        // The image. `url()` is the only form the IR carries; gradients fall
        // through to the function skip below.
        if let Ok(url) = input.try_parse(|i| i.expect_url().map(|u| u.as_ref().to_string())) {
            out.image = Some(url);
            continue;
        }

        // Skip over functions like linear-gradient() etc.
        if input
            .try_parse(|i: &mut Parser<'_, '_>| {
                let _ = i.expect_function()?;
                i.parse_nested_block(
                    |nested: &mut Parser<'_, '_>| -> Result<(), ParseError<'_, ()>> {
                        while nested.next().is_ok() {}
                        Ok(())
                    },
                )
            })
            .is_ok()
        {
            continue;
        }

        // Keywords: repeat and position are kept, the rest are consumed so
        // parsing can carry on past them.
        enum Kw {
            Repeat(BackgroundRepeat),
            Position(PositionComponent),
            Size(BackgroundSize),
            Ignored,
        }
        if let Ok(kw) = input.try_parse(|i| {
            let ident = i.expect_ident()?;
            Ok(match ident.as_ref() {
                // repeat keywords
                "repeat" => Kw::Repeat(BackgroundRepeat::Repeat),
                "repeat-x" => Kw::Repeat(BackgroundRepeat::RepeatX),
                "repeat-y" => Kw::Repeat(BackgroundRepeat::RepeatY),
                "no-repeat" => Kw::Repeat(BackgroundRepeat::NoRepeat),
                "space" => Kw::Repeat(BackgroundRepeat::Space),
                "round" => Kw::Repeat(BackgroundRepeat::Round),
                // size keywords — only ever after the `/`
                "cover" => Kw::Size(BackgroundSize::Cover),
                "contain" => Kw::Size(BackgroundSize::Contain),
                // position keywords
                "left" => Kw::Position(PositionComponent::Horizontal(0.0)),
                "right" => Kw::Position(PositionComponent::Horizontal(100.0)),
                "top" => Kw::Position(PositionComponent::Vertical(0.0)),
                "bottom" => Kw::Position(PositionComponent::Vertical(100.0)),
                "center" => Kw::Position(PositionComponent::Either(50.0)),
                // `auto` is a size on either axis and the initial value of
                // several other components; attachment / box (origin, clip) /
                // `none` carry nothing the IR models.
                "auto" | "scroll" | "fixed" | "local" | "padding-box" | "border-box"
                | "content-box" | "none" => Kw::Ignored,
                _ => return Err(i.new_custom_error::<_, ()>(())),
            })
        }) {
            match kw {
                Kw::Repeat(r) => out.repeat = Some(r),
                Kw::Size(s) => out.size = Some(s),
                Kw::Position(p) if !size_follows => positions.push(p),
                Kw::Position(_) | Kw::Ignored => {}
            }
            continue;
        }

        // Lengths and percentages: position offsets before the `/`, the
        // explicit size after it.
        if let Ok(len) = input.try_parse(|i| parse_length(i).ok_or(i.new_custom_error::<_, ()>(())))
        {
            if size_follows {
                out.size = Some(match out.size {
                    // Second value completes the pair; `Length::Auto` stands
                    // in for the axis the author left implicit.
                    Some(BackgroundSize::Explicit(x, Length::Auto)) => {
                        BackgroundSize::Explicit(x, len)
                    }
                    _ => BackgroundSize::Explicit(len, Length::Auto),
                });
            } else {
                positions.push(match len {
                    Length::Percent(p) => PositionComponent::Either(p),
                    other => PositionComponent::Length(other),
                });
            }
            continue;
        }

        // Anything else that tokenizes as a number or dimension is consumed
        // without interpretation.
        if input
            .try_parse(|i| match i.next()? {
                Token::Dimension { .. } | Token::Percentage { .. } | Token::Number { .. } => Ok(()),
                _ => Err(i.new_custom_error::<_, ()>(())),
            })
            .is_ok()
        {
            continue;
        }

        // The "/" delimiter separates position from size. The positions
        // collected so far stand; everything after it belongs to the size.
        if input.try_parse(|i| i.expect_delim('/')).is_ok() {
            size_follows = true;
            continue;
        }

        // Nothing matched, exit the loop
        break;
    }

    let (x, y) = resolve_position(&positions);
    out.position_x = x;
    out.position_y = y;
    out
}

/// One token of a `background-position` value, before axes are assigned.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PositionComponent {
    /// `left` / `right` — horizontal by name.
    Horizontal(f32),
    /// `top` / `bottom` — vertical by name.
    Vertical(f32),
    /// `center` or a percentage — axis decided by position in the value.
    Either(f32),
    /// A non-percentage offset, likewise axis-by-position.
    Length(Length),
}

/// Assign collected position components to axes.
pub(crate) fn resolve_position(parts: &[PositionComponent]) -> (Option<Length>, Option<Length>) {
    if parts.is_empty() {
        return (None, None);
    }
    let mut x: Option<Length> = None;
    let mut y: Option<Length> = None;
    let mut unassigned: Vec<Length> = Vec::new();

    for part in parts {
        match *part {
            PositionComponent::Horizontal(p) => x = Some(Length::Percent(p)),
            PositionComponent::Vertical(p) => y = Some(Length::Percent(p)),
            PositionComponent::Either(p) => unassigned.push(Length::Percent(p)),
            PositionComponent::Length(l) => unassigned.push(l),
        }
    }

    let mut rest = unassigned.into_iter();
    if x.is_none() {
        x = rest.next();
    }
    if y.is_none() {
        y = rest.next();
    }
    // A lone value sets its own axis and centres the other one: `left` is
    // `left center`, `top` is `center top`, `25%` is `25% center`.
    if parts.len() == 1 {
        x = x.or(Some(Length::Percent(50.0)));
        y = y.or(Some(Length::Percent(50.0)));
    }
    (x, y)
}

/// Which axis a `background-position-x` / `-y` longhand names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Axis {
    Horizontal,
    Vertical,
}

/// Parse a bare `url(...)` value (`background-image`). Returns the target as
/// written; `none` and gradients yield `None`, since the IR carries only a
/// single raster reference.
pub(crate) fn parse_url_value(input: &mut Parser<'_, '_>) -> Option<String> {
    input
        .try_parse(|i| i.expect_url().map(|u| u.as_ref().to_string()))
        .ok()
}

/// Parse a `background-repeat` value. The two-value form (`no-repeat repeat`)
/// collapses to its horizontal component, which is what the axis-less IR
/// field can express.
pub(crate) fn parse_background_repeat(input: &mut Parser<'_, '_>) -> Option<BackgroundRepeat> {
    let ident = input.try_parse(|i| i.expect_ident_cloned()).ok()?;
    BackgroundRepeat::from_css(&ident.to_ascii_lowercase())
}

/// Parse a `background-size` value: `cover`, `contain`, or one/two
/// lengths where a missing second axis is `auto`.
pub(crate) fn parse_background_size(input: &mut Parser<'_, '_>) -> Option<BackgroundSize> {
    if let Ok(kw) = input.try_parse(|i| {
        let ident = i.expect_ident()?;
        Ok(match ident.as_ref() {
            "cover" => BackgroundSize::Cover,
            "contain" => BackgroundSize::Contain,
            "auto" => BackgroundSize::Auto,
            _ => return Err(i.new_custom_error::<_, ()>(())),
        })
    }) {
        return Some(kw);
    }
    let x = parse_length(input)?;
    let y = input
        .try_parse(|i| parse_length(i).ok_or(i.new_custom_error::<_, ()>(())))
        .unwrap_or(Length::Auto);
    Some(BackgroundSize::Explicit(x, y))
}

/// Parse a `background-position` value into its two axes.
pub(crate) fn parse_background_position(
    input: &mut Parser<'_, '_>,
) -> (Option<Length>, Option<Length>) {
    let mut parts = Vec::new();
    while let Some(part) = parse_position_component(input) {
        parts.push(part);
    }
    resolve_position(&parts)
}

/// Parse a single-axis `background-position-x` / `-y` value.
pub(crate) fn parse_background_position_axis(
    input: &mut Parser<'_, '_>,
    axis: Axis,
) -> Option<Length> {
    let part = parse_position_component(input)?;
    let (x, y) = resolve_position(&[part]);
    match axis {
        Axis::Horizontal => x,
        Axis::Vertical => y,
    }
}

/// Parse one keyword or length of a position value.
fn parse_position_component(input: &mut Parser<'_, '_>) -> Option<PositionComponent> {
    if let Ok(part) = input.try_parse(|i| {
        let ident = i.expect_ident()?;
        Ok(match ident.as_ref() {
            "left" => PositionComponent::Horizontal(0.0),
            "right" => PositionComponent::Horizontal(100.0),
            "top" => PositionComponent::Vertical(0.0),
            "bottom" => PositionComponent::Vertical(100.0),
            "center" => PositionComponent::Either(50.0),
            _ => return Err(i.new_custom_error::<_, ()>(())),
        })
    }) {
        return Some(part);
    }
    match input.try_parse(|i| parse_length(i).ok_or(i.new_custom_error::<_, ()>(()))) {
        Ok(Length::Percent(p)) => Some(PositionComponent::Either(p)),
        Ok(other) => Some(PositionComponent::Length(other)),
        Err(_) => None,
    }
}

fn parse_hex_color(hex: &str) -> Option<Color> {
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            Some(Color::rgb(r, g, b))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Color::rgb(r, g, b))
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some(Color::rgba(r, g, b, a))
        }
        _ => None,
    }
}

fn parse_rgb_function<'i, 't>(input: &mut Parser<'i, 't>) -> Result<Color, ParseError<'i, ()>> {
    input.expect_function_matching("rgb")?;
    input.parse_nested_block(|input| {
        let r = parse_color_component(input)?;
        input.expect_comma()?;
        let g = parse_color_component(input)?;
        input.expect_comma()?;
        let b = parse_color_component(input)?;
        Ok(Color::rgb(r, g, b))
    })
}

fn parse_color_component<'i, 't>(input: &mut Parser<'i, 't>) -> Result<u8, ParseError<'i, ()>> {
    let location = input.current_source_location();
    match input.next()? {
        Token::Number {
            int_value: Some(v), ..
        } => Ok((*v).clamp(0, 255) as u8),
        Token::Percentage { unit_value, .. } => {
            Ok((unit_value * 255.0).round().clamp(0.0, 255.0) as u8)
        }
        _ => Err(location.new_custom_error(())),
    }
}

pub(crate) fn parse_length(input: &mut Parser<'_, '_>) -> Option<Length> {
    match input.next().ok()? {
        Token::Dimension { value, unit, .. } => {
            let length = match unit.as_ref() {
                "px" => Length::Px(*value),
                "em" => Length::Em(*value),
                "rem" => Length::Rem(*value),
                "%" => Length::Percent(*value),
                // ex = x-height, approximately 0.5em
                "ex" => Length::Em(*value * 0.5),
                // lh = current line-height. CSS L4 unit. Default
                // line-height is ~1.2× font-size, so approximate as 1.2em.
                "lh" => Length::Em(*value * 1.2),
                // pt = points, 1pt = 96/72 px
                "pt" => Length::Px(*value * 96.0 / 72.0),
                _ => return None,
            };
            Some(length)
        }
        Token::Percentage { unit_value, .. } => Some(Length::Percent(*unit_value * 100.0)),
        Token::Number { value, .. } if *value == 0.0 => Some(Length::Px(0.0)),
        Token::Ident(ident) => match ident.as_ref() {
            "auto" => Some(Length::Auto),
            // `none` is the initial value for max-width/max-height. Treating
            // it as Auto here means "no constraint", which matches the spec
            // for those properties and is a harmless no-op anywhere else.
            "none" => Some(Length::Auto),
            _ => None,
        },
        _ => None,
    }
}

/// Parse a length value or the `normal` keyword (-> 0px).
pub(crate) fn parse_length_or_normal(input: &mut Parser<'_, '_>) -> Option<Length> {
    if let Ok(ident) = input.try_parse(|i| i.expect_ident_cloned())
        && ident.as_ref() == "normal"
    {
        return Some(Length::Px(0.0));
    }
    parse_length(input)
}

pub(crate) fn parse_integer(input: &mut Parser<'_, '_>) -> Option<u32> {
    if let Ok(Token::Number {
        int_value: Some(v), ..
    }) = input.next().cloned()
        && v >= 0
    {
        return Some(v as u32);
    }
    None
}

pub(crate) fn parse_text_decoration(input: &mut Parser<'_, '_>) -> Option<TextDecorationValue> {
    let mut result = TextDecorationValue::default();
    let mut found = false;
    while let Ok(token) = input.try_parse(|i| i.expect_ident_cloned()) {
        match token.as_ref() {
            "underline" => result.underline = true,
            "line-through" => result.line_through = true,
            "overline" => result.overline = true,
            "none" | "blink" => {} // `blink` is deprecated but recognised
            _ => continue,
        }
        found = true;
    }
    if found { Some(result) } else { None }
}
