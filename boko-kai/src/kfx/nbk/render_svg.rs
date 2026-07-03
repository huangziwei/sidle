//! Stroke → SVG rendering.
//!
//! Ports the pen-path branch of `scribe_notebook_stroke`
//! (`ref/scribe-library/kfxlib/yj_to_epub_notebook.py`): point coords are
//! `position + stroke_bounds origin`; the polyline is split into sub-paths
//! whenever per-point thickness changes (re-including up to two prior points so
//! segments stay continuous). v1 renders variable-density (pencil) strokes as a
//! plain path rather than the feathered density-map raster, and omits page
//! templates (white background) — both deferred to a later fidelity pass.

use std::fmt::Write;

use super::density;
use super::note_model::{Page, Stroke};

const BRUSH_HIGHLIGHTER: i64 = 1;
const INCLUDE_PRIOR_LINE_SEGMENT: bool = true;

/// Render one page to a standalone SVG document string (white page background +
/// ruled/grid template) — the notebook gallery view.
pub fn page_to_svg(page: &Page) -> String {
    let mut s = svg_open(page);
    // White page background.
    s.push_str("<rect x=\"0\" y=\"0\" width=\"100%\" height=\"100%\" fill=\"white\"/>");
    // Ruled/grid/margin template, composited under the ink. It is authored at
    // device-screen scale (its own viewBox), so a nested <svg> rescales it to
    // fill the high-res page canvas (preserveAspectRatio="none": the aspect
    // ratios match, so this just fills without distortion).
    if let Some(t) = &page.template {
        let _ = write!(
            s,
            "<svg x=\"0\" y=\"0\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\" \
             preserveAspectRatio=\"none\">{}</svg>",
            page.canvas_width, page.canvas_height, t.width, t.height, t.shapes_svg
        );
    }
    render_ink(&mut s, page);
    s.push_str("</svg>");
    s
}

/// Render one page as a **transparent ink-only overlay** — no white background,
/// no ruled template — for compositing on top of the host document page in the
/// reader (the ink the user drew *over* a sideloaded PDF). Keeps ALL ink: the
/// vector `<path>` pen/marker/highlighter strokes AND the raster `<image>`
/// pencil strokes (whose PNG is already transparent except where the graphite
/// lands), so the page shows through everywhere there is no ink.
pub fn page_to_overlay_svg(page: &Page) -> String {
    let mut s = svg_open(page);
    render_ink(&mut s, page);
    s.push_str("</svg>");
    s
}

/// Open an `<svg>` whose user space is the page's canvas units. The fit to the
/// host box is left to the embedding element (the overlay fills the rendered
/// page; the gallery letterboxes), so no `preserveAspectRatio` is forced here.
fn svg_open(page: &Page) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {} {}\">",
        page.canvas_width, page.canvas_height
    );
    s
}

/// Composite a page's ink: shape-tool shapes first (they precede the strokes in
/// page content order — typically guides drawn first), then the strokes.
fn render_ink(s: &mut String, page: &Page) {
    for shape in &page.shapes {
        s.push_str(shape);
    }
    for stroke in &page.strokes {
        render_stroke(s, stroke);
    }
}

fn render_stroke(s: &mut String, stroke: &Stroke) {
    if stroke.num_points == 0 || stroke.position_x.len() < stroke.num_points {
        return;
    }

    let variable_thickness = stroke.thickness_adjust.iter().any(|&t| t != 100);
    // Pencil (variable-density) strokes render as a feathered raster, not a path
    // — triggered by the data (any density_adjust ≠ 100), not the brush id.
    let variable_density = stroke.density_adjust.iter().any(|&d| d != 100);
    let base_thickness = stroke.thickness.round() as i64;
    let opacity = if stroke.brush_type == BRUSH_HIGHLIGHTER {
        0.2
    } else {
        1.0
    };

    // Build deduplicated points with per-point thickness and density.
    let mut points: Vec<(i64, i64, i64, f64)> = Vec::with_capacity(stroke.num_points);
    let mut last: Option<(i64, i64)> = None;
    for i in 0..stroke.num_points {
        let x = stroke.position_x[i] + stroke.bounds[0];
        let y = stroke.position_y[i] + stroke.bounds[1];
        let taf = if variable_thickness {
            *stroke.thickness_adjust.get(i).unwrap_or(&100)
        } else {
            100
        };
        let taf_q = (taf / 10) * 10; // QUANTIZE_THICKNESS
        let t = (stroke.thickness * taf_q as f64 / 100.0).round() as i64;
        let d = if variable_density {
            *stroke.density_adjust.get(i).unwrap_or(&100) as f64 / 100.0
        } else {
            1.0
        };
        if last != Some((x, y)) {
            points.push((x, y, t, d));
            last = Some((x, y));
        }
    }
    if points.is_empty() {
        return;
    }
    if variable_density {
        density::render(s, stroke, &points);
        return;
    }
    if points.len() < 2 {
        return;
    }

    let color = color_str(stroke.color);
    let _ = write!(
        s,
        "<g fill=\"none\" stroke=\"{color}\" stroke-width=\"{base_thickness}\" \
         stroke-linejoin=\"round\" stroke-linecap=\"round\""
    );
    if opacity < 1.0 {
        let _ = write!(s, " opacity=\"{opacity:.2}\"");
    }
    s.push('>');

    // Split into sub-paths on thickness change, re-including up to 2 prior points.
    let mut prev_t: Option<i64> = None;
    let mut path: Vec<(i64, i64)> = Vec::new();
    let mut path_t: i64 = base_thickness;

    let flush = |s: &mut String, path: &[(i64, i64)], t: i64, base: i64| {
        if path.len() < 2 {
            return;
        }
        s.push_str("<path");
        if t != base {
            let _ = write!(s, " stroke-width=\"{t}\"");
        }
        s.push_str(" d=\"");
        for (k, (x, y)) in path.iter().enumerate() {
            let _ = write!(s, "{}{} {}", if k == 0 { "M " } else { " L " }, x, y);
        }
        s.push_str("\"/>");
    };

    for (i, &(x, y, t, _)) in points.iter().enumerate() {
        if i == 0 || Some(t) != prev_t {
            // start a new sub-path
            flush(s, &path, path_t, base_thickness);
            path.clear();
            path_t = t;
            for j in if INCLUDE_PRIOR_LINE_SEGMENT {
                [2usize, 1]
            } else {
                [1, 1]
            } {
                if i >= j {
                    let (px, py, _, _) = points[i - j];
                    path.push((px, py));
                }
            }
        }
        path.push((x, y));
        prev_t = Some(t);
    }
    flush(s, &path, path_t, base_thickness);

    s.push_str("</g>");
}

fn color_str(color: i64) -> String {
    let c = (color & 0xff_ff_ff) as u32;
    format!("#{c:06x}")
}

#[cfg(test)]
mod tests {
    use super::super::template::Template;
    use super::*;

    /// A page with one straight two-point vector stroke and a (dummy) template.
    fn page_with_one_stroke() -> Page {
        Page {
            container_id: "cTESTpage0".to_string(),
            canvas_width: 1000,
            canvas_height: 2000,
            shapes: Vec::new(),
            template: Some(Template {
                width: 100,
                height: 200,
                shapes_svg: "<line x1=\"0\" y1=\"0\" x2=\"100\" y2=\"0\"/>".to_string(),
            }),
            strokes: vec![Stroke {
                brush_type: 7, // marker → vector path
                color: 0,
                thickness: 6.0,
                random_seed: 0,
                bounds: [10, 10, 200, 200],
                num_points: 2,
                position_x: vec![0, 50],
                position_y: vec![0, 60],
                thickness_adjust: Vec::new(),
                density_adjust: Vec::new(),
            }],
        }
    }

    #[test]
    fn overlay_is_transparent_and_template_free_but_keeps_ink() {
        let page = page_with_one_stroke();
        let gallery = page_to_svg(&page);
        let overlay = page_to_overlay_svg(&page);

        // Both share the canvas-unit viewBox.
        assert!(gallery.contains("viewBox=\"0 0 1000 2000\""));
        assert!(overlay.contains("viewBox=\"0 0 1000 2000\""));

        // Gallery paints a white page + the ruled template; the overlay paints
        // neither (the host PDF page must show through).
        assert!(gallery.contains("fill=\"white\""));
        assert!(gallery.contains("<line")); // template
        assert!(!overlay.contains("fill=\"white\""));
        assert!(!overlay.contains("<line"));

        // The ink itself survives into the overlay (the stroke is a <path>).
        assert!(overlay.contains("<path"));
    }

    #[test]
    fn overlay_keeps_raster_pencil_ink() {
        // A variable-density (pencil) stroke renders as a transparent <image>;
        // the overlay must keep it (it's the bulk of real handwriting).
        let mut page = page_with_one_stroke();
        page.strokes[0].brush_type = 5;
        page.strokes[0].density_adjust = vec![40, 80]; // any != 100 → raster branch
        let overlay = page_to_overlay_svg(&page);
        assert!(overlay.contains("<image"));
        assert!(!overlay.contains("fill=\"white\""));
    }
}
