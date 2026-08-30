//! Replace the cover image inside an existing KFX.

use std::path::Path;

use anyhow::{Context, Result};

use crate::library::import::{sha256_of_bytes, write_bytes_atomic};

/// Replace the cover inside `kfx_path` with `new_image` (JPEG/PNG/WebP — bokai
pub fn replace_cover(kfx_path: &Path, new_image: &[u8]) -> Result<String> {
    let kfx_bytes =
        std::fs::read(kfx_path).with_context(|| format!("read {}", kfx_path.display()))?;
    let patched = bokai::formats::kfx::cover_replace::replace_cover(&kfx_bytes, new_image)
        .map_err(|e| anyhow::anyhow!("bokai kfx cover replace: {e:?}"))?;
    write_bytes_atomic(kfx_path, &patched)?;
    Ok(sha256_of_bytes(&patched))
}

/// Rebuild the KFX at `kfx_path` by re-converting `epub_path` (which must
/// already declare a cover). Used to give a cover to a cover-less bokai-produced
/// KFX (an EPUB import whose source had no cover): [`super::epub_cover::insert_cover`]
pub fn reconvert_from_epub(
    epub_path: &Path,
    kfx_path: &Path,
    metadata_override: impl FnOnce(&bokai::Metadata) -> bokai::Metadata,
) -> Result<String> {
    let mut book =
        bokai::Book::open(epub_path).with_context(|| format!("open {}", epub_path.display()))?;
    book.set_metadata_override(metadata_override(book.metadata()));
    let mut buf = std::io::Cursor::new(Vec::new());
    book.export(bokai::Format::Kfx, &mut buf)
        .with_context(|| "export epub→kfx for cover insert")?;
    let bytes = buf.into_inner();
    write_bytes_atomic(kfx_path, &bytes)?;
    Ok(sha256_of_bytes(&bytes))
}
