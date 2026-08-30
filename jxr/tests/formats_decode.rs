//! Guards for the deep/exotic decode formats: 16-bit gray (BD16 + SHIFT_BITS

use jxr::decode::{container, decode_image};

fn check(jxr_bytes: &[u8], raw: &[u8], expect_nc: usize, expect_fmt: &str) {
    let parsed = container::parse(jxr_bytes).expect("container parse");
    assert_eq!(parsed.format, expect_fmt);
    let img = decode_image(&parsed).expect("decode");
    assert_eq!((img.width, img.height), (48, 32));
    assert_eq!(img.num_components, expect_nc);

    let n = 48usize * 32;
    assert_eq!(raw.len(), n * expect_nc * 4);
    for i in 0..n {
        for c in 0..expect_nc {
            let off = (i * expect_nc + c) * 4;
            let expected = i32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
            assert_eq!(img.image_plane[c][i], expected, "sample {i} component {c}");
        }
    }
}

#[test]
fn decodes_libjxr_gray16_exactly() {
    check(
        include_bytes!("fixtures/fmt_gray16.jxr"),
        include_bytes!("fixtures/fmt_gray16.raw"),
        1,
        "16bppGray",
    );
}

#[test]
fn decodes_libjxr_rgba64_half_with_separate_alpha_exactly() {
    check(
        include_bytes!("fixtures/fmt_rgba64_half.jxr"),
        include_bytes!("fixtures/fmt_rgba64_half.raw"),
        4,
        "64bppRGBAHalf",
    );
}

#[test]
fn decodes_libjxr_rgbe_exactly() {
    check(
        include_bytes!("fixtures/fmt_rgbe.jxr"),
        include_bytes!("fixtures/fmt_rgbe.raw"),
        4,
        "32bppRGBE",
    );
}
