//! Regression guard for the JPEG-XR **color** decode path (INT_YUV444 →
//! OUT_RGB), which is load-bearing for the Sidle reader once color KFX exist.
//!
//! `color444_16x16_lossless.jxr` is a real **libjxr**-minted lossless 4:4:4
//! color JXR (BSD reference encoder), produced from the deterministic image
//! `expected_rgb()` below — a true external consumer, not a self-loop.
//! Lossless 4:4:4 ⇒ the decode must be **bit-exact**.
//!
//! This locks in the fix to `decoder.rs`'s YUV444 plane-header read (it must
//! consume the *two* 4-bit reserved fields = 8 bits; reading 4 desynced the
//! whole codestream). The fixture is committed; this test needs only that.

use jxr::decode::{container, decoder::Decoder};

/// The exact RGB image the fixture was encoded from: per-channel gradients +
/// saturated corner swatches. `out[(y*16+x)*3 + {0,1,2}]` = R,G,B.
fn expected_rgb() -> Vec<u8> {
    let (w, h) = (16usize, 16usize);
    let mut v = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 3;
            v[i] = (x * 255 / w) as u8;
            v[i + 1] = (y * 255 / h) as u8;
            v[i + 2] = ((x + y) * 255 / (w + h)) as u8;
        }
    }
    let put = |v: &mut [u8], x0: usize, y0: usize, c: [u8; 3]| {
        for dy in 0..4 {
            for dx in 0..4 {
                let i = ((y0 + dy) * w + (x0 + dx)) * 3;
                v[i..i + 3].copy_from_slice(&c);
            }
        }
    };
    put(&mut v, 0, 0, [255, 0, 0]);
    put(&mut v, w - 4, 0, [0, 255, 0]);
    put(&mut v, 0, h - 4, [0, 0, 255]);
    put(&mut v, w - 4, h - 4, [255, 255, 255]);
    v
}

#[test]
fn decodes_libjxr_color444_exactly() {
    let bytes = include_bytes!("fixtures/color444_16x16_lossless.jxr");
    let parsed = container::parse(bytes).expect("container parse");
    let img = Decoder::new(parsed.image_data).decode().expect("decode");

    assert_eq!((img.width, img.height), (16, 16));
    assert_eq!(img.num_components, 3, "expected RGB");
    assert_eq!(img.output_clr_fmt, 7, "OUT_RGB");

    let expected = expected_rgb();
    for y in 0..16usize {
        for x in 0..16usize {
            let di = y * 16 + x;
            let si = di * 3;
            let got = [
                img.image_plane[0][di],
                img.image_plane[1][di],
                img.image_plane[2][di],
            ];
            assert_eq!(
                got,
                [
                    expected[si] as i32,
                    expected[si + 1] as i32,
                    expected[si + 2] as i32
                ],
                "pixel ({x},{y}) mismatch (R,G,B)"
            );
        }
    }
}
