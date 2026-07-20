//! Note-model walk: `document_data` → ordered pages → ink layers → strokes.
//!
//! Ports `process_scribe_notebook_page_section` + `process_notebook_content`
//! from kfxlib's `yj_to_epub_notebook.py`. A page container
//! (`$260`) carries the canvas size and a `$141` content list; each content
//! node (`$159 == $270`) either inlines a `$146` stroke list or references a
//! `$259` story via `$176` whose `$146` we then walk. `nmdl.stroke` nodes are
//! emitted; `nmdl.stroke_group` nodes are pure containers (we flatten them).

use std::collections::HashMap;

use crate::formats::kfx::ion::{IonParser, IonValue};

use super::NbkError;
use super::shapes::{self, ShapeIds};
use super::stroke::decode_stroke_values;
use super::symtab::SymTab;
use super::template::{self, Template};

// Structural YJ symbol ids (stable, below every notebook's local base, so they
// resolve from the shared YJ_symbols import — referenced here by id directly).
const REF_ANNOT: u64 = 598; // $598 kfx_id reference
const READING_ORDERS: u64 = 169; // $169
const SECTION_LIST: u64 = 170; // $170
const PAGE_CONTENT: u64 = 141; // $141 (page container -> content)
const STORY_LIST: u64 = 146; // $146 (story -> strokes)
const STORY_REF: u64 = 176; // $176 (content -> story)
const CONTENT_TYPE: u64 = 159; // $159
const CONTENT_CONTAINER: u64 = 270; // $270
const KVG_SVG: u64 = 272; // $272 (KVG SVG — shape-tool shapes)

/// One decoded stroke (canvas-unit coordinates).
#[derive(Debug, Clone)]
pub struct Stroke {
    pub brush_type: i64,
    pub color: i64,
    pub thickness: f64,
    /// Seeds the deterministic stipple of the variable-density (pencil) raster.
    pub random_seed: i64,
    /// `[x0, y0, x1, y1]` in canvas units; point coords are relative to `[x0,y0]`.
    pub bounds: [i64; 4],
    pub num_points: usize,
    pub position_x: Vec<i64>,
    pub position_y: Vec<i64>,
    /// thickness_adjust_factor per point (empty = constant 100).
    pub thickness_adjust: Vec<i64>,
    /// density_adjust_factor per point (empty = constant 100).
    pub density_adjust: Vec<i64>,
}

/// One notebook page.
#[derive(Debug, Clone)]
pub struct Page {
    /// The page-container fragment's `kfx_id` (e.g. `cC9KkbR1zStWRzxfccUugsw0`).
    /// On a sideloaded-doc (PDOC) notebook this is the per-page link target: the
    /// book's `.yjr` `handwritten_note` record names it as the note "body", so
    /// an ink page joins to its host-page anchor by `container_id == body`.
    pub container_id: String,
    pub canvas_width: i64,
    pub canvas_height: i64,
    pub strokes: Vec<Stroke>,
    /// Pre-rendered SVG for each shape-tool shape (circle/rectangle/line/…),
    /// composited over the template and under the ink.
    pub shapes: Vec<String>,
    /// Ruled/grid/margin background composited under the ink (`None` = blank).
    pub template: Option<Template>,
}

/// Immutable context threaded through the content walk.
struct Walk<'a> {
    parsed: &'a HashMap<&'a str, IonValue>,
    ids: &'a Ids,
    shape_ids: &'a ShapeIds,
}

/// Collected page content, in walk order.
#[derive(Default)]
struct Sink {
    strokes: Vec<Stroke>,
    shapes: Vec<String>,
}

/// Resolved local symbol ids for one notebook.
struct Ids {
    nmdl_type: u64,
    nmdl_stroke: u64,
    canvas_width: u64,
    canvas_height: u64,
    template_id: u64,
    stroke_bounds: u64,
    color: u64,
    brush_type: u64,
    thickness: u64,
    random_seed: u64,
    stroke_points: u64,
    num_points: u64,
    position_x: u64,
    position_y: u64,
    thickness_adjust: u64,
    density_adjust: u64,
}

impl Ids {
    fn resolve(sym: &SymTab) -> Result<Ids, NbkError> {
        let need = |name: &str| {
            sym.id_of(name)
                .ok_or_else(|| NbkError::Format(format!("missing symbol {name}")))
        };
        Ok(Ids {
            nmdl_type: need("nmdl.type")?,
            nmdl_stroke: need("nmdl.stroke")?,
            canvas_width: need("nmdl.canvas_width")?,
            canvas_height: need("nmdl.canvas_height")?,
            // template_id is optional (a page may be blank); resolve leniently.
            template_id: sym.id_of("nmdl.template_id").unwrap_or(u64::MAX),
            stroke_bounds: need("nmdl.stroke_bounds")?,
            color: need("nmdl.color")?,
            brush_type: need("nmdl.brush_type")?,
            thickness: need("nmdl.thickness")?,
            random_seed: sym.id_of("nmdl.random_seed").unwrap_or(u64::MAX),
            stroke_points: need("nmdl.stroke_points")?,
            num_points: need("nmdl.num_points")?,
            position_x: need("nmdl.position_x")?,
            position_y: need("nmdl.position_y")?,
            // adjust factors are optional in practice; resolve leniently.
            thickness_adjust: sym
                .id_of("nmdl.thickness_adjust_factor")
                .unwrap_or(u64::MAX),
            density_adjust: sym.id_of("nmdl.density_adjust_factor").unwrap_or(u64::MAX),
        })
    }
}

/// Parse every fragment blob into an Ion value, build the symbol table, and walk
/// the page tree into `Vec<Page>` in reading order.
pub fn build_pages(frags: &HashMap<String, Vec<u8>>) -> Result<Vec<Page>, NbkError> {
    let st_blob = frags
        .get("$ion_symbol_table")
        .ok_or_else(|| NbkError::Format("no $ion_symbol_table fragment".into()))?;
    let sym = SymTab::from_fragment(st_blob)?;
    let ids = Ids::resolve(&sym)?;
    let shape_ids = ShapeIds::resolve(&sym);

    // Parse all fragments once.
    let parsed: HashMap<&str, IonValue> = frags
        .iter()
        .filter_map(|(id, blob)| IonParser::new(blob).parse().ok().map(|v| (id.as_str(), v)))
        .collect();

    // Page order from document_data (fallback: metadata) -> $169[0].$170.
    let doc = parsed
        .get("document_data")
        .or_else(|| parsed.get("metadata"))
        .ok_or_else(|| NbkError::Format("no document_data/metadata fragment".into()))?;
    let doc_fields = doc
        .unwrap_annotated()
        .as_struct()
        .ok_or_else(|| NbkError::Format("document_data is not a struct".into()))?;

    let reading_orders = field(doc_fields, READING_ORDERS)
        .and_then(|v| v.as_list())
        .ok_or_else(|| NbkError::Format("document_data has no reading orders".into()))?;
    let first = reading_orders
        .first()
        .and_then(|ro| ro.as_struct())
        .ok_or_else(|| NbkError::Format("empty reading order".into()))?;
    let section_refs = field(first, SECTION_LIST)
        .and_then(|v| v.as_list())
        .ok_or_else(|| NbkError::Format("reading order has no section list".into()))?;

    let walk = Walk {
        parsed: &parsed,
        ids: &ids,
        shape_ids: &shape_ids,
    };

    let mut pages = Vec::new();
    for sref in section_refs {
        let Some(cid) = ref_target(sref) else {
            continue;
        };
        let Some(container) = parsed.get(cid) else {
            continue;
        };
        let Some(cfields) = container.unwrap_annotated().as_struct() else {
            continue;
        };

        let canvas_width = field(cfields, ids.canvas_width)
            .and_then(|v| v.as_int())
            .unwrap_or(15624);
        let canvas_height = field(cfields, ids.canvas_height)
            .and_then(|v| v.as_int())
            .unwrap_or(20832);

        let mut sink = Sink::default();
        if let Some(IonValue::List(items)) = field(cfields, PAGE_CONTENT) {
            for item in items {
                process_content(item, &walk, &mut sink);
            }
        }

        let template =
            field(cfields, ids.template_id).and_then(|tid| template::resolve(tid, &parsed, &sym));

        pages.push(Page {
            container_id: cid.to_string(),
            canvas_width,
            canvas_height,
            strokes: sink.strokes,
            shapes: sink.shapes,
            template,
        });
    }

    Ok(pages)
}

/// Recursively walk a content node, collecting `nmdl.stroke`s into
/// `sink.strokes` and shape-tool shapes (`$272` nodes) into `sink.shapes`.
fn process_content(node: &IonValue, walk: &Walk, sink: &mut Sink) {
    // A bare reference resolves to the target fragment.
    if let Some(target) = ref_target(node) {
        if let Some(frag) = walk.parsed.get(target) {
            process_content(frag, walk, sink);
        }
        return;
    }

    let Some(fields) = node.unwrap_annotated().as_struct() else {
        return;
    };

    match field(fields, CONTENT_TYPE).and_then(|v| v.as_symbol()) {
        // $270 containers carry walkable content (inline list or a $176 story).
        Some(CONTENT_CONTAINER) => {
            if let Some(IonValue::List(items)) = field(fields, STORY_LIST) {
                for item in items {
                    process_content(item, walk, sink);
                }
            } else if let Some(story_ref) = field(fields, STORY_REF).and_then(ref_target)
                && let Some(story) = walk.parsed.get(story_ref)
                && let Some(sfields) = story.unwrap_annotated().as_struct()
                && let Some(IonValue::List(items)) = field(sfields, STORY_LIST)
            {
                for item in items {
                    process_content(item, walk, sink);
                }
            }
        }
        // $272 KVG-SVG nodes are shape-tool shapes (circle/rectangle/line/…).
        Some(KVG_SVG) => {
            if let Some(svg) = shapes::render_kvg_svg(fields, walk.shape_ids) {
                sink.shapes.push(svg);
            }
        }
        _ => {}
    }

    // Emit if this node is itself a stroke.
    if field(fields, walk.ids.nmdl_type).and_then(|v| v.as_symbol()) == Some(walk.ids.nmdl_stroke)
        && let Some(stroke) = parse_stroke(fields, walk.ids)
    {
        sink.strokes.push(stroke);
    }
}

fn parse_stroke(fields: &[(u64, IonValue)], ids: &Ids) -> Option<Stroke> {
    let bounds_list = field(fields, ids.stroke_bounds)?.as_list()?;
    if bounds_list.len() < 4 {
        return None;
    }
    let bounds = [
        bounds_list[0].as_int()?,
        bounds_list[1].as_int()?,
        bounds_list[2].as_int()?,
        bounds_list[3].as_int()?,
    ];

    let brush_type = field(fields, ids.brush_type)
        .and_then(|v| v.as_int())
        .unwrap_or(0);
    let color = field(fields, ids.color)
        .and_then(|v| v.as_int())
        .unwrap_or(0);
    let thickness = field(fields, ids.thickness).and_then(as_f64).unwrap_or(0.0);
    let random_seed = field(fields, ids.random_seed)
        .and_then(|v| v.as_int())
        .unwrap_or(0);

    let sp = field(fields, ids.stroke_points)?.as_struct()?;
    let num_points = field(sp, ids.num_points)?.as_int()? as usize;
    if num_points == 0 {
        return None;
    }

    let position_x = decode_axis(sp, ids.position_x, num_points)?;
    let position_y = decode_axis(sp, ids.position_y, num_points)?;
    let thickness_adjust = decode_axis(sp, ids.thickness_adjust, num_points).unwrap_or_default();
    let density_adjust = decode_axis(sp, ids.density_adjust, num_points).unwrap_or_default();

    Some(Stroke {
        brush_type,
        color,
        thickness,
        random_seed,
        bounds,
        num_points,
        position_x,
        position_y,
        thickness_adjust,
        density_adjust,
    })
}

fn decode_axis(sp: &[(u64, IonValue)], id: u64, num_points: usize) -> Option<Vec<i64>> {
    let blob = match field(sp, id)? {
        IonValue::Blob(b) => b,
        _ => return None,
    };
    decode_stroke_values(blob, num_points)
}

fn field(fields: &[(u64, IonValue)], id: u64) -> Option<&IonValue> {
    fields.iter().find(|(k, _)| *k == id).map(|(_, v)| v)
}

/// A `$598::"id-string"` reference -> the target fragment id.
fn ref_target(v: &IonValue) -> Option<&str> {
    if let IonValue::Annotated(anns, inner) = v
        && anns.contains(&REF_ANNOT)
    {
        return inner.as_string();
    }
    None
}

fn as_f64(v: &IonValue) -> Option<f64> {
    match v {
        IonValue::Float(f) => Some(*f),
        IonValue::Int(i) => Some(*i as f64),
        IonValue::Decimal(s) => s.parse().ok(),
        _ => None,
    }
}
