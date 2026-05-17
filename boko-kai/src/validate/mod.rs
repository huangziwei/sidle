//! Conversion validation — verify boko-kai's KFX output preserves the
//! semantics of the source EPUB. Each submodule covers one feature
//! (ruby today; emphasis/links/headings later) and exposes:
//!
//! - an independent extractor for the source EPUB (using minimal XHTML
//!   tokenization, NOT going through boko's IR — so a parser-side bug
//!   surfaces here rather than being silently mirrored on both sides),
//! - an extractor for the converted KFX (using boko's own KFX parser,
//!   since the format is shared with the Kindle reader anyway),
//! - a comparison function producing a `Report`.

pub mod ruby;
pub mod style;
pub mod tags;
pub mod text;
