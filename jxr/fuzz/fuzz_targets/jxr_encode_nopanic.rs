//! Encoder no-panic fuzz target: arbitrary — frequently INVALID — plane
//! shapes and option values. The encoder must return `Ok` or `Err`, never
//! panic; and anything it accepts must decode (an encoder that emits
//! undecodable bytes on weird-but-accepted input is a bug).
//!
//! Like `jxr_encode_roundtrip`, run with overflow checks ON (the cargo-fuzz
//! default): encode-side arithmetic processes caller-typed data, where any
//! overflow is a real bug worth knowing about.

#![no_main]

#[path = "common.rs"]
mod common;

use arbitrary::Unstructured;
use common::draw_raw;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Ok(case) = draw_raw(&mut u) else { return };
    let input = jxr::TypedInput {
        width: case.width,
        height: case.height,
        samples: case.planes.as_samples(),
        premultiplied_alpha: case.premultiplied,
    };
    // Any panic here is the finding; Err is the expected outcome for the
    // invalid majority.
    let Ok(bytes) = jxr::encode_typed(&input, case.mode, case.opts) else {
        return;
    };
    // Accepted ⇒ must be a well-formed file end-to-end.
    let c = jxr::decode::container::parse(&bytes).expect("accepted input: container must parse");
    let img = jxr::decode::decode_image(&c).expect("accepted input: file must decode");
    assert_eq!((img.width, img.height), (case.width, case.height));
    let _ = img.to_pixel_buffer();
});
