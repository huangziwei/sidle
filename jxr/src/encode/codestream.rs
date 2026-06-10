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
    let short = width <= 1 << 16 && height <= 1 << 16;
    for &b in b"WMPHOTO\x00" {
        bw.write_bits(b as u64, 8);
    }
    bw.write_bits(1, 4); // codec_version (==1)
    bw.write_bits(0, 1); // hard_tiling_flag
    bw.write_bits(1, 3); // codec_subversion (==1)
    bw.write_bits(0, 1); // tiling_flag → single tile
    bw.write_bits(0, 1); // frequency_mode → spatial
    bw.write_bits(0, 3); // spatial_xfrm_subordinate
    bw.write_bits(0, 1); // index_table_present_flag → none
    bw.write_bits(NO_OVERLAP_FILTERING as u64, 2); // overlap_mode
    bw.write_flag(short); // short_header_flag (16- vs 32-bit dims)
    bw.write_bits(1, 1); // long_word_flag (Amazon sets this; decoder ignores it)
    bw.write_bits(0, 1); // windowing_flag
    bw.write_flag(trim_flexbits != 0); // trim_flexbits_flag
    bw.write_bits(0, 1); // reserved_d
    bw.write_bits(0, 1); // red_blue_not_swapped_flag
    bw.write_flag(premultiplied_alpha); // premultiplied_alpha_flag
    bw.write_flag(alpha_image_plane); // alpha_image_plane_flag
    bw.write_bits(output_clr_fmt as u64, 4); // output_clr_fmt
    bw.write_bits(BD8 as u64, 4); // output_bitdepth
    // (width-1), (height-1): u16 BE short / u32 BE long (byte-aligned here).
    if short {
        write_u16_be(bw, (width - 1) as u16);
        write_u16_be(bw, (height - 1) as u16);
    } else {
        for v in [width - 1, height - 1] {
            for sh in [24u32, 16, 8, 0] {
                bw.write_bits(((v >> sh) & 0xFF) as u64, 8);
            }
        }
    }
    // tiling_flag=0 → no tile dims; windowing_flag=0 → decoder derives padding.
}

/// The 4-bit `trim_flexbits` value of the spatial tile's flex plane header
/// (read by `flex_tile_plane_header` when the image-header flag is set;
/// primary plane only — the alpha plane's flex header is skipped).
pub fn write_trim_flexbits(bw: &mut BitWriter, trim: u8) {
    bw.write_bits(trim as u64 & 0xF, 4);
}

/// `image_plane_header` for the single grayscale plane, DCONLY, with a uniform
/// DC quantizer byte `dc_quant` (0 ⇒ scaling factor 1, i.e. no DC quantization).
/// Mirrors `Decoder::image_plane_header`; ends byte-aligned
/// (`discard_remainder_bits`).
pub fn write_image_plane_header_gray_dconly(bw: &mut BitWriter, dc_quant: u8) {
    bw.write_bits(INT_YONLY as u64, 3); // internal_clr_fmt
    bw.write_bits(0, 1); // scaled_flag
    bw.write_bits(DCONLY as u64, 4); // bands_present
    // INT_YONLY → num_components = 1, no extra format bits.
    // BD8 → no shift bits.
    bw.write_flag(true); // dc_image_plane_uniform
    // QP::read(num_components=1, num_qps=1): num_components==1 ⇒ no
    // component_mode bits; COMP_UNIFORM ⇒ one 8-bit quant value.
    bw.write_bits(dc_quant as u64, 8);
    // bands_present == DCONLY ⇒ no LP/HP reserved bits or QP.
    bw.align_to_byte();
}

/// `image_plane_header` for the single grayscale plane with **NOHIGHPASS**
/// (DC + LP), uniform DC and LP quantizers. Mirrors `Decoder::image_plane_header`.
pub fn write_image_plane_header_gray_nohighpass(bw: &mut BitWriter, dc_quant: u8, lp_quant: u8) {
    bw.write_bits(INT_YONLY as u64, 3); // internal_clr_fmt
    bw.write_bits(0, 1); // scaled_flag
    bw.write_bits(NOHIGHPASS as u64, 4); // bands_present
    bw.write_flag(true); // dc_image_plane_uniform
    bw.write_bits(dc_quant as u64, 8); // DC QP
    // bands_present != DCONLY:
    bw.write_bits(0, 1); // reserved_i_bit
    bw.write_flag(true); // lp_image_plane_uniform
    bw.write_bits(lp_quant as u64, 8); // LP QP
    // bands_present == NOHIGHPASS ⇒ no HP block.
    bw.align_to_byte();
}

/// `image_plane_header` for a single-component (`INT_YONLY`) plane with
/// **ALL_BANDS** (DC + LP + HP + flexbits), uniform DC/LP/HP quantizers.
/// Mirrors `Decoder::image_plane_header`. Serves both the grayscale primary
/// plane and the **alpha image plane** — the spec's plane-header syntax is
/// plane-role-agnostic, and an alpha plane must be YONLY (the decoder rejects
/// anything else), so the alpha header is this writer with the alpha QPs.
pub fn write_image_plane_header_gray_allbands(
    bw: &mut BitWriter,
    dc_quant: u8,
    lp_quant: u8,
    hp_quant: u8,
) {
    write_image_plane_header_gray_scaled(bw, dc_quant, lp_quant, hp_quant, false)
}

/// [`write_image_plane_header_gray_allbands`] with the `scaled_flag` exposed.
pub fn write_image_plane_header_gray_scaled(
    bw: &mut BitWriter,
    dc_quant: u8,
    lp_quant: u8,
    hp_quant: u8,
    scaled: bool,
) {
    write_image_plane_header_gray_bands(bw, ALL_BANDS, dc_quant, lp_quant, hp_quant, scaled)
}

/// The general single-component (`INT_YONLY`) plane header: any
/// `bands_present` × `scaled_flag`, uniform QPs. The LP/HP QP blocks shrink
/// with the band set exactly as `Decoder::image_plane_header` reads them.
pub fn write_image_plane_header_gray_bands(
    bw: &mut BitWriter,
    bands: u8,
    dc_quant: u8,
    lp_quant: u8,
    hp_quant: u8,
    scaled: bool,
) {
    bw.write_bits(INT_YONLY as u64, 3);
    bw.write_flag(scaled); // scaled_flag
    bw.write_bits(bands as u64, 4); // bands_present
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

/// `image_plane_header` for the **color** (`INT_YUV444`) plane, **DCONLY** —
/// the staging foundation for the color path (constant-color MBs round-trip
/// exactly). Uniform DC QP shared across components. Ends byte-aligned.
pub fn write_image_plane_header_color_dconly(bw: &mut BitWriter, dc_quant: u8) {
    bw.write_bits(INT_YUV444 as u64, 3); // internal_clr_fmt
    bw.write_bits(0, 1); // scaled_flag
    bw.write_bits(DCONLY as u64, 4); // bands_present
    bw.write_bits(0, 8); // YUV_444 reserved_e_bit (4) + reserved_f (4)
    bw.write_flag(true); // dc_image_plane_uniform
    write_uniform_qp(bw, dc_quant);
    // bands_present == DCONLY ⇒ no LP/HP block.
    bw.align_to_byte();
}

/// `image_plane_header` for the **color** (`INT_YUV444`) plane, **NOHIGHPASS**
/// (DC + LP) — staging step between DCONLY and ALL_BANDS. Uniform DC + LP QPs.
pub fn write_image_plane_header_color_nohighpass(bw: &mut BitWriter, dc_quant: u8, lp_quant: u8) {
    bw.write_bits(INT_YUV444 as u64, 3); // internal_clr_fmt
    bw.write_bits(0, 1); // scaled_flag
    bw.write_bits(NOHIGHPASS as u64, 4); // bands_present
    bw.write_bits(0, 8); // YUV_444 reserved_e_bit (4) + reserved_f (4)
    bw.write_flag(true); // dc_image_plane_uniform
    write_uniform_qp(bw, dc_quant);
    bw.write_bits(0, 1); // reserved_i_bit / use-DC-QP-for-LP = 0 (don't reuse)
    bw.write_flag(true); // lp_image_plane_uniform
    write_uniform_qp(bw, lp_quant);
    // bands_present == NOHIGHPASS ⇒ no HP block.
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
    debug_assert!(matches!(int_fmt, INT_YUV444 | INT_YUV422 | INT_YUV420));
    bw.write_bits(int_fmt as u64, 3); // internal_clr_fmt
    bw.write_flag(scaled); // scaled_flag
    bw.write_bits(bands as u64, 4); // bands_present
    // Format-specific reserved/centering block — 8 zero bits for all three.
    bw.write_bits(0, 8);
    // BD8 → no shift/mantissa bits.
    bw.write_flag(true); // dc_image_plane_uniform
    write_uniform_qp(bw, dc_quant);
    if bands != DCONLY {
        bw.write_bits(0, 1); // reserved_i_bit / use-DC-QP-for-LP = 0
        bw.write_flag(true); // lp_image_plane_uniform
        write_uniform_qp(bw, lp_quant);
        if bands != NOHIGHPASS {
            bw.write_bits(0, 1); // reserved_j_bit / use-LP-QP-for-HP = 0
            bw.write_flag(true); // hp_image_plane_uniform
            write_uniform_qp(bw, hp_quant);
        }
    }
    bw.align_to_byte();
}

/// One band's QP for a multi-component plane in `COMP_UNIFORM` mode: a 2-bit
/// `component_mode` (0) then a single 8-bit quant index applied to every
/// component. Mirrors `QP::read` with `num_components > 1`.
fn write_uniform_qp(bw: &mut BitWriter, quant: u8) {
    bw.write_bits(COMP_UNIFORM as u64, 2);
    bw.write_bits(quant as u64, 8);
}

/// `vlw_esc` variable-length value. We only need small values; `< 0xfb` uses
/// the 2-byte form. Mirrors `Decoder::vlw_esc`.
pub fn write_vlw_esc(bw: &mut BitWriter, value: u64) {
    assert!(value < 0xfb * 256, "vlw_esc large form not implemented");
    bw.write_bits((value >> 8) & 0xff, 8);
    bw.write_bits(value & 0xff, 8);
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
