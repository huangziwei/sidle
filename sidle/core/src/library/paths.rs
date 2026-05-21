//! Library folder layout.
//!
//! ```text
//! ~/Library/Application Support/sidle/
//! ├── library.db
//! └── books/<sha>/
//!     ├── [Author] Title (Year).epub
//!     ├── [Author] Title (Year).kfx
//!     └── cover.jpg
//! ```

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct LibraryPaths {
    pub root: PathBuf,
}

impl LibraryPaths {
    /// Resolve the default library root: `<data_dir>/sidle`.
    pub fn default_root() -> anyhow::Result<Self> {
        let base = dirs::data_dir()
            .ok_or_else(|| anyhow::anyhow!("could not resolve user data directory"))?;
        Ok(Self { root: base.join("sidle") })
    }

    pub fn db(&self) -> PathBuf {
        self.root.join("library.db")
    }

    /// Per-book directory: holds the EPUB, KFX, and cover for one sha.
    pub fn book_dir(&self, sha: &str) -> PathBuf {
        self.root.join("books").join(sha)
    }

    pub fn cover(&self, sha: &str, ext: &str) -> PathBuf {
        self.book_dir(sha).join(format!("cover.{ext}"))
    }

    /// Ensure base subdirectories exist.
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(self.root.join("books"))?;
        Ok(())
    }

    pub fn ensure_sha(&self, sha: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(self.book_dir(sha))
    }

    /// Remove the per-sha directory. Surfaces IO errors so callers can roll
    /// back the matching `books` row delete — a silent swallow here was the
    /// source of orphan `books/<sha>/` dirs left after `library_remove`
    /// (Spotlight/Quicklook/Books.app holding a handle on the EPUB returned
    /// EBUSY, the error went nowhere, the row was already gone).
    ///
    /// Treats `NotFound` as success: a re-run after a partial failure should
    /// be a no-op rather than an error.
    pub fn remove_sha(&self, sha: &str) -> std::io::Result<()> {
        match std::fs::remove_dir_all(self.book_dir(sha)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Map a media type or filename to an image extension we want on disk.
pub fn cover_ext_from(media_or_path: &str) -> &'static str {
    let lower = media_or_path.to_ascii_lowercase();
    if lower.contains("png") {
        "png"
    } else if lower.contains("gif") {
        "gif"
    } else if lower.contains("webp") {
        "webp"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") || lower.contains("jpeg") {
        "jpg"
    } else if let Some(ext) = Path::new(&lower).extension().and_then(|e| e.to_str()) {
        match ext {
            "png" => "png",
            "gif" => "gif",
            "webp" => "webp",
            _ => "jpg",
        }
    } else {
        "jpg"
    }
}

/// Build a filesystem-safe basename in the form `[Author] Title (Year)`.
///
/// - Drops the `[Author] ` prefix if no author.
/// - Drops the ` (Year)` suffix if no year can be extracted.
/// - Falls back to `Untitled` for an empty title.
/// - Strips characters that Finder/macOS reject and collapses whitespace.
/// - Truncates to ~180 chars to stay well under HFS+ filename limits.
pub fn format_basename(authors: &[String], title: &str, date: Option<&str>) -> String {
    let title = sanitize_segment(title);
    let title = if title.is_empty() {
        "Untitled".to_string()
    } else {
        title
    };

    let author = authors.first().map(|a| sanitize_segment(a)).filter(|s| !s.is_empty());
    let year = date.and_then(extract_year);

    let mut out = String::new();
    if let Some(a) = author {
        out.push('[');
        out.push_str(&a);
        out.push_str("] ");
    }
    out.push_str(&title);
    if let Some(y) = year {
        out.push_str(" (");
        out.push_str(&y);
        out.push(')');
    }

    truncate_chars(&out, 180)
}

fn sanitize_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => out.push('_'),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_year(date: &str) -> Option<String> {
    let bytes = date.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        let window = &bytes[i..i + 4];
        if window.iter().all(|b| b.is_ascii_digit()) {
            // Require the 4-digit run to be exactly 4 digits — not part of a
            // longer number like `20230412` (don't want "2023" out of that;
            // an explicit ISO date like `2023-04-12` parses fine because of
            // the dash separator).
            let prev_digit = i > 0 && bytes[i - 1].is_ascii_digit();
            let next_digit = i + 4 < bytes.len() && bytes[i + 4].is_ascii_digit();
            if !prev_digit && !next_digit {
                let s = std::str::from_utf8(window).ok()?.to_string();
                let year: u32 = s.parse().ok()?;
                if (1000..=9999).contains(&year) {
                    return Some(s);
                }
            }
        }
        i += 1;
    }
    None
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_full() {
        let s = format_basename(&["乙 一".into()], "ＧＯＴＨ (角川文庫)", Some("2005-09-25"));
        assert_eq!(s, "[乙 一] ＧＯＴＨ (角川文庫) (2005)");
    }

    #[test]
    fn basename_no_author() {
        let s = format_basename(&[], "天国旅行", Some("2010"));
        assert_eq!(s, "天国旅行 (2010)");
    }

    #[test]
    fn basename_no_year() {
        let s = format_basename(&["木原 浩勝".into()], "新耳袋", None);
        assert_eq!(s, "[木原 浩勝] 新耳袋");
    }

    #[test]
    fn basename_sanitizes_path_separators() {
        let s = format_basename(&["A/B\\C".into()], "Title: With/Slashes?", Some("2020"));
        assert_eq!(s, "[A_B_C] Title_ With_Slashes_ (2020)");
    }

    #[test]
    fn extract_year_finds_first_4digit() {
        assert_eq!(extract_year("2023-04-12"), Some("2023".into()));
        assert_eq!(extract_year("April 2023"), Some("2023".into()));
        assert_eq!(extract_year("12345"), None); // too many digits → not isolated
        assert_eq!(extract_year("no year here"), None);
    }
}
