//! JPEG-XR TIFF-like container parser.
//!
//! Port of calibre's `jxr_container.JXRContainer`. JXR files begin with a
//! "II-BC 01" magic, followed by a single IFD describing one image: pixel
//! format, dimensions, and a pointer to the WMPHOTO codestream. We extract
//! the codestream bytes and pass them to the image decoder.

use super::misc::{Deserializer, DeserializerError};

#[derive(Debug)]
pub enum ContainerError {
    BadSignature(String),
    MissingField(&'static str),
    UnsupportedPixelFormat(String),
    MultipleImages,
    Truncated,
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
    pub image_width: u32,
    pub image_height: u32,
    pub pixel_format_uuid: String,
    /// WMPHOTO codestream bytes.
    pub image_data: &'a [u8],
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
        3 => field_data.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]]) as u64),
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

const SUPPORTED_UUIDS: &[(&str, &str)] = &[
    ("24c3dd6f-034e-fe4b-b185-3d77768dc905", "BlackWhite"),
    ("24c3dd6f-034e-fe4b-b185-3d77768dc908", "8bppGray"),
    ("24c3dd6f-034e-fe4b-b185-3d77768dc90b", "16bppGray"),
    ("24c3dd6f-034e-fe4b-b185-3d77768dc90c", "24bppBGR"),
    ("24c3dd6f-034e-fe4b-b185-3d77768dc90d", "24bppRGB"),
    ("24c3dd6f-034e-fe4b-b185-3d77768dc90f", "32bppRGBA"),
    ("24c3dd6f-034e-fe4b-b185-3d77768dc920", "24bpp3Channels"),
    ("24c3dd6f-034e-fe4b-b185-3d77768dc921", "32bpp4Channels"),
];

/// Format the 16 bytes of a UUID in the canonical `8-4-4-4-12` form. We have
/// to do this by hand because the Microsoft JXR pixel-format UUID is encoded
/// with a *mixed* endian (first three fields are little-endian, last two are
/// big-endian) — same as Microsoft GUID-on-disk format, which is what
/// Python's `uuid.UUID(bytes=field_data)` produces.
fn format_jxr_uuid(b: &[u8]) -> String {
    // Python's `uuid.UUID(bytes=b)` interprets the bytes as the big-endian
    // 128-bit integer form, then prints canonical 8-4-4-4-12. So we must
    // emit the bytes in straight order. (NOT `bytes_le`.)
    debug_assert_eq!(b.len(), 16);
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

pub fn parse(data: &[u8]) -> std::result::Result<JxrContainer<'_>, ContainerError> {
    let mut ds = Deserializer::new(data);
    let sig = ds.extract(4, true)?;
    if sig != [0x49, 0x49, 0xBC, 0x01] {
        return Err(ContainerError::BadSignature(format!("{:02x?}", sig)));
    }

    let ifd_offset = read_u32_le(&mut ds)?;
    // Skip ahead to ifd_offset.
    if (ifd_offset as usize) < ds.offset {
        return Err(ContainerError::BadSignature("IFD offset before header".into()));
    }
    let skip = ifd_offset as usize - ds.offset;
    let _ = ds.extract(skip, true)?;

    let mut pixel_format: Option<String> = None;
    let mut image_width: Option<u32> = None;
    let mut image_height: Option<u32> = None;
    let mut image_offset: Option<u32> = None;
    let mut image_byte_count: Option<u32> = None;

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
            &data[off..off + field_data_len as usize]
        };

        match field_tag {
            0xbc01 => {
                if field_data.len() < 16 {
                    return Err(ContainerError::MissingField("pixel_format uuid"));
                }
                pixel_format = Some(format_jxr_uuid(&field_data[..16]));
            }
            0xbc80 => image_width = field_value_u64(field_type, field_data).map(|n| n as u32),
            0xbc81 => image_height = field_value_u64(field_type, field_data).map(|n| n as u32),
            0xbcc0 => image_offset = field_value_u64(field_type, field_data).map(|n| n as u32),
            0xbcc1 => image_byte_count = field_value_u64(field_type, field_data).map(|n| n as u32),
            _ => {} // other fields (DPI, transformation, etc.) we don't need
        }
    }

    let pixel_format = pixel_format.ok_or(ContainerError::MissingField("pixel_format"))?;
    let image_width = image_width.ok_or(ContainerError::MissingField("image_width"))?;
    let image_height = image_height.ok_or(ContainerError::MissingField("image_height"))?;
    let image_offset = image_offset.ok_or(ContainerError::MissingField("image_offset"))?;
    let image_byte_count = image_byte_count.unwrap_or(0);

    if !SUPPORTED_UUIDS.iter().any(|(u, _)| *u == pixel_format) {
        return Err(ContainerError::UnsupportedPixelFormat(pixel_format));
    }

    // Multi-image marker.
    let next_ifd_offset = read_u32_le(&mut ds)?;
    if next_ifd_offset != 0 {
        return Err(ContainerError::MultipleImages);
    }

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

    Ok(JxrContainer {
        image_width,
        image_height,
        pixel_format_uuid: pixel_format,
        image_data,
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
