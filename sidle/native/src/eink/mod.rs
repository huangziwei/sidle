//! On-device display + input plumbing.
//!
//! Display goes through a real X11 window (`fb.rs`, via the pure-Rust `x11rb`)

pub mod buttons;
pub mod fb;
pub mod input;
pub mod screenshot;
pub mod touch;
pub mod x11poc;
pub mod xprobe;
