//! Malformed-input hardening for the decoder.
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
//! That property belongs to the decode fuzzer, which runs with
//! `-Coverflow-checks=off` to match the shipped binary; asserting it here
//! would test the wrong configuration.

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
malformed_fixture!(
    alloc_bomb_huge_dims,
    "alloc_bomb_huge_dims.jxr",
    "malformed"
);

// Explicit tile columns summing past the macroblock grid — the tile-rest
// `checked_sub` guard (would otherwise wrap to a huge usize tile size).
malformed_fixture!(
    tile_columns_exceed_grid,
    "tile_columns_exceed_grid.jxr",
    "malformed"
);

// Stream truncated within the container/header — the bit reader reports
// insufficient data instead of reading past the buffer.
malformed_fixture!(truncated_header, "truncated_header.jxr", "container-err");

// Per-MB QP-set selector decoded out of range (index ≥ num_qps) — would index
// past the quant-scaling table at `scaling_factor`. Found by the Phase-3
// certification fuzz run (1 h × 8 workers); `decode_qp_index` now bounds it.
malformed_fixture!(
    qp_index_out_of_range,
    "qp_index_out_of_range.jxr",
    "malformed"
);

// Degenerate windowing: 679123969×1 px with margins that don't pad to the MB
// grid → mb_height truncates to 0, zeroing the decode-budget product while the
// MB-grid build still allocated one column Vec per mb_width (~1 GB of empty
// Vec headers from a 702-byte stream) before the first tile startcode check.
// Found by the Phase-7 certification fuzz run (slow-unit report); the
// extended-size whole-macroblock guard now rejects it at the image header.
malformed_fixture!(
    zero_mb_rows_column_bomb,
    "zero_mb_rows_column_bomb.jxr",
    "malformed"
);
