//! All raster-image transforms in one place, shared across conversion
//! directions (EPUB→KFX and KFX→EPUB).
//!
//! - [`jpeg`] — JPEG sanitize/strip/transcode for KFX bundling (EPUB→KFX).
//! - [`jxr_decode`] — pure-Rust JPEG-XR decoder (KFX→EPUB).
//! - [`jxr_encode`] — pure-Rust JPEG-XR encoder (EPUB→KFX), grayscale/color.
//! - [`jxr_transcode`] — KFX→EPUB glue: `jxr_decode` → JPEG re-encode. Kept
//!   separate from the codec (it depends on `ConvertError` / `jpeg_encoder`),
//!   so `jxr_decode` + `jxr_encode` stay extraction-ready.

pub mod jpeg;
pub mod jxr_decode;
pub mod jxr_encode;
pub mod jxr_transcode;
