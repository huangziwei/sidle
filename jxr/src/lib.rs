#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
// jxr is a vendored, frozen T.832 codec port whose style is dictated by the
// standard's pseudocode — index-driven loops over scan-order tables and plane
// grids, transform routines that take many coefficient arguments, and hex
// literals grouped by bit-field rather than byte boundary. The clippy lints
// below are a structural mismatch with that faithful-port style, not defects;
// the codec is fuzz-certified against its current source, so we silence them
// crate-wide rather than refactor working, frozen code — which keeps genuine new
// warnings in the active crates visible instead of buried under codec noise.
#![allow(clippy::needless_range_loop)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]
#![allow(clippy::unusual_byte_groupings)]
#![allow(clippy::manual_range_contains)]
#![allow(clippy::manual_range_patterns)]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::useless_conversion)]

pub mod decode;
pub mod encode;

pub use encode::{
    BandQp, BandsPresent, ChannelOrder, ChromaSampling, ColorMode, EncodeError, EncodeOptions,
    ImageInput, Overlap, QpPlan, QpSet, SamplePlanes, TileQps, TypedInput, deinterleave, encode,
    encode_typed, encode_with_alpha_qp, encode_with_options, quality_to_qp,
};
