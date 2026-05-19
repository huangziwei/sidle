//! Kindle USB mass-storage transport.
//!
//! - P2a: push KFX to `/documents`, delete what we've sent.
//! - P2b: pull `.kfx`/`.kfx-zip` from `/dedrm` and import them. boko's
//!   `Book::open` handles both single-container and multi-container bundles
//!   so this module just hands paths to the standard import pipeline.

pub mod dedrm;
pub mod detect;
pub mod manifest;
pub mod monitor;
pub mod push;
