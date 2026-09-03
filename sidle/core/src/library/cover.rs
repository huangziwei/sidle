//! Putting a cover everywhere a book keeps one.

use std::path::Path;

use rusqlite::Connection;

use crate::library::db::{self, BookRow};
use crate::library::paths::LibraryPaths;
use crate::library::{convert, cover_fetch, epub_cover, kfx_cover, thumbnail};

/// What happened to one book's cover.
#[derive(Debug)]
pub enum Outcome {
    /// A new cover was written; the path is the sidecar.
    Updated {
        cover_path: String,
    },
    /// The book has no catalogue ASIN to fetch from (refetch only).
    NoAsin,
    Failed {
        error: String,
    },
}

/// Re-fetch one book's colour cover from Amazon and install it.
pub fn refetch(conn: &Connection, paths: &LibraryPaths, book: &BookRow) -> Outcome {
    let Some(asin) = book.amazon_asin.as_deref() else {
        return Outcome::NoAsin;
    };
    // A stored value that isn't catalogue-shaped can't resolve to a real
    // `/images/P/` cover, so "no ASIN" is more honest than "failed".
    if !cover_fetch::looks_like_real_amazon_asin(asin) {
        return Outcome::NoAsin;
    }
    let Some(bytes) = cover_fetch::fetch_color_cover(asin, &book.language) else {
        return Outcome::Failed {
            error: "no cover returned (404, placeholder, or network error \
                    — see [sidle/cover-fetch] log lines)"
                .into(),
        };
    };
    install(conn, paths, book, &bytes, "jpg", "refetch")
}

/// Install a cover from an image file the caller picked. The format is sniffed
/// from the bytes, so a `.png` mislabeled `.jpg` still lands correctly.
pub fn set_from_file(
    conn: &Connection,
    paths: &LibraryPaths,
    book: &BookRow,
    src: &Path,
) -> Outcome {
    let bytes = match std::fs::read(src) {
        Ok(b) => b,
        Err(e) => {
            return Outcome::Failed {
                error: format!("read {}: {e}", src.display()),
            };
        }
    };
    let Some(ext) = sniff_image_format(&bytes) else {
        return Outcome::Failed {
            error: "unsupported image format (expected JPG, PNG, or WebP)".into(),
        };
    };
    install(conn, paths, book, &bytes, ext, "set-cover")
}

/// Write `bytes` as this book's cover: sidecar, thumbnail, EPUB, KFX.
///
/// `tag` names the caller in the log lines the best-effort steps emit.
pub fn install(
    conn: &Connection,
    paths: &LibraryPaths,
    book: &BookRow,
    bytes: &[u8],
    ext: &str,
    tag: &str,
) -> Outcome {
    let out = paths.cover(&book.sha256, ext);
    if let Err(e) = std::fs::write(&out, bytes) {
        return Outcome::Failed {
            error: format!("write {}: {e}", out.display()),
        };
    }
    let out_str = out.to_string_lossy().to_string();
    let _ = db::set_cover_path(conn, book.id, &out_str);
    // Refresh the picker thumbnail to match. Best-effort (see
    // `library::thumbnail`).
    let _ = thumbnail::ensure_thumbnail(paths, &book.sha256, &out);
    // A previous cover at a different filename (`cover.png` replaced by
    // `cover.jpg`) would otherwise stay on disk beside the new one.
    if let Some(old) = book.cover_path.as_deref()
        && old != out_str.as_str()
    {
        let _ = std::fs::remove_file(old);
    }

    // Into the EPUB, so external readers see it too. `ensure_cover` regenerates
    // the EPUB from the KFX when the EPUB is the derived side and has no cover
    // slot, else inserts one.
    if let Some(epub) = book.epub_path.as_deref()
        && let Err(e) = epub_cover::ensure_cover(
            Path::new(epub),
            book.kfx_path.as_deref().map(Path::new),
            bytes,
            ext,
            book.kind.as_deref() == Some("kfx_to_epub"),
        )
    {
        eprintln!(
            "[sidle/{tag}] book {} epub cover swap failed: {e:#}",
            book.id
        );
    }

    // And into the KFX — that's the copy pushed to the Kindle, and its embedded
    if let Some(kfx) = book.kfx_path.as_deref()
        && let Some(new_sha) = swap_or_insert_kfx_cover(book, kfx, bytes, tag)
    {
        let _ = db::set_kfx_path_and_sha(conn, book.id, kfx, &new_sha);
    }

    Outcome::Updated {
        cover_path: out_str,
    }
}

/// Embed `bytes` as the KFX cover, returning the new sha256 (for `kfx_sha256`)
pub fn swap_or_insert_kfx_cover(
    book: &BookRow,
    kfx: &str,
    bytes: &[u8],
    tag: &str,
) -> Option<String> {
    let kfx_path = Path::new(kfx);
    match kfx_cover::replace_cover(kfx_path, bytes) {
        Ok(sha) => Some(sha),
        Err(e) => {
            // The KFX may be rebuilt from its EPUB only when the EPUB is the SOURCE
            // (`kind == "epub_to_kfx"`): a KFX-sourced book's KFX is authoritative.
            let epub_is_source = book.kind.as_deref() == Some("epub_to_kfx");
            let Some(epub) = book.epub_path.as_deref().filter(|_| epub_is_source) else {
                eprintln!(
                    "[sidle/{tag}] book {} kfx cover swap failed: {e:#}",
                    book.id
                );
                return None;
            };
            match kfx_cover::reconvert_from_epub(Path::new(epub), kfx_path, |src| {
                convert::book_metadata_override(src, book)
            }) {
                Ok(sha) => {
                    eprintln!(
                        "[sidle/{tag}] book {} kfx was coverless; cover inserted via reconvert",
                        book.id
                    );
                    Some(sha)
                }
                Err(e2) => {
                    eprintln!(
                        "[sidle/{tag}] book {} kfx cover swap failed ({e:#}); \
                         reconvert failed: {e2:#}",
                        book.id
                    );
                    None
                }
            }
        }
    }
}

/// Magic-byte sniff for the three image formats sidle accepts as covers.
/// Returns the canonical lowercase extension, or `None` if no header matches.
pub fn sniff_image_format(bytes: &[u8]) -> Option<&'static str> {
    // JPEG: FF D8 FF
    if bytes.len() >= 3 && bytes[0..3] == [0xFF, 0xD8, 0xFF] {
        return Some("jpg");
    }
    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if bytes.len() >= 8 && bytes[0..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("png");
    }
    // WebP: "RIFF" .... "WEBP"
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_formats_are_read_from_the_bytes() {
        // JPEG SOI + APP0 (the typical EXIF/JFIF prefix).
        assert_eq!(
            sniff_image_format(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]),
            Some("jpg")
        );
        assert_eq!(sniff_image_format(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(
            sniff_image_format(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0]),
            Some("png")
        );
        assert_eq!(sniff_image_format(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(sniff_image_format(b"GIF89a"), None);
        assert_eq!(sniff_image_format(b"PK\x03\x04"), None); // ZIP
        assert_eq!(sniff_image_format(&[0xFF, 0xD8]), None); // too short to be sure
        assert_eq!(sniff_image_format(&[]), None);
    }
}
