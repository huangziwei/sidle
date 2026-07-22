//! # bokai
//!
//! A high-performance, format-agnostic ebook processing engine.
//!
//! ## Architecture
//!
//! Bokai uses an **Importer** architecture for reading ebooks:
//! - `Book` is the runtime handle that wraps format-specific backends
//! - `Importer` trait defines the interface for format backends
//! - Lazy loading via `ByteSource` for efficient random access
//!
//! ## Supported Formats
//!
//! | Format   | Read | Write |
//! |----------|------|-------|
//! | EPUB     | ✓    | ✓     |
//! | KFX      | ✓    | ✓     |
//! | AZW3     | ✓    | ✓     |
//! | MOBI     | ✓    | -     |
//! | PDF      | ✓    | -     |
//! | Markdown | -    | ✓     |
//!
//! ## Module map
//!
//! Layered: vocabulary (`model`, `style`) → markup compiler (`html`) →
//! format internals (`formats`) → directions (`import`, `export`).
//!
//! - [`model`] — the IR and the [`Book`] runtime handle: chapters, nodes,
//!   roles, semantics, links, metadata
//! - [`style`] — CSS vocabulary: property types, typed + raw declarations,
//!   cascade, computed-style pool
//! - [`html`] — the chapter-markup compiler (HTML/XHTML + CSS → IR),
//!   serving every importer whose chapter content is HTML
//! - [`formats`] — per-format internals shared by import and export
//!   (containers, parsers, schemas, source repairs; PDF page rasterization
//!   lives at [`formats::pdf::render`])
//! - [`import`] — the [`Importer`] trait and per-format importers
//! - [`export`] — the [`export::Exporter`] trait and per-format exporters
//! - [`image`], [`io`] — image codecs and byte sources
//! - [`validate`] — book/conversion validation (source structure, fidelity,
//!   tag coverage)
//! - `util`, `trace` — crate-internal helpers
//!
//! ## Quick Start
//!
//! ```no_run
//! use bokai::Book;
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

pub mod export;
pub mod formats;
pub mod html;
pub mod image;
pub mod import;
pub mod io;
pub mod model;
pub mod style;

// The JPEG-XR codec is its own top-level workspace crate (`../jxr`).
// Re-exported because `model::Book`'s public API exposes `jxr::ColorMode`.
pub use jxr;

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
