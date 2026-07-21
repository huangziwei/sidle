//! Raster-image transforms shared across conversion directions (EPUB→KFX and
//! KFX→EPUB).
//!
//! - [`jpeg`] — JPEG sanitize/strip/transcode for KFX bundling (EPUB→KFX).
//! - [`svg`] — SVG → white-flattened raster for KFX bundling (EPUB→KFX);
//!   KFX has no vector resource format. Also hosts the process-wide
//!   system-font database shared with the Aozora cover generator.
//! - [`jxr_transcode`] — KFX→EPUB glue: JXR decode → JPEG re-encode. This is
//!   pipeline glue (it depends on `ConvertError` / `jpeg_encoder`), not part
//!   of the codec.
//! - [`media_type`] — what a payload's leading bytes say it is.
//!
//! The JPEG-XR codec itself lives in the standalone, zero-dependency
//! top-level [`jxr`] crate (re-exported as `bokai::jxr`).

pub mod jpeg;
pub mod jxr_transcode;
pub mod media_type;
pub mod svg;

pub use media_type::{ImageFormat, media_type_of};
