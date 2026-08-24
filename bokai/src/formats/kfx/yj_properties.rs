//! KFX style → CSS property translation: the property table, value
//! translation, `writing-mode` emission, and the block-image wrapper
//! partition. `KfxImporter` resolves named `$157` styles through it.
//!
//! `style_schema.rs` holds the export-direction table; a property here has a
//! counterpart there.

#![allow(non_snake_case)]

use crate::formats::kfx::container::{SymbolTable, get_field};
use crate::formats::kfx::ion::IonValue;
use crate::style::CssDecl;

/// One entry in the YJ → CSS property map.
#[derive(Debug, Clone)]
pub struct Prop {
    pub name: &'static str,
    /// For enumerated properties, maps the YJ symbol id to its CSS value.
    /// `None` drops the declaration.
    pub values: Option<&'static [(&'static str, Option<&'static str>)]>,
}

/// The YJ property mapping for a symbol id resolved to its text name.
pub fn prop_for(name: &str) -> Option<&'static Prop> {
    YJ_PROPERTY_INFO
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// True for a `list_style` ($100) symbol name whose marker numbers its items.
pub fn list_style_numbers_items(symbol_name: &str) -> bool {
    prop_for("list_style")
        .and_then(|prop| prop.values)
        .and_then(|table| table.iter().find(|(name, _)| *name == symbol_name))
        .and_then(|(_, css)| *css)
        .and_then(crate::style::ListStyleType::from_css)
        .is_some_and(crate::style::ListStyleType::is_ordered)
}

/// CSS length unit ↔ KFX symbol map.
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
/// a CSS declaration.
pub fn convert_yj_properties(fields: &[(u64, IonValue)], symbols: &SymbolTable) -> CssDecl {
    let mut out = CssDecl::new();

    for (k, v) in fields {
        let key_text = symbols.resolve(*k);
        let Some(prop) = prop_for(key_text) else {
            continue;
        };

        // `property_value` yields `None` for a symbol outside an enum table.
        if let Some(value_str) = property_value(prop, v, symbols)
            && !value_str.is_empty()
        {
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

    resolve_line_height_units(&mut out);
    resolve_box_align(&mut out);
    merge_axis_pair(&mut out, "background-position", "0%");
    merge_axis_pair(&mut out, "background-size", "auto");
    out
}

/// Floor for a resolved `line-height`: a line box shorter than the text it
/// holds overlaps its neighbours.
const MINIMUM_LINE_HEIGHT: f64 = 1.0;

/// Resolve a declaration's `lh` lengths into units every reader has.
/// `line-height: 1lh` becomes `normal`; any other multiple becomes the
/// unitless number, and every other property takes the multiple in `em`.
fn resolve_line_height_units(decl: &mut CssDecl) {
    let scale = crate::formats::kfx::style_schema::DOCUMENT_LINE_HEIGHT_EM as f64;
    for (name, value) in decl.items.iter_mut() {
        let Some(multiple) = value.strip_suffix("lh").and_then(|n| n.parse::<f64>().ok()) else {
            continue;
        };
        *value = if name == "line-height" {
            if (0.99..=1.01).contains(&multiple) {
                "normal".to_string()
            } else {
                css_number((multiple * scale).max(MINIMUM_LINE_HEIGHT))
            }
        } else {
            format!("{}em", css_number(multiple * scale))
        };
    }
}

/// Serialize a computed length at the five decimals the rest of the KFX
/// length path keeps, so `4.16667lh` reads back as `5em` and not
/// `5.000004em`.
fn css_number(value: f64) -> String {
    format!("{}", (value * 1e5).round() / 1e5)
}

/// Fold a KFX `…x` / `…y` pair into the CSS shorthand carrying both axes.
/// `missing` is the value for an axis the style leaves unstated.
fn merge_axis_pair(decl: &mut CssDecl, property: &str, missing: &str) {
    let x = decl.take(&format!("{property}-x"));
    let y = decl.take(&format!("{property}-y"));
    if x.is_none() && y.is_none() {
        return;
    }
    let x = x.unwrap_or_else(|| missing.to_string());
    let y = y.unwrap_or_else(|| missing.to_string());
    decl.set(property.to_string(), format!("{x} {y}"));
}

/// The pseudo-class rules a KFX style carries for hyperlink states.
/// `link_unvisited_style` and `link_visited_style` each hold a nested style
/// struct. Returns `(pseudo-class, declarations)` sorted by pseudo-class.
pub fn convert_yj_link_states(
    fields: &[(u64, IonValue)],
    symbols: &SymbolTable,
) -> Vec<(&'static str, CssDecl)> {
    let mut out: Vec<(&'static str, CssDecl)> = Vec::new();
    for (k, v) in fields {
        let pseudo = match symbols.resolve(*k) {
            "link_unvisited_style" => "link",
            "link_visited_style" => "visited",
            _ => continue,
        };
        let Some(nested) = v.unwrap_annotated().as_struct() else {
            continue;
        };
        let decl = convert_yj_properties(nested, symbols);
        if !decl.is_empty() {
            out.push((pseudo, decl));
        }
    }
    out.sort_by_key(|(pseudo, _)| *pseudo);
    out
}

/// Marker for KFX `box_align` between the property table and
/// [`resolve_box_align`]. Never reaches a stylesheet.
const BOX_ALIGN: &str = "-kfx-box-align";

/// Turn a `box_align` marker into the margins that position the box.
/// `box_align` places the box inside its container; `text_alignment` places
/// text inside the box. A block image takes [`take_box_align_margins`].
fn resolve_box_align(decl: &mut CssDecl) {
    let Some(align) = decl.take(BOX_ALIGN) else {
        return;
    };
    // A box the source positioned explicitly keeps its own margins.
    if decl.get("margin-left").is_some() || decl.get("margin-right").is_some() {
        return;
    }
    if align != "right" {
        decl.set("margin-right", "auto");
    }
    if align != "left" {
        decl.set("margin-left", "auto");
    }
}

/// True for the font-family values meaning the document default font.
fn is_default_font_name(s: &str) -> bool {
    s == "default" || s == "$amzn_fixup_default_font$"
}

/// Quote font-family names unsafe as unquoted CSS identifiers: an unquoted
/// token opening with `@` parses as an at-keyword. Names standing for the
/// reader's own font choice are dropped, leaving an empty value.
fn normalize_font_family(value: &str) -> String {
    value
        .split(',')
        .filter_map(|fam| {
            let f = fam.trim();
            if f.is_empty() || is_kfx_reader_font(f) {
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

/// True for a family name standing for the reader's own font choice:
/// `default` and `$amzn_fixup_default_font$`.
fn is_kfx_reader_font(name: &str) -> bool {
    const READER_FONTS: &[&str] = &["default", "$amzn_fixup_default_font$"];
    READER_FONTS.iter().any(|f| name.eq_ignore_ascii_case(f))
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

/// True for a run of CSS identifiers separated by single spaces: each word
/// ASCII-alphanumeric-or-hyphen, none opening with a digit, hyphen or `@`.
fn is_safe_unquoted_font(f: &str) -> bool {
    f.split(' ').all(|word| {
        !word.is_empty()
            && word.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

fn property_value(prop: &Prop, value: &IonValue, symbols: &SymbolTable) -> Option<String> {
    let inner = value.unwrap_annotated();

    // Enum lookup for symbol and bool values only. `line_height` carries
    // `auto` on some styles and a `{value, unit}` length struct on others.
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
                // An unmapped enum value yields no declaration.
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
        // A font-family naming the document default font yields no
        // declaration.
        IonValue::String(s) if prop.name == "font-family" && is_default_font_name(s) => None,
        IonValue::String(s) => Some(s.clone()),
        IonValue::Symbol(id) => {
            let s = symbols.resolve(*id).to_string();
            if prop.name == "font-family" && is_default_font_name(&s) {
                None
            } else if prop.name == "background-image" {
                // The symbol names an `external_resource`, wrapped as a CSS
                // url; the importer swaps it for the exported filename.
                Some(format!("url({s})"))
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

/// Format an integer property value: a packed ARGB value for a color
/// property, a unitless number for every other.
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
        // pt divides by KFX_PT_PER_CSS_PX into px.
        (Some(v), Some("pt")) => Some(match v.parse::<f64>() {
            Ok(pt) => format!(
                "{}px",
                ((pt / crate::formats::kfx::style_schema::KFX_PT_PER_CSS_PX) * 1e5).round() / 1e5
            ),
            Err(_) => format!("{}pt", v),
        }),
        (Some(v), Some(u)) => Some(format!("{}{}", v, u)),
        (Some(v), None) => Some(v),
        _ => None,
    }
}

/// Format an ARGB-packed color int as `#RRGGBB` or `rgba(r,g,b,a)`.
fn color_str_argb(n: i64) -> String {
    let v = n as u32;
    let alpha = (v >> 24) & 0xff;
    let r = (v >> 16) & 0xff;
    let g = (v >> 8) & 0xff;
    let b = v & 0xff;
    if alpha == 0xff || alpha == 0 {
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

/// Short color-name table, abbreviating common hex codes.
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

// YJ_PROPERTY_INFO — keys are KFX symbol names resolved through the symbol
// table; an enumerated property's `values` maps symbol name → CSS value, and
// a `None` value drops the declaration.

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
    // Keyed by symbol name: $350 normal, $355 thin→100, $356 ultra_light→200,
    // $357 light→300, $359 medium→500, $360 semi_bold→600, $361 bold,
    // $362 ultra_bold→800, $363 heavy→900. $358 book is unmapped.
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
    // `language` maps to no CSS declaration. Every spine `<html>` carries the
    // book-level `xml:lang`.

    // ---- writing-mode ----
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
    // Keys are the YJ symbol names `border_color_top`, `border_style_top`,
    // `border_weight_top`; `name` carries the CSS spelling.
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
    // Reverse of the export `ValueTransform::Map` in `style_schema.rs`; the
    // matching export shorthand parse lives in `declaration.rs`.
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
    // `box_align` positions the box within its container, and
    // [`resolve_box_align`] turns the marker into margins.
    (
        "box_align",
        Prop {
            name: BOX_ALIGN,
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
    // `yj.float_clear` is `$628`; `style_schema.rs` maps `clear` back to it.
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
    // `direction` and `unicode-bidi` map to no CSS declaration; the spine's
    // `page-progression-direction` and `writing-mode` carry the axis.
    // ---- visibility ----
    (
        "visibility",
        Prop {
            name: "visibility",
            values: Some(&[("false", Some("hidden")), ("true", Some("visible"))]),
        },
    ),
    // ---- vertical alignment ----
    // `baseline_style` carries superscript and subscript. `normal` maps to
    // nothing: `vertical-align` does not inherit and computes to `baseline`.
    (
        "baseline_style",
        Prop {
            name: "vertical-align",
            values: Some(&[
                ("normal", None),
                ("text_baseline", None),
                ("superscript", Some("super")),
                ("subscript", Some("sub")),
                ("center", Some("middle")),
                ("top", Some("top")),
                ("bottom", Some("bottom")),
                ("text_top", Some("text-top")),
                ("text_bottom", Some("text-bottom")),
            ]),
        },
    ),
    // The block-level spelling, used for table-cell alignment.
    (
        "yj.vertical_align",
        Prop {
            name: "vertical-align",
            values: Some(&[
                ("top", Some("top")),
                ("center", Some("middle")),
                ("bottom", Some("bottom")),
            ]),
        },
    ),
    // ---- line breaking ----
    // `line-break` inherits, and `normal` is emitted: a child resetting it
    // under a `loose` ancestor means it.
    (
        "line_break",
        Prop {
            name: "line-break",
            values: Some(&[
                ("auto", Some("auto")),
                ("loose", Some("loose")),
                ("normal", Some("normal")),
                ("strict", Some("strict")),
            ]),
        },
    ),
    // ---- backgrounds ----
    // `background_image` carries a symbol naming an `external_resource`;
    // `property_value` renders it as a CSS `url()`.
    (
        "background_image",
        Prop {
            name: "background-image",
            values: None,
        },
    ),
    (
        "background_repeat",
        Prop {
            name: "background-repeat",
            values: Some(&[
                ("no_repeat", Some("no-repeat")),
                ("repeat_x", Some("repeat-x")),
                ("repeat_y", Some("repeat-y")),
            ]),
        },
    ),
    (
        "background_positionx",
        Prop {
            name: "background-position-x",
            values: None,
        },
    ),
    (
        "background_positiony",
        Prop {
            name: "background-position-y",
            values: None,
        },
    ),
    (
        "background_sizex",
        Prop {
            name: "background-size-x",
            values: None,
        },
    ),
    (
        "background_sizey",
        Prop {
            name: "background-size-y",
            values: None,
        },
    ),
    (
        "background_origin",
        Prop {
            name: "background-origin",
            values: Some(&[
                ("border_bounds", Some("border-box")),
                ("padding_bounds", Some("padding-box")),
                ("content_bounds", Some("content-box")),
            ]),
        },
    ),
    // ---- outline / decoration colours ----
    // Colour-valued, no enum table: the shared value converter turns the
    // packed 32-bit ARGB integer into a CSS colour.
    (
        "underline_color",
        Prop {
            name: "text-decoration-color",
            values: None,
        },
    ),
    (
        "outline_color",
        Prop {
            name: "outline-color",
            values: None,
        },
    ),
    (
        "outline_weight",
        Prop {
            name: "outline-width",
            values: None,
        },
    ),
];

/// All YJ properties seen in the book — used for the validator scorecard.
#[allow(dead_code)]
pub fn all_property_names() -> impl Iterator<Item = &'static str> {
    YJ_PROPERTY_INFO.iter().map(|(k, _)| *k)
}

/// KFX layout-hint values from a named `$style` entity's fields. Returns
/// `(layout_hints, heading_level)`: `$760` → `"heading"`, `$282` → `"figure"`,
/// `$453` → `"caption"`, and a heading level `"1"`..`"6"`.
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
                // List of symbols keyed by symbol id: `symbols.rs` names
                // `$760` "treat_as_title".
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

/// Layout hints and heading level from a content element's own `$761` /
/// `$790` fields. `export::kfx` writes both on the content element.
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
            // Keyed by symbol id: `symbols.rs` names `$760` "treat_as_title".
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
/// `<div>` carrying them: a percentage `width`, `float`, `clear`,
/// `text-align` and the `break-*` family.
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

/// Read back the `box_align` an image style's auto margins stand for,
/// removing them. `None` when the pair is not the one [`resolve_box_align`]
/// writes.
fn take_box_align_margins(decl: &mut CssDecl) -> Option<&'static str> {
    let align = match (decl.get("margin-left"), decl.get("margin-right")) {
        (Some("auto"), Some("auto")) => "center",
        (Some("auto"), None) => "right",
        (None, Some("auto")) => "left",
        _ => return None,
    };
    decl.take("margin-left");
    decl.take("margin-right");
    Some(align)
}

/// Partition a block-flow image's merged style into `(wrapper, img)` halves,
/// `None` when no property triggers a wrapper. A percentage `width` moves
/// onto the wrapper; a `box_align` beside a `float` is dropped.
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
    let floated = wrapper_decl
        .items
        .iter()
        .any(|(k, v)| k == "float" && v != "none");
    if floated
        && let Some(pos) = img_decl
            .items
            .iter()
            .position(|(k, v)| k == "width" && v.ends_with('%'))
    {
        let (_, w) = img_decl.items.remove(pos);
        wrapper_decl.set("width", w);
        img_decl.set("width", "100%");
    }
    if let Some(align) = take_box_align_margins(&mut wrapper_decl)
        && !floated
    {
        wrapper_decl.set("text-align", align);
    }
    Some((wrapper_decl, img_decl))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::kfx::container::symbol_id_for_name;

    /// Build a style field list from `(property, value)` symbol names.
    fn style(pairs: &[(&str, &str)]) -> Vec<(u64, IonValue)> {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    symbol_id_for_name(k).unwrap_or_else(|| panic!("unknown property {k}")),
                    IonValue::Symbol(
                        symbol_id_for_name(v).unwrap_or_else(|| panic!("unknown value {v}")),
                    ),
                )
            })
            .collect()
    }

    fn css(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        let symbols = SymbolTable::from_fragment(None);
        convert_yj_properties(&style(pairs), &symbols).items
    }

    /// A `{value, unit}` length field, the shape every KFX dimension takes.
    fn length(property: &str, value: &str, unit: &str) -> (u64, IonValue) {
        (
            symbol_id_for_name(property).unwrap_or_else(|| panic!("unknown property {property}")),
            IonValue::Struct(vec![
                (
                    symbol_id_for_name("value").expect("value symbol"),
                    IonValue::Decimal(value.to_string()),
                ),
                (
                    symbol_id_for_name("unit").expect("unit symbol"),
                    IonValue::Symbol(symbol_id_for_name(unit).expect("unit value")),
                ),
            ]),
        )
    }

    fn convert(fields: &[(u64, IonValue)]) -> CssDecl {
        convert_yj_properties(fields, &SymbolTable::from_fragment(None))
    }

    /// Superscript and subscript are the reason `baseline_style` matters:
    /// dropped, a footnote marker or an ordinal arrives as ordinary text.
    #[test]
    fn baseline_style_carries_super_and_subscript() {
        assert_eq!(
            css(&[("baseline_style", "superscript")]),
            vec![("vertical-align".to_string(), "super".to_string())]
        );
        assert_eq!(
            css(&[("baseline_style", "subscript")]),
            vec![("vertical-align".to_string(), "sub".to_string())]
        );
        assert_eq!(
            css(&[("baseline_style", "text_bottom")]),
            vec![("vertical-align".to_string(), "text-bottom".to_string())]
        );
    }

    /// `pt` lengths come out as `px`; `em` and `percent` keep their unit.
    #[test]
    fn a_point_length_reads_back_as_the_pixels_it_states() {
        let symbols = SymbolTable::from_fragment(None);
        let length = |value: &str, unit: &str| {
            let fields = vec![
                (
                    symbol_id_for_name("value").expect("value symbol"),
                    IonValue::Decimal(value.to_string()),
                ),
                (
                    symbol_id_for_name("unit").expect("unit symbol"),
                    IonValue::Symbol(symbol_id_for_name(unit).expect("unit value")),
                ),
            ];
            format_length_struct(&fields, &symbols)
        };
        assert_eq!(length("0.45", "pt"), Some("1px".to_string()));
        assert_eq!(length("1.8", "pt"), Some("4px".to_string()));
        assert_eq!(length("1.5", "em"), Some("1.5em".to_string()));
        assert_eq!(length("75", "percent"), Some("75%".to_string()));
    }

    /// `vertical-align` does not inherit and carries no declaration at its
    /// initial value.
    #[test]
    fn baseline_style_normal_emits_nothing() {
        assert!(css(&[("baseline_style", "normal")]).is_empty());
    }

    /// The block-level spelling uses CSS's `middle`, not KFX's `center`.
    #[test]
    fn yj_vertical_align_maps_center_to_middle() {
        assert_eq!(
            css(&[("yj.vertical_align", "center")]),
            vec![("vertical-align".to_string(), "middle".to_string())]
        );
    }

    /// `line-break` DOES inherit, so `normal` under a `loose` ancestor is a
    /// real reset and must survive.
    #[test]
    fn line_break_keeps_its_initial_value() {
        assert_eq!(
            css(&[("line_break", "loose")]),
            vec![("line-break".to_string(), "loose".to_string())]
        );
        assert_eq!(
            css(&[("line_break", "normal")]),
            vec![("line-break".to_string(), "normal".to_string())]
        );
    }

    #[test]
    fn background_origin_maps_bounds_to_box() {
        assert_eq!(
            css(&[("background_origin", "border_bounds")]),
            vec![("background-origin".to_string(), "border-box".to_string())]
        );
    }

    /// Colour-valued additions go through the shared value converter, the
    /// same path `text_color` uses.
    #[test]
    fn underline_color_becomes_a_css_colour() {
        let symbols = SymbolTable::from_fragment(None);
        let fields = vec![(
            symbol_id_for_name("underline_color").unwrap(),
            IonValue::Int(4278190080), // opaque black
        )];
        let out = convert_yj_properties(&fields, &symbols).items;
        let (name, value) = out.first().expect("a declaration");
        assert_eq!(name, "text-decoration-color");
        assert!(!value.is_empty(), "colour should convert, got empty");
    }

    /// A style carrying link states: 0xFF0000FF unvisited, 0xFF800080 visited.
    fn link_state_style() -> Vec<(u64, IonValue)> {
        let nested = |argb: i64| {
            IonValue::Struct(vec![(
                symbol_id_for_name("text_color").unwrap(),
                IonValue::Int(argb),
            )])
        };
        vec![
            (
                symbol_id_for_name("link_visited_style").unwrap(),
                nested(0xFF80_0080),
            ),
            (
                symbol_id_for_name("link_unvisited_style").unwrap(),
                nested(0xFF00_00FF),
            ),
        ]
    }

    /// The nested state styles are the one place a KFX book states its link
    /// colours, and each becomes its own pseudo-class rule.
    #[test]
    fn link_states_become_pseudo_class_rules() {
        let symbols = SymbolTable::from_fragment(None);
        let fields = link_state_style();
        let states = convert_yj_link_states(&fields, &symbols);
        let names: Vec<&str> = states.iter().map(|(p, _)| *p).collect();
        // Sorted by pseudo-class, holding the Ion field order out of the output.
        assert_eq!(names, vec!["link", "visited"]);
        for (pseudo, decl) in &states {
            let (prop, value) = decl.items.first().expect("a declaration");
            assert_eq!(prop, "color", "{pseudo} should set a colour");
            assert!(!value.is_empty());
        }
        assert_ne!(
            states[0].1.items, states[1].1.items,
            "the two states have different colours and must stay distinct"
        );
    }

    /// The base rule is the unconditional half: a nested state style must not
    /// leak into it.
    #[test]
    fn link_states_stay_out_of_the_base_rule() {
        let symbols = SymbolTable::from_fragment(None);
        assert!(
            convert_yj_properties(&link_state_style(), &symbols).is_empty(),
            "state styles are not base declarations"
        );
    }

    /// A title page's publisher logo: `box_align: center` with a percentage
    /// width, centered by the wrapper's `text-align`.
    #[test]
    fn a_centered_block_image_centers_from_its_wrapper() {
        let fields = vec![
            (
                symbol_id_for_name("box_align").expect("box_align symbol"),
                IonValue::Symbol(symbol_id_for_name("center").expect("center value")),
            ),
            length("width", "58.594", "percent"),
            length("margin_top", "6.66667", "lh"),
        ];
        let (wrapper, img) = partition_image_style(convert(&fields)).expect("a wrapper");
        assert_eq!(wrapper.get("text-align"), Some("center"));
        assert_eq!(wrapper.get("margin-left"), None, "margins aligned nothing");
        assert_eq!(wrapper.get("margin-right"), None);
        assert_eq!(wrapper.get("margin-top"), Some("8em"));
        // The width sizes the image itself; only a float hoists it upward.
        assert_eq!(img.get("width"), Some("58.594%"));
    }

    /// A full-height plate — `box_align: center` and no width at all — is the
    /// case auto margins could never have centered, since the wrapper has
    /// nothing to shrink to.
    #[test]
    fn a_centered_block_image_without_a_width_still_centers() {
        let fields = vec![
            (
                symbol_id_for_name("box_align").expect("box_align symbol"),
                IonValue::Symbol(symbol_id_for_name("center").expect("center value")),
            ),
            length("height", "100", "percent"),
        ];
        let (wrapper, img) = partition_image_style(convert(&fields)).expect("a wrapper");
        assert_eq!(wrapper.get("text-align"), Some("center"));
        assert_eq!(img.get("height"), Some("100%"));
    }

    /// `box_align` states an edge as readily as the middle, and the wrapper
    /// keeps the direction the margins encoded.
    #[test]
    fn a_right_aligned_block_image_keeps_its_edge() {
        let fields = vec![
            (
                symbol_id_for_name("box_align").expect("box_align symbol"),
                IonValue::Symbol(symbol_id_for_name("right").expect("right value")),
            ),
            length("width", "25", "percent"),
        ];
        let (wrapper, _) = partition_image_style(convert(&fields)).expect("a wrapper");
        assert_eq!(wrapper.get("text-align"), Some("right"));
    }

    /// Margins a style set itself are not `box_align` residue and stay put.
    #[test]
    fn a_block_image_keeps_margins_it_stated_itself() {
        let fields = vec![
            length("margin_left", "2", "em"),
            length("width", "50", "percent"),
        ];
        let (wrapper, _) = partition_image_style(convert(&fields)).expect("a wrapper");
        assert_eq!(wrapper.get("margin-left"), Some("2em"));
        assert_eq!(wrapper.get("text-align"), None);
    }

    /// A `box_align` beside a `float` is dropped.
    #[test]
    fn a_floated_block_image_takes_no_box_alignment() {
        let fields = vec![
            (
                symbol_id_for_name("box_align").expect("box_align symbol"),
                IonValue::Symbol(symbol_id_for_name("center").expect("center value")),
            ),
            (
                symbol_id_for_name("float").expect("float symbol"),
                IonValue::Symbol(symbol_id_for_name("left").expect("left value")),
            ),
            length("width", "40", "percent"),
        ];
        let (wrapper, img) = partition_image_style(convert(&fields)).expect("a wrapper");
        assert_eq!(wrapper.get("text-align"), None);
        // The float takes the percentage width, and the image fills it.
        assert_eq!(wrapper.get("width"), Some("40%"));
        assert_eq!(img.get("width"), Some("100%"));
    }

    /// `lh` counts line heights, resolved to a multiple in `em`.
    #[test]
    fn a_line_height_multiple_resolves_to_em() {
        let decl = convert(&[length("margin_top", "4.16667", "lh")]);
        assert_eq!(decl.get("margin-top"), Some("5em"));
        let decl = convert(&[length("margin_bottom", "0.166667", "lh")]);
        assert_eq!(decl.get("margin-bottom"), Some("0.2em"));
    }

    /// `line-height: 1lh` is KFX for the default line height, which CSS
    /// spells `normal`.
    #[test]
    fn a_single_line_height_becomes_normal() {
        let decl = convert(&[length("line_height", "1", "lh")]);
        assert_eq!(decl.get("line-height"), Some("normal"));
    }

    /// Any other multiple survives as the unitless number CSS multiplies the
    /// font-size by, floored at `MINIMUM_LINE_HEIGHT`.
    #[test]
    fn other_line_height_multiples_survive_as_numbers() {
        let decl = convert(&[length("line_height", "1.5", "lh")]);
        assert_eq!(decl.get("line-height"), Some("1.8"));
        let decl = convert(&[length("line_height", "0.5", "lh")]);
        assert_eq!(decl.get("line-height"), Some("1"));
    }
}
