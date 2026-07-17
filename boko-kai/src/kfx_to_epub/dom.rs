//! Re-export of the shared XHTML DOM (see [`crate::export::xdom`]).
//!
//! The DOM type, serializer, and part-level passes moved to `export::xdom`
//! so the IR route's normalized export builds chapters through the exact
//! same code — byte-identical output by construction. This module keeps the
//! historical `kfx_to_epub::dom` path alive for the mechanical pipeline.

pub use crate::export::xdom::{ClassMap, Dom, Element, NodeId, new_book_part};
