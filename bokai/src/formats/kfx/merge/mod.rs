//! `.kfx-zip` → `.kfx` merge dispatcher.

mod catalog;
mod common;
mod container;
mod fast;
mod fragment;
mod mechanical;
mod node;
mod structure;
mod symtab;

use std::io;
use std::path::Path;

/// Which merge implementation to use.
#[derive(Debug, Clone, Copy, Default)]
pub enum MergeMode {
    /// Byte-passthrough merge. Entity bodies are copied verbatim from source
    #[default]
    Fast,
    /// Faithful port of calibre's `convert_to_single_kfx` pipeline. Slower,
    /// full Ion roundtrip on every entity. Kept as the correctness reference
    /// — any change to the fast path is validated against this baseline.
    Mechanical,
}

/// Merge a `.kfx-zip` bundle into a single `.kfx` container payload (bytes).
/// Uses the default [`MergeMode`] (currently [`MergeMode::Fast`]). For
/// explicit control, call [`merge_kfx_zip_with_mode`].
pub fn merge_kfx_zip(path: &Path) -> io::Result<Vec<u8>> {
    merge_kfx_zip_with_mode(path, MergeMode::default())
}

/// Merge in-memory `.kfx-zip` bytes into a single `.kfx` payload, no filesystem.
/// Always the thread-free [`MergeMode::Mechanical`] path.
pub fn merge_kfx_zip_bytes(data: &[u8]) -> io::Result<Vec<u8>> {
    mechanical::merge_kfx_zip_reader(io::Cursor::new(data))
}

/// Merge using the specified mode. Each mode is terminal: [`MergeMode::Fast`]
pub fn merge_kfx_zip_with_mode(path: &Path, mode: MergeMode) -> io::Result<Vec<u8>> {
    match mode {
        MergeMode::Mechanical => mechanical::merge_kfx_zip(path),
        MergeMode::Fast => fast::merge_kfx_zip(path),
    }
}
