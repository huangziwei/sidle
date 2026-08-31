//! Bit/byte-stream reader for the JXR decoder.

pub struct Deserializer<'a> {
    pub buffer: &'a [u8],
    pub offset: usize,
    bits_remaining: u32,
    remainder: u64,
}

#[derive(Debug)]
pub enum DeserializerError {
    Insufficient(String),
    UnexpectedBits(u32),
    HuffmanFailure,
    Unsupported(String),
}

impl std::fmt::Display for DeserializerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeserializerError::Insufficient(s) => write!(f, "insufficient data: {s}"),
            DeserializerError::UnexpectedBits(n) => write!(f, "unexpected {n} bit(s) remaining"),
            DeserializerError::HuffmanFailure => write!(f, "huffman decode failed"),
            DeserializerError::Unsupported(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for DeserializerError {}

pub type Result<T> = std::result::Result<T, DeserializerError>;

impl<'a> Deserializer<'a> {
    pub fn new(buffer: &'a [u8]) -> Self {
        Self {
            buffer,
            offset: 0,
            bits_remaining: 0,
            remainder: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.buffer.len().saturating_sub(self.offset)
    }

    #[allow(dead_code)] // `len`'s conventional companion
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read `size` bytes from current offset. Errors if too few bytes are
    pub fn extract(&mut self, size: usize, check_remaining: bool) -> Result<&'a [u8]> {
        if check_remaining && self.bits_remaining != 0 {
            return Err(DeserializerError::UnexpectedBits(self.bits_remaining));
        }
        if self.offset + size > self.buffer.len() {
            return Err(DeserializerError::Insufficient(format!(
                "need {} bytes, have {}",
                size,
                self.buffer.len().saturating_sub(self.offset)
            )));
        }
        let out = &self.buffer[self.offset..self.offset + size];
        self.offset += size;
        Ok(out)
    }

    /// Read an unsigned 8-bit value byte-aligned.
    pub fn unpack_u8(&mut self) -> Result<u8> {
        let s = self.extract(1, true)?;
        Ok(s[0])
    }

    /// Read a big-endian unsigned 16-bit value byte-aligned.
    pub fn unpack_u16_be(&mut self) -> Result<u16> {
        let s = self.extract(2, true)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }

    /// Read a big-endian unsigned 32-bit value byte-aligned.
    pub fn unpack_u32_be(&mut self) -> Result<u32> {
        let s = self.extract(4, true)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// Read `size` bits (unsigned) from the bit buffer, refilling 8 bits at a
    /// time from `buffer` as needed.
    pub fn unpack_bits(&mut self, size: u32) -> Result<u64> {
        while self.bits_remaining < size {
            let byte = self.extract(1, false)?[0];
            self.remainder = (self.remainder << 8) | byte as u64;
            self.bits_remaining += 8;
        }
        self.bits_remaining -= size;
        let value = self.remainder >> self.bits_remaining;
        // Mask off the consumed high bits in remainder to keep it small.
        let mask: u64 = if self.bits_remaining == 64 {
            u64::MAX
        } else {
            (1u64 << self.bits_remaining) - 1
        };
        self.remainder &= mask;
        Ok(value)
    }

    /// Convenience: bit-flag (single bit) read.
    pub fn unpack_flag(&mut self) -> Result<bool> {
        Ok(self.unpack_bits(1)? == 1)
    }

    /// Verify a bit-field's value is in `allowed`. Returns the value on
    /// success. Mirrors calibre's `check_bit_field`.
    pub fn check_bit_field(&mut self, size: u32, name: &str, allowed: &[u64]) -> Result<u64> {
        let v = self.unpack_bits(size)?;
        if !allowed.contains(&v) {
            return Err(DeserializerError::Unsupported(format!(
                "{name} = {v} not in {allowed:?}"
            )));
        }
        Ok(v)
    }

    /// Huffman decode. Prefixes a synthetic leading 1 so codes of different
    /// lengths can share a single table; matches calibre's `huff(table, name)`,
    /// which walks the code one bit at a time as the fallback path below does.
    pub fn huff(&mut self, table: &HuffTable) -> Result<i32> {
        // Fast path: every code in these tables is at most 8 bits, so with a
        // full byte in the accumulator one 256-entry lookup resolves the symbol
        // and its length in one step.
        while self.bits_remaining < 8 && self.offset < self.buffer.len() {
            let byte = self.buffer[self.offset];
            self.offset += 1;
            self.remainder = (self.remainder << 8) | byte as u64;
            self.bits_remaining += 8;
        }
        if self.bits_remaining >= 8 {
            let peek = ((self.remainder >> (self.bits_remaining - 8)) & 0xff) as usize;
            let e = table.fast[peek];
            if e != 0 {
                self.bits_remaining -= (e >> 8) as u32;
                self.remainder &= (1u64 << self.bits_remaining) - 1;
                return Ok((e & 0xff) as i32 - 1);
            }
        }
        let mut k: usize = 1;
        while k <= 0xff {
            k = (k << 1) + self.unpack_bits(1)? as usize;
            let v = table.lut[k];
            if v >= 0 {
                return Ok(v as i32);
            }
        }
        Err(DeserializerError::HuffmanFailure)
    }

    /// Drop buffered bits to realign to the next byte boundary.
    ///
    /// `huff` reads ahead a whole byte, so the accumulator can hold bytes that
    /// were fetched but not consumed; rewind `offset` past them. `coded_tiles`
    /// reads `offset` as the stream position at each tile boundary.
    pub fn discard_remainder_bits(&mut self) {
        self.offset -= (self.bits_remaining / 8) as usize;
        self.bits_remaining = 0;
        self.remainder = 0;
    }
}

/// Build a huffman table from a sparse `{bit_string: value}` map. Mirrors
/// calibre's `HBIN`: a binary string like `"010"` becomes the key
/// `int("1" + "010", 2) = 0b1010 = 10`.
pub fn hbin(entries: &[(&str, i32)]) -> HuffTable {
    let mut lut = [-1i8; 512];
    let mut fast = [0u16; 256];
    for (s, v) in entries {
        let mut k: usize = 1;
        for c in s.chars() {
            k = (k << 1) + (if c == '1' { 1 } else { 0 });
        }
        assert!(
            (0..=i8::MAX as i32).contains(v),
            "huffman value {v} does not fit the table's i8 slot"
        );
        lut[k] = *v as i8;
        let len = s.len();
        assert!(len <= 8, "huffman code longer than the 8-bit fast table");
        let bits = k & ((1 << len) - 1);
        let lo = bits << (8 - len);
        for slot in fast.iter_mut().skip(lo).take(1 << (8 - len)) {
            *slot = ((len as u16) << 8) | (*v as u16 + 1);
        }
    }
    HuffTable { lut, fast }
}

/// Flat lookup table for one Huffman code set: index = the accumulated code
/// with its synthetic leading 1; `-1` means "no code ends here".
pub struct HuffTable {
    lut: [i8; 512],
    /// `code << (8 - len)` → `(len << 8) | (value + 1)`; `0` = no code.
    fast: [u16; 256],
}

impl HuffTable {
    /// `(code_key, value)` for every code in the table, ascending by key.
    pub fn iter(&self) -> impl Iterator<Item = (u64, i32)> + '_ {
        self.lut
            .iter()
            .enumerate()
            .filter(|&(_, &v)| v >= 0)
            .map(|(k, &v)| (k as u64, v as i32))
    }

    /// Every value the table can code, ascending by code key.
    #[cfg(test)]
    pub fn values(&self) -> impl Iterator<Item = i32> + '_ {
        self.iter().map(|(_, v)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_basic() {
        let mut d = Deserializer::new(&[1, 2, 3, 4, 5]);
        assert_eq!(d.extract(2, true).unwrap(), &[1, 2]);
        assert_eq!(d.offset, 2);
    }

    #[test]
    fn unpack_bits_crossing_bytes() {
        let mut d = Deserializer::new(&[0b1010_1100, 0b0110_0011]);
        // Read 4 bits => 0b1010 = 10
        assert_eq!(d.unpack_bits(4).unwrap(), 0b1010);
        // Read 6 bits => 0b1100_01 = 49
        assert_eq!(d.unpack_bits(6).unwrap(), 0b110001);
        // 6 bits consumed since byte 1 left, so 6 remaining: 0b10_0011
        assert_eq!(d.unpack_bits(6).unwrap(), 0b100011);
    }

    #[test]
    fn hbin_codes_match_calibre() {
        // From VAL_DC_YUV: "10" -> 0, "001" -> 1, etc.
        let t = hbin(&[("10", 0), ("001", 1), ("00001", 2)]);
        // Prefix '1' added: "110" = 6, "1001" = 9, "100001" = 33
        assert_eq!(t.lut[6], 0);
        assert_eq!(t.lut[9], 1);
        assert_eq!(t.lut[33], 2);
    }

    #[test]
    fn huff_matches_bit_at_a_time_decode() {
        // The 8-bit peek must agree with the one-bit-at-a-time walk for every
        // code in a table, including the 8-bit-long codes.
        let entries: &[(&str, i32)] = &[
            ("1", 0),
            ("01", 1),
            ("001", 2),
            ("0001", 3),
            ("00001", 4),
            ("000001", 5),
            ("0000001", 6),
            ("00000000", 7),
            ("00000001", 8),
        ];
        let t = hbin(entries);
        for (code, want) in entries {
            // Pad past the code so the reader always has a full byte to peek.
            let mut bits: Vec<u8> = code.bytes().map(|b| b - b'0').collect();
            bits.resize(bits.len().div_ceil(8) * 8 + 8, 0);
            let buf: Vec<u8> = bits
                .chunks(8)
                .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b))
                .collect();
            let mut d = Deserializer::new(&buf);
            assert_eq!(d.huff(&t).unwrap(), *want, "code {code}");
        }
    }

    #[test]
    fn discard_remainder_bits_rewinds_read_ahead_bytes() {
        // `huff` fills the accumulator to a whole byte, so after a short code
        // whole bytes can sit unread in it. Discarding must hand `offset` back:
        // `coded_tiles` uses it as the tile-boundary stream position.
        let t = hbin(&[("1", 0)]);
        let buf = [0b0100_0000u8, 0x55, 0xAA];
        let mut d = Deserializer::new(&buf);
        // Leave a partial byte buffered, so `huff`'s refill pulls byte 1 in
        // whole while two bits of byte 0 are still unconsumed.
        assert_eq!(d.unpack_bits(1).unwrap(), 0);
        assert_eq!(d.huff(&t).unwrap(), 0);
        // Two bits consumed, both inside byte 0: realigning lands at byte 1,
        // and byte 1 has been read into the accumulator but not consumed.
        d.discard_remainder_bits();
        assert_eq!(d.offset, 1);
        assert_eq!(d.unpack_u8().unwrap(), 0x55);
    }
}
