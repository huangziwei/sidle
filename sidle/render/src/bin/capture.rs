//! Reads captured pages and prints the geometry each one settled on.
//!
//! Usage: `sidle-render-capture <capture>...`
//!
//! The first capture is the control. Every later one prints as its
//! difference from that control.

use std::error::Error;
use std::path::{Path, PathBuf};

use sidle_render::geom::Rect;
use sidle_render::oracle::Capture;

fn main() -> Result<(), Box<dyn Error>> {
    let paths: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if paths.is_empty() {
        eprintln!("usage: sidle-render-capture <capture>...");
        std::process::exit(2);
    }

    let mut control: Option<Geometry> = None;
    println!(
        "{:<34} {:>4} {:>4} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6}",
        "probe", "elem", "line", "top", "pitch", "left", "right", "adv", "high"
    );
    for path in &paths {
        let capture = Capture::read(path)?;
        let geometry = Geometry::of(&capture);
        let name = name_of(path);
        match &control {
            None => {
                println!("{}", geometry.row(&name));
                control = Some(geometry);
            }
            Some(base) => println!("{}", geometry.delta_row(&name, base)),
        }
    }
    Ok(())
}

/// A rectangle read along the block axis, and at each end of the inline one.
type Axes = (fn(&Rect) -> f32, fn(&Rect) -> f32, fn(&Rect) -> f32);

/// What one page settled on, in dots.
struct Geometry {
    elements: usize,
    lines: usize,
    /// Where the first line sits along the block axis.
    top: f32,
    /// The distance between one line and the next.
    pitch: f32,
    /// The inline extent text was set in.
    left: f32,
    right: f32,
    /// The advance one glyph takes, at the middle of the page's glyphs.
    advance: f32,
    /// How tall a glyph's own box is.
    height: f32,
}

impl Geometry {
    fn of(capture: &Capture) -> Self {
        let vertical = capture.is_vertical();
        let rects: Vec<Rect> = capture.runs().iter().filter_map(|e| e.rect()).collect();
        let glyphs = capture.glyph_rects();

        // `vertical` swaps which coordinate each axis reads.
        let (block_of, inline_start, inline_end): Axes = if vertical {
            (|r| r.x, |r| r.y, |r| r.bottom())
        } else {
            (|r| r.y, |r| r.x, |r| r.right())
        };

        let mut blocks: Vec<f32> = rects.iter().map(block_of).collect();
        blocks.sort_by(f32::total_cmp);
        blocks.dedup();

        let pitch = median(
            &blocks
                .windows(2)
                .map(|pair| pair[1] - pair[0])
                .collect::<Vec<f32>>(),
        );
        let advance = median(
            &glyphs
                .iter()
                .map(|g| if vertical { g.height } else { g.width })
                .collect::<Vec<f32>>(),
        );
        let height = median(
            &glyphs
                .iter()
                .map(|g| if vertical { g.width } else { g.height })
                .collect::<Vec<f32>>(),
        );

        Self {
            elements: capture.elements.len(),
            lines: capture.lines().len(),
            top: blocks.first().copied().unwrap_or_default(),
            pitch,
            left: rects
                .iter()
                .map(inline_start)
                .reduce(f32::min)
                .unwrap_or_default(),
            right: rects
                .iter()
                .map(inline_end)
                .reduce(f32::max)
                .unwrap_or_default(),
            advance,
            height,
        }
    }

    fn row(&self, name: &str) -> String {
        format!(
            "{:<34} {:>4} {:>4} {:>6.0} {:>6.0} {:>6.0} {:>6.0} {:>6.0} {:>6.0}",
            name,
            self.elements,
            self.lines,
            self.top,
            self.pitch,
            self.left,
            self.right,
            self.advance,
            self.height
        )
    }

    fn delta_row(&self, name: &str, base: &Geometry) -> String {
        let d = |a: f32, b: f32| {
            let v = a - b;
            if v.abs() < 0.5 {
                ".".to_string()
            } else {
                format!("{v:+.0}")
            }
        };
        let n = |a: usize, b: usize| {
            if a == b {
                ".".to_string()
            } else {
                format!("{:+}", a as i64 - b as i64)
            }
        };
        format!(
            "{:<34} {:>4} {:>4} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6}",
            name,
            n(self.elements, base.elements),
            n(self.lines, base.lines),
            d(self.top, base.top),
            d(self.pitch, base.pitch),
            d(self.left, base.left),
            d(self.right, base.right),
            d(self.advance, base.advance),
            d(self.height, base.height)
        )
    }
}

fn median(values: &[f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    sorted[sorted.len() / 2]
}

/// A capture's file name without its extension or page suffix.
fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .replace(".script.txt", "")
        .replace("_element.txt", "")
}
