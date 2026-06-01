//! Page-template (KVG) decode + render — the ruled/grid/margin under-layer.
//!
//! A Scribe notebook ships the templates its pages use inside its own
//! `note_template_collection` (reading order 1). Each content page's
//! `nmdl.template_id` points at a template page section (`$260`) whose story
//! (`$259`) holds one KVG-SVG content node (`$159 == $272`): a `$66`×`$67`
//! viewBox plus a `$250` list of path shapes (`$273` — each `$249` path-command
//! list, `$75` ARGB stroke, `$76` width). Ports the `$272`/`$273` subset of
//! `process_content` + `process_kvg_shape`
//! (`ref/scribe-library/kfxlib/yj_to_epub_content.py` / `yj_to_epub_misc.py`) —
//! enough for the line-art templates; other KVG shape types are skipped.
//!
//! The template viewBox (e.g. 1860×2480) is authored at device-screen scale,
//! while the ink page canvas is the high-res NMDL grid (e.g. 15624×20832), so
//! the renderer composites the template as a nested `<svg>` whose own viewBox
//! rescales it to fill the page — matching kfxlib, which references the template
//! SVG via a full-bleed `<image>`.

use std::collections::HashMap;
use std::fmt::Write;

use crate::kfx::ion::IonValue;

use super::symtab::SymTab;

// KVG / structural YJ symbol ids (stable, below every notebook's local base).
const CONTENT_TYPE: u64 = 159; // $159
const KVG_SVG: u64 = 272; // $272 KVG SVG content node
const KVG_PATH: u64 = 273; // $273 path shape
const SHAPE_LIST: u64 = 250; // $250
const PATH_DATA: u64 = 249; // $249
const VIEW_W: u64 = 66; // $66
const VIEW_H: u64 = 67; // $67
const FILL: u64 = 70; // $70
const STROKE: u64 = 75; // $75 (ARGB)
const STROKE_WIDTH: u64 = 76; // $76
const PAGE_CONTENT: u64 = 141; // $141
const STORY_LIST: u64 = 146; // $146
const STORY_REF: u64 = 176; // $176
const BLANK_TEMPLATE: u64 = 349; // $349 = blank / no template

/// A decoded page template: a viewBox plus pre-rendered SVG `<path>` line-art.
#[derive(Debug, Clone)]
pub struct Template {
    pub width: i64,
    pub height: i64,
    /// Concatenated `<path .../>` shape elements.
    pub shapes_svg: String,
}

/// Resolve a page's `nmdl.template_id` to a [`Template`].
///
/// Follows `template_id → template page ($260) → $141[0].$176 → template story
/// ($259) → $146[0] ($272 KVG SVG) → $66/$67 + $250 shapes`. Returns `None` for
/// the blank template (`$349`) or any unresolved / non-KVG reference.
pub fn resolve(
    template_id: &IonValue,
    parsed: &HashMap<&str, IonValue>,
    sym: &SymTab,
) -> Option<Template> {
    if template_id.as_symbol() == Some(BLANK_TEMPLATE) {
        return None;
    }
    let page_key = ref_name(template_id, sym)?;
    let page = parsed.get(page_key.as_str())?.unwrap_annotated().as_struct()?;

    // $141[0] is a content node referencing the template story via $176.
    let content = field(page, PAGE_CONTENT)?
        .as_list()?
        .first()?
        .unwrap_annotated()
        .as_struct()?;
    let story_key = ref_name(field(content, STORY_REF)?, sym)?;
    let story = parsed.get(story_key.as_str())?.unwrap_annotated().as_struct()?;

    // $146[0] is the KVG SVG content node ($159 == $272).
    let kvg = field(story, STORY_LIST)?
        .as_list()?
        .first()?
        .unwrap_annotated()
        .as_struct()?;
    if field(kvg, CONTENT_TYPE).and_then(|v| v.as_symbol()) != Some(KVG_SVG) {
        return None;
    }

    let width = field(kvg, VIEW_W).and_then(|v| v.as_int())?;
    let height = field(kvg, VIEW_H).and_then(|v| v.as_int())?;

    let mut shapes_svg = String::new();
    if let Some(IonValue::List(shapes)) = field(kvg, SHAPE_LIST) {
        for shape in shapes {
            render_shape(&mut shapes_svg, shape);
        }
    }
    if shapes_svg.is_empty() {
        return None;
    }

    Some(Template {
        width,
        height,
        shapes_svg,
    })
}

/// Render one `$250` shape. Only `$273` path shapes (the template line-art) are
/// emitted; other KVG shape types are skipped.
fn render_shape(out: &mut String, shape: &IonValue) {
    let Some(fields) = shape.unwrap_annotated().as_struct() else {
        return;
    };
    if field(fields, CONTENT_TYPE).and_then(|v| v.as_symbol()) != Some(KVG_PATH) {
        return;
    }
    let Some(d) = field(fields, PATH_DATA)
        .and_then(|v| v.as_list())
        .map(render_path)
    else {
        return;
    };
    if d.is_empty() {
        return;
    }

    out.push_str("<path d=\"");
    out.push_str(&d);
    out.push('"');
    if let Some(c) = field(fields, STROKE).and_then(|v| v.as_int()) {
        let _ = write!(out, " stroke=\"{}\"", argb_str(c));
    }
    if let Some(w) = field(fields, STROKE_WIDTH).and_then(as_f64) {
        let _ = write!(out, " stroke-width=\"{}\"", num_str(w));
    }
    match field(fields, FILL).and_then(|v| v.as_int()) {
        Some(c) => {
            let _ = write!(out, " fill=\"{}\"", argb_str(c));
        }
        // A stroked path with no fill must say so or SVG fills it black.
        None => out.push_str(" fill=\"none\""),
    }
    out.push_str("/>");
}

/// Decode a `$249` path-command list to an SVG `d` string.
/// Opcodes: `0→M(2 args) 1→L(2) 2→Q(4) 3→C(6) 4→Z(0)` (kfxlib `process_path`).
/// Shared with [`super::shapes`] (the `line`/`$273` shape primitives reuse it).
pub(super) fn render_path(cmds: &[IonValue]) -> String {
    let mut d = String::new();
    let mut i = 0;
    while i < cmds.len() {
        // Opcodes are ints in templates but floats (0e0/1e0/…) in shape paths.
        let Some(op) = as_f64(&cmds[i]).map(|f| f as i64) else {
            break;
        };
        i += 1;
        let (letter, nargs) = match op {
            0 => ('M', 2),
            1 => ('L', 2),
            2 => ('Q', 4),
            3 => ('C', 6),
            4 => ('Z', 0),
            _ => break,
        };
        if !d.is_empty() {
            d.push(' ');
        }
        d.push(letter);
        for _ in 0..nargs {
            let Some(v) = cmds.get(i).and_then(as_f64) else {
                return d;
            };
            i += 1;
            d.push(' ');
            d.push_str(&num_str(v));
        }
    }
    d
}

/// Resolve a reference value (Symbol → name, String, or annotated wrapper) to a
/// fragment-id key. Notebook refs appear as bare local symbols *or* `$598`-style
/// annotated strings depending on the field, so handle every shape.
fn ref_name(v: &IonValue, sym: &SymTab) -> Option<String> {
    match v {
        IonValue::String(s) => Some(s.clone()),
        IonValue::Symbol(id) => sym.name(*id).map(|s| s.to_string()),
        IonValue::Annotated(_, inner) => ref_name(inner, sym),
        _ => None,
    }
}

/// ARGB integer → SVG color. Opaque alpha (`0xff`) emits `#rrggbb`; otherwise
/// `rgba(...)`. Mirrors kfxlib `color_str`. Shared with [`super::shapes`].
pub(super) fn argb_str(c: i64) -> String {
    let c = c as u64;
    let alpha = ((c >> 24) & 0xff) as u8;
    let rgb = (c & 0x00ff_ffff) as u32;
    if alpha == 0xff || alpha == 0 {
        format!("#{rgb:06x}")
    } else {
        let r = (rgb >> 16) & 0xff;
        let g = (rgb >> 8) & 0xff;
        let b = rgb & 0xff;
        format!("rgba({r},{g},{b},{:.3})", alpha as f64 / 255.0)
    }
}

/// Format a number like kfxlib `value_str`: integers bare, otherwise the
/// shortest round-tripping decimal (Rust's default `f64` Display). Shared with
/// [`super::shapes`].
pub(super) fn num_str(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn as_f64(v: &IonValue) -> Option<f64> {
    match v {
        IonValue::Float(f) => Some(*f),
        IonValue::Int(i) => Some(*i as f64),
        IonValue::Decimal(s) => s.parse().ok(),
        _ => None,
    }
}

fn field(fields: &[(u64, IonValue)], id: u64) -> Option<&IonValue> {
    fields.iter().find(|(k, _)| *k == id).map(|(_, v)| v)
}
