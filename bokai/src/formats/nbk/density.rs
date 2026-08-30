//! Variable-density (pencil) stroke → feathered raster PNG, embedded as a
//! data-URI `<image>`.

use std::fmt::Write as _;

use base64::Engine as _;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};

use super::note_model::Stroke;

const PNG_SCALE: i64 = 8; // PNG_SCALE_FACTOR
const GAMMA: f64 = 3.5; // PNG_DENSITY_GAMMA
const FEATHER: f64 = 0.75; // PNG_EDGE_FEATHERING

/// Render a variable-density stroke as a `<image>` element appended to `out`.
/// `points` are `(x, y, thickness, density)` in canvas units (density = daf/100).
pub fn render(out: &mut String, stroke: &Stroke, points: &[(i64, i64, i64, f64)]) {
    let b = stroke.bounds;
    let bound_w = b[2] - b[0];
    let bound_h = b[3] - b[1];
    let png_w = (bound_w / PNG_SCALE).max(1);
    let png_h = (bound_h / PNG_SCALE).max(1);
    let (pw, ph) = (png_w as usize, png_h as usize);

    // Scale the centreline into the PNG grid, interpolating midpoints across any
    // spatial gap so the stamped discs stay connected.
    let mut pts: Vec<(i64, i64, f64, f64)> = Vec::with_capacity(points.len() * 2);
    let mut last: Option<(i64, i64, f64, f64)> = None;
    for &(x, y, t, d) in points {
        let x0 = (x - b[0]).div_euclid(PNG_SCALE);
        let y0 = (y - b[1]).div_euclid(PNG_SCALE);
        let r0 = t as f64 / (PNG_SCALE as f64 * 2.0);
        let cur = (x0, y0, r0, d);
        if let Some(prev) = last {
            add_midpoints(&mut pts, prev, cur);
        }
        pts.push(cur);
        last = Some(cur);
    }

    let mut rng = Rng::new(stroke.random_seed);

    // Accumulate the feathered density field (max-combined across discs).
    let mut density = vec![0.0f64; pw * ph];
    for &(x, y, r, d) in &pts {
        if r <= 0.0 {
            continue;
        }
        let adjusted = 1.0 - (1.0 - d.clamp(0.0, 1.0)).powf(GAMMA);
        let int_radius = (r * 1.5).ceil() as i64;
        for xx in (x - int_radius)..=(x + int_radius) {
            if xx < 0 || xx >= png_w {
                continue;
            }
            for yy in (y - int_radius)..=(y + int_radius) {
                if yy < 0 || yy >= png_h {
                    continue;
                }
                let i = (xx + yy * png_w) as usize;
                let dx = (x - xx) as f64;
                let dy = (y - yy) as f64;
                let rel = (dx * dx + dy * dy).sqrt() / r;
                if rel <= FEATHER {
                    if density[i] < adjusted {
                        density[i] = adjusted;
                    }
                } else if rel <= rng.next_f64() * (2.0 - FEATHER) {
                    let reduced = adjusted * ((2.0 - FEATHER) - rel);
                    if density[i] < reduced {
                        density[i] = reduced;
                    }
                }
            }
        }
    }

    // Dither to RGBA: an "on" pixel is the stroke colour, else fully transparent
    // (kfxlib's mode-"1"/"P" + transparency=1, expressed as straight alpha).
    let color = stroke.color as u32;
    let (cr, cg, cb) = (
        ((color >> 16) & 0xff) as u8,
        ((color >> 8) & 0xff) as u8,
        (color & 0xff) as u8,
    );
    let mut rgba = vec![0u8; pw * ph * 4];
    for (i, &dens) in density.iter().enumerate() {
        if rng.next_f64() < dens {
            let p = i * 4;
            rgba[p] = cr;
            rgba[p + 1] = cg;
            rgba[p + 2] = cb;
            rgba[p + 3] = 255;
        }
    }

    let mut png = Vec::new();
    if PngEncoder::new(&mut png)
        .write_image(&rgba, png_w as u32, png_h as u32, ExtendedColorType::Rgba8)
        .is_err()
    {
        return;
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    let _ = write!(
        out,
        "<image x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" \
         href=\"data:image/png;base64,{b64}\"/>",
        b[0], b[1], bound_w, bound_h
    );
}

/// One `(x, y, radius, density)` sample on the PNG-scaled centreline.
type Pt = (i64, i64, f64, f64);

/// Recursively bisect the segment `a`–`b`, appending interpolated points while
/// the gap exceeds `max(r_a, r_b, 2)`. Mirrors kfxlib `add_points_if_needed` —
/// including its integer/floor arithmetic on the radius and density.
fn add_midpoints(pts: &mut Vec<Pt>, a: Pt, b: Pt) {
    let (x1, y1, r1, d1) = a;
    let (x2, y2, r2, d2) = b;
    let dx = (x1 - x2) as f64;
    let dy = (y1 - y2) as f64;
    let distance = (dx * dx + dy * dy).sqrt();
    if distance > r1.max(r2).max(2.0) {
        let mid = (
            (x1 + x2).div_euclid(2),
            (y1 + y2).div_euclid(2),
            ((r1 + r2) / 2.0).floor(),
            ((d1 + d2) / 2.0).floor(),
        );
        add_midpoints(pts, a, mid);
        add_midpoints(pts, mid, b);
        pts.push(mid);
    }
}

/// SplitMix64 — a tiny deterministic PRNG for the dither stipple.
struct Rng(u64);

impl Rng {
    fn new(seed: i64) -> Self {
        Rng((seed as u64) ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}
