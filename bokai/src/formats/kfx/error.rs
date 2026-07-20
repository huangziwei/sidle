//! Failure modes for reading and editing a KFX container.
//!
//! Covers the whole format-side surface: parsing a container ([`super::loader`]),
//! rewriting one ([`super::container_edit`], [`super::metadata_edit`],
//! [`super::cover_replace`], [`super::toc_repair`]), and decoding the images
//! inside it ([`super::image_extract`], [`super::cover_extract`]).

use std::io;

/// What can go wrong between "here are some bytes" and "here is usable KFX".
#[derive(Debug)]
pub enum KfxError {
    /// The bytes are not a KFX container, or the structure it declares is not
    /// one we can act on (no cover, no TOC, index table out of bounds, …).
    InvalidKfx(String),
    /// A bundled JPEG-XR image could not be decoded.
    JxrDecode(String),
    /// A decoded image could not be re-encoded as JPEG.
    JpegEncode(String),
    Io(io::Error),
}

impl std::fmt::Display for KfxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KfxError::InvalidKfx(m) => write!(f, "invalid KFX: {m}"),
            KfxError::JxrDecode(m) => write!(f, "JXR decode failed: {m}"),
            KfxError::JpegEncode(m) => write!(f, "JPEG encode failed: {m}"),
            KfxError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for KfxError {}

impl From<io::Error> for KfxError {
    fn from(e: io::Error) -> Self {
        KfxError::Io(e)
    }
}
