//! Forward JPEG-XR core-transform primitives — the encode-side inverses of
//! [`crate::image::jxr_decode::math`].
//!
//! The decoder's transform is a chain of integer *lifting steps* (each
//! modifies one lane using the others, which are unchanged in that step), so
//! every step is exactly invertible by reversing the order and flipping the
//! sign. We mirror the decoder's own primitives rather than porting libjxr's
//! forward verbatim, which guarantees `decoder(encoder(x)) == x` bit-for-bit
//! against *our* decoder. Cross-reference for the spec-forward shape:
//! `ref/libjxr/image/encode/strFwdTransform.c`.
//!
//! All arithmetic is `wrapping_*`, matching the decoder, so the round trip is
//! exact for every `i32` input.

/// Inverse of [`crate::image::jxr_decode::math::irotate1`].
#[inline]
pub fn fwd_rotate1(a: i32, b: i32) -> (i32, i32) {
    let mut a = a;
    let mut b = b;
    b = b.wrapping_sub((a.wrapping_add(1)) >> 1);
    a = a.wrapping_add((b.wrapping_add(1)) >> 1);
    (a, b)
}

/// Inverse of [`crate::image::jxr_decode::math::irotate2`].
#[inline]
pub fn fwd_rotate2(a: i32, b: i32) -> (i32, i32) {
    let mut a = a;
    let mut b = b;
    b = b.wrapping_sub((a.wrapping_mul(3).wrapping_add(4)) >> 3);
    a = a.wrapping_add((b.wrapping_mul(3).wrapping_add(4)) >> 3);
    (a, b)
}

/// Inverse of [`crate::image::jxr_decode::math::str_dct2x2up`]. Given the
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

/// Inverse of [`crate::image::jxr_decode::math::str_dct2x2dn`] (no `+1` round).
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

/// Inverse of [`crate::image::jxr_decode::math::inv_odd`].
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

/// Inverse of [`crate::image::jxr_decode::math::inv_odd_odd`]. The decoder
/// negates `b` and `c` on output, so we un-negate them first, then reverse the
/// lifting steps. The `t1`/`t2` intermediates are reconstructable because the
/// lanes they read (`d` after the first step, `c` after the second) are
/// restored to exactly those values by the first two inverse steps.
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

/// Undo a [`crate::image::jxr_decode::math::four_butterfly`] over the same
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

/// Forward of [`crate::image::jxr_decode::math::str_idct4x4_stage1`]: the
/// decoder's stage-1 ops applied in reverse, each replaced by its inverse.
pub fn fdct4x4_stage1(c: &mut [i32; 16]) {
    undo_four_butterfly(
        c,
        &[[0, 4, 8, 12], [1, 5, 9, 13], [2, 6, 10, 14], [3, 7, 11, 15]],
    );
    let (a, b, cc, d) = fwd_odd_odd(c[15], c[14], c[13], c[12]);
    c[15] = a; c[14] = b; c[13] = cc; c[12] = d;
    let (a, b, cc, d) = fwd_odd(c[10], c[8], c[11], c[9]);
    c[10] = a; c[8] = b; c[11] = cc; c[9] = d;
    let (a, b, cc, d) = fwd_odd(c[5], c[4], c[7], c[6]);
    c[5] = a; c[4] = b; c[7] = cc; c[6] = d;
    let (a, b, cc, d) = undo_dct2x2up(c[0], c[1], c[2], c[3]);
    c[0] = a; c[1] = b; c[2] = cc; c[3] = d;
}

/// Forward of [`crate::image::jxr_decode::math::str_idct4x4_stage2`].
pub fn fdct4x4_stage2(c: &mut [i32; 16]) {
    undo_four_butterfly(
        c,
        &[[0, 12, 3, 15], [4, 8, 7, 11], [1, 13, 2, 14], [5, 9, 6, 10]],
    );
    let (a, b, cc, d) = undo_dct2x2up(c[0], c[4], c[1], c[5]);
    c[0] = a; c[4] = b; c[1] = cc; c[5] = d;
    let (a, b, cc, d) = fwd_odd_odd(c[10], c[14], c[11], c[15]);
    c[10] = a; c[14] = b; c[11] = cc; c[15] = d;
    let (a, b, cc, d) = fwd_odd(c[8], c[12], c[9], c[13]);
    c[8] = a; c[12] = b; c[9] = cc; c[13] = d;
    let (a, b, cc, d) = fwd_odd(c[2], c[3], c[6], c[7]);
    c[2] = a; c[3] = b; c[6] = cc; c[7] = d;
}

// ---- Overlap pre-filter (inverse of the decoder's overlap *post* filter) ----

/// Inverse of [`crate::image::jxr_decode::math::t2x2h`] (same `val_round`).
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

/// Inverse of [`crate::image::jxr_decode::math::t2x2h_post`].
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

/// Inverse of [`crate::image::jxr_decode::math::inv_scale`].
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

/// Inverse of [`crate::image::jxr_decode::math::inv_toddodd_post`]. Same
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

/// Forward of [`crate::image::jxr_decode::math::overlap_post_filter_4x4`]:
/// the decoder's stages applied in reverse, each inverted.
pub fn overlap_pre_filter_4x4(input: [i32; 16]) -> [i32; 16] {
    let mut c = input;
    // undo t2x2h_post
    let r = undo_t2x2h_post([c[0], c[3], c[12], c[15]]);
    c[0] = r[0]; c[3] = r[1]; c[12] = r[2]; c[15] = r[3];
    let r = undo_t2x2h_post([c[1], c[2], c[13], c[14]]);
    c[1] = r[0]; c[2] = r[1]; c[13] = r[2]; c[14] = r[3];
    let r = undo_t2x2h_post([c[4], c[7], c[8], c[11]]);
    c[4] = r[0]; c[7] = r[1]; c[8] = r[2]; c[11] = r[3];
    let r = undo_t2x2h_post([c[5], c[6], c[9], c[10]]);
    c[5] = r[0]; c[6] = r[1]; c[9] = r[2]; c[10] = r[3];
    // undo inv_scale
    let (a, b) = undo_scale(c[0], c[15]); c[0] = a; c[15] = b;
    let (a, b) = undo_scale(c[1], c[14]); c[1] = a; c[14] = b;
    let (a, b) = undo_scale(c[4], c[11]); c[4] = a; c[11] = b;
    let (a, b) = undo_scale(c[5], c[10]); c[5] = a; c[10] = b;
    // undo inv_toddodd_post
    let r = undo_toddodd_post([c[10], c[11], c[14], c[15]]);
    c[10] = r[0]; c[11] = r[1]; c[14] = r[2]; c[15] = r[3];
    // undo inv_rotate (== fwd_rotate1)
    let (a, b) = fwd_rotate1(c[13], c[12]); c[13] = a; c[12] = b;
    let (a, b) = fwd_rotate1(c[9], c[8]); c[9] = a; c[8] = b;
    let (a, b) = fwd_rotate1(c[7], c[3]); c[7] = a; c[3] = b;
    let (a, b) = fwd_rotate1(c[6], c[2]); c[6] = a; c[2] = b;
    // undo t2x2h
    let r = undo_t2x2h([c[0], c[3], c[12], c[15]], 0);
    c[0] = r[0]; c[3] = r[1]; c[12] = r[2]; c[15] = r[3];
    let r = undo_t2x2h([c[1], c[2], c[13], c[14]], 0);
    c[1] = r[0]; c[2] = r[1]; c[13] = r[2]; c[14] = r[3];
    let r = undo_t2x2h([c[4], c[7], c[8], c[11]], 0);
    c[4] = r[0]; c[7] = r[1]; c[8] = r[2]; c[11] = r[3];
    let r = undo_t2x2h([c[5], c[6], c[9], c[10]], 0);
    c[5] = r[0]; c[6] = r[1]; c[9] = r[2]; c[10] = r[3];
    c
}

/// Forward of [`crate::image::jxr_decode::math::overlap_post_filter_4`].
pub fn overlap_pre_filter_4(input: [i32; 4]) -> [i32; 4] {
    let mut c = input;
    c[1] = c[1].wrapping_add(c[2]);
    c[0] = c[0].wrapping_add(c[3]);
    c[2] = c[2].wrapping_sub((c[1].wrapping_add(1)) >> 1);
    c[3] = c[3].wrapping_sub((c[0].wrapping_add(1)) >> 1);
    let (a, b) = fwd_rotate1(c[2], c[3]); c[2] = a; c[3] = b;
    c[2] = c[2].wrapping_neg();
    c[3] = c[3].wrapping_neg();
    c[1] = c[1].wrapping_sub(c[2]);
    c[0] = c[0].wrapping_sub(c[3]);
    c[2] = c[2].wrapping_add(c[1] >> 1);
    c[3] = c[3].wrapping_add(c[0] >> 1);
    c[1] = c[1].wrapping_sub((c[2].wrapping_mul(3).wrapping_add(4)) >> 3);
    c[0] = c[0].wrapping_sub((c[3].wrapping_mul(3).wrapping_add(4)) >> 3);
    let (a, b) = undo_scale(c[1], c[2]); c[1] = a; c[2] = b;
    let (a, b) = undo_scale(c[0], c[3]); c[0] = a; c[3] = b;
    c[2] = c[2].wrapping_add((c[1].wrapping_add(1)) >> 1);
    c[3] = c[3].wrapping_add((c[0].wrapping_add(1)) >> 1);
    c[1] = c[1].wrapping_sub(c[2]);
    c[0] = c[0].wrapping_sub(c[3]);
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::jxr_decode::math as dec;

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
}
