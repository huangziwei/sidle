//! JPEG-XR integer transform / overlap-filter primitives.
//!
//! Direct port of the bottom of `jxr_image.py` (lines ~2200-2545). Same
//! function names so the math can be cross-referenced against the spec.
//!
//! All values are `i32`. Python's arbitrary-precision ints don't actually
//! grow large here — the JXR spec works in fixed precision and rounds via
//! right-shift, which we match with arithmetic right shift.

#![allow(non_snake_case)]

#[inline]
pub fn chroma_component(i_component: usize) -> usize {
    if i_component == 0 { 0 } else { 1 }
}

#[inline]
pub fn signed_value(value: i32, sign_flag: bool) -> i32 {
    if sign_flag { -value } else { value }
}

#[inline]
pub fn twos_complement_byte(v: u32) -> i32 {
    if (v & 0x80) == 0 {
        v as i32
    } else {
        v as i32 - 256
    }
}

#[inline]
pub fn num_ones(x: u32) -> u32 {
    x.count_ones()
}

#[inline]
pub fn clip(x: i32, low: i32, high: i32) -> i32 {
    x.max(low).min(high)
}

/// IDCT stage 1: process the 16 sub-block coefficients.
///
/// Mutates `c` in place over indices 0..16.
pub fn str_idct4x4_stage1(c: &mut [i32; 16]) {
    // strDCT2x2up([c[0], c[1], c[2], c[3]])
    let (a, b, cc, d) = str_dct2x2up(c[0], c[1], c[2], c[3]);
    c[0] = a; c[1] = b; c[2] = cc; c[3] = d;

    // invOdd([c[5], c[4], c[7], c[6]]) -> assigned back to [c[5], c[4], c[7], c[6]]
    let (a, b, cc, d) = inv_odd(c[5], c[4], c[7], c[6]);
    c[5] = a; c[4] = b; c[7] = cc; c[6] = d;

    let (a, b, cc, d) = inv_odd(c[10], c[8], c[11], c[9]);
    c[10] = a; c[8] = b; c[11] = cc; c[9] = d;

    let (a, b, cc, d) = inv_odd_odd(c[15], c[14], c[13], c[12]);
    c[15] = a; c[14] = b; c[13] = cc; c[12] = d;

    four_butterfly(
        c,
        &[
            [0, 4, 8, 12],
            [1, 5, 9, 13],
            [2, 6, 10, 14],
            [3, 7, 11, 15],
        ],
    );
}

/// IDCT stage 2: act on the DCLP coefficients.
pub fn str_idct4x4_stage2(c: &mut [i32; 16]) {
    let (a, b, cc, d) = inv_odd(c[2], c[3], c[6], c[7]);
    c[2] = a; c[3] = b; c[6] = cc; c[7] = d;

    let (a, b, cc, d) = inv_odd(c[8], c[12], c[9], c[13]);
    c[8] = a; c[12] = b; c[9] = cc; c[13] = d;

    let (a, b, cc, d) = inv_odd_odd(c[10], c[14], c[11], c[15]);
    c[10] = a; c[14] = b; c[11] = cc; c[15] = d;

    let (a, b, cc, d) = str_dct2x2up(c[0], c[4], c[1], c[5]);
    c[0] = a; c[4] = b; c[1] = cc; c[5] = d;

    four_butterfly(
        c,
        &[
            [0, 12, 3, 15],
            [4, 8, 7, 11],
            [1, 13, 2, 14],
            [5, 9, 6, 10],
        ],
    );
}

/// 2x2 forward/up stride DCT (used by IDCT pipeline).
#[inline]
pub fn str_dct2x2up(a: i32, b: i32, c_big: i32, d: i32) -> (i32, i32, i32, i32) {
    let mut a = a;
    let mut b = b;
    let C = c_big;
    let mut d = d;

    a = a.wrapping_add(d);
    b = b.wrapping_sub(C);
    let t = (a.wrapping_sub(b).wrapping_add(1)) >> 1;
    let c = t.wrapping_sub(d);
    d = t.wrapping_sub(C);
    a = a.wrapping_sub(d);
    b = b.wrapping_add(c);

    (a, b, c, d)
}

#[inline]
pub fn str_dct2x2dn(a: i32, b: i32, c_big: i32, d: i32) -> (i32, i32, i32, i32) {
    let mut a = a;
    let mut b = b;
    let C = c_big;
    let mut d = d;

    a = a.wrapping_add(d);
    b = b.wrapping_sub(C);
    let t = (a.wrapping_sub(b)) >> 1;
    let c = t.wrapping_sub(d);
    d = t.wrapping_sub(C);
    a = a.wrapping_sub(d);
    b = b.wrapping_add(c);

    (a, b, c, d)
}

/// Inverse-odd butterfly used by both IDCT stages.
#[inline]
pub fn inv_odd(a: i32, b: i32, c: i32, d: i32) -> (i32, i32, i32, i32) {
    let mut a = a;
    let mut b = b;
    let mut c = c;
    let mut d = d;

    b = b.wrapping_add(d);
    a = a.wrapping_sub(c);
    d = d.wrapping_sub(b >> 1);
    c = c.wrapping_add((a.wrapping_add(1)) >> 1);

    let (na, nb) = irotate2(a, b);
    a = na; b = nb;
    let (nc, nd) = irotate2(c, d);
    c = nc; d = nd;

    c = c.wrapping_sub((b.wrapping_add(1)) >> 1);
    d = ((a.wrapping_add(1)) >> 1).wrapping_sub(d);
    b = b.wrapping_add(c);
    a = a.wrapping_sub(d);

    (a, b, c, d)
}

#[inline]
pub fn inv_odd_odd(a: i32, b: i32, c: i32, d: i32) -> (i32, i32, i32, i32) {
    let mut a = a;
    let mut b = b;
    let mut c = c;
    let mut d = d;

    d = d.wrapping_add(a);
    c = c.wrapping_sub(b);
    let t1 = d >> 1;
    a = a.wrapping_sub(t1);
    let t2 = c >> 1;
    b = b.wrapping_add(t2);

    a = a.wrapping_sub((b.wrapping_mul(3).wrapping_add(3)) >> 3);
    b = b.wrapping_add((a.wrapping_mul(3).wrapping_add(3)) >> 2);
    a = a.wrapping_sub((b.wrapping_mul(3).wrapping_add(4)) >> 3);

    b = b.wrapping_sub(t2);
    a = a.wrapping_add(t1);
    c = c.wrapping_add(b);
    d = d.wrapping_sub(a);

    // Python returns [a, -b, -c, d]
    (a, b.wrapping_neg(), c.wrapping_neg(), d)
}

#[inline]
pub fn irotate1(a: i32, b: i32) -> (i32, i32) {
    let mut a = a;
    let mut b = b;
    a = a.wrapping_sub((b.wrapping_add(1)) >> 1);
    b = b.wrapping_add((a.wrapping_add(1)) >> 1);
    (a, b)
}

#[inline]
pub fn irotate2(a: i32, b: i32) -> (i32, i32) {
    let mut a = a;
    let mut b = b;
    a = a.wrapping_sub((b.wrapping_mul(3).wrapping_add(4)) >> 3);
    b = b.wrapping_add((a.wrapping_mul(3).wrapping_add(4)) >> 3);
    (a, b)
}

/// Apply a strDCT2x2dn butterfly to the four indexed positions, for each
/// quadruple in `order`. Mirrors calibre's `fourbutterfly`.
pub fn four_butterfly(c: &mut [i32; 16], order: &[[usize; 4]]) {
    for o in order {
        let (a, b, cc, d) = str_dct2x2dn(c[o[0]], c[o[1]], c[o[2]], c[o[3]]);
        c[o[0]] = a;
        c[o[1]] = b;
        c[o[2]] = cc;
        c[o[3]] = d;
    }
}

/// Inverse odd-odd post (for `strPost4x4Stage2Split_alternate`).
pub fn inv_odd_odd_post(a: i32, b: i32, c: i32, d: i32) -> (i32, i32, i32, i32) {
    let mut a = a;
    let mut b = b;
    let mut c = c;
    let mut d = d;

    d = d.wrapping_add(a);
    c = c.wrapping_sub(b);
    let t1 = d >> 1;
    a = a.wrapping_sub(t1);
    let t2 = c >> 1;
    b = b.wrapping_add(t2);

    a = a.wrapping_sub((b.wrapping_mul(3).wrapping_add(6)) >> 3);
    b = b.wrapping_add((a.wrapping_mul(3).wrapping_add(2)) >> 2);
    a = a.wrapping_sub((b.wrapping_mul(3).wrapping_add(4)) >> 3);

    b = b.wrapping_sub(t2);
    a = a.wrapping_add(t1);
    c = c.wrapping_add(b);
    d = d.wrapping_sub(a);

    (a, b, c, d)
}

#[inline]
pub fn str_hst_dec1_alternate(a: i32, d: i32) -> (i32, i32) {
    let mut a = a;
    let mut d = d;

    a = a.wrapping_add(d);
    d = (a >> 1).wrapping_sub(d);
    a = a.wrapping_add((d.wrapping_mul(3)) >> 3);
    d = d.wrapping_add((a.wrapping_mul(3)) >> 4);

    d = d.wrapping_add(a >> 7);
    d = d.wrapping_sub(a >> 10);

    (a, d)
}

#[inline]
pub fn str_hst_dec(a: i32, b: i32, c: i32, d: i32) -> (i32, i32, i32, i32) {
    let mut a = a;
    let mut b = b;
    let mut d = d;

    b = b.wrapping_sub(c);
    a = a.wrapping_add((d.wrapping_mul(3).wrapping_add(4)) >> 3);

    d = d.wrapping_sub(b >> 1);
    let c_out = ((a.wrapping_sub(b)) >> 1).wrapping_sub(c);

    (a.wrapping_sub(c_out), b.wrapping_add(d), d, c_out)
}

/// First-level post-filter, applied to a 16-element block in a specific
/// permutation. Mirrors calibre's `strPost4x4Stage2Split_alternate`.
pub fn str_post_4x4_stage2_split_alternate(input: &[i32; 16]) -> [i32; 16] {
    let (mut p0m96, mut p0m32, mut p0p32, mut p0p96,
         mut p0m80, mut p0m16, mut p0p48, mut p0p112,
         mut p1m128, mut p1m64, mut p1p0, mut p1p64,
         mut p1m112, mut p1m48, mut p1p16, mut p1p80) = (
        input[0], input[1], input[2], input[3],
        input[4], input[5], input[6], input[7],
        input[8], input[9], input[10], input[11],
        input[12], input[13], input[14], input[15],
    );

    let (a, b, c, d) = str_dct2x2dn(p0m96, p0p96, p1m112, p1p80);
    p0m96 = a; p0p96 = b; p1m112 = c; p1p80 = d;

    let (a, b, c, d) = str_dct2x2dn(p0m32, p0p32, p1m48, p1p16);
    p0m32 = a; p0p32 = b; p1m48 = c; p1p16 = d;

    let (a, b, c, d) = str_dct2x2dn(p0m80, p0p112, p1m128, p1p64);
    p0m80 = a; p0p112 = b; p1m128 = c; p1p64 = d;

    let (a, b, c, d) = str_dct2x2dn(p0m16, p0p48, p1m64, p1p0);
    p0m16 = a; p0p48 = b; p1m64 = c; p1p0 = d;

    let (a, b, c, d) = inv_odd_odd_post(p1p0, p1p64, p1p16, p1p80);
    p1p0 = a; p1p64 = b; p1p16 = c; p1p80 = d;

    let (a, b) = irotate1(p0p48, p0p32); p0p48 = a; p0p32 = b;
    let (a, b) = irotate1(p0p112, p0p96); p0p112 = a; p0p96 = b;
    let (a, b) = irotate1(p1m64, p1m128); p1m64 = a; p1m128 = b;
    let (a, b) = irotate1(p1m48, p1m112); p1m48 = a; p1m112 = b;

    let (a, d) = str_hst_dec1_alternate(p0m96, p1p80); p0m96 = a; p1p80 = d;
    let (a, d) = str_hst_dec1_alternate(p0m32, p1p16); p0m32 = a; p1p16 = d;
    let (a, d) = str_hst_dec1_alternate(p0m80, p1p64); p0m80 = a; p1p64 = d;
    let (a, d) = str_hst_dec1_alternate(p0m16, p1p0); p0m16 = a; p1p0 = d;

    let (a, b, c, d) = str_hst_dec(p0m96, p1m112, p0p96, p1p80);
    p0m96 = a; p1m112 = b; p0p96 = c; p1p80 = d;
    let (a, b, c, d) = str_hst_dec(p0m32, p1m48, p0p32, p1p16);
    p0m32 = a; p1m48 = b; p0p32 = c; p1p16 = d;
    let (a, b, c, d) = str_hst_dec(p0m80, p1m128, p0p112, p1p64);
    p0m80 = a; p1m128 = b; p0p112 = c; p1p64 = d;
    let (a, b, c, d) = str_hst_dec(p0m16, p1m64, p0p48, p1p0);
    p0m16 = a; p1m64 = b; p0p48 = c; p1p0 = d;

    [
        p0m96, p0m32, p0p32, p0p96, p0m80, p0m16, p0p48, p0p112,
        p1m128, p1m64, p1p0, p1p64, p1m112, p1m48, p1p16, p1p80,
    ]
}

/// `T2x2h` with `valRound` parameter.
pub fn t2x2h(c: [i32; 4], val_round: i32) -> [i32; 4] {
    let mut c0 = c[0];
    let mut c1 = c[1];
    let mut c2 = c[2];
    let mut c3 = c[3];

    c0 = c0.wrapping_add(c3);
    c1 = c1.wrapping_sub(c2);
    let val_t1 = (c0.wrapping_sub(c1).wrapping_add(val_round)) >> 1;
    let val_t2 = c2;
    c2 = val_t1.wrapping_sub(c3);
    c3 = val_t1.wrapping_sub(val_t2);
    c0 = c0.wrapping_sub(c3);
    c1 = c1.wrapping_add(c2);

    [c0, c1, c2, c3]
}

/// `T2x2hPOST` — variant with rotation step.
pub fn t2x2h_post(c: [i32; 4]) -> [i32; 4] {
    let mut c0 = c[0];
    let mut c1 = c[1];
    let mut c2 = c[2];
    let mut c3 = c[3];

    c1 = c1.wrapping_sub(c2);
    c0 = c0.wrapping_add((c3.wrapping_mul(3).wrapping_add(4)) >> 3);
    c3 = c3.wrapping_sub(c1 >> 1);
    c2 = ((c0.wrapping_sub(c1)) >> 1).wrapping_sub(c2);
    // swap c2, c3
    std::mem::swap(&mut c2, &mut c3);
    c0 = c0.wrapping_sub(c3);
    c1 = c1.wrapping_add(c2);

    [c0, c1, c2, c3]
}

/// `OverlapPostFilter4x4` — second-level overlap filter on a 16-coeff block.
pub fn overlap_post_filter_4x4(input: [i32; 16]) -> [i32; 16] {
    let mut c = input;

    let r = t2x2h([c[0], c[3], c[12], c[15]], 0);
    c[0] = r[0]; c[3] = r[1]; c[12] = r[2]; c[15] = r[3];

    let r = t2x2h([c[1], c[2], c[13], c[14]], 0);
    c[1] = r[0]; c[2] = r[1]; c[13] = r[2]; c[14] = r[3];

    let r = t2x2h([c[4], c[7], c[8], c[11]], 0);
    c[4] = r[0]; c[7] = r[1]; c[8] = r[2]; c[11] = r[3];

    let r = t2x2h([c[5], c[6], c[9], c[10]], 0);
    c[5] = r[0]; c[6] = r[1]; c[9] = r[2]; c[10] = r[3];

    let (a, b) = inv_rotate(c[13], c[12]); c[13] = a; c[12] = b;
    let (a, b) = inv_rotate(c[9], c[8]); c[9] = a; c[8] = b;
    let (a, b) = inv_rotate(c[7], c[3]); c[7] = a; c[3] = b;
    let (a, b) = inv_rotate(c[6], c[2]); c[6] = a; c[2] = b;

    let r = inv_toddodd_post([c[10], c[11], c[14], c[15]]);
    c[10] = r[0]; c[11] = r[1]; c[14] = r[2]; c[15] = r[3];

    let (a, b) = inv_scale(c[0], c[15]); c[0] = a; c[15] = b;
    let (a, b) = inv_scale(c[1], c[14]); c[1] = a; c[14] = b;
    let (a, b) = inv_scale(c[4], c[11]); c[4] = a; c[11] = b;
    let (a, b) = inv_scale(c[5], c[10]); c[5] = a; c[10] = b;

    let r = t2x2h_post([c[0], c[3], c[12], c[15]]);
    c[0] = r[0]; c[3] = r[1]; c[12] = r[2]; c[15] = r[3];

    let r = t2x2h_post([c[1], c[2], c[13], c[14]]);
    c[1] = r[0]; c[2] = r[1]; c[13] = r[2]; c[14] = r[3];

    let r = t2x2h_post([c[4], c[7], c[8], c[11]]);
    c[4] = r[0]; c[7] = r[1]; c[8] = r[2]; c[11] = r[3];

    let r = t2x2h_post([c[5], c[6], c[9], c[10]]);
    c[5] = r[0]; c[6] = r[1]; c[9] = r[2]; c[10] = r[3];

    c
}

/// `OverlapPostFilter4` — 4-coeff edge overlap filter.
pub fn overlap_post_filter_4(input: [i32; 4]) -> [i32; 4] {
    let mut c = input;
    c[0] = c[0].wrapping_add(c[3]);
    c[1] = c[1].wrapping_add(c[2]);
    c[3] = c[3].wrapping_sub((c[0].wrapping_add(1)) >> 1);
    c[2] = c[2].wrapping_sub((c[1].wrapping_add(1)) >> 1);
    let (a, b) = inv_scale(c[0], c[3]); c[0] = a; c[3] = b;
    let (a, b) = inv_scale(c[1], c[2]); c[1] = a; c[2] = b;
    c[0] = c[0].wrapping_add((c[3].wrapping_mul(3).wrapping_add(4)) >> 3);
    c[1] = c[1].wrapping_add((c[2].wrapping_mul(3).wrapping_add(4)) >> 3);
    c[3] = c[3].wrapping_sub(c[0] >> 1);
    c[2] = c[2].wrapping_sub(c[1] >> 1);
    c[0] = c[0].wrapping_add(c[3]);
    c[1] = c[1].wrapping_add(c[2]);
    c[3] = c[3].wrapping_neg();
    c[2] = c[2].wrapping_neg();
    let (a, b) = inv_rotate(c[2], c[3]); c[2] = a; c[3] = b;
    c[3] = c[3].wrapping_add((c[0].wrapping_add(1)) >> 1);
    c[2] = c[2].wrapping_add((c[1].wrapping_add(1)) >> 1);
    c[0] = c[0].wrapping_sub(c[3]);
    c[1] = c[1].wrapping_sub(c[2]);
    c
}

#[inline]
pub fn inv_scale(c0: i32, c1: i32) -> (i32, i32) {
    let mut c0 = c0;
    let mut c1 = c1;
    c0 = c0.wrapping_add(c1);
    c1 = (c0 >> 1).wrapping_sub(c1);
    c0 = c0.wrapping_add((c1.wrapping_mul(3)) >> 3);
    c1 = c1.wrapping_add((c0.wrapping_mul(3)) >> 4);
    c1 = c1.wrapping_add(c0 >> 7);
    c1 = c1.wrapping_sub(c0 >> 10);
    (c0, c1)
}

#[inline]
pub fn inv_rotate(c0: i32, c1: i32) -> (i32, i32) {
    let mut c0 = c0;
    let mut c1 = c1;
    c0 = c0.wrapping_sub((c1.wrapping_add(1)) >> 1);
    c1 = c1.wrapping_add((c0.wrapping_add(1)) >> 1);
    (c0, c1)
}

pub fn inv_toddodd_post(input: [i32; 4]) -> [i32; 4] {
    let mut c = input;
    c[3] = c[3].wrapping_add(c[0]);
    c[2] = c[2].wrapping_sub(c[1]);
    let val_t1 = c[3] >> 1;
    let val_t2 = c[2] >> 1;
    c[0] = c[0].wrapping_sub(val_t1);
    c[1] = c[1].wrapping_add(val_t2);
    c[0] = c[0].wrapping_sub((c[1].wrapping_mul(3).wrapping_add(6)) >> 3);
    c[1] = c[1].wrapping_add((c[0].wrapping_mul(3).wrapping_add(2)) >> 2);
    c[0] = c[0].wrapping_sub((c[1].wrapping_mul(3).wrapping_add(4)) >> 3);
    c[1] = c[1].wrapping_sub(val_t2);
    c[0] = c[0].wrapping_add(val_t1);
    c[2] = c[2].wrapping_add(c[1]);
    c[3] = c[3].wrapping_sub(c[0]);
    c
}

/// Table 170 — `OverlapPostFilter2x2`: the chroma 2×2 cross-junction filter
/// (first-level overlap for YUV 4:2:0/4:2:2 block DCs).
pub fn overlap_post_filter_2x2(input: [i32; 4]) -> [i32; 4] {
    let mut c = input;
    c[0] += c[3];
    c[1] += c[2];
    c[3] -= (c[0] + 1) >> 1;
    c[2] -= (c[1] + 1) >> 1;
    c[1] += (c[0] + 2) >> 2;
    c[0] += (c[1] + 1) >> 1;
    c[0] += c[1] >> 5;
    c[0] += c[1] >> 9;
    c[0] += c[1] >> 13;
    c[1] += (c[0] + 2) >> 2;
    c[3] += (c[0] + 1) >> 1;
    c[2] += (c[1] + 1) >> 1;
    c[0] -= c[3];
    c[1] -= c[2];
    c
}

/// Table 171 — `OverlapPostFilter2`: the chroma 2-point edge filter.
pub fn overlap_post_filter_2(input: [i32; 2]) -> [i32; 2] {
    let mut c = input;
    c[1] += (c[0] + 2) >> 2;
    c[0] += (c[1] + 1) >> 1;
    c[0] += c[1] >> 5;
    c[0] += c[1] >> 9;
    c[0] += c[1] >> 13;
    c[1] += (c[0] + 2) >> 2;
    c
}
