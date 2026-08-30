//! Regression guard for the JPEG-XR **grayscale** decode path (YONLY), in
//! **frequency order** — libjxr's default bitstream layout, which the
//! spatial-order `color444` fixture doesn't exercise.

use jxr::decode::{container, decoder::Decoder};

/// The exact gray image the fixture was encoded from: two-axis gradient +
/// corner swatches + one LCG noise row at mid-height.
fn expected_gray() -> Vec<u8> {
    let (w, h) = (16usize, 16usize);
    let mut v = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            v[y * w + x] = ((x * 255 / w) + (y * 255 / h) / 2).min(255) as u8;
        }
    }
    let sw = 4;
    let mut put = |x0: usize, y0: usize, val: u8| {
        for dy in 0..sw {
            for dx in 0..sw {
                v[(y0 + dy) * w + (x0 + dx)] = val;
            }
        }
    };
    put(0, 0, 255);
    put(w - sw, 0, 0);
    put(0, h - sw, 64);
    put(w - sw, h - sw, 192);
    let mut s: u32 = (w as u32) * 31 + h as u32;
    let y = h / 2;
    for x in 0..w {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        v[y * w + x] = (s >> 24) as u8;
    }
    v
}

#[test]
fn decodes_libjxr_gray8_frequency_order_exactly() {
    let bytes = include_bytes!("fixtures/gray8_16x16_lossless.jxr");
    let parsed = container::parse(bytes).expect("container parse");
    let img = Decoder::new(parsed.image_data).decode().expect("decode");

    assert_eq!((img.width, img.height), (16, 16));
    assert_eq!(img.num_components, 1, "expected YONLY");

    let expected = expected_gray();
    for y in 0..16usize {
        for x in 0..16usize {
            let i = y * 16 + x;
            assert_eq!(
                img.image_plane[0][i], expected[i] as i32,
                "pixel ({x},{y}) mismatch"
            );
        }
    }
}
