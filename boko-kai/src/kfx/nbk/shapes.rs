//! Inline page shapes (the Scribe shape tool: circle / rectangle / triangle /
//! line / arrow).
//!
//! Unlike pen marks, a shape is **not** an `nmdl.stroke` — the device stores it
//! as a parametric KVG vector primitive inside a `$272` content node in the
//! page's story (`$250` shape list): `ellipse {cx,cy,radius_x,radius_y}`,
//! `rectangle {x,y,$56=w,$57=h}`, `polygon`/`polyline {vertex_list}`, or `line`
//! (a `$249` path). kfxlib's *notebook* renderer only walks strokes, so it drops
//! every shape (its shape page comes out as an empty `<g/>`); the earlier port
//! inherited that. This module renders them.
//!
//! Each `$272` node carries its own viewBox (`$66`×`$67`) and is positioned on
//! the page at `$59` (left) / `$58` (top) with size `$56`×`$57` — so we emit a
//! positioned nested `<svg>`, exactly as for templates. Geometry attributes
//! aside, the per-shape stroke/fill/width/transform handling mirrors kfxlib
//! `process_kvg_shape`.

use std::fmt::Write;

use crate::kfx::ion::IonValue;

use super::symtab::SymTab;
use super::template::{argb_str, num_str, render_path};

// Structural YJ symbol ids (below every notebook's local base).
const CONTENT_TYPE: u64 = 159; // $159
const KVG_PATH: u64 = 273; // $273 path shape
const SHAPE_LIST: u64 = 250; // $250
const PATH_DATA: u64 = 249; // $249
const VIEW_W: u64 = 66; // $66 viewBox width
const VIEW_H: u64 = 67; // $67 viewBox height
const WIDTH: u64 = 56; // $56 render width / rect width
const HEIGHT: u64 = 57; // $57 render height / rect height
const TOP: u64 = 58; // $58 page position (y)
const LEFT: u64 = 59; // $59 page position (x)
const FILL: u64 = 70; // $70
const STROKE: u64 = 75; // $75 (ARGB)
const STROKE_WIDTH: u64 = 76; // $76
const TRANSFORM: u64 = 98; // $98 (matrix)

/// Local-symbol ids for the shape primitives + their dimension fields, resolved
/// once per notebook (they live above the YJ base, so ids vary per file).
pub struct ShapeIds {
    ellipse: u64,
    rectangle: u64,
    polygon: u64,
    polyline: u64,
    line: u64,
    shape_dims: u64,
    cx: u64,
    cy: u64,
    radius_x: u64,
    radius_y: u64,
    vertex_list: u64,
    x: u64,
    y: u64,
}

impl ShapeIds {
    pub fn resolve(sym: &SymTab) -> ShapeIds {
        let id = |n: &str| sym.id_of(n).unwrap_or(u64::MAX);
        ShapeIds {
            ellipse: id("ellipse"),
            rectangle: id("rectangle"),
            polygon: id("polygon"),
            polyline: id("polyline"),
            line: id("line"),
            shape_dims: id("shape_dimensions"),
            cx: id("cx"),
            cy: id("cy"),
            radius_x: id("radius_x"),
            radius_y: id("radius_y"),
            vertex_list: id("vertex_list"),
            x: id("x"),
            y: id("y"),
        }
    }
}

/// Render a `$272` KVG-SVG page node (one or more shapes) as a positioned nested
/// `<svg>`, or `None` if it carries no drawable shape.
pub fn render_kvg_svg(fields: &[(u64, IonValue)], ids: &ShapeIds) -> Option<String> {
    let view_w = field(fields, VIEW_W).and_then(as_f64)?;
    let view_h = field(fields, VIEW_H).and_then(as_f64)?;
    let top = field(fields, TOP).and_then(as_f64).unwrap_or(0.0);
    let left = field(fields, LEFT).and_then(as_f64).unwrap_or(0.0);
    let w = field(fields, WIDTH).and_then(as_f64).unwrap_or(view_w);
    let h = field(fields, HEIGHT).and_then(as_f64).unwrap_or(view_h);

    let mut body = String::new();
    if let Some(IonValue::List(shapes)) = field(fields, SHAPE_LIST) {
        for shape in shapes {
            render_shape(&mut body, shape, ids);
        }
    }
    if body.is_empty() {
        return None;
    }

    // Position with a plain <g> (translate + scale), NOT a nested <svg viewBox>:
    // an `<svg>` clips to its viewport, which would lop off the part of a shape
    // that legitimately extends past its bounding box — e.g. an arrowhead prong
    // pokes a few units above the box. A <g> never clips. Local shape units are
    // 1:1 with the page unless $56/$57 differ from the $66/$67 viewBox.
    let sx = if view_w != 0.0 { w / view_w } else { 1.0 };
    let sy = if view_h != 0.0 { h / view_h } else { 1.0 };
    let mut transform = format!("translate({} {})", num_str(left), num_str(top));
    if (sx - 1.0).abs() > f64::EPSILON || (sy - 1.0).abs() > f64::EPSILON {
        let _ = write!(transform, " scale({} {})", num_str(sx), num_str(sy));
    }
    Some(format!("<g transform=\"{transform}\">{body}</g>"))
}

/// Render one `$250` shape primitive (geometry + stroke/fill/width/transform).
fn render_shape(out: &mut String, shape: &IonValue, ids: &ShapeIds) {
    let Some(f) = shape.unwrap_annotated().as_struct() else {
        return;
    };
    let Some(stype) = field(f, CONTENT_TYPE).and_then(|v| v.as_symbol()) else {
        return;
    };

    // Geometry element (opening tag + geometry attributes), by shape type.
    let opening: Option<String> = if stype == KVG_PATH || stype == ids.line {
        field(f, PATH_DATA)
            .and_then(|v| v.as_list())
            .map(render_path)
            .filter(|d| !d.is_empty())
            .map(|d| format!("<path d=\"{d}\""))
    } else if stype == ids.ellipse {
        field(f, ids.shape_dims)
            .and_then(|v| v.as_struct())
            .map(|d| {
                format!(
                    "<ellipse cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\"",
                    dim(d, ids.cx),
                    dim(d, ids.cy),
                    dim(d, ids.radius_x),
                    dim(d, ids.radius_y),
                )
            })
    } else if stype == ids.rectangle {
        field(f, ids.shape_dims)
            .and_then(|v| v.as_struct())
            .map(|d| {
                format!(
                    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"",
                    dim(d, ids.x),
                    dim(d, ids.y),
                    dim(d, WIDTH),
                    dim(d, HEIGHT),
                )
            })
    } else if stype == ids.polygon || stype == ids.polyline {
        let tag = if stype == ids.polygon {
            "polygon"
        } else {
            "polyline"
        };
        field(f, ids.shape_dims)
            .and_then(|v| v.as_struct())
            .and_then(|d| field(d, ids.vertex_list))
            .and_then(|v| v.as_list())
            .map(|verts| format!("<{tag} points=\"{}\"", points_str(verts)))
    } else {
        None // unknown shape primitive — skip
    };

    let Some(mut elem) = opening else {
        return;
    };

    if let Some(c) = field(f, STROKE).and_then(|v| v.as_int()) {
        let _ = write!(elem, " stroke=\"{}\"", argb_str(c));
    }
    if let Some(w) = field(f, STROKE_WIDTH).and_then(as_f64) {
        let _ = write!(elem, " stroke-width=\"{}\"", num_str(w));
    }
    match field(f, FILL).and_then(|v| v.as_int()) {
        Some(c) => {
            let _ = write!(elem, " fill=\"{}\"", argb_str(c));
        }
        // Shape-tool shapes are outlines; stroke without fill must say none.
        None => elem.push_str(" fill=\"none\""),
    }
    if let Some(IonValue::List(m)) = field(f, TRANSFORM)
        && let Some(t) = matrix_str(m)
    {
        let _ = write!(elem, " transform=\"{t}\"");
    }
    elem.push_str("/>");
    out.push_str(&elem);
}

/// A `shape_dimensions` sub-field as a formatted number (default `0`).
fn dim(dims: &[(u64, IonValue)], id: u64) -> String {
    field(dims, id)
        .and_then(as_f64)
        .map(num_str)
        .unwrap_or_else(|| "0".into())
}

/// A flat `[x0,y0, x1,y1, …]` vertex list → SVG `points="x0,y0 x1,y1 …"`.
fn points_str(verts: &[IonValue]) -> String {
    let mut s = String::new();
    let mut it = verts.iter();
    while let (Some(x), Some(y)) = (it.next(), it.next()) {
        if let (Some(x), Some(y)) = (as_f64(x), as_f64(y)) {
            if !s.is_empty() {
                s.push(' ');
            }
            let _ = write!(s, "{},{}", num_str(x), num_str(y));
        }
    }
    s
}

/// A 6-element affine list → SVG `transform="matrix(…)"`.
///
/// The device stores the affine in a transposed (row-vector) convention vs SVG's
/// `matrix(a b c d e f)` — so the linear part's off-diagonal terms `b`/`c` must
/// be swapped, or a rotation comes out mirrored (its sign flipped). kfxlib bakes
/// the same swap into `process_transform`'s rotate() special cases (device
/// `[0,1,-1,0]` ⇒ SVG `rotate(-90)` = `matrix(0,-1,1,0)`), but its generic
/// `matrix(...)` fallback omits it — which is exactly the arbitrary-rotation case
/// the shape-tool arrow hits, so we apply the swap here.
fn matrix_str(m: &[IonValue]) -> Option<String> {
    if m.len() != 6 {
        return None;
    }
    let mut s = String::from("matrix(");
    for (i, &idx) in [0, 2, 1, 3, 4, 5].iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&num_str(as_f64(&m[idx])?));
    }
    s.push(')');
    Some(s)
}

fn field(fields: &[(u64, IonValue)], id: u64) -> Option<&IonValue> {
    fields.iter().find(|(k, _)| *k == id).map(|(_, v)| v)
}

fn as_f64(v: &IonValue) -> Option<f64> {
    match v {
        IonValue::Float(f) => Some(*f),
        IonValue::Int(i) => Some(*i as f64),
        IonValue::Decimal(s) => s.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kfx::ion::IonValue::Float;

    #[test]
    fn matrix_swaps_b_and_c() {
        // Device affine [a,b,c,d,e,f] is transposed vs SVG: [0,1,-1,0,5,6] must
        // emit matrix(0 -1 1 0 5 6) (== SVG rotate(-90) about a translated origin).
        let m = [0.0, 1.0, -1.0, 0.0, 5.0, 6.0].map(Float);
        assert_eq!(matrix_str(&m).as_deref(), Some("matrix(0 -1 1 0 5 6)"));
    }

    #[test]
    fn points_pairs_up_a_flat_vertex_list() {
        let verts = [1.0, 2.0, -3.5, 4.0].map(Float);
        assert_eq!(points_str(&verts), "1,2 -3.5,4");
    }
}
