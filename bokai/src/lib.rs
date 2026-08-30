//! # bokai
//!
//! A high-performance, format-agnostic ebook processing engine.

pub mod export;
pub mod formats;
pub mod html;
pub mod image;
pub mod import;
pub mod io;
pub mod model;
pub mod style;
pub mod text;

// The JPEG-XR codec is its own top-level workspace crate (`../jxr`).
// Re-exported because `model::Book`'s public API exposes `jxr::ColorMode`.
pub use jxr;

#[cfg(feature = "validate")]
pub mod validate;

pub(crate) mod trace;
pub(crate) mod util;

// Primary exports from model
pub use model::{
    Book, Chapter, ContentBlock, Format, Metadata, Node, NodeId, Resource, Role, SectionNode,
    SectionTree, SemanticMap, TextRange, TocEntry, extract_section_tree,
};

// Primary exports from style
pub use style::{ComputedStyle, ListStyleType, Origin, StyleId, StylePool, Stylesheet, ToCss};

// Primary exports from html (the chapter-markup compiler)
pub use html::compile_html;

// Primary exports from other modules
pub use export::{
    Azw3Config, Azw3Exporter, EpubExporter, Exporter, MarkdownConfig, MarkdownExporter,
};
pub use import::{ChapterId, Importer, SpineEntry};
pub use io::{ByteSource, FileSource};
