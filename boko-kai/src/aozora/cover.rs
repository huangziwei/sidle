//! Generate the Aozora cover as a JPEG.
//!
//! Faithful port of `buildCoverSvg` from the aozora-epub reference tool. The
//! SVG template is identical: 1050×1500, cream background `#F2EBDA`, sage
//! `#8B9E78` double border, "青空文庫" letter-spaced collection label,
//! auto-fit title (sizes 100→36 step 4, 1–3 lines), divider, author below.
//! Rasterized via `resvg` + `tiny-skia` and encoded JPEG via the existing
//! `jpeg-encoder` dep.
//!
//! Fonts are looked up via `fontdb::load_system_fonts()` —
//! on a macOS host, so
//! `Hiragino Mincho ProN` (named first in the SVG `font-family` list)
//! resolves at runtime. No bundled fallback.

use std::io;
use std::sync::{Arc, OnceLock};

use resvg::tiny_skia;
use resvg::usvg;

const COVER_W: u32 = 1050;
const COVER_H: u32 = 1500;

/// Render a cover for the given title + author to JPEG bytes. Quality 85,
/// dimensions 1050×1500 (matches the HTML tool).
pub fn render_cover_jpeg(title: &str, author: &str) -> io::Result<Vec<u8>> {
    let svg = build_cover_svg(title, author);
    rasterize_to_jpeg(&svg)
}

// =========================================================================
// SVG template
// =========================================================================

const PAD: u32 = 60;
const BW: f32 = 2.0;
const GAP: u32 = 16;
const BG: &str = "#F2EBDA";
const BORDER: &str = "#8B9E78";
const FG: &str = "#2C2418";
const MUTED: &str = "#6B6256";
const FONT_STACK: &str = "'Hiragino Mincho ProN', 'Yu Mincho', 'Noto Serif JP', serif";

fn inner_w() -> u32 {
    // Matches the JS `var innerW = w - (pad + gap) * 2 - 60;`.
    COVER_W - (PAD + GAP) * 2 - 60
}

/// Build the cover SVG as a string. Pure function — same input always
/// produces the same SVG.
pub fn build_cover_svg(title: &str, author: &str) -> String {
    let iw = inner_w() as f32;

    // Title fit search: 100→36 step 4, 1-3 lines. Take the first font size
    // where the (worst-line) estimated width fits within `iw`.
    let mut title_font_size: f32 = 36.0;
    let mut title_lines: Vec<String> = vec![title.to_string()];
    let mut fitted = false;
    let mut fs = 100.0_f32;
    while fs >= 36.0 {
        if estimate_width(title, fs) <= iw {
            title_lines = vec![title.to_string()];
            title_font_size = fs;
            fitted = true;
            break;
        }
        let two = split_lines(title, 2, fs);
        if two.max_w <= iw {
            title_lines = two.lines;
            title_font_size = fs;
            fitted = true;
            break;
        }
        let three = split_lines(title, 3, fs);
        if three.max_w <= iw {
            title_lines = three.lines;
            title_font_size = fs;
            fitted = true;
            break;
        }
        fs -= 4.0;
    }
    if !fitted {
        // Overflow fallback — match the JS branch: 36px, 3 lines, accept overflow.
        title_font_size = 36.0;
        title_lines = split_lines(title, 3, 36.0).lines;
    }

    let title_spacing = title_font_size * 1.5;
    let title_block_h = title_lines.len() as f32 * title_spacing;
    let title_y = COVER_H as f32 * 0.35 - title_block_h / 2.0 + title_font_size;
    let author_font_size = (title_font_size * 0.45).round().clamp(26.0, 40.0);

    let mut svg = String::new();
    svg.push_str(&format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}">
"#,
        w = COVER_W,
        h = COVER_H,
    ));
    // Background
    svg.push_str(&format!(
        r#"<rect width="{w}" height="{h}" fill="{bg}"/>
"#,
        w = COVER_W,
        h = COVER_H,
        bg = BG,
    ));
    // Outer border
    svg.push_str(&format!(
        r#"<rect x="{x}" y="{y}" width="{w}" height="{h}" fill="none" stroke="{c}" stroke-width="{bw}"/>
"#,
        x = PAD,
        y = PAD,
        w = COVER_W - PAD * 2,
        h = COVER_H - PAD * 2,
        c = BORDER,
        bw = BW,
    ));
    // Inner border
    svg.push_str(&format!(
        r#"<rect x="{x}" y="{y}" width="{w}" height="{h}" fill="none" stroke="{c}" stroke-width="{bw}"/>
"#,
        x = PAD + GAP,
        y = PAD + GAP,
        w = COVER_W - (PAD + GAP) * 2,
        h = COVER_H - (PAD + GAP) * 2,
        c = BORDER,
        bw = BW * 0.5,
    ));
    // Collection label
    svg.push_str(&format!(
        r#"<text x="{x}" y="{y}" text-anchor="middle" font-family="{f}" font-size="26" letter-spacing="0.5em" fill="{c}">青空文庫</text>
"#,
        x = COVER_W as f32 / 2.0,
        y = (PAD + GAP + 56) as f32,
        f = FONT_STACK,
        c = MUTED,
    ));
    // Title (each line as its own <text> element)
    for (i, line) in title_lines.iter().enumerate() {
        let ly = title_y + i as f32 * title_spacing;
        svg.push_str(&format!(
            r#"<text x="{x}" y="{y}" text-anchor="middle" font-family="{f}" font-size="{fs}" fill="{c}">{t}</text>
"#,
            x = COVER_W as f32 / 2.0,
            y = ly,
            f = FONT_STACK,
            fs = title_font_size,
            c = FG,
            t = escape_xml(line),
        ));
    }
    // Divider
    let div_y = title_y + title_lines.len() as f32 * title_spacing + 16.0;
    svg.push_str(&format!(
        r#"<line x1="{x1}" y1="{y}" x2="{x2}" y2="{y}" stroke="{c}" stroke-width="1"/>
"#,
        x1 = COVER_W as f32 / 2.0 - 50.0,
        x2 = COVER_W as f32 / 2.0 + 50.0,
        y = div_y,
        c = BORDER,
    ));
    // Author
    let author_y = div_y + 40.0 + author_font_size;
    svg.push_str(&format!(
        r#"<text x="{x}" y="{y}" text-anchor="middle" font-family="{f}" font-size="{fs}" fill="{c}">{t}</text>
"#,
        x = COVER_W as f32 / 2.0,
        y = author_y,
        f = FONT_STACK,
        fs = author_font_size,
        c = MUTED,
        t = escape_xml(author),
    ));
    svg.push_str("</svg>");
    svg
}

/// Heuristic glyph width: CJK ≈ 1em, latin ≈ 0.55em. Matches the JS
/// `estimateWidth` exactly — same `> 0x7F` cutoff on `charCodeAt(i)`.
fn estimate_width(text: &str, font_size: f32) -> f32 {
    let mut w = 0.0;
    for c in text.chars() {
        w += if (c as u32) > 0x7F {
            font_size
        } else {
            font_size * 0.55
        };
    }
    w
}

struct SplitLines {
    lines: Vec<String>,
    max_w: f32,
}

/// JS `splitLines`: divide `text` into `n` near-equal segments by char
/// count and return them with the widest line's estimated width.
fn split_lines(text: &str, n: usize, fs: f32) -> SplitLines {
    let chars: Vec<char> = text.chars().collect();
    let chars_per_line = chars.len().div_ceil(n);
    let mut lines: Vec<String> = Vec::new();
    for i in 0..n {
        let start = i * chars_per_line;
        let end = ((i + 1) * chars_per_line).min(chars.len());
        if start >= end {
            continue;
        }
        let seg: String = chars[start..end].iter().collect();
        lines.push(seg);
    }
    let max_w = lines
        .iter()
        .map(|l| estimate_width(l, fs))
        .fold(0.0_f32, f32::max);
    SplitLines { lines, max_w }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// =========================================================================
// Rasterize + JPEG encode
// =========================================================================

/// Cache the system-font scan. `load_system_fonts()` walks every
/// `/Library/Fonts`, `~/Library/Fonts`, `/System/Library/Fonts` entry on
/// macOS — ~150-300 ms per call. We scan once per process and clone the
/// `Arc<Database>` into each render's `Options`.
fn cached_fontdb() -> Arc<usvg::fontdb::Database> {
    static FONTDB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTDB
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone()
}

fn rasterize_to_jpeg(svg: &str) -> io::Result<Vec<u8>> {
    // usvg 0.47's `Options` owns the fontdb (Arc-wrapped). Reuse the
    // cached one — cloning the Arc is constant-time.
    let opts = usvg::Options {
        // Default `font-family` if SVG names none. The HTML tool always
        // names a font stack, but set a sane default for robustness.
        font_family: "Hiragino Mincho ProN".to_string(),
        fontdb: cached_fontdb(),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_str(svg, &opts)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("svg parse: {e}")))?;

    let mut pixmap =
        tiny_skia::Pixmap::new(COVER_W, COVER_H).ok_or_else(|| io::Error::other("alloc pixmap"))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );

    // tiny-skia gives us premultiplied RGBA. Cover has alpha=1 throughout
    // (solid cream background), so premultiplied RGB == straight RGB and
    // jpeg-encoder can take the RGBA data directly (alpha is ignored).
    let mut out: Vec<u8> = Vec::with_capacity(256 * 1024);
    let encoder = jpeg_encoder::Encoder::new(&mut out, 85);
    encoder
        .encode(
            pixmap.data(),
            COVER_W as u16,
            COVER_H as u16,
            jpeg_encoder::ColorType::Rgba,
        )
        .map_err(|e| io::Error::other(format!("jpeg encode: {e}")))?;
    Ok(out)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_width_distinguishes_cjk_and_latin() {
        let w_cjk = estimate_width("青空文庫", 100.0);
        let w_ascii = estimate_width("Aozora", 100.0);
        // 4 CJK chars * 100 = 400. 6 ASCII chars * 55 = 330.
        assert_eq!(w_cjk, 400.0);
        assert_eq!(w_ascii, 100.0 * 0.55 * 6.0);
    }

    #[test]
    fn split_lines_balances_segments() {
        let split = split_lines("黒死館殺人事件", 2, 100.0);
        // 7 chars / 2 = ceil 4 → segments of 4 + 3.
        assert_eq!(split.lines.len(), 2);
        assert_eq!(split.lines[0].chars().count(), 4);
        assert_eq!(split.lines[1].chars().count(), 3);
        // Widest line is 4 CJK chars = 400px.
        assert_eq!(split.max_w, 400.0);
    }

    #[test]
    fn build_cover_svg_produces_well_formed_svg() {
        let svg = build_cover_svg("テスト本", "著者名");
        assert!(svg.starts_with("<svg"));
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains(r#"width="1050""#));
        assert!(svg.contains(r#"height="1500""#));
        assert!(svg.contains("テスト本"));
        assert!(svg.contains("著者名"));
        assert!(svg.contains("青空文庫"));
    }

    #[test]
    fn long_title_splits_to_two_lines() {
        // ~16 CJK chars at 100em wouldn't fit (1600 > inner ~810).
        let svg = build_cover_svg("これは少し長めの本のタイトルです", "著者");
        // Multi-line render emits multiple <text font-size="..."> tags
        // for the title; just confirm we kept all the chars somewhere.
        for c in "これは少し長めの本のタイトルです".chars() {
            assert!(svg.contains(c.to_string().as_str()), "char {} missing", c);
        }
    }

    #[test]
    fn rasterize_to_jpeg_produces_jpeg_magic() {
        let bytes = render_cover_jpeg("テスト", "著者").expect("render");
        assert!(
            bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
            "JPEG SOI magic missing"
        );
        // EOI marker at end.
        assert_eq!(&bytes[bytes.len() - 2..], &[0xFF, 0xD9]);
        // Reasonable size: 1050x1500 q85 JPEG should be >5KB, <500KB.
        assert!(
            bytes.len() > 5_000,
            "JPEG suspiciously small: {} bytes",
            bytes.len()
        );
        assert!(
            bytes.len() < 500_000,
            "JPEG suspiciously large: {} bytes",
            bytes.len()
        );
    }
}
