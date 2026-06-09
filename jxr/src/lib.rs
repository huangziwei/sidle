#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

pub mod decode;
pub mod encode;

pub use encode::{ColorMode, EncodeError, ImageInput, QpSet, encode, quality_to_qp};
