//! Raster-image transforms shared across conversion directions (EPUB→KFX and
//! KFX→EPUB).

pub mod jpeg;
pub mod media_type;
pub mod svg;

pub use media_type::{ImageFormat, media_type_of};
