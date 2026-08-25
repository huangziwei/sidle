//! Core data model for ebook processing: `book` (metadata and the runtime
//! handle), `chapter` and `node` (the IR tree), `semantic`, `links`, `notes`,
//! `position`, `text`, `font`, `section_tree` and `toc_shape`.

mod book;
mod chapter;
mod font;
mod links;
mod node;
mod notes;
mod position;
mod resolved;
pub mod role_map;
pub mod section_tree;
mod semantic;
mod text;
pub mod toc_shape;

pub use book::{
    Book, CollectionInfo, Contributor, Format, Landmark, LandmarkType, Metadata, OrientationLock,
    PageSpread, Panel, PanelRect, PeriodicalKind, Resource, TocEntry,
};

pub use chapter::{Chapter, ChapterId, ChapterSummary, ChildIter, DfsIter};

pub use node::{Node, NodeId, Role, TextRange};

pub use semantic::SemanticMap;

pub use links::{AnchorTarget, GlobalNodeId, InternalLocation, Link, LinkTarget};

pub use position::PositionMap;
pub use text::SourceText;

pub use notes::NoteRole;
pub use resolved::ResolvedLinks;

pub use font::FontFace;

pub use section_tree::{ContentBlock, SectionNode, SectionTree, extract_section_tree};

pub use toc_shape::{TocNode, TocTree, merge_by_document_order, nest_by_label_indent};
