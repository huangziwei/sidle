//! HP band encoder — inverse of the decoder's `mb_cbphp` + `mb_hp_flex` for
//! grayscale ALL_BANDS. The coefficient is `(vlc << model_bits) + flex`.

use super::bitstream::BitWriter;
use super::coeff;
use crate::decode::consts::*;
use crate::decode::math::num_ones;
use crate::decode::state::{AdaptiveScan, AdaptiveVLC, CBPHPModel};
use crate::decode::tables;

// From the decoder's `mb_cbphp`.
const I_OUT: [i32; 16] = [0, 15, 3, 12, 1, 2, 4, 8, 5, 6, 9, 10, 7, 11, 13, 14];
const I_OFF: [i32; 6] = [0, 4, 2, 8, 12, 1];
const I_FLC: [u32; 6] = [0, 2, 1, 2, 2, 0];

/// All adaptive state the HP band threads across macroblocks.
pub struct HpState {
    pub model: coeff::ModelState,
    pub first_ind: AdaptiveVLC,
    pub ind0: AdaptiveVLC,
    pub ind1: AdaptiveVLC,
    pub abs0: AdaptiveVLC,
    pub abs1: AdaptiveVLC,
    pub hor_scan: AdaptiveScan,
    pub ver_scan: AdaptiveScan,
    pub num_cbphp: AdaptiveVLC,
    pub num_blk_cbphp: AdaptiveVLC,
    pub cbphp_model: CBPHPModel,
}

impl HpState {
    pub fn new() -> Self {
        let mut s = HpState {
            model: coeff::ModelState::init(2), // HP band ⇒ m_bits = 0
            first_ind: AdaptiveVLC::default(),
            ind0: AdaptiveVLC::default(),
            ind1: AdaptiveVLC::default(),
            abs0: AdaptiveVLC::default(),
            abs1: AdaptiveVLC::default(),
            hor_scan: AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_H_PRIME),
            ver_scan: AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_V_PRIME),
            num_cbphp: AdaptiveVLC::default(),
            num_blk_cbphp: AdaptiveVLC::default(),
            cbphp_model: CBPHPModel::default(),
        };
        s.first_ind.init_table2();
        s.ind0.init_table2();
        s.ind1.init_table2();
        s.abs0.init_table1();
        s.abs1.init_table1();
        s.num_cbphp.init_table1();
        s.num_blk_cbphp.init_table1();
        // The cbphp model's initialize_context values.
        s.cbphp_model.cbphp_state = [0, 0];
        s.cbphp_model.count_ones = [-4, -4];
        s.cbphp_model.count_zeroes = [4, 4];
        s
    }

    pub fn adapt(&mut self) {
        self.first_ind.adapt_table2(4);
        self.ind0.adapt_table2(3);
        self.ind1.adapt_table2(3);
        self.abs0.adapt_table1();
        self.abs1.adapt_table1();
        self.num_cbphp.adapt_table1();
        self.num_blk_cbphp.adapt_table1();
    }
}

/// Reverse the `pred_cbphp_444` cascade (a chain of self-inverse XORs).
pub(crate) fn unpredict_cascade(mut x: i32) -> i32 {
    x ^= (x & 0x3300) << 2;
    x ^= (x & 0x00CC) << 6;
    x ^= (x & 0x33) << 2;
    x ^= 0x20 & (x << 1);
    x ^= 0x10 & (x << 3);
    x ^= 0x02 & (x << 1);
    x
}

/// Encode `mb_cbphp` (the 16-bit per-block HP coded-block-pattern) — inverse of
/// `mb_cbphp` + `pred_cbphp_444`. `neighbor_bit` is the predicted edge bit.
#[allow(clippy::too_many_arguments)]
pub fn encode_cbphp(bw: &mut BitWriter, st: &mut HpState, mb_cbphp: i32, neighbor_bit: i32) {
    // ---- undo prediction → i_diff ----
    let state = st.cbphp_model.cbphp_state[0];
    let i_diff = if state == 0 {
        unpredict_cascade(mb_cbphp) ^ neighbor_bit
    } else if state == 2 {
        mb_cbphp ^ 0xFFFF
    } else {
        mb_cbphp
    };

    // ---- code i_diff: 4 nibbles, one per i_block group ----
    let nibbles = [
        i_diff & 0xF,
        (i_diff >> 4) & 0xF,
        (i_diff >> 8) & 0xF,
        (i_diff >> 12) & 0xF,
    ];
    let i_cbphp = (0..4).fold(0i32, |m, b| m | (((nibbles[b] != 0) as i32) << b));

    // num_cbphp = popcount of i_cbphp, then the pattern refinement.
    let num_cbphp = num_ones(i_cbphp as u32) as i32;
    super::entropy::write_huff(
        bw,
        tables::num_cbphp(st.num_cbphp.table_index as usize),
        num_cbphp,
    );
    st.num_cbphp.discrim_val1 +=
        NUM_CBPHP_DELTA[st.num_cbphp.delta_table_index as usize][num_cbphp as usize];
    match num_cbphp {
        1 => bw.write_bits(i_cbphp.trailing_zeros() as u64, 2),
        2 => super::entropy::write_huff(bw, tables::ref_cbphp1(), i_cbphp),
        3 => bw.write_bits((0x0F ^ i_cbphp).trailing_zeros() as u64, 2),
        _ => {} // 0 and 4 carry no extra bits
    }

    // Each nonzero nibble: num_blk_cbphp + i_out/i_off/i_flc.
    for &nib in nibbles.iter() {
        if nib == 0 {
            continue;
        }
        let i_code = I_OUT.iter().position(|&v| v == nib).unwrap() as i32;
        // i_val with I_OFF[i_val] <= i_code < I_OFF[i_val] + 2^I_FLC[i_val]
        let i_val = (1..=5usize)
            .find(|&v| {
                let lo = I_OFF[v];
                i_code >= lo && i_code < lo + (1 << I_FLC[v])
            })
            .unwrap();
        let num_blk = (i_val - 1) as i32;
        super::entropy::write_huff(
            bw,
            tables::num_cbphp(st.num_blk_cbphp.table_index as usize),
            num_blk,
        );
        st.num_blk_cbphp.discrim_val1 +=
            NUM_BLK_CBPHP_DELTA1[st.num_blk_cbphp.delta_table_index as usize][num_blk as usize];
        if I_FLC[i_val] != 0 {
            bw.write_bits((i_code - I_OFF[i_val]) as u64, I_FLC[i_val]);
        }
    }

    // ---- update cbphp model (uses popcount of the *predicted* value) ----
    let n_orig = num_ones(mb_cbphp as u32) as i32;
    let m = &mut st.cbphp_model;
    m.count_ones[0] = (m.count_ones[0] + n_orig - 3).clamp(-16, 15);
    m.count_zeroes[0] = (m.count_zeroes[0] + (16 - n_orig) - 3).clamp(-16, 15);
    m.cbphp_state[0] = if m.count_ones[0] < 0 {
        if m.count_ones[0] < m.count_zeroes[0] {
            1
        } else {
            2
        }
    } else if m.count_zeroes[0] < 0 {
        2
    } else {
        0
    };
}

/// Encode the HP band of one macroblock — inverse of `mb_cbphp` + `mb_hp_flex` +
pub fn encode_hp_mb(
    sink: &mut super::codestream::Sink,
    st: &mut HpState,
    buf: &[i32; 256],
    mb_dclp: &[i32; 16],
    cbphp_left: i32,
    cbphp_top: i32,
    is_left: bool,
    is_top: bool,
    emit_flex: bool,
    trim: u32,
) -> i32 {
    // HP prediction mode from the LP coefficients.
    let s_hor = mb_dclp[1].abs() + mb_dclp[2].abs() + mb_dclp[3].abs();
    let s_ver = mb_dclp[4].abs() + mb_dclp[8].abs() + mb_dclp[12].abs();
    let mode = if s_hor * 4 < s_ver {
        PREDICT_FROM_TOP
    } else if s_ver * 4 < s_hor {
        PREDICT_FROM_LEFT
    } else {
        NO_PREDICTION
    };

    // HP residuals: forward HP minus the intra-MB prediction.
    let mut res = [[0i32; 16]; 16];
    for blk in 0..16 {
        for pos in 1..16 {
            res[blk][pos] = buf[blk * 16 + pos];
        }
    }
    if mode == PREDICT_FROM_TOP {
        for &blk in &[1usize, 2, 3, 5, 6, 7, 9, 10, 11, 13, 14, 15] {
            for &k in &[2usize, 10, 9] {
                res[blk][k] = buf[blk * 16 + k] - buf[(blk - 1) * 16 + k];
            }
        }
    } else if mode == PREDICT_FROM_LEFT {
        for blk in 4..16 {
            for &k in &[1usize, 5, 6] {
                res[blk][k] = buf[blk * 16 + k] - buf[(blk - 4) * 16 + k];
            }
        }
    }

    // Coarse (vlc) levels and the per-block cbphp bit (in I_HIER_SCAN_ORDER).
    let mb = st.model.m_bits as u32;
    let mut coarse = [[0i32; 16]; 16];
    for blk in 0..16 {
        for pos in 1..16 {
            let r = res[blk][pos];
            let c = (r.unsigned_abs() >> mb) as i32;
            coarse[blk][pos] = if r < 0 { -c } else { c };
        }
    }
    let mut mb_cbphp = 0i32;
    for k in 0..16 {
        let blk = I_HIER_SCAN_ORDER[k];
        if (1..16).any(|pos| coarse[blk][pos] != 0) {
            mb_cbphp |= 1 << k;
        }
    }

    let neighbor_bit = if is_left {
        if is_top { 1 } else { (cbphp_top >> 10) & 1 }
    } else {
        (cbphp_left >> 5) & 1
    };
    encode_cbphp(sink.hp(), st, mb_cbphp, neighbor_bit);

    // Per-block: run-level coarse (if not skipped) then flexbits (every block).
    let mut lap = 0i32;
    let mut cbp = mb_cbphp;
    for k in 0..16 {
        let blk = I_HIER_SCAN_ORDER[k];
        if cbp & 1 != 0 {
            let pairs = {
                let scan = if mode == PREDICT_FROM_TOP {
                    &mut st.ver_scan
                } else {
                    &mut st.hor_scan
                };
                let mut pairs: Vec<(u32, i32)> = Vec::new();
                let mut run = 0u32;
                for i in 1..16usize {
                    let pos = scan.translate(i);
                    if coarse[blk][pos] != 0 {
                        pairs.push((run, coarse[blk][pos]));
                        run = 0;
                        scan.adapt(i);
                    } else {
                        run += 1;
                    }
                }
                pairs
            };
            lap += pairs.len() as i32;
            coeff::encode_block(
                sink.hp(),
                &pairs,
                1,
                &mut st.first_ind,
                &mut st.ind0,
                &mut st.ind1,
                &mut st.abs0,
                &mut st.abs1,
            );
        }
        cbp >>= 1;
        if mb > 0 {
            for &n in &I_TRANSPOSE_FLEX[1..] {
                if !emit_flex {
                    break;
                }
                coeff::encode_flexbits(sink.flex(), coarse[blk][n], res[blk][n], mb as i32, trim);
            }
        }
    }

    st.model.update(lap, 2);
    mb_cbphp
}
