//! Conversion-fidelity validation — did EPUB ⇄ KFX conversion preserve the
//! source's semantics? Every check takes a **pair**
//! (`validate(epub_bytes, kfx_bytes) -> Report`) and diffs one feature across
//! the conversion; a loss is a bokai converter bug, so these are the CI checks.
//!
//! Each module exposes an independent EPUB-side extractor (minimal XHTML
//! tokenization, NOT bokai's IR, so a parser bug surfaces here rather than being
//! mirrored on both sides) and a KFX-side extractor (bokai's own KFX parser).
//! The diff is direction-neutral; the [`Direction`] only
//! changes how a printed report labels source vs target.
//!
//! Boundary note: [`nav`] asks *did the source TOC/headings survive the
//! conversion* (a bokai bug if not); the separate `source::toc` check asks *is
//! the source TOC itself deficient* (a book defect → editor). Different
//! questions.

// Shared by every child module's `super::Direction` references.
pub(crate) use super::Direction;

pub mod epub_diff;
pub mod fxl;
pub mod images;
pub mod links;
pub mod metadata;
pub mod nav;
pub mod page_progression;
pub mod ruby;
pub mod text;
pub mod writing_mode;
