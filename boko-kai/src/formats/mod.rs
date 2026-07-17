//! Format internals shared by both conversion directions.
//!
//! Each child holds one format's machinery — containers, parsers, schemas,
//! in-place edits, repairs — used by that format's importer (`crate::import`)
//! and exporter (`crate::export`) alike. The per-direction entry points live
//! in those modules; nothing here depends on either direction.

#[cfg(feature = "aozora")]
pub mod aozora;
pub mod epub;
pub mod kfx;
pub mod markdown;
pub mod mobi;
pub mod pdf;
