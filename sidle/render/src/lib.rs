//! Sidle's native renderer: a styled document tree in, positioned boxes out.
//!
//! [`Layout::chapter`] walks a [`bokai::model::Chapter`] into a [`Fragment`]
//! tree, one rectangle per box, each carrying the [`bokai::model::NodeId`] it
//! came from. [`paint`] draws that tree onto a pixel buffer.
//!
//! Every measurement is along the writing axis and across it, so [`Axis`]
//! decides where a box lands and the rest of layout does not.

pub mod book;
#[cfg(feature = "raster")]
pub mod chrome;
pub mod decorate;
pub mod flow;
pub mod font;
pub mod fragment;
pub mod geom;
pub mod inline;
#[cfg(feature = "oracle")]
pub mod oracle;
pub mod pages;
#[cfg(feature = "raster")]
pub mod paint;
#[cfg(feature = "probe")]
pub mod probe;
pub mod resolve;
pub mod resource;
pub mod settings;
pub mod text;
pub mod units;

pub use book::BookResources;
pub use decorate::{Decorations, Kind, Mark, Span, Tint};
pub use flow::{Layout, Page, Viewport};
pub use font::Fonts;
pub use fragment::{Content, Fragment, Glyph, GlyphRun, Node, Orientation};
pub use geom::{Axis, Edges, Rect, Size};
pub use pages::Pages;
pub use resource::{Bitmap, Resources, Unknown};
pub use settings::{Direction, Panel, Settings};
pub use units::Metrics;
