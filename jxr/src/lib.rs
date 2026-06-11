#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod decode;
pub mod encode;

pub use encode::{
    BandQp, BandsPresent, ChannelOrder, ChromaSampling, ColorMode, EncodeError, EncodeOptions,
    ImageInput, Overlap, QpPlan, QpSet, SamplePlanes, TileQps, TypedInput, deinterleave, encode,
    encode_typed, encode_with_alpha_qp, encode_with_options, quality_to_qp,
};
