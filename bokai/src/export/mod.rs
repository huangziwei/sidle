//! Export module for writing ebooks to various formats.
//!
//! Provides the `Exporter` trait and format-specific implementations.
//!
//! # Architecture
//!
//! The `Exporter` trait uses a builder pattern:
//! - `new()` creates an exporter with default configuration
//! - `with_config()` allows customization
//! - `export()` writes to any `Write + Seek` destination
//!
//! # Example
//!
//! ```no_run
//! use bokai::{Book, Format};
//! use bokai::export::{EpubExporter, Exporter};
//! use std::fs::File;
//!
//! let mut book = Book::open("input.azw3")?;
//! let mut file = File::create("output.epub")?;
//!
//! // Using the exporter directly
//! EpubExporter::new().export(&mut book, &mut file)?;
//!
//! // Or using the Book convenience method
//! // book.export(Format::Epub, &mut file)?;
//! # Ok::<(), std::io::Error>(())
//! ```

use std::io::{self, Seek, Write};

use crate::model::Book;

pub mod azw3;
pub mod epub;
mod kfx;
mod markdown;

pub use azw3::{Azw3Config, Azw3Exporter};
pub use epub::normalize::{InlineStyleEmit, LinkOutcome, SourceStyles};
pub use epub::synth::{
    CssArtifact, SynthesisResult, escape_xml, escape_xml_into, generate_css, generate_css_all,
    synthesize_html, synthesize_html_with_class_list, synthesize_xhtml_document,
    synthesize_xhtml_document_with_class_list, synthesize_xhtml_document_with_links,
};
pub use epub::{
    Assets, EpubConfig, EpubExporter, EpubPackage, PackageAsset, PackageDocument, PackageOptions,
    build_package,
};
pub use epub::{
    ChapterContent, GlobalStylePool, NormalizedContent, SourceElements, normalize_book,
    normalize_book_with,
};
pub use kfx::{KfxConfig, KfxExporter, PdfKfxMeta, pdf_to_kfx};
pub use markdown::{MarkdownConfig, MarkdownExporter};

pub use epub::{nav, opf};

/// Trait for exporting books to specific formats.
///
/// Exporters use a builder pattern where configuration is held in the struct,
/// and the `export` method writes to any `Write + Seek` destination.
pub trait Exporter {
    /// Export the book to the provided writer.
    ///
    /// The writer can be:
    /// - `std::fs::File` for disk output
    /// - `Vec<u8>` for in-memory output
    /// - `std::io::Cursor<Vec<u8>>` for seekable in-memory output
    /// - Any other type implementing `Write + Seek`
    fn export<W: Write + Seek>(&self, book: &mut Book, writer: &mut W) -> io::Result<()>;
}
