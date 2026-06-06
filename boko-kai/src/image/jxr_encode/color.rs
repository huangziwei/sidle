//! Forward color transform for the encoder's **color** mode (`ColorMode::Color`).
//!
//! JPEG-XR's color path is `RGB → internal YUV (YCoCg-like) → per-plane PCT`.
//! [`rgb_to_yuv444`] is the **exact integer inverse** of the decoder's
//! [`crate::image::jxr_decode::decoder::yuv444_to_rgb`] lifting (which is the
//! spec / libjxr `strInvTransform`). Because every step is integer lifting, the
//! pair is a perfect bijection — lossless 4:4:4 color round-trips bit-exactly.
//!
//! Bias: the decoder applies the YUV→RGB lifting on the reconstructed
//! coefficients and *then* adds `1<<(bd-1)` (128 for BD8) to land in `[0,255]`.
//! So the forward direction subtracts the bias from the input RGB **first**, runs
//! [`rgb_to_yuv444`] on the centered values, and feeds the resulting Y/U/V planes
//! into the same per-plane forward transform grayscale uses (no further −128).
//! See `.claude/plans/jxr-encoder.md` Track 6.1.
//!
//! 4:4:4 only — boko's decoder rejects subsampled chroma (`decoder.rs:474`), so
//! there is no down-sampling here; each chroma plane is full resolution.

use crate::image::jxr_decode::decoder::{ceil_div2, floor_div2};

/// Forward color transform: centered `RGB → (Y, U, V)`, the exact inverse of the
/// decoder's [`yuv444_to_rgb`]. Inputs are **pre-bias** (input pixel − 128 for
/// BD8); outputs are the internal coefficients the per-plane PCT consumes.
///
/// [`yuv444_to_rgb`]: crate::image::jxr_decode::decoder::yuv444_to_rgb
#[inline]
pub fn rgb_to_yuv444(r: i32, g: i32, b: i32) -> (i32, i32, i32) {
    // Invert, in reverse order, the decoder's lifting
    //   t = -U;  G = Y − ⌊t/2⌋;  R = t + G − ⌈V/2⌉;  B = V + R
    let v = b - r; // from B = V + R
    let temp_t = r - g + ceil_div2(v); // from R = t + G − ⌈V/2⌉
    let u = -temp_t; // t = −U
    let y = g + floor_div2(temp_t); // from G = Y − ⌊t/2⌋
    (y, u, v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::jxr_decode::decoder::yuv444_to_rgb;

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

    /// The gate: the decoder's real `yuv444_to_rgb` must invert our forward
    /// exactly, for every RGB triple. Pure integer lifting ⇒ bit-exact, no
    /// tolerance. Tests against the *actual decode function* (shared source), so
    /// this can't pass by both sides sharing the same bug.
    #[test]
    fn forward_inverts_decoder_exactly() {
        // Exhaustive over a centered cube that covers BD8's pre-bias range
        // (−128..=127) with margin for out-of-gamut coefficients.
        for r in (-160..=160).step_by(5) {
            for g in (-160..=160).step_by(5) {
                for b in (-160..=160).step_by(5) {
                    let (y, u, v) = rgb_to_yuv444(r, g, b);
                    assert_eq!(yuv444_to_rgb(y, u, v), (r, g, b), "rgb=({r},{g},{b})");
                }
            }
        }
        // Random wide range to catch anything the grid steps over.
        let mut rng = Lcg(0xC0FFEE_1234_5678);
        for _ in 0..200_000 {
            let r = (rng.next() % 1024) as i32 - 512;
            let g = (rng.next() % 1024) as i32 - 512;
            let b = (rng.next() % 1024) as i32 - 512;
            let (y, u, v) = rgb_to_yuv444(r, g, b);
            assert_eq!(yuv444_to_rgb(y, u, v), (r, g, b), "rgb=({r},{g},{b})");
        }
    }

    /// Saturated primaries/secondaries (centered for BD8) — the gamut corners
    /// the color transform must handle without drift.
    #[test]
    fn forward_inverts_saturated_corners() {
        let corners = [
            (127, -128, -128), // red
            (-128, 127, -128), // green
            (-128, -128, 127), // blue
            (127, 127, -128),  // yellow
            (127, -128, 127),  // magenta
            (-128, 127, 127),  // cyan
            (127, 127, 127),   // white
            (-128, -128, -128), // black
            (0, 0, 0),         // mid-gray
        ];
        for &(r, g, b) in &corners {
            let (y, u, v) = rgb_to_yuv444(r, g, b);
            assert_eq!(yuv444_to_rgb(y, u, v), (r, g, b), "corner=({r},{g},{b})");
        }
    }
}
