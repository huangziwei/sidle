//! JPEG-XR decoder: TIFF-like container parsing + full T.832 codestream
//! reconstruction. Line-by-line port of `jxr_image.py` (John Howell, KFX
//! Input plugin), itself written from the ITU-T T.832 pseudo-code — so the
//! codestream side covers the whole spec, not just the Kindle subset.
//!
//! Entry points: [`container::parse`] for the outer file, then
//! [`decoder::Decoder`] on the extracted codestream bytes.
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

pub mod consts;
pub mod container;
pub mod decoder;
pub mod math;
pub mod misc;
pub mod state;
pub mod tables;
