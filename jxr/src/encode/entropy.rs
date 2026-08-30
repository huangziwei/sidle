//! Static Huffman (VLC) encoding — the inverse of the decoder's
//! [`crate::decode::misc::Deserializer::huff`].

use super::bitstream::BitWriter;
use std::collections::HashMap;

/// Emit `value` using a decoder Huffman `table` (the same `code → value` map
/// the decoder reads with `huff`).
pub fn write_huff(bw: &mut BitWriter, table: &HashMap<u64, i32>, value: i32) {
    let code_key = table
        .iter()
        .find_map(|(&k, &v)| (v == value).then_some(k))
        .unwrap_or_else(|| panic!("value {value} not present in huffman table"));
    // The code key is `1` followed by the real code bits; its highest set bit
    // is the synthetic leading 1, so the code length is that bit's position.
    let len = 63 - code_key.leading_zeros();
    let bits = code_key & ((1u64 << len) - 1);
    bw.write_bits(bits, len);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::misc::Deserializer;
    use crate::decode::tables as t;

    /// Every value in `table` must encode then decode back to itself.
    fn roundtrip_all(table: &HashMap<u64, i32>) {
        let mut values: Vec<i32> = table.values().copied().collect();
        values.sort_unstable();
        for v in values {
            let mut bw = BitWriter::new();
            write_huff(&mut bw, table, v);
            let bytes = bw.finish();
            let mut d = Deserializer::new(&bytes);
            assert_eq!(d.huff(table).unwrap(), v, "value {v}");
        }
    }

    #[test]
    fn huff_roundtrips_every_table() {
        roundtrip_all(t::val_dc_yuv());
        for i in 0..2 {
            roundtrip_all(t::num_cbphp(i));
            roundtrip_all(t::num_blkcbphp2(i));
            roundtrip_all(t::abs_level_index(i));
        }
        for i in 0..5 {
            roundtrip_all(t::first_index(i));
        }
        for i in 0..4 {
            roundtrip_all(t::index_a(i));
        }
        roundtrip_all(t::index_b());
        roundtrip_all(t::run_index());
        for mr in 2..=4 {
            roundtrip_all(t::run_value(mr));
        }
        roundtrip_all(t::ref_cbphp1());
        roundtrip_all(t::num_ch_blk());
        roundtrip_all(t::chr_cbphp());
        roundtrip_all(t::cbplp_yuv1_444());
        roundtrip_all(t::cbplp_yuv1_42x());
    }

    #[test]
    fn huff_sequence_roundtrips() {
        // Codes written back-to-back (no alignment) decode in order — prefix
        // codes are self-terminating, so the decoder stops at each boundary.
        let table = t::first_index(2);
        let vals: Vec<i32> = (0..12).chain(0..12).collect();
        let mut bw = BitWriter::new();
        for &v in &vals {
            write_huff(&mut bw, table, v);
        }
        let bytes = bw.finish();
        let mut d = Deserializer::new(&bytes);
        for &v in &vals {
            assert_eq!(d.huff(table).unwrap(), v);
        }
    }
}
