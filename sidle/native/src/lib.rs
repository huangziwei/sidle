//! Library facade exposing the pure-logic modules so `cargo test` runs
//! on the host. The full binary is Linux-only — `eink/fb.rs` and
//! `eink/touch.rs` use `libc::ioctl` whose signature differs on macOS
//! (BSD takes `c_ulong`, Linux takes `c_int`). Anything that depends on
//! the framebuffer or touch driver lives in `main.rs` only; anything
//! pure (parsing, HTTP shape, filesystem scans) is re-declared here so
//! the test runner can build it without dragging in the device modules.

pub mod api;
pub mod config;
pub mod cover_cache;
pub mod device_state;
pub mod wrap;
