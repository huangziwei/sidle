//! Raster-image transforms shared across conversion directions (EPUB→KFX and
//! KFX→EPUB).
//!
//! - [`jpeg`] — JPEG sanitize/strip/transcode for KFX bundling (EPUB→KFX).
//! - [`jxr_transcode`] — KFX→EPUB glue: JXR decode → JPEG re-encode. This is
//!   pipeline glue (it depends on `ConvertError` / `jpeg_encoder`), not part
//!   of the codec.
//!
//! The JPEG-XR codec itself lives in the standalone, zero-dependency
//! top-level [`jxr`] crate (re-exported as `boko::jxr`).

pub mod jpeg;
pub mod jxr_transcode;
