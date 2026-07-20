//! MOBI/AZW3 format support (read-only).

pub mod fonts;
mod headers;
pub mod huffcdic;
pub(crate) mod index;
pub mod palmdoc;
pub mod parser;
pub mod skeleton;
pub mod tbs;
pub mod writer_transform;

// Transform for reading MOBI/KF8 files
pub mod transform;

// Filepos handling for link resolution
pub mod filepos;

use std::io;

use crate::io::ByteSource;

pub use parser::{
    Compression, Encoding, ExthHeader, HuffCdicReader, MobiFormat, MobiHeader, NULL_INDEX,
    NcxEntry, PdbInfo, TocNode, build_toc_from_ncx, detect_font_type, detect_format,
    detect_image_type, is_metadata_record, parse_exth, parse_fdst, parse_ncx_index, read_index,
    strip_trailing_data,
};

/// Sniff PDB + MOBI header + EXTH from a byte source and classify the format.
///
/// Cheap (reads PDB header + record 0 only). Used by `Book::from_bytes` and
/// `Book::open_format` to route `.mobi` inputs: KF8 / KF8+MOBI6 combo files
/// go through the AZW3 importer (which extracts source CSS, the KF8 spine,
/// proper xml:lang on chapter HTML, and matches what Apple Books / strict
/// EPUB-3 readers need for vertical Japanese text). Pure MOBI6 files keep
/// the single-text-stream MOBI importer.
pub fn sniff_format(source: &dyn ByteSource) -> io::Result<MobiFormat> {
    let file_len = source.len();
    let header_start = source.read_at(0, 78)?;
    if header_start.len() < 78 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "File too short for PDB header",
        ));
    }
    let num_records = u16::from_be_bytes([header_start[76], header_start[77]]) as usize;
    let header_size = 78 + num_records * 8;
    let header_bytes = source.read_at(0, header_size)?;
    let (pdb, _) = PdbInfo::parse(&header_bytes)?;
    if pdb.num_records < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Not enough records",
        ));
    }
    let (start, end) = pdb.record_range(0, file_len)?;
    let record0 = source.read_at(start, (end - start) as usize)?;
    let mobi = MobiHeader::parse(&record0)?;
    let exth = parse_exth(&record0, &mobi);
    Ok(detect_format(&mobi, exth.as_ref()))
}

/// Parse Kindle base32 encoding (0-9A-V) to number.
/// Used for kindle: URI references like kindle:embed:XXXX.
#[inline]
pub fn parse_base32(s: &[u8]) -> usize {
    let mut result = 0usize;
    for &b in s {
        result = result.wrapping_mul(32);
        let val = match b {
            b'0'..=b'9' => (b - b'0') as usize,
            b'A'..=b'V' => (b - b'A') as usize + 10,
            b'a'..=b'v' => (b - b'a') as usize + 10,
            _ => continue,
        };
        result = result.wrapping_add(val);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_base32() {
        // Single digits
        assert_eq!(parse_base32(b"0"), 0);
        assert_eq!(parse_base32(b"1"), 1);
        assert_eq!(parse_base32(b"9"), 9);
        assert_eq!(parse_base32(b"A"), 10);
        assert_eq!(parse_base32(b"V"), 31);

        // Lowercase
        assert_eq!(parse_base32(b"a"), 10);
        assert_eq!(parse_base32(b"v"), 31);

        // Multi-digit
        assert_eq!(parse_base32(b"10"), 32); // 1*32 + 0
        assert_eq!(parse_base32(b"100"), 1024); // 1*32*32 + 0*32 + 0

        // Real kindle embed reference
        assert_eq!(parse_base32(b"0001"), 1);
        assert_eq!(parse_base32(b"000V"), 31);
    }
}
