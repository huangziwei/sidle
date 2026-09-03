//! Conversion-fidelity validation: does EPUB ⇄ KFX conversion preserve the
//! source's semantics? Each check diffs one feature across an `(epub, kfx)` pair.

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
