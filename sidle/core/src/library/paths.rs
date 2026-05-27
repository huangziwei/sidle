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

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct LibraryPaths {
    pub root: PathBuf,
}

/// On-disk pointer to the library data root, stored as `config.json` in the
/// fixed app-local state dir. An absent file or absent field means "use the
/// default root" (the state dir itself).
#[derive(Debug, Default, Serialize, Deserialize)]
struct LibraryConfig {
    library_root: Option<String>,
}

impl LibraryPaths {
    /// Resolve the default library root: `<data_dir>/sidle`.
    pub fn default_root() -> anyhow::Result<Self> {
        let base = dirs::data_dir()
            .ok_or_else(|| anyhow::anyhow!("could not resolve user data directory"))?;
        Ok(Self { root: base.join("sidle") })
    }

    /// The fixed app-local state dir, `<data_dir>/sidle` — never moves with the
    /// library. Holds `config.json` (the root pointer), and is also the library
    /// root the app falls back to when no pointer is set.
    pub fn state_dir() -> anyhow::Result<PathBuf> {
        let base = dirs::data_dir()
            .ok_or_else(|| anyhow::anyhow!("could not resolve user data directory"))?;
        Ok(base.join("sidle"))
    }

    /// Path to the root-pointer config in the app-local state dir.
    pub fn config_path() -> anyhow::Result<PathBuf> {
        Ok(Self::state_dir()?.join("config.json"))
    }

    /// Resolve the active library root: the `config.json` pointer if set, else
    /// the default (the state dir). Errors if the config is present but
    /// malformed, or names a root that doesn't currently exist (e.g. an
    /// unplugged external drive, or a hand-edited bad path) — failing loudly
    /// beats silently opening the empty default library in the wrong place.
    ///
    /// Used by `bootstrap` (Tauri app) and the LAN server's default branch, so
    /// both agree on a relocated library.
    pub fn resolve() -> anyhow::Result<Self> {
        Self::resolve_in(&Self::state_dir()?)
    }

    /// Point the library root at `new` and persist it in `config.json`. Does
    /// NOT move any files — the relocate flow (§6) copies data to `new` first,
    /// then calls this, then relaunches.
    pub fn set_root(new: &Path) -> anyhow::Result<()> {
        Self::set_root_in(&Self::state_dir()?, new)
    }

    /// [`resolve`](Self::resolve) against an explicit state dir — the testable
    /// core, so tests never touch the real `~/Library/Application Support`.
    fn resolve_in(state_dir: &Path) -> anyhow::Result<Self> {
        let cfg_path = state_dir.join("config.json");
        let bytes = match std::fs::read(&cfg_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self { root: state_dir.to_path_buf() });
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("read {}", cfg_path.display())));
            }
        };
        let cfg: LibraryConfig = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", cfg_path.display()))?;
        match cfg.library_root {
            Some(root) => {
                let root = PathBuf::from(root);
                if !root.is_dir() {
                    anyhow::bail!(
                        "configured library root {} does not exist — reconnect the drive, \
                         or remove {} to revert to the default library",
                        root.display(),
                        cfg_path.display(),
                    );
                }
                Ok(Self { root })
            }
            None => Ok(Self { root: state_dir.to_path_buf() }),
        }
    }

    /// [`set_root`](Self::set_root) against an explicit state dir (testable core).
    fn set_root_in(state_dir: &Path, new: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("create state dir {}", state_dir.display()))?;
        let cfg = LibraryConfig {
            library_root: Some(new.to_string_lossy().into_owned()),
        };
        let json = serde_json::to_vec_pretty(&cfg).context("serialize config.json")?;
        let cfg_path = state_dir.join("config.json");
        std::fs::write(&cfg_path, json)
            .with_context(|| format!("write {}", cfg_path.display()))?;
        Ok(())
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

    /// Thumbnail sidecar: the small grayscale JPEG derived from the cover at
    /// import time and served to the Kindle picker (`/cover/{id}?thumb=1`).
    /// Always `.jpg` regardless of the source cover's extension — the
    /// thumbnail is re-encoded, so its format is fixed. See
    /// [`crate::library::thumbnail`].
    pub fn cover_thumb(&self, sha: &str) -> PathBuf {
        self.book_dir(sha).join("cover.thumb.jpg")
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

/// Length of the sha256 prefix used as the on-device filename infix
/// (`<basename>.<sha8>.kfx`). 8 hex chars = 32 bits — collision-free for
/// any realistic personal library (50% chance at ~93k books per the
/// birthday bound) and short enough to stay readable.
pub const SHA_INFIX_LEN: usize = 8;

/// First [`SHA_INFIX_LEN`] hex chars of a KFX sha256.
///
/// Panics if `kfx_sha256.len() < SHA_INFIX_LEN`. The caller is responsible
/// for ensuring this — every code path either reads from `books.kfx_sha256`
/// (a full sha256, 64 hex chars) or short-circuits when the column is
/// `NULL`.
pub fn sha_infix(kfx_sha256: &str) -> &str {
    &kfx_sha256[..SHA_INFIX_LEN]
}

/// Build the canonical on-device basename for a KFX: `<stem>.<sha8>.kfx`.
///
/// `kfx_path` is the on-disk file in the local library (under
/// `books/<sha>/`); we take its file_stem so the device-side name mirrors
/// the Mac-side name. Falls back to `book-<sha8>` if the path has no
/// usable stem (shouldn't happen for library-managed files; defense in
/// depth).
///
/// Used by both the USB push (`device::push::push_one`) and the LAN
/// server's Content-Disposition header so the same file shows up under
/// the same name regardless of how it got onto the Kindle.
pub fn kfx_device_filename(kfx_path: &str, kfx_sha256: &str) -> String {
    let stem = Path::new(kfx_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("book-{}", sha_infix(kfx_sha256)));
    format!("{stem}.{}.kfx", sha_infix(kfx_sha256))
}

/// Parse the `<sha8>` out of an on-device filename matching the
/// `<basename>.<sha8>.kfx` shape. Returns `None` for anything else —
/// used both as a "is this ours?" gate (push/delete) and to look the
/// matching library row back up (`device_list_ours`).
pub fn parse_sha_infix(filename: &str) -> Option<String> {
    let stem = filename.strip_suffix(".kfx")?;
    let (_, sha) = stem.rsplit_once('.')?;
    if sha.len() == SHA_INFIX_LEN && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(sha.to_string())
    } else {
        None
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

    const SAMPLE_SHA: &str = "deadbeefcafef00d1234567890abcdefdeadbeefcafef00d1234567890abcdef";

    #[test]
    fn sha_infix_returns_first_8() {
        assert_eq!(sha_infix(SAMPLE_SHA), "deadbeef");
    }

    #[test]
    fn kfx_device_filename_uses_stem_and_sha() {
        let path = "/Users/me/Library/Application Support/sidle/books/abc/[Author] Title (2024).kfx";
        assert_eq!(
            kfx_device_filename(path, SAMPLE_SHA),
            "[Author] Title (2024).deadbeef.kfx"
        );
    }

    #[test]
    fn kfx_device_filename_falls_back_when_no_stem() {
        assert_eq!(kfx_device_filename("", SAMPLE_SHA), "book-deadbeef.deadbeef.kfx");
    }

    #[test]
    fn parse_sha_infix_round_trips_canonical_name() {
        let path = "/x/[A] T (2020).kfx";
        let name = kfx_device_filename(path, SAMPLE_SHA);
        assert_eq!(parse_sha_infix(&name).as_deref(), Some("deadbeef"));
    }

    #[test]
    fn parse_sha_infix_rejects_non_kfx() {
        assert_eq!(parse_sha_infix("foo.deadbeef.epub"), None);
    }

    #[test]
    fn parse_sha_infix_rejects_no_infix() {
        assert_eq!(parse_sha_infix("just-a-file.kfx"), None);
    }

    #[test]
    fn parse_sha_infix_rejects_wrong_length() {
        // 7 hex chars — too short
        assert_eq!(parse_sha_infix("foo.deadbee.kfx"), None);
        // 9 hex chars — too long
        assert_eq!(parse_sha_infix("foo.deadbeef0.kfx"), None);
    }

    #[test]
    fn parse_sha_infix_rejects_non_hex() {
        assert_eq!(parse_sha_infix("foo.deadbeeZ.kfx"), None);
    }

    #[test]
    fn parse_sha_infix_handles_basename_with_dots() {
        // basename can contain dots ("v1.2"); only the LAST `.`-separated
        // segment before `.kfx` is the sha.
        let name = "[A] Series v1.2 (2024).deadbeef.kfx";
        assert_eq!(parse_sha_infix(name).as_deref(), Some("deadbeef"));
    }

    // §4b root pointer. Exercised against an explicit state dir so the real
    // `~/Library/Application Support/sidle/config.json` is never touched.

    #[test]
    fn resolve_defaults_to_state_dir_when_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let p = LibraryPaths::resolve_in(dir.path()).expect("resolve");
        assert_eq!(p.root.as_path(), dir.path());
    }

    #[test]
    fn set_root_then_resolve_returns_the_pointer() {
        let state = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap(); // the relocated root — must exist
        LibraryPaths::set_root_in(state.path(), lib.path()).expect("set_root");
        let p = LibraryPaths::resolve_in(state.path()).expect("resolve");
        assert_eq!(p.root.as_path(), lib.path());
    }

    #[test]
    fn resolve_errors_on_missing_configured_root() {
        let state = tempfile::tempdir().unwrap();
        let missing = state.path().join("not-mounted");
        LibraryPaths::set_root_in(state.path(), &missing).expect("set_root");
        assert!(LibraryPaths::resolve_in(state.path()).is_err());
    }

    #[test]
    fn resolve_errors_on_malformed_config() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(state.path().join("config.json"), b"{ not valid json").unwrap();
        assert!(LibraryPaths::resolve_in(state.path()).is_err());
    }
}
