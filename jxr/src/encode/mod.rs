//! JPEG-XR encoder, dual grayscale/color.
//!
//! Forward mirror of [`crate::decode`], built bottom-up against the decoder
//! as a round-trip oracle (history: the repo's
//! `.claude/plans/finished_or_stale/jxr-encoder.md`).
//!
//! Status: **grayscale + color complete** — full ALL_BANDS (DC + LP + HP +
//! flexbits), multi-MB prediction, windowing, and per-band quantization
//! ([`quant`]). Grayscale ([`gray`]) is `8bppGray`; color ([`color`]) is
//! `24bppRGB` via the YUV 4:4:4 transform. `QpSet::LOSSLESS` round-trips
//! bit-exact (both modes); `QP > 0` is lossy (ship mode).

pub mod bitstream;
pub mod codestream;
pub mod color;
pub mod container;
pub mod coeff;
pub mod entropy;
pub mod gray;
pub mod hp;
pub mod quant;
pub mod transform;

pub use quant::QpSet;

/// How color is handled on the way into the KFX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// Single luma plane (`8bppGray`). Default: every Kindle we target is B&W
    /// e-ink, and the source EPUB retains the color master, so color is only
    /// dropped from the device copy and is recoverable by reconverting.
    Grayscale,
    /// Full color via the internal YUV 4:4:4 transform + chroma planes
    /// (`24bppRGB`). For desktop/Sidle-reader color; the device is still B&W so
    /// grayscale stays the pipeline default. A 1-plane input encodes as
    /// grayscale even in this mode (no chroma to synthesize).
    Color,
}

/// Errors from the encoder.
#[derive(Debug)]
pub enum EncodeError {
    /// Input the encoder can't represent.
    Unsupported(String),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::Unsupported(s) => write!(f, "unsupported: {s}"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Chroma sampling of the encoded codestream for color (3-/4-plane) inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChromaSampling {
    /// Full-resolution chroma (4:4:4) — the only lossless-capable choice.
    #[default]
    Yuv444,
    /// Horizontally halved chroma (4:2:2). Lossy by construction.
    Yuv422,
    /// Chroma halved both ways (4:2:0). Lossy by construction.
    Yuv420,
    /// Luma only (`-d 0` analog): chroma dropped; decoders reconstruct
    /// gray R=G=B from the transform's luma.
    YOnly,
}

/// Which subbands the codestream carries (T.832 `bands_present`): trailing
/// bands are DROPPED at encode time — a precision/size trade entirely
/// decided by the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BandsPresent {
    /// DC + LP + HP + flexbits (everything).
    #[default]
    All,
    /// DC + LP + HP without the flexbits refinement.
    NoFlexbits,
    /// DC + LP only.
    NoHighpass,
    /// DC only.
    DcOnly,
}

impl BandsPresent {
    fn code(self) -> u8 {
        use crate::decode::consts::{ALL_BANDS, DCONLY, NOFLEXBITS, NOHIGHPASS};
        match self {
            BandsPresent::All => ALL_BANDS,
            BandsPresent::NoFlexbits => NOFLEXBITS,
            BandsPresent::NoHighpass => NOHIGHPASS,
            BandsPresent::DcOnly => DCONLY,
        }
    }
}

/// Encoder options beyond the plain [`encode`] surface. Started as the 4b
/// close-out shape and extended as Phase 4 lands more of the envelope (this
/// is the Phase-7 consolidation point, begun early). `Default` reproduces
/// classic `encode(…, QpSet::LOSSLESS)` behavior.
#[derive(Debug, Clone, Copy)]
pub struct EncodeOptions {
    /// Primary-plane per-band quantizers.
    pub qp: QpSet,
    /// Alpha-plane quantizers (4-plane input); `None` = same as `qp`.
    pub alpha_qp: Option<QpSet>,
    /// Chroma sampling for color inputs (ignored for 1-plane grayscale).
    pub chroma: ChromaSampling,
    /// Subband truncation (`bands_present`).
    pub bands: BandsPresent,
    /// `trim_flexbits` (0–15): drop the low flexbits on emission. Only
    /// meaningful with `bands == All`; ignored otherwise.
    pub trim_flexbits: u8,
    /// Scaled arithmetic (`scaled_flag = 1`): 3 extra fraction bits through
    /// the transforms; chroma DC-LP coded at half amplitude (floor — lossy);
    /// the mode libjxr uses for everything lossy. Exactly invertible for
    /// gray/zero-chroma content; NOT bit-lossless for color at q1.
    pub scaled: bool,
    /// Explicit top window margin (0–63): the coded image gets `window_top`
    /// extra rows above the content (edge-replicated) and the header carries
    /// explicit T.832 window margins (`windowing_flag = 1`) so decoders crop
    /// them. With both 0 (default) the classic derived windowing is used.
    /// Margins are a coded-domain placement knob (e.g. aligning content to
    /// the MB grid); pixels are unaffected.
    pub window_top: u8,
    /// Explicit left window margin (0–63). See [`Self::window_top`].
    pub window_left: u8,
    /// Uniform tile columns (the JxrEncApp `-U` analog): the MB grid splits
    /// into this many near-equal tile columns, each independently entropy-
    /// coded (random access / error resilience; mildly worse compression).
    /// 0 or 1 = single column. More than one tile in either dimension adds
    /// the T.832 index table.
    pub tile_cols: u16,
    /// Uniform tile rows. See [`Self::tile_cols`].
    pub tile_rows: u16,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            qp: QpSet::LOSSLESS,
            alpha_qp: None,
            chroma: ChromaSampling::Yuv444,
            bands: BandsPresent::All,
            trim_flexbits: 0,
            scaled: false,
            window_top: 0,
            window_left: 0,
            tile_cols: 0,
            tile_rows: 0,
        }
    }
}

/// Split `mb` macroblocks into `n` near-equal tile spans (remainder spread
/// over the leading tiles), the uniform-tiling distribution.
fn uniform_tiles(mb: usize, n: usize) -> Vec<usize> {
    let n = n.max(1);
    let (base, rem) = (mb / n, mb % n);
    (0..n).map(|i| base + usize::from(i < rem)).collect()
}

/// Interleaved 8-bit channel orders accepted by [`deinterleave`] — the common
/// memory layouts (`24bppBGR`, `32bppBGRA`, …) normalized to the planar
/// R,G,B(,A) layout [`ImageInput`] takes. Premultiplied variants
/// (PRGBA/PBGRA) are an [`Rgba`](ChannelOrder::Rgba)/[`Bgra`](ChannelOrder::Bgra)
/// byte order plus `ImageInput::premultiplied_alpha = true` — premultiplication
/// is a property flag, not a different layout. The emitted file always uses
/// the canonical GUID for its channel count (`24bppRGB`, `32bppBGRA`/
/// `32bppPBGRA`): byte-order-only GUID variants would change nothing but the
/// order a conformant decoder interleaves its output in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelOrder {
    /// One byte per pixel (`8bppGray`).
    Gray,
    /// R,G,B triplets (`24bppRGB` memory order).
    Rgb,
    /// B,G,R triplets (`24bppBGR` memory order).
    Bgr,
    /// R,G,B,A quads (`32bppRGBA` memory order).
    Rgba,
    /// B,G,R,A quads (`32bppBGRA` memory order).
    Bgra,
}

impl ChannelOrder {
    fn channels(self) -> usize {
        match self {
            ChannelOrder::Gray => 1,
            ChannelOrder::Rgb | ChannelOrder::Bgr => 3,
            ChannelOrder::Rgba | ChannelOrder::Bgra => 4,
        }
    }
    /// Normalized plane index (R,G,B,A order) of interleaved position `i`.
    fn plane_of(self, i: usize) -> usize {
        match self {
            ChannelOrder::Gray | ChannelOrder::Rgb | ChannelOrder::Rgba => i,
            ChannelOrder::Bgr | ChannelOrder::Bgra => match i {
                0 => 2,
                2 => 0,
                other => other,
            },
        }
    }
}

/// De-interleave an 8-bit pixel buffer in the given channel `order` into the
/// normalized planar layout [`ImageInput`] takes (R,G,B\[,A\] planes, or a
/// single gray plane). `bytes.len()` must equal `width × height × channels`.
pub fn deinterleave(
    order: ChannelOrder,
    bytes: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<Vec<u8>>, EncodeError> {
    let n = width as usize * height as usize;
    let ch = order.channels();
    if bytes.len() != n * ch {
        return Err(EncodeError::Unsupported(format!(
            "interleaved buffer len {} != {width}x{height}x{ch}",
            bytes.len()
        )));
    }
    let mut planes = vec![vec![0u8; n]; ch];
    for (px, chunk) in bytes.chunks_exact(ch).enumerate() {
        for (i, &v) in chunk.iter().enumerate() {
            planes[order.plane_of(i)][px] = v;
        }
    }
    Ok(planes)
}

/// 8-bit pixel input. One plane per component, each row-major with
/// `len == width * height`: **1** plane = grayscale, **3** = RGB, **4** =
/// RGB + alpha (encoded as a T.832 alpha image plane). **2** planes
/// (gray+alpha) is rejected: JPEG XR has no grayscale-with-alpha container
/// pixel format (the channels+alpha GUID family starts at 3 channels), so
/// such a file would be unrepresentable — expand to RGBA. Interleaved
/// buffers in common memory orders normalize via [`deinterleave`].
pub struct ImageInput<'a> {
    pub width: u32,
    pub height: u32,
    pub planes: &'a [Vec<u8>],
    /// The alpha plane (4-plane input) holds premultiplied values. Sets the
    /// codestream `premultiplied_alpha_flag` (a property bit, not a different
    /// coding — T.832 8.3.17) and selects the `32bppPBGRA` container GUID.
    pub premultiplied_alpha: bool,
}

/// Encode 8-bit grayscale/RGB(A) pixels into a JPEG-XR file (TIFF container +
/// WMPHOTO codestream) at the given per-band quantizers. `QpSet::LOSSLESS` is
/// bit-exact; higher QP trades fidelity for size (ship mode). An alpha plane
/// (4-plane input) is quantized with the same `qp` — use
/// [`encode_with_alpha_qp`] to quantize it independently. Output is decodable
/// by `decode` and structurally clones a real Amazon JXR.
pub fn encode(
    input: &ImageInput<'_>,
    mode: ColorMode,
    qp: QpSet,
) -> Result<Vec<u8>, EncodeError> {
    encode_with_alpha_qp(input, mode, qp, qp)
}

/// [`encode`] with the alpha image plane quantized by its **own** per-band
/// `alpha_qp` (the JxrEncApp `-Q` analog; the plane carries its own QPs in its
/// plane header). Ignored unless the input has a 4th (alpha) plane.
pub fn encode_with_alpha_qp(
    input: &ImageInput<'_>,
    mode: ColorMode,
    qp: QpSet,
    alpha_qp: QpSet,
) -> Result<Vec<u8>, EncodeError> {
    encode_with_options(
        input,
        mode,
        EncodeOptions { qp, alpha_qp: Some(alpha_qp), ..Default::default() },
    )
}

/// [`encode`] over the full option surface ([`EncodeOptions`]): chroma
/// sampling (4:4:4 / 4:2:2 / 4:2:0 / luma-only), independent alpha QPs, and
/// scaled arithmetic.
pub fn encode_with_options(
    input: &ImageInput<'_>,
    mode: ColorMode,
    opts: EncodeOptions,
) -> Result<Vec<u8>, EncodeError> {
    use crate::decode::consts::{INT_YUV420, INT_YUV422, INT_YUV444};
    let (qp, alpha_qp) = (opts.qp, opts.alpha_qp.unwrap_or(opts.qp));
    let bands = opts.bands.code();
    if opts.trim_flexbits > 15 {
        return Err(EncodeError::Unsupported("trim_flexbits must be 0–15".into()));
    }
    if opts.window_top > 63 || opts.window_left > 63 {
        return Err(EncodeError::Unsupported(
            "window margins are 6-bit fields (0–63)".into(),
        ));
    }
    let window = (opts.window_top as u32, opts.window_left as u32);
    // Tile grid: uniform split of the padded MB grid (which includes the
    // window margins). Each tile must be ≥ 1 MB; counts are 12-bit fields.
    let mb_cols = ((input.width + window.1).div_ceil(16)) as usize;
    let mb_rows = ((input.height + window.0).div_ceil(16)) as usize;
    let (tc, tr) = (opts.tile_cols.max(1) as usize, opts.tile_rows.max(1) as usize);
    if tc > 4096 || tr > 4096 {
        return Err(EncodeError::Unsupported("tile counts are 12-bit fields (≤ 4096)".into()));
    }
    if tc > mb_cols || tr > mb_rows {
        return Err(EncodeError::Unsupported(format!(
            "{tc}x{tr} tiles over a {mb_cols}x{mb_rows} MB grid (every tile needs ≥ 1 MB)"
        )));
    }
    let (tile_cols_mb, tile_rows_mb) = if tc > 1 || tr > 1 {
        (uniform_tiles(mb_cols, tc), uniform_tiles(mb_rows, tr))
    } else {
        (Vec::new(), Vec::new())
    };
    let tiles: (&[usize], &[usize]) = (&tile_cols_mb, &tile_rows_mb);
    let (w, h) = (input.width, input.height);
    if w == 0 || h == 0 {
        return Err(EncodeError::Unsupported("zero-size image".into()));
    }
    if w > 1 << 28 || h > 1 << 28 {
        // The long header carries 32-bit dims, but cap at the sane end of the
        // spec range (the decoder's own decompression budget would reject the
        // grid anyway).
        return Err(EncodeError::Unsupported("dims exceed 2^28".into()));
    }
    let n = w as usize * h as usize;
    let check = |p: &[u8]| {
        if p.len() == n {
            Ok(())
        } else {
            Err(EncodeError::Unsupported("plane len != width*height".into()))
        }
    };
    if input.planes.len() == 2 {
        return Err(EncodeError::Unsupported(
            "gray+alpha: JPEG XR has no grayscale-with-alpha container pixel format; \
             supply RGBA (4 planes)"
                .into(),
        ));
    }
    if input.premultiplied_alpha && input.planes.len() != 4 {
        return Err(EncodeError::Unsupported(
            "premultiplied_alpha set but no alpha plane (4 planes)".into(),
        ));
    }
    if input.planes.len() == 4 {
        if mode != ColorMode::Color {
            return Err(EncodeError::Unsupported(
                "alpha requires ColorMode::Color (no gray+alpha pixel format exists)".into(),
            ));
        }
        for p in input.planes {
            check(p)?;
        }
        if opts.chroma == ChromaSampling::YOnly {
            return Err(EncodeError::Unsupported(
                "YOnly chroma with an alpha plane is not implemented".into(),
            ));
        }
        if opts.bands != BandsPresent::All || opts.trim_flexbits != 0 {
            return Err(EncodeError::Unsupported(
                "band truncation / trim_flexbits with an alpha plane is not implemented".into(),
            ));
        }
        let fmt = match opts.chroma {
            ChromaSampling::Yuv422 => INT_YUV422,
            ChromaSampling::Yuv420 => INT_YUV420,
            _ => INT_YUV444,
        };
        // No auto-gray collapse here even when R==G==B: gray+alpha has no
        // container pixel format, so collapsing would change the declared
        // format — and all-zero chroma planes cost next to nothing.
        return Ok(color::encode_color_alpha(
            &input.planes[0],
            &input.planes[1],
            &input.planes[2],
            &input.planes[3],
            w,
            h,
            qp,
            alpha_qp,
            input.premultiplied_alpha,
            fmt,
            opts.scaled,
            window,
            tiles,
        ));
    }
    // A 1-plane input is grayscale regardless of mode — there's no chroma to
    // synthesize. Color auto-gray-detect (R==G==B) lives in the pipeline.
    let want_color = mode == ColorMode::Color && input.planes.len() != 1;
    if !want_color {
        if input.planes.len() != 1 {
            return Err(EncodeError::Unsupported(format!(
                "grayscale expects 1 plane, got {}",
                input.planes.len()
            )));
        }
        check(&input.planes[0])?;
        // QpSet::LOSSLESS at All bands ⇒ bit-exact; truncation/trim ⇒ lossy.
        return Ok(gray::encode_grayscale_options(
            &input.planes[0],
            w,
            h,
            qp,
            opts.scaled,
            bands,
            opts.trim_flexbits,
            window,
            tiles,
        ));
    }
    if input.planes.len() != 3 {
        return Err(EncodeError::Unsupported(format!(
            "color expects 3 planes (RGB), got {}",
            input.planes.len()
        )));
    }
    check(&input.planes[0])?;
    check(&input.planes[1])?;
    check(&input.planes[2])?;
    // Auto-gray: a "color" image whose channels are identical everywhere carries
    // no chroma — emit `8bppGray` (smaller; the chroma planes would be all-zero;
    // the chroma-sampling choice is moot on a gray image).
    if input.planes[0] == input.planes[1] && input.planes[1] == input.planes[2] {
        return Ok(gray::encode_grayscale_options(
            &input.planes[0],
            w,
            h,
            qp,
            opts.scaled,
            bands,
            opts.trim_flexbits,
            window,
            tiles,
        ));
    }
    let (r, g, b) = (&input.planes[0], &input.planes[1], &input.planes[2]);
    let trim = opts.trim_flexbits;
    match opts.chroma {
        ChromaSampling::YOnly if opts.bands == BandsPresent::All && trim == 0 => {
            Ok(color::encode_yonly_from_color(r, g, b, w, h, qp, opts.scaled, window, tiles))
        }
        ChromaSampling::YOnly => Err(EncodeError::Unsupported(
            "band truncation / trim with YOnly chroma is not implemented".into(),
        )),
        ChromaSampling::Yuv444
            if !opts.scaled
                && opts.bands == BandsPresent::All
                && trim == 0
                && window == (0, 0)
                && tiles.0.is_empty() =>
        {
            // The classic byte-stable path (clone of the original encoder).
            Ok(color::encode_color(r, g, b, w, h, qp))
        }
        ChromaSampling::Yuv444 => Ok(color::encode_color_options(
            r, g, b, w, h, qp, INT_YUV444, bands, opts.scaled, trim, window, tiles,
        )),
        ChromaSampling::Yuv422 => Ok(color::encode_color_options(
            r, g, b, w, h, qp, INT_YUV422, bands, opts.scaled, trim, window, tiles,
        )),
        ChromaSampling::Yuv420 => Ok(color::encode_color_options(
            r, g, b, w, h, qp, INT_YUV420, bands, opts.scaled, trim, window, tiles,
        )),
    }
}

/// Map a 0–100 quality knob to per-band quantizers. 100 ⇒ lossless; lower ⇒
/// coarser, with HP quantized hardest (the `1:2:4` dc:lp:hp ratio Amazon-style).
/// Tuned so the mid-80s land near Amazon's per-plate size on LN content; the
/// default is refined against real plates in the pipeline (Phase 5).
pub fn quality_to_qp(quality: u8) -> QpSet {
    if quality >= 100 {
        return QpSet::LOSSLESS;
    }
    let base = (((100 - quality as i32) + 2) / 3).clamp(1, 40) as u8;
    QpSet { dc: base, lp: base.saturating_mul(2), hp: base.saturating_mul(4) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip oracle: decode JXR bytes straight to i32 planes via the
    /// decoder, bypassing its JPEG re-encode in `jxr_transcode::transcode`.
    fn decode_to_planes(jxr: &[u8]) -> crate::decode::decoder::DecodedImage {
        let container = crate::decode::container::parse(jxr).expect("container parse");
        crate::decode::decoder::Decoder::new(container.image_data)
            .decode()
            .expect("decode")
    }

    #[test]
    fn roundtrip_constant_grayscale_dconly() {
        // DCONLY reconstructs a flat block exactly (LP/HP zero). Across several
        // sizes this also exercises multi-MB DC prediction + model/abs-table
        // adaptation: MB(0,0) codes the full DC, every other MB predicts to 0.
        for &(w, h) in &[(16u32, 16u32), (32, 16), (16, 32), (48, 32)] {
            for val in [128u8, 0, 255, 64, 100, 200] {
                let plane = vec![val; (w * h) as usize];
                let input = ImageInput {
                    width: w,
                    height: h,
                    planes: std::slice::from_ref(&plane),
                    premultiplied_alpha: false,
                };
                let jxr = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).expect("encode");
                let decoded = decode_to_planes(&jxr);
                assert_eq!((decoded.width, decoded.height), (w, h));
                for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                    assert_eq!(got, val as i32, "w={w} h={h} val={val} pixel {i}");
                }
            }
        }
    }

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

    /// Exact inverse of `transform::forward_transform_mb` (no overlap).
    fn inverse_transform_mb(buf: &mut [i32; 256]) -> [i32; 256] {
        use crate::decode::consts::MB_PIXEL_MAP;
        use crate::decode::math::{str_idct4x4_stage1, str_idct4x4_stage2};
        let mut dclp = [0i32; 16];
        for j in 0..16 {
            dclp[j] = buf[j * 16];
        }
        str_idct4x4_stage2(&mut dclp);
        for j in 0..16 {
            buf[j * 16] = dclp[j];
        }
        for j in 0..16 {
            let mut blk = [0i32; 16];
            blk.copy_from_slice(&buf[j * 16..j * 16 + 16]);
            str_idct4x4_stage1(&mut blk);
            buf[j * 16..j * 16 + 16].copy_from_slice(&blk);
        }
        let mut s = [0i32; 256];
        for by in 0..4 {
            for bx in 0..4 {
                let bb = by * 16 + bx * 64;
                for py in 0..4 {
                    for px in 0..4 {
                        s[(by * 4 + py) * 16 + bx * 4 + px] = buf[bb + MB_PIXEL_MAP[px + py * 4]];
                    }
                }
            }
        }
        s
    }

    /// A zero-HP 16×16 block from random pixels: forward, drop HP, inverse.
    /// `None` if any reconstructed pixel would clip (rare; caller retries).
    fn zero_hp_block(r: &mut Lcg) -> Option<[u8; 256]> {
        let mut samples = [0i32; 256];
        for s in samples.iter_mut() {
            *s = (r.next() % 41) as i32 - 20;
        }
        let mut buf = transform::forward_transform_mb(&samples);
        for (p, v) in buf.iter_mut().enumerate() {
            if p % 16 != 0 {
                *v = 0; // drop HP, keep the per-block DC at [k*16]
            }
        }
        let s = inverse_transform_mb(&mut buf);
        let mut out = [0u8; 256];
        for (o, &v) in out.iter_mut().zip(s.iter()) {
            let p = v + 128;
            if !(0..=255).contains(&p) {
                return None;
            }
            *o = p as u8;
        }
        Some(out)
    }

    #[test]
    fn roundtrip_zero_hp_grayscale_nohighpass() {
        // NOHIGHPASS is lossless for zero-HP content: exercises the full LP
        // run-level + refine + prediction path across multi-MB sizes.
        let mut r = Lcg(0xabcd_ef01);
        for &(mbw, mbh) in &[(1usize, 1usize), (2, 1), (1, 2), (2, 2), (3, 2)] {
            let (w, h) = (mbw * 16, mbh * 16);
            let mut pixels = vec![0u8; w * h];
            for mx in 0..mbw {
                for my in 0..mbh {
                    let blk = loop {
                        if let Some(b) = zero_hp_block(&mut r) {
                            break b;
                        }
                    };
                    for py in 0..16 {
                        for px in 0..16 {
                            pixels[(my * 16 + py) * w + (mx * 16 + px)] = blk[py * 16 + px];
                        }
                    }
                }
            }
            let input = ImageInput {
                width: w as u32,
                height: h as u32,
                planes: std::slice::from_ref(&pixels),
                premultiplied_alpha: false,
            };
            let jxr = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).expect("encode");
            let decoded = decode_to_planes(&jxr);
            for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                assert_eq!(got, pixels[i] as i32, "mbw={mbw} mbh={mbh} pixel {i}");
            }
        }
    }

    #[test]
    fn roundtrip_arbitrary_grayscale_allbands_lossless() {
        // The real goal: ANY grayscale image round-trips exactly (ALL_BANDS).
        let mut r = Lcg(0x5151_2727);
        for &(mbw, mbh) in &[(1usize, 1usize), (2, 1), (1, 2), (2, 2), (3, 3)] {
            let (w, h) = (mbw * 16, mbh * 16);
            let pixels: Vec<u8> = (0..w * h).map(|_| (r.next() % 256) as u8).collect();
            let input = ImageInput {
                width: w as u32,
                height: h as u32,
                planes: std::slice::from_ref(&pixels),
                premultiplied_alpha: false,
            };
            let jxr = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).expect("encode");
            let decoded = decode_to_planes(&jxr);
            for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                assert_eq!(got, pixels[i] as i32, "mbw={mbw} mbh={mbh} pixel {i}");
            }
        }
    }

    #[test]
    fn roundtrip_non_aligned_grayscale_lossless() {
        // Arbitrary (non-16-aligned) dimensions: edge-pad + decoder crop.
        let mut r = Lcg(0x9090_3434);
        for &(w, h) in &[(17u32, 31u32), (100, 50), (33, 16), (16, 33), (45, 45), (1, 1)] {
            let pixels: Vec<u8> = (0..(w * h) as usize).map(|_| (r.next() % 256) as u8).collect();
            let input = ImageInput {
                width: w,
                height: h,
                planes: std::slice::from_ref(&pixels),
                premultiplied_alpha: false,
            };
            let jxr = encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).expect("encode");
            let decoded = decode_to_planes(&jxr);
            assert_eq!((decoded.width, decoded.height), (w, h));
            for (i, &got) in decoded.image_plane[0].iter().enumerate() {
                assert_eq!(got, pixels[i] as i32, "w={w} h={h} pixel {i}");
            }
        }
    }

    #[test]
    fn lossy_roundtrip_is_a_fixpoint() {
        // Lossy correctness without an external oracle: a decoded image already
        // sits on the quantization grid, so re-encoding it must yield a
        // byte-identical JXR (encode∘decode∘encode is a fixpoint). This holds
        // iff our forward quantizer is the exact inverse of the decoder's
        // dequant for every band. Mid-range pixels keep the reconstruction in
        // [0,255] so no clamping perturbs the second generation. Aligned sizes
        // only: windowing regenerates edge-padding from the (now lossy) edge, so
        // boundary MBs of a padded image aren't a fixpoint — a property of the
        // padding, not the quantizer.
        let mut r = Lcg(0x1357_9bdf);
        let qps = [
            QpSet { dc: 4, lp: 8, hp: 16 },
            QpSet { dc: 8, lp: 16, hp: 32 },
            QpSet { dc: 1, lp: 4, hp: 6 },
        ];
        for &(w, h) in &[(32u32, 32u32), (48, 32), (64, 48)] {
            let pixels: Vec<u8> = (0..(w * h) as usize).map(|_| 96 + (r.next() % 64) as u8).collect();
            for &qp in &qps {
                let input = ImageInput {
                    width: w,
                    height: h,
                    planes: std::slice::from_ref(&pixels),
                    premultiplied_alpha: false,
                };
                let jxr1 = encode(&input, ColorMode::Grayscale, qp).expect("encode");
                let dec1 = decode_to_planes(&jxr1);
                let p1: Vec<u8> = dec1.image_plane[0].iter().map(|&v| v.clamp(0, 255) as u8).collect();
                let input2 = ImageInput {
                    width: w,
                    height: h,
                    planes: std::slice::from_ref(&p1),
                    premultiplied_alpha: false,
                };
                let jxr2 = encode(&input2, ColorMode::Grayscale, qp).expect("re-encode");
                assert_eq!(jxr1, jxr2, "not a fixpoint at qp={qp:?} {w}x{h}");
            }
        }
    }

    #[test]
    fn lossy_error_grows_with_qp() {
        // Clean synthetic original with energy in every band: coarser quant ⇒
        // strictly more error. The fixpoint test only proves self-consistency;
        // this rules out a deadzone/rounding bug that would still round-trip but
        // quantize badly. (Monotonic on a clean master — unlike PSNR vs Amazon's
        // own already-quantized pixels.)
        let (w, h) = (64usize, 64usize);
        let pixels: Vec<u8> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as i32, (i / w) as i32);
                let v = 110 + (x % 17) - (y % 13) + ((x * y) % 11) * 3; // LP + HP energy
                v.clamp(0, 255) as u8
            })
            .collect();
        let mse = |qp: QpSet| -> f64 {
            let input =
                ImageInput {
                    width: w as u32,
                    height: h as u32,
                    planes: std::slice::from_ref(&pixels),
                    premultiplied_alpha: false,
                };
            let jxr = encode(&input, ColorMode::Grayscale, qp).expect("encode");
            let d = decode_to_planes(&jxr);
            let se: f64 = pixels
                .iter()
                .zip(d.image_plane[0].iter())
                .map(|(&r, &g)| {
                    let e = r as f64 - g.clamp(0, 255) as f64;
                    e * e
                })
                .sum();
            se / (w * h) as f64
        };
        let m0 = mse(QpSet::LOSSLESS);
        let m4 = mse(QpSet { dc: 16, lp: 16, hp: 16 }); // sf = 4
        let m8 = mse(QpSet { dc: 32, lp: 32, hp: 32 }); // sf = 8
        assert_eq!(m0, 0.0, "lossless must be exact");
        assert!(m4 > 0.0 && m8 > m4, "error must grow with QP: m0={m0} m4={m4} m8={m8}");
    }

    #[test]
    fn encode_color_via_public_api() {
        // 3-plane Color round-trips exact; 1-plane Color falls back to grayscale.
        let mut r = Lcg(0x1111_2222_3333_4444);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        // High bits: the LCG's low 8 bits alias at period 256, which would make
        // R==G==B (zero chroma) when n is a multiple of 256.
        let rp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let gp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let bp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let planes = [rp.clone(), gp.clone(), bp.clone()];
        let input = ImageInput { width: w, height: h, planes: &planes, premultiplied_alpha: false };
        let jxr = encode(&input, ColorMode::Color, QpSet::LOSSLESS).expect("color encode");
        let d = decode_to_planes(&jxr);
        assert_eq!(d.num_components, 3, "3-plane color must emit RGB");
        for i in 0..n {
            assert_eq!(
                (d.image_plane[0][i], d.image_plane[1][i], d.image_plane[2][i]),
                (rp[i] as i32, gp[i] as i32, bp[i] as i32),
                "pixel {i}"
            );
        }
        // 1-plane in Color mode → grayscale (1 component, no synthesized chroma).
        let gplanes = [rp.clone()];
        let ginput = ImageInput { width: w, height: h, planes: &gplanes, premultiplied_alpha: false };
        let gjxr = encode(&ginput, ColorMode::Color, QpSet::LOSSLESS).expect("gray fallback");
        assert_eq!(decode_to_planes(&gjxr).num_components, 1, "1-plane color-mode ⇒ grayscale");
    }

    #[test]
    fn color_mode_auto_gray_detects_equal_channels() {
        // Three identical channels carry no chroma → must emit 8bppGray.
        let mut r = Lcg(0xaaaa_5555_cccc_3333);
        let (w, h) = (32u32, 16u32);
        let n = (w * h) as usize;
        let plane: Vec<u8> = (0..n).map(|_| (r.next() % 256) as u8).collect();
        let planes = [plane.clone(), plane.clone(), plane.clone()];
        let input = ImageInput { width: w, height: h, planes: &planes, premultiplied_alpha: false };
        let jxr = encode(&input, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        let d = decode_to_planes(&jxr);
        assert_eq!(d.num_components, 1, "equal RGB channels must auto-detect to grayscale");
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], plane[i] as i32, "pixel {i}");
        }
    }

    fn noise_planes<const N: usize>(r: &mut Lcg, n: usize) -> [Vec<u8>; N] {
        std::array::from_fn(|_| (0..n).map(|_| (r.next() >> 32) as u8).collect())
    }

    #[test]
    fn roundtrip_rgba_alpha_plane_lossless() {
        // 4-plane RGBA → YUV444 primary plane + YONLY alpha image plane,
        // per-MB interleaved; all four channels bit-exact at LOSSLESS,
        // including non-16-aligned dims (both planes pad + crop identically).
        let mut r = Lcg(0xfeed_f00d_1234_5678);
        for &(w, h) in &[(48u32, 32u32), (17, 31), (16, 16), (100, 50)] {
            let n = (w * h) as usize;
            let planes: [Vec<u8>; 4] = noise_planes(&mut r, n);
            let input =
                ImageInput { width: w, height: h, planes: &planes, premultiplied_alpha: false };
            let jxr = encode(&input, ColorMode::Color, QpSet::LOSSLESS).expect("rgba encode");
            let d = decode_to_planes(&jxr);
            assert_eq!(d.num_components, 4, "{w}x{h}: 3 primary + alpha");
            assert!(d.has_alpha && !d.premultiplied_alpha, "{w}x{h}: alpha flags");
            for c in 0..4 {
                for i in 0..n {
                    assert_eq!(d.image_plane[c][i], planes[c][i] as i32, "{w}x{h} ch{c} px{i}");
                }
            }
        }
    }

    #[test]
    fn rgba_alpha_own_qp_is_independent() {
        // The alpha plane carries its own QPs in its plane header: a lossy
        // alpha leaves the lossless RGB bit-exact, and conversely.
        let mut r = Lcg(0xa1fa_0000_dead_beef);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        let planes: [Vec<u8>; 4] = noise_planes(&mut r, n);
        let input = ImageInput { width: w, height: h, planes: &planes, premultiplied_alpha: false };
        let lossy = QpSet { dc: 32, lp: 64, hp: 128 };

        let jxr = encode_with_alpha_qp(&input, ColorMode::Color, QpSet::LOSSLESS, lossy).unwrap();
        let d = decode_to_planes(&jxr);
        for c in 0..3 {
            for i in 0..n {
                assert_eq!(d.image_plane[c][i], planes[c][i] as i32, "RGB must stay exact ch{c}");
            }
        }
        assert!(
            (0..n).any(|i| d.image_plane[3][i] != planes[3][i] as i32),
            "noise alpha at dc32/lp64/hp128 must show quantization error"
        );

        let jxr2 = encode_with_alpha_qp(&input, ColorMode::Color, lossy, QpSet::LOSSLESS).unwrap();
        let d2 = decode_to_planes(&jxr2);
        for i in 0..n {
            assert_eq!(d2.image_plane[3][i], planes[3][i] as i32, "alpha must stay exact px{i}");
        }
        assert!(
            (0..3).any(|c| (0..n).any(|i| d2.image_plane[c][i] != planes[c][i] as i32)),
            "noise RGB at dc32/lp64/hp128 must show quantization error"
        );
    }

    #[test]
    fn rgba_premultiplied_flag_passthrough() {
        // premultiplied_alpha → `32bppPBGRA` GUID + the codestream property
        // bit (T.832 8.3.17), exposed by the decoder and the PixelBuffer.
        use crate::decode::pixels::AlphaMode;
        let mut r = Lcg(0x9e37_79b9_7f4a_7c15);
        let (w, h) = (32u32, 16u32);
        let n = (w * h) as usize;
        let planes: [Vec<u8>; 4] = noise_planes(&mut r, n);
        let input = ImageInput { width: w, height: h, planes: &planes, premultiplied_alpha: true };
        let jxr = encode(&input, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        let c = crate::decode::container::parse(&jxr).expect("container");
        assert_eq!(c.pixel_format_uuid, "24c3dd6f-034e-fe4b-b185-3d77768dc910", "32bppPBGRA");
        let d = crate::decode::decode_image(&c).expect("decode");
        assert!(d.has_alpha && d.premultiplied_alpha);
        let pb = d.to_pixel_buffer().expect("pixel buffer");
        assert_eq!(pb.alpha, AlphaMode::Premultiplied);
        assert_eq!(pb.channels, 4);

        let input2 =
            ImageInput { width: w, height: h, planes: &planes, premultiplied_alpha: false };
        let jxr2 = encode(&input2, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        let c2 = crate::decode::container::parse(&jxr2).expect("container");
        assert_eq!(c2.pixel_format_uuid, "24c3dd6f-034e-fe4b-b185-3d77768dc90f", "32bppBGRA");
        let d2 = crate::decode::decode_image(&c2).expect("decode");
        assert!(d2.has_alpha && !d2.premultiplied_alpha);
        assert_eq!(d2.to_pixel_buffer().unwrap().alpha, AlphaMode::Straight);
    }

    #[test]
    fn rgba_equal_channels_stays_color() {
        // R==G==B with an alpha plane must NOT auto-gray (gray+alpha has no
        // container pixel format): stays a 4-component 32bppBGRA file.
        let mut r = Lcg(0x5555_aaaa_5555_aaaa);
        let (w, h) = (32u32, 16u32);
        let n = (w * h) as usize;
        let g: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let a: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let planes = [g.clone(), g.clone(), g.clone(), a.clone()];
        let input = ImageInput { width: w, height: h, planes: &planes, premultiplied_alpha: false };
        let jxr = encode(&input, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        let c = crate::decode::container::parse(&jxr).expect("container");
        assert_eq!(c.pixel_format_uuid, "24c3dd6f-034e-fe4b-b185-3d77768dc90f");
        let d = decode_to_planes(&jxr);
        assert_eq!(d.num_components, 4, "no auto-gray with alpha");
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], g[i] as i32);
            assert_eq!(d.image_plane[3][i], a[i] as i32);
        }
    }

    #[test]
    fn rgba_lossy_fixpoint_with_alpha() {
        // encode∘decode∘encode is byte-identical with lossy QPs on both planes
        // (alpha QP ≠ primary QP) — the quantizer-inversion discipline extended
        // to the alpha plane. Mid-range pixels + aligned dims, as in the
        // gray/color fixpoint tests.
        let mut r = Lcg(0x0ddc_0ffe_e123_4567);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        let mk = |r: &mut Lcg| -> Vec<u8> {
            (0..n).map(|_| 96 + ((r.next() >> 32) as u8 % 64)).collect()
        };
        let planes = [mk(&mut r), mk(&mut r), mk(&mut r), mk(&mut r)];
        let (qp, aqp) = (QpSet { dc: 4, lp: 8, hp: 16 }, QpSet { dc: 8, lp: 16, hp: 32 });
        let input = ImageInput { width: w, height: h, planes: &planes, premultiplied_alpha: false };
        let jxr1 = encode_with_alpha_qp(&input, ColorMode::Color, qp, aqp).unwrap();
        let d = decode_to_planes(&jxr1);
        let ch = |c: usize| -> Vec<u8> {
            d.image_plane[c].iter().map(|&v| v.clamp(0, 255) as u8).collect()
        };
        let p2 = [ch(0), ch(1), ch(2), ch(3)];
        let input2 = ImageInput { width: w, height: h, planes: &p2, premultiplied_alpha: false };
        let jxr2 = encode_with_alpha_qp(&input2, ColorMode::Color, qp, aqp).unwrap();
        assert_eq!(jxr1, jxr2, "rgba lossy must be a fixpoint");
    }

    #[test]
    fn deinterleave_orders_normalize_to_rgb_planes() {
        // Two pixels in each memory order; planes come out R,G,B(,A) always.
        let bgra = [1u8, 2, 3, 4, 5, 6, 7, 8]; // px0 = B1 G2 R3 A4
        let p = deinterleave(ChannelOrder::Bgra, &bgra, 2, 1).unwrap();
        assert_eq!(p, vec![vec![3, 7], vec![2, 6], vec![1, 5], vec![4, 8]]);
        let rgba = [3u8, 2, 1, 4, 7, 6, 5, 8]; // same pixels, R,G,B,A order
        assert_eq!(deinterleave(ChannelOrder::Rgba, &rgba, 2, 1).unwrap(), p);
        let bgr = [1u8, 2, 3, 5, 6, 7];
        let rgb = [3u8, 2, 1, 7, 6, 5];
        let p3 = deinterleave(ChannelOrder::Bgr, &bgr, 2, 1).unwrap();
        assert_eq!(p3, vec![vec![3, 7], vec![2, 6], vec![1, 5]]);
        assert_eq!(deinterleave(ChannelOrder::Rgb, &rgb, 2, 1).unwrap(), p3);
        // Gray = single-plane passthrough.
        assert_eq!(
            deinterleave(ChannelOrder::Gray, &[9u8, 10], 2, 1).unwrap(),
            vec![vec![9, 10]]
        );
        // Wrong buffer length is rejected.
        assert!(deinterleave(ChannelOrder::Rgb, &[0u8; 5], 2, 1).is_err());
    }

    #[test]
    fn bgra_and_rgba_inputs_encode_byte_identical() {
        // The same pixels handed over in BGRA vs RGBA memory order must yield
        // the SAME file (orderings are input sugar; the codestream and the
        // canonical 32bppBGRA container don't depend on the source order).
        let mut r = Lcg(0xc0de_ba5e_c0de_ba5e);
        let (w, h) = (32u32, 16u32);
        let n = (w * h) as usize;
        let rgba: Vec<u8> = (0..n * 4).map(|_| (r.next() >> 32) as u8).collect();
        let bgra: Vec<u8> = rgba
            .chunks_exact(4)
            .flat_map(|px| [px[2], px[1], px[0], px[3]])
            .collect();
        let pa = deinterleave(ChannelOrder::Rgba, &rgba, w, h).unwrap();
        let pb = deinterleave(ChannelOrder::Bgra, &bgra, w, h).unwrap();
        assert_eq!(pa, pb, "normalized planes must be identical");
        let ia = ImageInput { width: w, height: h, planes: &pa, premultiplied_alpha: false };
        let ib = ImageInput { width: w, height: h, planes: &pb, premultiplied_alpha: false };
        let fa = encode(&ia, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        let fb = encode(&ib, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        assert_eq!(fa, fb, "same pixels, different input order ⇒ same file");
        // And the file really carries those pixels (spot the first pixel).
        let d = decode_to_planes(&fa);
        assert_eq!(
            (d.image_plane[0][0], d.image_plane[1][0], d.image_plane[2][0], d.image_plane[3][0]),
            (rgba[0] as i32, rgba[1] as i32, rgba[2] as i32, rgba[3] as i32)
        );
    }

    #[test]
    fn encode_with_options_dispatch() {
        use crate::decode::container::parse;
        let mut r = Lcg(0x0715_0042_0715_0042);
        let (w, h) = (32u32, 16u32);
        let n = (w * h) as usize;
        let rp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let gp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let bp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let planes = [rp.clone(), gp.clone(), bp.clone()];
        let input = ImageInput { width: w, height: h, planes: &planes, premultiplied_alpha: false };
        let dec = |jxr: &[u8]| {
            let c = parse(jxr).unwrap();
            crate::decode::decode_image(&c).unwrap()
        };
        // Default == classic encode, byte-for-byte.
        let a = encode_with_options(&input, ColorMode::Color, EncodeOptions::default()).unwrap();
        let b = encode(&input, ColorMode::Color, QpSet::LOSSLESS).unwrap();
        assert_eq!(a, b, "Default options must reproduce classic encode bytes");
        // 4:2:0 via options: decodes to shape.
        let f = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions { chroma: ChromaSampling::Yuv420, ..Default::default() },
        )
        .unwrap();
        let d = dec(&f);
        assert_eq!((d.width, d.height, d.num_components), (w, h, 3));
        // YOnly: gray replication.
        let f = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions { chroma: ChromaSampling::YOnly, ..Default::default() },
        )
        .unwrap();
        let d = dec(&f);
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], d.image_plane[1][i]);
        }
        // Scaled 444 lossless: bounded error (chroma half-step only).
        let f = encode_with_options(
            &input,
            ColorMode::Color,
            EncodeOptions { scaled: true, ..Default::default() },
        )
        .unwrap();
        let d = dec(&f);
        for i in 0..n {
            assert!((d.image_plane[0][i] - rp[i] as i32).abs() <= 2);
        }
        // YOnly + alpha rejected.
        let four = [rp.clone(), gp.clone(), bp.clone(), rp.clone()];
        let input4 = ImageInput { width: w, height: h, planes: &four, premultiplied_alpha: false };
        assert!(encode_with_options(
            &input4,
            ColorMode::Color,
            EncodeOptions { chroma: ChromaSampling::YOnly, ..Default::default() },
        )
        .is_err());
        // 420 + alpha works.
        let f = encode_with_options(
            &input4,
            ColorMode::Color,
            EncodeOptions { chroma: ChromaSampling::Yuv420, ..Default::default() },
        )
        .unwrap();
        let d = dec(&f);
        assert_eq!(d.num_components, 4);
        assert!(d.has_alpha);
    }

    /// 4c: band truncation + trim_flexbits + long header via EncodeOptions.
    #[test]
    fn bands_trim_and_long_header() {
        use crate::decode::container::parse;
        let mut r = Lcg(0x0042_4c00_0042_4c00);
        let (w, h) = (48u32, 32u32);
        let n = (w * h) as usize;
        let gray: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let planes = [gray.clone()];
        let input = ImageInput { width: w, height: h, planes: &planes, premultiplied_alpha: false };
        let dec = |jxr: &[u8]| {
            let c = parse(jxr).unwrap();
            crate::decode::decode_image(&c).unwrap()
        };
        let mse = |jxr: &[u8]| -> f64 {
            let d = dec(jxr);
            gray.iter()
                .zip(d.image_plane[0].iter())
                .map(|(&a, &b)| {
                    let e = a as f64 - b.clamp(0, 255) as f64;
                    e * e
                })
                .sum::<f64>()
                / n as f64
        };
        // Bands truncate monotonically: every level decodes, error grows,
        // size shrinks.
        let opts = |bands: BandsPresent, trim: u8| EncodeOptions {
            bands,
            trim_flexbits: trim,
            ..Default::default()
        };
        let all = encode_with_options(&input, ColorMode::Grayscale, opts(BandsPresent::All, 0)).unwrap();
        let noflex =
            encode_with_options(&input, ColorMode::Grayscale, opts(BandsPresent::NoFlexbits, 0)).unwrap();
        let nohp =
            encode_with_options(&input, ColorMode::Grayscale, opts(BandsPresent::NoHighpass, 0)).unwrap();
        let dconly =
            encode_with_options(&input, ColorMode::Grayscale, opts(BandsPresent::DcOnly, 0)).unwrap();
        assert_eq!(mse(&all), 0.0, "All bands lossless must be exact");
        let (m_nf, m_nh, m_dc) = (mse(&noflex), mse(&nohp), mse(&dconly));
        assert!(m_nf > 0.0 && m_nh > m_nf && m_dc > m_nh, "{m_nf} {m_nh} {m_dc}");
        assert!(noflex.len() < all.len() && nohp.len() < noflex.len() && dconly.len() < nohp.len());
        // Trim: error grows with trim at All bands; trim=15 ≈ NoFlexbits-ish.
        let t4 = encode_with_options(&input, ColorMode::Grayscale, opts(BandsPresent::All, 4)).unwrap();
        let t15 = encode_with_options(&input, ColorMode::Grayscale, opts(BandsPresent::All, 15)).unwrap();
        let (m_t4, m_t15) = (mse(&t4), mse(&t15));
        assert!(m_t4 > 0.0 && m_t15 >= m_t4, "{m_t4} {m_t15}");
        assert!(t4.len() < all.len() && t15.len() < t4.len());
        // Same for the color path (420 + trim decodes fine).
        let rp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let gp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let bp: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
        let cplanes = [rp, gp, bp];
        let cinput =
            ImageInput { width: w, height: h, planes: &cplanes, premultiplied_alpha: false };
        for (chroma, bands, trim) in [
            (ChromaSampling::Yuv444, BandsPresent::NoFlexbits, 0u8),
            (ChromaSampling::Yuv444, BandsPresent::All, 6),
            (ChromaSampling::Yuv420, BandsPresent::All, 6),
            (ChromaSampling::Yuv420, BandsPresent::NoHighpass, 0),
        ] {
            let f = encode_with_options(
                &cinput,
                ColorMode::Color,
                EncodeOptions { chroma, bands, trim_flexbits: trim, ..Default::default() },
            )
            .unwrap();
            let d = dec(&f);
            assert_eq!((d.width, d.height, d.num_components), (w, h, 3));
        }
        // Long header: dims beyond 2^16 encode + decode exactly.
        let (lw, lh) = (70_000u32, 16u32);
        let big: Vec<u8> = (0..(lw as usize * 16)).map(|i| (i % 251) as u8).collect();
        let bplanes = [big.clone()];
        let binput =
            ImageInput { width: lw, height: lh, planes: &bplanes, premultiplied_alpha: false };
        let f = encode(&binput, ColorMode::Grayscale, QpSet::LOSSLESS).unwrap();
        let d = dec(&f);
        assert_eq!((d.width, d.height), (lw, lh));
        for (i, &v) in d.image_plane[0].iter().enumerate() {
            assert_eq!(v, big[i] as i32, "long-header px{i}");
        }
    }

    /// 4c: explicit window margins (`windowing_flag = 1`). The image sits at
    /// `(top, left)` inside the coded grid; decoders crop the margins away, so
    /// every lossless-exact path must stay exact — including odd margins over
    /// subsampled chroma (gray content) and the alpha plane (which shares the
    /// placement).
    #[test]
    fn explicit_window_margins_roundtrip() {
        use crate::decode::container::parse;
        let mut r = Lcg(0x717d_0042_717d_0042);
        let dec = |jxr: &[u8]| {
            let c = parse(jxr).unwrap();
            crate::decode::decode_image(&c).unwrap()
        };
        let windows = [(1u8, 1u8), (5, 9), (63, 63), (0, 7), (16, 0)];
        for &(w, h) in &[(30u32, 20u32), (48, 32), (16, 16)] {
            let n = (w * h) as usize;
            let gray: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
            let gplanes = [gray.clone()];
            let ginput =
                ImageInput { width: w, height: h, planes: &gplanes, premultiplied_alpha: false };
            let cplanes: [Vec<u8>; 3] = noise_planes(&mut r, n);
            let cinput =
                ImageInput { width: w, height: h, planes: &cplanes, premultiplied_alpha: false };
            let aplanes: [Vec<u8>; 4] = noise_planes(&mut r, n);
            let ainput =
                ImageInput { width: w, height: h, planes: &aplanes, premultiplied_alpha: false };
            for &(top, left) in &windows {
                let opts = EncodeOptions {
                    window_top: top,
                    window_left: left,
                    ..Default::default()
                };
                // Grayscale: exact.
                let f = encode_with_options(&ginput, ColorMode::Grayscale, opts).unwrap();
                let d = dec(&f);
                assert_eq!((d.width, d.height), (w, h), "({top},{left}) {w}x{h}");
                for i in 0..n {
                    assert_eq!(d.image_plane[0][i], gray[i] as i32, "gray ({top},{left}) px{i}");
                }
                // Color 4:4:4: exact.
                let f = encode_with_options(&cinput, ColorMode::Color, opts).unwrap();
                let d = dec(&f);
                for c in 0..3 {
                    for i in 0..n {
                        assert_eq!(
                            d.image_plane[c][i], cplanes[c][i] as i32,
                            "rgb ({top},{left}) ch{c} px{i}"
                        );
                    }
                }
                // RGBA: exact on all four channels.
                let f = encode_with_options(&ainput, ColorMode::Color, opts).unwrap();
                let d = dec(&f);
                assert_eq!(d.num_components, 4);
                for c in 0..4 {
                    for i in 0..n {
                        assert_eq!(
                            d.image_plane[c][i], aplanes[c][i] as i32,
                            "rgba ({top},{left}) ch{c} px{i}"
                        );
                    }
                }
                // 4:2:0 with gray content (zero chroma): exact even at odd margins.
                let gcolor = [gray.clone(), gray.clone(), gray.clone()];
                // (auto-gray would collapse this; go through the color driver.)
                let f = color::encode_color_options(
                    &gcolor[0],
                    &gcolor[1],
                    &gcolor[2],
                    w,
                    h,
                    QpSet::LOSSLESS,
                    crate::decode::consts::INT_YUV420,
                    crate::decode::consts::ALL_BANDS,
                    false,
                    0,
                    (top as u32, left as u32),
                    (&[], &[]),
                );
                let d = dec(&f);
                for i in 0..n {
                    assert_eq!(d.image_plane[0][i], gray[i] as i32, "420 ({top},{left}) px{i}");
                }
            }
        }
        // Margins are 6-bit fields: 64 is rejected.
        let p = vec![0u8; 256];
        let planes = [p.clone()];
        let input = ImageInput { width: 16, height: 16, planes: &planes, premultiplied_alpha: false };
        assert!(encode_with_options(
            &input,
            ColorMode::Grayscale,
            EncodeOptions { window_top: 64, ..Default::default() },
        )
        .is_err());
    }

    /// 4c: tiling. Multi-tile files (index table, per-tile packets, per-tile
    /// entropy resets, tile-relative prediction edges) round-trip exactly at
    /// lossless across grids, content kinds, and tiles×window combos; and
    /// because tiling only re-segments the entropy stream — coefficients are
    /// untouched — a tiled lossy decode must equal the untiled one
    /// pixel-for-pixel.
    #[test]
    fn tiled_roundtrip_and_equivalence() {
        use crate::decode::container::parse;
        let mut r = Lcg(0x7113_d042_7113_d042);
        let dec = |jxr: &[u8]| {
            let c = parse(jxr).unwrap();
            crate::decode::decode_image(&c).unwrap()
        };
        // 600px = 38 MB columns: 2 columns ⇒ 19-MB tiles, so the within-tile
        // 16-MB adapt cadence fires mid-tile too.
        for &(w, h, tc, tr) in &[
            (600u32, 32u32, 2u16, 2u16),
            (96, 96, 3, 2),
            (64, 64, 4, 1),
            (64, 64, 1, 4),
            (48, 32, 3, 2), // uneven split: 3 MBs / 3 cols, 2 MBs / 2 rows
            (17, 31, 2, 2), // non-aligned dims
        ] {
            let n = (w * h) as usize;
            let gray: Vec<u8> = (0..n).map(|_| (r.next() >> 32) as u8).collect();
            let gplanes = [gray.clone()];
            let ginput =
                ImageInput { width: w, height: h, planes: &gplanes, premultiplied_alpha: false };
            let opts = EncodeOptions { tile_cols: tc, tile_rows: tr, ..Default::default() };
            let f = encode_with_options(&ginput, ColorMode::Grayscale, opts).unwrap();
            let d = dec(&f);
            assert_eq!((d.width, d.height), (w, h));
            for i in 0..n {
                assert_eq!(d.image_plane[0][i], gray[i] as i32, "gray {tc}x{tr} {w}x{h} px{i}");
            }
            // Color + RGBA, exact at lossless.
            let cplanes: [Vec<u8>; 4] = noise_planes(&mut r, n);
            let cinput = ImageInput {
                width: w,
                height: h,
                planes: &cplanes[..3],
                premultiplied_alpha: false,
            };
            let f = encode_with_options(&cinput, ColorMode::Color, opts).unwrap();
            let d = dec(&f);
            for c in 0..3 {
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[c][i], cplanes[c][i] as i32,
                        "rgb {tc}x{tr} {w}x{h} ch{c} px{i}"
                    );
                }
            }
            let ainput =
                ImageInput { width: w, height: h, planes: &cplanes, premultiplied_alpha: false };
            let f = encode_with_options(&ainput, ColorMode::Color, opts).unwrap();
            let d = dec(&f);
            assert_eq!(d.num_components, 4);
            for c in 0..4 {
                for i in 0..n {
                    assert_eq!(
                        d.image_plane[c][i], cplanes[c][i] as i32,
                        "rgba {tc}x{tr} {w}x{h} ch{c} px{i}"
                    );
                }
            }
        }
        // Tiled lossy == untiled lossy, pixel-for-pixel (and likewise at 420).
        let (w, h) = (96u32, 64u32);
        let n = (w * h) as usize;
        let cplanes: [Vec<u8>; 3] = noise_planes(&mut r, n);
        let cinput =
            ImageInput { width: w, height: h, planes: &cplanes, premultiplied_alpha: false };
        for chroma in [ChromaSampling::Yuv444, ChromaSampling::Yuv420] {
            let qp = QpSet { dc: 16, lp: 32, hp: 64 };
            let base = EncodeOptions { qp, chroma, scaled: true, ..Default::default() };
            let untiled = encode_with_options(&cinput, ColorMode::Color, base).unwrap();
            let tiled = encode_with_options(
                &cinput,
                ColorMode::Color,
                EncodeOptions { tile_cols: 3, tile_rows: 2, ..base },
            )
            .unwrap();
            let (du, dt) = (dec(&untiled), dec(&tiled));
            for c in 0..3 {
                assert_eq!(
                    du.image_plane[c], dt.image_plane[c],
                    "tiling must not change reconstruction ({chroma:?} ch{c})"
                );
            }
        }
        // Tiles × explicit window margins.
        let gplanes = [cplanes[0].clone()];
        let ginput =
            ImageInput { width: w, height: h, planes: &gplanes, premultiplied_alpha: false };
        let f = encode_with_options(
            &ginput,
            ColorMode::Grayscale,
            EncodeOptions {
                tile_cols: 2,
                tile_rows: 2,
                window_top: 5,
                window_left: 9,
                ..Default::default()
            },
        )
        .unwrap();
        let d = dec(&f);
        assert_eq!((d.width, d.height), (w, h));
        for i in 0..n {
            assert_eq!(d.image_plane[0][i], gplanes[0][i] as i32, "tiles×window px{i}");
        }
        // Large noise tiles: packet offsets beyond 0xfaff exercise the 0xfb
        // 32-bit vlw_esc escape in the index table.
        let (bw, bh) = (768u32, 768u32);
        let bn = (bw * bh) as usize;
        let big: Vec<u8> = (0..bn).map(|_| (r.next() >> 32) as u8).collect();
        let bplanes = [big.clone()];
        let binput =
            ImageInput { width: bw, height: bh, planes: &bplanes, premultiplied_alpha: false };
        let f = encode_with_options(
            &binput,
            ColorMode::Grayscale,
            EncodeOptions { tile_cols: 2, tile_rows: 2, ..Default::default() },
        )
        .unwrap();
        assert!(f.len() > 4 * 0xfb00, "noise file must be big enough to need the escape");
        let d = dec(&f);
        for i in 0..bn {
            assert_eq!(d.image_plane[0][i], big[i] as i32, "vlw-escape px{i}");
        }
        // Validation: every tile needs ≥ 1 MB.
        let small = vec![0u8; 256];
        let splanes = [small];
        let sinput =
            ImageInput { width: 16, height: 16, planes: &splanes, premultiplied_alpha: false };
        assert!(encode_with_options(
            &sinput,
            ColorMode::Grayscale,
            EncodeOptions { tile_cols: 2, ..Default::default() },
        )
        .is_err());
    }

    #[test]
    fn alpha_input_validation() {
        let (w, h) = (16u32, 16u32);
        let n = (w * h) as usize;
        let p = vec![128u8; n];
        // 2 planes: no gray+alpha container pixel format exists.
        let two = [p.clone(), p.clone()];
        let input = ImageInput { width: w, height: h, planes: &two, premultiplied_alpha: false };
        assert!(encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).is_err());
        assert!(encode(&input, ColorMode::Color, QpSet::LOSSLESS).is_err());
        // 4 planes in Grayscale mode: alpha cannot ride a grayscale image.
        let four = [p.clone(), p.clone(), p.clone(), p.clone()];
        let input = ImageInput { width: w, height: h, planes: &four, premultiplied_alpha: false };
        assert!(encode(&input, ColorMode::Grayscale, QpSet::LOSSLESS).is_err());
        // premultiplied flag without an alpha plane.
        let three = [p.clone(), p.clone(), p.clone()];
        let input = ImageInput { width: w, height: h, planes: &three, premultiplied_alpha: true };
        assert!(encode(&input, ColorMode::Color, QpSet::LOSSLESS).is_err());
        // wrong plane length among the 4.
        let bad = [p.clone(), p.clone(), p.clone(), vec![0u8; n - 1]];
        let input = ImageInput { width: w, height: h, planes: &bad, premultiplied_alpha: false };
        assert!(encode(&input, ColorMode::Color, QpSet::LOSSLESS).is_err());
    }
}
