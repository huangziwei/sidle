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
}
