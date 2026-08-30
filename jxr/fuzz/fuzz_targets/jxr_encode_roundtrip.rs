//! Encoder fuzz target — a ROUND-TRIP ORACLE, not just no-panic: draw a

#![no_main]

#[path = "common.rs"]
mod common;

use arbitrary::Unstructured;
use common::{Expect, draw_valid};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Ok(case) = draw_valid(&mut u) else { return };
    let input = jxr::TypedInput {
        width: case.width,
        height: case.height,
        samples: case.planes.as_samples(),
        premultiplied_alpha: case.premultiplied,
    };
    let bytes = match jxr::encode_typed(&input, case.mode, case.opts.clone()) {
        Ok(b) => b,
        Err(e) => panic!(
            "documented-valid case rejected: {e} (mode {:?}, {}x{}, opts {:?})",
            case.mode, case.width, case.height, case.opts
        ),
    };

    // Container + headers.
    let c = jxr::decode::container::parse(&bytes).expect("our container must parse");
    assert_eq!((c.image_width, c.image_height), (case.width, case.height));
    let mut d = jxr::decode::decoder::Decoder::new(c.image_data);
    let s = d.parse_headers().expect("our headers must parse");
    assert_eq!(
        (s.width, s.height),
        (case.width, case.height),
        "header dims"
    );
    assert_eq!(s.frequency_mode, case.opts.frequency, "frequency mode");
    assert_eq!(
        s.overlap_mode,
        overlap_code(case.opts.overlap),
        "overlap mode"
    );
    assert_eq!(
        s.tile_cols,
        case.opts.tile_cols.max(1) as usize,
        "tile cols"
    );
    assert_eq!(
        s.tile_rows,
        case.opts.tile_rows.max(1) as usize,
        "tile rows"
    );
    assert_eq!(
        (s.margins.0, s.margins.1),
        (case.opts.window_top as u32, case.opts.window_left as u32),
        "window margins"
    );
    let np = case.planes.num_planes();
    let multi = matches!(
        case.mode,
        jxr::ColorMode::Cmyk | jxr::ColorMode::CmykDirect | jxr::ColorMode::NComponent
    );
    // N-component never has alpha (the plane count IS the channel count);
    // only CMYK's 5th plane is an alpha image plane.
    let has_alpha = (np == 4 && !multi && !matches!(case.planes, common::Planes::Rgbe(_)))
        || (matches!(case.mode, jxr::ColorMode::Cmyk | jxr::ColorMode::CmykDirect) && np == 5);
    assert_eq!(
        s.alpha_image_plane, has_alpha,
        "alpha image plane flag (mode {:?}, np {np}, {}x{}, opts {:?})",
        case.mode, case.width, case.height, case.opts
    );
    assert_eq!(
        s.planes[0].bands_present,
        bands_code(case.opts.bands),
        "bands_present"
    );
    assert_eq!(s.planes[0].scaled, case.opts.scaled, "scaled flag");
    if let Some(fmt) = case.int_fmt {
        assert_eq!(s.planes[0].internal_clr_fmt, fmt, "internal color format");
    }

    // Full decode — our own valid file must never panic or error.
    let img = jxr::decode::decode_image(&c).unwrap_or_else(|e| {
        panic!(
            "our own file failed to decode: {e} (mode {:?}, opts {:?})",
            case.mode, case.opts
        )
    });
    assert_eq!((img.width, img.height), (case.width, case.height));
    // The pixel-buffer packer must never panic either (Err is fine: packed
    // formats and some exotic layouts are documented self-serve).
    let _ = img.to_pixel_buffer();

    // Pixel contract.
    let n = (case.width * case.height) as usize;
    let cmp_planes = if case.auto_gray { 1 } else { np };
    match case.expect {
        Expect::DecodeOnly => {}
        Expect::F32Idem => {
            // Decoded patterns are on the representable set by construction:
            // re-encoding them must round-trip bit-exactly.
            let redo: Vec<Vec<u32>> = img
                .image_plane
                .iter()
                .map(|p| p.iter().map(|&v| v as u32).collect())
                .collect();
            let n_dec = redo.len();
            let again = jxr::encode_typed(
                &jxr::TypedInput {
                    width: case.width,
                    height: case.height,
                    samples: jxr::SamplePlanes::F32(&redo),
                    premultiplied_alpha: case.premultiplied && n_dec == 4,
                },
                if n_dec == 1 {
                    jxr::ColorMode::Grayscale
                } else {
                    jxr::ColorMode::Color
                },
                case.opts.clone(),
            )
            .expect("re-encode of decoded F32 must be accepted");
            let c2 = jxr::decode::container::parse(&again).unwrap();
            let img2 = jxr::decode::decode_image(&c2).expect("re-encoded F32 must decode");
            // The decoder can emit -0.0 (a flushed tiny negative keeps its
            let zfold = |v: i32| if v & 0x7fff_ffff == 0 { 0 } else { v };
            for (ch, plane2) in img2.image_plane.iter().enumerate() {
                for (i, (&a, &b)) in plane2.iter().zip(&img.image_plane[ch]).enumerate() {
                    assert_eq!(
                        zfold(a),
                        zfold(b),
                        "F32 idempotence broke at px{i} ch{ch}: {a:#x} vs {b:#x} \
                         (mode {:?}, opts {:?})",
                        case.mode,
                        case.opts
                    );
                }
            }
        }
        Expect::Exact | Expect::Bounded(_) => {
            let bound = match case.expect {
                Expect::Bounded(b) => b,
                _ => 0,
            };
            assert!(
                img.image_plane.len() >= cmp_planes,
                "decoded {} planes, input had {cmp_planes}",
                img.image_plane.len()
            );
            for ch in 0..cmp_planes {
                for i in 0..n {
                    let Some(want) = case.planes.expected(ch, i) else {
                        continue;
                    };
                    let got = img.image_plane[ch][i] as i64;
                    assert!(
                        (got - want).abs() <= bound,
                        "px{i} ch{ch}: got {got}, want {want} (±{bound}; mode {:?}, opts {:?})",
                        case.mode,
                        case.opts
                    );
                }
            }
        }
    }
});

fn overlap_code(o: jxr::Overlap) -> u8 {
    match o {
        jxr::Overlap::None => 0,
        jxr::Overlap::One => 1,
        jxr::Overlap::Two => 2,
    }
}

fn bands_code(b: jxr::BandsPresent) -> u8 {
    use jxr::decode::consts::{ALL_BANDS, DCONLY, NOFLEXBITS, NOHIGHPASS};
    match b {
        jxr::BandsPresent::All => ALL_BANDS,
        jxr::BandsPresent::NoFlexbits => NOFLEXBITS,
        jxr::BandsPresent::NoHighpass => NOHIGHPASS,
        jxr::BandsPresent::DcOnly => DCONLY,
    }
}
