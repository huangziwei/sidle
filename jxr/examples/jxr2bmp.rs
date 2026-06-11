//! Best-effort `.jxr` → 24-bit BMP viewer:
//!
//! ```text
//! cargo run --release --example jxr2bmp -- input.jxr [output.bmp] [--tonemap]
//! ```
//!
//! This is a *viewer*, not a color-managed converter — it exists so a decoded
//! image can be looked at without writing pixel-packing boilerplate. Display
//! mapping policy (the part that is policy, which is why it lives in an
//! example and not in the library API):
//!
//! - `U8` / `U16` samples are treated as display-encoded and pass through
//!   (`U16` takes the high byte).
//! - `F16` / `F32` samples are treated as linear scene-referred (scRGB-style)
//!   values: negatives clamp to 0, then either a straight clamp to 1.0 or —
//!   with `--tonemap` — a luminance Reinhard curve (white point at the 99.9th
//!   luminance percentile) compresses HDR highlights, then the sRGB transfer
//!   encodes for display.
//! - Gray replicates to RGB; straight alpha composites over black
//!   (premultiplied alpha already is over black, so it just drops).
//! - Fixed-point (`I16`/`I32`), packed 5-6-5, CMYK, RGBE and N-channel
//!   layouts are rejected: each needs an interpretation choice this viewer
//!   refuses to make silently.

use jxr::decode::pixels::{AlphaMode, ColorModel, PixelBuffer, SampleType};
use jxr::decode::{container, decode_image};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let tonemap = if let Some(i) = args.iter().position(|a| a == "--tonemap") {
        args.remove(i);
        true
    } else {
        false
    };
    if args.is_empty() || args.len() > 2 {
        eprintln!("usage: jxr2bmp <input.jxr> [output.bmp] [--tonemap]");
        std::process::exit(2);
    }
    let input = &args[0];
    let output = args.get(1).cloned().unwrap_or_else(|| {
        let p = std::path::Path::new(input);
        p.with_extension("bmp").to_string_lossy().into_owned()
    });

    let bytes = std::fs::read(input).unwrap_or_else(|e| {
        eprintln!("{input}: {e}");
        std::process::exit(1);
    });
    let parsed = container::parse(&bytes).unwrap_or_else(|e| {
        eprintln!("{input}: container parse failed: {e:?}");
        std::process::exit(1);
    });
    let img = decode_image(&parsed).unwrap_or_else(|e| {
        eprintln!("{input}: decode failed: {e:?}");
        std::process::exit(1);
    });
    let buf = img.to_pixel_buffer().unwrap_or_else(|e| {
        eprintln!("{input}: pixel buffer failed: {e:?}");
        std::process::exit(1);
    });
    println!(
        "{input}: {}×{} ch={} color={:?} alpha={:?} sample={:?}",
        buf.width, buf.height, buf.channels, buf.color, buf.alpha, buf.sample
    );

    let rgb = to_display_rgb(&buf, tonemap).unwrap_or_else(|msg| {
        eprintln!("{input}: {msg}");
        std::process::exit(1);
    });
    write_bmp(&output, buf.width as usize, buf.height as usize, &rgb);
    println!(
        "wrote {output} ({})",
        if tonemap { "Reinhard tonemap + sRGB" } else { "clamp + per-type encoding" }
    );
}

/// Map the decoded buffer to display-encoded RGB bytes, per the policy in the
/// module doc. `Err` carries a human-readable refusal.
fn to_display_rgb(buf: &PixelBuffer, tonemap: bool) -> Result<Vec<[u8; 3]>, String> {
    let color_ch: usize = match buf.color {
        ColorModel::Gray => 1,
        ColorModel::Rgb => 3,
        other => return Err(format!("{other:?} layout needs an interpretation choice this viewer doesn't make — convert explicitly")),
    };
    let n = buf.width as usize * buf.height as usize;
    let nch = buf.channels as usize;

    // Pull a sample as f32 in its native value domain.
    let sample = |i: usize| -> f32 {
        match buf.sample {
            SampleType::U8 => buf.data[i] as f32 / 255.0,
            SampleType::U16 => {
                u16::from_le_bytes([buf.data[i * 2], buf.data[i * 2 + 1]]) as f32 / 65535.0
            }
            SampleType::F16 => {
                half_to_f32(u16::from_le_bytes([buf.data[i * 2], buf.data[i * 2 + 1]]))
            }
            SampleType::F32 => f32::from_le_bytes(
                buf.data[i * 4..i * 4 + 4].try_into().unwrap(),
            ),
            _ => unreachable!("rejected above"),
        }
    };
    let float_input = matches!(buf.sample, SampleType::F16 | SampleType::F32);
    if !float_input && !matches!(buf.sample, SampleType::U8 | SampleType::U16) {
        return Err(format!(
            "{:?} samples are fixed-point/packed — their display scaling is format policy this viewer doesn't guess",
            buf.sample
        ));
    }

    // Gather pixels (gray replicated), alpha composited over black. Integer
    // samples are display-encoded so the multiply is an approximation there;
    // float samples composite in linear, which is the correct order.
    let mut px = vec![[0.0f32; 3]; n];
    for p in 0..n {
        let base = p * nch;
        let mut v = [0.0f32; 3];
        for c in 0..3 {
            v[c] = sample(base + if color_ch == 1 { 0 } else { c });
        }
        if nch > color_ch && buf.alpha == AlphaMode::Straight {
            let a = sample(base + color_ch).clamp(0.0, 1.0);
            v = [v[0] * a, v[1] * a, v[2] * a];
        }
        px[p] = v;
    }

    if float_input {
        if tonemap {
            // Reinhard on luminance, white point at the 99.9th percentile.
            let mut lums: Vec<f32> = px
                .iter()
                .step_by(16)
                .map(|p| 0.2126 * p[0].max(0.0) + 0.7152 * p[1].max(0.0) + 0.0722 * p[2].max(0.0))
                .collect();
            lums.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let lwhite = lums[((lums.len() - 1) as f64 * 0.999) as usize].max(1.0);
            let lw2 = lwhite * lwhite;
            for p in px.iter_mut() {
                let l = 0.2126 * p[0].max(0.0) + 0.7152 * p[1].max(0.0) + 0.0722 * p[2].max(0.0);
                if l > 0.0 {
                    let s = (l * (1.0 + l / lw2) / (1.0 + l)) / l;
                    *p = [p[0] * s, p[1] * s, p[2] * s];
                } else {
                    *p = [0.0; 3];
                }
            }
        }
        Ok(px
            .iter()
            .map(|p| [encode8(srgb_encode(p[0])), encode8(srgb_encode(p[1])), encode8(srgb_encode(p[2]))])
            .collect())
    } else {
        // Already display-encoded: no transfer, just quantize.
        Ok(px.iter().map(|p| [encode8(p[0]), encode8(p[1]), encode8(p[2])]).collect())
    }
}

fn encode8(c: f32) -> u8 {
    (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

fn srgb_encode(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// IEEE 754 half bit pattern → f32 (the decoder hands halfs through as bits).
fn half_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let exp = (h >> 10) & 0x1F;
    let man = (h & 0x3FF) as f32;
    match exp {
        0 => sign * man * (-24f32).exp2(),
        31 => {
            if man == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        e => sign * (1.0 + man / 1024.0) * ((e as f32) - 15.0).exp2(),
    }
}

fn write_bmp(path: &str, w: usize, h: usize, rgb: &[[u8; 3]]) {
    let row_bytes = (w * 3 + 3) & !3;
    let data_size = row_bytes * h;
    let mut out = Vec::with_capacity(54 + data_size);
    out.extend(*b"BM");
    out.extend((54u32 + data_size as u32).to_le_bytes());
    out.extend([0u8; 4]);
    out.extend(54u32.to_le_bytes());
    out.extend(40u32.to_le_bytes());
    out.extend((w as i32).to_le_bytes());
    out.extend((h as i32).to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(24u16.to_le_bytes());
    out.extend([0u8; 8]); // BI_RGB, biSizeImage 0
    out.extend(2835i32.to_le_bytes());
    out.extend(2835i32.to_le_bytes());
    out.extend([0u8; 8]);
    for y in (0..h).rev() {
        let start = out.len();
        for x in 0..w {
            let p = rgb[y * w + x];
            out.extend([p[2], p[1], p[0]]);
        }
        out.resize(start + row_bytes, 0);
    }
    std::fs::write(path, out).unwrap_or_else(|e| {
        eprintln!("{path}: write failed: {e}");
        std::process::exit(1);
    });
}
