//! Forward JPEG-XR core-transform primitives — the encode-side inverses of
//! [`crate::decode::math`].

use crate::decode::consts::MB_PIXEL_MAP;

/// Inverse of [`crate::decode::math::irotate1`].
#[inline]
pub fn fwd_rotate1(a: i32, b: i32) -> (i32, i32) {
    let mut a = a;
    let mut b = b;
    b = b.wrapping_sub((a.wrapping_add(1)) >> 1);
    a = a.wrapping_add((b.wrapping_add(1)) >> 1);
    (a, b)
}

/// Inverse of [`crate::decode::math::irotate2`].
#[inline]
pub fn fwd_rotate2(a: i32, b: i32) -> (i32, i32) {
    let mut a = a;
    let mut b = b;
    b = b.wrapping_sub((a.wrapping_mul(3).wrapping_add(4)) >> 3);
    a = a.wrapping_add((b.wrapping_mul(3).wrapping_add(4)) >> 3);
    (a, b)
}

/// Inverse of [`crate::decode::math::str_dct2x2up`]. Given the
/// decoded `(a',b',c',d')` returns the original `(A,B,C,D)`.
#[inline]
pub fn undo_dct2x2up(ap: i32, bp: i32, cp: i32, dp: i32) -> (i32, i32, i32, i32) {
    let a2 = ap.wrapping_add(dp);
    let b2 = bp.wrapping_sub(cp);
    let t = (a2.wrapping_sub(b2).wrapping_add(1)) >> 1;
    let dcap = t.wrapping_sub(cp); // D = t - c'
    let ccap = t.wrapping_sub(dp); // C = t - d'
    let a = a2.wrapping_sub(dcap);
    let b = b2.wrapping_add(ccap);
    (a, b, ccap, dcap)
}

/// Inverse of [`crate::decode::math::str_dct2x2dn`] (no `+1` round).
#[inline]
pub fn undo_dct2x2dn(ap: i32, bp: i32, cp: i32, dp: i32) -> (i32, i32, i32, i32) {
    let a2 = ap.wrapping_add(dp);
    let b2 = bp.wrapping_sub(cp);
    let t = (a2.wrapping_sub(b2)) >> 1;
    let dcap = t.wrapping_sub(cp);
    let ccap = t.wrapping_sub(dp);
    let a = a2.wrapping_sub(dcap);
    let b = b2.wrapping_add(ccap);
    (a, b, ccap, dcap)
}

/// Inverse of [`crate::decode::math::inv_odd`].
#[inline]
pub fn fwd_odd(a: i32, b: i32, c: i32, d: i32) -> (i32, i32, i32, i32) {
    let mut a = a;
    let mut b = b;
    let mut c = c;
    let mut d = d;
    a = a.wrapping_add(d); // undo `a -= d`
    b = b.wrapping_sub(c); // undo `b += c`
    d = ((a.wrapping_add(1)) >> 1).wrapping_sub(d); // self-inverse: undo `d = (a+1>>1) - d`
    c = c.wrapping_add((b.wrapping_add(1)) >> 1); // undo `c -= (b+1)>>1`
    let (nc, nd) = fwd_rotate2(c, d);
    c = nc;
    d = nd;
    let (na, nb) = fwd_rotate2(a, b);
    a = na;
    b = nb;
    c = c.wrapping_sub((a.wrapping_add(1)) >> 1); // undo `c += (a+1)>>1`
    d = d.wrapping_add(b >> 1); // undo `d -= b>>1`
    a = a.wrapping_add(c); // undo `a -= c`
    b = b.wrapping_sub(d); // undo `b += d`
    (a, b, c, d)
}

/// Inverse of [`crate::decode::math::inv_odd_odd`]. The decoder
#[inline]
pub fn fwd_odd_odd(a: i32, b: i32, c: i32, d: i32) -> (i32, i32, i32, i32) {
    let mut a = a;
    let mut b = b.wrapping_neg();
    let mut c = c.wrapping_neg();
    let mut d = d;
    d = d.wrapping_add(a); // undo `d -= a`
    let t1 = d >> 1; // == the decoder's t1 (d after its first step)
    c = c.wrapping_sub(b); // undo `c += b`
    let t2 = c >> 1; // == the decoder's t2 (c after its second step)
    a = a.wrapping_sub(t1); // undo `a += t1`
    b = b.wrapping_add(t2); // undo `b -= t2`
    a = a.wrapping_add((b.wrapping_mul(3).wrapping_add(4)) >> 3); // undo `a -= (3b+4)>>3`
    b = b.wrapping_sub((a.wrapping_mul(3).wrapping_add(3)) >> 2); // undo `b += (3a+3)>>2`
    a = a.wrapping_add((b.wrapping_mul(3).wrapping_add(3)) >> 3); // undo `a -= (3b+3)>>3`
    b = b.wrapping_sub(t2); // undo `b += t2`
    a = a.wrapping_add(t1); // undo `a -= t1`
    c = c.wrapping_add(b); // undo `c -= b`
    d = d.wrapping_sub(a); // undo `d += a`
    (a, b, c, d)
}

/// Undo a [`crate::decode::math::four_butterfly`] over the same
/// quadruples (each quad is disjoint, so order within the set is irrelevant).
fn undo_four_butterfly(c: &mut [i32; 16], order: &[[usize; 4]]) {
    for o in order {
        let (a, b, cc, d) = undo_dct2x2dn(c[o[0]], c[o[1]], c[o[2]], c[o[3]]);
        c[o[0]] = a;
        c[o[1]] = b;
        c[o[2]] = cc;
        c[o[3]] = d;
    }
}

/// Forward of [`crate::decode::math::str_idct4x4_stage1`]: the
/// decoder's stage-1 ops applied in reverse, each replaced by its inverse.
pub fn fdct4x4_stage1(c: &mut [i32; 16]) {
    undo_four_butterfly(
        c,
        &[[0, 4, 8, 12], [1, 5, 9, 13], [2, 6, 10, 14], [3, 7, 11, 15]],
    );
    let (a, b, cc, d) = fwd_odd_odd(c[15], c[14], c[13], c[12]);
    c[15] = a;
    c[14] = b;
    c[13] = cc;
    c[12] = d;
    let (a, b, cc, d) = fwd_odd(c[10], c[8], c[11], c[9]);
    c[10] = a;
    c[8] = b;
    c[11] = cc;
    c[9] = d;
    let (a, b, cc, d) = fwd_odd(c[5], c[4], c[7], c[6]);
    c[5] = a;
    c[4] = b;
    c[7] = cc;
    c[6] = d;
    let (a, b, cc, d) = undo_dct2x2up(c[0], c[1], c[2], c[3]);
    c[0] = a;
    c[1] = b;
    c[2] = cc;
    c[3] = d;
}

/// Forward of [`crate::decode::math::str_idct4x4_stage2`].
pub fn fdct4x4_stage2(c: &mut [i32; 16]) {
    undo_four_butterfly(
        c,
        &[[0, 12, 3, 15], [4, 8, 7, 11], [1, 13, 2, 14], [5, 9, 6, 10]],
    );
    let (a, b, cc, d) = undo_dct2x2up(c[0], c[4], c[1], c[5]);
    c[0] = a;
    c[4] = b;
    c[1] = cc;
    c[5] = d;
    let (a, b, cc, d) = fwd_odd_odd(c[10], c[14], c[11], c[15]);
    c[10] = a;
    c[14] = b;
    c[11] = cc;
    c[15] = d;
    let (a, b, cc, d) = fwd_odd(c[8], c[12], c[9], c[13]);
    c[8] = a;
    c[12] = b;
    c[9] = cc;
    c[13] = d;
    let (a, b, cc, d) = fwd_odd(c[2], c[3], c[6], c[7]);
    c[2] = a;
    c[3] = b;
    c[6] = cc;
    c[7] = d;
}

/// Stage 1 only of [`forward_transform_mb_with`]: scatter into the permuted
pub fn forward_stage1_mb(samples: &[i32; 256]) -> [i32; 256] {
    let mut buf = [0i32; 256];
    // Inverse of second_level_coefficient_combination: scatter pixels into the
    // permuted per-block layout. Block (bx,by) lives at base by*16 + bx*64.
    for by in 0..4 {
        for bx in 0..4 {
            let block_base = by * 16 + bx * 64;
            for py in 0..4 {
                for px in 0..4 {
                    let within = MB_PIXEL_MAP[px + py * 4];
                    buf[block_base + within] = samples[(by * 4 + py) * 16 + (bx * 4 + px)];
                }
            }
        }
    }
    // Inverse of second_level_inverse_transform: forward DCT on each 4×4 block.
    for j in 0..16 {
        let mut block = [0i32; 16];
        block.copy_from_slice(&buf[j * 16..j * 16 + 16]);
        fdct4x4_stage1(&mut block);
        buf[j * 16..j * 16 + 16].copy_from_slice(&block);
    }
    buf
}

/// Stage 2 only of [`forward_transform_mb_with`]: forward DCT on the 16
/// block DCs (optionally floor-halved first — scaled-mode chroma).
pub fn forward_stage2_mb(buf: &mut [i32; 256], halve_dclp: bool) {
    // Inverse of first_level_inverse_transform: forward DCT on the 16 block DCs.
    let mut dclp = [0i32; 16];
    for j in 0..16 {
        dclp[j] = buf[j * 16];
        if halve_dclp {
            dclp[j] >>= 1;
        }
    }
    fdct4x4_stage2(&mut dclp);
    for j in 0..16 {
        buf[j * 16] = dclp[j];
    }
}

/// Stage 1 only of [`forward_transform_chroma_mb_420`] (per-block forward
/// DCT; raw block DCs at `buf[16j]` for the DC-domain overlap pre-filter).
pub fn forward_stage1_chroma_420(samples: &[i32; 64]) -> [i32; 64] {
    let mut buf = [0i32; 64];
    for j in 0..4 {
        let (bx4, by4) = (4 * (j % 2), 4 * (j / 2));
        let mut block = [0i32; 16];
        for py in 0..4 {
            for px in 0..4 {
                block[MB_PIXEL_MAP[px + 4 * py]] = samples[(by4 + py) * 8 + bx4 + px];
            }
        }
        fdct4x4_stage1(&mut block);
        buf[16 * j..16 * j + 16].copy_from_slice(&block);
    }
    buf
}

/// Stage 2 only of [`forward_transform_chroma_mb_420`].
pub fn forward_stage2_chroma_420(buf: &mut [i32; 64], halve_dclp: bool) {
    // Across-block stage, decoder ops undone in reverse: unswap(1,2), then the
    // exact t2x2h inverse. Scaled mode floor-halves the block-DCs first
    // (inverse of the decoder's post-inverse ×2; libjxr `strDCT2x2dnEnc`).
    let mut d = [buf[0], buf[16], buf[32], buf[48]];
    if halve_dclp {
        for v in d.iter_mut() {
            *v >>= 1;
        }
    }
    d.swap(1, 2);
    let v = undo_t2x2h(d, 0);
    for (j, vj) in v.iter().enumerate() {
        buf[16 * j] = *vj;
    }
}

/// Stage 1 only of [`forward_transform_chroma_mb_422`].
pub fn forward_stage1_chroma_422(samples: &[i32; 128]) -> [i32; 128] {
    let mut buf = [0i32; 128];
    for j in 0..8 {
        let (bx4, by4) = (4 * (j % 2), 4 * (j / 2));
        let mut block = [0i32; 16];
        for py in 0..4 {
            for px in 0..4 {
                block[MB_PIXEL_MAP[px + 4 * py]] = samples[(by4 + py) * 8 + bx4 + px];
            }
        }
        fdct4x4_stage1(&mut block);
        buf[16 * j..16 * j + 16].copy_from_slice(&block);
    }
    buf
}

/// Stage 2 only of [`forward_transform_chroma_mb_422`].
pub fn forward_stage2_chroma_422(buf: &mut [i32; 128], halve_dclp: bool) {
    let mut d = [0i32; 8];
    for (j, dj) in d.iter_mut().enumerate() {
        *dj = buf[16 * j];
        if halve_dclp {
            *dj >>= 1;
        }
    }
    // Decoder order: 2pt(0,4); t2x2h(0,1,2,3)+swap(1,2); t2x2h(4,6,5,7)+swap(5,6).
    // Undone in reverse below.
    d.swap(5, 6);
    let v = undo_t2x2h([d[4], d[6], d[5], d[7]], 0);
    d[4] = v[0];
    d[6] = v[1];
    d[5] = v[2];
    d[7] = v[3];
    d.swap(1, 2);
    let v = undo_t2x2h([d[0], d[1], d[2], d[3]], 0);
    d[0] = v[0];
    d[1] = v[1];
    d[2] = v[2];
    d[3] = v[3];
    // Undo the 2-pt lifting (decoder: d0 -= (d4+1)>>1; d4 += d0).
    d[4] = d[4].wrapping_sub(d[0]);
    d[0] = d[0].wrapping_add(d[4].wrapping_add(1) >> 1);
    for (j, dj) in d.iter().enumerate() {
        buf[16 * j] = *dj;
    }
}

// ---- Overlap pre-filter (inverse of the decoder's overlap *post* filter) ----

/// Inverse of [`crate::decode::math::t2x2h`] (same `val_round`).
#[inline]
pub fn undo_t2x2h(input: [i32; 4], val_round: i32) -> [i32; 4] {
    let (c0p, c1p, c2p, c3p) = (input[0], input[1], input[2], input[3]);
    let a = c0p.wrapping_add(c3p);
    let b = c1p.wrapping_sub(c2p);
    let t1 = (a.wrapping_sub(b).wrapping_add(val_round)) >> 1;
    let c3 = t1.wrapping_sub(c2p);
    let c2 = t1.wrapping_sub(c3p);
    let c0 = a.wrapping_sub(c3);
    let c1 = b.wrapping_add(c2);
    [c0, c1, c2, c3]
}

/// Inverse of [`crate::decode::math::t2x2h_post`].
#[inline]
pub fn undo_t2x2h_post(input: [i32; 4]) -> [i32; 4] {
    let mut c0 = input[0];
    let mut c1 = input[1];
    let mut c2 = input[2];
    let mut c3 = input[3];
    c1 = c1.wrapping_sub(c2); // undo `c1 += c2`
    c0 = c0.wrapping_add(c3); // undo `c0 -= c3`
    std::mem::swap(&mut c2, &mut c3); // undo the swap
    c2 = ((c0.wrapping_sub(c1)) >> 1).wrapping_sub(c2); // self-inverse
    c3 = c3.wrapping_add(c1 >> 1); // undo `c3 -= c1>>1`
    c0 = c0.wrapping_sub((c3.wrapping_mul(3).wrapping_add(4)) >> 3); // undo `c0 += (3c3+4)>>3`
    c1 = c1.wrapping_add(c2); // undo `c1 -= c2`
    [c0, c1, c2, c3]
}

/// Inverse of [`crate::decode::math::inv_scale`].
#[inline]
pub fn undo_scale(c0: i32, c1: i32) -> (i32, i32) {
    let mut c0 = c0;
    let mut c1 = c1;
    c1 = c1.wrapping_add(c0 >> 10);
    c1 = c1.wrapping_sub(c0 >> 7);
    c1 = c1.wrapping_sub((c0.wrapping_mul(3)) >> 4);
    c0 = c0.wrapping_sub((c1.wrapping_mul(3)) >> 3);
    c1 = (c0 >> 1).wrapping_sub(c1);
    c0 = c0.wrapping_sub(c1);
    (c0, c1)
}

/// Inverse of [`crate::decode::math::inv_toddodd_post`]. Same
/// `t1`/`t2` reconstruction trick as [`fwd_odd_odd`], different constants and
/// no output negation.
#[inline]
pub fn undo_toddodd_post(input: [i32; 4]) -> [i32; 4] {
    let mut c0 = input[0];
    let mut c1 = input[1];
    let mut c2 = input[2];
    let mut c3 = input[3];
    c3 = c3.wrapping_add(c0); // undo `c3 -= c0`
    let t1 = c3 >> 1;
    c2 = c2.wrapping_sub(c1); // undo `c2 += c1`
    let t2 = c2 >> 1;
    c0 = c0.wrapping_sub(t1); // undo `c0 += t1`
    c1 = c1.wrapping_add(t2); // undo `c1 -= t2`
    c0 = c0.wrapping_add((c1.wrapping_mul(3).wrapping_add(4)) >> 3); // undo `c0 -= (3c1+4)>>3`
    c1 = c1.wrapping_sub((c0.wrapping_mul(3).wrapping_add(2)) >> 2); // undo `c1 += (3c0+2)>>2`
    c0 = c0.wrapping_add((c1.wrapping_mul(3).wrapping_add(6)) >> 3); // undo `c0 -= (3c1+6)>>3`
    c1 = c1.wrapping_sub(t2); // undo `c1 += t2`
    c0 = c0.wrapping_add(t1); // undo `c0 -= t1`
    c2 = c2.wrapping_add(c1); // undo `c2 -= c1`
    c3 = c3.wrapping_sub(c0); // undo `c3 += c0`
    [c0, c1, c2, c3]
}

/// Forward of [`crate::decode::math::overlap_post_filter_4x4`]:
/// the decoder's stages applied in reverse, each inverted.
pub fn overlap_pre_filter_4x4(input: [i32; 16]) -> [i32; 16] {
    let mut c = input;
    // undo t2x2h_post
    let r = undo_t2x2h_post([c[0], c[3], c[12], c[15]]);
    c[0] = r[0];
    c[3] = r[1];
    c[12] = r[2];
    c[15] = r[3];
    let r = undo_t2x2h_post([c[1], c[2], c[13], c[14]]);
    c[1] = r[0];
    c[2] = r[1];
    c[13] = r[2];
    c[14] = r[3];
    let r = undo_t2x2h_post([c[4], c[7], c[8], c[11]]);
    c[4] = r[0];
    c[7] = r[1];
    c[8] = r[2];
    c[11] = r[3];
    let r = undo_t2x2h_post([c[5], c[6], c[9], c[10]]);
    c[5] = r[0];
    c[6] = r[1];
    c[9] = r[2];
    c[10] = r[3];
    // undo inv_scale
    let (a, b) = undo_scale(c[0], c[15]);
    c[0] = a;
    c[15] = b;
    let (a, b) = undo_scale(c[1], c[14]);
    c[1] = a;
    c[14] = b;
    let (a, b) = undo_scale(c[4], c[11]);
    c[4] = a;
    c[11] = b;
    let (a, b) = undo_scale(c[5], c[10]);
    c[5] = a;
    c[10] = b;
    // undo inv_toddodd_post
    let r = undo_toddodd_post([c[10], c[11], c[14], c[15]]);
    c[10] = r[0];
    c[11] = r[1];
    c[14] = r[2];
    c[15] = r[3];
    // undo inv_rotate (== fwd_rotate1)
    let (a, b) = fwd_rotate1(c[13], c[12]);
    c[13] = a;
    c[12] = b;
    let (a, b) = fwd_rotate1(c[9], c[8]);
    c[9] = a;
    c[8] = b;
    let (a, b) = fwd_rotate1(c[7], c[3]);
    c[7] = a;
    c[3] = b;
    let (a, b) = fwd_rotate1(c[6], c[2]);
    c[6] = a;
    c[2] = b;
    // undo t2x2h
    let r = undo_t2x2h([c[0], c[3], c[12], c[15]], 0);
    c[0] = r[0];
    c[3] = r[1];
    c[12] = r[2];
    c[15] = r[3];
    let r = undo_t2x2h([c[1], c[2], c[13], c[14]], 0);
    c[1] = r[0];
    c[2] = r[1];
    c[13] = r[2];
    c[14] = r[3];
    let r = undo_t2x2h([c[4], c[7], c[8], c[11]], 0);
    c[4] = r[0];
    c[7] = r[1];
    c[8] = r[2];
    c[11] = r[3];
    let r = undo_t2x2h([c[5], c[6], c[9], c[10]], 0);
    c[5] = r[0];
    c[6] = r[1];
    c[9] = r[2];
    c[10] = r[3];
    c
}

/// Inverse of [`crate::decode::math::str_dct2x2dn`]: recover `(a, b, C, d)`
/// from the forward's output tuple. `t` is recomputable from the restored
/// pre-output values, so the inversion is exact.
#[inline]
fn undo_str_dct2x2dn(ap: i32, bp: i32, cp: i32, dp: i32) -> (i32, i32, i32, i32) {
    let a1 = ap.wrapping_add(dp);
    let b1 = bp.wrapping_sub(cp);
    let t = a1.wrapping_sub(b1) >> 1;
    let d = t.wrapping_sub(cp);
    let big_c = t.wrapping_sub(dp);
    (a1.wrapping_sub(d), b1.wrapping_add(big_c), big_c, d)
}

/// Inverse of [`crate::decode::math::inv_odd_odd_post`] (mechanical lifting
/// reversal; `t1`/`t2` are re-taken once `d`/`c` are restored to the states
/// they were sampled from).
#[inline]
fn undo_inv_odd_odd_post(a: i32, b: i32, c: i32, d: i32) -> (i32, i32, i32, i32) {
    let (mut a, mut b, mut c, mut d) = (a, b, c, d);
    d = d.wrapping_add(a);
    c = c.wrapping_sub(b);
    let t1 = d >> 1;
    let t2 = c >> 1;
    a = a.wrapping_sub(t1);
    b = b.wrapping_add(t2);
    a = a.wrapping_add((b.wrapping_mul(3).wrapping_add(4)) >> 3);
    b = b.wrapping_sub((a.wrapping_mul(3).wrapping_add(2)) >> 2);
    a = a.wrapping_add((b.wrapping_mul(3).wrapping_add(6)) >> 3);
    b = b.wrapping_sub(t2);
    a = a.wrapping_add(t1);
    c = c.wrapping_add(b);
    d = d.wrapping_sub(a);
    (a, b, c, d)
}

/// Inverse of [`crate::decode::math::str_hst_dec1_alternate`].
#[inline]
fn undo_str_hst_dec1_alternate(a: i32, d: i32) -> (i32, i32) {
    let (mut a, mut d) = (a, d);
    d = d.wrapping_add(a >> 10);
    d = d.wrapping_sub(a >> 7);
    d = d.wrapping_sub(a.wrapping_mul(3) >> 4);
    a = a.wrapping_sub(d.wrapping_mul(3) >> 3);
    d = (a >> 1).wrapping_sub(d);
    a = a.wrapping_sub(d);
    (a, d)
}

/// Inverse of [`crate::decode::math::str_hst_dec`]: the forward returns
/// `(a1 − c_out, b1 + d1, d1, c_out)`; every intermediate is recoverable in
/// closed form.
#[inline]
fn undo_str_hst_dec(w: i32, x: i32, y: i32, z: i32) -> (i32, i32, i32, i32) {
    let d1 = y;
    let c_out = z;
    let a1 = w.wrapping_add(c_out);
    let b1 = x.wrapping_sub(d1);
    let c = (a1.wrapping_sub(b1) >> 1).wrapping_sub(c_out);
    let d = d1.wrapping_add(b1 >> 1);
    let a = a1.wrapping_sub((d.wrapping_mul(3).wrapping_add(4)) >> 3);
    let b = b1.wrapping_add(c);
    (a, b, c, d)
}

/// Forward of [`crate::decode::math::str_post_4x4_stage2_split_alternate`]
/// (libjxr `strPre4x4Stage2Split`): the first-level (block-DC domain)
pub fn str_pre_4x4_stage2_split_alternate(input: &[i32; 16]) -> [i32; 16] {
    let mut c = *input;
    // E⁻¹: undo the four str_hst_dec quads.
    for &[i0, i1, i2, i3] in &[
        [0usize, 12, 3, 15],
        [1, 13, 2, 14],
        [4, 8, 7, 11],
        [5, 9, 6, 10],
    ] {
        let (a, b, cc, d) = undo_str_hst_dec(c[i0], c[i1], c[i2], c[i3]);
        c[i0] = a;
        c[i1] = b;
        c[i2] = cc;
        c[i3] = d;
    }
    // D⁻¹: undo the four str_hst_dec1_alternate pairs.
    for &[i0, i1] in &[[0usize, 15], [1, 14], [4, 11], [5, 10]] {
        let (a, d) = undo_str_hst_dec1_alternate(c[i0], c[i1]);
        c[i0] = a;
        c[i1] = d;
    }
    // C⁻¹: irotate1 undone by fwd_rotate1, same pairs.
    for &[i0, i1] in &[[6usize, 2], [7, 3], [9, 8], [13, 12]] {
        let (a, b) = fwd_rotate1(c[i0], c[i1]);
        c[i0] = a;
        c[i1] = b;
    }
    // B⁻¹: undo inv_odd_odd_post on (10, 11, 14, 15).
    let (a, b, cc, d) = undo_inv_odd_odd_post(c[10], c[11], c[14], c[15]);
    c[10] = a;
    c[11] = b;
    c[14] = cc;
    c[15] = d;
    // A⁻¹: undo the four str_dct2x2dn quads.
    for &[i0, i1, i2, i3] in &[
        [0usize, 3, 12, 15],
        [1, 2, 13, 14],
        [4, 7, 8, 11],
        [5, 6, 9, 10],
    ] {
        let (a, b, cc, d) = undo_str_dct2x2dn(c[i0], c[i1], c[i2], c[i3]);
        c[i0] = a;
        c[i1] = b;
        c[i2] = cc;
        c[i3] = d;
    }
    c
}

/// Forward of [`crate::decode::math::overlap_post_filter_2x2`] (Table 170's
/// inverse — libjxr's chroma 2×2 junction PRE-filter).
pub fn overlap_pre_filter_2x2(input: [i32; 4]) -> [i32; 4] {
    let mut c = input;
    c[1] = c[1].wrapping_add(c[2]);
    c[0] = c[0].wrapping_add(c[3]);
    c[2] = c[2].wrapping_sub((c[1].wrapping_add(1)) >> 1);
    c[3] = c[3].wrapping_sub((c[0].wrapping_add(1)) >> 1);
    c[1] = c[1].wrapping_sub((c[0].wrapping_add(2)) >> 2);
    c[0] = c[0].wrapping_sub(c[1] >> 13);
    c[0] = c[0].wrapping_sub(c[1] >> 9);
    c[0] = c[0].wrapping_sub(c[1] >> 5);
    c[0] = c[0].wrapping_sub((c[1].wrapping_add(1)) >> 1);
    c[1] = c[1].wrapping_sub((c[0].wrapping_add(2)) >> 2);
    c[2] = c[2].wrapping_add((c[1].wrapping_add(1)) >> 1);
    c[3] = c[3].wrapping_add((c[0].wrapping_add(1)) >> 1);
    c[1] = c[1].wrapping_sub(c[2]);
    c[0] = c[0].wrapping_sub(c[3]);
    c
}

/// Forward of [`crate::decode::math::overlap_post_filter_2`] (Table 171's
/// inverse — the chroma 2-point edge PRE-filter).
pub fn overlap_pre_filter_2(input: [i32; 2]) -> [i32; 2] {
    let mut c = input;
    c[1] = c[1].wrapping_sub((c[0].wrapping_add(2)) >> 2);
    c[0] = c[0].wrapping_sub(c[1] >> 13);
    c[0] = c[0].wrapping_sub(c[1] >> 9);
    c[0] = c[0].wrapping_sub(c[1] >> 5);
    c[0] = c[0].wrapping_sub((c[1].wrapping_add(1)) >> 1);
    c[1] = c[1].wrapping_sub((c[0].wrapping_add(2)) >> 2);
    c
}

/// Forward of [`crate::decode::math::overlap_post_filter_4`].
pub fn overlap_pre_filter_4(input: [i32; 4]) -> [i32; 4] {
    let mut c = input;
    c[1] = c[1].wrapping_add(c[2]);
    c[0] = c[0].wrapping_add(c[3]);
    c[2] = c[2].wrapping_sub((c[1].wrapping_add(1)) >> 1);
    c[3] = c[3].wrapping_sub((c[0].wrapping_add(1)) >> 1);
    let (a, b) = fwd_rotate1(c[2], c[3]);
    c[2] = a;
    c[3] = b;
    c[2] = c[2].wrapping_neg();
    c[3] = c[3].wrapping_neg();
    c[1] = c[1].wrapping_sub(c[2]);
    c[0] = c[0].wrapping_sub(c[3]);
    c[2] = c[2].wrapping_add(c[1] >> 1);
    c[3] = c[3].wrapping_add(c[0] >> 1);
    c[1] = c[1].wrapping_sub((c[2].wrapping_mul(3).wrapping_add(4)) >> 3);
    c[0] = c[0].wrapping_sub((c[3].wrapping_mul(3).wrapping_add(4)) >> 3);
    let (a, b) = undo_scale(c[1], c[2]);
    c[1] = a;
    c[2] = b;
    let (a, b) = undo_scale(c[0], c[3]);
    c[0] = a;
    c[3] = b;
    c[2] = c[2].wrapping_add((c[1].wrapping_add(1)) >> 1);
    c[3] = c[3].wrapping_add((c[0].wrapping_add(1)) >> 1);
    c[1] = c[1].wrapping_sub(c[2]);
    c[0] = c[0].wrapping_sub(c[3]);
    c
}

/// Test-only stage-1+2 compositions over one MB — the shape the inversion
/// tests exercise (production paths run the stages separately, interleaved
/// with the DC pre-filter and quantization).
#[cfg(test)]
pub(crate) fn forward_transform_mb(samples: &[i32; 256]) -> [i32; 256] {
    let mut buf = forward_stage1_mb(samples);
    forward_stage2_mb(&mut buf, false);
    buf
}

#[cfg(test)]
fn forward_transform_chroma_mb_420(samples: &[i32; 64], halve_dclp: bool) -> [i32; 64] {
    let mut buf = forward_stage1_chroma_420(samples);
    forward_stage2_chroma_420(&mut buf, halve_dclp);
    buf
}

#[cfg(test)]
fn forward_transform_chroma_mb_422(samples: &[i32; 128], halve_dclp: bool) -> [i32; 128] {
    let mut buf = forward_stage1_chroma_422(samples);
    forward_stage2_chroma_422(&mut buf, halve_dclp);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::math as dec;

    /// Deterministic LCG so the round-trip vectors are reproducible without a
    /// dependency. Values span roughly a coefficient range.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> i32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as i64).rem_euclid(8001) as i32 - 4000
        }
        fn block(&mut self) -> [i32; 16] {
            let mut b = [0i32; 16];
            for v in &mut b {
                *v = self.next();
            }
            b
        }
    }

    #[test]
    fn rotate1_inverts_decoder() {
        let mut r = Lcg(0x1111);
        for _ in 0..5000 {
            let (a, b) = (r.next(), r.next());
            let (fa, fb) = fwd_rotate1(a, b);
            assert_eq!(dec::irotate1(fa, fb), (a, b));
        }
    }

    #[test]
    fn rotate2_inverts_decoder() {
        let mut r = Lcg(0x2222);
        for _ in 0..5000 {
            let (a, b) = (r.next(), r.next());
            let (fa, fb) = fwd_rotate2(a, b);
            assert_eq!(dec::irotate2(fa, fb), (a, b));
        }
    }

    #[test]
    fn dct2x2up_inverts_decoder() {
        let mut r = Lcg(0x3333);
        for _ in 0..5000 {
            let x = (r.next(), r.next(), r.next(), r.next());
            let e = undo_dct2x2up(x.0, x.1, x.2, x.3);
            assert_eq!(dec::str_dct2x2up(e.0, e.1, e.2, e.3), x);
        }
    }

    #[test]
    fn dct2x2dn_inverts_decoder() {
        let mut r = Lcg(0x4444);
        for _ in 0..5000 {
            let x = (r.next(), r.next(), r.next(), r.next());
            let e = undo_dct2x2dn(x.0, x.1, x.2, x.3);
            assert_eq!(dec::str_dct2x2dn(e.0, e.1, e.2, e.3), x);
        }
    }

    #[test]
    fn odd_inverts_decoder() {
        let mut r = Lcg(0x5555);
        for _ in 0..5000 {
            let x = (r.next(), r.next(), r.next(), r.next());
            let e = fwd_odd(x.0, x.1, x.2, x.3);
            assert_eq!(dec::inv_odd(e.0, e.1, e.2, e.3), x);
        }
    }

    #[test]
    fn odd_odd_inverts_decoder() {
        let mut r = Lcg(0x6666);
        for _ in 0..5000 {
            let x = (r.next(), r.next(), r.next(), r.next());
            let e = fwd_odd_odd(x.0, x.1, x.2, x.3);
            assert_eq!(dec::inv_odd_odd(e.0, e.1, e.2, e.3), x);
        }
    }

    #[test]
    fn stage1_inverts_decoder() {
        let mut r = Lcg(0x7777);
        for _ in 0..5000 {
            let x = r.block();
            let mut y = x;
            fdct4x4_stage1(&mut y);
            dec::str_idct4x4_stage1(&mut y);
            assert_eq!(y, x);
        }
    }

    #[test]
    fn stage2_inverts_decoder() {
        let mut r = Lcg(0x8888);
        for _ in 0..5000 {
            let x = r.block();
            let mut y = x;
            fdct4x4_stage2(&mut y);
            dec::str_idct4x4_stage2(&mut y);
            assert_eq!(y, x);
        }
    }

    #[test]
    fn t2x2h_inverts_decoder() {
        let mut r = Lcg(0xeeee);
        for _ in 0..5000 {
            let x = [r.next(), r.next(), r.next(), r.next()];
            assert_eq!(dec::t2x2h(undo_t2x2h(x, 0), 0), x);
        }
    }

    #[test]
    fn t2x2h_post_inverts_decoder() {
        let mut r = Lcg(0xbbbb);
        for _ in 0..5000 {
            let x = [r.next(), r.next(), r.next(), r.next()];
            assert_eq!(dec::t2x2h_post(undo_t2x2h_post(x)), x);
        }
    }

    #[test]
    fn scale_inverts_decoder() {
        let mut r = Lcg(0xdddd);
        for _ in 0..5000 {
            let (a, b) = (r.next(), r.next());
            let (pa, pb) = undo_scale(a, b);
            assert_eq!(dec::inv_scale(pa, pb), (a, b));
        }
    }

    #[test]
    fn toddodd_post_inverts_decoder() {
        let mut r = Lcg(0xcccc);
        for _ in 0..5000 {
            let x = [r.next(), r.next(), r.next(), r.next()];
            assert_eq!(dec::inv_toddodd_post(undo_toddodd_post(x)), x);
        }
    }

    #[test]
    fn overlap4x4_inverts_decoder() {
        let mut r = Lcg(0x9999);
        for _ in 0..5000 {
            let x = r.block();
            assert_eq!(dec::overlap_post_filter_4x4(overlap_pre_filter_4x4(x)), x);
        }
    }

    #[test]
    fn overlap4_inverts_decoder() {
        let mut r = Lcg(0xaaaa);
        for _ in 0..5000 {
            let x = [r.next(), r.next(), r.next(), r.next()];
            assert_eq!(dec::overlap_post_filter_4(overlap_pre_filter_4(x)), x);
        }
    }

    /// First-level (block-DC domain) pre-filter and the chroma junction/edge
    /// pre-filters invert the decoder's post filters exactly.
    #[test]
    fn first_level_pre_filters_invert_decoder() {
        let mut r = Lcg(0x4242_1717);
        for _ in 0..5000 {
            let x = r.block();
            assert_eq!(
                dec::str_post_4x4_stage2_split_alternate(&str_pre_4x4_stage2_split_alternate(&x)),
                x
            );
            let q = [x[0], x[1], x[2], x[3]];
            assert_eq!(dec::overlap_post_filter_2x2(overlap_pre_filter_2x2(q)), q);
            let p = [x[4], x[5]];
            assert_eq!(dec::overlap_post_filter_2(overlap_pre_filter_2(p)), p);
        }
    }

    /// The decoder's 420-chroma reconstruction (the `first_level_inverse_transform`
    /// chroma arm + per-block stage-1 + Table-157 scatter), composed by hand
    /// exactly as `decoder.rs` runs it.
    fn decode_chroma_mb_420(buf: &[i32; 64]) -> [i32; 64] {
        let mut buf = *buf;
        let mut d = [buf[0], buf[16], buf[32], buf[48]];
        d = dec::t2x2h(d, 0);
        d.swap(1, 2);
        for (j, dj) in d.iter().enumerate() {
            buf[16 * j] = *dj;
        }
        let mut out = [0i32; 64];
        for j in 0..4 {
            let mut block = [0i32; 16];
            block.copy_from_slice(&buf[16 * j..16 * j + 16]);
            dec::str_idct4x4_stage1(&mut block);
            let (bx4, by4) = (4 * (j % 2), 4 * (j / 2));
            for py in 0..4 {
                for px in 0..4 {
                    out[(by4 + py) * 8 + bx4 + px] = block[MB_PIXEL_MAP[px + 4 * py]];
                }
            }
        }
        out
    }

    /// The decoder's 422-chroma reconstruction (the Table-151 across-block
    /// sequence from its sample-reconstruction stage, plus stage-1 and scatter),
    /// composed by hand.
    fn decode_chroma_mb_422(buf: &[i32; 128]) -> [i32; 128] {
        let mut buf = *buf;
        let mut d = [0i32; 8];
        for (j, dj) in d.iter_mut().enumerate() {
            *dj = buf[16 * j];
        }
        d[0] = d[0].wrapping_sub(d[4].wrapping_add(1) >> 1);
        d[4] = d[4].wrapping_add(d[0]);
        let a = dec::t2x2h([d[0], d[1], d[2], d[3]], 0);
        d[0] = a[0];
        d[1] = a[1];
        d[2] = a[2];
        d[3] = a[3];
        d.swap(1, 2);
        let b = dec::t2x2h([d[4], d[6], d[5], d[7]], 0);
        d[4] = b[0];
        d[6] = b[1];
        d[5] = b[2];
        d[7] = b[3];
        d.swap(5, 6);
        for (j, dj) in d.iter().enumerate() {
            buf[16 * j] = *dj;
        }
        let mut out = [0i32; 128];
        for j in 0..8 {
            let mut block = [0i32; 16];
            block.copy_from_slice(&buf[16 * j..16 * j + 16]);
            dec::str_idct4x4_stage1(&mut block);
            let (bx4, by4) = (4 * (j % 2), 4 * (j / 2));
            for py in 0..4 {
                for px in 0..4 {
                    out[(by4 + py) * 8 + bx4 + px] = block[MB_PIXEL_MAP[px + 4 * py]];
                }
            }
        }
        out
    }

    #[test]
    fn chroma_mb_420_inverts_decoder() {
        let mut r = Lcg(0x420420);
        for _ in 0..2000 {
            let mut x = [0i32; 64];
            for v in x.iter_mut() {
                *v = r.next();
            }
            assert_eq!(
                decode_chroma_mb_420(&forward_transform_chroma_mb_420(&x, false)),
                x
            );
        }
    }

    #[test]
    fn chroma_mb_422_inverts_decoder() {
        let mut r = Lcg(0x422422);
        for _ in 0..2000 {
            let mut x = [0i32; 128];
            for v in x.iter_mut() {
                *v = r.next();
            }
            assert_eq!(
                decode_chroma_mb_422(&forward_transform_chroma_mb_422(&x, false)),
                x
            );
        }
    }
}
