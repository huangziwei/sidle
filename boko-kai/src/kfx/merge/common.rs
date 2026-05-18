//! Helpers shared between the mechanical and fast merge paths.

/// Generate a calibre-style `CR!` container id. Used when no source container
/// supplies a stable id and no asset_id is recoverable from `$490` metadata.
pub fn generate_container_id() -> String {
    let mut state: u128 = {
        #[cfg(target_arch = "wasm32")]
        {
            (js_sys::Date::now() as u128) * 1_000_000
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        }
    };
    let chars: Vec<char> = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ".chars().collect();
    let mut id = String::from("CR!");
    for _ in 0..28 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let idx = ((state >> 56) as usize) % chars.len();
        id.push(chars[idx]);
    }
    id
}
