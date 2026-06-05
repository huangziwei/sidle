//! All raster-image transforms in one place, shared across conversion
//! directions (EPUB→KFX and KFX→EPUB).
//!
//! - [`jpeg`] — JPEG sanitize/strip/transcode for KFX bundling (EPUB→KFX).
//! - [`jxr_decode`] — pure-Rust JPEG-XR decoder (KFX→EPUB).
//!
//! Planned (see `.claude/plans/jxr-encoder.md`):
//! - `jxr_encode` — pure-Rust JPEG-XR encoder (EPUB→KFX), dual grayscale/color.
//! - `jxr_common` — transform/table/state primitives shared by decode + encode,
//!   extracted from `jxr_decode` as the encoder needs them.

pub mod jpeg;
pub mod jxr_decode;
