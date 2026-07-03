//! Bit/byte-stream reader for the JXR decoder.
//!
//! Port of calibre's `jxr_misc.Deserializer`. Same semantics: byte-aligned
//! reads via `extract`/`unpack_*`, unaligned reads via `unpack_bits`. The
//! deserializer keeps a bit buffer for fractional reads and panics if a
//! byte-aligned op is requested with bits still pending (matching calibre).

use std::collections::HashMap;

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
    /// available. By default verifies the bit buffer is empty (byte-aligned
    /// read); pass `check_remaining=false` to skip that (used internally by
    /// the bit reader to refill its 8-bit window).
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

    /// Huffman decode: read bits one at a time, prefixing with a synthetic
    /// leading 1 so codes of different lengths can share a single table.
    /// Matches calibre's `huff(table, name)`.
    pub fn huff(&mut self, table: &HashMap<u64, i32>) -> Result<i32> {
        let mut k: u64 = 1;
        while k <= 0xff {
            k = (k << 1) + self.unpack_bits(1)?;
            if let Some(&v) = table.get(&k) {
                return Ok(v);
            }
        }
        Err(DeserializerError::HuffmanFailure)
    }

    pub fn discard_remainder_bits(&mut self) {
        self.bits_remaining = 0;
        self.remainder = 0;
    }
}

/// Build a huffman table from a sparse `{bit_string: value}` map. Mirrors
/// calibre's `HBIN`: a binary string like `"010"` becomes the key
/// `int("1" + "010", 2) = 0b1010 = 10`.
pub fn hbin(entries: &[(&str, i32)]) -> HashMap<u64, i32> {
    let mut out = HashMap::with_capacity(entries.len());
    for (s, v) in entries {
        let mut k: u64 = 1;
        for c in s.chars() {
            k = (k << 1) + (if c == '1' { 1 } else { 0 });
        }
        out.insert(k, *v);
    }
    out
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
        assert_eq!(t.get(&6), Some(&0));
        assert_eq!(t.get(&9), Some(&1));
        assert_eq!(t.get(&33), Some(&2));
    }
}
