//! Library folder layout.
//!
//! ```text
//! ~/Library/Application Support/sidle/
//! ├── library.db
//! ├── epubs/<sha>/source.epub
//! └── cache/<sha>/
//!     ├── book.kfx
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

    pub fn epub_dir(&self, sha: &str) -> PathBuf {
        self.root.join("epubs").join(sha)
    }

    pub fn source_epub(&self, sha: &str) -> PathBuf {
        self.epub_dir(sha).join("source.epub")
    }

    pub fn cache_dir(&self, sha: &str) -> PathBuf {
        self.root.join("cache").join(sha)
    }

    pub fn kfx(&self, sha: &str) -> PathBuf {
        self.cache_dir(sha).join("book.kfx")
    }

    pub fn cover(&self, sha: &str, ext: &str) -> PathBuf {
        self.cache_dir(sha).join(format!("cover.{ext}"))
    }

    /// Ensure all base subdirectories exist.
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(self.root.join("epubs"))?;
        std::fs::create_dir_all(self.root.join("cache"))?;
        Ok(())
    }

    pub fn ensure_sha(&self, sha: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(self.epub_dir(sha))?;
        std::fs::create_dir_all(self.cache_dir(sha))?;
        Ok(())
    }

    /// Remove the per-sha directories. Best-effort.
    pub fn remove_sha(&self, sha: &str) {
        let _ = std::fs::remove_dir_all(self.epub_dir(sha));
        let _ = std::fs::remove_dir_all(self.cache_dir(sha));
    }
}

/// Extension helper that maps a media type or fallback path to an image extension.
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
