//! Coverage reports — **not** book validators. These measure *boko's own*
//! completeness, not whether a book is valid: they answer "what should boko
//! learn to handle next?" and never judge a book right or wrong.
//!
//! - [`tags`] — which HTML element names get a semantic role in `role_map` vs
//!   fall through to the generic-container catch-all.
//! - [`style`] — which CSS property names boko's parser accepts vs silently
//!   drops.
//!
//! Kept here for proximity to the validators they inform, but they belong to
//! the boko roadmap, not the pass/fail gate.

pub mod style;
pub mod tags;
