//! Failure modes for reading and editing a KFX container.

use std::io;

/// Error type of the KFX format modules.
#[derive(Debug)]
pub enum KfxError {
    /// Not a KFX container, or one declaring no cover, no TOC, or an
    /// out-of-bounds index table.
    InvalidKfx(String),
    /// Non-zero `bcDRMScheme`: the entity payloads are encrypted.
    Encrypted(i64),
    /// Non-zero `bcComprType`: the entity payloads are compressed.
    Compressed(i64),
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
            KfxError::Encrypted(scheme) => {
                write!(f, "KFX container is encrypted (bcDRMScheme {scheme})")
            }
            KfxError::Compressed(kind) => {
                write!(f, "KFX container is compressed (bcComprType {kind})")
            }
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
