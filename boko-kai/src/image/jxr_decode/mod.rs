//! Pure-Rust JPEG-XR codestream decoder for the KFX → EPUB mechanical port.
//!
//! ## Layout
//!
//! - `misc` — bit/byte stream reader used by both the container and
//!   codestream parsers.
//! - `container` — TIFF-like outer file parser (extracts the WMPHOTO
//!   codestream bytes + image dimensions / pixel format UUID).
//! - `consts` / `tables` — constants and Huffman tables from the spec.
//! - `math` — IDCT / butterfly / overlap-filter primitives.
//! - `state` — plane / MB / adaptive-VLC structs.
//! - `decoder` — the codestream decoder pipeline.
//!
//! ## Why pure Rust
//!
//! KFX raw media is `image/jxr` for most images in modern Amazon files
//! (Japanese light novels especially). EPUB readers don't support JXR;
//! calibre transcodes via Pillow → libjxr. We don't want a C FFI dep or
//! a system magick binary for sidle/Tauri distribution, so we ported
//! calibre's pure-Python decoder line-by-line.
//!
//! The KFX→EPUB JPEG re-encode glue (which needs `ConvertError` /
//! `jpeg_encoder`) lives in [`super::jxr_transcode`], keeping this codec
//! dependency-free so it can be lifted into a standalone crate.

pub mod consts;
pub mod container;
pub mod decoder;
pub mod math;
pub mod misc;
pub mod state;
pub mod tables;
