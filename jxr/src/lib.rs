#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

pub mod decode;
pub mod encode;

pub use encode::{
    BandsPresent, ChannelOrder, ChromaSampling, ColorMode, EncodeError, EncodeOptions, ImageInput, QpSet,
    deinterleave, encode, encode_with_alpha_qp, encode_with_options, quality_to_qp,
};
