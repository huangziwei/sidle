//! Aozora Bunko → EPUB conversion.

pub mod cover;
pub mod epub_builder;
pub mod gaiji;
pub mod parser_txt;

pub use cover::{build_cover_svg, render_cover_jpeg};
pub use epub_builder::{EpubInput, build_epub};
pub use parser_txt::{Document, TocEntry, parse_txt};
