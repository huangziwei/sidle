//! Replace the cover image inside an existing KFX.
//!
//! KFX-side parallel of [`super::epub_cover`]. The cover-fetch flow fetches the
//! true cover by ASIN; for the EPUB we swap it via `epub_cover`, and here we
//! swap it into the imported KFX too — so the copy we push to the Kindle shows
//! the real cover. A store KFX can ship the *wrong* cover: the publisher may
//! drop a house-logo placeholder into `cover_image` (verified on こちらあみ子,
//! whose KFX cover was a 1200×1600 "筑摩eBOOKS" logo, not the book art). The
//! home tile / sleep-screen renders whatever that embedded resource holds.
//!
//! The container surgery lives in boko (`kfx::cover_replace`); this layer is
//! just file I/O. Rewriting the KFX changes its bytes, so the caller must
//! persist the returned sha256 as `kfx_sha256` — that hash is the on-device
//! filename infix (`push`), so the row and any future push stay linked.

use std::path::Path;

use anyhow::{Context, Result};

use crate::library::import::{sha256_of_bytes, write_bytes_atomic};

/// Replace the cover inside `kfx_path` with `new_image` (JPEG/PNG/WebP — boko
/// normalizes to a sleep-screen-safe JFIF JPEG). Rewrites the file in place
/// (temp + atomic rename) and returns the sha256 of the new bytes for the
/// caller to store as `kfx_sha256`.
///
/// Errors if the KFX has no `cover_image` to replace, or the rewrite fails.
pub fn replace_cover(kfx_path: &Path, new_image: &[u8]) -> Result<String> {
    let kfx_bytes =
        std::fs::read(kfx_path).with_context(|| format!("read {}", kfx_path.display()))?;
    let patched = boko::kfx::cover_replace::replace_cover(&kfx_bytes, new_image)
        .map_err(|e| anyhow::anyhow!("boko kfx cover replace: {e:?}"))?;
    write_bytes_atomic(kfx_path, &patched)?;
    Ok(sha256_of_bytes(&patched))
}
