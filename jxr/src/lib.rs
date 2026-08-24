#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
// `needless_range_loop`, `too_many_arguments` and `unusual_byte_groupings`
// name the shape of a T.832 port: index-driven loops over scan-order tables,
// transforms taking many coefficients, hex literals grouped by bit-field.
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
