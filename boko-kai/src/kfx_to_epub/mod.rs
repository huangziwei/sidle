//! Mechanical port of calibre's KFX → EPUB pipeline.
//!
//! This is a parallel path from boko's generic `KfxImporter` + `EpubExporter`.
//! Where the IR pipeline loses KFX-specific signal (ruby, writing-mode, ppd,
//! anchor offsets, image bytes), this module preserves it by mirroring
//! calibre's `yj_to_epub_*` translator closely enough to be a correctness
//! reference, then improving on the defects calibre exhibits against the
//! source KFX.
//!
//! See `.claude/plans/kfx-to-epub-port.md` for the phase plan.
//!
//! ## Phase 1 status
//!
//! - Step 1 (resources): in progress — image bundling, JXR transcode, cover.
//! - Steps 2-5: stubs / not started.

pub mod loader;
pub mod output;
pub mod resources;

pub mod jxr;

use std::io;

pub use loader::BookData;
pub use output::EpubOutput;

/// Failure modes for the mechanical port.
#[derive(Debug)]
pub enum ConvertError {
    /// KFX container couldn't be parsed (bad header, missing index, etc.).
    InvalidKfx(String),
    /// JPEG-XR image couldn't be decoded.
    JxrDecode(String),
    /// JPEG re-encode of a decoded raster failed.
    JpegEncode(String),
    /// I/O failure writing the EPUB.
    Io(io::Error),
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConvertError::InvalidKfx(m) => write!(f, "invalid KFX: {m}"),
            ConvertError::JxrDecode(m) => write!(f, "JXR decode failed: {m}"),
            ConvertError::JpegEncode(m) => write!(f, "JPEG encode failed: {m}"),
            ConvertError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for ConvertError {}

impl From<io::Error> for ConvertError {
    fn from(e: io::Error) -> Self {
        ConvertError::Io(e)
    }
}

/// Convert a KFX container in memory to a complete EPUB byte stream.
///
/// This is the main entry point for the mechanical port. Mirrors calibre's
/// `KFX_EPUB(book).decompile_to_epub()` orchestration.
pub fn convert_to_epub(kfx_bytes: &[u8]) -> Result<Vec<u8>, ConvertError> {
    let book = loader::load(kfx_bytes)?;
    let mut out = output::EpubOutput::new();

    // Phase 1 step 1: resources (images + raw media + cover).
    resources::process(&book, &mut out)?;

    // Phase 1 step 1 scaffolding: emit one placeholder chapter per
    // bundled image so `<img src>` references exist for the validator.
    // Steps 2-4 will replace this with the real content pipeline.
    resources::emit_image_scaffold_chapters(&mut out);

    out.finalize(&book.metadata).map_err(ConvertError::Io)
}
