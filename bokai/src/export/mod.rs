//! Export module for writing ebooks to various formats.
//!
//! Provides the `Exporter` trait and format-specific implementations.

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
    AssetSink, Assets, EpubConfig, EpubExporter, EpubPackage, PackageAsset, PackageDocument,
    PackageOptions, build_package, build_package_into,
};
pub use epub::{
    ChapterContent, GlobalStylePool, NormalizedContent, SourceElements, normalize_book,
    normalize_book_with,
};
pub use kfx::{KfxConfig, KfxExporter};
#[cfg(feature = "pdf")]
pub use kfx::{PdfKfxMeta, pdf_to_kfx};
pub use markdown::{MarkdownConfig, MarkdownExporter};

pub use epub::{nav, opf};

/// Trait for exporting books to specific formats.
pub trait Exporter {
    /// Export the book to the provided writer.
    ///
    /// The writer can be:
    fn export<W: Write + Seek>(&self, book: &mut Book, writer: &mut W) -> io::Result<()>;
}
