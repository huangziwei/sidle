//! Decode-path fuzz target: arbitrary bytes through the full consumer

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
