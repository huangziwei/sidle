//! Kindle Scribe handwritten-notebook (`.nbk`) decode → SVG.
//!
//! An `.nbk` is a KDF/KPF SQLite container (the same `fragments(id,
//! payload_type, payload_value)` shape as KFX) whose Ion-binary payloads
//! describe a vector-ink "note model" (`nmdl.*`). This module is the offline
//! extraction core: de-fingerprint the SQLite, read fragments, resolve the
//! per-file symbol table, walk pages → ink layers → strokes, and render each
//! page to SVG. It reuses boko's Ion parser + `KFX_SYMBOL_TABLE`; the genuinely
//! new pieces (vs. KFX books) are the KDF-SQLite read, the `nmdl` stroke decode,
//! and stroke→SVG.
//!
//! Ported from `ref/scribe-library/kfxlib` (GPLv3, compatible with this crate's
//! GPL-3.0-or-later). Gated behind the `nbk` feature: pulls `rusqlite`
//! (bundled C SQLite, native-only).

mod density;
mod fingerprint;
mod kdf;
mod note_model;
mod render_svg;
mod shapes;
mod stroke;
mod symtab;
mod template;

use std::path::Path;

pub use note_model::{Page, Stroke};

/// A decoded Scribe notebook: pages in reading order.
#[derive(Debug, Clone)]
pub struct Notebook {
    pub pages: Vec<Page>,
}

impl Notebook {
    /// Number of pages (matches the device's page count).
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Render every page to a standalone SVG document.
    pub fn page_svgs(&self) -> Vec<String> {
        self.pages.iter().map(render_svg::page_to_svg).collect()
    }

    /// Render a single page (0-indexed) to an SVG document.
    pub fn page_svg(&self, index: usize) -> Option<String> {
        self.pages.get(index).map(render_svg::page_to_svg)
    }

    /// Render every page as a transparent, ink-only overlay (no white page, no
    /// ruled template) — for compositing the user's handwritten ink on top of
    /// its host document page in the reader. See [`render_svg::page_to_overlay_svg`].
    pub fn page_overlay_svgs(&self) -> Vec<String> {
        self.pages.iter().map(render_svg::page_to_overlay_svg).collect()
    }

    /// Render a single page (0-indexed) as a transparent ink-only overlay.
    pub fn page_overlay_svg(&self, index: usize) -> Option<String> {
        self.pages.get(index).map(render_svg::page_to_overlay_svg)
    }
}

/// Errors from notebook decoding.
#[derive(Debug)]
pub enum NbkError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    /// Malformed or unexpected note-model structure.
    Format(String),
}

impl std::fmt::Display for NbkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NbkError::Io(e) => write!(f, "nbk io error: {e}"),
            NbkError::Sqlite(e) => write!(f, "nbk sqlite error: {e}"),
            NbkError::Format(m) => write!(f, "nbk format error: {m}"),
        }
    }
}

impl std::error::Error for NbkError {}

impl From<std::io::Error> for NbkError {
    fn from(e: std::io::Error) -> Self {
        NbkError::Io(e)
    }
}

impl From<rusqlite::Error> for NbkError {
    fn from(e: rusqlite::Error) -> Self {
        NbkError::Sqlite(e)
    }
}

/// Open and fully decode a Scribe `.nbk` file into a [`Notebook`].
pub fn open(nbk_path: &Path) -> Result<Notebook, NbkError> {
    let fragments = kdf::read_fragments(nbk_path)?;
    let pages = note_model::build_pages(&fragments)?;
    Ok(Notebook { pages })
}
