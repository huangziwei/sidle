//! Phase 1 step 6: KFX style → CSS property translation.
//!
//! Port of `yj_to_epub_properties.py`. The original is ~2500 LOC; this
//! starts with a minimal-viable port covering the property table, value
//! translation, and `writing-mode` emission. Long tail (advanced color
//! transforms, font-family normalisation, layout-hint synthesis, etc.) is
//! left to a follow-up pass — calibre handles these but the validator
//! doesn't measure them.
//!
//! Identifiers track calibre as much as Rust syntax allows.

#![allow(non_snake_case)]

use std::collections::HashMap;

use crate::kfx::ion::IonValue;

use super::loader::{BookData, SymbolTable};

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
    YJ_PROPERTY_INFO.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
}

/// CSS length unit ↔ KFX symbol map. Calibre's `YJ_LENGTH_UNITS`.
pub fn length_unit_for(symbol_name: &str) -> Option<&'static str> {
    match symbol_name {
        "ch" => Some("ch"), "cm" => Some("cm"), "em" => Some("em"),
        "ex" => Some("ex"), "in_" | "in" => Some("in"), "lh" => Some("lh"),
        "mm" => Some("mm"), "percent" => Some("%"), "pt" => Some("pt"),
        "px" => Some("px"), "rem" => Some("rem"), "vh" => Some("vh"),
        "vmax" => Some("vmax"), "vmin" => Some("vmin"), "vw" => Some("vw"),
        _ => None,
    }
}

/// A small CSS rule: selector + property/value pairs. Used when emitting
/// either an inline `style="..."` or a stylesheet entry.
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

/// Translate a KFX style's properties (as `(symbol_id, IonValue)` pairs) to
/// a CSS declaration. Mirrors calibre's `convert_yj_properties` at the
/// minimum-viable level we need for step 6.
pub fn convert_yj_properties(
    fields: &[(u64, IonValue)],
    symbols: &SymbolTable,
    book: &BookData,
) -> CssDecl {
    let _ = book; // reserved for resource_name lookups (background-image, etc.)
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
                out.set(prop.name.to_string(), value_str);
            }
        }
    }

    out
}

/// Translate a single KFX property value to a CSS string. Handles the four
/// cases calibre supports: enum (Prop.values), length (struct with unit/value),
/// color, plain int/float, string/symbol.
fn property_value(prop: &Prop, value: &IonValue, symbols: &SymbolTable) -> Option<String> {
    let inner = value.unwrap_annotated();

    // Enum value lookup.
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
                // Unknown enum value; fall through to log + emit raw symbol.
                Some(format!("/* unknown {}: {} */", prop.name, sym))
            }
            IonValue::Bool(b) => {
                let key = if *b { "true" } else { "false" };
                for (k, mapped) in table {
                    if *k == key {
                        return Some(mapped.unwrap_or("").to_string());
                    }
                }
                None
            }
            _ => None,
        }
    } else {
        // Plain value: numeric, length-struct, string, color int.
        match inner {
            IonValue::Int(n) => Some(format_int_value(prop.name, *n)),
            IonValue::Float(f) => Some(format!("{}", f)),
            IonValue::Decimal(d) => Some(d.clone()),
            IonValue::String(s) => Some(s.clone()),
            IonValue::Symbol(id) => Some(symbols.resolve(*id).to_string()),
            IonValue::Struct(struct_fields) => {
                // KFX length structs: { value: N, unit: "px" }
                format_length_struct(struct_fields, symbols)
            }
            _ => None,
        }
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
        if let Some(name) = COLOR_NAME.iter().find(|(h, _)| *h == hex.as_str()).map(|(_, n)| *n) {
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
    ("#000000", "black"), ("#000080", "navy"), ("#0000ff", "blue"),
    ("#008000", "green"), ("#008080", "teal"), ("#00ff00", "lime"),
    ("#00ffff", "cyan"), ("#800000", "maroon"), ("#800080", "purple"),
    ("#808000", "olive"), ("#808080", "gray"), ("#ff0000", "red"),
    ("#ff00ff", "magenta"), ("#ffff00", "yellow"), ("#ffffff", "white"),
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
    ("font_family", Prop { name: "font-family", values: None }),
    ("font_size", Prop { name: "font-size", values: None }),
    ("font_style", Prop { name: "font-style", values: Some(&[
        ("italic", Some("italic")), ("normal", Some("normal")), ("oblique", Some("oblique")),
    ])}),
    ("font_weight", Prop { name: "font-weight", values: Some(&[
        ("bold", Some("bold")), ("normal", Some("normal")),
        ("font_weight_100", Some("100")), ("font_weight_200", Some("200")),
        ("font_weight_300", Some("300")), ("font_weight_500", Some("500")),
        ("font_weight_600", Some("600")), ("font_weight_800", Some("800")),
        ("font_weight_900", Some("900")),
    ])}),
    ("font_variant", Prop { name: "font-variant", values: Some(&[
        ("normal", Some("normal")), ("small-caps", Some("small-caps")),
    ])}),
    ("font_stretch", Prop { name: "font-stretch", values: Some(&[
        ("condensed", Some("condensed")), ("expanded", Some("expanded")),
        ("normal", Some("normal")), ("semi-condensed", Some("semi-condensed")),
        ("semi-expanded", Some("semi-expanded")),
    ])}),
    ("text_color", Prop { name: "color", values: None }),
    ("text_background_color", Prop { name: "background-color", values: None }),
    ("text_alignment", Prop { name: "text-align", values: Some(&[
        ("center", Some("center")), ("justify", Some("justify")),
        ("left", Some("left")), ("right", Some("right")),
    ])}),
    ("text_alignment_last", Prop { name: "text-align-last", values: Some(&[
        ("auto", Some("auto")), ("center", Some("center")), ("end", Some("end")),
        ("justify", Some("justify")), ("left", Some("left")), ("right", Some("right")),
        ("start", Some("start")),
    ])}),
    ("text_indent", Prop { name: "text-indent", values: None }),
    ("text_transform", Prop { name: "text-transform", values: Some(&[
        ("lowercase", Some("lowercase")), ("none", Some("none")),
        ("capitalize", Some("capitalize")), ("uppercase", Some("uppercase")),
    ])}),
    ("letterspacing", Prop { name: "letter-spacing", values: None }),
    ("wordspacing", Prop { name: "word-spacing", values: None }),
    ("line_height", Prop { name: "line-height", values: Some(&[
        ("auto", Some("normal")),
    ])}),
    // `language` is intentionally NOT mapped to CSS. Calibre's
    // `-kfx-attrib-xml-lang` is a sentinel for "set xml:lang attribute",
    // not real CSS, and is stripped by simplify_styles before serialization.
    // Book-level `xml:lang` on every spine `<html>` (set in `process_section`)
    // covers the same intent; per-element lang overrides are rare and not
    // present in our corpus.

    // ---- writing-mode (THE big one for this port) ----
    ("writing_mode", Prop { name: "writing-mode", values: Some(&[
        ("horizontal_tb", Some("horizontal-tb")),
        ("vertical_rl", Some("vertical-rl")),
        ("vertical_lr", Some("vertical-lr")),
    ])}),

    // ---- margins / padding / dimensions ----
    ("margin", Prop { name: "margin", values: None }),
    ("margin_top", Prop { name: "margin-top", values: None }),
    ("margin_bottom", Prop { name: "margin-bottom", values: None }),
    ("margin_left", Prop { name: "margin-left", values: None }),
    ("margin_right", Prop { name: "margin-right", values: None }),
    ("padding", Prop { name: "padding", values: None }),
    ("padding_top", Prop { name: "padding-top", values: None }),
    ("padding_bottom", Prop { name: "padding-bottom", values: None }),
    ("padding_left", Prop { name: "padding-left", values: None }),
    ("padding_right", Prop { name: "padding-right", values: None }),
    ("width", Prop { name: "width", values: None }),
    ("height", Prop { name: "height", values: None }),
    ("min_width", Prop { name: "min-width", values: None }),
    ("min_height", Prop { name: "min-height", values: None }),
    ("max_width", Prop { name: "max-width", values: None }),
    ("max_height", Prop { name: "max-height", values: None }),
    ("top", Prop { name: "top", values: None }),
    ("left", Prop { name: "left", values: None }),
    ("right", Prop { name: "right", values: None }),
    ("bottom", Prop { name: "bottom", values: None }),

    // ---- borders ----
    ("border_color", Prop { name: "border-color", values: None }),
    ("border_top_color", Prop { name: "border-top-color", values: None }),
    ("border_bottom_color", Prop { name: "border-bottom-color", values: None }),
    ("border_left_color", Prop { name: "border-left-color", values: None }),
    ("border_right_color", Prop { name: "border-right-color", values: None }),
    ("border_width", Prop { name: "border-width", values: None }),
    ("border_top_width", Prop { name: "border-top-width", values: None }),
    ("border_bottom_width", Prop { name: "border-bottom-width", values: None }),
    ("border_left_width", Prop { name: "border-left-width", values: None }),
    ("border_right_width", Prop { name: "border-right-width", values: None }),
    ("border_style", Prop { name: "border-style", values: Some(BORDER_STYLES) }),
    ("border_top_style", Prop { name: "border-top-style", values: Some(BORDER_STYLES) }),
    ("border_bottom_style", Prop { name: "border-bottom-style", values: Some(BORDER_STYLES) }),
    ("border_left_style", Prop { name: "border-left-style", values: Some(BORDER_STYLES) }),
    ("border_right_style", Prop { name: "border-right-style", values: Some(BORDER_STYLES) }),

    // ---- ruby ----
    ("ruby_align", Prop { name: "ruby-align", values: Some(&[
        ("center", Some("center")), ("space-around", Some("space-around")),
        ("space-between", Some("space-between")), ("start", Some("start")),
    ])}),
    ("ruby_position", Prop { name: "ruby-position", values: Some(&[
        ("under", Some("under")), ("over", Some("over")),
    ])}),

    // ---- text decoration ----
    ("underline", Prop { name: "text-decoration", values: Some(&[
        ("dashed", Some("underline dashed")), ("dotted", Some("underline dotted")),
        ("double", Some("underline double")), ("none", None),
        ("solid", Some("underline")),
    ])}),
    ("strikethrough", Prop { name: "text-decoration", values: Some(&[
        ("dashed", Some("line-through dashed")), ("dotted", Some("line-through dotted")),
        ("double", Some("line-through double")), ("none", None),
        ("solid", Some("line-through")),
    ])}),

    // ---- spacing ----
    ("space_before", Prop { name: "margin-top", values: None }),
    ("space_after", Prop { name: "margin-bottom", values: None }),
    ("left_indent", Prop { name: "margin-left", values: None }),
    ("right_indent", Prop { name: "margin-right", values: None }),

    // ---- nobreak / whitespace ----
    ("nobreak", Prop { name: "white-space", values: Some(&[
        ("false", Some("normal")), ("true", Some("nowrap")),
    ])}),

    // ---- text orientation ----
    ("text_orientation", Prop { name: "text-orientation", values: Some(&[
        ("auto", Some("mixed")), ("sideways", Some("sideways")),
        ("upright", Some("upright")),
    ])}),

    // ---- direction (page progression) ----
    ("direction", Prop { name: "direction", values: Some(&[
        ("ltr", Some("ltr")), ("rtl", Some("rtl")),
    ])}),

    // ---- visibility ----
    ("visibility", Prop { name: "visibility", values: Some(&[
        ("false", Some("hidden")), ("true", Some("visible")),
    ])}),
];

/// All YJ properties seen in the book — used for the validator scorecard.
#[allow(dead_code)]
pub fn all_property_names() -> impl Iterator<Item = &'static str> {
    YJ_PROPERTY_INFO.iter().map(|(k, _)| *k)
}

/// Resolve a KFX style entity to its CSS declarations. Looks up the
/// `style_name` in `book.by_type[$157]`, walks the fields, and returns
/// a `CssDecl`.
pub fn style_decl_for(style_name: &str, book: &BookData) -> CssDecl {
    let Some(styles) = book.by_type.get(&(crate::kfx::symbols::KfxSymbol::Style as u64)) else {
        return CssDecl::new();
    };
    let Some(value) = styles.get(style_name) else {
        return CssDecl::new();
    };
    let Some(fields) = value.unwrap_annotated().as_struct() else {
        return CssDecl::new();
    };
    convert_yj_properties(fields, &book.symbols, book)
}

/// KFX layout-hint values used by the tag-promotion pass in
/// `content::consolidate_html`. Returns `(layout_hints, heading_level)`
/// — both come from the named `$style` entity, not the content element
/// itself. Calibre's `LAYOUT_HINT_ELEMENT_NAMES` maps the KFX symbols:
///   `$453 caption` → `"caption"`,
///   `$282 figure`  → `"figure"`,
///   `$760 heading` → `"heading"`.
///
/// `layout_hints` is a list because a single style can declare multiple
/// (e.g. `["heading", "figure"]`). `heading_level` is a string `"1"`..`"6"`.
pub fn style_layout_hints_for(
    style_name: &str,
    book: &BookData,
) -> (Vec<String>, Option<String>) {
    use crate::kfx::symbols::KfxSymbol;
    let Some(styles) = book.by_type.get(&(KfxSymbol::Style as u64)) else {
        return (Vec::new(), None);
    };
    let Some(value) = styles.get(style_name) else {
        return (Vec::new(), None);
    };
    let Some(fields) = value.unwrap_annotated().as_struct() else {
        return (Vec::new(), None);
    };
    let mut hints: Vec<String> = Vec::new();
    let mut heading_level: Option<String> = None;
    for (k, v) in fields {
        let key = book.symbols.resolve(*k);
        match key {
            "layout_hints" => {
                // List of symbols. Map each to its element-name equivalent
                // (caption / figure / heading).
                if let IonValue::List(items) = v.unwrap_annotated() {
                    for item in items {
                        if let IonValue::Symbol(id) = item.unwrap_annotated() {
                            let sym = book.symbols.resolve(*id);
                            let name = match sym {
                                "caption" => "caption",
                                "figure" => "figure",
                                "heading" => "heading",
                                _ => continue,
                            };
                            hints.push(name.to_string());
                        }
                    }
                }
            }
            "yj.semantics.heading_level" => {
                match v.unwrap_annotated() {
                    IonValue::Int(n) => heading_level = Some(n.to_string()),
                    IonValue::String(s) => heading_level = Some(s.clone()),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    (hints, heading_level)
}

/// Build a stylesheet from a deduplicated map of style_name → CssDecl.
/// Emits one class per distinct style. Class names = "s_" + safe_name.
pub fn render_stylesheet(styles_used: &HashMap<String, CssDecl>) -> String {
    let mut s = String::new();
    let mut keys: Vec<&String> = styles_used.keys().collect();
    keys.sort();
    for k in keys {
        let decl = &styles_used[k];
        if decl.is_empty() {
            continue;
        }
        s.push_str(&format!(".s_{} {{ {} }}\n", safe_class_name(k), decl.to_inline()));
    }
    s
}

fn safe_class_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
