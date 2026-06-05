//! MSB-first bit writer — the encode-side mirror of the decoder's
//! [`crate::image::jxr_decode::misc::Deserializer`] bit reader. Anything
//! written here reads back identically via `Deserializer::unpack_bits` /
//! `huff` (proven in tests).

/// Accumulates bits MSB-first and flushes whole bytes as they fill. `cur`
/// holds `nbits` pending bits in its low positions (always `< 8` between
/// writes).
#[derive(Default)]
pub struct BitWriter {
    out: Vec<u8>,
    cur: u64,
    nbits: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append the low `size` bits of `value`, most-significant first. `size`
    /// must be `<= 56` so it fits alongside the `< 8` pending bits in a `u64`.
    pub fn write_bits(&mut self, value: u64, size: u32) {
        debug_assert!(size <= 56, "write_bits size {size} > 56");
        let masked = if size == 0 { 0 } else { value & ((1u64 << size) - 1) };
        self.cur = (self.cur << size) | masked;
        self.nbits += size;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.out.push((self.cur >> self.nbits) as u8);
        }
        // Drop already-emitted high bits so `cur` can't overflow on the next shift.
        self.cur &= if self.nbits == 0 {
            0
        } else {
            (1u64 << self.nbits) - 1
        };
    }

    /// Write a single flag bit.
    pub fn write_flag(&mut self, b: bool) {
        self.write_bits(b as u64, 1);
    }

    /// Pad the current partial byte to a byte boundary with zero bits — the
    /// encode-side counterpart to the decoder discarding its remainder before
    /// a byte-aligned read.
    pub fn align_to_byte(&mut self) {
        if self.nbits > 0 {
            self.write_bits(0, 8 - self.nbits);
        }
    }

    /// Total bits written so far (including pending fractional bits).
    pub fn bit_len(&self) -> usize {
        self.out.len() * 8 + self.nbits as usize
    }

    /// Finish, padding the final partial byte with zeros, and return the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.align_to_byte();
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::jxr_decode::misc::Deserializer;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
    }

    #[test]
    fn bits_roundtrip_via_decoder() {
        let mut r = Lcg(0x1234_5678);
        for _ in 0..3000 {
            let n = (r.next() % 24 + 1) as usize;
            let mut fields: Vec<(u64, u32)> = Vec::with_capacity(n);
            let mut bw = BitWriter::new();
            for _ in 0..n {
                let size = (r.next() % 32 + 1) as u32; // 1..=32
                let value = r.next() & ((1u64 << size) - 1);
                fields.push((value, size));
                bw.write_bits(value, size);
            }
            let bytes = bw.finish();
            let mut d = Deserializer::new(&bytes);
            for (value, size) in fields {
                assert_eq!(d.unpack_bits(size).unwrap(), value);
            }
        }
    }

    #[test]
    fn align_pads_with_zeros() {
        let mut bw = BitWriter::new();
        bw.write_bits(0b101, 3);
        bw.align_to_byte();
        bw.write_bits(0xAB, 8);
        let bytes = bw.finish();
        assert_eq!(bytes, vec![0b1010_0000, 0xAB]);
    }

    #[test]
    fn flag_writes_single_bits() {
        let mut bw = BitWriter::new();
        for b in [true, false, true, true, false, false, false, true] {
            bw.write_flag(b);
        }
        assert_eq!(bw.finish(), vec![0b1011_0001]);
    }
}
