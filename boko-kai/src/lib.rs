//! # boko
//!
//! A high-performance, format-agnostic ebook processing engine.
//!
//! ## Architecture
//!
//! Boko uses an **Importer** architecture for reading ebooks:
//! - `Book` is the runtime handle that wraps format-specific backends
//! - `Importer` trait defines the interface for format backends
//! - Lazy loading via `ByteSource` for efficient random access
//!
//! ## Supported Formats
//!
//! | Format | Read | Write |
//! |--------|------|-------|
//! | EPUB   | ✓    | ✓     |
//! | KFX    | ✓    | ✓     |
//! | AZW3   | ✓    | -     |
//! | MOBI   | ✓    | -     |
//!
//! ## Quick Start
//!
//! ```no_run
//! use boko::Book;
//!
//! let mut book = Book::open("input.epub")?;
//! println!("Title: {}", book.metadata().title);
//!
//! // Iterate chapters (collect spine first to avoid borrow issues)
//! let spine: Vec<_> = book.spine().to_vec();
//! for entry in spine {
//!     let content = book.load_raw(entry.id)?;
//!     println!("Chapter: {} bytes", content.len());
//! }
//! # Ok::<(), std::io::Error>(())
//! ```

pub mod dom;
pub mod export;
pub mod image;
pub mod import;
pub mod io;
pub mod model;
pub mod style;

// The JPEG-XR codec is its own top-level workspace crate (`../jxr`).
// Re-exported because `model::Book`'s public API exposes `jxr::ColorMode`.
pub use jxr;

#[cfg(feature = "aozora")]
pub mod aozora;
pub mod epub;
pub mod kfx;
pub mod kfx_to_epub;
pub mod mobi;
pub mod render;
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

// Primary exports from dom
pub use dom::compile_html;

// Primary exports from other modules
pub use export::{EpubExporter, Exporter};
pub use import::{ChapterId, Importer, SpineEntry};
pub use io::{ByteSource, FileSource};
