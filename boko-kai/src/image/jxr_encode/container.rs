//! TIFF-like JPEG-XR container writer — the inverse of
//! [`crate::image::jxr_decode::container::parse`]. Wraps a WMPHOTO codestream
//! in the minimal `II-BC-01` outer file with a single IFD describing one
//! image (pixel format, dimensions, codestream pointer).

/// Microsoft JXR pixel-format GUIDs (on-disk byte order). Match
/// `jxr_decode::container::SUPPORTED_UUIDS`.
pub mod pixel_format {
    /// `8bppGray` — single 8-bit luma plane.
    pub const GRAY8: [u8; 16] = [
        0x24, 0xc3, 0xdd, 0x6f, 0x03, 0x4e, 0xfe, 0x4b, 0xb1, 0x85, 0x3d, 0x77, 0x76, 0x8d, 0xc9,
        0x08,
    ];
    /// `24bppRGB` — three 8-bit channels (for the future color path).
    pub const RGB24: [u8; 16] = [
        0x24, 0xc3, 0xdd, 0x6f, 0x03, 0x4e, 0xfe, 0x4b, 0xb1, 0x85, 0x3d, 0x77, 0x76, 0x8d, 0xc9,
        0x0d,
    ];
}

const TIFF_TYPE_BYTE: u16 = 1;
const TIFF_TYPE_LONG: u16 = 4;
const TIFF_TYPE_FLOAT: u16 = 11;

/// 96-dpi resolution Amazon stamps on every plate (`96.0` as IEEE-754 FLOAT).
const RESOLUTION_96DPI: u32 = 0x42c0_0000; // 96.0f32.to_bits()

/// Wrap `codestream` in a JXR TIFF container for a `width`×`height` image of
/// the given `pixel_format` GUID. The IFD mirrors a real Amazon JXR's tag set
/// and order exactly (clone-by-diff): pixel format, transformation, dims,
/// resolution, image offset/byte-count.
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
    use crate::image::jxr_decode::container::parse;

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
