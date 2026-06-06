//! WMPHOTO codestream framing writer (encode side) for the minimal grayscale
//! path: spatial mode, single tile, no overlap, no windowing, **DCONLY**,
//! `8bppGray`, uniform DC QP.
//!
//! Each writer mirrors the corresponding decoder reader in
//! `jxr_decode::decoder` field-for-field, so the decoder parses exactly what we
//! emit. The DC *value* coding (`mb_dc`) lives in [`super::coeff`]; this module
//! is the surrounding header/tile frame.

use super::bitstream::BitWriter;
use crate::image::jxr_decode::consts::*;

/// `image_header` for a `width`×`height` image with the given `output_clr_fmt`
/// (`OUT_YONLY` for grayscale, `OUT_RGB` for color). Mirrors
/// `Decoder::image_header`. `width`/`height` are the true (unpadded) dims;
/// `windowing_flag = 0` makes the decoder derive the 16-aligned padding and crop
/// back to these.
pub fn write_image_header(bw: &mut BitWriter, width: u32, height: u32, output_clr_fmt: u8) {
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
    bw.write_bits(1, 1); // short_header_flag → 16-bit dims
    bw.write_bits(1, 1); // long_word_flag (Amazon sets this; decoder ignores it)
    bw.write_bits(0, 1); // windowing_flag
    bw.write_bits(0, 1); // trim_flexbits_flag
    bw.write_bits(0, 1); // reserved_d
    bw.write_bits(0, 1); // red_blue_not_swapped_flag
    bw.write_bits(0, 1); // premultiplied_alpha_flag
    bw.write_bits(0, 1); // alpha_image_plane_flag
    bw.write_bits(output_clr_fmt as u64, 4); // output_clr_fmt
    bw.write_bits(BD8 as u64, 4); // output_bitdepth
    // short header: (width-1), (height-1) as big-endian u16 (byte-aligned here).
    write_u16_be(bw, (width - 1) as u16);
    write_u16_be(bw, (height - 1) as u16);
    // tiling_flag=0 → no tile dims; windowing_flag=0 → decoder derives padding.
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

/// `image_plane_header` for the single grayscale plane with **ALL_BANDS**
/// (DC + LP + HP + flexbits), uniform DC/LP/HP quantizers. Mirrors
/// `Decoder::image_plane_header`.
pub fn write_image_plane_header_gray_allbands(
    bw: &mut BitWriter,
    dc_quant: u8,
    lp_quant: u8,
    hp_quant: u8,
) {
    bw.write_bits(INT_YONLY as u64, 3);
    bw.write_bits(0, 1); // scaled_flag
    bw.write_bits(ALL_BANDS as u64, 4); // bands_present (0)
    bw.write_flag(true); // dc_image_plane_uniform
    bw.write_bits(dc_quant as u64, 8);
    bw.write_bits(0, 1); // reserved_i_bit
    bw.write_flag(true); // lp_image_plane_uniform
    bw.write_bits(lp_quant as u64, 8);
    bw.write_bits(0, 1); // reserved_j_bit
    bw.write_flag(true); // hp_image_plane_uniform
    bw.write_bits(hp_quant as u64, 8);
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
    bw.write_bits(INT_YUV444 as u64, 3); // internal_clr_fmt
    bw.write_bits(0, 1); // scaled_flag
    bw.write_bits(ALL_BANDS as u64, 4); // bands_present (0)
    // YUV_444: two 4-bit reserved fields (reserved_e_bit + reserved_f) = 8 bits.
    bw.write_bits(0, 8);
    // BD8 → no shift/mantissa bits.
    // DC: uniform, one quant shared by all 3 components (COMP_UNIFORM).
    bw.write_flag(true); // dc_image_plane_uniform
    write_uniform_qp(bw, dc_quant);
    // LP: "don't reuse DC QP" (0) → its own uniform QP.
    bw.write_bits(0, 1); // reserved_i_bit / use-DC-QP-for-LP = 0 (don't reuse)
    bw.write_flag(true); // lp_image_plane_uniform
    write_uniform_qp(bw, lp_quant);
    // HP: "don't reuse LP QP" (0) → its own uniform QP.
    bw.write_bits(0, 1); // reserved_j_bit / use-LP-QP-for-HP = 0 (don't reuse)
    bw.write_flag(true); // hp_image_plane_uniform
    write_uniform_qp(bw, hp_quant);
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
