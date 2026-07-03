//! Consumer-grade pixel buffer: packs a [`DecodedImage`]'s raw `i32` planes
//! into interleaved little-endian samples with an explicit layout, so callers
//! don't need to know the per-bitdepth value conventions (clamping, float
//! bit patterns, RGBE, …). The raw planes remain available on
//! [`DecodedImage`] as the escape hatch.

use super::consts::*;
use super::decoder::DecodedImage;

/// Per-sample storage type of a [`PixelBuffer`] (always little-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleType {
    /// 1 byte, 0–255.
    U8,
    /// 2 bytes, 0–65535.
    U16,
    /// 2 bytes, signed (fixed-point formats).
    I16,
    /// 2 bytes, IEEE 754 half **bit pattern**.
    F16,
    /// 4 bytes, signed (32-bit fixed-point formats).
    I32,
    /// 4 bytes, IEEE 754 single **bit pattern**.
    F32,
    /// 2 bytes, packed 5-6-5 RGB (one sample per *pixel*).
    Packed565,
}

impl SampleType {
    /// Bytes per sample (1, 2 or 4).
    pub fn bytes(self) -> usize {
        match self {
            SampleType::U8 => 1,
            SampleType::U16 | SampleType::I16 | SampleType::F16 | SampleType::Packed565 => 2,
            SampleType::I32 | SampleType::F32 => 4,
        }
    }
}

/// Color interpretation of the channels (alpha, when present, follows them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorModel {
    /// Single luma channel.
    Gray,
    /// Red, green, blue.
    Rgb,
    /// Cyan, magenta, yellow, black ink.
    Cmyk,
    /// Shared-exponent RGBE: 4 channels of U8 (R, G, B, E).
    Rgbe,
    /// N arbitrary channels.
    NChannel(u8),
}

/// How to interpret the trailing alpha channel, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlphaMode {
    /// No alpha channel.
    None,
    /// Straight (non-premultiplied) alpha.
    Straight,
    /// Color channels are premultiplied by alpha.
    Premultiplied,
}

/// Interleaved little-endian pixels + layout description.
pub struct PixelBuffer {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Total channels per pixel, including alpha (1 for `Packed565`).
    pub channels: u8,
    /// Color interpretation of the leading channels.
    pub color: ColorModel,
    /// Whether (and how) the trailing channel is alpha.
    pub alpha: AlphaMode,
    /// Per-sample storage type.
    pub sample: SampleType,
    /// `width × height × channels` samples, row-major, channels interleaved,
    /// each sample `sample.bytes()` long, little-endian.
    pub data: Vec<u8>,
}

/// Error from [`DecodedImage::to_pixel_buffer`].
#[derive(Debug)]
pub enum PixelError {
    /// A (color format, bit depth) combination this packer doesn't cover.
    Unsupported(String),
}

impl std::fmt::Display for PixelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PixelError::Unsupported(s) => write!(f, "pixel packing unsupported: {s}"),
        }
    }
}

impl std::error::Error for PixelError {}

impl DecodedImage {
    /// Pack the decoded planes into an interleaved [`PixelBuffer`].
    pub fn to_pixel_buffer(&self) -> Result<PixelBuffer, PixelError> {
        let (color, base_channels): (ColorModel, usize) = match self.output_clr_fmt {
            OUT_YONLY => (ColorModel::Gray, 1),
            OUT_RGB | OUT_YUV444 | OUT_YUV422 | OUT_YUV420 => (ColorModel::Rgb, 3),
            OUT_CMYK | OUT_CMYKDIRECT => (ColorModel::Cmyk, 4),
            OUT_RGBE => (ColorModel::Rgbe, 4),
            OUT_NCOMPONENT => {
                let n = self.num_components - usize::from(self.has_alpha);
                (ColorModel::NChannel(n as u8), n)
            }
            other => {
                return Err(PixelError::Unsupported(format!(
                    "output color format {other}"
                )));
            }
        };
        // RGBE's exponent plane is layout, not alpha; everything beyond the
        // base channels otherwise is the alpha plane.
        let has_alpha = self.has_alpha && self.output_clr_fmt != OUT_RGBE;
        let alpha = if !has_alpha {
            AlphaMode::None
        } else if self.premultiplied_alpha {
            AlphaMode::Premultiplied
        } else {
            AlphaMode::Straight
        };
        let channels = base_channels + usize::from(has_alpha);
        if channels > self.image_plane.len() {
            return Err(PixelError::Unsupported(format!(
                "{channels} channels claimed but {} planes decoded",
                self.image_plane.len()
            )));
        }

        let sample = match self.output_bitdepth {
            BD8 | BD1WHITE1 | BD1BLACK1 => SampleType::U8,
            BD16 => SampleType::U16,
            BD16S => SampleType::I16,
            BD16F => SampleType::F16,
            BD32S => SampleType::I32,
            BD32F => SampleType::F32,
            BD565 => SampleType::Packed565,
            other => {
                return Err(PixelError::Unsupported(format!("bit depth {other}")));
            }
        };

        let n = self.width as usize * self.height as usize;
        if sample == SampleType::Packed565 {
            // Already packed into plane 0 by the clipping stage.
            let mut data = Vec::with_capacity(n * 2);
            for i in 0..n {
                data.extend_from_slice(&(self.image_plane[0][i] as u16).to_le_bytes());
            }
            return Ok(PixelBuffer {
                width: self.width,
                height: self.height,
                channels: 1,
                color,
                alpha: AlphaMode::None,
                sample,
                data,
            });
        }

        let mut data = Vec::with_capacity(n * channels * sample.bytes());
        for i in 0..n {
            for plane in self.image_plane.iter().take(channels) {
                let v = plane[i];
                match sample {
                    SampleType::U8 => data.push(v.clamp(0, 255) as u8),
                    SampleType::U16 => {
                        data.extend_from_slice(&(v.clamp(0, 65535) as u16).to_le_bytes())
                    }
                    SampleType::I16 => {
                        data.extend_from_slice(&(v.clamp(-32768, 32767) as i16).to_le_bytes())
                    }
                    SampleType::F16 => data.extend_from_slice(&(v as u16).to_le_bytes()),
                    SampleType::I32 | SampleType::F32 => data.extend_from_slice(&v.to_le_bytes()),
                    SampleType::Packed565 => unreachable!(),
                }
            }
        }
        Ok(PixelBuffer {
            width: self.width,
            height: self.height,
            channels: channels as u8,
            color,
            alpha,
            sample,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decoder::DecodeTiming;

    fn img(planes: Vec<Vec<i32>>, clr: u8, bd: u8, has_alpha: bool) -> DecodedImage {
        DecodedImage {
            width: 2,
            height: 1,
            num_components: planes.len(),
            image_plane: planes,
            output_clr_fmt: clr,
            output_bitdepth: bd,
            red_blue_swapped: false,
            has_alpha,
            premultiplied_alpha: false,
            timing: DecodeTiming::default(),
        }
    }

    #[test]
    fn packs_gray8() {
        let b = img(vec![vec![-5, 300]], OUT_YONLY, BD8, false)
            .to_pixel_buffer()
            .unwrap();
        assert_eq!(
            (b.channels, b.sample, b.color),
            (1, SampleType::U8, ColorModel::Gray)
        );
        assert_eq!(b.data, vec![0, 255]); // clamped
    }

    #[test]
    fn packs_rgba16f_bits() {
        let planes = vec![
            vec![0x3c00, 0],
            vec![0, 0],
            vec![0, 0],
            vec![0x3c00, 0x3c00],
        ];
        let mut im = img(planes, OUT_RGB, BD16F, true);
        im.premultiplied_alpha = true;
        let b = im.to_pixel_buffer().unwrap();
        assert_eq!(b.channels, 4);
        assert_eq!(b.alpha, AlphaMode::Premultiplied);
        assert_eq!(b.sample, SampleType::F16);
        assert_eq!(&b.data[..2], &0x3c00u16.to_le_bytes()); // half 1.0 bits kept
    }
}
