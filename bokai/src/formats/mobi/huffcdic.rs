//! HUFF/CDIC decompression for MOBI files
//!
//! Some MOBI files use Huffman compression instead of PalmDOC LZ77.

use std::io;

/// Dictionary entry: (slice data, is_leaf flag)
#[derive(Clone)]
enum DictEntry {
    Leaf(Vec<u8>),
    Node(Vec<u8>),
    Unpacked(Vec<u8>),
}

/// HUFF/CDIC decompressor
pub struct HuffCdicReader {
    /// dict1: 256 entries of (codelen, term, maxcode). `maxcode` is held in `u64`
    /// because `((maxcode + 1) << (32 - codelen)) - 1` exceeds 32 bits.
    dict1: Vec<(u8, bool, u64)>,
    /// mincode indexed by code length `0..=32` (33 entries: a codelen-0
    /// sentinel plus calibre's `(0,) + dict2[0::2]` layout).
    mincode: Vec<u64>,
    /// maxcode indexed by code length `0..=32` (33 entries; see `mincode`).
    maxcode: Vec<u64>,
    /// Dictionary entries from CDIC records
    dictionary: Vec<DictEntry>,
}

impl HuffCdicReader {
    /// Create a new reader from HUFF and CDIC records
    pub fn new(huff: &[u8], cdics: &[&[u8]]) -> io::Result<Self> {
        let mut reader = Self {
            dict1: Vec::with_capacity(256),
            mincode: Vec::with_capacity(33),
            maxcode: Vec::with_capacity(33),
            dictionary: Vec::new(),
        };

        reader.load_huff(huff)?;
        for cdic in cdics {
            reader.load_cdic(cdic)?;
        }

        Ok(reader)
    }

    fn load_huff(&mut self, huff: &[u8]) -> io::Result<()> {
        // Check header: "HUFF\x00\x00\x00\x18"
        if huff.len() < 24 || &huff[0..8] != b"HUFF\x00\x00\x00\x18" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid HUFF header",
            ));
        }

        let off1 = u32::from_be_bytes([huff[8], huff[9], huff[10], huff[11]]) as usize;
        let off2 = u32::from_be_bytes([huff[12], huff[13], huff[14], huff[15]]) as usize;

        // Load dict1: 256 entries at off1
        if huff.len() < off1 + 256 * 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HUFF dict1 truncated",
            ));
        }

        for i in 0..256 {
            let pos = off1 + i * 4;
            let v = u32::from_be_bytes([huff[pos], huff[pos + 1], huff[pos + 2], huff[pos + 3]]);

            let codelen = (v & 0x1f) as u8;
            let term = (v & 0x80) != 0;
            let maxcode_raw = (v >> 8) as u64;

            // maxcode = ((maxcode_raw + 1) << (32 - codelen)) - 1, in u64 so the
            // shift can't overflow. `maxcode_raw` is only 24 bits, so the result
            // fits comfortably; codelen 0 never occurs in a valid table.
            let maxcode = if codelen > 0 {
                ((maxcode_raw + 1) << (32 - codelen as u32)) - 1
            } else {
                0
            };

            self.dict1.push((codelen, term, maxcode));
        }

        // Load dict2: 64 entries at off2 (32 mincode/maxcode pairs)
        if huff.len() < off2 + 64 * 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HUFF dict2 truncated",
            ));
        }

        // Build mincode/maxcode indexed by code length 0..=32 (33 entries).
        // Faithful port of calibre's `huffcdic.Reader.load_huff`:
        self.mincode.push(0); // codelen 0: 0 << 32
        self.maxcode.push((1u64 << 32) - 1); // codelen 0: ((0 + 1) << 32) - 1
        for codelen in 1u32..=32 {
            let pos = off2 + (codelen as usize - 1) * 8;
            let mincode_raw =
                u32::from_be_bytes([huff[pos], huff[pos + 1], huff[pos + 2], huff[pos + 3]]) as u64;
            let maxcode_raw =
                u32::from_be_bytes([huff[pos + 4], huff[pos + 5], huff[pos + 6], huff[pos + 7]])
                    as u64;

            let shift = 32 - codelen;
            self.mincode.push(mincode_raw << shift);
            self.maxcode.push(((maxcode_raw + 1) << shift) - 1);
        }

        Ok(())
    }

    fn load_cdic(&mut self, cdic: &[u8]) -> io::Result<()> {
        // Check header: "CDIC\x00\x00\x00\x10"
        if cdic.len() < 16 || &cdic[0..8] != b"CDIC\x00\x00\x00\x10" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid CDIC header",
            ));
        }

        let phrases = u32::from_be_bytes([cdic[8], cdic[9], cdic[10], cdic[11]]) as usize;
        let bits = u32::from_be_bytes([cdic[12], cdic[13], cdic[14], cdic[15]]) as usize;

        let n = std::cmp::min(1 << bits, phrases.saturating_sub(self.dictionary.len()));

        // Read offset table
        if cdic.len() < 16 + n * 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "CDIC offset table truncated",
            ));
        }

        for i in 0..n {
            let off_pos = 16 + i * 2;
            let off = u16::from_be_bytes([cdic[off_pos], cdic[off_pos + 1]]) as usize;

            if 16 + off + 2 > cdic.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "CDIC entry truncated",
                ));
            }

            let blen = u16::from_be_bytes([cdic[16 + off], cdic[16 + off + 1]]);
            let slice_len = (blen & 0x7fff) as usize;
            let is_leaf = (blen & 0x8000) != 0;

            let slice_start = 16 + off + 2;
            let slice_end = std::cmp::min(slice_start + slice_len, cdic.len());
            let slice = cdic[slice_start..slice_end].to_vec();

            if is_leaf {
                self.dictionary.push(DictEntry::Leaf(slice));
            } else {
                self.dictionary.push(DictEntry::Node(slice));
            }
        }

        Ok(())
    }

    /// Decompress a text record
    pub fn decompress(&mut self, data: &[u8]) -> io::Result<Vec<u8>> {
        let mut result = Vec::new();
        self.unpack_into(data, &mut result)?;
        Ok(result)
    }

    fn unpack_into(&mut self, data: &[u8], output: &mut Vec<u8>) -> io::Result<()> {
        let bitsleft = data.len() * 8;
        let mut bits_remaining = bitsleft as i64;

        // Pad data for safe reading
        let mut padded = data.to_vec();
        padded.extend_from_slice(&[0u8; 8]);

        let mut pos = 0usize;
        let mut x = read_u64_be(&padded, pos);
        let mut n: i32 = 32;

        while bits_remaining > 0 {
            if n <= 0 {
                pos += 4;
                x = read_u64_be(&padded, pos);
                n += 32;
            }

            let code = ((x >> n) & 0xFFFFFFFF) as u32;

            // Look up in dict1 using top 8 bits.
            let (mut codelen, term, mut maxcode) = self.dict1[(code >> 24) as usize];

            if !term {
                // Walk up the code lengths. mincode/maxcode are indexed by
                // codelen 0..=31; the 32-bit `code` is compared in u64 space
                // because the thresholds routinely exceed 32 bits.
                while (codelen as usize) < self.mincode.len()
                    && (code as u64) < self.mincode[codelen as usize]
                {
                    codelen += 1;
                }
                match self.maxcode.get(codelen as usize) {
                    Some(&m) => maxcode = m,
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "HUFF code length out of range",
                        ));
                    }
                }
            }

            n -= codelen as i32;
            bits_remaining -= codelen as i64;

            if bits_remaining < 0 {
                break;
            }

            // Dictionary index: (maxcode - code) >> (32 - codelen).
            let r = if codelen > 0 {
                (maxcode.wrapping_sub(code as u64) >> (32 - codelen as u32)) as usize
            } else {
                0
            };

            if r >= self.dictionary.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Dictionary index {} out of bounds (len {})",
                        r,
                        self.dictionary.len()
                    ),
                ));
            }

            // Get the slice, unpacking recursively if needed
            match &self.dictionary[r] {
                DictEntry::Leaf(slice) => {
                    output.extend_from_slice(slice);
                }
                DictEntry::Node(slice) => {
                    // Need to recursively unpack
                    let slice_copy = slice.clone();
                    let mut unpacked = Vec::new();
                    self.unpack_into(&slice_copy, &mut unpacked)?;
                    output.extend_from_slice(&unpacked);
                    self.dictionary[r] = DictEntry::Unpacked(unpacked);
                }
                DictEntry::Unpacked(slice) => {
                    output.extend_from_slice(slice);
                }
            }
        }

        Ok(())
    }
}

fn read_u64_be(data: &[u8], pos: usize) -> u64 {
    if pos + 8 <= data.len() {
        u64::from_be_bytes([
            data[pos],
            data[pos + 1],
            data[pos + 2],
            data[pos + 3],
            data[pos + 4],
            data[pos + 5],
            data[pos + 6],
            data[pos + 7],
        ])
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_u64_be() {
        let data = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
        assert_eq!(read_u64_be(&data, 0), 1);

        let data2 = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(read_u64_be(&data2, 0), 0x0100000000000000);
    }

    /// Build a minimal valid HUFF record with uniform 8-bit codes.
    /// All 256 byte values map through 8-bit Huffman codes to identity.
    fn make_huff() -> Vec<u8> {
        let off1: u32 = 24;
        let off2: u32 = 24 + 256 * 4; // 1048

        let mut huff = vec![0u8; 24];
        huff[0..8].copy_from_slice(b"HUFF\x00\x00\x00\x18");
        huff[8..12].copy_from_slice(&off1.to_be_bytes());
        huff[12..16].copy_from_slice(&off2.to_be_bytes());

        // Table 1: 256 entries. All uniform 8-bit, term=true.
        let entry = 0x0000FF88u32;
        for _ in 0..256 {
            huff.extend_from_slice(&entry.to_be_bytes());
        }

        // Table 2: 32 pairs of (mincode_raw, maxcode_raw)
        for i in 0..32u32 {
            if i + 1 == 8 {
                huff.extend_from_slice(&0u32.to_be_bytes()); // min=0
                huff.extend_from_slice(&255u32.to_be_bytes()); // max=255
            } else {
                huff.extend_from_slice(&[0u8; 8]); // unused
            }
        }

        huff
    }

    /// Build a minimal valid CDIC record with 4 single-byte leaf entries.
    fn make_cdic() -> Vec<u8> {
        let num_phrases: u32 = 4;
        let bits: u32 = 2; // 1 << 2 = 4 entries max per CDIC

        let mut cdic = vec![0u8; 16];
        cdic[0..8].copy_from_slice(b"CDIC\x00\x00\x00\x10");
        cdic[8..12].copy_from_slice(&num_phrases.to_be_bytes());
        cdic[12..16].copy_from_slice(&bits.to_be_bytes());

        // Offset table: 4 entries x 2 bytes = 8 bytes
        // Entry data starts at offset 8 (relative to byte 16)
        let offset_table_size = 4 * 2; // 8
        for i in 0..4u16 {
            let offset = offset_table_size as u16 + i * 3;
            cdic.extend_from_slice(&offset.to_be_bytes());
        }

        // Entries: each is leaf (0x8001) + one byte
        for i in 0..4u8 {
            cdic.extend_from_slice(&0x8001u16.to_be_bytes()); // leaf, len=1
            cdic.push(i);
        }

        cdic
    }

    #[test]
    fn test_load_huff_valid() {
        let huff = make_huff();
        let mut reader = HuffCdicReader {
            dict1: Vec::new(),
            mincode: Vec::new(),
            maxcode: Vec::new(),
            dictionary: Vec::new(),
        };
        assert!(reader.load_huff(&huff).is_ok());
        assert_eq!(reader.dict1.len(), 256);
    }

    #[test]
    fn test_load_huff_rejects_bad_magic() {
        let mut reader = HuffCdicReader {
            dict1: Vec::new(),
            mincode: Vec::new(),
            maxcode: Vec::new(),
            dictionary: Vec::new(),
        };
        // JPEG SOI marker — not HUFF
        assert!(reader.load_huff(b"\xFF\xD8\xFF\xE0JFIF").is_err());
    }

    #[test]
    fn test_load_cdic_valid() {
        let cdic = make_cdic();
        let mut reader = HuffCdicReader {
            dict1: Vec::new(),
            mincode: Vec::new(),
            maxcode: Vec::new(),
            dictionary: Vec::new(),
        };
        assert!(reader.load_cdic(&cdic).is_ok());
        assert_eq!(reader.dictionary.len(), 4);
    }

    #[test]
    fn test_load_cdic_rejects_jpeg_bytes() {
        // A caller that reads one record past the CDICs hands over an
        // image; `load_cdic` must reject it, because 0xFF 0xD8 is not
        // "CDIC".
        let jpeg = b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00";
        let mut reader = HuffCdicReader {
            dict1: Vec::new(),
            mincode: Vec::new(),
            maxcode: Vec::new(),
            dictionary: Vec::new(),
        };
        let err = reader.load_cdic(jpeg).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("Invalid CDIC header"),
            "Expected 'Invalid CDIC header', got: {}",
            err
        );
    }

    #[test]
    fn test_reader_new_with_valid_cdics() {
        let huff = make_huff();
        let cdic = make_cdic();
        // Correct usage: only CDIC records passed
        let result = HuffCdicReader::new(&huff, &[cdic.as_slice()]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_reader_new_with_poison_record_fails() {
        // An extra non-CDIC record (JPEG image bytes) after the real
        // CDICs must fail the whole load, not be skipped.
        let huff = make_huff();
        let cdic = make_cdic();
        let jpeg = b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01\x01\x00";

        match HuffCdicReader::new(&huff, &[cdic.as_slice(), jpeg.as_slice()]) {
            Err(e) => assert!(
                e.to_string().contains("Invalid CDIC header"),
                "Expected 'Invalid CDIC header', got: {e}"
            ),
            Ok(_) => panic!("Should fail when JPEG bytes are passed as CDIC"),
        }
    }
}
