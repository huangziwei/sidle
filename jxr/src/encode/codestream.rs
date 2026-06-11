//! WMPHOTO codestream framing writer (encode side): spatial mode, single
//! tile, no overlap, derived windowing (`windowing_flag = 0`), ALL_BANDS,
//! `8bppGray` or `24bppRGB`, uniform per-band QP.
//!
//! Each writer mirrors the corresponding decoder reader in
//! `decode::decoder` field-for-field, so the decoder parses exactly what we
//! emit. The DC *value* coding (`mb_dc`) lives in [`super::coeff`]; this module
//! is the surrounding header/tile frame.

use super::bitstream::BitWriter;
use crate::decode::consts::*;

/// `image_header` for a `width`×`height` image with the given `output_clr_fmt`
/// (`OUT_YONLY` for grayscale, `OUT_RGB` for color). Mirrors
/// `Decoder::image_header`. `width`/`height` are the true (unpadded) dims;
/// `windowing_flag = 0` makes the decoder derive the 16-aligned padding and crop
/// back to these. `alpha_image_plane` declares the in-codestream alpha plane
/// (T.832 "alpha image plane"; per-MB interleaved — what JxrEncApp calls
/// *interleaved* alpha, `-a 3`); `premultiplied_alpha` is the matching
/// property bit (8.3.17: a property, not a different coding).
pub fn write_image_header(
    bw: &mut BitWriter,
    width: u32,
    height: u32,
    output_clr_fmt: u8,
    premultiplied_alpha: bool,
    alpha_image_plane: bool,
) {
    write_image_header_ext(bw, width, height, output_clr_fmt, premultiplied_alpha, alpha_image_plane, 0)
}

/// [`write_image_header`] with `trim_flexbits` (1–15 sets the flag; the 4-bit
/// trim value itself is emitted in the spatial tile's flex plane header) and
/// AUTOMATIC long-header selection: dims beyond the 16-bit short-header range
/// switch `short_header_flag` off and emit 32-bit dims.
#[allow(clippy::too_many_arguments)]
pub fn write_image_header_ext(
    bw: &mut BitWriter,
    width: u32,
    height: u32,
    output_clr_fmt: u8,
    premultiplied_alpha: bool,
    alpha_image_plane: bool,
    trim_flexbits: u8,
) {
    let mut spec = ImageHeaderSpec::new(width, height, output_clr_fmt);
    spec.premultiplied_alpha = premultiplied_alpha;
    spec.alpha_image_plane = alpha_image_plane;
    spec.trim_flexbits = trim_flexbits;
    write_image_header_spec(bw, &spec);
}

/// Every `image_header` degree of freedom the encoder supports, in one spec
/// so the emission lives in a single writer that mirrors
/// `Decoder::image_header` field-for-field. [`ImageHeaderSpec::new`] is the
/// classic frame (single tile, spatial order, no overlap, derived windowing);
/// drivers override fields from there.
#[derive(Clone, Debug)]
pub struct ImageHeaderSpec {
    /// Window (output) dims — what the decoder crops to and the container
    /// declares.
    pub width: u32,
    pub height: u32,
    pub output_clr_fmt: u8,
    /// T.832 `OUTPUT_BITDEPTH` code (BD8 unless a deep input set it).
    pub output_bitdepth: u8,
    pub premultiplied_alpha: bool,
    pub alpha_image_plane: bool,
    pub trim_flexbits: u8,
    /// `overlap_mode` (0/1/2 — none / first-level / both levels).
    pub overlap_mode: u8,
    /// Explicit window margins (top, left, bottom, right), each < 64, with
    /// `top + height + bottom` and `left + width + right` 16-aligned.
    /// Non-zero top/left ⇒ `windowing_flag = 1` and all four are emitted;
    /// otherwise the flag stays 0 and bottom/right must equal the
    /// decoder-derived 16-alignment pads (asserted).
    pub margins: (u32, u32, u32, u32),
    /// Tile column widths / row heights in MB units — EVERY tile including
    /// the last (the writer drops the last entry; the decoder re-derives it).
    /// Empty = 1 tile in that dimension.
    pub tile_cols_mb: Vec<usize>,
    pub tile_rows_mb: Vec<usize>,
    /// Frequency order (band-major tile packets) instead of spatial.
    pub frequency_mode: bool,
}

impl ImageHeaderSpec {
    pub fn new(width: u32, height: u32, output_clr_fmt: u8) -> Self {
        Self {
            width,
            height,
            output_clr_fmt,
            output_bitdepth: BD8,
            premultiplied_alpha: false,
            alpha_image_plane: false,
            trim_flexbits: 0,
            overlap_mode: NO_OVERLAP_FILTERING,
            margins: (0, 0, 0, 0),
            tile_cols_mb: Vec::new(),
            tile_rows_mb: Vec::new(),
            frequency_mode: false,
        }
    }

    /// Number of tiles (≥ 1 per dimension).
    pub fn num_tiles(&self) -> (usize, usize) {
        (self.tile_cols_mb.len().max(1), self.tile_rows_mb.len().max(1))
    }

    /// Whether the codestream carries an index table (frequency mode or
    /// more than one tile — T.832 requires it for both).
    pub fn index_table_present(&self) -> bool {
        let (c, r) = self.num_tiles();
        self.frequency_mode || c * r > 1
    }

    /// Whether the short header form can carry this spec: dims fit 16 bits
    /// AND every emitted tile-size list entry fits 8 bits.
    fn short_header(&self) -> bool {
        let lists_fit = self
            .tile_cols_mb
            .iter()
            .chain(self.tile_rows_mb.iter())
            .all(|&mb| mb <= 0xFF);
        self.width <= 1 << 16 && self.height <= 1 << 16 && lists_fit
    }
}

/// The general `image_header` writer ([`ImageHeaderSpec`]). Field order is
/// the decoder's: fixed flags, dims, tile counts + size lists, window
/// margins.
pub fn write_image_header_spec(bw: &mut BitWriter, s: &ImageHeaderSpec) {
    let short = s.short_header();
    let (num_cols, num_rows) = s.num_tiles();
    let tiling = num_cols * num_rows > 1;
    let windowing = s.margins.0 != 0 || s.margins.1 != 0;
    debug_assert!(
        windowing
            || ((s.margins.2 == 0 || s.margins.2 == (16 - s.height % 16) % 16)
                && (s.margins.3 == 0 || s.margins.3 == (16 - s.width % 16) % 16)),
        "windowing_flag=0 requires decoder-derivable bottom/right margins"
    );
    for &b in b"WMPHOTO\x00" {
        bw.write_bits(b as u64, 8);
    }
    bw.write_bits(1, 4); // codec_version (==1)
    bw.write_bits(0, 1); // hard_tiling_flag (soft tiles: overlap crosses tiles)
    bw.write_bits(1, 3); // codec_subversion (==1)
    bw.write_flag(tiling); // tiling_flag
    bw.write_flag(s.frequency_mode); // frequency_mode
    bw.write_bits(0, 3); // spatial_xfrm_subordinate
    bw.write_flag(s.index_table_present()); // index_table_present_flag
    bw.write_bits(s.overlap_mode as u64, 2); // overlap_mode
    bw.write_flag(short); // short_header_flag (16- vs 32-bit dims)
    bw.write_bits(1, 1); // long_word_flag (Amazon sets this; decoder ignores it)
    bw.write_flag(windowing); // windowing_flag
    bw.write_flag(s.trim_flexbits != 0); // trim_flexbits_flag
    bw.write_bits(0, 1); // reserved_d
    bw.write_bits(0, 1); // red_blue_not_swapped_flag
    bw.write_flag(s.premultiplied_alpha); // premultiplied_alpha_flag
    bw.write_flag(s.alpha_image_plane); // alpha_image_plane_flag
    bw.write_bits(s.output_clr_fmt as u64, 4); // output_clr_fmt
    bw.write_bits(s.output_bitdepth as u64, 4); // output_bitdepth
    // (width-1), (height-1): u16 BE short / u32 BE long (byte-aligned here).
    if short {
        write_u16_be(bw, (s.width - 1) as u16);
        write_u16_be(bw, (s.height - 1) as u16);
    } else {
        for v in [s.width - 1, s.height - 1] {
            for sh in [24u32, 16, 8, 0] {
                bw.write_bits(((v >> sh) & 0xFF) as u64, 8);
            }
        }
    }
    if tiling {
        bw.write_bits(num_cols as u64 - 1, 12); // num_ver_tiles_minus1
        bw.write_bits(num_rows as u64 - 1, 12); // num_hor_tiles_minus1
    }
    // Tile size lists: all but the last entry, 8-bit short / 16-bit long.
    let size_bits = if short { 8 } else { 16 };
    if num_cols > 1 {
        for &wmb in &s.tile_cols_mb[..num_cols - 1] {
            bw.write_bits(wmb as u64, size_bits);
        }
    }
    if num_rows > 1 {
        for &hmb in &s.tile_rows_mb[..num_rows - 1] {
            bw.write_bits(hmb as u64, size_bits);
        }
    }
    if windowing {
        for m in [s.margins.0, s.margins.1, s.margins.2, s.margins.3] {
            debug_assert!(m < 64, "window margins are 6-bit fields");
            bw.write_bits(m as u64, 6);
        }
    }
}

/// The 4-bit `trim_flexbits` value of the spatial tile's flex plane header
/// (read by `flex_tile_plane_header` when the image-header flag is set;
/// primary plane only — the alpha plane's flex header is skipped).
pub fn write_trim_flexbits(bw: &mut BitWriter, trim: u8) {
    bw.write_bits(trim as u64 & 0xF, 4);
}





/// The depth-conditional plane-header fields after the format-specific
/// block (`Decoder::image_plane_header` order): `shift_bits` for the deep
/// integer depths, `len_mantissa` + `exp_bias` for BD32F, nothing else.
pub fn write_depth_plane_fields(bw: &mut BitWriter, d: &super::convert::Depth) {
    match d.bitdepth {
        BD16 | BD16S | BD32S => bw.write_bits(d.shift_bits as u64, 8),
        BD32F => {
            bw.write_bits(d.len_mantissa as u64, 8);
            bw.write_bits((d.exp_bias as u8) as u64, 8); // two's-complement byte
        }
        _ => {}
    }
}

/// The general single-component (`INT_YONLY`) plane header: any
/// `bands_present` × `scaled_flag`, uniform QPs, any output depth. The LP/HP
/// QP blocks shrink with the band set exactly as
/// `Decoder::image_plane_header` reads them.
pub fn write_image_plane_header_gray_bands(
    bw: &mut BitWriter,
    bands: u8,
    dc_quant: u8,
    lp_quant: u8,
    hp_quant: u8,
    scaled: bool,
    depth: &super::convert::Depth,
) {
    bw.write_bits(INT_YONLY as u64, 3);
    bw.write_flag(scaled); // scaled_flag
    bw.write_bits(bands as u64, 4); // bands_present
    // INT_YONLY → no format-specific block.
    write_depth_plane_fields(bw, depth);
    bw.write_flag(true); // dc_image_plane_uniform
    bw.write_bits(dc_quant as u64, 8);
    if bands != DCONLY {
        bw.write_bits(0, 1); // reserved_i_bit
        bw.write_flag(true); // lp_image_plane_uniform
        bw.write_bits(lp_quant as u64, 8);
        if bands != NOHIGHPASS {
            bw.write_bits(0, 1); // reserved_j_bit
            bw.write_flag(true); // hp_image_plane_uniform
            bw.write_bits(hp_quant as u64, 8);
        }
    }
    bw.align_to_byte();
}



/// `image_plane_header` for the **color** (`INT_YUV444`, 3-component) plane with
/// **ALL_BANDS** (DC + LP + HP + flexbits) and uniform per-band quantizers shared
/// across components (`COMP_UNIFORM`). Mirrors `Decoder::image_plane_header` for
/// YUV444 + `QP::read`, including the **two** 4-bit reserved fields (8 bits) the
/// spec mandates for YUV_444 (the field whose absence-by-one-nibble was the
/// Track-6.0 decoder bug). `scaled_flag = 0` (matches grayscale; lossy still
/// spec-valid). Ends byte-aligned.
pub fn write_image_plane_header_color_allbands(
    bw: &mut BitWriter,
    dc_quant: u8,
    lp_quant: u8,
    hp_quant: u8,
) {
    write_image_plane_header_yuv(bw, INT_YUV444, ALL_BANDS, dc_quant, lp_quant, hp_quant);
}

/// `image_plane_header` for a 3-component YUV plane of any sampling
/// (`INT_YUV444`/`INT_YUV422`/`INT_YUV420`) and any `bands_present`, with
/// uniform per-band `COMP_UNIFORM` quantizers. Mirrors
/// `Decoder::image_plane_header` field-for-field. The format-specific block
/// after `bands_present` is 8 zero bits in every case: YUV444 = two 4-bit
/// reserved fields; YUV420 = reserved_e(1) + chroma_centering_x(3) +
/// reserved_g(1) + chroma_centering_y(3); YUV422 = reserved_e(1) +
/// centering_x(3) + reserved_h(4) — and we always declare centering 0/0
/// (co-sited with even luma, the only values libjxr writes and exactly what
/// the even-centered downsample filter produces). `scaled_flag = 0`. Ends
/// byte-aligned.
pub fn write_image_plane_header_yuv(
    bw: &mut BitWriter,
    int_fmt: u8,
    bands: u8,
    dc_quant: u8,
    lp_quant: u8,
    hp_quant: u8,
) {
    write_image_plane_header_yuv_scaled(bw, int_fmt, bands, dc_quant, lp_quant, hp_quant, false)
}

/// [`write_image_plane_header_yuv`] with the `scaled_flag` exposed (scaled
/// arithmetic: samples carry 3 extra fraction bits; chroma DC-LP is coded at
/// half amplitude; the decoder's output stage shifts back down).
#[allow(clippy::too_many_arguments)]
pub fn write_image_plane_header_yuv_scaled(
    bw: &mut BitWriter,
    int_fmt: u8,
    bands: u8,
    dc_quant: u8,
    lp_quant: u8,
    hp_quant: u8,
    scaled: bool,
) {
    let plan = super::quant::QpPlan::uniform(
        super::quant::QpSet { dc: dc_quant, lp: lp_quant, hp: hp_quant },
        None,
    );
    write_image_plane_header_yuv_plan(bw, int_fmt, bands, &plan, scaled, &super::convert::Depth::BD8);
}

/// [`write_image_plane_header_yuv_scaled`] over a full [`super::quant::QpPlan`]:
/// each band's `image_plane_uniform` flag is set iff the plan keeps that band
/// at one image-wide QP set (single tile entry, single set) — otherwise the
/// flag is 0 and every tile's `*_tile_plane_header` carries the band's sets
/// ([`emit_codestream`]'s `tile_headers` hook).
pub fn write_image_plane_header_yuv_plan(
    bw: &mut BitWriter,
    int_fmt: u8,
    bands: u8,
    plan: &super::quant::QpPlan,
    scaled: bool,
    depth: &super::convert::Depth,
) {
    debug_assert!(matches!(int_fmt, INT_YUV444 | INT_YUV422 | INT_YUV420));
    let dc_uniform = plan.tiles.len() == 1;
    let lp_uniform = dc_uniform && plan.num_lp_qps() == 1;
    let hp_uniform = dc_uniform && plan.num_hp_qps() == 1;
    bw.write_bits(int_fmt as u64, 3); // internal_clr_fmt
    bw.write_flag(scaled); // scaled_flag
    bw.write_bits(bands as u64, 4); // bands_present
    // Format-specific reserved/centering block — 8 zero bits for all three.
    bw.write_bits(0, 8);
    write_depth_plane_fields(bw, depth);
    bw.write_flag(dc_uniform); // dc_image_plane_uniform
    if dc_uniform {
        write_band_qp(bw, &[plan.tiles[0].dc], 3);
    }
    if bands != DCONLY {
        bw.write_bits(0, 1); // reserved_i_bit
        bw.write_flag(lp_uniform); // lp_image_plane_uniform
        if lp_uniform {
            write_band_qp(bw, &plan.tiles[0].lp, 3);
        }
        if bands != NOHIGHPASS {
            bw.write_bits(0, 1); // reserved_j_bit
            bw.write_flag(hp_uniform); // hp_image_plane_uniform
            if hp_uniform {
                write_band_qp(bw, &plan.tiles[0].hp, 3);
            }
        }
    }
    bw.align_to_byte();
}

/// `image_plane_header` for a multi-component per-channel plane
/// (`INT_YUVK`, 4 components; `INT_NCOMPONENT`, 3–16): any `bands_present` ×
/// `scaled_flag` × depth, uniform QPs with `COMP_UNIFORM`/`COMP_SEPARATE`
/// emission (component 0 = `qp`, all others = `chroma_qp` — the only
/// component shapes this writer emits; `COMP_INDEPENDENT` for > 3
/// components would need per-component bytes the public surface doesn't
/// carry). Field order mirrors `Decoder::image_plane_header`: the
/// NCOMPONENT format-specific block is 4-bit `nc − 1` + 4 reserved bits
/// (or the 15 ⊕ 12-bit extension at exactly 16); YUVK has none.
#[allow(clippy::too_many_arguments)]
pub fn write_image_plane_header_multi(
    bw: &mut BitWriter,
    int_fmt: u8,
    nc: usize,
    bands: u8,
    qp: super::quant::QpSet,
    chroma_qp: super::quant::QpSet,
    scaled: bool,
    depth: &super::convert::Depth,
) {
    debug_assert!(matches!(int_fmt, INT_YUVK | INT_NCOMPONENT));
    debug_assert!(if int_fmt == INT_YUVK { nc == 4 } else { (3..=16).contains(&nc) });
    bw.write_bits(int_fmt as u64, 3); // internal_clr_fmt
    bw.write_flag(scaled); // scaled_flag
    bw.write_bits(bands as u64, 4); // bands_present
    if int_fmt == INT_NCOMPONENT {
        if nc < 16 {
            bw.write_bits(nc as u64 - 1, 4);
            bw.write_bits(0, 4); // reserved_h
        } else {
            bw.write_bits(15, 4);
            bw.write_bits(0, 12); // num_components − 16
        }
    }
    write_depth_plane_fields(bw, depth);
    let set = |lum: u8, chr: u8| super::quant::BandQp::separate(lum, chr);
    bw.write_flag(true); // dc_image_plane_uniform
    write_band_qp(bw, &[set(qp.dc, chroma_qp.dc)], nc);
    if bands != DCONLY {
        bw.write_bits(0, 1); // reserved_i_bit
        bw.write_flag(true); // lp_image_plane_uniform
        write_band_qp(bw, &[set(qp.lp, chroma_qp.lp)], nc);
        if bands != NOHIGHPASS {
            bw.write_bits(0, 1); // reserved_j_bit
            bw.write_flag(true); // hp_image_plane_uniform
            write_band_qp(bw, &[set(qp.hp, chroma_qp.hp)], nc);
        }
    }
    bw.align_to_byte();
}


/// One band's QP sets, general form — mirrors `QP::read(nc, num_qps, …)`:
/// per set, a 2-bit `component_mode` (when `nc > 1`) derived from the byte
/// pattern, then the mode's QP bytes. Single-component planes carry bare
/// bytes (no mode bits).
pub fn write_band_qp(bw: &mut BitWriter, sets: &[super::quant::BandQp], nc: usize) {
    for q in sets {
        let [y, u, v] = q.0;
        if nc == 1 {
            bw.write_bits(y as u64, 8);
            continue;
        }
        if y == u && u == v {
            bw.write_bits(COMP_UNIFORM as u64, 2);
            bw.write_bits(y as u64, 8);
        } else if u == v {
            bw.write_bits(COMP_SEPARATE as u64, 2);
            bw.write_bits(y as u64, 8);
            bw.write_bits(u as u64, 8);
        } else {
            bw.write_bits(COMP_INDEPENDENT as u64, 2);
            for b in [y, u, v] {
                bw.write_bits(b as u64, 8);
            }
        }
    }
}

/// Per-MB QP-set index (mirrors `Decoder::decode_qp_index`): index 0 is a
/// single 0 bit; index `v > 0` is a 1 bit + `v − 1` in the table's width
/// for `num_qp`.
pub fn write_qp_index(bw: &mut BitWriter, idx: usize, num_qp: usize) {
    const BITS: [u32; 17] = [0, 0, 1, 1, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4];
    debug_assert!(idx < num_qp);
    if idx == 0 {
        bw.write_bits(0, 1);
    } else {
        bw.write_bits(1, 1);
        bw.write_bits((idx - 1) as u64, BITS[num_qp.min(16)]);
    }
}

/// `vlw_esc` variable-length value: 2-byte form below `0xfb00`, the `0xfb`
/// 32-bit escape up to `u32::MAX`, the `0xfc` 64-bit escape beyond. Mirrors
/// `Decoder::vlw_esc`.
pub fn write_vlw_esc(bw: &mut BitWriter, value: u64) {
    if value < 0xfb * 256 {
        bw.write_bits((value >> 8) & 0xff, 8);
        bw.write_bits(value & 0xff, 8);
    } else if value <= u32::MAX as u64 {
        bw.write_bits(0xfb, 8);
        bw.write_bits(value, 32);
    } else {
        bw.write_bits(0xfc, 8);
        bw.write_bits(value, 64);
    }
}

/// Where one macroblock's band sections are written: a single writer
/// (SPATIAL order — bands interleaved per MB inside one tile packet) or one
/// writer per band (FREQUENCY order — the same sections routed into per-band
/// tile packets; T.832 coded_tiles reads each band packet with the identical
/// per-MB section sequence, so routing is the only difference).
pub enum Sink<'a> {
    Spatial(&'a mut BitWriter),
    Frequency {
        dc: &'a mut BitWriter,
        lp: &'a mut BitWriter,
        hp: &'a mut BitWriter,
        flex: &'a mut BitWriter,
    },
}

impl Sink<'_> {
    pub fn dc(&mut self) -> &mut BitWriter {
        match self {
            Sink::Spatial(w) => w,
            Sink::Frequency { dc, .. } => dc,
        }
    }
    pub fn lp(&mut self) -> &mut BitWriter {
        match self {
            Sink::Spatial(w) => w,
            Sink::Frequency { lp, .. } => lp,
        }
    }
    pub fn hp(&mut self) -> &mut BitWriter {
        match self {
            Sink::Spatial(w) => w,
            Sink::Frequency { hp, .. } => hp,
        }
    }
    pub fn flex(&mut self) -> &mut BitWriter {
        match self {
            Sink::Spatial(w) => w,
            Sink::Frequency { flex, .. } => flex,
        }
    }
}

/// One encodable image plane from the tile driver's point of view: reset the
/// per-tile entropy/prediction state ([`Self::begin_tile`] — the decoder's
/// `initialize_context`), then emit MBs at GLOBAL grid coordinates. The
/// composite (primary + alpha) implementation interleaves both planes per MB,
/// exactly as `Decoder::coded_tiles` reads them.
pub trait TileEncode {
    /// Start a tile whose first MB is `(first_mbx, first_mby)` and which is
    /// `tile_w` MBs wide: fresh entropy models / VLC tables / scans, and
    /// tile-relative edge & adapt cadence from here on.
    fn begin_tile(&mut self, first_mbx: usize, first_mby: usize, tile_w: usize);
    /// Emit one MB (all planes' sections, interleaved) at global `(mbx, mby)`,
    /// each band section routed through the [`Sink`].
    fn encode_mb_at(&mut self, sink: &mut Sink, mbx: usize, mby: usize);
}

/// Assemble a complete WMPHOTO codestream: image header (`spec`), plane
/// headers (`write_plane_headers`), index table when required, the
/// `subsequent_bytes` field (0), then the byte-aligned tile packets in the
/// decoder's row-major tile order. SPATIAL = one packet per tile
/// (`common_tile_header` + the 4-bit `trim_flexbits` flex header value when
/// set + the tile's MBs, bands interleaved per MB). FREQUENCY
/// (`spec.frequency_mode`) = `num_bands` packets per tile in DC/LP/HP/FLEX
/// order, each with its own `common_tile_header` (the trim value sits in the
/// FLEX packet — its flex tile plane header), the same MB raster per packet.
///
/// Single tile spatial reproduces the classic byte layout exactly (no index
/// table, same alignment points).
pub fn emit_codestream(
    spec: &ImageHeaderSpec,
    write_plane_headers: impl FnOnce(&mut BitWriter),
    tile_headers: &dyn Fn(&mut BitWriter, usize, usize),
    num_bands: usize,
    mbw: usize,
    mbh: usize,
    plane: &mut dyn TileEncode,
) -> Vec<u8> {
    let mut head = BitWriter::new();
    write_image_header_spec(&mut head, spec);
    write_plane_headers(&mut head);

    let cols: Vec<usize> =
        if spec.tile_cols_mb.is_empty() { vec![mbw] } else { spec.tile_cols_mb.clone() };
    let rows: Vec<usize> =
        if spec.tile_rows_mb.is_empty() { vec![mbh] } else { spec.tile_rows_mb.clone() };
    debug_assert_eq!(cols.iter().sum::<usize>(), mbw, "tile columns must cover the MB grid");
    debug_assert_eq!(rows.iter().sum::<usize>(), mbh, "tile rows must cover the MB grid");

    // Tile packets, each in its own writer so it starts byte-aligned and its
    // byte offset is known for the index table without back-patching.
    let mut packets: Vec<Vec<u8>> = Vec::with_capacity(cols.len() * rows.len());
    let mut top = 0usize;
    let mut tile_idx = 0usize;
    for &th in &rows {
        let mut left = 0usize;
        for &tw_mb in &cols {
            plane.begin_tile(left, top, tw_mb);
            if !spec.frequency_mode {
                let mut tw = BitWriter::new();
                write_common_tile_header(&mut tw);
                tile_headers(&mut tw, tile_idx, SPATIAL_BAND);
                let mut sink = Sink::Spatial(&mut tw);
                for mby in top..top + th {
                    for mbx in left..left + tw_mb {
                        plane.encode_mb_at(&mut sink, mbx, mby);
                    }
                }
                tw.align_to_byte();
                packets.push(tw.finish());
            } else {
                // One writer per band; each PRESENT packet opens with its own
                // common_tile_header. Absent trailing bands receive no bits
                // (the per-MB sections are gated by `bands_present`) and
                // their writers are simply not pushed. The decoder reads this
                // tile's packets consecutively in DC/LP/HP/FLEX order (its
                // `coded_tiles` band loop), so they concatenate in that order.
                let [mut dcw, mut lpw, mut hpw, mut fxw] =
                    [BitWriter::new(), BitWriter::new(), BitWriter::new(), BitWriter::new()];
                for (b, w) in [&mut dcw, &mut lpw, &mut hpw, &mut fxw].into_iter().enumerate() {
                    if b < num_bands {
                        write_common_tile_header(w);
                        tile_headers(w, tile_idx, b);
                    }
                }
                {
                    let mut sink = Sink::Frequency {
                        dc: &mut dcw,
                        lp: &mut lpw,
                        hp: &mut hpw,
                        flex: &mut fxw,
                    };
                    for mby in top..top + th {
                        for mbx in left..left + tw_mb {
                            plane.encode_mb_at(&mut sink, mbx, mby);
                        }
                    }
                }
                for (b, mut w) in [dcw, lpw, hpw, fxw].into_iter().enumerate() {
                    if b < num_bands {
                        w.align_to_byte();
                        packets.push(w.finish());
                    } else {
                        debug_assert!(w.finish().is_empty(), "absent band must stay silent");
                    }
                }
            }
            left += tw_mb;
            tile_idx += 1;
        }
        top += th;
    }

    if spec.index_table_present() {
        head.write_bits(1, 16); // index_table_startcode
        let mut off = 0u64;
        for p in &packets {
            write_vlw_esc(&mut head, off); // offset from the first tile's start
            off += p.len() as u64;
        }
    }
    write_vlw_esc(&mut head, 0); // subsequent_bytes (no profile/level info)

    let mut out = head.finish();
    for p in packets {
        out.extend_from_slice(&p);
    }
    out
}

/// The `band` value [`emit_codestream`]'s `tile_headers` hook receives for a
/// SPATIAL tile packet (all bands' tile-header fields in flex→DC→LP→HP
/// order); frequency packets receive their band index 0–3.
pub const SPATIAL_BAND: usize = 4;

/// The classic `tile_headers` hook: uniform QPs (no per-tile fields), just
/// the 4-bit `trim_flexbits` value in the flex header when set.
pub fn classic_tile_headers(trim: u8) -> impl Fn(&mut BitWriter, usize, usize) {
    move |w, _tile, band| {
        if (band == SPATIAL_BAND || band == 3) && trim != 0 {
            write_trim_flexbits(w, trim);
        }
    }
}

/// Bands carried by a `bands_present` code (= frequency packets per tile).
pub fn band_count(bands: u8) -> usize {
    match bands {
        ALL_BANDS => 4,
        NOFLEXBITS => 3,
        NOHIGHPASS => 2,
        _ => 1,
    }
}

/// `common_tile_header`: 24-bit start code (==1) + one arbitrary byte. Mirrors
/// `Decoder::common_tile_header`.
pub fn write_common_tile_header(bw: &mut BitWriter) {
    bw.write_bits(1, 24); // tile_startcode
    bw.write_bits(0, 8); // arbitrary_byte
}

fn write_u16_be(bw: &mut BitWriter, v: u16) {
    bw.write_bits((v >> 8) as u64 & 0xff, 8);
    bw.write_bits(v as u64 & 0xff, 8);
}
