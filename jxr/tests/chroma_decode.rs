//! Guards for YUV 4:2:0 / 4:2:2 decode reconstruction: joint-coded chroma
//! entropy, chroma transforms + overlap, upsampling.
//!
//! Both fixtures are **libjxr-minted** (JxrEncApp, q16 lossy, frequency order,
//! overlap 1 — encoder defaults): real external producers. The expected `.rgb`
//! blobs (interleaved RGB, row-major) are libjxr's own output for these files,
//! pixel-exact against JxrDecApp. Decoding a lossy file is deterministic, so
//! exact equality — not a tolerance — is the correct assertion.
//!
//! The 4:2:2 fixture is also what holds `UpdateModelMB` to Table 116's
//! `iWeight2`: get that weight wrong and this file desyncs the bitstream.

use jxr::decode::{container, decoder::Decoder};

fn check(jxr_bytes: &[u8], expected_rgb: &[u8]) {
    let parsed = container::parse(jxr_bytes).expect("container parse");
    let img = Decoder::new(parsed.image_data).decode().expect("decode");
    assert_eq!((img.width, img.height), (48, 32));
    assert_eq!(img.num_components, 3, "expected RGB output");

    let n = 48usize * 32;
    assert_eq!(expected_rgb.len(), n * 3);
    for i in 0..n {
        for c in 0..3 {
            assert_eq!(
                img.image_plane[c][i].clamp(0, 255) as u8,
                expected_rgb[i * 3 + c],
                "pixel {} component {c} mismatch",
                i
            );
        }
    }
}

#[test]
fn decodes_libjxr_yuv420_q16_exactly() {
    check(
        include_bytes!("fixtures/c420_48x32_q16.jxr"),
        include_bytes!("fixtures/c420_48x32_q16.rgb"),
    );
}

#[test]
fn decodes_libjxr_yuv422_q16_exactly() {
    check(
        include_bytes!("fixtures/c422_48x32_q16.jxr"),
        include_bytes!("fixtures/c422_48x32_q16.rgb"),
    );
}
