//! Encoder no-panic fuzz target: arbitrary — frequently INVALID — plane

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
