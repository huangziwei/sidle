//! Source validation — is one book file well-formed on its own? Each check
//! reads a single input in its native format and never consults a
//! derived/converted copy. These flag defects **in the source book**, so they
//! are what the book editor turns into a repair list.
//!
//! - [`epub`] — EPUB-3 structural conformance (a Rust `epubcheck` replacement):
//!   mimetype, container/OPF wiring, manifest ↔ zip ↔ spine integrity, nav
//!   presence, non-linear reachability, href resolution.
//! - [`toc`] — cross-format declared-TOC audit: is the reader's chapter sidebar
//!   chapterless while the book itself clearly has chapters? Sniffs EPUB vs KFX
//!   and reads only that source.
//!
//! Planned: a KFX structural checker (`source::kfx`) — the same idea as
//! `source::epub` for KFX (container/entity integrity, nav reachability,
//! style/resource resolution). See the validator-architecture plan.

pub mod epub;
pub mod toc;
