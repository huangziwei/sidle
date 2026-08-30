//! KF8 embedded-font (`FONT`) record decoding.

use std::io::Read;

use super::parser::detect_font_type;

/// Obfuscation covers only the first 1040 bytes of the payload (Amazon's
/// scheme, same constant in calibre and KindleUnpack).
const XOR_EXTENT: usize = 1040;

/// Refuse to inflate to more than this — a corrupt/hostile size field must
/// not balloon; real embedded fonts are a few MB at most.
const MAX_FONT_SIZE: u32 = 64 * 1024 * 1024;

/// A decoded embedded font: the raw font file plus its sniffed type.
pub struct FontRecord {
    pub data: Vec<u8>,
    /// File extension (`ttf` / `otf` / `woff` / `woff2`), which
    /// `guess_media_type` maps to the EPUB core font media types.
    pub ext: &'static str,
}

/// Decode one `FONT` record into a usable font file. `None` when the record
pub fn parse_font_record(record: &[u8]) -> Option<FontRecord> {
    if record.len() < 24 || &record[..4] != b"FONT" {
        return None;
    }
    let field = |i: usize| -> u32 {
        u32::from_be_bytes([
            record[4 + i * 4],
            record[5 + i * 4],
            record[6 + i * 4],
            record[7 + i * 4],
        ])
    };
    let usize_field = field(0);
    let flags = field(1);
    let dstart = field(2) as usize;
    let xor_len = field(3) as usize;
    let xor_start = field(4) as usize;

    if dstart > record.len() || usize_field > MAX_FONT_SIZE {
        return None;
    }
    let mut payload = record[dstart..].to_vec();

    if flags & 0b10 != 0 && xor_len > 0 {
        let key = record.get(xor_start..xor_start + xor_len)?;
        for (i, b) in payload.iter_mut().take(XOR_EXTENT).enumerate() {
            *b ^= key[i % xor_len];
        }
    }

    let data = if flags & 0b01 != 0 {
        let mut out = Vec::with_capacity(usize_field as usize);
        let mut decoder = flate2::read::ZlibDecoder::new(payload.as_slice());
        decoder
            .by_ref()
            .take(MAX_FONT_SIZE as u64)
            .read_to_end(&mut out)
            .ok()?;
        out
    } else {
        payload
    };

    let ext = detect_font_type(&data)?;
    Some(FontRecord { data, ext })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A minimal fake TrueType payload: sfnt 1.0 magic + filler.
    fn fake_ttf() -> Vec<u8> {
        let mut f = vec![0x00, 0x01, 0x00, 0x00];
        f.extend(
            std::iter::successors(Some(7u8), |n| Some(n.wrapping_mul(31).wrapping_add(11)))
                .take(2000),
        );
        f
    }

    /// Build a FONT record: optional XOR obfuscation + optional zlib.
    fn build_record(font: &[u8], xor_key: Option<&[u8]>, zlib: bool) -> Vec<u8> {
        let mut payload = font.to_vec();
        let mut flags = 0u32;
        if zlib {
            flags |= 0b01;
            let mut enc =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(&payload).unwrap();
            payload = enc.finish().unwrap();
        }
        let key = xor_key.unwrap_or(&[]);
        if !key.is_empty() {
            flags |= 0b10;
            for (i, b) in payload.iter_mut().take(XOR_EXTENT).enumerate() {
                *b ^= key[i % key.len()];
            }
        }
        // Header: FONT + usize + flags + dstart + xor_len + xor_start.
        // Key (when present) sits right after the 24-byte header.
        let dstart = 24 + key.len();
        let mut rec = Vec::new();
        rec.extend_from_slice(b"FONT");
        rec.extend_from_slice(&(font.len() as u32).to_be_bytes());
        rec.extend_from_slice(&flags.to_be_bytes());
        rec.extend_from_slice(&(dstart as u32).to_be_bytes());
        rec.extend_from_slice(&(key.len() as u32).to_be_bytes());
        rec.extend_from_slice(&24u32.to_be_bytes());
        rec.extend_from_slice(key);
        rec.extend_from_slice(&payload);
        rec
    }

    #[test]
    fn test_plain_record_roundtrip() {
        let font = fake_ttf();
        let rec = build_record(&font, None, false);
        let parsed = parse_font_record(&rec).expect("plain record parses");
        assert_eq!(parsed.data, font);
        assert_eq!(parsed.ext, "ttf");
    }

    #[test]
    fn test_xor_and_zlib_roundtrip() {
        // The real-world shape (flags 0b11): zlib first, then the stream's
        // leading bytes XOR-obfuscated — decode order is XOR, then inflate.
        let font = fake_ttf();
        let rec = build_record(&font, Some(b"0123456789abcdefghij"), true);
        let parsed = parse_font_record(&rec).expect("obfuscated record parses");
        assert_eq!(parsed.data, font);
        assert_eq!(parsed.ext, "ttf");
    }

    #[test]
    fn test_otf_sniff() {
        let mut font = b"OTTO".to_vec();
        font.extend_from_slice(&[0u8; 64]);
        let rec = build_record(&font, None, true);
        assert_eq!(parse_font_record(&rec).unwrap().ext, "otf");
    }

    #[test]
    fn test_rejects_garbage() {
        assert!(parse_font_record(b"FONT").is_none(), "truncated header");
        assert!(
            parse_font_record(b"NOTF\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0").is_none(),
            "wrong magic"
        );
        // Well-formed header, payload that isn't a font.
        let rec = build_record(&[0xFFu8; 100], None, false);
        assert!(parse_font_record(&rec).is_none(), "unrecognized payload");
    }
}
