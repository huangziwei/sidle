//! Library folder layout.
//!
//! ```text
//! ~/Library/Application Support/Sidle/
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
    /// Resolve the default library root: `<data_dir>/Sidle`.
    pub fn default_root() -> anyhow::Result<Self> {
        let base = dirs::data_dir()
            .ok_or_else(|| anyhow::anyhow!("could not resolve user data directory"))?;
        Ok(Self {
            root: base.join("Sidle"),
        })
    }

    /// The fixed app-local state dir, `<data_dir>/Sidle`, unmoved by a library
    /// relocate. Holds `config.json`, and is the library root an unset pointer
    /// resolves to. [`resolve`](Self::resolve) renames a lowercase `sidle`.
    pub fn state_dir() -> anyhow::Result<PathBuf> {
        let base = dirs::data_dir()
            .ok_or_else(|| anyhow::anyhow!("could not resolve user data directory"))?;
        Ok(base.join("Sidle"))
    }

    /// Path to the root-pointer config in the app-local state dir.
    pub fn config_path() -> anyhow::Result<PathBuf> {
        Ok(Self::state_dir()?.join("config.json"))
    }

    /// The active library root: the `config.json` pointer if set, else the
    /// state dir. `Err` for a malformed config, and for a pointer naming a root
    /// that does not exist — an unplugged external drive, a hand-edited path.
    ///
    /// The entry point for `bootstrap` and the LAN server's default branch.
    /// Renames a lowercase `sidle` app-support dir first.
    pub fn resolve() -> anyhow::Result<Self> {
        Self::migrate_legacy_state_dir();
        Self::resolve_in(&Self::state_dir()?)
    }

    /// Point the library root at `new` and persist it in `config.json`. Moves
    /// no files: the relocate flow copies data to `new`, calls this, relaunches.
    pub fn set_root(new: &Path) -> anyhow::Result<()> {
        Self::set_root_in(&Self::state_dir()?, new)
    }

    /// [`resolve`](Self::resolve) against an explicit `state_dir`.
    fn resolve_in(state_dir: &Path) -> anyhow::Result<Self> {
        let cfg_path = state_dir.join("config.json");
        let bytes = match std::fs::read(&cfg_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    root: state_dir.to_path_buf(),
                });
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
            None => Ok(Self {
                root: state_dir.to_path_buf(),
            }),
        }
    }

    /// [`set_root`](Self::set_root) against an explicit `state_dir`.
    fn set_root_in(state_dir: &Path, new: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("create state dir {}", state_dir.display()))?;
        let cfg = LibraryConfig {
            library_root: Some(new.to_string_lossy().into_owned()),
        };
        let json = serde_json::to_vec_pretty(&cfg).context("serialize config.json")?;
        let cfg_path = state_dir.join("config.json");
        std::fs::write(&cfg_path, json).with_context(|| format!("write {}", cfg_path.display()))?;
        Ok(())
    }

    /// Rename a lowercase `sidle` app-support dir to `Sidle`. Book paths are
    /// stored root-relative, and the library resolves under either name. A
    /// case-only rename on case-insensitive APFS, a move elsewhere.
    ///
    /// Best-effort and idempotent: a no-op against a stored `Sidle`, and a
    /// failure leaves the dir where it is.
    fn migrate_legacy_state_dir() {
        if let Some(base) = dirs::data_dir() {
            Self::migrate_legacy_state_dir_in(&base);
        }
    }

    fn migrate_legacy_state_dir_in(base: &Path) {
        // The stored case, which a case-insensitive FS preserves, separates
        // `sidle` from `Sidle`.
        let (mut has_lower, mut has_proper) = (false, false);
        if let Ok(entries) = std::fs::read_dir(base) {
            for e in entries.flatten() {
                match e.file_name().to_str() {
                    Some("sidle") => has_lower = true,
                    Some("Sidle") => has_proper = true,
                    _ => {}
                }
            }
        }
        if has_lower && !has_proper {
            let (old, new) = (base.join("sidle"), base.join("Sidle"));
            if let Err(e) = std::fs::rename(&old, &new) {
                eprintln!(
                    "[sidle/paths] couldn't rename legacy {} -> {}: {e}",
                    old.display(),
                    new.display()
                );
            }
        }
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

    /// Cached per-page anchor geometry for a PDF-backed KFX (eid→page map, page
    /// boxes), keyed inside the file by the book's `kfx_sha256`. A derived-asset
    /// sidecar; see [`crate::library::pdf_geom`].
    pub fn pdf_geom(&self, sha: &str) -> PathBuf {
        self.book_dir(sha).join("pdf_geom.json")
    }

    /// Thumbnail sidecar: the small color JPEG derived from the cover at import
    /// time and served to the Kindle picker (`/cover/{id}?thumb=1`). Re-encoded
    /// to `.jpg` whatever the source cover's extension. See
    /// [`crate::library::thumbnail`].
    pub fn cover_thumb(&self, sha: &str) -> PathBuf {
        self.book_dir(sha).join("cover.thumb.jpg")
    }

    /// Library-wide marker recording the thumbnail format version the boot
    /// backfill last produced. Lets a format change (e.g. grayscale → color)
    /// force a one-time rebuild of every `cover.thumb.jpg`. See
    /// [`crate::library::thumbnail::THUMB_FORMAT_VERSION`].
    pub fn cover_thumb_format(&self) -> PathBuf {
        self.root.join("cover-thumb.fmt")
    }

    /// Staging directory for the on-device app's self-update bundle
    /// (`bin/sidle` and `manifest.json`), which `sidle-server` serves over
    /// `/device/...`. Keyed off the active library root, as [`db`](Self::db) is.
    pub fn device_dist(&self) -> PathBuf {
        self.root.join("device-dist")
    }

    /// TLS material for the LAN server: the private CA and the server leaf it
    /// signs. Keyed off the active library root, as
    /// [`device_dist`](Self::device_dist) is.
    pub fn tls_dir(&self) -> PathBuf {
        self.root.join("tls")
    }

    /// The private CA certificate, and the one file here that also travels to
    /// the device as `etc/ca.pem`, the picker's *sole* trust root.
    pub fn ca_cert(&self) -> PathBuf {
        self.tls_dir().join("ca.pem")
    }

    /// The CA's private key. Never leaves this machine; 0600.
    pub fn ca_key(&self) -> PathBuf {
        self.tls_dir().join("ca-key.pem")
    }

    /// The server leaf, re-issued whenever the addresses it must cover change.
    pub fn server_cert(&self) -> PathBuf {
        self.tls_dir().join("server.pem")
    }

    /// The server leaf's private key; 0600.
    pub fn server_key(&self) -> PathBuf {
        self.tls_dir().join("server-key.pem")
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

    /// Remove the per-sha directory. An IO error surfaces to the caller, which
    /// rolls the matching `books` row delete back. `NotFound` is success, and a
    /// re-run after a partial failure is a no-op.
    pub fn remove_sha(&self, sha: &str) -> std::io::Result<()> {
        match std::fs::remove_dir_all(self.book_dir(sha)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    // ── Notebooks (Scribe handwriting) ──────────────────────────────────────
    // Layout: `notebooks/<uuid>/{nbk, cover.png, pages/page-<n>.svg}`, keyed by
    // the device `.notebooks/<uuid>` dir name. A notebook's identity is that
    // uuid; its bytes change under edits.

    /// Per-notebook directory: raw `nbk` backup + cover + cached page SVGs.
    pub fn notebook_dir(&self, uuid: &str) -> PathBuf {
        self.root.join("notebooks").join(uuid)
    }

    pub fn notebook_nbk(&self, uuid: &str) -> PathBuf {
        self.notebook_dir(uuid).join("nbk")
    }

    /// Device cover thumbnail (PNG). May not exist (cloud-only notebooks).
    pub fn notebook_cover(&self, uuid: &str) -> PathBuf {
        self.notebook_dir(uuid).join("cover.png")
    }

    pub fn notebook_pages_dir(&self, uuid: &str) -> PathBuf {
        self.notebook_dir(uuid).join("pages")
    }

    /// Cached SVG for one 0-based page.
    pub fn notebook_page_svg(&self, uuid: &str, index: usize) -> PathBuf {
        self.notebook_pages_dir(uuid)
            .join(format!("page-{index}.svg"))
    }

    /// Create the notebook dir (and its `pages/` subdir).
    pub fn ensure_notebook(&self, uuid: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(self.notebook_pages_dir(uuid))
    }

    /// Remove a notebook's files. `NotFound` is success (idempotent).
    pub fn remove_notebook(&self, uuid: &str) -> std::io::Result<()> {
        match std::fs::remove_dir_all(self.notebook_dir(uuid)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    // ── Misc device backup (the configured sync collections) ──────────────────
    // Layout: `device-backup/<serial>/<collection-id>/…`, the files a Sync pulls
    // off a Kindle, one subdir per collection in `device-sync.json`. See
    // `device_backup::SyncCollections`. A filename repeats across devices
    // (`sidle-native.log`); the `<serial>` segment separates them.

    /// Root holding every device's misc backup, one `<serial>/` subdir each.
    pub fn device_backup_dir(&self) -> PathBuf {
        self.root.join("device-backup")
    }

    /// One collection's files as pulled off one device. `id` is expected to
    /// have passed `device_backup::sanitize_id`.
    pub fn device_backup_collection(&self, serial: &str, id: &str) -> PathBuf {
        self.device_backup_dir()
            .join(sanitize_device_id(serial))
            .join(id)
    }

    /// Create the misc-backup dir for one device. Each per-collection subdir is
    /// created as files land in it.
    pub fn ensure_device_backup(&self, serial: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(self.device_backup_dir().join(sanitize_device_id(serial)))
    }

    /// Which device folders a Sync brings across, as edited in the app's
    /// settings. Keyed off the active library root, as
    /// [`device_dist`](Self::device_dist) is. Absent until the defaults are
    /// edited; see
    /// [`SyncCollections::load`](crate::library::device_backup::SyncCollections::load).
    pub fn device_sync_config(&self) -> PathBuf {
        self.root.join("device-sync.json")
    }

    // ── Handwritten ink on a sideloaded doc (PDOC) ──────────────────────────
    // Layout: `books/<sha>/ink/<asin>/{nbk, <container>.overlay.svg,
    // <container>.plain.svg}` — the raw nbk backup plus the per-page renders,
    // keyed by the ink notebook's page-container id. Nested under the host
    // book's `books/<sha>/`, which [`remove_sha`](Self::remove_sha) takes whole.

    /// Per-book ink directory for one ink notebook (one `asin`).
    pub fn book_ink_dir(&self, sha: &str, asin: &str) -> PathBuf {
        self.book_dir(sha).join("ink").join(asin)
    }

    /// Raw `nbk` backup for one ink notebook.
    pub fn book_ink_nbk(&self, sha: &str, asin: &str) -> PathBuf {
        self.book_ink_dir(sha, asin).join("nbk")
    }

    /// Transparent ink-only overlay SVG for one ink page — composited over the
    /// host PDF page in the reader.
    pub fn book_ink_overlay_svg(&self, sha: &str, asin: &str, container_id: &str) -> PathBuf {
        self.book_ink_dir(sha, asin)
            .join(format!("{}.overlay.svg", sanitize_ink_id(container_id)))
    }

    /// White-background "plain" SVG for one ink page — the gallery / standalone view.
    pub fn book_ink_plain_svg(&self, sha: &str, asin: &str, container_id: &str) -> PathBuf {
        self.book_ink_dir(sha, asin)
            .join(format!("{}.plain.svg", sanitize_ink_id(container_id)))
    }

    /// Create the ink dir for one `asin`.
    pub fn ensure_book_ink(&self, sha: &str, asin: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(self.book_ink_dir(sha, asin))
    }

    /// Remove all ink for one `asin`. `NotFound` is success (idempotent).
    pub fn remove_book_ink(&self, sha: &str, asin: &str) -> std::io::Result<()> {
        match std::fs::remove_dir_all(self.book_ink_dir(sha, asin)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Filesystem-safe form of an ink page-container id (a KFX `kfx_id`, which in
/// practice holds `[A-Za-z0-9_-]`). Names the cached SVG on disk; the `book_ink`
/// row holds the true id.
fn sanitize_ink_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A device serial as a single path segment for `device-backup/<serial>/`.
/// [`sanitize_ink_id`] keeps the alphanumerics a serial holds in practice, and
/// keeps `.` — an empty or all-dot result becomes `unknown-device`, bounding
/// `..` as traversal. The WiFi push carries this serial verbatim from the
/// request body.
fn sanitize_device_id(serial: &str) -> String {
    let s = sanitize_ink_id(serial);
    if s.is_empty() || s.chars().all(|c| c == '.') {
        "unknown-device".to_string()
    } else {
        s
    }
}

/// Length of the sha256 prefix used as the on-device filename infix
/// (`<basename>.<sha8>.kfx`). 8 hex chars = 32 bits; the birthday bound puts a
/// 50% collision chance at ~93k books.
pub const SHA_INFIX_LEN: usize = 8;

/// First [`SHA_INFIX_LEN`] hex chars of a KFX sha256.
///
/// Panics for `kfx_sha256.len() < SHA_INFIX_LEN`. Every caller reads
/// `books.kfx_sha256` (64 hex chars) or short-circuits on `NULL`.
pub fn sha_infix(kfx_sha256: &str) -> &str {
    &kfx_sha256[..SHA_INFIX_LEN]
}

/// Build the canonical on-device basename for a KFX: `<stem>.<sha8>.kfx`.
///
/// `<stem>` is the file_stem of `kfx_path`, the local library file under
/// `books/<sha>/`, which makes the device-side name mirror the Mac-side one. A
/// path with no usable stem yields `book-<sha8>`.
///
/// The USB push (`device::push::push_one`) and the LAN server's
/// Content-Disposition header both name a file through here.
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

/// The on-disk image extension for a media type or filename.
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
/// - Truncates to 180 chars, under the HFS+ filename limit.
pub fn format_basename(authors: &[String], title: &str, date: Option<&str>) -> String {
    let title = sanitize_segment(title);
    let title = if title.is_empty() {
        "Untitled".to_string()
    } else {
        title
    };

    let author = authors
        .first()
        .map(|a| sanitize_segment(a))
        .filter(|s| !s.is_empty());
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

/// Make `s` safe to use as a single filesystem path segment: replace the
/// characters Finder/macOS reject (and NUL) with `_`, turn control chars into
/// spaces, and collapse runs of whitespace. Shared by [`format_basename`] and
/// the library export (per-author subfolder names).
pub fn sanitize_segment(s: &str) -> String {
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

/// A free path: `path` itself, else ` (2)`, ` (3)`, … inserted before the
/// extension until one is free. Returns `path` at the attempt cap. Shared by the
/// library export and the notebook PDF export.
pub fn dedup_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 2..10_000 {
        let name = match &ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let cand = dir.join(name);
        if !cand.exists() {
            return cand;
        }
    }
    path
}

fn extract_year(date: &str) -> Option<String> {
    let bytes = date.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        let window = &bytes[i..i + 4];
        if window.iter().all(|b| b.is_ascii_digit()) {
            // A digit on either side makes this window part of a longer run
            // (`20230412`). A dash-separated `2023-04-12` has neither.
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

    /// The WiFi misc push carries the serial from the network verbatim. A
    /// dot-only segment resolves to `unknown-device`, not to traversal out of
    /// `device-backup/`.
    #[test]
    fn device_id_rejects_traversal_segments() {
        let paths = LibraryPaths {
            root: PathBuf::from("/tmp/root"),
        };
        for serial in ["..", ".", "...", ""] {
            assert_eq!(
                paths.device_backup_collection(serial, "screenshots"),
                PathBuf::from("/tmp/root/device-backup/unknown-device/screenshots"),
                "serial {serial:?} escaped the backup dir"
            );
        }
        // Separators fold to `_`, keeping a crafted serial one segment.
        assert_eq!(
            paths.device_backup_collection("../../etc", "logs"),
            PathBuf::from("/tmp/root/device-backup/.._.._etc/logs")
        );
        // An alphanumeric serial passes through untouched.
        assert_eq!(
            paths.device_backup_collection("G000AB12345678", "logs"),
            PathBuf::from("/tmp/root/device-backup/G000AB12345678/logs")
        );
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
        let path =
            "/Users/me/Library/Application Support/sidle/books/abc/[Author] Title (2024).kfx";
        assert_eq!(
            kfx_device_filename(path, SAMPLE_SHA),
            "[Author] Title (2024).deadbeef.kfx"
        );
    }

    #[test]
    fn kfx_device_filename_falls_back_when_no_stem() {
        assert_eq!(
            kfx_device_filename("", SAMPLE_SHA),
            "book-deadbeef.deadbeef.kfx"
        );
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
        // A basename holds dots ("v1.2"); the LAST `.`-separated
        // segment before `.kfx` is the sha.
        let name = "[A] Series v1.2 (2024).deadbeef.kfx";
        assert_eq!(parse_sha_infix(name).as_deref(), Some("deadbeef"));
    }

    // Root pointer, against an explicit state dir. The real
    // `~/Library/Application Support/Sidle/config.json` is untouched.

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

    #[test]
    fn migrate_legacy_state_dir_renames_lowercase_to_proper_case() {
        let base = tempfile::tempdir().unwrap();
        let lower = base.path().join("sidle");
        std::fs::create_dir_all(&lower).unwrap();
        std::fs::write(lower.join("config.json"), b"{}").unwrap();

        LibraryPaths::migrate_legacy_state_dir_in(base.path());

        let names: Vec<String> = std::fs::read_dir(base.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n == "Sidle"),
            "renamed to Sidle: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "sidle"),
            "no lowercase left: {names:?}"
        );
        assert!(
            base.path().join("Sidle/config.json").is_file(),
            "contents preserved"
        );

        // Idempotent: a second run is a no-op.
        LibraryPaths::migrate_legacy_state_dir_in(base.path());
        assert!(base.path().join("Sidle/config.json").is_file());
    }

    #[test]
    fn migrate_legacy_state_dir_noop_without_legacy_dir() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("Sidle")).unwrap();
        LibraryPaths::migrate_legacy_state_dir_in(base.path()); // no panic, no change
        assert!(base.path().join("Sidle").is_dir());
    }
}
