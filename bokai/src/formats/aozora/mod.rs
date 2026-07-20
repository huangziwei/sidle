//! Aozora Bunko → EPUB conversion.
//!
//! Faithful port of the standalone aozora-epub JS reference tool. The HTML
//! tool is the spec; output is functionally identical, not byte-identical.
//!
//! The pipeline lives entirely upstream of the EPUB importer:
//!
//! ```text
//! .zip → parse (txt) → Document → EpubBuilder → EPUB bytes
//! ```
//!
//! `EpubImporter::from_source` then takes over and the existing
//! EPUB → KFX path runs unmodified.

pub mod cover;
pub mod epub_builder;
pub mod parser_txt;

pub use cover::{build_cover_svg, render_cover_jpeg};
pub use epub_builder::{EpubInput, build_epub};
pub use parser_txt::{Document, TocEntry, parse_txt};
