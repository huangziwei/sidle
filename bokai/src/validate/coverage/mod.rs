//! Coverage reports — **not** book validators. These measure *bokai's own*
//! completeness, not whether a book is valid: they answer "what should bokai
//! learn to handle next?" and never judge a book right or wrong.
//!
//! - [`tags`] — which HTML element names get a semantic role in `role_map` vs
//!   fall through to the generic-container catch-all.
//! - [`style`] — which CSS property names bokai's parser accepts vs silently
//!   drops.
//!
//! Kept here for proximity to the validators they inform, but they belong to
//! the bokai roadmap, not the pass/fail gate.

pub mod style;
pub mod tags;
