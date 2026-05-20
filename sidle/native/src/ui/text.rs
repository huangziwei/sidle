//! Text rasterization onto the framebuffer.
//!
//! We load the Kindle's bundled Japanese sans-serif (TBGothicMed) and
//! rasterize each glyph at a fixed size via fontdue. fontdue returns an
//! 8-bit coverage bitmap; we threshold it to 1-bit because eink's DU
//! waveform is B/W and antialiased gray smears on the panel. Coverage
//! above 96/255 becomes a black pixel, below stays white. Crisp at small
//! sizes; would need GC16 + dithering for true grayscale text.
//!
//! Glyphs are cached per (codepoint, px) because rasterization isn't free
//! and CJK titles repeat characters often.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use fontdue::{Font, FontSettings, Metrics};

use crate::eink::fb::Framebuffer;

/// Path on KOA2 firmware 5.16. If absent on newer firmware, font probe
/// (M5 setup) lists alternatives — switch the constant here.
pub const JP_FONT_PATH: &str = "/usr/java/lib/fonts/TBGothicMed_213.ttf";

const COVERAGE_THRESHOLD: u8 = 96;

pub struct TextRenderer {
    font: Font,
    px: f32,
    cache: HashMap<(char, u32), (Metrics, Vec<u8>)>,
}

impl TextRenderer {
    pub fn load(px: f32) -> Result<Self> {
        let path = Path::new(JP_FONT_PATH);
        let bytes = std::fs::read(path)
            .with_context(|| format!("read {}", path.display()))?;
        let font = Font::from_bytes(bytes, FontSettings::default())
            .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
        Ok(Self {
            font,
            px,
            cache: HashMap::new(),
        })
    }

    pub fn line_height(&self) -> u32 {
        // fontdue's `horizontal_line_metrics` is the canonical value; round
        // up so adjacent rows don't tear into each other.
        self.font
            .horizontal_line_metrics(self.px)
            .map(|m| (m.ascent - m.descent + m.line_gap).ceil() as u32)
            .unwrap_or((self.px * 1.4) as u32)
    }

    /// Total advance width of `s` at the current px. Used by the overlay
    /// to center text inside the banner.
    pub fn measure_width(&mut self, s: &str) -> u32 {
        let px_key = self.px.to_bits();
        let mut w = 0u32;
        for ch in s.chars() {
            let entry = self
                .cache
                .entry((ch, px_key))
                .or_insert_with(|| self.font.rasterize(ch, self.px));
            w = w.saturating_add(entry.0.advance_width.round().max(0.0) as u32);
        }
        w
    }

    /// Draw `s` starting at baseline (x, y_baseline). Returns the
    /// advanced X. `inverted=true` swaps colors (white-on-black) so the
    /// caller can highlight a tapped row by painting the row's background
    /// black first and calling with `inverted=true`.
    pub fn draw(
        &mut self,
        fb: &mut Framebuffer,
        x: i32,
        y_baseline: i32,
        s: &str,
        inverted: bool,
    ) -> i32 {
        let fg = if inverted { 0xFF } else { 0x00 };
        let px_key = self.px.to_bits();
        let mut cur_x = x;
        for ch in s.chars() {
            // Cache key uses bit pattern of f32 — same px always keys the same.
            let entry = self
                .cache
                .entry((ch, px_key))
                .or_insert_with(|| self.font.rasterize(ch, self.px));
            let (metrics, bitmap) = entry;
            let gx0 = cur_x + metrics.xmin;
            let gy0 = y_baseline - metrics.height as i32 - metrics.ymin;
            blit_threshold(fb, gx0, gy0, metrics.width, metrics.height, bitmap, fg);
            cur_x += metrics.advance_width.round() as i32;
        }
        cur_x
    }
}

fn blit_threshold(
    fb: &mut Framebuffer,
    x: i32,
    y: i32,
    w: usize,
    h: usize,
    coverage: &[u8],
    fg: u8,
) {
    if w == 0 || h == 0 {
        return;
    }
    let line_length = fb.fix.line_length as usize;
    let bpp = (fb.var.bits_per_pixel / 8).max(1) as usize;
    let fb_w = fb.var.xres as i32;
    let fb_h = fb.var.yres as i32;
    let pixels = fb.pixels_mut();
    for row in 0..h {
        let py = y + row as i32;
        if py < 0 || py >= fb_h {
            continue;
        }
        let row_base = py as usize * line_length;
        let cov_row = &coverage[row * w..row * w + w];
        for col in 0..w {
            let px = x + col as i32;
            if px < 0 || px >= fb_w {
                continue;
            }
            if cov_row[col] >= COVERAGE_THRESHOLD {
                let idx = row_base + px as usize * bpp;
                if idx < pixels.len() {
                    pixels[idx] = fg;
                }
            }
        }
    }
}
