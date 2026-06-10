//! Malformed-input hardening regression (Phase 3).
//!
//! These fixtures are real fuzz-found byte sequences that exercise the
//! decoder's *structural* guards: a lying header that would otherwise allocate
//! gigabytes, a tile layout that overruns the macroblock grid, and a stream
//! that ends mid-header. Each must resolve to a clean error — never a panic,
//! OOM, or hang — and does so in **every** build configuration (these guards
//! fire before any reconstruction arithmetic), so they are valid under the
//! overflow-checks-on `cargo test` build, in the workspace and in the lifted
//! standalone copy alike.
//!
//! Scope note: integer-overflow-on-garbage in the reconstruction stage is NOT
//! covered here. Such inputs wrap (the decoder's shipping/release semantics —
//! garbage pixels, never a panic), which only holds with overflow checks off.
//! That property is verified by the decode fuzzer, which runs with
//! `-Coverflow-checks=off` to match the shipped binary (see
//! scripts/jxr-fuzz-overnight.sh); asserting it here would test the wrong
//! configuration. The full artifact corpus replays clean under those semantics.

use jxr::decode::decoder::DecodeError;
use jxr::decode::{container, decode_image};

/// Run the full consumer pipeline and report how the input resolved. Reaching
/// the return at all means no panic occurred.
fn classify(bytes: &[u8]) -> &'static str {
    let c = match container::parse(bytes) {
        Ok(c) => c,
        Err(_) => return "container-err",
    };
    let mut img = match decode_image(&c) {
        Ok(i) => i,
        Err(DecodeError::Malformed(_)) => return "malformed",
        Err(DecodeError::Unsupported(_)) => return "unsupported",
        Err(DecodeError::BadSignature(_)) => return "bad-signature",
        Err(DecodeError::Bits(_)) => return "bits",
    };
    // Exercise the rest of the consumer surface too (these allocate from the
    // now-validated geometry and must also stay panic-free).
    let _ = img.to_pixel_buffer();
    jxr::decode::apply_orientation(&mut img, c.orientation);
    "ok"
}

macro_rules! malformed_fixture {
    ($name:ident, $file:literal, $expect:literal) => {
        #[test]
        fn $name() {
            let bytes = include_bytes!(concat!("fixtures/malformed/", $file));
            assert_eq!(
                classify(bytes),
                $expect,
                "{} must resolve cleanly as `{}` (no panic/OOM/hang)",
                $file,
                $expect
            );
        }
    };
}

// Lying dimensions: a 43-byte stream declaring a 126977×53480449-macroblock
// image — the decompression-bomb guard rejects it before allocating.
malformed_fixture!(alloc_bomb_huge_dims, "alloc_bomb_huge_dims.jxr", "malformed");

// Explicit tile columns summing past the macroblock grid — the tile-rest
// `checked_sub` guard (would otherwise wrap to a huge usize tile size).
malformed_fixture!(tile_columns_exceed_grid, "tile_columns_exceed_grid.jxr", "malformed");

// Stream truncated within the container/header — the bit reader reports
// insufficient data instead of reading past the buffer.
malformed_fixture!(truncated_header, "truncated_header.jxr", "container-err");
