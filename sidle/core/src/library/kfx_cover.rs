//! Replace the cover image inside an existing KFX.
//!
//! KFX-side parallel of [`super::epub_cover`]. The cover-fetch flow fetches the
//! true cover by ASIN; for the EPUB we swap it via `epub_cover`, and here we
//! swap it into the imported KFX too — so the copy we push to the Kindle shows
//! the real cover. A store KFX can ship the *wrong* cover: the publisher may
//! drop a house-logo placeholder into `cover_image` — こちらあみ子 carries a
//! 1200×1600 "筑摩eBOOKS" logo there rather than the book art. The home tile /
//! sleep-screen renders whatever that embedded resource holds.
//!
//! The container surgery lives in bokai (`kfx::cover_replace`); this layer is
//! just file I/O. Rewriting the KFX changes its bytes, so the caller must
//! persist the returned sha256 as `kfx_sha256` — that hash is the on-device
//! filename infix (`push`), so the row and any future push stay linked.

use std::path::Path;

use anyhow::{Context, Result};

use crate::library::import::{sha256_of_bytes, write_bytes_atomic};

/// Replace the cover inside `kfx_path` with `new_image` (JPEG/PNG/WebP — bokai
/// normalizes to a sleep-screen-safe JFIF JPEG). Rewrites the file in place
/// (temp + atomic rename) and returns the sha256 of the new bytes for the
/// caller to store as `kfx_sha256`.
///
/// Errors if the KFX has no `cover_image` to replace, or the rewrite fails.
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
/// adds the cover to the EPUB, then this reconverts so the KFX gains a real
/// cover section via bokai's proven EPUB→KFX exporter — cheaper in risk than
/// splicing a cover resource into an existing container.
///
/// `metadata_override` receives the EPUB's own metadata and returns the values
/// to bake into the KFX, so a normal conversion's edited-metadata handling
/// (title/author/… from the library row) is preserved through the reconvert.
/// Returns the new sha256 for `kfx_sha256`.
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
