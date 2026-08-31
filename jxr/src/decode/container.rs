//! JPEG-XR TIFF-like container parser.

use super::misc::{Deserializer, DeserializerError};

/// Errors from [`parse`] — the outer TIFF-like file is malformed,
/// truncated, or describes something this crate doesn't read.
#[derive(Debug)]
pub enum ContainerError {
    /// The leading `II BC 01` magic is absent.
    BadSignature(String),
    /// A required IFD tag (dimensions, pixel format, codestream pointer)
    /// is missing.
    MissingField(&'static str),
    /// The pixel-format GUID is not in the known table.
    UnsupportedPixelFormat(String),
    /// The file chains more than one IFD (multi-image files not supported).
    MultipleImages,
    /// A declared (offset, length) range runs past the end of the file.
    Truncated,
    /// Raw read error from the byte cursor.
    Bits(DeserializerError),
}

impl std::fmt::Display for ContainerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContainerError::BadSignature(s) => write!(f, "bad TIF signature: {s}"),
            ContainerError::MissingField(s) => write!(f, "missing required field: {s}"),
            ContainerError::UnsupportedPixelFormat(s) => write!(f, "unsupported pixel format: {s}"),
            ContainerError::MultipleImages => write!(f, "multi-image file not supported"),
            ContainerError::Truncated => write!(f, "image data truncated"),
            ContainerError::Bits(e) => write!(f, "bitstream: {e}"),
        }
    }
}

impl std::error::Error for ContainerError {}

impl From<DeserializerError> for ContainerError {
    fn from(e: DeserializerError) -> Self {
        ContainerError::Bits(e)
    }
}

/// What we extract from a JXR container before handing off to the decoder.
pub struct JxrContainer<'a> {
    /// Image width from the `IMAGE_WIDTH` tag.
    pub image_width: u32,
    /// Image height from the `IMAGE_HEIGHT` tag.
    pub image_height: u32,
    /// Pixel-format GUID, lowercase-hex `8-4-4-4-12` form.
    pub pixel_format_uuid: String,
    /// Presentation orientation from the SPATIAL_XFRM_PRIMARY tag (0..7;
    /// 0 = none). NOT auto-applied — see [`crate::decode::apply_orientation`].
    pub orientation: u8,
    /// Resolved pixel-format name (`GUID_PKPixelFormat*` naming, e.g.
    /// "64bppRGBAHalf").
    pub format: &'static str,
    /// WMPHOTO codestream bytes.
    pub image_data: &'a [u8],
    /// Separate planar-alpha codestream (ALPHA_OFFSET/ALPHA_BYTE_COUNT tags),
    /// when present. Decode it as its own image — see [`super::decode_image`].
    pub alpha_data: Option<&'a [u8]>,
    /// Raw ICC profile bytes (tag 0x8773), when present.
    pub icc_profile: Option<&'a [u8]>,
    /// Raw XMP packet bytes (tag 0x02BC), when present.
    pub xmp: Option<&'a [u8]>,
    /// Absolute file offset of the EXIF sub-IFD (tag 0x8769), when present
    /// (a structure, not a blob — exposed as an offset for the caller).
    pub exif_ifd_offset: Option<u32>,
}

/// Map of TIFF field type → bytes per value. From calibre's `FIELD_TYPE_LEN`.
fn field_type_len(t: u16) -> Option<u32> {
    match t {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

/// Read a numeric `field_data` blob as a single integer value, picking the
/// type from `field_type`. Mirrors the `LEN_FMT` table in calibre.
fn field_value_u64(field_type: u16, field_data: &[u8]) -> Option<u64> {
    match field_type {
        1 | 7 => field_data.first().copied().map(u64::from),
        2 => None, // signed char doesn't apply here
        3 => field_data
            .get(..2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as u64),
        4 => field_data
            .get(..4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64),
        6 => field_data.first().copied().map(|b| b as i8 as i64 as u64),
        8 => field_data
            .get(..2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as i64 as u64),
        9 => field_data
            .get(..4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64 as u64),
        _ => None,
    }
}

/// The Microsoft pixel-format GUID family: `24c3dd6f-034e-fe4b-b185-3d77768dc9XX`,
/// last byte selecting the format. 0x0f is `32bppBGRA`; the plain RGBA formats
/// are NOT in this family — see `ODDBALL_UUIDS`.
const FAMILY_PREFIX: &str = "24c3dd6f-034e-fe4b-b185-3d77768dc9";
const FAMILY_FORMATS: &[(u8, &str)] = &[
    (0x05, "BlackWhite"),
    (0x08, "8bppGray"),
    (0x09, "16bppRGB555"),
    (0x0a, "16bppRGB565"),
    (0x0b, "16bppGray"),
    (0x0c, "24bppBGR"),
    (0x0d, "24bppRGB"),
    (0x0e, "32bppBGR"),
    (0x0f, "32bppBGRA"),
    (0x10, "32bppPBGRA"),
    (0x11, "32bppGrayFloat"),
    (0x12, "48bppRGBFixedPoint"),
    (0x13, "16bppGrayFixedPoint"),
    (0x14, "32bppRGB101010"),
    (0x15, "48bppRGB"),
    (0x16, "64bppRGBA"),
    (0x17, "64bppPRGBA"),
    (0x18, "96bppRGBFixedPoint"),
    (0x19, "128bppRGBAFloat"),
    (0x1a, "128bppPRGBAFloat"),
    (0x1b, "128bppRGBFloat"),
    (0x1c, "32bppCMYK"),
    (0x1d, "64bppRGBAFixedPoint"),
    (0x1e, "128bppRGBAFixedPoint"),
    (0x1f, "64bppCMYK"),
    (0x20, "24bpp3Channels"),
    (0x21, "32bpp4Channels"),
    (0x22, "40bpp5Channels"),
    (0x23, "48bpp6Channels"),
    (0x24, "56bpp7Channels"),
    (0x25, "64bpp8Channels"),
    (0x26, "48bpp3Channels"),
    (0x27, "64bpp4Channels"),
    (0x28, "80bpp5Channels"),
    (0x29, "96bpp6Channels"),
    (0x2a, "112bpp7Channels"),
    (0x2b, "128bpp8Channels"),
    (0x2c, "40bppCMYKAlpha"),
    (0x2d, "80bppCMYKAlpha"),
    (0x2e, "32bpp3ChannelsAlpha"),
    (0x2f, "40bpp4ChannelsAlpha"),
    (0x30, "48bpp5ChannelsAlpha"),
    (0x31, "56bpp6ChannelsAlpha"),
    (0x32, "64bpp7ChannelsAlpha"),
    (0x33, "72bpp8ChannelsAlpha"),
    (0x34, "64bpp3ChannelsAlpha"),
    (0x35, "80bpp4ChannelsAlpha"),
    (0x36, "96bpp5ChannelsAlpha"),
    (0x37, "112bpp6ChannelsAlpha"),
    (0x38, "128bpp7ChannelsAlpha"),
    (0x39, "144bpp8ChannelsAlpha"),
    (0x3a, "64bppRGBAHalf"),
    (0x3b, "48bppRGBHalf"),
    (0x3d, "32bppRGBE"),
    (0x3e, "16bppGrayHalf"),
    (0x3f, "32bppGrayFixedPoint"),
    (0x40, "64bppRGBFixedPoint"),
    (0x41, "128bppRGBFixedPoint"),
    (0x42, "64bppRGBHalf"),
    (0x44, "12bppYCC420"),
    (0x45, "16bppYCC422"),
    (0x46, "20bppYCC422"),
    (0x47, "32bppYCC422"),
    (0x48, "24bppYCC444"),
    (0x49, "30bppYCC444"),
];

/// The four formats with GUIDs outside the family (textual form = the
/// on-disk bytes hex'd in order, matching [`format_jxr_uuid`]).
const ODDBALL_UUIDS: &[(&str, &str)] = &[
    ("956b8cd9-fe3e-d647-bb25-eb1748ab0cf1", "32bppRGB"),
    ("2dadc7f5-8d6a-dd43-a7a8-a29935261ae9", "32bppRGBA"),
    ("50a6c43c-27a5-374d-a916-3142c7ebedba", "32bppPRGBA"),
    ("8fd7fee3-dbe8-cf4a-84c1-e97f6136b327", "96bppRGBFloat"),
];

/// Resolve a textual pixel-format UUID to its format name.
pub fn format_name(uuid: &str) -> Option<&'static str> {
    if let Some(suffix) = uuid.strip_prefix(FAMILY_PREFIX) {
        let b = u8::from_str_radix(suffix, 16).ok()?;
        return FAMILY_FORMATS
            .iter()
            .find(|(s, _)| *s == b)
            .map(|(_, n)| *n);
    }
    ODDBALL_UUIDS
        .iter()
        .find(|(u, _)| *u == uuid)
        .map(|(_, n)| *n)
}

/// Format the 16 bytes of a UUID in the canonical `8-4-4-4-12` form. Done by
/// hand: a JXR pixel-format GUID uses Microsoft's on-disk layout, whose first
/// three fields are little-endian and last two big-endian.
fn format_jxr_uuid(b: &[u8]) -> String {
    // The stored bytes are already the big-endian 128-bit integer form the
    // canonical spelling wants, so emit them in straight order — never
    // byte-swapped within the first three groups.
    debug_assert_eq!(b.len(), 16);
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

/// Parse the TIFF-like JXR container: the primary (and any separate-alpha)
/// WMPHOTO codestream, pixel-format GUID, dimensions, and optional ICC/XMP/EXIF
/// ranges. Unknown tags are tolerated; codestream bytes are not touched.
pub fn parse(data: &[u8]) -> std::result::Result<JxrContainer<'_>, ContainerError> {
    let mut ds = Deserializer::new(data);
    let sig = ds.extract(4, true)?;
    if sig != [0x49, 0x49, 0xBC, 0x01] {
        return Err(ContainerError::BadSignature(format!("{:02x?}", sig)));
    }

    let ifd_offset = read_u32_le(&mut ds)?;
    // Skip ahead to ifd_offset.
    if (ifd_offset as usize) < ds.offset {
        return Err(ContainerError::BadSignature(
            "IFD offset before header".into(),
        ));
    }
    let skip = ifd_offset as usize - ds.offset;
    let _ = ds.extract(skip, true)?;

    let mut pixel_format: Option<String> = None;
    let mut orientation: u8 = 0;
    let mut image_width: Option<u32> = None;
    let mut image_height: Option<u32> = None;
    let mut image_offset: Option<u32> = None;
    let mut image_byte_count: Option<u32> = None;
    let mut alpha_offset: Option<u32> = None;
    let mut alpha_byte_count: Option<u32> = None;
    let mut icc_span: Option<(u32, u32)> = None;
    let mut xmp_span: Option<(u32, u32)> = None;
    let mut exif_ifd_offset: Option<u32> = None;

    let num_entries = read_u16_le(&mut ds)? as usize;

    for _ in 0..num_entries {
        let field_tag = read_u16_le(&mut ds)?;
        let field_type = read_u16_le(&mut ds)?;
        let field_count = read_u32_le(&mut ds)?;

        let per = field_type_len(field_type).unwrap_or(0);
        let field_data_len = (per as u64) * (field_count as u64);
        let field_data_owned: Vec<u8>;
        let field_data: &[u8] = if field_data_len <= 4 {
            let s = ds.extract(field_data_len as usize, true)?;
            field_data_owned = s.to_vec();
            // Skip padding so the IFD entry is always 12 bytes.
            let _ = ds.extract(4 - field_data_len as usize, true)?;
            &field_data_owned[..]
        } else {
            let off = read_u32_le(&mut ds)? as usize;
            if off + field_data_len as usize > data.len() {
                return Err(ContainerError::Truncated);
            }
            match field_tag {
                0x8773 => icc_span = Some((off as u32, field_data_len as u32)),
                0x02bc => xmp_span = Some((off as u32, field_data_len as u32)),
                _ => {}
            }
            &data[off..off + field_data_len as usize]
        };

        match field_tag {
            0xbc01 => {
                if field_data.len() < 16 {
                    return Err(ContainerError::MissingField("pixel_format uuid"));
                }
                pixel_format = Some(format_jxr_uuid(&field_data[..16]));
            }
            // A.7.19 SPATIAL_XFRM_PRIMARY: presentation orientation 0..7.
            0xbc02 => {
                orientation = field_value_u64(field_type, field_data).unwrap_or(0).min(7) as u8;
            }
            0xbc80 => image_width = field_value_u64(field_type, field_data).map(|n| n as u32),
            0xbc81 => image_height = field_value_u64(field_type, field_data).map(|n| n as u32),
            0xbcc0 => image_offset = field_value_u64(field_type, field_data).map(|n| n as u32),
            0xbcc1 => image_byte_count = field_value_u64(field_type, field_data).map(|n| n as u32),
            0xbcc2 => alpha_offset = field_value_u64(field_type, field_data).map(|n| n as u32),
            0xbcc3 => alpha_byte_count = field_value_u64(field_type, field_data).map(|n| n as u32),
            0x8769 => exif_ifd_offset = field_value_u64(field_type, field_data).map(|n| n as u32),
            _ => {} // other fields (DPI etc.) we don't need yet
        }
    }

    let pixel_format = pixel_format.ok_or(ContainerError::MissingField("pixel_format"))?;
    let image_width = image_width.ok_or(ContainerError::MissingField("image_width"))?;
    let image_height = image_height.ok_or(ContainerError::MissingField("image_height"))?;
    let image_offset = image_offset.ok_or(ContainerError::MissingField("image_offset"))?;
    let image_byte_count = image_byte_count.unwrap_or(0);

    let format = match format_name(&pixel_format) {
        Some(n) => n,
        None => return Err(ContainerError::UnsupportedPixelFormat(pixel_format)),
    };

    // Subsequent IFDs (thumbnails/extra images) are tolerated: the primary
    // image is the first IFD per Annex A; we simply don't walk the rest.
    let _next_ifd_offset = read_u32_le(&mut ds)?;

    let image_data: &[u8] = if image_byte_count > 0 {
        let start = image_offset as usize;
        let end = start + image_byte_count as usize;
        if end > data.len() {
            return Err(ContainerError::Truncated);
        }
        &data[start..end]
    } else {
        let start = image_offset as usize;
        if start > data.len() {
            return Err(ContainerError::Truncated);
        }
        &data[start..]
    };

    let alpha_data: Option<&[u8]> = match (alpha_offset, alpha_byte_count) {
        (Some(off), count) => {
            let start = off as usize;
            if start > data.len() {
                return Err(ContainerError::Truncated);
            }
            // Tolerate lying byte counts: jxrencapp writes ALPHA_BYTE_COUNT
            // as the total file size; the codestream parser stops at its own
            // end anyway, so clamp to EOF.
            let end = match count {
                Some(c) if c > 0 => (start + c as usize).min(data.len()),
                _ => data.len(),
            };
            Some(&data[start..end])
        }
        _ => None,
    };

    let icc_profile = icc_span.map(|(o, l)| &data[o as usize..(o + l) as usize]);
    let xmp = xmp_span.map(|(o, l)| &data[o as usize..(o + l) as usize]);

    Ok(JxrContainer {
        image_width,
        image_height,
        pixel_format_uuid: pixel_format,
        orientation,
        format,
        image_data,
        alpha_data,
        icc_profile,
        xmp,
        exif_ifd_offset,
    })
}

fn read_u16_le(ds: &mut Deserializer<'_>) -> std::result::Result<u16, DeserializerError> {
    let b = ds.extract(2, true)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32_le(ds: &mut Deserializer<'_>) -> std::result::Result<u32, DeserializerError> {
    let b = ds.extract(4, true)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}
