//! Regression guards for YUV 4:2:0 / 4:2:2 decode reconstruction (joint-coded
//! chroma entropy, chroma transforms + overlap, upsampling) — the paths added
//! in Phase 2 of the general-codec plan.
//!
//! Both fixtures are **libjxr-minted** (JxrEncApp, q16 lossy, frequency order,
//! overlap 1 — encoder defaults): real external producers. The expected `.rgb`
//! blobs (interleaved RGB, row-major) were captured from a decode that was
//! verified **pixel-exact against JxrDecApp** across a 110-case oracle matrix
//! (scripts/jxr-oracle, 2026-06-09) — i.e., they are libjxr's own output for
//! these files. Decoding a lossy file is deterministic, so exact equality is
//! the correct assertion.
//!
//! The 4:2:2 fixture additionally locks in the `UpdateModelMB` chroma-weight
//! fix (Table 116 `iWeight2`): before it, this exact file desynced the
//! bitstream (`decode_run 13 not in 1..=11`).

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
