//! The schema engine behind epubcheck's `RSC-005` channel.
//!
//! epubcheck hand-codes only a minority of what it reports about a package
//! document or a content document. The rest it gets by validating them against
//! **RELAX NG** grammars and **Schematron** assertion sets (vendored under
//! `xml/schema/`), funnelling every violation through one message
//! id. Reproducing that channel means reproducing the two engines.
//!
//! Why an engine rather than more hand-ported rules: a hand-written check covers
//! the violation shape that prompted it, so a validator built that way is only
//! as good as the books it has already seen. A grammar covers the shapes the
//! format admits — including the ones in a book that arrives tomorrow, which is
//! the bar this validator is held to.
//!
//! **Why not [`crate::html`]'s tree.** That one is html5ever's `TreeSink`
//! target, and the HTML5 parsing algorithm exists to *repair* malformed input:
//! it infers `<tbody>`, hoists misplaced `<meta>`, reparents stray content,
//! drops duplicate attributes, and never fails. Validating its output would
//! judge a different document than the one on disk. A schema must judge the
//! document as written, and XML has no error recovery — so this module parses
//! its own tree ([`tree`]), with the opposite contract.
//!
//! - [`tree`] — the namespace-aware document tree both engines walk.
//! - [`preprocess`] — the rewrites epubcheck applies before a schema runs.
//! - [`nvdl`] — which schema validates which part of a document.
//! - [`relaxng`] — the grammar half: a derivative-based RELAX NG validator.
//! - [`schematron`] — the assertion half, and the XPath it is written in.
//! - [`schema`] — epubcheck's own schemas, vendored and compiled in.
//! - [`engine`] — the map from a resource to the schemas that judge it.

pub mod dtd;
pub mod engine;
pub mod nvdl;
pub mod preprocess;
pub mod relaxng;
pub mod schema;
pub mod schematron;
pub mod tree;

pub use tree::{Attr, Document, Element, Name, NodeId, NodeKind, ParseError};
