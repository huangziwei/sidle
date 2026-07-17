//! KFX style → CSS property translation.
//!
//! Port of `yj_to_epub_properties.py`, plus the block-image wrapper
//! partition from `yj_to_epub_content.py`. Covers the property table, value
//! translation, and `writing-mode` emission; the long tail (advanced color
//! transforms, layout-hint synthesis, etc.) is left to a follow-up pass.
//! Shared by both KFX→EPUB engines: `kfx_to_epub` resolves named `$157`
//! styles through it, and `KfxImporter` uses the same conversion so the two
//! routes' stylesheets agree property-for-property.
//!
//! Identifiers track calibre as much as Rust syntax allows.
//!
//! Transitional home: this is the import-direction (KFX→CSS) half of style
//! translation, while `style_schema.rs` holds the export-direction table —
//! the two are slated to merge into one bidirectional table.

#![allow(non_snake_case)]

use crate::formats::kfx::container::{SymbolTable, get_field};
use crate::formats::kfx::ion::IonValue;
use crate::style::CssDecl;

/// One entry in the YJ → CSS property map. Mirrors calibre's `Prop` class.
#[derive(Debug, Clone)]
pub struct Prop {
    pub name: &'static str,
    /// For enumerated properties, maps the YJ symbol id to its CSS value.
    /// `None` means "drop the declaration entirely" (calibre uses Python
    /// `None` for the same purpose).
    pub values: Option<&'static [(&'static str, Option<&'static str>)]>,
}

/// Look up the YJ property mapping for a given symbol id, resolved to its
/// text name. Returns `None` if we don't know how to map it.
pub fn prop_for(name: &str) -> Option<&'static Prop> {
    YJ_PROPERTY_INFO
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// CSS length unit ↔ KFX symbol map. Calibre's `YJ_LENGTH_UNITS`.
pub fn length_unit_for(symbol_name: &str) -> Option<&'static str> {
    match symbol_name {
        "ch" => Some("ch"),
        "cm" => Some("cm"),
        "em" => Some("em"),
        "ex" => Some("ex"),
        "in_" | "in" => Some("in"),
        "lh" => Some("lh"),
        "mm" => Some("mm"),
        "percent" => Some("%"),
        "pt" => Some("pt"),
        "px" => Some("px"),
        "rem" => Some("rem"),
        "vh" => Some("vh"),
        "vmax" => Some("vmax"),
        "vmin" => Some("vmin"),
        "vw" => Some("vw"),
        _ => None,
    }
}

/// Translate a KFX style's properties (as `(symbol_id, IonValue)` pairs) to
/// a CSS declaration. Mirrors calibre's `convert_yj_properties` at the
/// minimum-viable level we need for step 6.
pub fn convert_yj_properties(fields: &[(u64, IonValue)], symbols: &SymbolTable) -> CssDecl {
    let mut out = CssDecl::new();

    for (k, v) in fields {
        let key_text = symbols.resolve(*k);
        let Some(prop) = prop_for(key_text) else {
            continue;
        };

        // Skip private kfx symbols when we have an explicit enum table that
        // doesn't include this value; otherwise emit the converted value.
        if let Some(value_str) = property_value(prop, v, symbols) {
            // Calibre maps Python None to "drop the property"; we map it to
            // skip the declaration entirely.
            if !value_str.is_empty() {
                let value_str = if prop.name == "font-family" {
                    normalize_font_family(&value_str)
                } else {
                    value_str
                };
                if !value_str.is_empty() {
                    out.set(prop.name.to_string(), value_str);
                }
            }
        }
    }

    out
}

/// Translate a single KFX property value to a CSS string. Handles the four
/// cases calibre supports: enum (Prop.values), length (struct with unit/value),
/// color, plain int/float, string/symbol.
/// calibre's `DEFAULT_FONT_NAMES` (yj_to_epub_properties.py:715): font-family
/// values meaning "the document default font". calibre replaces these with the
/// real default family in its font pass (`font_name_replacements`,
/// yj_to_epub_metadata.py:114); boko defers that pass, so we emit no font-family
/// (inherit the default) rather than the literal sentinel, which is invalid CSS.
fn is_default_font_name(s: &str) -> bool {
    s == "default" || s == "$amzn_fixup_default_font$"
}

/// Quote font-family names that aren't safe as unquoted CSS identifiers. KFX
/// carries legacy vertical-writing font variants (`@ヒラギノ明朝`) and CJK family
/// names; an unquoted token starting with `@` is parsed as a CSS at-keyword and
/// rejected (epubcheck CSS-008 "Token … not allowed here"). Generic keywords
/// and plain ASCII-identifier names stay unquoted (e.g. `times new roman`,
/// `serif`); everything else is quoted.
fn normalize_font_family(value: &str) -> String {
    value
        .split(',')
        .filter_map(|fam| {
            let f = fam.trim();
            if f.is_empty() {
                None
            } else if is_generic_font_keyword(f) || is_safe_unquoted_font(f) {
                Some(f.to_string())
            } else {
                Some(format!(
                    "\"{}\"",
                    f.replace('\\', "\\\\").replace('"', "\\\"")
                ))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_generic_font_keyword(f: &str) -> bool {
    matches!(
        f.to_ascii_lowercase().as_str(),
        "serif"
            | "sans-serif"
            | "monospace"
            | "cursive"
            | "fantasy"
            | "system-ui"
            | "math"
            | "emoji"
            | "fangsong"
            | "ui-serif"
            | "ui-sans-serif"
            | "ui-monospace"
            | "ui-rounded"
            | "inherit"
            | "initial"
            | "unset"
            | "revert"
    )
}

/// A font-family value is safe unquoted iff it's a run of CSS identifiers
/// separated by single spaces: each word ASCII-alphanumeric-or-hyphen and not
/// starting with a digit/hyphen/`@`. `times new roman` qualifies; `@ipaex明朝`
/// (non-ASCII, `@`-prefixed) does not.
fn is_safe_unquoted_font(f: &str) -> bool {
    f.split(' ').all(|word| {
        !word.is_empty()
            && word.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

fn property_value(prop: &Prop, value: &IonValue, symbols: &SymbolTable) -> Option<String> {
    let inner = value.unwrap_annotated();

    // Enum value lookup — for symbol/bool values ONLY. A property can carry
    // both enum keywords and plain values: `line_height` is `auto` (→ enum
    // table → `normal`) on some styles and a `{value: 0.6, unit: lh}` length
    // struct on others. Calibre dispatches on the VALUE's type before ever
    // consulting the enum map (`property_value`,
    // yj_to_epub_properties.py:1174 — the IonStruct branch precedes the
    // value_map lookup); matching the table first and bailing on a miss
    // silently dropped every numeric value of an enum-carrying property.
    if let Some(table) = prop.values {
        // The lookup key can be a symbol id (most common) or a bool.
        match inner {
            IonValue::Symbol(id) => {
                let sym = symbols.resolve(*id);
                for (k, mapped) in table {
                    if *k == sym {
                        return Some(mapped.unwrap_or("").to_string());
                    }
                }
                // Unknown enum value → drop the declaration (calibre maps an
                // unmapped enum to Python `None`, i.e. skip it). Emitting a CSS
                // comment as the value (`float: /* unknown center */`) leaves a
                // dangling `prop:` with no value — invalid CSS, rejected by
                // epubcheck as CSS-008 ("premature end of grammar").
                return None;
            }
            IonValue::Bool(b) => {
                let key = if *b { "true" } else { "false" };
                for (k, mapped) in table {
                    if *k == key {
                        return Some(mapped.unwrap_or("").to_string());
                    }
                }
                return None;
            }
            _ => {}
        }
    }

    // Plain value: numeric, length-struct, string, color int.
    match inner {
        IonValue::Int(n) => Some(format_int_value(prop.name, *n)),
        IonValue::Float(f) => Some(format!("{}", f)),
        IonValue::Decimal(d) => Some(d.clone()),
        // `font-family: $amzn_fixup_default_font$` / `default` mean "the
        // document default font"; calibre substitutes the real family in its
        // font pass (deferred here), so emit nothing rather than invalid CSS.
        IonValue::String(s) if prop.name == "font-family" && is_default_font_name(s) => None,
        IonValue::String(s) => Some(s.clone()),
        IonValue::Symbol(id) => {
            let s = symbols.resolve(*id).to_string();
            if prop.name == "font-family" && is_default_font_name(&s) {
                None
            } else {
                Some(s)
            }
        }
        IonValue::Struct(struct_fields) => {
            // KFX length structs: { value: N, unit: "px" }
            format_length_struct(struct_fields, symbols)
        }
        _ => None,
    }
}

/// Format an integer property value. For color properties (background-color,
/// color, border-color, etc.) the int is an ARGB packed value; otherwise
/// it's treated as a unitless number (px implied for length-like props).
fn format_int_value(prop_name: &str, n: i64) -> String {
    if is_color_prop(prop_name) {
        return color_str_argb(n);
    }
    // For known length-like props, default to px.
    if is_length_prop(prop_name) {
        return format!("{}px", n);
    }
    n.to_string()
}

fn is_color_prop(name: &str) -> bool {
    matches!(
        name,
        "color"
            | "background-color"
            | "border-color"
            | "border-top-color"
            | "border-bottom-color"
            | "border-left-color"
            | "border-right-color"
            | "outline-color"
            | "text-decoration-color"
            | "text-emphasis-color"
            | "-kfx-fill-color"
            | "-kfx-link-color"
            | "-kfx-visited-color"
            | "-webkit-text-stroke-color"
            | "column-rule-color"
    )
}

fn is_length_prop(name: &str) -> bool {
    matches!(
        name,
        "font-size"
            | "line-height"
            | "letter-spacing"
            | "word-spacing"
            | "text-indent"
            | "margin"
            | "margin-top"
            | "margin-bottom"
            | "margin-left"
            | "margin-right"
            | "padding"
            | "padding-top"
            | "padding-bottom"
            | "padding-left"
            | "padding-right"
            | "border-width"
            | "border-top-width"
            | "border-bottom-width"
            | "border-left-width"
            | "border-right-width"
            | "width"
            | "height"
            | "min-width"
            | "min-height"
            | "max-width"
            | "max-height"
            | "top"
            | "left"
            | "right"
            | "bottom"
    )
}

/// Format a KFX length struct (`{value, unit}`) as a CSS length string.
fn format_length_struct(fields: &[(u64, IonValue)], symbols: &SymbolTable) -> Option<String> {
    let mut value: Option<String> = None;
    let mut unit: Option<&str> = None;
    for (k, v) in fields {
        let key = symbols.resolve(*k);
        match key {
            "value" => {
                value = match v.unwrap_annotated() {
                    IonValue::Int(n) => Some(n.to_string()),
                    IonValue::Float(f) => Some(format!("{}", f)),
                    IonValue::Decimal(d) => Some(d.clone()),
                    _ => None,
                };
            }
            "unit" => {
                if let IonValue::Symbol(id) = v.unwrap_annotated() {
                    let sym = symbols.resolve(*id);
                    unit = length_unit_for(sym);
                }
            }
            _ => {}
        }
    }
    match (value, unit) {
        (Some(v), Some(u)) => Some(format!("{}{}", v, u)),
        (Some(v), None) => Some(v),
        _ => None,
    }
}

/// Format an ARGB-packed color int as a CSS color string. Calibre emits
/// `#RRGGBB` or `rgba(r,g,b,a)` depending on alpha. We start with the
/// simple subset.
fn color_str_argb(n: i64) -> String {
    let v = n as u32;
    let alpha = (v >> 24) & 0xff;
    let r = (v >> 16) & 0xff;
    let g = (v >> 8) & 0xff;
    let b = v & 0xff;
    if alpha == 0xff || alpha == 0 {
        // Calibre uses #000000-style; map known shortcuts later.
        let hex = format!("#{:02x}{:02x}{:02x}", r, g, b);
        if let Some(name) = COLOR_NAME
            .iter()
            .find(|(h, _)| *h == hex.as_str())
            .map(|(_, n)| *n)
        {
            name.to_string()
        } else {
            hex
        }
    } else {
        let a = alpha as f64 / 255.0;
        format!("rgba({}, {}, {}, {:.3})", r, g, b, a)
    }
}

/// Calibre's short color-name table. Used to abbreviate common hex codes.
static COLOR_NAME: &[(&str, &str)] = &[
    ("#000000", "black"),
    ("#000080", "navy"),
    ("#0000ff", "blue"),
    ("#008000", "green"),
    ("#008080", "teal"),
    ("#00ff00", "lime"),
    ("#00ffff", "cyan"),
    ("#800000", "maroon"),
    ("#800080", "purple"),
    ("#808000", "olive"),
    ("#808080", "gray"),
    ("#ff0000", "red"),
    ("#ff00ff", "magenta"),
    ("#ffff00", "yellow"),
    ("#ffffff", "white"),
];

// ---------------------------------------------------------------------------
// YJ_PROPERTY_INFO — direct port of calibre's table.
//
// Keys are KFX symbol names (resolved via the symbol table). Values are
// `Prop { name, values? }`. For enumerated properties, `values` lists
// (kfx_symbol_name → css_value); `None` value means "drop the declaration".
// ---------------------------------------------------------------------------

static BORDER_STYLES: &[(&str, Option<&str>)] = &[
    ("none", Some("none")),
    ("solid", Some("solid")),
    ("dotted", Some("dotted")),
    ("dashed", Some("dashed")),
    ("double", Some("double")),
    ("ridge", Some("ridge")),
    ("groove", Some("groove")),
    ("inset", Some("inset")),
    ("outset", Some("outset")),
];

static YJ_PROPERTY_INFO: &[(&str, Prop)] = &[
    // ---- text / font ----
    (
        "font_family",
        Prop {
            name: "font-family",
            values: None,
        },
    ),
    (
        "font_size",
        Prop {
            name: "font-size",
            values: None,
        },
    ),
    (
        "font_style",
        Prop {
            name: "font-style",
            values: Some(&[
                ("italic", Some("italic")),
                ("normal", Some("normal")),
                ("oblique", Some("oblique")),
            ]),
        },
    ),
    // Direct port of calibre's `Prop("font-weight", {...})`
    // (yj_to_epub_properties.py:238), keyed by symbol: $350 normal, $355 thin→100,
    // $356 ultra_light→200, $357 light→300, $359 medium→500, $360 semi_bold→600,
    // $361 bold, $362 ultra_bold→800, $363 heavy→900. ($358 "book" is unmapped, as
    // in calibre.) boko's prior `font_weight_100…` keys never matched a real symbol
    // name, so the whole family silently dropped.
    (
        "font_weight",
        Prop {
            name: "font-weight",
            values: Some(&[
                ("normal", Some("normal")),
                ("thin", Some("100")),
                ("ultra_light", Some("200")),
                ("light", Some("300")),
                ("medium", Some("500")),
                ("semi_bold", Some("600")),
                ("bold", Some("bold")),
                ("ultra_bold", Some("800")),
                ("heavy", Some("900")),
            ]),
        },
    ),
    (
        "font_variant",
        Prop {
            name: "font-variant",
            values: Some(&[
                ("normal", Some("normal")),
                ("small-caps", Some("small-caps")),
            ]),
        },
    ),
    (
        "font_stretch",
        Prop {
            name: "font-stretch",
            values: Some(&[
                ("condensed", Some("condensed")),
                ("expanded", Some("expanded")),
                ("normal", Some("normal")),
                ("semi-condensed", Some("semi-condensed")),
                ("semi-expanded", Some("semi-expanded")),
            ]),
        },
    ),
    (
        "text_color",
        Prop {
            name: "color",
            values: None,
        },
    ),
    (
        "text_background_color",
        Prop {
            name: "background-color",
            values: None,
        },
    ),
    (
        "text_alignment",
        Prop {
            name: "text-align",
            values: Some(&[
                ("center", Some("center")),
                ("justify", Some("justify")),
                ("left", Some("left")),
                ("right", Some("right")),
            ]),
        },
    ),
    (
        "text_alignment_last",
        Prop {
            name: "text-align-last",
            values: Some(&[
                ("auto", Some("auto")),
                ("center", Some("center")),
                ("end", Some("end")),
                ("justify", Some("justify")),
                ("left", Some("left")),
                ("right", Some("right")),
                ("start", Some("start")),
            ]),
        },
    ),
    (
        "text_indent",
        Prop {
            name: "text-indent",
            values: None,
        },
    ),
    (
        "text_transform",
        Prop {
            name: "text-transform",
            values: Some(&[
                ("lowercase", Some("lowercase")),
                ("none", Some("none")),
                ("capitalize", Some("capitalize")),
                ("uppercase", Some("uppercase")),
            ]),
        },
    ),
    (
        "letterspacing",
        Prop {
            name: "letter-spacing",
            values: None,
        },
    ),
    (
        "wordspacing",
        Prop {
            name: "word-spacing",
            values: None,
        },
    ),
    (
        "line_height",
        Prop {
            name: "line-height",
            values: Some(&[("auto", Some("normal"))]),
        },
    ),
    // `language` is intentionally NOT mapped to CSS. Calibre's
    // `-kfx-attrib-xml-lang` is a sentinel for "set xml:lang attribute",
    // not real CSS, and is stripped by simplify_styles before serialization.
    // Book-level `xml:lang` on every spine `<html>` (set in `process_section`)
    // covers the same intent; per-element lang overrides are rare and not
    // present in our corpus.

    // ---- writing-mode (THE big one for this port) ----
    (
        "writing_mode",
        Prop {
            name: "writing-mode",
            values: Some(&[
                ("horizontal_tb", Some("horizontal-tb")),
                ("vertical_rl", Some("vertical-rl")),
                ("vertical_lr", Some("vertical-lr")),
            ]),
        },
    ),
    // ---- margins / padding / dimensions ----
    (
        "margin",
        Prop {
            name: "margin",
            values: None,
        },
    ),
    (
        "margin_top",
        Prop {
            name: "margin-top",
            values: None,
        },
    ),
    (
        "margin_bottom",
        Prop {
            name: "margin-bottom",
            values: None,
        },
    ),
    (
        "margin_left",
        Prop {
            name: "margin-left",
            values: None,
        },
    ),
    (
        "margin_right",
        Prop {
            name: "margin-right",
            values: None,
        },
    ),
    (
        "padding",
        Prop {
            name: "padding",
            values: None,
        },
    ),
    (
        "padding_top",
        Prop {
            name: "padding-top",
            values: None,
        },
    ),
    (
        "padding_bottom",
        Prop {
            name: "padding-bottom",
            values: None,
        },
    ),
    (
        "padding_left",
        Prop {
            name: "padding-left",
            values: None,
        },
    ),
    (
        "padding_right",
        Prop {
            name: "padding-right",
            values: None,
        },
    ),
    (
        "width",
        Prop {
            name: "width",
            values: None,
        },
    ),
    (
        "height",
        Prop {
            name: "height",
            values: None,
        },
    ),
    (
        "min_width",
        Prop {
            name: "min-width",
            values: None,
        },
    ),
    (
        "min_height",
        Prop {
            name: "min-height",
            values: None,
        },
    ),
    (
        "max_width",
        Prop {
            name: "max-width",
            values: None,
        },
    ),
    (
        "max_height",
        Prop {
            name: "max-height",
            values: None,
        },
    ),
    (
        "top",
        Prop {
            name: "top",
            values: None,
        },
    ),
    (
        "left",
        Prop {
            name: "left",
            values: None,
        },
    ),
    (
        "right",
        Prop {
            name: "right",
            values: None,
        },
    ),
    (
        "bottom",
        Prop {
            name: "bottom",
            values: None,
        },
    ),
    // ---- borders ----
    // Keys are the canonical YJ symbol names (per `symbols.rs`): `border_color_top`,
    // `border_style_top`, `border_weight_top` — NOT the CSS-style `border_top_color`
    // ordering. The old keys never matched any KFX field, so every per-side border was
    // silently dropped on import (no box rendered in the reader). The CSS property name
    // (`name:`) is the correct CSS spelling.
    (
        "border_color",
        Prop {
            name: "border-color",
            values: None,
        },
    ),
    (
        "border_color_top",
        Prop {
            name: "border-top-color",
            values: None,
        },
    ),
    (
        "border_color_bottom",
        Prop {
            name: "border-bottom-color",
            values: None,
        },
    ),
    (
        "border_color_left",
        Prop {
            name: "border-left-color",
            values: None,
        },
    ),
    (
        "border_color_right",
        Prop {
            name: "border-right-color",
            values: None,
        },
    ),
    (
        "border_weight",
        Prop {
            name: "border-width",
            values: None,
        },
    ),
    (
        "border_weight_top",
        Prop {
            name: "border-top-width",
            values: None,
        },
    ),
    (
        "border_weight_bottom",
        Prop {
            name: "border-bottom-width",
            values: None,
        },
    ),
    (
        "border_weight_left",
        Prop {
            name: "border-left-width",
            values: None,
        },
    ),
    (
        "border_weight_right",
        Prop {
            name: "border-right-width",
            values: None,
        },
    ),
    (
        "border_style",
        Prop {
            name: "border-style",
            values: Some(BORDER_STYLES),
        },
    ),
    (
        "border_style_top",
        Prop {
            name: "border-top-style",
            values: Some(BORDER_STYLES),
        },
    ),
    (
        "border_style_bottom",
        Prop {
            name: "border-bottom-style",
            values: Some(BORDER_STYLES),
        },
    ),
    (
        "border_style_left",
        Prop {
            name: "border-left-style",
            values: Some(BORDER_STYLES),
        },
    ),
    (
        "border_style_right",
        Prop {
            name: "border-right-style",
            values: Some(BORDER_STYLES),
        },
    ),
    // ---- text emphasis (圏点) ----
    // Reverse of the export `ValueTransform::Map` in style_schema.rs. Common in
    // Japanese; previously absent here, so 圏点 was dropped on the reader path
    // (the matching export shorthand-parse gap is fixed in declaration.rs).
    (
        "text_emphasis_style",
        Prop {
            name: "text-emphasis-style",
            values: Some(&[
                ("filled_dot", Some("filled dot")),
                ("open_dot", Some("open dot")),
                ("filled_circle", Some("filled circle")),
                ("open_circle", Some("open circle")),
                ("filled_double_circle", Some("filled double-circle")),
                ("open_double_circle", Some("open double-circle")),
                ("filled_triangle", Some("filled triangle")),
                ("open_triangle", Some("open triangle")),
                ("filled_sesame", Some("filled sesame")),
                ("open_sesame", Some("open sesame")),
                ("none", None),
            ]),
        },
    ),
    // Packed-int colour (handled by `is_color_prop`).
    (
        "text_emphasis_color",
        Prop {
            name: "text-emphasis-color",
            values: None,
        },
    ),
    // ---- text-combine-upright (縦中横) ----
    (
        "text_combine",
        Prop {
            name: "text-combine-upright",
            values: Some(&[("all", Some("all")), ("none", Some("none"))]),
        },
    ),
    // ---- fragmentation (page/column breaks) ----
    // `break-inside: avoid` keeps a 罫囲み box from splitting across pages.
    (
        "break_inside",
        Prop {
            name: "break-inside",
            values: Some(&[("auto", Some("auto")), ("avoid", Some("avoid"))]),
        },
    ),
    (
        "break_before",
        Prop {
            name: "break-before",
            values: Some(&[
                ("auto", Some("auto")),
                ("avoid", Some("avoid")),
                ("always", Some("page")),
            ]),
        },
    ),
    (
        "break_after",
        Prop {
            name: "break-after",
            values: Some(&[
                ("auto", Some("auto")),
                ("avoid", Some("avoid")),
                ("always", Some("page")),
            ]),
        },
    ),
    (
        "keep_lines_together",
        Prop {
            name: "orphans",
            values: None,
        },
    ),
    // ---- lists ----
    (
        "list_style",
        Prop {
            name: "list-style-type",
            values: Some(&[
                ("none", Some("none")),
                ("disc", Some("disc")),
                ("circle", Some("circle")),
                ("square", Some("square")),
                ("numeric", Some("decimal")),
                ("roman_lower", Some("lower-roman")),
                ("roman_upper", Some("upper-roman")),
                ("alpha_lower", Some("lower-alpha")),
                ("alpha_upper", Some("upper-alpha")),
            ]),
        },
    ),
    (
        "list_style_position",
        Prop {
            name: "list-style-position",
            values: Some(&[("outside", Some("outside")), ("inside", Some("inside"))]),
        },
    ),
    // ---- text wrapping ----
    (
        "word_break",
        Prop {
            name: "word-break",
            values: Some(&[("normal", Some("normal")), ("break_all", Some("break-all"))]),
        },
    ),
    (
        "hyphens",
        Prop {
            name: "hyphens",
            values: Some(&[
                ("auto", Some("auto")),
                ("manual", Some("manual")),
                ("none", Some("none")),
            ]),
        },
    ),
    // ---- box sizing / radius / table borders ----
    (
        "sizing_bounds",
        Prop {
            name: "box-sizing",
            values: Some(&[
                ("content_bounds", Some("content-box")),
                ("border_bounds", Some("border-box")),
            ]),
        },
    ),
    (
        "border_radius_top_left",
        Prop {
            name: "border-top-left-radius",
            values: None,
        },
    ),
    (
        "border_radius_top_right",
        Prop {
            name: "border-top-right-radius",
            values: None,
        },
    ),
    (
        "border_radius_bottom_left",
        Prop {
            name: "border-bottom-left-radius",
            values: None,
        },
    ),
    (
        "border_radius_bottom_right",
        Prop {
            name: "border-bottom-right-radius",
            values: None,
        },
    ),
    (
        "border_spacing_horizontal",
        Prop {
            name: "-webkit-border-horizontal-spacing",
            values: None,
        },
    ),
    (
        "border_spacing_vertical",
        Prop {
            name: "-webkit-border-vertical-spacing",
            values: None,
        },
    ),
    (
        "table_border_collapse",
        Prop {
            name: "border-collapse",
            values: Some(&[("true", Some("collapse")), ("false", Some("separate"))]),
        },
    ),
    // ---- alignment / decoration / variant / yj breaks ----
    // `box_align` is the container's content alignment (calibre maps $580 →
    // text-align). Values left/center/right.
    (
        "box_align",
        Prop {
            name: "text-align",
            values: Some(&[
                ("center", Some("center")),
                ("left", Some("left")),
                ("right", Some("right")),
            ]),
        },
    ),
    (
        "float",
        Prop {
            name: "float",
            values: Some(&[
                ("none", Some("none")),
                ("left", Some("left")),
                ("right", Some("right")),
            ]),
        },
    ),
    // `yj.float_clear` is `$628` → `clear` in calibre; the epub→kfx direction
    // already maps `clear` back to it (`style_schema.rs`).
    (
        "yj.float_clear",
        Prop {
            name: "clear",
            values: Some(&[
                ("none", Some("none")),
                ("left", Some("left")),
                ("right", Some("right")),
                ("both", Some("both")),
            ]),
        },
    ),
    (
        "overline",
        Prop {
            name: "text-decoration-line",
            values: Some(&[("solid", Some("overline")), ("none", None)]),
        },
    ),
    (
        "glyph_transform",
        Prop {
            name: "font-variant",
            values: Some(&[("small_caps", Some("small-caps"))]),
        },
    ),
    // yj-internal break props (Amazon emits these alongside the CSS break-*).
    (
        "yj_break_before",
        Prop {
            name: "break-before",
            values: Some(&[
                ("auto", Some("auto")),
                ("always", Some("page")),
                ("avoid", Some("avoid")),
            ]),
        },
    ),
    (
        "yj_break_after",
        Prop {
            name: "break-after",
            values: Some(&[
                ("auto", Some("auto")),
                ("always", Some("page")),
                ("avoid", Some("avoid")),
            ]),
        },
    ),
    // ---- ruby ----
    // Canonical YJ symbol names (`symbols.rs`): the previous `ruby_align`/
    // `ruby_position` keys were not real symbols and never matched.
    (
        "ruby_text_align",
        Prop {
            name: "ruby-align",
            values: Some(&[
                ("center", Some("center")),
                ("space_around", Some("space-around")),
                ("space_between", Some("space-between")),
                ("start", Some("start")),
            ]),
        },
    ),
    (
        "ruby_position_vertical",
        Prop {
            name: "ruby-position",
            values: Some(&[("under", Some("under")), ("over", Some("over"))]),
        },
    ),
    (
        "ruby_position_horizontal",
        Prop {
            name: "ruby-position",
            values: Some(&[("under", Some("under")), ("over", Some("over"))]),
        },
    ),
    // ---- text decoration ----
    (
        "underline",
        Prop {
            name: "text-decoration",
            values: Some(&[
                ("dashed", Some("underline dashed")),
                ("dotted", Some("underline dotted")),
                ("double", Some("underline double")),
                ("none", None),
                ("solid", Some("underline")),
            ]),
        },
    ),
    (
        "strikethrough",
        Prop {
            name: "text-decoration",
            values: Some(&[
                ("dashed", Some("line-through dashed")),
                ("dotted", Some("line-through dotted")),
                ("double", Some("line-through double")),
                ("none", None),
                ("solid", Some("line-through")),
            ]),
        },
    ),
    // ---- spacing ----
    (
        "space_before",
        Prop {
            name: "margin-top",
            values: None,
        },
    ),
    (
        "space_after",
        Prop {
            name: "margin-bottom",
            values: None,
        },
    ),
    (
        "left_indent",
        Prop {
            name: "margin-left",
            values: None,
        },
    ),
    (
        "right_indent",
        Prop {
            name: "margin-right",
            values: None,
        },
    ),
    // ---- nobreak / whitespace ----
    (
        "nobreak",
        Prop {
            name: "white-space",
            values: Some(&[("false", Some("normal")), ("true", Some("nowrap"))]),
        },
    ),
    // ---- text orientation ----
    (
        "text_orientation",
        Prop {
            name: "text-orientation",
            values: Some(&[
                ("auto", Some("mixed")),
                ("sideways", Some("sideways")),
                ("upright", Some("upright")),
            ]),
        },
    ),
    // ---- direction (page progression) ----
    // Intentionally NOT mapped to a CSS declaration: the `direction` (and
    // `unicode-bidi`) CSS properties are forbidden in EPUB style sheets
    // (epubcheck CSS-001). Page-progression / RTL direction is carried by the
    // spine `page-progression-direction` (PPD) attribute + `writing-mode`
    // instead. Per-element
    // direction, if ever needed, belongs on the HTML `dir` attribute, not CSS.
    // ---- visibility ----
    (
        "visibility",
        Prop {
            name: "visibility",
            values: Some(&[("false", Some("hidden")), ("true", Some("visible"))]),
        },
    ),
];

/// All YJ properties seen in the book — used for the validator scorecard.
#[allow(dead_code)]
pub fn all_property_names() -> impl Iterator<Item = &'static str> {
    YJ_PROPERTY_INFO.iter().map(|(k, _)| *k)
}

/// KFX layout-hint values used by the tag-promotion pass in
/// `kfx_to_epub::content::consolidate_html`. Returns
/// `(layout_hints, heading_level)` — both from a named `$style` entity's
/// fields, not the content element itself. Calibre's
/// `LAYOUT_HINT_ELEMENT_NAMES` maps the KFX symbols:
///   `$453 caption` → `"caption"`,
///   `$282 figure`  → `"figure"`,
///   `$760 heading` → `"heading"`.
///
/// `layout_hints` is a list because a single style can declare multiple
/// (e.g. `["heading", "figure"]`). `heading_level` is a string `"1"`..`"6"`.
pub fn style_fields_layout_hints(
    fields: &[(u64, IonValue)],
    symbols: &SymbolTable,
) -> (Vec<String>, Option<String>) {
    use crate::formats::kfx::symbols::KfxSymbol;
    let mut hints: Vec<String> = Vec::new();
    let mut heading_level: Option<String> = None;
    for (k, v) in fields {
        let key = symbols.resolve(*k);
        match key {
            "layout_hints" => {
                // List of symbols. Key by symbol ID — calibre's
                // `LAYOUT_HINT_ELEMENT_NAMES` maps `$760`/`$282`/`$453`, and
                // boko's symbol table names `$760` "treat_as_title" (calibre
                // leaves it nameless). The previous code matched the resolved
                // NAME "heading", which no real symbol carries, so every
                // named-style heading was silently dropped — the root cause of
                // 0 `<hN>` on Amazon KFX whose heading-ness lives on a `$style`
                // entity rather than inline. Mirror `layout_hints_from_element_fields`.
                if let IonValue::List(items) = v.unwrap_annotated() {
                    for item in items {
                        if let IonValue::Symbol(id) = item.unwrap_annotated() {
                            let name = match *id {
                                x if x == KfxSymbol::TreatAsTitle as u64 => "heading",
                                x if x == KfxSymbol::Figure as u64 => "figure",
                                x if x == KfxSymbol::Caption as u64 => "caption",
                                _ => continue,
                            };
                            hints.push(name.to_string());
                        }
                    }
                }
            }
            "yj.semantics.heading_level" => match v.unwrap_annotated() {
                IonValue::Int(n) => heading_level = Some(n.to_string()),
                IonValue::String(s) => heading_level = Some(s.clone()),
                _ => {}
            },
            _ => {}
        }
    }
    (hints, heading_level)
}

/// Layout hints / heading level extracted from a content element's own
/// outer fields (as opposed to its named `$style` entity). Mirrors
/// `style_layout_hints_for` but reads from the inline `$761` /
/// `$790` fields. Required because boko's `export::kfx` writes the
/// layout_hints and heading_level directly on the content element, not
/// on the style entity — calibre's `LAYOUT_HINT_ELEMENT_NAMES` keys by
/// the symbol id (`$760` / `$282` / `$453`) which is the same here.
pub fn layout_hints_from_element_fields(
    fields: &[(u64, IonValue)],
) -> (Vec<String>, Option<String>) {
    use crate::formats::kfx::symbols::KfxSymbol;
    let mut hints: Vec<String> = Vec::new();
    let mut heading_level: Option<String> = None;
    if let Some(layout_hints) = get_field(fields, KfxSymbol::LayoutHints as u64)
        && let IonValue::List(items) = layout_hints.unwrap_annotated()
    {
        for item in items {
            let IonValue::Symbol(id) = item.unwrap_annotated() else {
                continue;
            };
            // Match calibre's `LAYOUT_HINT_ELEMENT_NAMES`: key by the
            // symbol id, not its name (boko's local symbol table calls
            // `$760` "treat_as_title", calibre leaves it nameless).
            let name = match *id {
                x if x == KfxSymbol::TreatAsTitle as u64 => "heading",
                x if x == KfxSymbol::Figure as u64 => "figure",
                x if x == KfxSymbol::Caption as u64 => "caption",
                _ => continue,
            };
            hints.push(name.to_string());
        }
    }
    if let Some(level) = get_field(fields, KfxSymbol::YjSemanticsHeadingLevel as u64) {
        match level.unwrap_annotated() {
            IonValue::Int(n) => heading_level = Some(n.to_string()),
            IonValue::String(s) => heading_level = Some(s.clone()),
            _ => {}
        }
    }
    (hints, heading_level)
}

/// Merged-style properties that force a block-flow `<img>` into a wrapper
/// `<div>` carrying them. KFX resolves a percentage `width` against the space
/// left between the element's own margins — how CSS sizes a block wrapper's
/// content box, not how it sizes a replaced element — and `float` / `clear` /
/// `text-align` / the `break-*` family are dead or wrong on a replaced inline
/// element. The boko-emittable subset of calibre's
/// `BLOCK_CONTAINER_PROPERTIES` (`yj_to_epub_content.py:49`), plus `clear`
/// (calibre leaves it dead on the `<img>`; Kindle honors it, the wrapper is
/// where CSS does too).
pub fn img_wrapper_trigger(prop: &str) -> bool {
    matches!(
        prop,
        "margin"
            | "margin-top"
            | "margin-left"
            | "margin-bottom"
            | "margin-right"
            | "float"
            | "clear"
            | "text-indent"
            | "text-align"
            | "text-align-last"
            | "break-before"
            | "break-after"
            | "break-inside"
            | "page-break-before"
            | "page-break-after"
            | "page-break-inside"
            | "overflow"
            | "transform"
            | "transform-origin"
            | "display"
    )
}

/// Properties that belong on the wrapper `<div>` once one exists: every
/// trigger property plus `box-sizing` (meaningless on the replaced element,
/// meaningful on the box that carries the margins).
pub fn img_wrapper_prop(prop: &str) -> bool {
    img_wrapper_trigger(prop) || prop == "box-sizing"
}

/// Partition a block-flow image's merged style (named `$style` + the content
/// element's own inline properties) into `(wrapper, img)` halves, or `None`
/// when nothing triggers a wrapper and the image stays bare.
///
/// Includes calibre's `fit_width` hoist: a float is shrink-to-fit, so a
/// child's percentage width would resolve against the float's own
/// content-derived width — circular; the author meant % of the column. The
/// percentage moves onto the float and the image fills it.
pub fn partition_image_style(merged: CssDecl) -> Option<(CssDecl, CssDecl)> {
    if !merged.items.iter().any(|(k, _)| img_wrapper_trigger(k)) {
        return None;
    }
    let mut wrapper_decl = CssDecl::new();
    let mut img_decl = CssDecl::new();
    for (k, v) in merged.items {
        if img_wrapper_prop(&k) {
            wrapper_decl.set(k, v);
        } else {
            img_decl.set(k, v);
        }
    }
    if wrapper_decl
        .items
        .iter()
        .any(|(k, v)| k == "float" && v != "none")
        && let Some(pos) = img_decl
            .items
            .iter()
            .position(|(k, v)| k == "width" && v.ends_with('%'))
    {
        let (_, w) = img_decl.items.remove(pos);
        wrapper_decl.set("width", w);
        img_decl.set("width", "100%");
    }
    Some((wrapper_decl, img_decl))
}
