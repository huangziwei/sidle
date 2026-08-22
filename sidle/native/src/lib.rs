//! The host-buildable half of the picker.
//!
//! `eink/fb.rs` and `eink/touch.rs` pass `libc::ioctl` a `c_ulong` on BSD and a
//! `c_int` on Linux, and stay declared in `main.rs` alone. The modules below
//! parse, shape HTTP, and scan the filesystem.

pub mod api;
pub mod collate;
pub mod config;
pub mod cover_cache;
pub mod dedrm;
pub mod device_state;
pub mod discover;
pub mod font;
pub mod handwriting;
pub mod readinglog;
pub mod receipt;
pub mod running;
pub mod selfupdate;
pub mod series;
pub mod updates;
pub mod wrap;
