//! Decode-path fuzz target: arbitrary bytes through the full consumer
//! pipeline — container parse, primary + separate-alpha codestream decode,
//! pixel-buffer packing, orientation transform. Any panic, abort, OOM, or
//! timeout is a finding; errors are the expected outcome for garbage.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(container) = jxr::decode::container::parse(data) else {
        return;
    };
    let Ok(mut img) = jxr::decode::decode_image(&container) else {
        return;
    };
    let _ = img.to_pixel_buffer();
    jxr::decode::apply_orientation(&mut img, container.orientation);
});
