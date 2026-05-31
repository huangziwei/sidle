//! Stroke-point value decoder.
//!
//! Each `nmdl.position_x` / `nmdl.position_y` / pressure / tilt / adjust blob is
//! a 2-byte signature `\x01\x01`, a u32-LE count, then a stream of 4-bit
//! instructions (high nibble first; a trailing nibble is zero-padded). Each
//! instruction yields an `increment`; values are reconstructed by **second-order
//! (delta-of-delta) integration**. Verbatim port of `decode_stroke_values`
//! (`ref/scribe-library/kfxlib/yj_to_epub_notebook.py`).

/// Decode `num_points` integer values from a stroke-point blob.
/// Returns `None` if the blob is malformed or truncated.
pub fn decode_stroke_values(data: &[u8], num_points: usize) -> Option<Vec<i64>> {
    if data.len() < 6 || data[0] != 0x01 || data[1] != 0x01 {
        return None;
    }
    let num_vals = u32::from_le_bytes([data[2], data[3], data[4], data[5]]) as usize;
    if num_vals != num_points {
        return None;
    }
    let mut p = 6;

    // Need at least ceil(num_vals/2) instruction nibbles available.
    if (data.len() - p) * 2 < num_vals {
        return None;
    }

    // Read packed 4-bit instructions (high nibble first).
    let mut instrs: Vec<u8> = Vec::with_capacity(num_vals + 1);
    while instrs.len() < num_vals {
        let b = *data.get(p)?;
        p += 1;
        instrs.push(b >> 4);
        instrs.push(b & 0x0f);
    }
    if instrs.len() > num_vals {
        // trailing padding nibble (should be 0)
        instrs.pop();
    }

    let mut vals = Vec::with_capacity(num_vals);
    let mut change: i64 = 0;
    let mut value: i64 = 0;

    for (i, &instr) in instrs.iter().enumerate().take(num_vals) {
        let n = (instr & 3) as usize;

        let mut increment: i64 = if instr & 4 != 0 {
            // small literal 0..3
            n as i64
        } else {
            match n {
                0 => 0,
                1 => {
                    let v = *data.get(p)? as i64;
                    p += 1;
                    v
                }
                2 => {
                    let v = u16::from_le_bytes([*data.get(p)?, *data.get(p + 1)?]) as i64;
                    p += 2;
                    v
                }
                _ => {
                    // n == 3: u8 + (u16 << 8). kfxlib flags this as unexpected
                    // but still decodes it.
                    let lo = *data.get(p)? as i64;
                    p += 1;
                    let hi = u16::from_le_bytes([*data.get(p)?, *data.get(p + 1)?]) as i64;
                    p += 2;
                    lo + (hi << 8)
                }
            }
        };

        if instr & 8 != 0 {
            increment = -increment;
        }

        if i == 0 {
            change = 0;
            value = increment;
        } else {
            change += increment;
            value += change;
        }
        vals.push(value);
    }

    Some(vals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_of_delta_roundtrip() {
        // values [10, 12, 16]: increments [10, +2, +2] (second-order).
        // instr[0]=1 (n=1, read 1 byte 0x0a); instr[1]=instr[2]=6 (literal n=2).
        // packed nibbles: 0x16, 0x60 (last nibble pad); data byte 0x0a.
        let blob = [0x01, 0x01, 0x03, 0x00, 0x00, 0x00, 0x16, 0x60, 0x0a];
        assert_eq!(decode_stroke_values(&blob, 3), Some(vec![10, 12, 16]));
    }

    #[test]
    fn rejects_bad_signature() {
        let blob = [0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(decode_stroke_values(&blob, 0), None);
    }

    #[test]
    fn count_mismatch_is_none() {
        let blob = [0x01, 0x01, 0x03, 0x00, 0x00, 0x00, 0x16, 0x60, 0x0a];
        assert_eq!(decode_stroke_values(&blob, 2), None);
    }
}
