//! TIFF-like JPEG-XR container writer — the inverse of

/// Microsoft JXR pixel-format GUIDs (on-disk byte order). Match
pub mod pixel_format {
    const fn guid(last: u8) -> [u8; 16] {
        [
            0x24, 0xc3, 0xdd, 0x6f, 0x03, 0x4e, 0xfe, 0x4b, 0xb1, 0x85, 0x3d, 0x77, 0x76, 0x8d,
            0xc9, last,
        ]
    }

    /// `8bppGray` — single 8-bit luma plane.
    pub const GRAY8: [u8; 16] = guid(0x08);
    /// `24bppRGB` — three 8-bit channels.
    pub const RGB24: [u8; 16] = guid(0x0d);
    /// `32bppBGRA` — RGB + straight alpha. The GUID JxrEncApp mints for 8-bit
    /// RGB-with-alpha (`-c 9`); there is no in-family plain-RGBA32. Pairs with
    /// `red_blue_not_swapped_flag = 0` exactly like [`RGB24`].
    pub const BGRA32: [u8; 16] = guid(0x0f);
    /// `32bppPBGRA` — RGB + **premultiplied** alpha (pairs with
    /// `premultiplied_alpha_flag = 1`).
    pub const PBGRA32: [u8; 16] = guid(0x10);
    /// `16bppGray` — single 16-bit unsigned luma plane (BD16).
    pub const GRAY16: [u8; 16] = guid(0x0b);
    /// `48bppRGB` — three 16-bit unsigned channels (BD16).
    pub const RGB48: [u8; 16] = guid(0x15);
    /// `16bppGrayFixed` — single signed 16-bit fixed-point plane (BD16S).
    pub const GRAY16_FIXED: [u8; 16] = guid(0x13);
    /// `48bppRGBFixed` — three signed 16-bit fixed-point channels (BD16S).
    pub const RGB48_FIXED: [u8; 16] = guid(0x12);
    /// `32bppGrayFixed` — single signed 32-bit fixed-point plane (BD32S).
    pub const GRAY32_FIXED: [u8; 16] = guid(0x3f);
    /// `96bppRGBFixed` — three signed 32-bit fixed-point channels (BD32S).
    pub const RGB96_FIXED: [u8; 16] = guid(0x18);
    /// `16bppGrayHalf` — single IEEE-754 half plane (BD16F).
    pub const GRAY16_HALF: [u8; 16] = guid(0x3e);
    /// `48bppRGBHalf` — three IEEE-754 half channels (BD16F).
    pub const RGB48_HALF: [u8; 16] = guid(0x3b);
    /// `32bppRGBE` — Radiance shared-exponent HDR (BD8 + `OUT_RGBE`).
    pub const RGBE32: [u8; 16] = guid(0x3d);
    /// `64bppRGBA` — four 16-bit unsigned channels (BD16 + alpha plane).
    pub const RGBA64: [u8; 16] = guid(0x16);
    /// `64bppPRGBA` — premultiplied [`RGBA64`].
    pub const PRGBA64: [u8; 16] = guid(0x17);
    /// `64bppRGBAFixedPoint` — four signed 16-bit channels (BD16S + alpha).
    pub const RGBA64_FIXED: [u8; 16] = guid(0x1d);
    /// `128bppRGBAFixedPoint` — four signed 32-bit channels (BD32S + alpha).
    pub const RGBA128_FIXED: [u8; 16] = guid(0x1e);
    /// `64bppRGBAHalf` — four IEEE-754 half channels (BD16F + alpha).
    pub const RGBA64_HALF: [u8; 16] = guid(0x3a);
    /// `128bppRGBAFloat` — four IEEE-754 single channels (BD32F + alpha).
    pub const RGBA128_FLOAT: [u8; 16] = guid(0x19);
    /// `128bppPRGBAFloat` — premultiplied [`RGBA128_FLOAT`].
    pub const PRGBA128_FLOAT: [u8; 16] = guid(0x1a);
    /// `32bppGrayFloat` — single IEEE-754 single plane (BD32F).
    pub const GRAY32_FLOAT: [u8; 16] = guid(0x11);
    /// `128bppRGBFloat` — three IEEE-754 single channels, container stride
    /// padded to 4 (the format the reference encoder mints for RGB float;
    /// the codestream itself carries 3 components).
    pub const RGB128_FLOAT: [u8; 16] = guid(0x1b);
    /// `32bppCMYK` — four 8-bit ink channels (BD8 + `OUT_CMYK`).
    pub const CMYK32: [u8; 16] = guid(0x1c);
    /// `64bppCMYK` — four 16-bit ink channels (BD16 + `OUT_CMYK`).
    pub const CMYK64: [u8; 16] = guid(0x1f);
    /// `40bppCMYKAlpha` — [`CMYK32`] + an 8-bit alpha image plane.
    pub const CMYKA40: [u8; 16] = guid(0x2c);
    /// `80bppCMYKAlpha` — [`CMYK64`] + a 16-bit alpha image plane.
    pub const CMYKA80: [u8; 16] = guid(0x2d);
    /// `BlackWhite` — bi-level (BD1WHITE1 or BD1BLACK1 in the codestream).
    pub const BLACKWHITE: [u8; 16] = guid(0x05);
    /// `16bppRGB555` — packed 5-5-5 (BD5).
    pub const RGB555: [u8; 16] = guid(0x09);
    /// `16bppRGB565` — packed 5-6-5 (BD565).
    pub const RGB565: [u8; 16] = guid(0x0a);
    /// `32bppRGB101010` — packed 10-10-10 (BD10).
    pub const RGB101010: [u8; 16] = guid(0x14);
    /// `xxbpp[3–8]Channels[Alpha]` — `OUT_NCOMPONENT` with `n` channels at
    /// 8 bits (base `0x20`; `0x2e` with an alpha plane) or 16 bits (`0x26`;
    /// `0x34` with alpha). The GUID family stops at 8 channels.
    pub const fn nchannel(n: usize, deep: bool, alpha: bool) -> [u8; 16] {
        let base = match (deep, alpha) {
            (false, false) => 0x20u8,
            (true, false) => 0x26,
            (false, true) => 0x2e,
            (true, true) => 0x34,
        };
        guid(base + (n as u8 - 3))
    }
}

const TIFF_TYPE_BYTE: u16 = 1;
const TIFF_TYPE_LONG: u16 = 4;
const TIFF_TYPE_FLOAT: u16 = 11;

/// 96-dpi resolution Amazon stamps on every plate (`96.0` as IEEE-754 FLOAT).
const RESOLUTION_96DPI: u32 = 0x42c0_0000; // 96.0f32.to_bits()

/// Wrap `codestream` in a JXR TIFF container for a `width`×`height` image of
pub fn write_container(
    codestream: &[u8],
    width: u32,
    height: u32,
    pixel_format: &[u8; 16],
) -> Vec<u8> {
    const HEADER_LEN: u32 = 8; // II-BC-01 (4) + ifd_offset (4)
    const NUM_ENTRIES: u16 = 8;
    // IFD: count(2) + entries(N*12) + next_ifd(4)
    const IFD_LEN: u32 = 2 + NUM_ENTRIES as u32 * 12 + 4;

    let ifd_offset = HEADER_LEN;
    let uuid_offset = ifd_offset + IFD_LEN;
    let codestream_offset = uuid_offset + 16;

    let mut out: Vec<u8> = Vec::with_capacity(codestream_offset as usize + codestream.len());
    out.extend_from_slice(&[0x49, 0x49, 0xBC, 0x01]);
    out.extend_from_slice(&ifd_offset.to_le_bytes());

    // Tags MUST be ascending (TIFF requirement); this order matches Amazon's.
    out.extend_from_slice(&NUM_ENTRIES.to_le_bytes());
    push_entry(&mut out, 0xbc01, TIFF_TYPE_BYTE, 16, uuid_offset); // pixel_format (out-of-line)
    push_entry(&mut out, 0xbc02, TIFF_TYPE_LONG, 1, 0); // transformation (no rotate/flip)
    push_entry(&mut out, 0xbc80, TIFF_TYPE_LONG, 1, width); // image_width
    push_entry(&mut out, 0xbc81, TIFF_TYPE_LONG, 1, height); // image_height
    push_entry(&mut out, 0xbc82, TIFF_TYPE_FLOAT, 1, RESOLUTION_96DPI); // width_res
    push_entry(&mut out, 0xbc83, TIFF_TYPE_FLOAT, 1, RESOLUTION_96DPI); // height_res
    push_entry(&mut out, 0xbcc0, TIFF_TYPE_LONG, 1, codestream_offset); // image_offset
    push_entry(&mut out, 0xbcc1, TIFF_TYPE_LONG, 1, codestream.len() as u32); // image_byte_count
    out.extend_from_slice(&0u32.to_le_bytes()); // next IFD offset = none

    out.extend_from_slice(pixel_format);
    out.extend_from_slice(codestream);
    out
}

fn push_entry(out: &mut Vec<u8>, tag: u16, field_type: u16, count: u32, value_or_offset: u32) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&field_type.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&value_or_offset.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::container::parse;

    #[test]
    fn container_roundtrips_via_decoder_parse() {
        let codestream: Vec<u8> = (0..137u32).map(|i| (i * 7 % 256) as u8).collect();
        let bytes = write_container(&codestream, 640, 480, &pixel_format::GRAY8);
        let parsed = parse(&bytes).expect("decoder parses our container");
        assert_eq!(parsed.image_width, 640);
        assert_eq!(parsed.image_height, 480);
        assert_eq!(
            parsed.pixel_format_uuid,
            "24c3dd6f-034e-fe4b-b185-3d77768dc908"
        );
        assert_eq!(parsed.image_data, &codestream[..]);
    }

    #[test]
    fn container_roundtrips_rgb24_via_decoder_parse() {
        let codestream: Vec<u8> = (0..200u32).map(|i| (i * 13 % 256) as u8).collect();
        let bytes = write_container(&codestream, 1351, 1920, &pixel_format::RGB24);
        let parsed = parse(&bytes).expect("decoder parses our RGB24 container");
        assert_eq!((parsed.image_width, parsed.image_height), (1351, 1920));
        assert_eq!(
            parsed.pixel_format_uuid,
            "24c3dd6f-034e-fe4b-b185-3d77768dc90d"
        );
        assert_eq!(parsed.image_data, &codestream[..]);
    }
}
