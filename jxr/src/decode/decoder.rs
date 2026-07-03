//! JPEG-XR codestream decoder.
//!
//! Faithful port of calibre's `jxr_image.py` decoder pipeline. Reads a
//! WMPHOTO codestream and produces a `DecodedImage` of i32 samples per
//! component which the caller can pack into JPEG/PNG/etc.
//!
//! Identifiers track Python: e.g. `decode_block` and the various
//! `tile_MB_*` methods are matched by `decode_block` and `tile_*_mb_*`
//! here. Where Python relied on instance attributes that span multiple
//! classes, we centralise the state on `Decoder` and pass plane indices
//! into helpers to keep the borrow checker happy.

#![allow(non_snake_case)]
// JPEG-XR is a faithful port of the spec's decode pipeline: explicit index
// loops over parallel per-sample/per-band arrays and the codec's wide
// block-decode parameter lists are intentional, and read clearer than iterator
// or param-struct rewrites would. Allowed deliberately, not by neglect.
#![allow(clippy::needless_range_loop, clippy::too_many_arguments)]

use super::consts::*;
use super::math::*;
use super::misc::{Deserializer, DeserializerError};
use super::state::*;
use super::tables;

/// Errors from codestream decoding. `Unsupported` is reserved for
/// spec-legal input this crate doesn't reconstruct; `Malformed` for input
/// that is internally inconsistent (the hardening guards' verdict on
/// untrusted bytes).
#[derive(Debug)]
pub enum DecodeError {
    /// Bitstream read error (ran out of bits / malformed escape).
    Bits(DeserializerError),
    /// Spec-legal input this decoder does not reconstruct.
    Unsupported(String),
    /// The `WMPHOTO` codestream magic is absent.
    BadSignature(String),
    /// The codestream is internally inconsistent or describes an impossible
    /// geometry (e.g. tile widths exceeding the image, or dimensions that
    /// would allocate far more memory than the input could encode). Distinct
    /// from `Unsupported`, which is reserved for spec-legal input we don't yet
    /// reconstruct. Raised by the hardening guards on untrusted input.
    Malformed(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Bits(e) => write!(f, "{e}"),
            DecodeError::Unsupported(s) => write!(f, "unsupported: {s}"),
            DecodeError::BadSignature(s) => write!(f, "bad signature: {s}"),
            DecodeError::Malformed(s) => write!(f, "malformed: {s}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<DeserializerError> for DecodeError {
    fn from(e: DeserializerError) -> Self {
        DecodeError::Bits(e)
    }
}

type Result<T> = std::result::Result<T, DecodeError>;

// Hardening ceilings for untrusted input (NOT T.832 limits). The decoder
// materialises the entire macroblock grid and sample planes up front, so a
// lying header could otherwise demand gigabytes from a handful of bytes.
// `MAX_MB_COMPONENTS` caps resident memory regardless of input — roughly a
// 4700² RGB or 8192² grayscale image, comfortably under the fuzz RSS limit —
// while the per-byte budget rejects geometries far too large for the
// codestream to actually encode. The floor keeps every small valid file
// admissible (the smallest coded macroblock still costs real bits).
const MAX_MB_COMPONENTS: u64 = 1 << 18;
const MB_BUDGET_PER_BYTE: u64 = 64;
const MB_BUDGET_FLOOR: u64 = 4096;

/// Decoded raster samples per component, sized to the original image area.
#[derive(Clone)]
pub struct DecodedImage {
    /// Original (pre-padding) width in pixels.
    pub width: u32,
    /// Original (pre-padding) height in pixels.
    pub height: u32,
    /// `image_plane[component]` is a flat `Vec<i32>` with row-major layout
    /// `pixel[y][x] = image_plane[c][y * width + x]`.
    pub image_plane: Vec<Vec<i32>>,
    /// Plane count, including alpha when present.
    pub num_components: usize,
    /// `OUTPUT_CLR_FMT` (`consts::OUT_*`) — the value convention the planes
    /// are in after output formatting.
    pub output_clr_fmt: u8,
    /// `OUTPUT_BITDEPTH` (`consts::BD*`).
    pub output_bitdepth: u8,
    /// RGB planes are stored B,G,R (the container GUID decides; packed
    /// formats re-pack accordingly).
    pub red_blue_swapped: bool,
    /// An alpha channel is present as the final plane (in-codestream planar
    /// alpha, or a separate container codestream merged by `decode_image`).
    pub has_alpha: bool,
    /// The codestream's PREMULTIPLIED_ALPHA_FLAG.
    pub premultiplied_alpha: bool,
    /// Wall-clock sub-stage timing of the decode that produced this image.
    pub timing: DecodeTiming,
}

/// Per-image sub-stage timing collected during `Decoder::decode`. Used by
/// the `BOKO_KFX2EPUB_TRACE=1` aggregator to scope the next perf lever.
#[derive(Debug, Default, Clone, Copy)]
pub struct DecodeTiming {
    /// `image_header` + plane headers + index table + vlw_esc/profile_level_info.
    pub header: std::time::Duration,
    /// `coded_tiles`: Huffman + adaptive VLC + dequant. The "parse" phase.
    pub coded_tiles: std::time::Duration,
    /// `sample_reconstruction`: inverse transforms + overlap filters.
    pub sample_recon: std::time::Duration,
    /// `output_formatting` + `construct_image`: color conv, shift, pack.
    pub output_fmt: std::time::Duration,
}

/// Headers-only view of a codestream, returned by
/// [`Decoder::parse_headers`]: the file's coding shape — dimensions,
/// color/depth, structure, per-plane coding parameters — without any
/// entropy or pixel work. The raw `u8` codes are expressed in the
/// [`consts`](crate::decode::consts) vocabulary.
#[derive(Debug, Clone)]
pub struct HeaderSummary {
    /// Original (pre-padding) image width in pixels.
    pub width: u32,
    /// Original (pre-padding) image height in pixels.
    pub height: u32,
    /// `OUTPUT_CLR_FMT` (`consts::OUT_*`).
    pub output_clr_fmt: u8,
    /// `OUTPUT_BITDEPTH` (`consts::BD*`).
    pub output_bitdepth: u8,
    /// `OVERLAP_MODE` (0 = none, 1 = first-level, 2 = both).
    pub overlap_mode: u8,
    /// Frequency order: per-band tile packets instead of spatial order.
    pub frequency_mode: bool,
    /// Tile grid columns (1 = untiled).
    pub tile_cols: usize,
    /// Tile grid rows (1 = untiled).
    pub tile_rows: usize,
    /// Window margins (top, left, bottom, right): coded-grid pixels a
    /// conformant decoder crops away.
    pub margins: (u32, u32, u32, u32),
    /// The codestream carries a T.832 alpha image plane (per-MB interleaved).
    pub alpha_image_plane: bool,
    /// `PREMULTIPLIED_ALPHA_FLAG`.
    pub premultiplied_alpha: bool,
    /// Per-plane coding parameters: `[primary]` or `[primary, alpha]`.
    pub planes: Vec<PlaneSummary>,
}

/// One image plane's header fields (see [`HeaderSummary::planes`]).
#[derive(Debug, Clone)]
pub struct PlaneSummary {
    /// This is the alpha image plane.
    pub is_alpha: bool,
    /// `INTERNAL_CLR_FMT` (`consts::INT_*`).
    pub internal_clr_fmt: u8,
    /// Coded components in this plane.
    pub num_components: usize,
    /// `BANDS_PRESENT` (`consts::{ALL_BANDS, NOFLEXBITS, NOHIGHPASS, DCONLY}`).
    pub bands_present: u8,
    /// `SCALED_FLAG` — scaled (3 extra fraction bits) arithmetic.
    pub scaled: bool,
    /// `SHIFT_BITS` (the 32-bit integer formats' pre-shift).
    pub shift_bits: u32,
    /// Custom-float `LEN_MANTISSA` (float outputs).
    pub len_mantissa: u32,
    /// Custom-float `EXP_BIAS` (float outputs).
    pub exp_bias: i32,
    /// Image-plane-uniform DC quantizer scaling factors,
    /// `[component][qp_set]`; `None` when DC QPs vary per tile.
    pub dc_scaling: Option<Vec<Vec<i32>>>,
    /// LP scaling factors, as [`Self::dc_scaling`] (`None` = per-tile/MB).
    pub lp_scaling: Option<Vec<Vec<i32>>>,
    /// HP scaling factors, as [`Self::dc_scaling`] (`None` = per-tile/MB).
    pub hp_scaling: Option<Vec<Vec<i32>>>,
}

/// Top-level decoder. Owns the bitstream cursor and all plane state.
pub struct Decoder<'a> {
    pub(crate) ds: Deserializer<'a>,
    pub(crate) hdr: ImageHeader,
    pub(crate) planes: Vec<Plane>,

    // Tile-level transient state.
    pub(crate) trim_flexbits: u32,
    pub(crate) index_offset_tile: Vec<u64>,
}

impl<'a> Decoder<'a> {
    /// A decoder over one WMPHOTO codestream (the `image_data` /
    /// `alpha_data` slice a parsed container exposes). No work happens
    /// until [`decode`](Self::decode) or
    /// [`parse_headers`](Self::parse_headers).
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            ds: Deserializer::new(data),
            hdr: ImageHeader {
                hard_tiling_flag: 0,
                tiling_flag: 0,
                frequency_mode: 0,
                spatial_xfrm_subordinate: 0,
                index_table_present_flag: 0,
                overlap_mode: NO_OVERLAP_FILTERING,
                short_header_flag: false,
                long_word_flag: false,
                windowing_flag: 0,
                trim_flexbits_flag: 0,
                red_blue_not_swapped_flag: 0,
                premultiplied_alpha_flag: 0,
                alpha_image_plane_flag: 0,
                output_clr_fmt: OUT_YONLY,
                output_bitdepth: BD8,
                image_width: 0,
                image_height: 0,
                width: 0,
                height: 0,
                num_ver_tiles_minus1: 0,
                num_hor_tiles_minus1: 0,
                num_tile_cols: 1,
                num_tile_rows: 1,
                tile_width_in_mb: vec![],
                tile_height_in_mb: vec![],
                left_mb_index_of_tile: vec![],
                top_mb_index_of_tile: vec![],
                extra_pixels_top: 0,
                extra_pixels_left: 0,
                extra_pixels_bottom: 0,
                extra_pixels_right: 0,
                mb_width: 0,
                mb_height: 0,
            },
            planes: Vec::new(),
            trim_flexbits: 0,
            index_offset_tile: Vec::new(),
        }
    }

    /// Decode the codestream completely: headers, entropy decode, inverse
    /// transforms, overlap post-filters, output formatting. Returns the
    /// reconstructed planes; errors (never panics) on malformed or
    /// unsupported input.
    pub fn decode(mut self) -> Result<DecodedImage> {
        use std::time::Instant;
        let mut timing = DecodeTiming::default();

        let t_hdr = Instant::now();
        self.parse_headers()?;
        let num_bands_primary = self.planes[0].num_bands;

        if self.hdr.index_table_present_flag != 0 {
            self.index_table_tiles(num_bands_primary)?;
        }

        let subsequent_bytes = self.vlw_esc()?;
        if subsequent_bytes > 0 {
            let i_bytes = self.profile_level_info()?;
            if subsequent_bytes != i_bytes {
                // profile_level_info must not claim to have consumed more than
                // the declared subsequent-byte count (else the skip below wraps
                // to a huge length on a u64 underflow).
                let extra = subsequent_bytes.checked_sub(i_bytes).ok_or_else(|| {
                    DecodeError::Malformed(format!(
                        "profile/level info ({i_bytes} B) overruns declared {subsequent_bytes} B"
                    ))
                })?;
                let _ = self.ds.extract(extra as usize, true)?;
            }
        }
        timing.header = t_hdr.elapsed();

        let t_tiles = Instant::now();
        self.coded_tiles(num_bands_primary)?;
        self.ds.discard_remainder_bits();
        timing.coded_tiles = t_tiles.elapsed();

        // SampleReconstruction + OutputFormatting per plane.
        for p in 0..self.planes.len() {
            let t_sr = Instant::now();
            self.sample_reconstruction(p);
            timing.sample_recon += t_sr.elapsed();

            let t_of = Instant::now();
            self.output_formatting(p)?;
            timing.output_fmt += t_of.elapsed();
        }

        let t_out = Instant::now();
        let mut img = self.construct_image();
        timing.output_fmt += t_out.elapsed();
        img.timing = timing;
        Ok(img)
    }

    /// Parse only the image header and image-plane header(s) — no entropy
    /// decode, no pixel work. Cheap format sniffing (dimensions, color/bit
    /// depth, `scaled_flag`, `shift_bits`, `len_mantissa`/`exp_bias`, QPs…);
    /// also how the encoder's external parity gates diff header fields
    /// against reference-encoder output.
    pub fn parse_headers(&mut self) -> Result<HeaderSummary> {
        self.image_header()?;
        self.planes.push(Plane::new(false));
        self.image_plane_header(0)?;
        if self.hdr.alpha_image_plane_flag != 0 {
            self.planes.push(Plane::new(true));
            self.image_plane_header(1)?;
        }
        let h = &self.hdr;
        Ok(HeaderSummary {
            width: h.image_width,
            height: h.image_height,
            output_clr_fmt: h.output_clr_fmt,
            output_bitdepth: h.output_bitdepth,
            overlap_mode: h.overlap_mode,
            frequency_mode: h.frequency_mode != 0,
            tile_cols: h.num_tile_cols,
            tile_rows: h.num_tile_rows,
            margins: (
                h.extra_pixels_top,
                h.extra_pixels_left,
                h.extra_pixels_bottom,
                h.extra_pixels_right,
            ),
            alpha_image_plane: h.alpha_image_plane_flag != 0,
            premultiplied_alpha: h.premultiplied_alpha_flag != 0,
            planes: self
                .planes
                .iter()
                .map(|p| PlaneSummary {
                    is_alpha: p.is_alpha,
                    internal_clr_fmt: p.internal_clr_fmt,
                    num_components: p.num_components,
                    bands_present: p.bands_present,
                    scaled: p.scaled_flag != 0,
                    shift_bits: p.shift_bits,
                    len_mantissa: p.len_mantissa,
                    exp_bias: p.exp_bias,
                    dc_scaling: p.dc_qp.as_ref().map(|q| q.quant_scaling_factor.clone()),
                    lp_scaling: p.lp_qp.as_ref().map(|q| q.quant_scaling_factor.clone()),
                    hp_scaling: p.hp_qp.as_ref().map(|q| q.quant_scaling_factor.clone()),
                })
                .collect(),
        })
    }

    fn image_header(&mut self) -> Result<()> {
        let sig = self.ds.extract(8, true)?;
        if sig != b"WMPHOTO\x00" {
            return Err(DecodeError::BadSignature(format!("{:02x?}", sig)));
        }
        self.ds.check_bit_field(4, "codec_version", &[1])?;
        self.hdr.hard_tiling_flag = self.ds.unpack_bits(1)? as u32;
        self.ds.check_bit_field(3, "codec_subversion", &[1])?;

        self.hdr.tiling_flag = self.ds.unpack_bits(1)? as u32;
        self.hdr.frequency_mode = self.ds.unpack_bits(1)? as u32;
        self.hdr.spatial_xfrm_subordinate =
            self.ds
                .check_bit_field(3, "spatial_xfrm_subordinate", &[0, 1, 2, 3, 4, 5, 6, 7])?
                as u32;
        self.hdr.index_table_present_flag = self.ds.unpack_bits(1)? as u32;
        self.hdr.overlap_mode = self.ds.unpack_bits(2)? as u8;

        self.hdr.short_header_flag = self.ds.unpack_flag()?;
        self.hdr.long_word_flag = self.ds.unpack_flag()?;
        self.hdr.windowing_flag = self.ds.unpack_bits(1)? as u32;
        self.hdr.trim_flexbits_flag = self.ds.unpack_bits(1)? as u32;
        self.ds.check_bit_field(1, "reserved_d", &[0])?;
        self.hdr.red_blue_not_swapped_flag = self.ds.unpack_bits(1)? as u32;
        // 8.3.17: premultiplied alpha is a property, not a different coding —
        // accept and expose (consumer decides whether to un-premultiply).
        self.hdr.premultiplied_alpha_flag = self.ds.unpack_bits(1)? as u32;
        self.hdr.alpha_image_plane_flag = self.ds.unpack_bits(1)? as u32;

        self.hdr.output_clr_fmt = self.ds.check_bit_field(
            4,
            "output_clr_fmt",
            &[
                OUT_YONLY as u64,
                OUT_YUV420 as u64,
                OUT_YUV422 as u64,
                OUT_YUV444 as u64,
                OUT_CMYK as u64,
                OUT_CMYKDIRECT as u64,
                OUT_NCOMPONENT as u64,
                OUT_RGB as u64,
                OUT_RGBE as u64,
            ],
        )? as u8;
        self.hdr.output_bitdepth = self.ds.check_bit_field(
            4,
            "output_bitdepth",
            &[
                BD1WHITE1 as u64,
                BD8 as u64,
                BD16 as u64,
                BD16S as u64,
                BD16F as u64,
                BD32S as u64,
                BD32F as u64,
                BD5 as u64,
                BD10 as u64,
                BD565 as u64,
                BD1BLACK1 as u64,
            ],
        )? as u8;

        // image width/height
        let hl_short = self.hdr.short_header_flag;
        let read_hl = |ds: &mut Deserializer| -> std::result::Result<u32, DeserializerError> {
            if hl_short {
                Ok(ds.unpack_u16_be()? as u32)
            } else {
                ds.unpack_u32_be()
            }
        };

        // wrapping_add: WIDTH_MINUS1 = u32::MAX must wrap to 0 (and be caught
        // by the macroblock-grid check below) in every build configuration,
        // not panic under overflow checks.
        self.hdr.image_width = read_hl(&mut self.ds)?.wrapping_add(1);
        self.hdr.image_height = read_hl(&mut self.ds)?.wrapping_add(1);
        self.hdr.width = self.hdr.image_width;
        self.hdr.height = self.hdr.image_height;

        if self.hdr.tiling_flag != 0 {
            self.hdr.num_ver_tiles_minus1 = self.ds.unpack_bits(12)? as u32;
            self.hdr.num_hor_tiles_minus1 = self.ds.unpack_bits(12)? as u32;
        }

        self.hdr.num_tile_cols = self.hdr.num_ver_tiles_minus1 as usize + 1;
        self.hdr.num_tile_rows = self.hdr.num_hor_tiles_minus1 as usize + 1;

        let bh_short = self.hdr.short_header_flag;
        let read_bh = |ds: &mut Deserializer| -> std::result::Result<u32, DeserializerError> {
            if bh_short {
                Ok(ds.unpack_u8()? as u32)
            } else {
                Ok(ds.unpack_u16_be()? as u32)
            }
        };

        let mut tile_w_mb = Vec::with_capacity(self.hdr.num_ver_tiles_minus1 as usize + 1);
        for _ in 0..self.hdr.num_ver_tiles_minus1 {
            tile_w_mb.push(read_bh(&mut self.ds)? as usize);
        }
        let mut tile_h_mb = Vec::with_capacity(self.hdr.num_hor_tiles_minus1 as usize + 1);
        for _ in 0..self.hdr.num_hor_tiles_minus1 {
            tile_h_mb.push(read_bh(&mut self.ds)? as usize);
        }

        let mut left_mb = vec![0usize];
        for mb in &tile_w_mb {
            left_mb.push(left_mb.last().unwrap() + mb);
        }
        let mut top_mb = vec![0usize];
        for mb in &tile_h_mb {
            top_mb.push(top_mb.last().unwrap() + mb);
        }

        if self.hdr.windowing_flag != 0 {
            self.hdr.extra_pixels_top = self.ds.unpack_bits(6)? as u32;
            self.hdr.extra_pixels_left = self.ds.unpack_bits(6)? as u32;
            self.hdr.extra_pixels_bottom = self.ds.unpack_bits(6)? as u32;
            self.hdr.extra_pixels_right = self.ds.unpack_bits(6)? as u32;
        } else {
            self.hdr.extra_pixels_top = 0;
            self.hdr.extra_pixels_left = 0;
            self.hdr.extra_pixels_right = if self.hdr.width & 0xF != 0 {
                0x10 - (self.hdr.width & 0xF)
            } else {
                0
            };
            self.hdr.extra_pixels_bottom = if self.hdr.height & 0xF != 0 {
                0x10 - (self.hdr.height & 0xF)
            } else {
                0
            };
        }

        // saturating_add: keeps a near-u32::MAX width from panicking under
        // overflow checks; a saturated value then fails the grid check below.
        self.hdr.width = self
            .hdr
            .width
            .saturating_add(self.hdr.extra_pixels_left + self.hdr.extra_pixels_right);
        self.hdr.height = self
            .hdr
            .height
            .saturating_add(self.hdr.extra_pixels_top + self.hdr.extra_pixels_bottom);

        self.hdr.mb_width = (self.hdr.width / 16) as usize;
        self.hdr.mb_height = (self.hdr.height / 16) as usize;

        // The margin-extended size must be a whole, positive number of
        // macroblocks (T.832 windowing semantics — the margins exist to pad
        // the image to the MB grid; libjxr fails violations with
        // WMP_errInvalidParameter, strdec.c:3056 — its legacy salvage there,
        // reinterpreting right/bottom margins on already-aligned dims as an
        // output crop, is deliberately not ported). Load-bearing for the
        // decode-budget guard in `image_plane_header`: height 1 + margins 0
        // truncates mb_height to 0, zeroing the budgeted mb_width × mb_height
        // product while the grid build still allocates one column Vec per
        // mb_width — a 702-byte header claiming 679 Mpx × 1 allocated ~1 GB
        // of empty column headers before the first tile startcode check
        // (Phase-7 certification slow-unit finding).
        if self.hdr.width & 0xF != 0
            || self.hdr.height & 0xF != 0
            || self.hdr.mb_width == 0
            || self.hdr.mb_height == 0
        {
            return Err(DecodeError::Malformed(format!(
                "extended image size {}×{} is not a whole positive number of 16-pixel macroblocks",
                self.hdr.width, self.hdr.height
            )));
        }

        // Append the last "rest" tile entry. The explicit tile widths/heights
        // must fit within the macroblock grid; a lying header that overshoots
        // would otherwise wrap to a huge usize tile size — in release the
        // subtraction wraps silently (no debug panic), feeding runaway loops
        // and allocations downstream — so reject it as malformed here.
        let prev_left = *left_mb.last().unwrap();
        let prev_top = *top_mb.last().unwrap();
        let rest_w = self.hdr.mb_width.checked_sub(prev_left).ok_or_else(|| {
            DecodeError::Malformed(format!(
                "tile columns ({prev_left}) exceed {} macroblock columns",
                self.hdr.mb_width
            ))
        })?;
        let rest_h = self.hdr.mb_height.checked_sub(prev_top).ok_or_else(|| {
            DecodeError::Malformed(format!(
                "tile rows ({prev_top}) exceed {} macroblock rows",
                self.hdr.mb_height
            ))
        })?;
        tile_w_mb.push(rest_w);
        tile_h_mb.push(rest_h);
        left_mb.push(left_mb.last().unwrap() + tile_w_mb.last().unwrap());
        top_mb.push(top_mb.last().unwrap() + tile_h_mb.last().unwrap());

        self.hdr.tile_width_in_mb = tile_w_mb;
        self.hdr.tile_height_in_mb = tile_h_mb;
        self.hdr.left_mb_index_of_tile = left_mb;
        self.hdr.top_mb_index_of_tile = top_mb;

        Ok(())
    }

    fn image_plane_header(&mut self, p: usize) -> Result<()> {
        let plane = &mut self.planes[p];
        plane.internal_clr_fmt = self.ds.check_bit_field(
            3,
            "internal_clr_fmt",
            &[
                INT_YONLY as u64,
                INT_YUV420 as u64,
                INT_YUV422 as u64,
                INT_YUV444 as u64,
                INT_YUVK as u64,
                INT_NCOMPONENT as u64,
            ],
        )? as u8;
        plane.scaled_flag = self.ds.unpack_bits(1)? as u8;
        plane.bands_present = self.ds.check_bit_field(
            4,
            "bands_present",
            &[
                ALL_BANDS as u64,
                NOFLEXBITS as u64,
                NOHIGHPASS as u64,
                DCONLY as u64,
            ],
        )? as u8;

        plane.lp_present = false;
        plane.hp_present = false;
        plane.flexbits_present = false;
        plane.num_bands = 1;
        if plane.bands_present != DCONLY {
            plane.lp_present = true;
            plane.num_bands += 1;
            if plane.bands_present != NOHIGHPASS {
                plane.hp_present = true;
                plane.num_bands += 1;
                if plane.bands_present != NOFLEXBITS {
                    plane.flexbits_present = true;
                    plane.num_bands += 1;
                }
            }
        }

        match plane.internal_clr_fmt {
            INT_YONLY => plane.num_components = 1,
            INT_YUVK => plane.num_components = 4,
            INT_YUV444 => {
                plane.num_components = 3;
                // libjxr `ReadImagePlaneHeader` (strdec.c:2862-2866) reads TWO
                // 4-bit reserved fields here (8 bits total) for YUV_444 —
                // `reserved_e_bit` then `reserved_f`. Reading only 4 desyncs the
                // entire codestream. Latent until now: every Amazon plate is
                // 8bppGray/YONLY, so this YUV444 path had never run on real data;
                // verified against libjxr-minted color JXRs (jxr-encoder.md 6.0).
                self.ds.unpack_bits(8)?; // reserved_e_bit (4) + reserved_f (4)
            }
            INT_YUV420 | INT_YUV422 => {
                plane.num_components = 3;
                self.ds.unpack_bits(1)?; // reserved_e
                plane.chroma_centering_x = self.ds.unpack_bits(3)? as u32;
                if plane.internal_clr_fmt == INT_YUV420 {
                    self.ds.unpack_bits(1)?; // reserved_g_bit
                    plane.chroma_centering_y = self.ds.unpack_bits(3)? as u32;
                } else {
                    self.ds.unpack_bits(4)?; // reserved_h
                }
            }
            INT_NCOMPONENT => {
                let n = self.ds.unpack_bits(4)? as usize + 1;
                if n == 16 {
                    plane.num_components = self.ds.unpack_bits(12)? as usize + 16;
                } else {
                    plane.num_components = n;
                    self.ds.unpack_bits(4)?; // reserved_h
                }
            }
            _ => {
                return Err(DecodeError::Unsupported(format!(
                    "internal_clr_fmt {}",
                    plane.internal_clr_fmt
                )));
            }
        }

        // UpdateModelMB (Table 116) indexes a 16-entry per-component weight
        // table by `num_components - 1`. The N-component syntax admits up to
        // 4111 components, which would index out of bounds on the very first
        // macroblock — the reference (jxr_image.py:1362) raises IndexError
        // there too, i.e. such a stream is undecodable — so reject it up front.
        if plane.num_components > 16 {
            return Err(DecodeError::Unsupported(format!(
                "{}-component image (max 16 supported)",
                plane.num_components
            )));
        }

        plane.chroma_per_blk = if plane.internal_clr_fmt == INT_YUV420 {
            1
        } else if plane.internal_clr_fmt == INT_YUV422 {
            2
        } else {
            4
        };
        plane.num_lp_coeff = plane.chroma_per_blk * 4;

        if matches!(self.hdr.output_bitdepth, BD16 | BD16S | BD32S) {
            plane.shift_bits = self.ds.unpack_bits(8)? as u32;
        } else if self.hdr.output_bitdepth == BD32F {
            plane.len_mantissa = self.ds.unpack_bits(8)? as u32;
            plane.exp_bias = twos_complement_byte(self.ds.unpack_bits(8)? as u32);
        }

        plane.dc_image_plane_uniform = self.ds.unpack_flag()?;
        if plane.dc_image_plane_uniform {
            let nc = plane.num_components;
            let sf = plane.scaled_flag;
            let qp = QP::read(&mut self.ds, nc, 1, sf, DC)?;
            self.planes[p].dc_qp = Some(qp);
        }

        let plane = &mut self.planes[p];
        if plane.bands_present != DCONLY {
            self.ds.check_bit_field(1, "reserved_i_bit", &[0])?;
            let plane = &mut self.planes[p];
            plane.lp_image_plane_uniform = self.ds.unpack_flag()?;
            if plane.lp_image_plane_uniform {
                let nc = plane.num_components;
                let sf = plane.scaled_flag;
                let qp = QP::read(&mut self.ds, nc, 1, sf, LP)?;
                self.planes[p].lp_qp = Some(qp);
            }
            let plane = &mut self.planes[p];
            if plane.bands_present != NOHIGHPASS {
                self.ds.check_bit_field(1, "reserved_j_bit", &[0])?;
                let plane = &mut self.planes[p];
                plane.hp_image_plane_uniform = self.ds.unpack_flag()?;
                if plane.hp_image_plane_uniform {
                    let nc = plane.num_components;
                    let sf = plane.scaled_flag;
                    let qp = QP::read(&mut self.ds, nc, 1, sf, HP)?;
                    self.planes[p].hp_qp = Some(qp);
                }
            }
        }

        // Build MB grid for this plane. Reject impossible geometry before
        // allocating: bound mb_width × mb_height × num_components by both the
        // absolute ceiling (resident memory) and the codestream length (a
        // decompression bomb can't claim more macroblocks than the bytes could
        // encode). See MAX_MB_COMPONENTS et al.
        let stream_len = self.ds.buffer.len() as u64;
        let hdr = &self.hdr;
        let plane = &mut self.planes[p];
        let mb_width = hdr.mb_width;
        let mb_height = hdr.mb_height;
        let mb_total = (mb_width as u64).saturating_mul(mb_height as u64);
        let mb_components = mb_total.saturating_mul(plane.num_components as u64);
        let stream_budget = stream_len
            .saturating_mul(MB_BUDGET_PER_BYTE)
            .saturating_add(MB_BUDGET_FLOOR);
        if mb_components > MAX_MB_COMPONENTS || mb_total > stream_budget {
            return Err(DecodeError::Malformed(format!(
                "image geometry {mb_width}×{mb_height} mb × {} comp \
                 (={mb_components} mb-components) exceeds the decode budget \
                 for a {stream_len}-byte stream",
                plane.num_components
            )));
        }
        plane.mb = (0..mb_width)
            .map(|_| Vec::with_capacity(mb_height))
            .collect();
        // Fill placeholder so we can index by [MBx][MBy].
        for col in &mut plane.mb {
            for _ in 0..mb_height {
                col.push(MB::new(0, 0, 0, 0, 1, None, None, None));
            }
        }

        for tx in 0..hdr.num_tile_cols {
            let first_mbx = hdr.left_mb_index_of_tile[tx];
            let tile_mb_width = hdr.tile_width_in_mb[tx];
            for ty in 0..hdr.num_tile_rows {
                let first_mby = hdr.top_mb_index_of_tile[ty];
                let tile_mb_height = hdr.tile_height_in_mb[ty];

                for mbxt in 0..tile_mb_width {
                    let mbx = mbxt + first_mbx;
                    for mbyt in 0..tile_mb_height {
                        let mby = mbyt + first_mby;
                        let left_mb = if mbx > 0 { Some((mbx - 1, mby)) } else { None };
                        let top_mb = if mby > 0 { Some((mbx, mby - 1)) } else { None };
                        let tl_mb = if mbx > 0 && mby > 0 {
                            Some((mbx - 1, mby - 1))
                        } else {
                            None
                        };
                        plane.mb[mbx][mby] =
                            MB::new(mbx, mby, mbxt, mbyt, tile_mb_width, left_mb, top_mb, tl_mb);
                    }
                }
            }
        }

        self.ds.discard_remainder_bits();

        Ok(())
    }

    fn index_table_tiles(&mut self, num_bands_primary: usize) -> Result<()> {
        let mut n_entries = self.hdr.num_tile_rows * self.hdr.num_tile_cols;
        if self.hdr.frequency_mode != 0 {
            n_entries *= num_bands_primary;
        }
        self.ds.check_bit_field(16, "index_table_startcode", &[1])?;
        // Each entry costs ≥2 bytes (vlw_esc), so the table cannot hold more
        // entries than the stream has bytes; cap the pre-allocation so a lying
        // tile count (up to ~16M from the 12-bit tile fields) can't reserve
        // hundreds of MB from a short stream. The loop still errors cleanly via
        // `?` once the bits run out.
        let cap = n_entries.min(self.ds.len());
        self.index_offset_tile = Vec::with_capacity(cap);
        for _ in 0..n_entries {
            let v = self.vlw_esc()?;
            self.index_offset_tile.push(v);
        }
        Ok(())
    }

    fn vlw_esc(&mut self) -> Result<u64> {
        let first = self.ds.unpack_bits(8)?;
        if first < 0xfb {
            Ok(first * 256 + self.ds.unpack_bits(8)?)
        } else if first == 0xfb {
            Ok(self.ds.unpack_bits(32)?)
        } else if first == 0xfc {
            Ok(self.ds.unpack_bits(64)?)
        } else {
            Ok(0)
        }
    }

    fn profile_level_info(&mut self) -> Result<u64> {
        let mut i_bytes: u64 = 0;
        loop {
            i_bytes += 4;
            self.ds
                .check_bit_field(8, "profile_idc", &[44, 55, 66, 88, 111])?;
            self.ds.unpack_bits(8)?; // level_idc
            self.ds.unpack_bits(15)?; // reserved_l
            if self.ds.unpack_bits(1)? != 0 {
                return Ok(i_bytes);
            }
        }
    }

    // -----------------------------------------------------------------
    // Coded tiles
    // -----------------------------------------------------------------

    fn coded_tiles(&mut self, num_bands_primary: usize) -> Result<()> {
        let first_tile_offset = self.ds.offset;
        let mut n: usize = 0;
        for ty in 0..self.hdr.num_tile_rows {
            let top_mb_index = self.hdr.top_mb_index_of_tile[ty];
            let height_mb = self.hdr.tile_height_in_mb[ty];

            for tx in 0..self.hdr.num_tile_cols {
                let left_mb_index = self.hdr.left_mb_index_of_tile[tx];
                let width_mb = self.hdr.tile_width_in_mb[tx];

                let tile_types: &[u8] = if self.hdr.frequency_mode != 0 {
                    &[DC, LP, HP, FLEX]
                } else {
                    &[u8::MAX] // marker for spatial
                };

                for (t, &tile_type) in tile_types.iter().enumerate() {
                    if num_bands_primary <= t {
                        continue;
                    }
                    if self.hdr.index_table_present_flag != 0 {
                        let current_tile_offset = (self.ds.offset - first_tile_offset) as u64;
                        if let Some(&expected) = self.index_offset_tile.get(n)
                            && expected != current_tile_offset
                        {
                            self.ds.offset = first_tile_offset + expected as usize;
                        }
                    }

                    self.common_tile_header()?;

                    for p in 0..self.planes.len() {
                        self.tile_plane_header(tile_type, p)?;
                    }

                    for mby in top_mb_index..top_mb_index + height_mb {
                        for mbx in left_mb_index..left_mb_index + width_mb {
                            for p in 0..self.planes.len() {
                                self.tile_mb(tile_type, p, mbx, mby)?;
                            }
                        }
                    }

                    self.ds.discard_remainder_bits();
                    n += 1;
                }
            }
        }
        Ok(())
    }

    fn common_tile_header(&mut self) -> Result<()> {
        self.ds.check_bit_field(24, "tile_startcode", &[1])?;
        self.ds.unpack_bits(8)?; // arbitrary_byte
        Ok(())
    }

    fn tile_plane_header(&mut self, tile_type: u8, p: usize) -> Result<()> {
        if tile_type == DC {
            self.dc_tile_plane_header(p)?;
        } else if tile_type == LP {
            self.lp_tile_plane_header(p)?;
        } else if tile_type == HP {
            self.hp_tile_plane_header(p)?;
        } else if tile_type == FLEX {
            self.flex_tile_plane_header(p)?;
        } else {
            // SpatialTile.tile_plane_header(): Flex, DC, LP, HP in that order.
            self.flex_tile_plane_header(p)?;
            self.dc_tile_plane_header(p)?;
            self.lp_tile_plane_header(p)?;
            self.hp_tile_plane_header(p)?;
        }
        Ok(())
    }

    fn dc_tile_plane_header(&mut self, p: usize) -> Result<()> {
        let plane = &self.planes[p];
        if !plane.dc_image_plane_uniform {
            let nc = plane.num_components;
            let sf = plane.scaled_flag;
            let qp = QP::read(&mut self.ds, nc, 1, sf, DC)?;
            self.planes[p].dc_qp = Some(qp);
            self.planes[p].lp_qp_eq_dc = false;
            self.planes[p].hp_qp_eq_lp = false;
        }
        Ok(())
    }

    fn lp_tile_plane_header(&mut self, p: usize) -> Result<()> {
        let plane = &self.planes[p];
        if plane.lp_present && !plane.lp_image_plane_uniform {
            let use_dc = self.ds.unpack_bits(1)? != 0;
            if use_dc {
                self.planes[p].lp_qp = self.planes[p].dc_qp.clone();
                self.planes[p].lp_qp_eq_dc = true;
            } else {
                let num_qps = self.ds.unpack_bits(4)? as usize + 1;
                let nc = self.planes[p].num_components;
                let sf = self.planes[p].scaled_flag;
                let qp = QP::read(&mut self.ds, nc, num_qps, sf, LP)?;
                self.planes[p].lp_qp = Some(qp);
                self.planes[p].lp_qp_eq_dc = false;
            }
            self.planes[p].hp_qp_eq_lp = false;
        }
        Ok(())
    }

    fn hp_tile_plane_header(&mut self, p: usize) -> Result<()> {
        let plane = &self.planes[p];
        if plane.hp_present && !plane.hp_image_plane_uniform {
            let use_lp = self.ds.unpack_bits(1)? != 0;
            if use_lp {
                self.planes[p].hp_qp = self.planes[p].lp_qp.clone();
                self.planes[p].hp_qp_eq_lp = true;
            } else {
                let num_qps = self.ds.unpack_bits(4)? as usize + 1;
                let nc = self.planes[p].num_components;
                let sf = self.planes[p].scaled_flag;
                let qp = QP::read(&mut self.ds, nc, num_qps, sf, HP)?;
                self.planes[p].hp_qp = Some(qp);
                self.planes[p].hp_qp_eq_lp = false;
            }
        }
        Ok(())
    }

    fn flex_tile_plane_header(&mut self, p: usize) -> Result<()> {
        let plane = &self.planes[p];
        if plane.flexbits_present && !plane.is_alpha {
            self.trim_flexbits = if self.hdr.trim_flexbits_flag != 0 {
                self.ds.unpack_bits(4)? as u32
            } else {
                0
            };
        }
        Ok(())
    }

    fn tile_mb(&mut self, tile_type: u8, p: usize, mbx: usize, mby: usize) -> Result<()> {
        // Lazily materialise this MB's coefficient buffers on first decode
        // (idempotent across the band passes of frequency mode). The grid was
        // built as cheap skeletons, so a stream rejected before reaching here
        // never paid for the per-MB buffers — see `MB::alloc_buffers`.
        let nc = self.planes[p].num_components;
        self.planes[p].mb[mbx][mby].alloc_buffers(nc);
        match tile_type {
            DC => self.mb_dc(p, mbx, mby)?,
            LP => {
                self.lp_tile_mb_qp(p, mbx, mby)?;
                self.lp_tile_mb_2(p, mbx, mby)?;
            }
            HP => {
                self.hp_tile_mb_qp(p, mbx, mby)?;
                if self.planes[p].hp_present {
                    self.mb_cbphp(p, mbx, mby)?;
                    self.mb_hp_flex(p, mbx, mby, true, false, 0)?;
                }
            }
            FLEX => {
                if self.planes[p].flexbits_present {
                    let trim = self.trim_flexbits;
                    self.mb_hp_flex(p, mbx, mby, false, true, trim)?;
                }
            }
            _ => {
                // Spatial: LP/HP QP decode, then DC, LP, HP+FLEX combined.
                self.lp_tile_mb_qp(p, mbx, mby)?;
                self.hp_tile_mb_qp(p, mbx, mby)?;
                self.mb_dc(p, mbx, mby)?;
                self.lp_tile_mb_2(p, mbx, mby)?;
                if self.planes[p].flexbits_present {
                    self.mb_cbphp(p, mbx, mby)?;
                    let trim = self.trim_flexbits;
                    self.mb_hp_flex(p, mbx, mby, true, true, trim)?;
                } else if self.planes[p].hp_present {
                    self.mb_cbphp(p, mbx, mby)?;
                    self.mb_hp_flex(p, mbx, mby, true, false, 0)?;
                }
            }
        }
        Ok(())
    }

    fn lp_tile_mb_qp(&mut self, p: usize, mbx: usize, mby: usize) -> Result<()> {
        let plane = &self.planes[p];
        if plane.lp_present
            && plane.lp_qp.as_ref().is_some_and(|q| q.num_qps > 1)
            && !plane.lp_qp_eq_dc
        {
            let num_qps = plane.lp_qp.as_ref().unwrap().num_qps;
            let idx = self.decode_qp_index(num_qps)?;
            let plane = &mut self.planes[p];
            plane.mb[mbx][mby].mb_qp_index_lp = idx;
            if let Some(qp) = plane.lp_qp.as_mut() {
                qp.index_qps = idx;
            }
        }
        Ok(())
    }

    fn hp_tile_mb_qp(&mut self, p: usize, mbx: usize, mby: usize) -> Result<()> {
        let plane = &self.planes[p];
        if plane.hp_present
            && plane.hp_qp.as_ref().is_some_and(|q| q.num_qps > 1)
            && !plane.hp_qp_eq_lp
        {
            let num_qps = plane.hp_qp.as_ref().unwrap().num_qps;
            let idx = self.decode_qp_index(num_qps)?;
            let plane = &mut self.planes[p];
            plane.mb[mbx][mby].mb_qp_index_hp = idx;
            if let Some(qp) = plane.hp_qp.as_mut() {
                qp.index_qps = idx;
            }
        }
        Ok(())
    }

    fn lp_tile_mb_2(&mut self, p: usize, mbx: usize, mby: usize) -> Result<()> {
        if self.planes[p].lp_present {
            self.mb_lp(p, mbx, mby)?;
        }
        Ok(())
    }

    fn decode_qp_index(&mut self, num_qp: usize) -> Result<usize> {
        let bits_qp_index = [0u32, 0, 1, 1, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4];
        if self.ds.unpack_bits(1)? != 0 {
            let bits = bits_qp_index[num_qp.min(16)];
            let v = self.ds.unpack_bits(bits)? as usize + 1;
            // The selected QP set must exist: `bits` spans a power-of-two range
            // that can exceed num_qp, so a valid stream only ever emits v <
            // num_qp; a larger value would index past the QP table (used as
            // `quant_scaling_factor[..][index_qps]`). Reject rather than panic.
            if v >= num_qp {
                return Err(DecodeError::Malformed(format!(
                    "QP-set index {v} out of range (num_qps {num_qp})"
                )));
            }
            Ok(v)
        } else {
            Ok(0)
        }
    }

    // -----------------------------------------------------------------
    // DC band
    // -----------------------------------------------------------------

    fn mb_dc(&mut self, p: usize, mbx: usize, mby: usize) -> Result<()> {
        let init = self.planes[p].mb[mbx][mby].initialize_context;
        if init {
            self.planes[p].abs_level_ind_dc_lum.init_table1();
            self.planes[p].abs_level_ind_dc_chr.init_table1();
            self.planes[p].model_dc.initialize_model_mb(DC);
        }

        let nc = self.planes[p].num_components;
        let mut dc_input: Vec<i32> = vec![0; nc];
        let mut i_lap_mean = [0i32; 2];

        let int_fmt = self.planes[p].internal_clr_fmt;
        if matches!(int_fmt, INT_YONLY | INT_YUVK | INT_NCOMPONENT) {
            for i_comp in 0..nc {
                let chroma = chroma_component(i_comp);
                let b_abs_level = self.ds.unpack_bits(1)? != 0;
                if b_abs_level {
                    i_lap_mean[chroma] += 1;
                }
                let bits = self.planes[p].model_dc.m_bits[chroma];
                dc_input[i_comp] = self.decode_dc(p, bits, false, b_abs_level)?;
            }
        } else {
            let val_dc_yuv = self.ds.huff(tables::val_dc_yuv())? as u32;
            for &(i_comp, mask) in &[(0usize, 4u32), (1, 2), (2, 1)] {
                let chroma = chroma_component(i_comp);
                let b_abs_level = (val_dc_yuv & mask) != 0;
                if b_abs_level {
                    i_lap_mean[chroma] += 1;
                }
                let bits = self.planes[p].model_dc.m_bits[chroma];
                let chroma_arg = chroma != 0;
                dc_input[i_comp] = self.decode_dc(p, bits, chroma_arg, b_abs_level)?;
            }
        }

        self.update_model_mb(p, &mut i_lap_mean, ModelBand::DC);

        if self.planes[p].mb[mbx][mby].reset_context {
            self.planes[p].abs_level_ind_dc_lum.adapt_table1();
            self.planes[p].abs_level_ind_dc_chr.adapt_table1();
        }

        for i in 0..nc {
            self.planes[p].mb[mbx][mby].mb_dclp[i * MB_DCLP_PER_COMP] = dc_input[i];
        }

        // DC mode prediction.
        let (is_left_edge, is_top_edge) = (
            self.planes[p].mb[mbx][mby].is_left_edge,
            self.planes[p].mb[mbx][mby].is_top_edge,
        );
        let mb_dc_mode = if is_left_edge && is_top_edge {
            NO_PREDICTION
        } else if is_left_edge && !is_top_edge {
            PREDICT_FROM_TOP
        } else if !is_left_edge && is_top_edge {
            PREDICT_FROM_LEFT
        } else {
            let left = self.planes[p].mb[mbx][mby].left_mb.unwrap();
            let top = self.planes[p].mb[mbx][mby].top_mb.unwrap();
            let topleft = self.planes[p].mb[mbx][mby].top_left_mb.unwrap();
            let i_left = self.planes[p].mb[left.0][left.1].mb_dclp[0];
            let i_top = self.planes[p].mb[top.0][top.1].mb_dclp[0];
            let i_topleft = self.planes[p].mb[topleft.0][topleft.1].mb_dclp[0];

            let (i_str_hor, i_str_ver);
            if matches!(int_fmt, INT_YONLY | INT_NCOMPONENT) {
                i_str_hor = (i_topleft - i_left).abs();
                i_str_ver = (i_topleft - i_top).abs();
            } else {
                let i_left_u = self.planes[p].mb[left.0][left.1].mb_dclp[MB_DCLP_PER_COMP];
                let i_top_u = self.planes[p].mb[top.0][top.1].mb_dclp[MB_DCLP_PER_COMP];
                let i_topleft_u = self.planes[p].mb[topleft.0][topleft.1].mb_dclp[MB_DCLP_PER_COMP];
                let i_left_v = self.planes[p].mb[left.0][left.1].mb_dclp[2 * MB_DCLP_PER_COMP];
                let i_top_v = self.planes[p].mb[top.0][top.1].mb_dclp[2 * MB_DCLP_PER_COMP];
                let i_topleft_v =
                    self.planes[p].mb[topleft.0][topleft.1].mb_dclp[2 * MB_DCLP_PER_COMP];
                // Table 128: chroma weighting scale by subsampling.
                let i_scale = match int_fmt {
                    INT_YUV420 => 8,
                    INT_YUV422 => 4,
                    _ => 2,
                };
                i_str_hor = (i_topleft - i_left).abs() * i_scale
                    + (i_topleft_u - i_left_u).abs()
                    + (i_topleft_v - i_left_v).abs();
                i_str_ver = (i_topleft - i_top).abs() * i_scale
                    + (i_topleft_u - i_top_u).abs()
                    + (i_topleft_v - i_top_v).abs();
            }
            let i_or_wt = 4;
            if i_str_hor * i_or_wt < i_str_ver {
                PREDICT_FROM_TOP
            } else if i_str_ver * i_or_wt < i_str_hor {
                PREDICT_FROM_LEFT
            } else {
                PREDICT_FROM_TOP_LEFT
            }
        };

        self.planes[p].mb[mbx][mby].mb_dc_mode = mb_dc_mode;
        for i_comp in 0..nc {
            let dst = i_comp * MB_DCLP_PER_COMP;
            match mb_dc_mode {
                PREDICT_FROM_LEFT => {
                    let left = self.planes[p].mb[mbx][mby].left_mb.unwrap();
                    let dv = self.planes[p].mb[left.0][left.1].mb_dclp[dst];
                    self.planes[p].mb[mbx][mby].mb_dclp[dst] += dv;
                }
                PREDICT_FROM_TOP => {
                    let top = self.planes[p].mb[mbx][mby].top_mb.unwrap();
                    let dv = self.planes[p].mb[top.0][top.1].mb_dclp[dst];
                    self.planes[p].mb[mbx][mby].mb_dclp[dst] += dv;
                }
                PREDICT_FROM_TOP_LEFT => {
                    let left = self.planes[p].mb[mbx][mby].left_mb.unwrap();
                    let top = self.planes[p].mb[mbx][mby].top_mb.unwrap();
                    let v_l = self.planes[p].mb[left.0][left.1].mb_dclp[dst];
                    let v_t = self.planes[p].mb[top.0][top.1].mb_dclp[dst];
                    // Table 129: chroma of subsampled formats rounds up.
                    let round = if i_comp > 0 && matches!(int_fmt, INT_YUV420 | INT_YUV422) {
                        1
                    } else {
                        0
                    };
                    self.planes[p].mb[mbx][mby].mb_dclp[dst] += (v_t + v_l + round) >> 1;
                }
                _ => {}
            }
        }

        let scaling = |this: &Self, p: usize, i: usize| -> i32 {
            this.planes[p].dc_qp.as_ref().unwrap().scaling_factor(i)
        };
        for i_comp in 0..nc {
            let v = self.planes[p].mb[mbx][mby].mb_dclp[i_comp * MB_DCLP_PER_COMP]
                * scaling(self, p, i_comp);
            self.planes[p].mb[mbx][mby].mb_buffer
                [i_comp * MB_BUF_PER_COMP + 16 * ICT4X4_INV_PERM[0]] = v;
        }

        Ok(())
    }

    fn decode_dc(
        &mut self,
        p: usize,
        i_model_bits: i32,
        b_chroma: bool,
        b_abs_level: bool,
    ) -> Result<i32> {
        let mut i_dc: i32 = if b_abs_level {
            self.decode_abs_level(p, b_chroma, false, ModelBand::DC)? - 1
        } else {
            0
        };
        if i_model_bits > 0 {
            let ref_v = self.ds.unpack_bits(i_model_bits as u32)? as i32;
            i_dc = (i_dc << i_model_bits) | ref_v;
        }
        let sign_flag = if i_dc == 0 {
            false
        } else {
            self.ds.unpack_bits(1)? != 0
        };
        Ok(signed_value(i_dc, sign_flag))
    }

    // -----------------------------------------------------------------
    // LP band
    // -----------------------------------------------------------------

    fn mb_lp(&mut self, p: usize, mbx: usize, mby: usize) -> Result<()> {
        let init = self.planes[p].mb[mbx][mby].initialize_context;
        if init {
            self.planes[p].count_zero_cbplp = 1;
            self.planes[p].count_max_cbplp = 1;
            self.initialize_lp_vlc(p);
            self.planes[p].lowpass_scan = Some(AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_H));
            self.planes[p].model_lp.initialize_model_mb(LP);
        }

        if self.planes[p].mb[mbx][mby].reset_totals
            && let Some(scan) = self.planes[p].lowpass_scan.as_mut()
        {
            scan.reset_totals();
        }

        let mut i_lap_mean = [0i32; 2];
        let plane_nc = self.planes[p].num_components;
        let int_fmt = self.planes[p].internal_clr_fmt;
        let is_42x = matches!(int_fmt, INT_YUV420 | INT_YUV422);
        // Table 53: 420/422 code U and V jointly as one chroma "plane".
        let i_full_planes = if is_42x { 2 } else { plane_nc };

        let i_cbplp: u32;
        if matches!(int_fmt, INT_YUV444 | INT_YUV420 | INT_YUV422) {
            let i_max = (i_full_planes as i32 * 4) - 5;
            if self.planes[p].count_zero_cbplp <= 0 || self.planes[p].count_max_cbplp < 0 {
                let table = if is_42x {
                    tables::cbplp_yuv1_42x()
                } else {
                    tables::cbplp_yuv1_444()
                };
                let cbplp_yuv1 = self.ds.huff(table)?;
                i_cbplp = if self.planes[p].count_max_cbplp < self.planes[p].count_zero_cbplp {
                    (i_max - cbplp_yuv1) as u32
                } else {
                    cbplp_yuv1 as u32
                };
            } else {
                i_cbplp = self.ds.unpack_bits(i_full_planes as u32)? as u32;
            }
            // UpdateCountCBPLP
            self.planes[p].count_zero_cbplp += 1 - if i_cbplp == 0 { 4 } else { 0 };
            self.planes[p].count_zero_cbplp = clip(self.planes[p].count_zero_cbplp, -8, 7);
            self.planes[p].count_max_cbplp += 1 - if i_cbplp == i_max as u32 { 4 } else { 0 };
            self.planes[p].count_max_cbplp = clip(self.planes[p].count_max_cbplp, -8, 7);
        } else {
            let mut cbp: u32 = 0;
            for n in 0..plane_nc {
                let bit = self.ds.unpack_bits(1)? as u32;
                cbp |= bit << n;
            }
            i_cbplp = cbp;
        }

        let mut lp_input: Vec<Vec<i32>> = vec![vec![0; 16]; plane_nc];

        for n in 0..i_full_planes {
            let i_index = chroma_component(n);
            if (i_cbplp >> n) & 1 == 1 {
                if is_42x && n > 0 {
                    // Joint U/V chroma block: one DECODE_BLOCK, coefficients
                    // interleaved U,V with a FIXED inverse scan (Table 53).
                    // Our LP storage is transpose-composed, so the spec's
                    // `LPInput[c][iTransposeNNN[iRemap]]` is `lp_input[c][iRemap]`.
                    let i_location = if int_fmt == INT_YUV420 { 10 } else { 2 };
                    let block = self.decode_block(p, true, i_location, ModelBand::LP)?;
                    i_lap_mean[1] += block.len() as i32;
                    let mut temp = [0i32; 14];
                    let mut i = 0usize;
                    for (run, level) in &block {
                        i += *run as usize;
                        temp[i] = *level;
                        i += 1;
                    }
                    const REMAP_ARR: [usize; 7] = [4, 1, 2, 3, 5, 6, 7];
                    let (offset, count) = if int_fmt == INT_YUV420 {
                        (1usize, 6usize)
                    } else {
                        (0, 14)
                    };
                    for (k, &t) in temp.iter().enumerate().take(count) {
                        lp_input[(k & 1) + 1][REMAP_ARR[(k >> 1) + offset]] = t;
                    }
                } else {
                    let i_location = 1;
                    let block = self.decode_block(p, i_index != 0, i_location, ModelBand::LP)?;
                    i_lap_mean[i_index] += block.len() as i32;

                    let mut i = 1usize;
                    for (run, level) in &block {
                        i += *run as usize;
                        self.adaptive_lp_scan(p, n, i, *level, &mut lp_input);
                        i += 1;
                    }
                }
            }

            let i_model_bits = self.planes[p].model_lp.m_bits[i_index];
            if i_model_bits > 0 {
                if is_42x && n > 0 {
                    // Chroma refinement interleaves U,V per coefficient
                    // (Table 53); linear order in our transposed storage.
                    let jmax = if int_fmt == INT_YUV420 { 4 } else { 8 };
                    for k in 1..jmax {
                        let cur = lp_input[1][k];
                        lp_input[1][k] = self.refine_lp(cur, i_model_bits)?;
                        let cur = lp_input[2][k];
                        lp_input[2][k] = self.refine_lp(cur, i_model_bits)?;
                    }
                } else {
                    for k in 1..16 {
                        let cur = lp_input[n][k];
                        lp_input[n][k] = self.refine_lp(cur, i_model_bits)?;
                    }
                }
            }
        }

        self.update_model_mb(p, &mut i_lap_mean, ModelBand::LP);

        if self.planes[p].mb[mbx][mby].reset_context {
            self.adapt_lp(p);
        }

        for i in 0..plane_nc {
            let cbase = i * MB_DCLP_PER_COMP;
            // Table 124: chroma of 420/422 carries only 3/7 LP coefficients.
            let jmax = if i > 0 && int_fmt == INT_YUV420 {
                4
            } else if i > 0 && int_fmt == INT_YUV422 {
                8
            } else {
                16
            };
            for j in 1..jmax {
                self.planes[p].mb[mbx][mby].mb_dclp[cbase + j] = lp_input[i][j];
            }
        }

        // MBLPMode prediction.
        let mb_dc_mode = self.planes[p].mb[mbx][mby].mb_dc_mode;
        let qp_idx_here = self.planes[p].mb[mbx][mby].mb_qp_index_lp;
        let mb_lp_mode = if mb_dc_mode == PREDICT_FROM_LEFT {
            let lm = self.planes[p].mb[mbx][mby].left_mb.unwrap();
            if qp_idx_here == self.planes[p].mb[lm.0][lm.1].mb_qp_index_lp {
                PREDICT_FROM_LEFT
            } else {
                NO_PREDICTION
            }
        } else if mb_dc_mode == PREDICT_FROM_TOP {
            let tm = self.planes[p].mb[mbx][mby].top_mb.unwrap();
            if qp_idx_here == self.planes[p].mb[tm.0][tm.1].mb_qp_index_lp {
                PREDICT_FROM_TOP
            } else {
                NO_PREDICTION
            }
        } else {
            NO_PREDICTION
        };
        self.planes[p].mb[mbx][mby].mb_lp_mode = mb_lp_mode;

        for i in 0..plane_nc {
            let cbase = i * MB_DCLP_PER_COMP;
            if i > 0 && int_fmt == INT_YUV420 {
                // Table 133 (420 chroma), indices in our transposed storage:
                // spec j=2 ↔ ours j=1 (left), spec j=1 ↔ ours j=2 (top).
                if mb_lp_mode == PREDICT_FROM_LEFT {
                    let lm = self.planes[p].mb[mbx][mby].left_mb.unwrap();
                    let v = self.planes[p].mb[lm.0][lm.1].mb_dclp[cbase + 1];
                    self.planes[p].mb[mbx][mby].mb_dclp[cbase + 1] += v;
                } else if mb_lp_mode == PREDICT_FROM_TOP {
                    let tm = self.planes[p].mb[mbx][mby].top_mb.unwrap();
                    let v = self.planes[p].mb[tm.0][tm.1].mb_dclp[cbase + 2];
                    self.planes[p].mb[mbx][mby].mb_dclp[cbase + 2] += v;
                }
            } else if i > 0 && int_fmt == INT_YUV422 {
                // Table 133 (422 chroma) translated: spec {4,2,6} ↔ ours
                // {4,1,5} (left); top: spec j4←top4, j1←top5, j5←own j1 ↔
                // ours j4←top4, j2←top6, j6←own j2; and the MBDCMode==top
                // special prediction applies even with LP prediction off.
                if mb_lp_mode == PREDICT_FROM_LEFT {
                    let lm = self.planes[p].mb[mbx][mby].left_mb.unwrap();
                    for j in [4usize, 1, 5] {
                        let v = self.planes[p].mb[lm.0][lm.1].mb_dclp[cbase + j];
                        self.planes[p].mb[mbx][mby].mb_dclp[cbase + j] += v;
                    }
                } else if mb_lp_mode == PREDICT_FROM_TOP {
                    let tm = self.planes[p].mb[mbx][mby].top_mb.unwrap();
                    let v4 = self.planes[p].mb[tm.0][tm.1].mb_dclp[cbase + 4];
                    self.planes[p].mb[mbx][mby].mb_dclp[cbase + 4] += v4;
                    let v6 = self.planes[p].mb[tm.0][tm.1].mb_dclp[cbase + 6];
                    self.planes[p].mb[mbx][mby].mb_dclp[cbase + 2] += v6;
                    let v2 = self.planes[p].mb[mbx][mby].mb_dclp[cbase + 2];
                    self.planes[p].mb[mbx][mby].mb_dclp[cbase + 6] += v2;
                } else if mb_dc_mode == PREDICT_FROM_TOP {
                    let v2 = self.planes[p].mb[mbx][mby].mb_dclp[cbase + 2];
                    self.planes[p].mb[mbx][mby].mb_dclp[cbase + 6] += v2;
                }
            } else if mb_lp_mode == PREDICT_FROM_LEFT {
                let lm = self.planes[p].mb[mbx][mby].left_mb.unwrap();
                for j in [1, 2, 3].iter().copied() {
                    let v = self.planes[p].mb[lm.0][lm.1].mb_dclp[cbase + j];
                    self.planes[p].mb[mbx][mby].mb_dclp[cbase + j] += v;
                }
            } else if mb_lp_mode == PREDICT_FROM_TOP {
                let tm = self.planes[p].mb[mbx][mby].top_mb.unwrap();
                for j in [4, 8, 12].iter().copied() {
                    let v = self.planes[p].mb[tm.0][tm.1].mb_dclp[cbase + j];
                    self.planes[p].mb[mbx][mby].mb_dclp[cbase + j] += v;
                }
            }
        }

        for i in 0..plane_nc {
            let scaling = self.planes[p].lp_qp.as_ref().unwrap().scaling_factor(i);
            let cbase = i * MB_DCLP_PER_COMP;
            if i > 0 && int_fmt == INT_YUV420 {
                // Chroma DC-LP into SPEC-RASTER block DC slots: our mb_dclp
                // is transpose-domain, so slot T420[j] gets dclp[j].
                const T420: [usize; 4] = [0, 2, 1, 3];
                for j in 1..4 {
                    let v = self.planes[p].mb[mbx][mby].mb_dclp[cbase + j] * scaling;
                    self.planes[p].mb[mbx][mby].mb_buffer[i * MB_BUF_PER_COMP + 16 * T420[j]] = v;
                }
            } else if i > 0 && int_fmt == INT_YUV422 {
                const T422: [usize; 8] = [0, 2, 1, 3, 4, 6, 5, 7];
                for j in 1..8 {
                    let v = self.planes[p].mb[mbx][mby].mb_dclp[cbase + j] * scaling;
                    self.planes[p].mb[mbx][mby].mb_buffer[i * MB_BUF_PER_COMP + 16 * T422[j]] = v;
                }
            } else {
                for j in 1..16 {
                    let v = self.planes[p].mb[mbx][mby].mb_dclp[cbase + j] * scaling;
                    let pos = 16 * ICT4X4_INV_PERM[j];
                    self.planes[p].mb[mbx][mby].mb_buffer[i * MB_BUF_PER_COMP + pos] = v;
                }
            }
        }

        Ok(())
    }

    fn adaptive_lp_scan(
        &mut self,
        p: usize,
        n: usize,
        i: usize,
        value: i32,
        lp_input: &mut [Vec<i32>],
    ) {
        let scan = self.planes[p].lowpass_scan.as_mut().unwrap();
        lp_input[n][scan.translate(i)] = value;
        scan.adapt(i);
    }

    // `pub(crate)` so the encoder's unit tests can use it as a round-trip
    // oracle: it depends only on the bitstream (no plane state), so a bare
    // `Decoder::new(bytes)` can drive it.
    pub(crate) fn refine_lp(&mut self, mut i_coeff: i32, i_model_bits: i32) -> Result<i32> {
        let coeff_ref = self.ds.unpack_bits(i_model_bits as u32)? as i32;
        if i_coeff > 0 {
            i_coeff = (i_coeff << i_model_bits) + coeff_ref;
        } else if i_coeff < 0 {
            i_coeff = (i_coeff << i_model_bits) - coeff_ref;
        } else {
            i_coeff = self.sign_optional(coeff_ref)?;
        }
        Ok(i_coeff)
    }

    fn sign_optional(&mut self, value: i32) -> Result<i32> {
        if value == 0 {
            return Ok(0);
        }
        Ok(if self.ds.unpack_bits(1)? != 0 {
            -value
        } else {
            value
        })
    }

    fn initialize_lp_vlc(&mut self, p: usize) {
        let pl = &mut self.planes[p];
        pl.dec_first_ind_lp_lum.init_table2();
        pl.dec_ind_lp_lum0.init_table2();
        pl.dec_ind_lp_lum1.init_table2();
        pl.dec_first_ind_lp_chr.init_table2();
        pl.dec_ind_lp_chr0.init_table2();
        pl.dec_ind_lp_chr1.init_table2();
        pl.abs_level_ind_lp0.init_table1();
        pl.abs_level_ind_lp1.init_table1();
    }

    fn adapt_lp(&mut self, p: usize) {
        let pl = &mut self.planes[p];
        pl.dec_first_ind_lp_lum.adapt_table2(4);
        pl.dec_ind_lp_lum0.adapt_table2(3);
        pl.dec_ind_lp_lum1.adapt_table2(3);
        pl.dec_first_ind_lp_chr.adapt_table2(4);
        pl.dec_ind_lp_chr0.adapt_table2(3);
        pl.dec_ind_lp_chr1.adapt_table2(3);
        pl.abs_level_ind_lp0.adapt_table1();
        pl.abs_level_ind_lp1.adapt_table1();
    }

    // -----------------------------------------------------------------
    // HP band: CBPHP + HP/FLEX coefficient decode
    // -----------------------------------------------------------------

    fn mb_cbphp(&mut self, p: usize, mbx: usize, mby: usize) -> Result<()> {
        let i_flc = [0u32, 2, 1, 2, 2, 0];
        let i_off = [0i32, 4, 2, 8, 12, 1];
        let i_out = [0i32, 15, 3, 12, 1, 2, 4, 8, 5, 6, 9, 10, 7, 11, 13, 14];
        let plane_nc = self.planes[p].num_components;
        let int_fmt = self.planes[p].internal_clr_fmt;
        let mut i_diff_cbphp: Vec<i32> = vec![0; plane_nc];

        let use_delta1_and_table1 = matches!(int_fmt, INT_YONLY | INT_NCOMPONENT | INT_YUVK);
        let use_delta1 = use_delta1_and_table1;
        let use_table1 = use_delta1_and_table1;

        if self.planes[p].mb[mbx][mby].initialize_context {
            self.planes[p].dec_num_cbphp.init_table1();
            self.planes[p].dec_num_blk_cbphp.init_table1();
        }

        let outer_iters = if matches!(int_fmt, INT_YUVK | INT_NCOMPONENT) {
            plane_nc
        } else {
            1
        };

        for i_comp in 0..outer_iters {
            let num_cbphp = {
                let idx = self.planes[p].dec_num_cbphp.table_index as usize;
                let v = self.ds.huff(tables::num_cbphp(idx))?;
                let dt = self.planes[p].dec_num_cbphp.delta_table_index as usize;
                self.planes[p].dec_num_cbphp.discrim_val1 += NUM_CBPHP_DELTA[dt][v as usize];
                v
            };
            let i_cbphp = self.refine_cbphp(num_cbphp)?;

            for i_block in 0..4 {
                if (i_cbphp & (1 << i_block)) == 0 {
                    continue;
                }
                let idx = self.planes[p].dec_num_blk_cbphp.table_index as usize;
                let table = if use_table1 {
                    tables::num_cbphp(idx)
                } else {
                    tables::num_blkcbphp2(idx)
                };
                let num_blk_cbphp = self.ds.huff(table)?;
                let dt = self.planes[p].dec_num_blk_cbphp.delta_table_index as usize;
                let delta_table: &[i32] = if use_delta1 {
                    &NUM_BLK_CBPHP_DELTA1[dt][..]
                } else {
                    &NUM_BLK_CBPHP_DELTA2[dt][..]
                };
                self.planes[p].dec_num_blk_cbphp.discrim_val1 +=
                    delta_table[num_blk_cbphp as usize];

                let mut i_val = (num_blk_cbphp + 1) as u32;
                let mut i_blk_cbphp: i32 = 0;
                if i_val >= 6 {
                    let chr = self.ds.huff(tables::chr_cbphp())?;
                    i_blk_cbphp = 0x10 * (chr + 1);
                    if i_val >= 9 {
                        i_val += self.ds.huff(tables::val_inc())? as u32;
                    }
                    i_val -= 6;
                }

                let mut i_code = i_off[i_val as usize];
                let flc = i_flc[i_val as usize];
                if flc != 0 {
                    i_code += self.ds.unpack_bits(flc)? as i32;
                }
                i_blk_cbphp += i_out[i_code as usize];

                if int_fmt == INT_YUV444 {
                    i_diff_cbphp[0] |= (i_blk_cbphp & 0x0F) << (i_block * 4);
                    for k in 0..2 {
                        if (i_blk_cbphp >> (k + 4)) & 0x01 != 0 {
                            let num = self.ds.huff(tables::num_ch_blk())? + 1;
                            let i_cbphp_chr = self.refine_cbphp(num)?;
                            i_diff_cbphp[k + 1] |= i_cbphp_chr << (i_block * 4);
                        }
                    }
                } else if int_fmt == INT_YUV422 {
                    // Table 57: chroma CBP is an 8-bit pattern (2×4 blocks);
                    // CBPHP_CH_BLK shares the CHR_CBPHP code table.
                    const I_SHIFT: [i32; 4] = [0, 1, 4, 5];
                    i_diff_cbphp[0] |= (i_blk_cbphp & 0x0F) << (i_block * 4);
                    for k in 0..2 {
                        if (i_blk_cbphp >> (k + 4)) & 0x01 != 0 {
                            let v = self.ds.huff(tables::chr_cbphp())?;
                            let i_cbphp_chr = I_SHIFT[(v + 1) as usize];
                            i_diff_cbphp[k + 1] |= i_cbphp_chr << I_SHIFT[i_block as usize];
                        }
                    }
                } else if int_fmt == INT_YUV420 {
                    i_diff_cbphp[0] |= (i_blk_cbphp & 0x0F) << (i_block * 4);
                    i_diff_cbphp[1] |= ((i_blk_cbphp >> 4) & 0x01) << i_block;
                    i_diff_cbphp[2] |= ((i_blk_cbphp >> 5) & 0x01) << i_block;
                } else {
                    i_diff_cbphp[i_comp] |= i_blk_cbphp << (i_block * 4);
                }
            }
        }

        if self.planes[p].mb[mbx][mby].initialize_context {
            self.planes[p].cbphp_model_hp.cbphp_state = [0, 0];
            self.planes[p].cbphp_model_hp.count_ones = [-4, -4];
            self.planes[p].cbphp_model_hp.count_zeroes = [4, 4];
        }

        // Table 65: 420/422 run the 444 predictor on luma only, then their
        // own chroma predictors.
        let pred_444_comps = if matches!(int_fmt, INT_YUV420 | INT_YUV422) {
            1
        } else {
            plane_nc
        };
        for i_comp in 0..pred_444_comps {
            let v = self.pred_cbphp_444(p, i_comp, &i_diff_cbphp, mbx, mby);
            self.planes[p].mb[mbx][mby].mb_cbphp[i_comp] = v;
        }
        if int_fmt == INT_YUV422 {
            for i_comp in 1..3 {
                let v = self.pred_cbphp_422(p, i_comp, &i_diff_cbphp, mbx, mby);
                self.planes[p].mb[mbx][mby].mb_cbphp[i_comp] = v;
            }
        } else if int_fmt == INT_YUV420 {
            for i_comp in 1..3 {
                let v = self.pred_cbphp_420(p, i_comp, &i_diff_cbphp, mbx, mby);
                self.planes[p].mb[mbx][mby].mb_cbphp[i_comp] = v;
            }
        }

        Ok(())
    }

    /// Table 67. Chroma CBPHP prediction for 4:2:2 (8-bit pattern, 2×4 blocks
    /// in raster order). `iNOrig` counts double so the shared model update
    /// stays on the 16-block scale.
    fn pred_cbphp_422(
        &mut self,
        p: usize,
        i_component: usize,
        i_diff_cbphp: &[i32],
        mbx: usize,
        mby: usize,
    ) -> i32 {
        let mut i_cbphp = i_diff_cbphp[i_component];
        let state = self.planes[p].cbphp_model_hp.cbphp_state[1];
        if state == 0 {
            if self.planes[p].mb[mbx][mby].is_left_edge {
                if self.planes[p].mb[mbx][mby].is_top_edge {
                    i_cbphp ^= 1;
                } else {
                    let tm = self.planes[p].mb[mbx][mby].top_mb.unwrap();
                    i_cbphp ^= (self.planes[p].mb[tm.0][tm.1].mb_cbphp[i_component] >> 6) & 1;
                }
            } else {
                let lm = self.planes[p].mb[mbx][mby].left_mb.unwrap();
                i_cbphp ^= (self.planes[p].mb[lm.0][lm.1].mb_cbphp[i_component] >> 1) & 1;
            }
            i_cbphp ^= (i_cbphp & 0x01) << 1;
            i_cbphp ^= (i_cbphp & 0x03) << 2;
            i_cbphp ^= (i_cbphp & 0x0C) << 2;
            i_cbphp ^= (i_cbphp & 0x30) << 2;
        } else if state == 2 {
            i_cbphp ^= 0x00FF;
        }
        let n_orig = num_ones(i_cbphp as u32) as i32 * 2;
        self.update_cbphp_model(p, 1, n_orig);
        i_cbphp
    }

    /// Table 68. Chroma CBPHP prediction for 4:2:0 (4-bit pattern, 2×2 blocks
    /// in raster order). `iNOrig` counts ×4 for the shared model update.
    fn pred_cbphp_420(
        &mut self,
        p: usize,
        i_component: usize,
        i_diff_cbphp: &[i32],
        mbx: usize,
        mby: usize,
    ) -> i32 {
        let mut i_cbphp = i_diff_cbphp[i_component];
        let state = self.planes[p].cbphp_model_hp.cbphp_state[1];
        if state == 0 {
            if self.planes[p].mb[mbx][mby].is_left_edge {
                if self.planes[p].mb[mbx][mby].is_top_edge {
                    i_cbphp ^= 1;
                } else {
                    let tm = self.planes[p].mb[mbx][mby].top_mb.unwrap();
                    i_cbphp ^= (self.planes[p].mb[tm.0][tm.1].mb_cbphp[i_component] >> 2) & 1;
                }
            } else {
                let lm = self.planes[p].mb[mbx][mby].left_mb.unwrap();
                i_cbphp ^= (self.planes[p].mb[lm.0][lm.1].mb_cbphp[i_component] >> 1) & 1;
            }
            i_cbphp ^= 0x02 & (i_cbphp << 1);
            i_cbphp ^= (i_cbphp & 0x3) << 2;
        } else if state == 2 {
            i_cbphp ^= 0x0F;
        }
        let n_orig = num_ones(i_cbphp as u32) as i32 * 4;
        self.update_cbphp_model(p, 1, n_orig);
        i_cbphp
    }

    /// Table 106 UpdateCBPHPModel — shared by all three predictors.
    fn update_cbphp_model(&mut self, p: usize, chroma_flag: usize, n_orig: i32) {
        let m = &mut self.planes[p].cbphp_model_hp;
        let i_n_diff = 3;
        m.count_ones[chroma_flag] += n_orig - i_n_diff;
        m.count_ones[chroma_flag] = clip(m.count_ones[chroma_flag], -16, 15);
        m.count_zeroes[chroma_flag] += (16 - n_orig) - i_n_diff;
        m.count_zeroes[chroma_flag] = clip(m.count_zeroes[chroma_flag], -16, 15);
        m.cbphp_state[chroma_flag] = if m.count_ones[chroma_flag] < 0 {
            if m.count_ones[chroma_flag] < m.count_zeroes[chroma_flag] {
                1
            } else {
                2
            }
        } else if m.count_zeroes[chroma_flag] < 0 {
            2
        } else {
            0
        };
    }

    fn refine_cbphp(&mut self, i_num: i32) -> Result<i32> {
        let v = match i_num {
            2 => self.ds.huff(tables::ref_cbphp1())?,
            1 => 1 << self.ds.unpack_bits(2)?,
            3 => 0x0F ^ (1 << self.ds.unpack_bits(2)?),
            4 => 0x0F,
            _ => 0,
        };
        Ok(v)
    }

    fn pred_cbphp_444(
        &mut self,
        p: usize,
        i_component: usize,
        i_diff_cbphp: &[i32],
        mbx: usize,
        mby: usize,
    ) -> i32 {
        let chroma_flag = if i_component > 0 { 1 } else { 0 };
        let mut i_cbphp = i_diff_cbphp[i_component];
        let state = self.planes[p].cbphp_model_hp.cbphp_state[chroma_flag];

        if state == 0 {
            if self.planes[p].mb[mbx][mby].is_left_edge {
                if self.planes[p].mb[mbx][mby].is_top_edge {
                    i_cbphp ^= 1;
                } else {
                    let tm = self.planes[p].mb[mbx][mby].top_mb.unwrap();
                    i_cbphp ^= (self.planes[p].mb[tm.0][tm.1].mb_cbphp[i_component] >> 10) & 1;
                }
            } else {
                let lm = self.planes[p].mb[mbx][mby].left_mb.unwrap();
                i_cbphp ^= (self.planes[p].mb[lm.0][lm.1].mb_cbphp[i_component] >> 5) & 1;
            }
            i_cbphp ^= 0x02 & (i_cbphp << 1);
            i_cbphp ^= 0x10 & (i_cbphp << 3);
            i_cbphp ^= 0x20 & (i_cbphp << 1);
            i_cbphp ^= (i_cbphp & 0x33) << 2;
            i_cbphp ^= (i_cbphp & 0x00CC) << 6;
            i_cbphp ^= (i_cbphp & 0x3300) << 2;
        } else if state == 2 {
            i_cbphp ^= 0x0000FFFF;
        }
        let n_orig = num_ones(i_cbphp as u32) as i32;
        self.update_cbphp_model(p, chroma_flag, n_orig);
        i_cbphp
    }

    fn mb_hp_flex(
        &mut self,
        p: usize,
        mbx: usize,
        mby: usize,
        do_hp: bool,
        do_flex: bool,
        i_trim_flex_bits: u32,
    ) -> Result<()> {
        let plane_nc = self.planes[p].num_components;

        if do_hp {
            if self.planes[p].mb[mbx][mby].initialize_context {
                self.initialize_hp_vlc(p);
                self.planes[p].highpass_hor_scan =
                    Some(AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_H_PRIME));
                self.planes[p].highpass_ver_scan =
                    Some(AdaptiveScan::new(&GRGI_ZIGZAG_INV_4X4_V_PRIME));
                self.planes[p].model_hp.initialize_model_mb(HP);
            }
            if self.planes[p].mb[mbx][mby].reset_totals {
                if let Some(s) = self.planes[p].highpass_hor_scan.as_mut() {
                    s.reset_totals();
                }
                if let Some(s) = self.planes[p].highpass_ver_scan.as_mut() {
                    s.reset_totals();
                }
            }
            // CalcHPPredMode (uses mb.MbDCLP[0])
            let mb = &self.planes[p].mb[mbx][mby];
            let strength_hor = mb.mb_dclp[1].abs() + mb.mb_dclp[2].abs() + mb.mb_dclp[3].abs();
            let strength_ver = mb.mb_dclp[4].abs() + mb.mb_dclp[8].abs() + mb.mb_dclp[12].abs();
            let (mut s_hor, mut s_ver) = (strength_hor, strength_ver);
            let int_fmt = self.planes[p].internal_clr_fmt;
            if !matches!(int_fmt, INT_YONLY | INT_NCOMPONENT) {
                // Table 135 chroma terms, translated into our transposed
                // mb_dclp storage (s_hor here ≙ spec iStrVer and vice versa).
                for i in 1..3 {
                    let cbase = i * MB_DCLP_PER_COMP;
                    match int_fmt {
                        INT_YUV420 => {
                            s_hor += mb.mb_dclp[cbase + 1].abs();
                            s_ver += mb.mb_dclp[cbase + 2].abs();
                        }
                        INT_YUV422 => {
                            s_hor += mb.mb_dclp[cbase + 1].abs() + mb.mb_dclp[cbase + 5].abs();
                            s_ver += mb.mb_dclp[cbase + 2].abs() + mb.mb_dclp[cbase + 6].abs();
                        }
                        _ => {
                            s_hor += mb.mb_dclp[cbase + 1].abs();
                            s_ver += mb.mb_dclp[cbase + 4].abs();
                        }
                    }
                }
            }
            let i_or_wt = 4;
            let mode = if s_hor * i_or_wt < s_ver {
                PREDICT_FROM_TOP
            } else if s_ver * i_or_wt < s_hor {
                PREDICT_FROM_LEFT
            } else {
                NO_PREDICTION
            };
            self.planes[p].mb[mbx][mby].mb_hp_mode = mode;
        }

        let mut i_lap_mean = [0i32; 2];

        for i_comp in 0..plane_nc {
            let i_index = chroma_component(i_comp);

            let i_model_bits = if do_flex {
                if do_hp {
                    self.planes[p].model_hp.m_bits[i_index]
                } else {
                    self.planes[p].mb[mbx][mby].model_bits_mb_hp[i_index]
                }
            } else {
                0
            };

            let i_cbphp_init = if do_hp {
                self.planes[p].mb[mbx][mby].mb_cbphp[i_comp]
            } else {
                0
            };
            let mut i_cbphp = i_cbphp_init;

            // Tables 69/70/83: chroma of 420/422 has 4/8 blocks per MB and
            // uses the IDENTITY block map (hier scan only for 16 blocks).
            let int_fmt = self.planes[p].internal_clr_fmt;
            let n_blocks = if i_comp > 0 && int_fmt == INT_YUV420 {
                4
            } else if i_comp > 0 && int_fmt == INT_YUV422 {
                8
            } else {
                16
            };
            for raw_block in 0..n_blocks {
                let i_block = if n_blocks == 16 {
                    I_HIER_SCAN_ORDER[raw_block]
                } else {
                    raw_block
                };
                if do_hp {
                    let mode = self.planes[p].mb[mbx][mby].mb_hp_mode;
                    let i_num_non_zero = self.decode_block_adaptive(
                        p,
                        mbx,
                        mby,
                        (i_cbphp & 1) != 0,
                        i_index != 0,
                        i_comp,
                        i_block,
                        mode,
                    )?;
                    i_lap_mean[i_index] += i_num_non_zero as i32;
                    i_cbphp >>= 1;
                }
                if do_flex && self.planes[p].flexbits_present {
                    self.block_flexbits(
                        p,
                        mbx,
                        mby,
                        i_comp,
                        i_block,
                        i_model_bits,
                        i_trim_flex_bits,
                    )?;
                }
            }
        }

        if do_hp {
            self.planes[p].mb[mbx][mby].model_bits_mb_hp[0] = self.planes[p].model_hp.m_bits[0];
            self.planes[p].mb[mbx][mby].model_bits_mb_hp[1] = self.planes[p].model_hp.m_bits[1];
            self.update_model_mb(p, &mut i_lap_mean, ModelBand::HP);
            if self.planes[p].mb[mbx][mby].reset_context {
                self.adapt_hp(p);
            }
        }

        if (do_hp && !self.planes[p].flexbits_present) || do_flex {
            self.hp_transform_coefficient_decoding(p, mbx, mby);
        }
        Ok(())
    }

    fn decode_block_adaptive(
        &mut self,
        p: usize,
        mbx: usize,
        mby: usize,
        b_no_skip: bool,
        b_chroma: bool,
        i_component: usize,
        i_block: usize,
        mb_hp_mode: u8,
    ) -> Result<usize> {
        let mut i_num_non_zero = 0usize;
        if b_no_skip {
            let mut i_location = 1;
            let block = self.decode_block(p, b_chroma, i_location, ModelBand::HP)?;
            for (run, level) in block {
                i_location += run as usize;
                if !(1..=15).contains(&i_location) {
                    return Err(DecodeError::Unsupported(format!(
                        "decode_block_adaptive iLocation {i_location}"
                    )));
                }
                let pos = {
                    let scan = if mb_hp_mode == PREDICT_FROM_TOP {
                        self.planes[p].highpass_ver_scan.as_mut().unwrap()
                    } else {
                        self.planes[p].highpass_hor_scan.as_mut().unwrap()
                    };
                    let t = scan.translate(i_location);
                    scan.adapt(i_location);
                    t
                };
                self.planes[p].mb[mbx][mby].hp_input_vlc
                    [i_component * HP_INPUT_PER_COMP + i_block * 16 + pos] = level;
                i_location += 1;
                i_num_non_zero += 1;
            }
        }
        Ok(i_num_non_zero)
    }

    fn block_flexbits(
        &mut self,
        p: usize,
        mbx: usize,
        mby: usize,
        i_component: usize,
        i_block: usize,
        i_model_bits: i32,
        i_trim_flex_bits: u32,
    ) -> Result<()> {
        let i_flex_bits_left = i_model_bits - i_trim_flex_bits as i32;
        if i_flex_bits_left <= 0 {
            return Ok(());
        }
        let hp_base = i_component * HP_INPUT_PER_COMP + i_block * 16;
        for &n in &I_TRANSPOSE_FLEX[1..] {
            let flex_ref = self.ds.unpack_bits(i_flex_bits_left as u32)? as i32;
            let i_vlc_coeff = self.planes[p].mb[mbx][mby].hp_input_vlc[hp_base + n];
            let i_flex_coeff = if i_vlc_coeff > 0 {
                flex_ref
            } else if i_vlc_coeff < 0 {
                -flex_ref
            } else {
                self.sign_optional(flex_ref)?
            };
            self.planes[p].mb[mbx][mby].hp_input_flex[hp_base + n] =
                i_flex_coeff << i_trim_flex_bits;
        }
        Ok(())
    }

    fn hp_transform_coefficient_decoding(&mut self, p: usize, mbx: usize, mby: usize) {
        let plane_nc = self.planes[p].num_components;
        let int_fmt = self.planes[p].internal_clr_fmt;
        let chroma_blocks = |i_comp: usize| -> usize {
            if i_comp > 0 && int_fmt == INT_YUV420 {
                4
            } else if i_comp > 0 && int_fmt == INT_YUV422 {
                8
            } else {
                16
            }
        };
        // Dequantize with THIS MB's HP QP-set index: in frequency mode this
        // function runs during the FLEX pass, when the plane-level
        // `index_qps` already holds the HP pass's LAST index.
        let hp_idx = self.planes[p].mb[mbx][mby].mb_qp_index_hp;
        for i_comp in 0..plane_nc {
            let i_index = if i_comp == 0 { 0 } else { 1 };
            let scaling = self.planes[p]
                .hp_qp
                .as_ref()
                .unwrap()
                .scaling_factor_at(i_comp, hp_idx);
            let bits = self.planes[p].mb[mbx][mby].model_bits_mb_hp[i_index];

            let hp_cbase = i_comp * HP_INPUT_PER_COMP;
            let mb_cbase = i_comp * MB_BUF_PER_COMP;
            for blk in 0..chroma_blocks(i_comp) {
                let blk_off = blk * 16;
                for j in 1..16 {
                    let vlc = self.planes[p].mb[mbx][mby].hp_input_vlc[hp_cbase + blk_off + j];
                    let flex = self.planes[p].mb[mbx][mby].hp_input_flex[hp_cbase + blk_off + j];
                    let val = ((vlc << bits) + flex) * scaling;
                    self.planes[p].mb[mbx][mby].mb_buffer[mb_cbase + blk_off + j] = val;
                }
            }
        }

        // HP prediction (Table 136). In-block coefficient indices are in our
        // storage domain ({2,10,9} ≙ spec {1,2,3}; {1,5,6} ≙ spec {4,8,12});
        // chroma 420/422 differ only in block-ID lists/strides (our chroma
        // blocks are raster-indexed, matching the spec's lists directly).
        let mode = self.planes[p].mb[mbx][mby].mb_hp_mode;
        const K_TOP: [usize; 3] = [2, 10, 9];
        const K_LEFT: [usize; 3] = [1, 5, 6];
        let pred_comps = if matches!(int_fmt, INT_YUV420 | INT_YUV422) {
            1
        } else {
            plane_nc
        };
        for i_comp in 0..pred_comps {
            let cbase = i_comp * MB_BUF_PER_COMP;
            if mode == PREDICT_FROM_TOP {
                for &blk_id in &[1usize, 2, 3, 5, 6, 7, 9, 10, 11, 13, 14, 15] {
                    for &k in &K_TOP {
                        let v_prev =
                            self.planes[p].mb[mbx][mby].mb_buffer[cbase + 16 * (blk_id - 1) + k];
                        self.planes[p].mb[mbx][mby].mb_buffer[cbase + 16 * blk_id + k] += v_prev;
                    }
                }
            } else if mode == PREDICT_FROM_LEFT {
                for blk_id in 4..16 {
                    for &k in &K_LEFT {
                        let v_prev =
                            self.planes[p].mb[mbx][mby].mb_buffer[cbase + 16 * (blk_id - 4) + k];
                        self.planes[p].mb[mbx][mby].mb_buffer[cbase + 16 * blk_id + k] += v_prev;
                    }
                }
            }
        }
        if matches!(int_fmt, INT_YUV420 | INT_YUV422) {
            for i_comp in 1..3 {
                let cbase = i_comp * MB_BUF_PER_COMP;
                // Chroma blocks are SPEC-RASTER indexed (2 columns): our TOP
                // ≙ spec mode 1 (stride −2 = row above), our LEFT ≙ spec
                // mode 0 (stride −1 = column left).
                let (top_blks, top_stride, left_blks, left_stride): (
                    &[usize],
                    usize,
                    &[usize],
                    usize,
                ) = if int_fmt == INT_YUV420 {
                    (&[2, 3], 2, &[1, 3], 1)
                } else {
                    (&[2, 4, 6, 3, 5, 7], 2, &[1, 3, 5, 7], 1)
                };
                if mode == PREDICT_FROM_TOP {
                    for &blk_id in top_blks {
                        for &k in &K_TOP {
                            let v_prev = self.planes[p].mb[mbx][mby].mb_buffer
                                [cbase + 16 * (blk_id - top_stride) + k];
                            self.planes[p].mb[mbx][mby].mb_buffer[cbase + 16 * blk_id + k] +=
                                v_prev;
                        }
                    }
                } else if mode == PREDICT_FROM_LEFT {
                    for &blk_id in left_blks {
                        for &k in &K_LEFT {
                            let v_prev = self.planes[p].mb[mbx][mby].mb_buffer
                                [cbase + 16 * (blk_id - left_stride) + k];
                            self.planes[p].mb[mbx][mby].mb_buffer[cbase + 16 * blk_id + k] +=
                                v_prev;
                        }
                    }
                }
            }
        }
    }

    fn initialize_hp_vlc(&mut self, p: usize) {
        let pl = &mut self.planes[p];
        pl.dec_first_ind_hp_lum.init_table2();
        pl.dec_ind_hp_lum0.init_table2();
        pl.dec_ind_hp_lum1.init_table2();
        pl.dec_first_ind_hp_chr.init_table2();
        pl.dec_ind_hp_chr0.init_table2();
        pl.dec_ind_hp_chr1.init_table2();
        pl.abs_level_ind_hp0.init_table1();
        pl.abs_level_ind_hp1.init_table1();
    }

    fn adapt_hp(&mut self, p: usize) {
        let pl = &mut self.planes[p];
        pl.dec_first_ind_hp_lum.adapt_table2(4);
        pl.dec_ind_hp_lum0.adapt_table2(3);
        pl.dec_ind_hp_lum1.adapt_table2(3);
        pl.dec_first_ind_hp_chr.adapt_table2(4);
        pl.dec_ind_hp_chr0.adapt_table2(3);
        pl.dec_ind_hp_chr1.adapt_table2(3);
        pl.abs_level_ind_hp0.adapt_table1();
        pl.abs_level_ind_hp1.adapt_table1();
        pl.dec_num_cbphp.adapt_table1();
        pl.dec_num_blk_cbphp.adapt_table1();
    }

    // -----------------------------------------------------------------
    // Shared band methods
    // -----------------------------------------------------------------

    fn decode_block(
        &mut self,
        p: usize,
        b_chroma: bool,
        i_location: usize,
        band: ModelBand,
    ) -> Result<Vec<(u32, i32)>> {
        if i_location > 15 {
            return Err(DecodeError::Unsupported(format!(
                "decode_block start {i_location}"
            )));
        }

        // decode_first_index
        let table_index = self.first_index_table_index(p, b_chroma, band);
        let first_index = self.ds.huff(tables::first_index(table_index as usize))?;
        let (delta1, delta2) = self.first_index_deltas(p, b_chroma, band);
        let v0 = FIRST_INDEX_DELTA[delta1 as usize][first_index as usize];
        let v1 = FIRST_INDEX_DELTA[delta2 as usize][first_index as usize];
        self.first_index_apply_delta(p, b_chroma, band, v0, v1);

        let run_is_zero = first_index & 0x01;
        let level_is_not_1 = (first_index >> 1) & 0x01;
        let next_is_immediate = (first_index >> 2) & 0x01;
        let next_after_run = (first_index >> 3) & 0x01;

        let mut i_context = run_is_zero & next_is_immediate;
        let level_sign_flag = self.ds.unpack_bits(1)? != 0;
        let level_val = if level_is_not_1 != 0 {
            self.decode_abs_level(p, b_chroma, i_context != 0, band)?
        } else {
            1
        };
        let level = signed_value(level_val, level_sign_flag);
        let run = if run_is_zero != 0 {
            0
        } else {
            self.decode_run((15 - i_location) as u32)?
        };
        let mut block = vec![(run, level)];
        let mut i_loc = i_location + run as usize + 1;

        let mut next_is_immediate = next_is_immediate;
        let mut next_after_run = next_after_run;
        while next_is_immediate != 0 || next_after_run != 0 {
            let run = if next_is_immediate != 0 {
                0
            } else {
                self.decode_run((15 - i_loc) as u32)?
            };
            i_loc += run as usize + 1;
            if i_loc > 16 {
                return Err(DecodeError::Unsupported(format!(
                    "decode_block iLoc {i_loc}"
                )));
            }

            let (table_index, delta1, delta2) =
                self.index_table_index(p, b_chroma, band, i_context != 0);
            let i_index = if i_loc < 15 {
                let v = self.ds.huff(tables::index_a(table_index as usize))?;
                let dv1 = INDEX1_DELTA[delta1 as usize][v as usize];
                let dv2 = INDEX1_DELTA[delta2 as usize][v as usize];
                self.index_apply_delta(p, b_chroma, band, i_context != 0, dv1, dv2);
                v
            } else if i_loc == 15 {
                self.ds.huff(tables::index_b())?
            } else {
                self.ds.unpack_bits(1)? as i32
            };

            let next_is_immediate_new = (i_index >> 1) & 0x01;
            next_after_run = (i_index >> 2) & 0x01;
            let level_is_not_1 = i_index & 0x01;
            i_context &= next_is_immediate_new;

            let sign = self.ds.unpack_bits(1)? != 0;
            let lvl = if level_is_not_1 != 0 {
                self.decode_abs_level(p, b_chroma, i_context != 0, band)?
            } else {
                1
            };
            let lvl_signed = signed_value(lvl, sign);
            block.push((run, lvl_signed));
            next_is_immediate = next_is_immediate_new;
        }

        Ok(block)
    }

    fn first_index_table_index(&self, p: usize, b_chroma: bool, band: ModelBand) -> u32 {
        match (band, b_chroma) {
            (ModelBand::LP, false) => self.planes[p].dec_first_ind_lp_lum.table_index,
            (ModelBand::LP, true) => self.planes[p].dec_first_ind_lp_chr.table_index,
            (ModelBand::HP, false) => self.planes[p].dec_first_ind_hp_lum.table_index,
            (ModelBand::HP, true) => self.planes[p].dec_first_ind_hp_chr.table_index,
            _ => 0,
        }
    }

    fn first_index_deltas(&self, p: usize, b_chroma: bool, band: ModelBand) -> (u32, u32) {
        match (band, b_chroma) {
            (ModelBand::LP, false) => (
                self.planes[p].dec_first_ind_lp_lum.delta_table_index,
                self.planes[p].dec_first_ind_lp_lum.delta2_table_index,
            ),
            (ModelBand::LP, true) => (
                self.planes[p].dec_first_ind_lp_chr.delta_table_index,
                self.planes[p].dec_first_ind_lp_chr.delta2_table_index,
            ),
            (ModelBand::HP, false) => (
                self.planes[p].dec_first_ind_hp_lum.delta_table_index,
                self.planes[p].dec_first_ind_hp_lum.delta2_table_index,
            ),
            (ModelBand::HP, true) => (
                self.planes[p].dec_first_ind_hp_chr.delta_table_index,
                self.planes[p].dec_first_ind_hp_chr.delta2_table_index,
            ),
            _ => (0, 0),
        }
    }

    fn first_index_apply_delta(
        &mut self,
        p: usize,
        b_chroma: bool,
        band: ModelBand,
        v0: i32,
        v1: i32,
    ) {
        let target = match (band, b_chroma) {
            (ModelBand::LP, false) => &mut self.planes[p].dec_first_ind_lp_lum,
            (ModelBand::LP, true) => &mut self.planes[p].dec_first_ind_lp_chr,
            (ModelBand::HP, false) => &mut self.planes[p].dec_first_ind_hp_lum,
            (ModelBand::HP, true) => &mut self.planes[p].dec_first_ind_hp_chr,
            _ => return,
        };
        target.discrim_val1 += v0;
        target.discrim_val2 += v1;
    }

    fn index_table_index(
        &self,
        p: usize,
        b_chroma: bool,
        band: ModelBand,
        ctx: bool,
    ) -> (u32, u32, u32) {
        let pl = &self.planes[p];
        let vlc = match (band, b_chroma, ctx) {
            (ModelBand::LP, false, false) => &pl.dec_ind_lp_lum0,
            (ModelBand::LP, false, true) => &pl.dec_ind_lp_lum1,
            (ModelBand::LP, true, false) => &pl.dec_ind_lp_chr0,
            (ModelBand::LP, true, true) => &pl.dec_ind_lp_chr1,
            (ModelBand::HP, false, false) => &pl.dec_ind_hp_lum0,
            (ModelBand::HP, false, true) => &pl.dec_ind_hp_lum1,
            (ModelBand::HP, true, false) => &pl.dec_ind_hp_chr0,
            (ModelBand::HP, true, true) => &pl.dec_ind_hp_chr1,
            _ => unreachable!(),
        };
        (
            vlc.table_index,
            vlc.delta_table_index,
            vlc.delta2_table_index,
        )
    }

    fn index_apply_delta(
        &mut self,
        p: usize,
        b_chroma: bool,
        band: ModelBand,
        ctx: bool,
        v0: i32,
        v1: i32,
    ) {
        let target = match (band, b_chroma, ctx) {
            (ModelBand::LP, false, false) => &mut self.planes[p].dec_ind_lp_lum0,
            (ModelBand::LP, false, true) => &mut self.planes[p].dec_ind_lp_lum1,
            (ModelBand::LP, true, false) => &mut self.planes[p].dec_ind_lp_chr0,
            (ModelBand::LP, true, true) => &mut self.planes[p].dec_ind_lp_chr1,
            (ModelBand::HP, false, false) => &mut self.planes[p].dec_ind_hp_lum0,
            (ModelBand::HP, false, true) => &mut self.planes[p].dec_ind_hp_lum1,
            (ModelBand::HP, true, false) => &mut self.planes[p].dec_ind_hp_chr0,
            (ModelBand::HP, true, true) => &mut self.planes[p].dec_ind_hp_chr1,
            _ => return,
        };
        target.discrim_val1 += v0;
        target.discrim_val2 += v1;
    }

    // `pub(crate)` as an encoder-test oracle (bitstream-only, no plane state).
    pub(crate) fn decode_run(&mut self, i_max_run: u32) -> Result<u32> {
        if !(1..=14).contains(&i_max_run) {
            return Err(DecodeError::Unsupported(format!(
                "decode_run iMaxRun {i_max_run}"
            )));
        }
        let i_run_binx = [10usize, 10, 5, 5, 5, 5, 0, 0, 0, 0];
        let i_run_fixed_length = [0u32, 0, 1, 1, 3, 0, 0, 1, 1, 2, 0, 0, 0, 0, 1];
        let i_remap = [1u32, 2, 3, 5, 7, 1, 2, 3, 5, 7, 1, 2, 3, 4, 5];

        let i_run = if i_max_run < 5 {
            if i_max_run == 1 {
                1
            } else {
                self.ds.huff(tables::run_value(i_max_run as usize))? as u32
            }
        } else {
            let i_index =
                self.ds.huff(tables::run_index())? as usize + i_run_binx[i_max_run as usize - 5];
            let i_fixed = i_run_fixed_length[i_index];
            let mut r = i_remap[i_index];
            if i_fixed != 0 {
                r += self.ds.unpack_bits(i_fixed)? as u32;
            }
            r
        };
        if !(1..=i_max_run).contains(&i_run) {
            return Err(DecodeError::Unsupported(format!(
                "decode_run {i_run} not in 1..={i_max_run}"
            )));
        }
        Ok(i_run)
    }

    fn decode_abs_level(
        &mut self,
        p: usize,
        b_chroma: bool,
        i_context: bool,
        band: ModelBand,
    ) -> Result<i32> {
        let vlc_idx = self.abs_level_table_index(p, b_chroma, i_context, band);
        let i_remap = [2i32, 3, 4, 6, 10, 14];
        let i_fixed_len = [0u32, 0, 1, 2, 2, 2];

        let abs_level_index = self.ds.huff(tables::abs_level_index(vlc_idx as usize))?;
        self.abs_level_apply_delta(
            p,
            b_chroma,
            i_context,
            band,
            ABS_LEVEL_INDEX_DELTA[0][abs_level_index as usize],
        );

        let i_level = if abs_level_index < 6 {
            let i_fixed = i_fixed_len[abs_level_index as usize];
            let mut lvl = i_remap[abs_level_index as usize];
            if i_fixed > 0 {
                lvl += self.ds.unpack_bits(i_fixed)? as i32;
            }
            lvl
        } else {
            let mut i_fixed = self.ds.unpack_bits(4)? as u32 + 4;
            if i_fixed == 19 {
                i_fixed += self.ds.unpack_bits(2)? as u32;
                if i_fixed == 22 {
                    i_fixed += self.ds.unpack_bits(3)? as u32;
                }
            }
            2 + (1i32.wrapping_shl(i_fixed)) + self.ds.unpack_bits(i_fixed)? as i32
        };

        Ok(i_level)
    }

    fn abs_level_table_index(
        &self,
        p: usize,
        b_chroma: bool,
        i_context: bool,
        band: ModelBand,
    ) -> u32 {
        let pl = &self.planes[p];
        let vlc = match (band, b_chroma, i_context) {
            (ModelBand::DC, false, _) => &pl.abs_level_ind_dc_lum,
            (ModelBand::DC, true, _) => &pl.abs_level_ind_dc_chr,
            (ModelBand::LP, _, false) => &pl.abs_level_ind_lp0,
            (ModelBand::LP, _, true) => &pl.abs_level_ind_lp1,
            (ModelBand::HP, _, false) => &pl.abs_level_ind_hp0,
            (ModelBand::HP, _, true) => &pl.abs_level_ind_hp1,
        };
        vlc.table_index
    }

    fn abs_level_apply_delta(
        &mut self,
        p: usize,
        b_chroma: bool,
        i_context: bool,
        band: ModelBand,
        dv: i32,
    ) {
        let pl = &mut self.planes[p];
        let vlc = match (band, b_chroma, i_context) {
            (ModelBand::DC, false, _) => &mut pl.abs_level_ind_dc_lum,
            (ModelBand::DC, true, _) => &mut pl.abs_level_ind_dc_chr,
            (ModelBand::LP, _, false) => &mut pl.abs_level_ind_lp0,
            (ModelBand::LP, _, true) => &mut pl.abs_level_ind_lp1,
            (ModelBand::HP, _, false) => &mut pl.abs_level_ind_hp0,
            (ModelBand::HP, _, true) => &mut pl.abs_level_ind_hp1,
        };
        vlc.discrim_val1 += dv;
    }

    fn update_model_mb(&mut self, p: usize, i_lap_mean: &mut [i32; 2], band: ModelBand) {
        let i_model_weight = 70;
        let i_weight0 = [240i32, 12, 1];
        let i_weight1: [[i32; 16]; 3] = [
            [
                0, 240, 120, 80, 60, 48, 40, 34, 30, 27, 24, 22, 20, 18, 17, 16,
            ],
            [0, 12, 6, 4, 3, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1],
            [0, 16, 8, 5, 4, 3, 3, 2, 2, 2, 2, 1, 1, 1, 1, 1],
        ];
        let band_idx = band.as_index();
        i_lap_mean[0] *= i_weight0[band_idx];
        // Table 116: 420/422 chroma uses iWeight2 (joint-coded chroma scale),
        // everything else iWeight1 by component count (+>>4 for HP).
        const I_WEIGHT2: [i32; 6] = [120, 37, 2, 120, 18, 1];
        match self.planes[p].internal_clr_fmt {
            INT_YUV420 => i_lap_mean[1] *= I_WEIGHT2[band_idx],
            INT_YUV422 => i_lap_mean[1] *= I_WEIGHT2[3 + band_idx],
            _ => {
                i_lap_mean[1] *= i_weight1[band_idx][self.planes[p].num_components - 1];
                if matches!(band, ModelBand::HP) {
                    i_lap_mean[1] >>= 4;
                }
            }
        }

        let i_num_models = if self.planes[p].internal_clr_fmt == INT_YONLY {
            1
        } else {
            2
        };
        let model = match band {
            ModelBand::DC => &mut self.planes[p].model_dc,
            ModelBand::LP => &mut self.planes[p].model_lp,
            ModelBand::HP => &mut self.planes[p].model_hp,
        };

        for j in 0..i_num_models {
            let mut i_ms = model.m_state[j];
            let i_delta = (i_lap_mean[j] - i_model_weight) >> 2;
            if i_delta <= -8 {
                let mut i_delta = i_delta + 4;
                if i_delta < -16 {
                    i_delta = -16;
                }
                i_ms += i_delta;
                if i_ms < -8 {
                    if model.m_bits[j] == 0 {
                        i_ms = -8;
                    } else {
                        i_ms = 0;
                        model.m_bits[j] -= 1;
                    }
                }
            } else if i_delta >= 8 {
                let mut i_delta = i_delta - 4;
                if i_delta > 15 {
                    i_delta = 15;
                }
                i_ms += i_delta;
                if i_ms > 8 {
                    if model.m_bits[j] >= 15 {
                        model.m_bits[j] = 15;
                        i_ms = 8;
                    } else {
                        i_ms = 0;
                        model.m_bits[j] += 1;
                    }
                }
            }
            model.m_state[j] = i_ms;
        }
    }

    // -----------------------------------------------------------------
    // Sample reconstruction
    // -----------------------------------------------------------------

    fn sample_reconstruction(&mut self, p: usize) {
        self.first_level_inverse_transform(p);
        if self.hdr.overlap_mode == FIRST_AND_SECOND_LEVEL_OVERLAP_FILTERING {
            self.first_level_overlap_filtering(p);
        }
        self.second_level_inverse_transform(p);
        self.second_level_coefficient_combination(p);
        if matches!(
            self.hdr.overlap_mode,
            FIRST_AND_SECOND_LEVEL_OVERLAP_FILTERING | SECOND_LEVEL_OVERLAP_FILTERING
        ) {
            self.second_level_overlap_filtering(p);
        }
    }

    fn first_level_inverse_transform(&mut self, p: usize) {
        let nc = self.planes[p].num_components;
        let scaled = self.planes[p].scaled_flag != 0;
        let int_fmt = self.planes[p].internal_clr_fmt;
        for i in 0..nc {
            let cbase = i * MB_BUF_PER_COMP;
            let chroma_42x = i > 0 && matches!(int_fmt, INT_YUV420 | INT_YUV422);
            for mby in 0..self.hdr.mb_height {
                for mbx in 0..self.hdr.mb_width {
                    let mb = &mut self.planes[p].mb[mbx][mby];
                    if chroma_42x && int_fmt == INT_YUV420 {
                        // Table 151, 420 chroma: 2×2 Hadamard + swap(1,2).
                        // Block-DC slots hold spec-domain dequantized DC-LP.
                        let mut d = [0i32; 4];
                        for (j, dj) in d.iter_mut().enumerate() {
                            *dj = mb.mb_buffer[cbase + j * 16];
                        }
                        d = t2x2h(d, 0);
                        d.swap(1, 2);
                        if scaled {
                            for v in d.iter_mut() {
                                *v = v.wrapping_mul(2);
                            }
                        }
                        for (j, dj) in d.iter().enumerate() {
                            mb.mb_buffer[cbase + j * 16] = *dj;
                        }
                    } else if chroma_42x {
                        // Table 151, 422 chroma: T2pt{0,4}, T2x2h{0..3},
                        // swap(1,2), T2x2h{4,6,5,7}, swap(5,6).
                        let mut d = [0i32; 8];
                        for (j, dj) in d.iter_mut().enumerate() {
                            *dj = mb.mb_buffer[cbase + j * 16];
                        }
                        d[0] -= (d[4] + 1) >> 1;
                        d[4] += d[0];
                        let a = t2x2h([d[0], d[1], d[2], d[3]], 0);
                        d[0] = a[0];
                        d[1] = a[1];
                        d[2] = a[2];
                        d[3] = a[3];
                        d.swap(1, 2);
                        let b = t2x2h([d[4], d[6], d[5], d[7]], 0);
                        d[4] = b[0];
                        d[6] = b[1];
                        d[5] = b[2];
                        d[7] = b[3];
                        d.swap(5, 6);
                        if scaled {
                            for v in d.iter_mut() {
                                *v = v.wrapping_mul(2);
                            }
                        }
                        for (j, dj) in d.iter().enumerate() {
                            mb.mb_buffer[cbase + j * 16] = *dj;
                        }
                    } else {
                        let mut dclp0 = [0i32; 16];
                        for j in 0..16 {
                            dclp0[j] = mb.mb_buffer[cbase + j * 16];
                        }
                        str_idct4x4_stage2(&mut dclp0);
                        if i > 0 && scaled {
                            for v in dclp0.iter_mut() {
                                *v = v.wrapping_mul(2);
                            }
                        }
                        for j in 0..16 {
                            mb.mb_buffer[cbase + j * 16] = dclp0[j];
                        }
                    }
                }
            }
        }
    }

    /// Read/write one chroma block DC (block `b`, spec-raster) of MB (x,y).
    #[inline]
    fn chroma_dc(&self, p: usize, cbase: usize, x: usize, y: usize, b: usize) -> i32 {
        self.planes[p].mb[x][y].mb_buffer[cbase + 16 * b]
    }

    #[inline]
    fn set_chroma_dc(&mut self, p: usize, cbase: usize, x: usize, y: usize, b: usize, v: i32) {
        self.planes[p].mb[x][y].mb_buffer[cbase + 16 * b] = v;
    }

    /// Apply `OverlapPostFilter2x2` across the four (x, y, block) cells.
    fn chroma_f2x2(&mut self, p: usize, cbase: usize, cells: [(usize, usize, usize); 4]) {
        let v = overlap_post_filter_2x2([
            self.chroma_dc(p, cbase, cells[0].0, cells[0].1, cells[0].2),
            self.chroma_dc(p, cbase, cells[1].0, cells[1].1, cells[1].2),
            self.chroma_dc(p, cbase, cells[2].0, cells[2].1, cells[2].2),
            self.chroma_dc(p, cbase, cells[3].0, cells[3].1, cells[3].2),
        ]);
        for (k, &(x, y, b)) in cells.iter().enumerate() {
            self.set_chroma_dc(p, cbase, x, y, b, v[k]);
        }
    }

    /// Apply `OverlapPostFilter2` across two (x, y, block) cells.
    fn chroma_f2(&mut self, p: usize, cbase: usize, cells: [(usize, usize, usize); 2]) {
        let v = overlap_post_filter_2([
            self.chroma_dc(p, cbase, cells[0].0, cells[0].1, cells[0].2),
            self.chroma_dc(p, cbase, cells[1].0, cells[1].1, cells[1].2),
        ]);
        for (k, &(x, y, b)) in cells.iter().enumerate() {
            self.set_chroma_dc(p, cbase, x, y, b, v[k]);
        }
    }

    /// Tables 154/155 — first-level overlap filtering for 420/422 chroma
    /// block DCs (overlap mode 2 only). Operates across MB boundaries on the
    /// spec-raster block-DC slots; structure transcribed table-for-table
    /// (corner difference → junction/edge filters → corner addition).
    fn first_level_overlap_chroma(&mut self, p: usize, i: usize) {
        let is420 = self.planes[p].internal_clr_fmt == INT_YUV420;
        let cbase = i * MB_BUF_PER_COMP;
        let hard = self.hdr.hard_tiling_flag != 0;
        let cols = self.hdr.num_tile_cols;
        let rows = self.hdr.num_tile_rows;
        let left = self.hdr.left_mb_index_of_tile.clone();
        let top = self.hdr.top_mb_index_of_tile.clone();
        // Bottom-corner block indices differ: 420 row1 = {2,3}; 422 row3 = {6,7}.
        let (bl, br) = if is420 {
            (2usize, 3usize)
        } else {
            (6usize, 7usize)
        };

        for ty in 0..rows {
            // Corner differences ("OverlapPostFilter1", −=).
            if ty == 0 || hard {
                let y = top[ty];
                let d = self.chroma_dc(p, cbase, left[0], y, 1);
                let v = self.chroma_dc(p, cbase, left[0], y, 0) - d;
                self.set_chroma_dc(p, cbase, left[0], y, 0, v);
                let xr = left[cols] - 1;
                let d = self.chroma_dc(p, cbase, xr, y, 0);
                let v = self.chroma_dc(p, cbase, xr, y, 1) - d;
                self.set_chroma_dc(p, cbase, xr, y, 1, v);
                if hard {
                    for tx in 1..cols.saturating_sub(1) {
                        let d = self.chroma_dc(p, cbase, left[tx], y, 1);
                        let v = self.chroma_dc(p, cbase, left[tx], y, 0) - d;
                        self.set_chroma_dc(p, cbase, left[tx], y, 0, v);
                        let d = self.chroma_dc(p, cbase, left[tx] - 1, y, 0);
                        let v = self.chroma_dc(p, cbase, left[tx] - 1, y, 1) - d;
                        self.set_chroma_dc(p, cbase, left[tx] - 1, y, 1, v);
                    }
                }
            }
            if ty == rows - 1 || hard {
                let y = top[ty + 1] - 1;
                let d = self.chroma_dc(p, cbase, left[0], y, br);
                let v = self.chroma_dc(p, cbase, left[0], y, bl) - d;
                self.set_chroma_dc(p, cbase, left[0], y, bl, v);
                let xr = left[cols] - 1;
                let d = self.chroma_dc(p, cbase, xr, y, bl);
                let v = self.chroma_dc(p, cbase, xr, y, br) - d;
                self.set_chroma_dc(p, cbase, xr, y, br, v);
                if hard {
                    for tx in 1..cols.saturating_sub(1) {
                        let d = self.chroma_dc(p, cbase, left[tx], y, br);
                        let v = self.chroma_dc(p, cbase, left[tx], y, bl) - d;
                        self.set_chroma_dc(p, cbase, left[tx], y, bl, v);
                        let d = self.chroma_dc(p, cbase, left[tx] - 1, y, bl);
                        let v = self.chroma_dc(p, cbase, left[tx] - 1, y, br) - d;
                        self.set_chroma_dc(p, cbase, left[tx] - 1, y, br, v);
                    }
                }
            }

            for tx in 0..cols {
                let (x0, x1) = (left[tx], left[tx + 1]);
                let (y0, y1) = (top[ty], top[ty + 1]);
                if is420 {
                    // Interior 2×2 junctions (across both MB axes).
                    for y in y0..y1.saturating_sub(1) {
                        for x in x0..x1.saturating_sub(1) {
                            self.chroma_f2x2(
                                p,
                                cbase,
                                [(x, y, 3), (x + 1, y, 2), (x, y + 1, 1), (x + 1, y + 1, 0)],
                            );
                        }
                    }
                } else {
                    // 422: within-MB row1↔row2 junction for every MB pair;
                    // across-MB row3↔row0 guarded to non-last rows.
                    for y in y0..y1 {
                        for x in x0..x1.saturating_sub(1) {
                            self.chroma_f2x2(
                                p,
                                cbase,
                                [(x, y, 3), (x + 1, y, 2), (x, y, 5), (x + 1, y, 4)],
                            );
                            if y != y1 - 1 {
                                self.chroma_f2x2(
                                    p,
                                    cbase,
                                    [(x, y, 7), (x + 1, y, 6), (x, y + 1, 1), (x + 1, y + 1, 0)],
                                );
                            }
                        }
                    }
                }
                if tx == 0 || hard {
                    let x = x0;
                    if is420 {
                        for y in y0..y1.saturating_sub(1) {
                            self.chroma_f2(p, cbase, [(x, y, 2), (x, y + 1, 0)]);
                        }
                    } else {
                        for y in y0..y1 {
                            self.chroma_f2(p, cbase, [(x, y, 2), (x, y, 4)]);
                            if y != y1 - 1 {
                                self.chroma_f2(p, cbase, [(x, y, 6), (x, y + 1, 0)]);
                            }
                        }
                    }
                }
                if tx == cols - 1 || hard {
                    let x = x1 - 1;
                    if is420 {
                        for y in y0..y1.saturating_sub(1) {
                            self.chroma_f2(p, cbase, [(x, y, 3), (x, y + 1, 1)]);
                        }
                    } else {
                        for y in y0..y1 {
                            self.chroma_f2(p, cbase, [(x, y, 3), (x, y, 5)]);
                            if y != y1 - 1 {
                                self.chroma_f2(p, cbase, [(x, y, 7), (x, y + 1, 1)]);
                            }
                        }
                    }
                }
                if ty == 0 || hard {
                    let y = y0;
                    for x in x0..x1.saturating_sub(1) {
                        self.chroma_f2(p, cbase, [(x, y, 1), (x + 1, y, 0)]);
                    }
                }
                if ty == rows - 1 || hard {
                    let y = y1 - 1;
                    for x in x0..x1.saturating_sub(1) {
                        self.chroma_f2(p, cbase, [(x, y, br), (x + 1, y, bl)]);
                    }
                }
                if !hard && tx != cols - 1 {
                    // Right across (soft tile boundary).
                    let x = x1 - 1;
                    for y in y0..y1.saturating_sub(1) {
                        if is420 {
                            self.chroma_f2x2(
                                p,
                                cbase,
                                [(x, y, 3), (x + 1, y, 2), (x, y + 1, 1), (x + 1, y + 1, 0)],
                            );
                        } else {
                            self.chroma_f2x2(
                                p,
                                cbase,
                                [(x, y, 3), (x + 1, y, 2), (x, y, 5), (x + 1, y, 4)],
                            );
                            self.chroma_f2x2(
                                p,
                                cbase,
                                [(x, y, 7), (x + 1, y, 6), (x, y + 1, 1), (x + 1, y + 1, 0)],
                            );
                        }
                    }
                }
                if !hard && ty != rows - 1 {
                    // Bottom across.
                    let y = y1 - 1;
                    for x in x0..x1.saturating_sub(1) {
                        if is420 {
                            self.chroma_f2x2(
                                p,
                                cbase,
                                [(x, y, 3), (x + 1, y, 2), (x, y + 1, 1), (x + 1, y + 1, 0)],
                            );
                        } else {
                            self.chroma_f2x2(
                                p,
                                cbase,
                                [(x, y, 3), (x + 1, y, 2), (x, y, 5), (x + 1, y, 4)],
                            );
                            self.chroma_f2x2(
                                p,
                                cbase,
                                [(x, y, 7), (x + 1, y, 6), (x, y + 1, 1), (x + 1, y + 1, 0)],
                            );
                        }
                    }
                }
                if !hard && tx != cols - 1 && ty != rows - 1 {
                    // Diagonal.
                    let (x, y) = (x1 - 1, y1 - 1);
                    if is420 {
                        self.chroma_f2x2(
                            p,
                            cbase,
                            [(x, y, 3), (x + 1, y, 2), (x, y + 1, 1), (x + 1, y + 1, 0)],
                        );
                    } else {
                        self.chroma_f2x2(
                            p,
                            cbase,
                            [(x, y, 3), (x + 1, y, 2), (x, y, 5), (x + 1, y, 4)],
                        );
                        self.chroma_f2x2(
                            p,
                            cbase,
                            [(x, y, 7), (x + 1, y, 6), (x, y + 1, 1), (x + 1, y + 1, 0)],
                        );
                    }
                }
                if !hard && tx == 0 && ty != rows - 1 {
                    // Left-edge continuation across the soft tile boundary.
                    let (x, y) = (x0, y1 - 1);
                    if is420 {
                        self.chroma_f2(p, cbase, [(x, y, 2), (x, y + 1, 0)]);
                    } else {
                        self.chroma_f2(p, cbase, [(x, y, 2), (x, y, 4)]);
                        self.chroma_f2(p, cbase, [(x, y, 6), (x, y + 1, 0)]);
                    }
                }
                if !hard && tx == cols - 1 && ty != rows - 1 {
                    let (x, y) = (x1 - 1, y1 - 1);
                    if is420 {
                        self.chroma_f2(p, cbase, [(x, y, 3), (x, y + 1, 1)]);
                    } else {
                        self.chroma_f2(p, cbase, [(x, y, 3), (x, y, 5)]);
                        self.chroma_f2(p, cbase, [(x, y, 7), (x, y + 1, 1)]);
                    }
                }
                if !hard && tx != cols - 1 && ty == 0 {
                    let (x, y) = (x1 - 1, y0);
                    self.chroma_f2(p, cbase, [(x, y, 1), (x + 1, y, 0)]);
                }
                if !hard && tx != cols - 1 && ty == rows - 1 {
                    let (x, y) = (x1 - 1, y1 - 1);
                    self.chroma_f2(p, cbase, [(x, y, br), (x + 1, y, bl)]);
                }
            }

            // Corner additions (+=), undoing the pre-differences.
            if ty == 0 || hard {
                let y = top[ty];
                let d = self.chroma_dc(p, cbase, left[0], y, 1);
                let v = self.chroma_dc(p, cbase, left[0], y, 0) + d;
                self.set_chroma_dc(p, cbase, left[0], y, 0, v);
                let xr = left[cols] - 1;
                let d = self.chroma_dc(p, cbase, xr, y, 0);
                let v = self.chroma_dc(p, cbase, xr, y, 1) + d;
                self.set_chroma_dc(p, cbase, xr, y, 1, v);
                if hard {
                    for tx in 1..cols.saturating_sub(1) {
                        let d = self.chroma_dc(p, cbase, left[tx], y, 1);
                        let v = self.chroma_dc(p, cbase, left[tx], y, 0) + d;
                        self.set_chroma_dc(p, cbase, left[tx], y, 0, v);
                        let d = self.chroma_dc(p, cbase, left[tx] - 1, y, 0);
                        let v = self.chroma_dc(p, cbase, left[tx] - 1, y, 1) + d;
                        self.set_chroma_dc(p, cbase, left[tx] - 1, y, 1, v);
                    }
                }
            }
            if ty == rows - 1 || hard {
                let y = top[ty + 1] - 1;
                let d = self.chroma_dc(p, cbase, left[0], y, br);
                let v = self.chroma_dc(p, cbase, left[0], y, bl) + d;
                self.set_chroma_dc(p, cbase, left[0], y, bl, v);
                let xr = left[cols] - 1;
                let d = self.chroma_dc(p, cbase, xr, y, bl);
                let v = self.chroma_dc(p, cbase, xr, y, br) + d;
                self.set_chroma_dc(p, cbase, xr, y, br, v);
                if hard {
                    for tx in 1..cols.saturating_sub(1) {
                        let d = self.chroma_dc(p, cbase, left[tx], y, br);
                        let v = self.chroma_dc(p, cbase, left[tx], y, bl) + d;
                        self.set_chroma_dc(p, cbase, left[tx], y, bl, v);
                        let d = self.chroma_dc(p, cbase, left[tx] - 1, y, bl);
                        let v = self.chroma_dc(p, cbase, left[tx] - 1, y, br) + d;
                        self.set_chroma_dc(p, cbase, left[tx] - 1, y, br, v);
                    }
                }
            }
        }
    }

    fn first_level_overlap_filtering(&mut self, p: usize) {
        let nc = self.planes[p].num_components;
        // LE/TE/RE/BE/etc lists from Python.
        let le1: [(i32, i32, usize); 4] = [(0, 0, 8), (0, 0, 12), (0, 1, 0), (0, 1, 4)];
        let le2: [(i32, i32, usize); 4] = [(0, 0, 9), (0, 0, 13), (0, 1, 1), (0, 1, 5)];
        let te1: [(i32, i32, usize); 4] = [(0, 0, 2), (0, 0, 3), (1, 0, 0), (1, 0, 1)];
        let te2: [(i32, i32, usize); 4] = [(0, 0, 6), (0, 0, 7), (1, 0, 4), (1, 0, 5)];
        let re1: [(i32, i32, usize); 4] = [(0, 0, 10), (0, 0, 14), (0, 1, 2), (0, 1, 6)];
        let re2: [(i32, i32, usize); 4] = [(0, 0, 11), (0, 0, 15), (0, 1, 3), (0, 1, 7)];
        let be1: [(i32, i32, usize); 4] = [(0, 0, 10), (0, 0, 11), (1, 0, 8), (1, 0, 9)];
        let be2: [(i32, i32, usize); 4] = [(0, 0, 14), (0, 0, 15), (1, 0, 12), (1, 0, 13)];
        let tlc: [(i32, i32, usize); 4] = [(0, 0, 0), (0, 0, 1), (0, 0, 4), (0, 0, 5)];
        let trc: [(i32, i32, usize); 4] = [(0, 0, 2), (0, 0, 3), (0, 0, 6), (0, 0, 7)];
        let blc: [(i32, i32, usize); 4] = [(0, 0, 8), (0, 0, 9), (0, 0, 12), (0, 0, 13)];
        let brc: [(i32, i32, usize); 4] = [(0, 0, 10), (0, 0, 11), (0, 0, 14), (0, 0, 15)];
        let flc: [(i32, i32, usize); 16] = [
            (0, 0, 10),
            (0, 0, 11),
            (1, 0, 8),
            (1, 0, 9),
            (0, 0, 14),
            (0, 0, 15),
            (1, 0, 12),
            (1, 0, 13),
            (0, 1, 2),
            (0, 1, 3),
            (1, 1, 0),
            (1, 1, 1),
            (0, 1, 6),
            (0, 1, 7),
            (1, 1, 4),
            (1, 1, 5),
        ];

        // Closure-free zzz: indexes into mb_buffer.
        let zzz_lookup = |z: usize| -> usize { XY_TRANSPOSE[z] * 16 };

        for i in 0..nc {
            if i > 0 && matches!(self.planes[p].internal_clr_fmt, INT_YUV420 | INT_YUV422) {
                // Tables 154/155: dedicated chroma geometry.
                self.first_level_overlap_chroma(p, i);
                continue;
            }
            for tx in 0..self.hdr.num_tile_cols {
                for ty in 0..self.hdr.num_tile_rows {
                    let first_mbx = self.hdr.left_mb_index_of_tile[tx];
                    let last_mbx = self.hdr.left_mb_index_of_tile[tx + 1] - 1;
                    let first_mby = self.hdr.top_mb_index_of_tile[ty];
                    let last_mby = self.hdr.top_mb_index_of_tile[ty + 1] - 1;

                    for y in first_mby..last_mby {
                        for x in first_mbx..last_mbx {
                            flc4x4_op(&mut self.planes[p].mb, i, x, y, &flc, &zzz_lookup);
                        }
                    }
                    if tx == 0 || self.hdr.hard_tiling_flag != 0 {
                        for y in first_mby..last_mby {
                            filter4_op(&mut self.planes[p].mb, i, first_mbx, y, &le1, &zzz_lookup);
                            filter4_op(&mut self.planes[p].mb, i, first_mbx, y, &le2, &zzz_lookup);
                        }
                    }
                    if ty == 0 || self.hdr.hard_tiling_flag != 0 {
                        for x in first_mbx..last_mbx {
                            filter4_op(&mut self.planes[p].mb, i, x, first_mby, &te1, &zzz_lookup);
                            filter4_op(&mut self.planes[p].mb, i, x, first_mby, &te2, &zzz_lookup);
                        }
                    }
                    if tx == self.hdr.num_tile_cols - 1 || self.hdr.hard_tiling_flag != 0 {
                        for y in first_mby..last_mby {
                            filter4_op(&mut self.planes[p].mb, i, last_mbx, y, &re1, &zzz_lookup);
                            filter4_op(&mut self.planes[p].mb, i, last_mbx, y, &re2, &zzz_lookup);
                        }
                    }
                    if ty == self.hdr.num_tile_rows - 1 || self.hdr.hard_tiling_flag != 0 {
                        for x in first_mbx..last_mbx {
                            filter4_op(&mut self.planes[p].mb, i, x, last_mby, &be1, &zzz_lookup);
                            filter4_op(&mut self.planes[p].mb, i, x, last_mby, &be2, &zzz_lookup);
                        }
                    }
                    if (tx == 0 && ty == 0) || self.hdr.hard_tiling_flag != 0 {
                        filter4_op(
                            &mut self.planes[p].mb,
                            i,
                            first_mbx,
                            first_mby,
                            &tlc,
                            &zzz_lookup,
                        );
                    }
                    if (tx == self.hdr.num_tile_cols - 1 && ty == 0)
                        || self.hdr.hard_tiling_flag != 0
                    {
                        filter4_op(
                            &mut self.planes[p].mb,
                            i,
                            last_mbx,
                            first_mby,
                            &trc,
                            &zzz_lookup,
                        );
                    }
                    if (tx == 0 && ty == self.hdr.num_tile_rows - 1)
                        || self.hdr.hard_tiling_flag != 0
                    {
                        filter4_op(
                            &mut self.planes[p].mb,
                            i,
                            first_mbx,
                            last_mby,
                            &blc,
                            &zzz_lookup,
                        );
                    }
                    if (tx == self.hdr.num_tile_cols - 1 && ty == self.hdr.num_tile_rows - 1)
                        || self.hdr.hard_tiling_flag != 0
                    {
                        filter4_op(
                            &mut self.planes[p].mb,
                            i,
                            last_mbx,
                            last_mby,
                            &brc,
                            &zzz_lookup,
                        );
                    }
                    if self.hdr.hard_tiling_flag == 0 {
                        if tx != self.hdr.num_tile_cols - 1 {
                            for y in first_mby..last_mby {
                                flc4x4_op(
                                    &mut self.planes[p].mb,
                                    i,
                                    last_mbx,
                                    y,
                                    &flc,
                                    &zzz_lookup,
                                );
                            }
                        }
                        if ty != self.hdr.num_tile_rows - 1 {
                            for x in first_mbx..last_mbx {
                                flc4x4_op(
                                    &mut self.planes[p].mb,
                                    i,
                                    x,
                                    last_mby,
                                    &flc,
                                    &zzz_lookup,
                                );
                            }
                        }
                        if tx != self.hdr.num_tile_cols - 1 && ty != self.hdr.num_tile_rows - 1 {
                            flc4x4_op(
                                &mut self.planes[p].mb,
                                i,
                                last_mbx,
                                last_mby,
                                &flc,
                                &zzz_lookup,
                            );
                        }
                        if tx == 0 && ty != self.hdr.num_tile_rows - 1 {
                            filter4_op(
                                &mut self.planes[p].mb,
                                i,
                                first_mbx,
                                last_mby,
                                &le1,
                                &zzz_lookup,
                            );
                            filter4_op(
                                &mut self.planes[p].mb,
                                i,
                                first_mbx,
                                last_mby,
                                &le2,
                                &zzz_lookup,
                            );
                        }
                        if tx != self.hdr.num_tile_cols - 1 && ty == 0 {
                            filter4_op(
                                &mut self.planes[p].mb,
                                i,
                                last_mbx,
                                first_mby,
                                &te1,
                                &zzz_lookup,
                            );
                            filter4_op(
                                &mut self.planes[p].mb,
                                i,
                                last_mbx,
                                first_mby,
                                &te2,
                                &zzz_lookup,
                            );
                        }
                        if tx == self.hdr.num_tile_cols - 1 && ty != self.hdr.num_tile_rows - 1 {
                            filter4_op(
                                &mut self.planes[p].mb,
                                i,
                                last_mbx,
                                last_mby,
                                &re1,
                                &zzz_lookup,
                            );
                            filter4_op(
                                &mut self.planes[p].mb,
                                i,
                                last_mbx,
                                last_mby,
                                &re2,
                                &zzz_lookup,
                            );
                        }
                        if tx != self.hdr.num_tile_cols - 1 && ty == self.hdr.num_tile_rows - 1 {
                            filter4_op(
                                &mut self.planes[p].mb,
                                i,
                                last_mbx,
                                last_mby,
                                &be1,
                                &zzz_lookup,
                            );
                            filter4_op(
                                &mut self.planes[p].mb,
                                i,
                                last_mbx,
                                last_mby,
                                &be2,
                                &zzz_lookup,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Blocks per MB for component `i`: 4/8 for 420/422 chroma, else 16.
    fn blocks_per_mb(&self, p: usize, i: usize) -> usize {
        if i > 0 && self.planes[p].internal_clr_fmt == INT_YUV420 {
            4
        } else if i > 0 && self.planes[p].internal_clr_fmt == INT_YUV422 {
            8
        } else {
            16
        }
    }

    fn second_level_inverse_transform(&mut self, p: usize) {
        let nc = self.planes[p].num_components;
        for i in 0..nc {
            let nblk = self.blocks_per_mb(p, i);
            let cbase = i * MB_BUF_PER_COMP;
            for mby in 0..self.hdr.mb_height {
                for mbx in 0..self.hdr.mb_width {
                    let mb = &mut self.planes[p].mb[mbx][mby];
                    for j in 0..nblk {
                        let block = &mut mb.mb_buffer[cbase + j * 16..cbase + j * 16 + 16];
                        let mut coeff = [0i32; 16];
                        coeff.copy_from_slice(block);
                        str_idct4x4_stage1(&mut coeff);
                        block.copy_from_slice(&coeff);
                    }
                }
            }
        }
    }

    fn second_level_coefficient_combination(&mut self, p: usize) {
        let nc = self.planes[p].num_components;
        let w = self.hdr.width as usize;
        let h = self.hdr.height as usize;
        let int_fmt = self.planes[p].internal_clr_fmt;
        let mut ip: Vec<Plane2D> = (0..nc)
            .map(|i| {
                let (cw, ch) = self.component_extended_dims(int_fmt, i, w, h);
                Plane2D::new(cw, ch)
            })
            .collect();
        for i in 0..nc {
            let plane2d = &mut ip[i];
            let stride = plane2d.stride;
            let cbase = i * MB_BUF_PER_COMP;
            if i > 0 && matches!(int_fmt, INT_YUV420 | INT_YUV422) {
                // Table 157 chroma arms: blocks are raster-indexed, 2 per
                // row; MB footprint is 8 px wide (and 8 px tall for 420).
                let nblk = if int_fmt == INT_YUV420 { 4 } else { 8 };
                let mb_h = if int_fmt == INT_YUV420 { 8 } else { 16 };
                for mby in 0..self.hdr.mb_height {
                    let mbyy = mby * mb_h;
                    for mbx in 0..self.hdr.mb_width {
                        let mbxx = mbx * 8;
                        let mb = &self.planes[p].mb[mbx][mby];
                        for j in 0..nblk {
                            let bx4 = mbxx + 4 * (j % 2);
                            let by4 = mbyy + 4 * (j / 2);
                            let blk_off = cbase + 16 * j;
                            for py in 0..4 {
                                let row = by4 + py;
                                for px in 0..4 {
                                    plane2d.data[row * stride + bx4 + px] =
                                        mb.mb_buffer[blk_off + MB_PIXEL_MAP[px + 4 * py]];
                                }
                            }
                        }
                    }
                }
            } else {
                for mby in 0..self.hdr.mb_height {
                    let mbyy = mby << 4;
                    for mbx in 0..self.hdr.mb_width {
                        let mbxx = mbx << 4;
                        let mb = &self.planes[p].mb[mbx][mby];
                        for by in 0..4 {
                            let by_x4 = mbyy + (by << 2);
                            let by_x16 = by << 4;
                            for bx in 0..4 {
                                let bx_x4 = mbxx + (bx << 2);
                                let bx_x64 = bx << 6;
                                for py in 0..4 {
                                    let py_x4 = py << 2;
                                    let row = by_x4 + py;
                                    for px in 0..4 {
                                        plane2d.data[row * stride + bx_x4 + px] = mb.mb_buffer
                                            [cbase + by_x16 + bx_x64 + MB_PIXEL_MAP[px + py_x4]];
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        self.planes[p].image_plane = ip;
    }

    /// Extended (MB-padded) dimensions of component `i`'s plane.
    fn component_extended_dims(&self, int_fmt: u8, i: usize, w: usize, h: usize) -> (usize, usize) {
        if i > 0 && int_fmt == INT_YUV420 {
            (w / 2, h / 2)
        } else if i > 0 && int_fmt == INT_YUV422 {
            (w / 2, h)
        } else {
            (w, h)
        }
    }

    fn second_level_overlap_filtering(&mut self, p: usize) {
        let nc = self.planes[p].num_components;
        let int_fmt = self.planes[p].internal_clr_fmt;
        for i in 0..nc {
            // Table 159: identical structure for chroma, with coordinates
            // divided by the subsampling factors.
            let dx = if i > 0 && matches!(int_fmt, INT_YUV420 | INT_YUV422) {
                2
            } else {
                1
            };
            let dy = if i > 0 && int_fmt == INT_YUV420 { 2 } else { 1 };
            for tx in 0..self.hdr.num_tile_cols {
                for ty in 0..self.hdr.num_tile_rows {
                    let first_mbx = self.hdr.left_mb_index_of_tile[tx] * 16 / dx;
                    let next_mbx = self.hdr.left_mb_index_of_tile[tx + 1] * 16 / dx;
                    let first_mby = self.hdr.top_mb_index_of_tile[ty] * 16 / dy;
                    let next_mby = self.hdr.top_mb_index_of_tile[ty + 1] * 16 / dy;

                    let ip = &mut self.planes[p].image_plane[i];

                    let mut x = first_mbx + 2;
                    while x < next_mbx.saturating_sub(2) {
                        let mut y = first_mby + 2;
                        while y < next_mby.saturating_sub(2) {
                            ip_4x4_op(ip, x, y, &XY4);
                            y += 4;
                        }
                        x += 4;
                    }

                    if tx == 0 || self.hdr.hard_tiling_flag != 0 {
                        let mut y = first_mby + 2;
                        while y < next_mby.saturating_sub(2) {
                            for xx in [0, 1] {
                                ip_4_op(ip, first_mbx + xx, y, &Y4);
                            }
                            y += 4;
                        }
                    }
                    if ty == 0 || self.hdr.hard_tiling_flag != 0 {
                        let mut x = first_mbx + 2;
                        while x < next_mbx.saturating_sub(2) {
                            for yy in [0, 1] {
                                ip_4_op(ip, x, first_mby + yy, &X4);
                            }
                            x += 4;
                        }
                    }
                    if tx == self.hdr.num_tile_cols - 1 || self.hdr.hard_tiling_flag != 0 {
                        let mut y = first_mby + 2;
                        while y < next_mby.saturating_sub(2) {
                            for xx in [-2isize, -1] {
                                let xpos = (next_mbx as isize + xx) as usize;
                                ip_4_op(ip, xpos, y, &Y4);
                            }
                            y += 4;
                        }
                    }
                    if ty == self.hdr.num_tile_rows - 1 || self.hdr.hard_tiling_flag != 0 {
                        let mut x = first_mbx + 2;
                        while x < next_mbx.saturating_sub(2) {
                            for yy in [-2isize, -1] {
                                let ypos = (next_mby as isize + yy) as usize;
                                ip_4_op(ip, x, ypos, &X4);
                            }
                            x += 4;
                        }
                    }
                    if (tx == 0 && ty == 0) || self.hdr.hard_tiling_flag != 0 {
                        ip_4_op(ip, first_mbx, first_mby, &XY2);
                    }
                    if (tx == self.hdr.num_tile_cols - 1 && ty == 0)
                        || self.hdr.hard_tiling_flag != 0
                    {
                        ip_4_op(ip, next_mbx - 2, first_mby, &XY2);
                    }
                    if (tx == 0 && ty == self.hdr.num_tile_rows - 1)
                        || self.hdr.hard_tiling_flag != 0
                    {
                        ip_4_op(ip, first_mbx, next_mby - 2, &XY2);
                    }
                    if (tx == self.hdr.num_tile_cols - 1 && ty == self.hdr.num_tile_rows - 1)
                        || self.hdr.hard_tiling_flag != 0
                    {
                        ip_4_op(ip, next_mbx - 2, next_mby - 2, &XY2);
                    }
                    if self.hdr.hard_tiling_flag == 0 {
                        if tx != self.hdr.num_tile_cols - 1 {
                            let mut y = first_mby + 2;
                            while y < next_mby.saturating_sub(2) {
                                ip_4x4_op(ip, next_mbx - 2, y, &XY4);
                                y += 4;
                            }
                        }
                        if ty != self.hdr.num_tile_rows - 1 {
                            let mut x = first_mbx + 2;
                            while x < next_mbx.saturating_sub(2) {
                                ip_4x4_op(ip, x, next_mby - 2, &XY4);
                                x += 4;
                            }
                        }
                        if tx != self.hdr.num_tile_cols - 1 && ty != self.hdr.num_tile_rows - 1 {
                            ip_4x4_op(ip, next_mbx - 2, next_mby - 2, &XY4);
                        }
                        if tx == 0 && ty != self.hdr.num_tile_rows - 1 {
                            for xx in 0..2 {
                                ip_4_op(ip, first_mbx + xx, next_mby - 2, &Y4);
                            }
                        }
                        if tx != self.hdr.num_tile_cols - 1 && ty == 0 {
                            for yy in 0..2 {
                                ip_4_op(ip, next_mbx - 2, first_mby + yy, &X4);
                            }
                        }
                        if tx == self.hdr.num_tile_cols - 1 && ty != self.hdr.num_tile_rows - 1 {
                            for xx in [-2isize, -1] {
                                let xpos = (next_mbx as isize + xx) as usize;
                                ip_4_op(ip, xpos, next_mby - 2, &Y4);
                            }
                        }
                        if tx != self.hdr.num_tile_cols - 1 && ty == self.hdr.num_tile_rows - 1 {
                            for yy in [-2isize, -1] {
                                let ypos = (next_mby as isize + yy) as usize;
                                ip_4_op(ip, next_mbx - 2, ypos, &X4);
                            }
                        }
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Output formatting
    // -----------------------------------------------------------------

    fn output_formatting(&mut self, p: usize) -> Result<()> {
        self.convert_internal_to_output_clr_fmt(p)?;
        self.add_bias(p)?;
        self.compute_scaling(p);
        self.postscaling_process(p)?;
        self.clipping_and_packing_stage(p)?;
        Ok(())
    }

    fn convert_internal_to_output_clr_fmt(&mut self, p: usize) -> Result<()> {
        let plane = &self.planes[p];
        if plane.is_alpha {
            if plane.internal_clr_fmt != INT_YONLY {
                return Err(DecodeError::Unsupported("alpha plane must be YONLY".into()));
            }
            return Ok(());
        }

        let int_fmt = plane.internal_clr_fmt;
        let out_fmt = self.hdr.output_clr_fmt;
        let w = self.hdr.width as usize;
        let h = self.hdr.height as usize;

        if int_fmt == INT_YONLY && out_fmt == OUT_RGB {
            // Replicate Y into 3 channels.
            let src = self.planes[p].image_plane[0].clone();
            self.planes[p].image_plane.push(src.clone());
            self.planes[p].image_plane.push(src);
            self.planes[p].num_components = 3;
            return Ok(());
        }

        if matches!(int_fmt, INT_YUV420 | INT_YUV422) && matches!(out_fmt, OUT_RGB | OUT_RGBE) {
            // Table 178: 420 upsamples vertically then horizontally; 422
            // horizontally only. After that the planes are 4:4:4-shaped and
            // the standard YUV444→RGB conversion below applies.
            self.upsample_chroma(p);
        }

        // RGBE shares the YUV→RGB lifting; PostScalingF2 packs E afterwards.
        if matches!(int_fmt, INT_YUV444 | INT_YUV420 | INT_YUV422)
            && matches!(out_fmt, OUT_RGB | OUT_RGBE)
        {
            // Packed formats: the pack stage (Table 196/197/198) expects
            // B,G,R plane order; the file's components are already swapped
            // when RED_BLUE_NOT_SWAPPED_FLAG is 0, so swap on flag == 1
            // (verified against JxrDecApp on a minted 565 file).
            let do_swap = matches!(self.hdr.output_bitdepth, BD5 | BD565 | BD10)
                && self.hdr.red_blue_not_swapped_flag != 0;
            // Get disjoint mutable borrows of the three component planes
            // so we can do the YUV→RGB transform in one row-major sweep.
            let stride = self.planes[p].image_plane[0].stride;
            let (left, right) = self.planes[p].image_plane.split_at_mut(1);
            let y_plane = &mut left[0].data;
            let (u_left, v_left) = right.split_at_mut(1);
            let u_plane = &mut u_left[0].data;
            let v_plane = &mut v_left[0].data;
            for row in 0..h {
                let base = row * stride;
                for col in 0..w {
                    let idx = base + col;
                    let (out0, out1, out2) =
                        yuv444_to_rgb(y_plane[idx], u_plane[idx], v_plane[idx]);
                    let (a, b, c) = if do_swap {
                        (out2, out1, out0)
                    } else {
                        (out0, out1, out2)
                    };
                    y_plane[idx] = a;
                    u_plane[idx] = b;
                    v_plane[idx] = c;
                }
            }
            return Ok(());
        }

        if int_fmt == INT_YUVK && out_fmt == OUT_CMYK {
            // Table 186 InvColorFmtConvert3: YUVK → CMYK lifting.
            let n = self.planes[p].image_plane[0].data.len();
            for idx in 0..n {
                let y = self.planes[p].image_plane[0].data[idx];
                let u = self.planes[p].image_plane[1].data[idx];
                let v = self.planes[p].image_plane[2].data[idx];
                let k0 = self.planes[p].image_plane[3].data[idx];
                let k = k0 + (y >> 1);
                let m = k - y - (u >> 1);
                let c = u + m + (v >> 1);
                let yy = c - v;
                self.planes[p].image_plane[0].data[idx] = c;
                self.planes[p].image_plane[1].data[idx] = m;
                self.planes[p].image_plane[2].data[idx] = yy;
                self.planes[p].image_plane[3].data[idx] = k;
            }
            return Ok(());
        }
        if int_fmt == INT_YUVK && out_fmt == OUT_CMYKDIRECT {
            // Table 187 InvColorFmtConvert4: pure channel shuffle.
            let n = self.planes[p].image_plane[0].data.len();
            for idx in 0..n {
                let y = self.planes[p].image_plane[0].data[idx];
                let u = self.planes[p].image_plane[1].data[idx];
                let v = self.planes[p].image_plane[2].data[idx];
                let k = self.planes[p].image_plane[3].data[idx];
                self.planes[p].image_plane[0].data[idx] = u;
                self.planes[p].image_plane[1].data[idx] = v;
                self.planes[p].image_plane[2].data[idx] = k;
                self.planes[p].image_plane[3].data[idx] = y;
            }
            return Ok(());
        }

        // Same-color-format passthrough.
        let same = matches!(
            (int_fmt, out_fmt),
            (INT_YONLY, OUT_YONLY) | (INT_YUV444, OUT_YUV444) | (INT_NCOMPONENT, OUT_NCOMPONENT)
        );
        if !same {
            return Err(DecodeError::Unsupported(format!(
                "color {} -> {} not supported",
                int_fmt, out_fmt
            )));
        }
        Ok(())
    }

    /// Table 180 chroma upsampling (separable; taps by chroma centering).
    /// Replaces chroma planes with full-extended-size planes.
    fn upsample_chroma(&mut self, p: usize) {
        let int_fmt = self.planes[p].internal_clr_fmt;
        let cx = self.planes[p].chroma_centering_x as usize;
        let cy = self.planes[p].chroma_centering_y as usize;
        const TAPS: [[i32; 4]; 5] = [
            [4, 4, 0, 8],
            [5, 3, 1, 7],
            [6, 2, 2, 6],
            [7, 1, 3, 5],
            [8, 0, 4, 4],
        ];
        let up_1d = |ori: &[i32], out: &mut [i32], h: &[i32; 4]| {
            let n = ori.len();
            for k in 0..n {
                let prev = ori[k.saturating_sub(1)];
                let next = ori[(k + 1).min(n - 1)];
                out[2 * k] = (h[2] * prev + h[3] * ori[k] + 4) >> 3;
                out[2 * k + 1] = (h[0] * ori[k] + h[1] * next + 4) >> 3;
            }
        };
        for i in 1..3 {
            let mut plane =
                std::mem::replace(&mut self.planes[p].image_plane[i], Plane2D::new(0, 0));
            if int_fmt == INT_YUV420 {
                // Vertical first (Table 178).
                let (w, h) = (plane.stride, plane.height);
                let mut vert = Plane2D::new(w, h * 2);
                let mut col_in = vec![0i32; h];
                let mut col_out = vec![0i32; h * 2];
                for x in 0..w {
                    for y in 0..h {
                        col_in[y] = plane.data[y * w + x];
                    }
                    up_1d(&col_in, &mut col_out, &TAPS[cy.min(4)]);
                    for y in 0..h * 2 {
                        vert.data[y * w + x] = col_out[y];
                    }
                }
                plane = vert;
            }
            // Horizontal (both 420 and 422).
            let (w, h) = (plane.stride, plane.height);
            let mut horiz = Plane2D::new(w * 2, h);
            let mut row_out = vec![0i32; w * 2];
            for y in 0..h {
                let row_in = &plane.data[y * w..(y + 1) * w];
                up_1d(row_in, &mut row_out, &TAPS[cx.min(4)]);
                horiz.data[y * w * 2..(y + 1) * w * 2].copy_from_slice(&row_out);
            }
            self.planes[p].image_plane[i] = horiz;
        }
    }

    fn add_bias(&mut self, p: usize) -> Result<()> {
        // Table 188. The bias for the deep integer formats is pre-shifted
        // down by SHIFT_BITS (PostScalingInt later shifts it back up).
        let i_scale = if self.planes[p].scaled_flag != 0 {
            3
        } else {
            0
        };
        let mut bias_base = match self.hdr.output_bitdepth {
            BD5 => 1 << 4,
            BD565 => 1 << 5,
            BD8 => 1 << 7,
            BD10 => 1 << 9,
            BD16 => 1 << 15,
            _ => 0i32,
        };
        if matches!(self.hdr.output_bitdepth, BD16 | BD16S | BD32S) {
            bias_base >>= self.planes[p].shift_bits;
        }
        let i_bias = bias_base << i_scale;
        // The alpha image plane is its own YONLY channel: it takes the plain
        // bias regardless of the image's color format (the OUT_CMYK half/−K
        // arm below is primary-plane semantics — indexing it against the
        // 1-component alpha plane walked out of bounds; found by the first
        // `-a 3` CMYKA decode, 6a).
        if self.planes[p].is_alpha {
            if i_bias != 0 {
                for v in &mut self.planes[p].image_plane[0].data {
                    *v += i_bias;
                }
            }
            return Ok(());
        }
        match self.hdr.output_clr_fmt {
            OUT_RGB | OUT_YUV444 | OUT_YUV422 | OUT_YUV420 | OUT_YONLY | OUT_NCOMPONENT
            | OUT_CMYKDIRECT => {
                let nc = if matches!(
                    self.hdr.output_clr_fmt,
                    OUT_RGB | OUT_YUV444 | OUT_YUV422 | OUT_YUV420
                ) {
                    3
                } else {
                    self.planes[p].num_components
                };
                if i_bias != 0 {
                    for i in 0..nc.min(self.planes[p].image_plane.len()) {
                        for v in &mut self.planes[p].image_plane[i].data {
                            *v += i_bias;
                        }
                    }
                }
            }
            OUT_CMYK => {
                let half = (bias_base >> 1) << i_scale;
                for i in 0..3 {
                    for v in &mut self.planes[p].image_plane[i].data {
                        *v += half;
                    }
                }
                for v in &mut self.planes[p].image_plane[3].data {
                    *v -= half;
                }
            }
            _ => {} // RGBE: no bias
        }
        Ok(())
    }

    fn compute_scaling(&mut self, p: usize) {
        let mut i_scale = 0i32;
        let mut i_rounding = 0i32;
        if self.planes[p].scaled_flag != 0 {
            i_scale = 3;
            i_rounding = match self.hdr.output_bitdepth {
                BD5 | BD565 | BD8 | BD10 | BD16S | BD16F | BD32S | BD32F => 3,
                BD1WHITE1 | BD1BLACK1 | BD16 => 4,
                _ => 0,
            };
        }
        // Per PLANE, not per image (jxr_image.py:991 tests the plane's own
        // `internal_clr_fmt`; its `[RGB, RGBE, YUV444]` list is effectively
        // `== YUV444` — RGB/RGBE are OUTPUT-format codes 7/8, which the
        // 3-bit internal field can never hold). The alpha image plane is
        // YONLY (1 component): scaling it with the image-level color format
        // indexed past its single plane. Found by 5e's scaled-alpha encodes
        // — the first files either encoder ever emitted that scale a
        // 1-component plane in a multi-component image.
        let output_components = if self.planes[p].internal_clr_fmt == INT_YUV444 {
            3
        } else {
            self.planes[p].num_components
        };
        for i in 0..output_components {
            let j_scale = if self.hdr.output_bitdepth == BD565 && i != 1 {
                i_scale + 1
            } else {
                i_scale
            };
            if i_rounding != 0 || j_scale != 0 {
                for v in &mut self.planes[p].image_plane[i].data {
                    *v = (*v + i_rounding) >> j_scale;
                }
            }
        }
    }

    fn postscaling_process(&mut self, p: usize) -> Result<()> {
        // Table 190.
        if self.hdr.output_clr_fmt == OUT_RGBE {
            // PostScalingF2 (Table 193): 3 internal values → 4-plane RGBE.
            let n = self.planes[p].image_plane[0].data.len();
            let stride = self.planes[p].image_plane[0].stride;
            let height = self.planes[p].image_plane[0].height;
            let mut e_plane = Plane2D::new(stride, height);
            for idx in 0..n {
                let mut out = [0i32; 4];
                let mut exps = [0i32; 3];
                for k in 0..3 {
                    let v = self.planes[p].image_plane[k].data[idx];
                    if v <= 0 {
                        out[k] = 0;
                        exps[k] = 0;
                    } else if (v >> 7) > 1 {
                        out[k] = (v & 0x7F) + 128;
                        exps[k] = v >> 7;
                    } else {
                        out[k] = v;
                        exps[k] = 1;
                    }
                }
                out[3] = exps[0].max(exps[1]).max(exps[2]);
                for k in 0..3 {
                    if out[3] > exps[k] {
                        let shift = out[3] - exps[k];
                        out[k] = (2 * out[k] + 1) >> (shift + 1);
                    }
                }
                for k in 0..3 {
                    self.planes[p].image_plane[k].data[idx] = out[k];
                }
                e_plane.data[idx] = out[3];
            }
            self.planes[p].image_plane.push(e_plane);
            self.planes[p].num_components = 4;
            return Ok(());
        }

        let nc = if matches!(
            self.hdr.output_clr_fmt,
            OUT_RGB | OUT_YUV444 | OUT_YUV422 | OUT_YUV420
        ) {
            3
        } else {
            self.planes[p].num_components
        };
        let nc = nc.min(self.planes[p].image_plane.len());
        match self.hdr.output_bitdepth {
            BD16 | BD16S | BD32S => {
                let s = self.planes[p].shift_bits;
                if s != 0 {
                    for i in 0..nc {
                        for v in &mut self.planes[p].image_plane[i].data {
                            *v <<= s;
                        }
                    }
                }
            }
            BD16F => {
                // Table 192: sign bit ‖ min(|x|, 32767) — the half bits.
                for i in 0..nc {
                    for v in &mut self.planes[p].image_plane[i].data {
                        let s = if *v < 0 { 1i32 } else { 0 };
                        let em = v.abs().min(32767);
                        *v = (s << 15) | em;
                    }
                }
            }
            BD32F => {
                let len_mantissa = self.planes[p].len_mantissa as i32;
                let exp_bias = self.planes[p].exp_bias;
                for i in 0..nc {
                    for v in &mut self.planes[p].image_plane[i].data {
                        *v = postscale_f32(*v, len_mantissa, exp_bias);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn clipping_and_packing_stage(&mut self, p: usize) -> Result<()> {
        // Tables 194/195: ints clip to range; BD16F/BD32F/BD32S pass through
        // unclipped (float bit patterns / full range) — windowing crop only.
        let out_h = self.hdr.image_height as usize;
        let out_w = self.hdr.image_width as usize;
        let n = self.hdr.extra_pixels_top as usize;
        let m = self.hdr.extra_pixels_left as usize;

        if matches!(self.hdr.output_bitdepth, BD5 | BD565 | BD10)
            && self.hdr.output_clr_fmt == OUT_RGB
        {
            // Tables 196/197/198: pack the three clipped channels into one
            // value per pixel (single output array). Channel order in the
            // planes at this point is the pack-ready order (the conversion
            // stage's swap handling is oracle-verified for 565; BD5/BD10
            // are unmintable via JxrEncApp — spec-transcribed self-loop).
            let (hi1, hi2, max0, max1) = match self.hdr.output_bitdepth {
                BD5 => (5u32, 10u32, 31, 31),
                BD10 => (10, 20, 1023, 1023),
                _ => (5, 11, 31, 63),
            };
            let stride = self.planes[p].image_plane[0].stride;
            let mut packed = Plane2D::new(out_w, out_h);
            for y in 0..out_h {
                for x in 0..out_w {
                    let src_idx = (y + n) * stride + m + x;
                    let c0 = clip(self.planes[p].image_plane[0].data[src_idx], 0, max0);
                    let c1 = clip(self.planes[p].image_plane[1].data[src_idx], 0, max1);
                    let c2 = clip(self.planes[p].image_plane[2].data[src_idx], 0, max0);
                    packed.data[y * out_w + x] = c0 + (c1 << hi1) + (c2 << hi2);
                }
            }
            self.planes[p].image_plane = vec![packed];
            self.planes[p].num_components = 1;
            return Ok(());
        }

        let (clip_low, clip_high) = match self.hdr.output_bitdepth {
            BD1BLACK1 | BD1WHITE1 => (0, 1),
            BD8 => (0, 255),
            BD16 => (0, 65535),
            BD16S => (-32768, 32767),
            BD16F | BD32F | BD32S => (i32::MIN, i32::MAX),
            _ => {
                return Err(DecodeError::Unsupported(format!(
                    "output bit depth {}",
                    self.hdr.output_bitdepth
                )));
            }
        };
        let nc = self.planes[p].num_components;

        for i in 0..nc {
            let src = &self.planes[p].image_plane[i];
            let src_stride = src.stride;
            let mut new_plane = Plane2D::new(out_w, out_h);
            for y in 0..out_h {
                let dst_row = &mut new_plane.data[y * out_w..(y + 1) * out_w];
                let src_y = y + n;
                let src_row = &src.data[src_y * src_stride + m..src_y * src_stride + m + out_w];
                for (d, s) in dst_row.iter_mut().zip(src_row.iter()) {
                    *d = clip(*s, clip_low, clip_high);
                }
            }
            self.planes[p].image_plane[i] = new_plane;
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Final image assembly
    // -----------------------------------------------------------------

    fn construct_image(mut self) -> DecodedImage {
        let w = self.hdr.image_width;
        let h = self.hdr.image_height;
        let num_components = self.planes[0].num_components
            + if self.hdr.alpha_image_plane_flag != 0 {
                1
            } else {
                0
            };

        // Each Plane2D is already row-major `[y*stride + x]` with stride =
        // image_width after clipping_and_packing_stage, so we can move the
        // contiguous Vec<i32> straight out — no transpose, no copy.
        let mut planes_flat: Vec<Vec<i32>> = Vec::with_capacity(num_components);
        let primary_nc = self.planes[0].num_components;
        for i in 0..primary_nc {
            debug_assert_eq!(self.planes[0].image_plane[i].stride, w as usize);
            planes_flat.push(std::mem::take(&mut self.planes[0].image_plane[i].data));
        }
        if self.hdr.alpha_image_plane_flag != 0 {
            debug_assert_eq!(self.planes[1].image_plane[0].stride, w as usize);
            planes_flat.push(std::mem::take(&mut self.planes[1].image_plane[0].data));
        }

        DecodedImage {
            width: w,
            height: h,
            image_plane: planes_flat,
            num_components,
            output_clr_fmt: self.hdr.output_clr_fmt,
            output_bitdepth: self.hdr.output_bitdepth,
            red_blue_swapped: self.hdr.red_blue_not_swapped_flag == 0,
            has_alpha: self.hdr.alpha_image_plane_flag != 0,
            premultiplied_alpha: self.hdr.premultiplied_alpha_flag != 0,
            timing: DecodeTiming::default(),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) enum ModelBand {
    DC,
    LP,
    HP,
}

impl ModelBand {
    fn as_index(self) -> usize {
        match self {
            ModelBand::DC => 0,
            ModelBand::LP => 1,
            ModelBand::HP => 2,
        }
    }
}

/// First-level overlap-filter helper: read 16 coefficients across 4 MBs,
/// run `strPost4x4Stage2Split_alternate`, write them back.
fn flc4x4_op(
    mb: &mut [Vec<MB>],
    i: usize,
    x: usize,
    y: usize,
    list: &[(i32, i32, usize); 16],
    zzz: &dyn Fn(usize) -> usize,
) {
    let cbase = i * MB_BUF_PER_COMP;
    let mut arr = [0i32; 16];
    for (k, (xx, yy, zz)) in list.iter().enumerate() {
        arr[k] = mb[x + *xx as usize][y + *yy as usize].mb_buffer[cbase + zzz(*zz)];
    }
    let out = str_post_4x4_stage2_split_alternate(&arr);
    for (k, (xx, yy, zz)) in list.iter().enumerate() {
        mb[x + *xx as usize][y + *yy as usize].mb_buffer[cbase + zzz(*zz)] = out[k];
    }
}

/// First-level overlap-filter helper for 4-coeff edge lists.
fn filter4_op(
    mb: &mut [Vec<MB>],
    i: usize,
    x: usize,
    y: usize,
    list: &[(i32, i32, usize); 4],
    zzz: &dyn Fn(usize) -> usize,
) {
    let cbase = i * MB_BUF_PER_COMP;
    let mut arr = [0i32; 4];
    for (k, (xx, yy, zz)) in list.iter().enumerate() {
        arr[k] = mb[x + *xx as usize][y + *yy as usize].mb_buffer[cbase + zzz(*zz)];
    }
    let out = overlap_post_filter_4(arr);
    for (k, (xx, yy, zz)) in list.iter().enumerate() {
        mb[x + *xx as usize][y + *yy as usize].mb_buffer[cbase + zzz(*zz)] = out[k];
    }
}

/// Second-level overlap-filter helper: 4x4 block.
fn ip_4x4_op(ip: &mut Plane2D, x: usize, y: usize, list: &[(i32, i32)]) {
    let stride = ip.stride;
    let mut arr = [0i32; 16];
    for (k, (xx, yy)) in list.iter().enumerate() {
        let xi = x + *xx as usize;
        let yi = y + *yy as usize;
        arr[k] = ip.data[yi * stride + xi];
    }
    let out = overlap_post_filter_4x4(arr);
    for (k, (xx, yy)) in list.iter().enumerate() {
        let xi = x + *xx as usize;
        let yi = y + *yy as usize;
        ip.data[yi * stride + xi] = out[k];
    }
}

/// Second-level overlap-filter helper: 4-coeff edge.
fn ip_4_op(ip: &mut Plane2D, x: usize, y: usize, list: &[(i32, i32)]) {
    let stride = ip.stride;
    let mut arr = [0i32; 4];
    for (k, (xx, yy)) in list.iter().enumerate() {
        let xi = x + *xx as usize;
        let yi = y + *yy as usize;
        arr[k] = ip.data[yi * stride + xi];
    }
    let out = overlap_post_filter_4(arr);
    for (k, (xx, yy)) in list.iter().enumerate() {
        let xi = x + *xx as usize;
        let yi = y + *yy as usize;
        ip.data[yi * stride + xi] = out[k];
    }
}

#[inline]
pub(crate) fn floor_div2(x: i32) -> i32 {
    // Python's `math.floor(x / 2)` differs from `x / 2` for negative odd x.
    // Use arithmetic right shift which is floor-toward-negative-infinity.
    x >> 1
}

#[inline]
pub(crate) fn ceil_div2(x: i32) -> i32 {
    // Python's `math.ceil(x / 2)` for ints. (x + sign(x)) / 2 for trunc-div
    // doesn't equal ceil. Use: ceil(a/2) = -floor(-a/2).
    -((-x) >> 1)
}

/// Per-pixel inverse color transform for `INT_YUV444 → OUT_RGB`, the exact
/// integer lifting the JPEG-XR spec / libjxr `strInvTransform` use. Returns the
/// **pre-bias** centered RGB (the decoder adds the `1<<(bd-1)` bias afterwards).
/// Single source of truth shared with the encoder's forward transform
/// ([`crate::encode::color::rgb_to_yuv444`], its exact inverse).
pub(crate) fn yuv444_to_rgb(y: i32, u: i32, v: i32) -> (i32, i32, i32) {
    let temp_t = -u;
    let g = y - floor_div2(temp_t); // out1
    let r = temp_t + g - ceil_div2(v); // out0
    let b = v + r; // out2
    (r, g, b)
}

/// Table 192, BD32F arm: reassemble the custom (LEN_MANTISSA, EXP_BIAS)
/// float as IEEE 754 single bits.
fn postscale_f32(ix: i32, len_mantissa: i32, exp_bias: i32) -> i32 {
    let i_s = if ix < 0 { 1i32 } else { 0 };
    let ix = ix.abs();
    let mut i_e = ix >> len_mantissa;
    let mut i_m = (ix & ((1 << len_mantissa) - 1)) | (1 << len_mantissa);
    if i_e == 0 {
        i_m ^= 1 << len_mantissa;
        i_e = 1;
    }
    i_e = i_e - exp_bias + 127;
    while i_m < (1 << len_mantissa) && i_e > 1 && i_m > 0 {
        i_e -= 1;
        i_m <<= 1;
    }
    if i_m < (1 << len_mantissa) {
        i_e = 0;
    } else {
        i_m ^= 1 << len_mantissa;
    }
    i_m <<= 23 - len_mantissa;
    (i_s << 31) | (i_e << 23) | i_m
}
