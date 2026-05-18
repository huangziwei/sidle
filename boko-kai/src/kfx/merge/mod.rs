//! `.kfx-zip` → `.kfx` merge dispatcher.
//!
//! Two paths share the support modules (`symtab`, `node`, `fragment`,
//! `container`, `structure`, `catalog`, `common`, `trace`):
//!
//!  - [`mechanical`]: faithful port of calibre's `convert_to_single_kfx`
//!    pipeline. Every entity is parsed → walked → re-encoded. Correctness
//!    ground truth.
//!  - [`fast`]: byte-passthrough merge. Skips entity-body parse + re-encode,
//!    synthesizes only `$270` and `$419`. Produces a different byte stream
//!    that calibre still accepts (verified to produce identical EPUBs).
//!
//! Default is [`MergeMode::Mechanical`]. Switch via CLI flag or the
//! `merge_kfx_zip_with_mode` entry point.

mod catalog;
mod common;
mod container;
mod fast;
mod fragment;
mod mechanical;
mod node;
mod structure;
mod symtab;
mod trace;

use std::io;
use std::path::Path;

/// Which merge implementation to use.
#[derive(Debug, Clone, Copy, Default)]
pub enum MergeMode {
    /// Faithful port of calibre's pipeline. Slower, full Ion roundtrip on
    /// every entity. The reference for correctness.
    #[default]
    Mechanical,
    /// Byte-passthrough merge. Entity bodies are copied verbatim from source
    /// to output; only the merged container's `$270` + `$419` are encoded
    /// fresh.
    Fast,
}

/// Merge a `.kfx-zip` bundle into a single `.kfx` container payload (bytes).
/// Uses the [`MergeMode::Mechanical`] path — for the fast path, call
/// [`merge_kfx_zip_with_mode`].
pub fn merge_kfx_zip(path: &Path) -> io::Result<Vec<u8>> {
    merge_kfx_zip_with_mode(path, MergeMode::Mechanical)
}

/// Merge using the specified mode. Falls back from [`MergeMode::Fast`] to
/// [`MergeMode::Mechanical`] if the fast path's preconditions don't hold
/// (e.g. multiple sources carry `doc_symbols`).
pub fn merge_kfx_zip_with_mode(path: &Path, mode: MergeMode) -> io::Result<Vec<u8>> {
    match mode {
        MergeMode::Mechanical => mechanical::merge_kfx_zip(path),
        MergeMode::Fast => match fast::merge_kfx_zip(path) {
            Ok(out) => Ok(out),
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                eprintln!(
                    "[merge] fast path unsupported for this bundle ({}); falling back to mechanical",
                    e
                );
                mechanical::merge_kfx_zip(path)
            }
            Err(e) => Err(e),
        },
    }
}
