//! Decoder state types ported from `jxr_image.py`.

#![allow(non_snake_case)]
// See decoder.rs: JPEG-XR spec port — explicit index loops over parallel state
// arrays and the wide constructor parameter list are intentional.
#![allow(clippy::needless_range_loop, clippy::too_many_arguments)]

use super::consts::*;
use super::math::clip;
use super::misc::{Deserializer, DeserializerError};

/// One adaptive VLC selector. Two flavours of table dynamics: `VLCTable1`
/// (single discriminator) and `VLCTable2` (two discriminators + delta-table
/// pointers). The flavor is implied by which `Initialize*` was called.
#[derive(Default, Clone, Debug)]
pub struct AdaptiveVLC {
    pub table_index: u32,
    pub delta_table_index: u32,
    pub delta2_table_index: u32,
    pub discrim_val1: i32,
    pub discrim_val2: i32,
}

impl AdaptiveVLC {
    pub fn init_table1(&mut self) {
        self.table_index = 0;
        self.delta_table_index = 0;
        self.discrim_val1 = 0;
    }

    pub fn adapt_table1(&mut self) {
        let max_table_index: u32 = 1;
        let lower: i32 = -8;
        let upper: i32 = 8;
        if self.discrim_val1 < lower && self.table_index != 0 {
            self.table_index -= 1;
            self.discrim_val1 = 0;
        } else if self.discrim_val1 > upper && self.table_index != max_table_index {
            self.table_index += 1;
            self.discrim_val1 = 0;
        } else {
            self.discrim_val1 = clip(self.discrim_val1, -64, 64);
        }
    }

    pub fn init_table2(&mut self) {
        self.delta_table_index = 0;
        self.discrim_val1 = 0;
        self.discrim_val2 = 0;
        self.table_index = 1;
        self.delta2_table_index = 1;
    }

    pub fn adapt_table2(&mut self, max_table_index: u32) {
        let mut changed = false;
        let lower: i32 = -8;
        let upper: i32 = 8;
        if self.discrim_val1 < lower && self.table_index != 0 {
            self.table_index -= 1;
            changed = true;
        } else if self.discrim_val2 > upper && self.table_index != max_table_index {
            self.table_index += 1;
            changed = true;
        }
        if changed {
            self.discrim_val1 = 0;
            self.discrim_val2 = 0;
            if self.table_index == max_table_index {
                self.delta_table_index = self.table_index - 1;
                self.delta2_table_index = self.table_index - 1;
            } else if self.table_index == 0 {
                self.delta_table_index = self.table_index;
                self.delta2_table_index = self.table_index;
            } else {
                self.delta_table_index = self.table_index - 1;
                self.delta2_table_index = self.table_index;
            }
        } else {
            self.discrim_val1 = clip(self.discrim_val1, -64, 64);
            self.discrim_val2 = clip(self.discrim_val2, -64, 64);
        }
    }
}

#[derive(Default, Clone, Debug)]
pub struct Model {
    pub m_state: [i32; 2],
    pub m_bits: [i32; 2],
}

impl Model {
    pub fn initialize_model_mb(&mut self, i_band: u8) {
        let bits = ((2 - i_band as i32) * 4).max(0);
        self.m_state = [0, 0];
        self.m_bits = [bits, bits];
    }
}

#[derive(Default, Clone, Debug)]
pub struct CBPHPModel {
    pub cbphp_state: [i32; 2],
    pub count_ones: [i32; 2],
    pub count_zeroes: [i32; 2],
}

#[derive(Clone, Debug)]
pub struct AdaptiveScan {
    pub order: Vec<usize>,
    pub totals: Vec<i32>,
}

impl AdaptiveScan {
    pub fn new(order: &[usize]) -> Self {
        let mut s = Self {
            order: order.to_vec(),
            totals: vec![0; order.len()],
        };
        s.reset_totals();
        s
    }

    pub fn reset_totals(&mut self) {
        // Python: [None, 32, 30, 28, 26, 24, 22, 20, 18, 16, 14, 12, 10, 8, 6, 4]
        let scan_totals: [i32; 16] = [0, 32, 30, 28, 26, 24, 22, 20, 18, 16, 14, 12, 10, 8, 6, 4];
        for (i, v) in scan_totals.iter().enumerate() {
            if i < self.totals.len() {
                self.totals[i] = *v;
            }
        }
    }

    pub fn translate(&self, i: usize) -> usize {
        self.order[i]
    }

    pub fn adapt(&mut self, i: usize) {
        self.totals[i] += 1;
        if i > 1 && self.totals[i] > self.totals[i - 1] {
            self.order.swap(i, i - 1);
            self.totals.swap(i, i - 1);
        }
    }
}

/// One quantization-parameter set. Each `QP` instance covers one band of
/// one plane and stores per-component, per-`NumQPs` scaling factors.
#[derive(Clone, Debug)]
pub struct QP {
    pub num_qps: usize,
    pub index_qps: usize,
    /// `[NumComponents][NumQPs]` of scaling factors.
    pub quant_scaling_factor: Vec<Vec<i32>>,
}

impl QP {
    pub fn read(
        ds: &mut Deserializer<'_>,
        num_components: usize,
        num_qps: usize,
        scaled_flag: u8,
        band: u8,
    ) -> Result<Self, DeserializerError> {
        let scaled_flag = scaled_flag != 0;
        let mut qsf: Vec<Vec<i32>> = (0..num_components).map(|_| vec![1; num_qps]).collect();

        for j in 0..num_qps {
            let component_mode = if num_components != 1 {
                ds.check_bit_field(2, "component_mode", &[0, 1, 2])? as u8
            } else {
                COMP_UNIFORM
            };
            match component_mode {
                COMP_UNIFORM => {
                    let quant = ds.unpack_bits(8)? as u32;
                    for i in 0..num_components {
                        qsf[i][j] = quant_map(quant, i, scaled_flag, band)?;
                    }
                }
                COMP_SEPARATE => {
                    let q_luma = ds.unpack_bits(8)? as u32;
                    qsf[0][j] = quant_map(q_luma, 0, scaled_flag, band)?;
                    let q_chroma = ds.unpack_bits(8)? as u32;
                    for i in 1..num_components {
                        qsf[i][j] = quant_map(q_chroma, i, scaled_flag, band)?;
                    }
                }
                COMP_INDEPENDENT => {
                    for i in 0..num_components {
                        let q = ds.unpack_bits(8)? as u32;
                        qsf[i][j] = quant_map(q, i, scaled_flag, band)?;
                    }
                }
                _ => unreachable!("checked above"),
            }
        }

        Ok(Self {
            num_qps,
            index_qps: 0,
            quant_scaling_factor: qsf,
        })
    }

    pub fn scaling_factor(&self, i_component: usize) -> i32 {
        self.quant_scaling_factor[i_component][self.index_qps]
    }

    /// Scaling factor at an explicit QP-set index (per-MB DQUANT lookups).
    pub fn scaling_factor_at(&self, i_component: usize, index: usize) -> i32 {
        self.quant_scaling_factor[i_component][index]
    }
}

pub(crate) fn quant_map(
    i_qp: u32,
    i_component: usize,
    scaled_flag: bool,
    band: u8,
) -> Result<i32, DeserializerError> {
    let sf: i32;
    if i_qp == 0 {
        sf = 1;
    } else if !scaled_flag {
        let i_not_scaled_shift: i32 = -2;
        let i_man: i32;
        let i_exp: i32;
        if i_qp < 32 {
            i_man = ((i_qp + 3) >> 2) as i32;
            i_exp = 0;
        } else if i_qp < 48 {
            i_man = ((16 + (i_qp & 15) + 1) >> 1) as i32;
            i_exp = (i_qp >> 4) as i32 + i_not_scaled_shift;
        } else {
            i_man = (16 + (i_qp & 15)) as i32;
            i_exp = ((i_qp >> 4) as i32 - 1) + i_not_scaled_shift;
        }
        sf = shift_left_signed(i_man, i_exp);
    } else {
        let i_scaled_shift: i32 = if i_component > 0 && (band == DC || band == LP) {
            0
        } else {
            1
        };
        let i_man: i32;
        let i_exp: i32;
        if i_qp < 16 {
            i_man = i_qp as i32;
            i_exp = i_scaled_shift;
        } else {
            i_man = (16 + (i_qp & 15)) as i32;
            i_exp = ((i_qp >> 4) as i32 - 1) + i_scaled_shift;
        }
        sf = shift_left_signed(i_man, i_exp);
    }

    if sf < 1 {
        return Err(DeserializerError::Unsupported(format!(
            "QuantMap: ScalingFactor {sf}"
        )));
    }
    Ok(sf)
}

#[inline]
fn shift_left_signed(man: i32, exp: i32) -> i32 {
    if exp >= 0 {
        man.wrapping_shl(exp as u32)
    } else {
        man >> (-exp) as u32
    }
}

/// One macroblock of decoder state.
pub struct MB {
    // Port-parity position fields (jxr_image.py stores them on each MB;
    // the Rust pipeline threads positions as arguments instead).
    #[allow(dead_code)]
    pub mbx: usize,
    #[allow(dead_code)]
    pub mby: usize,
    #[allow(dead_code)]
    pub mbxt: usize,
    #[allow(dead_code)]
    pub mbyt: usize,

    pub left_mb: Option<(usize, usize)>,
    pub top_mb: Option<(usize, usize)>,
    pub top_left_mb: Option<(usize, usize)>,

    pub is_left_edge: bool,
    pub is_top_edge: bool,
    pub initialize_context: bool,
    pub reset_totals: bool,
    pub reset_context: bool,

    pub mb_dc_mode: u8,
    pub mb_lp_mode: u8,
    pub mb_hp_mode: u8,

    /// HPInputVLC flat across (component, block, pos): index
    /// `c * HP_INPUT_PER_COMP + b * 16 + p` (c ∈ 0..nc, b ∈ 0..16, p ∈ 0..16).
    pub hp_input_vlc: Vec<i32>,
    /// HPInputFlex with the same layout as `hp_input_vlc`.
    pub hp_input_flex: Vec<i32>,
    /// MbDCLP flat across (component, pos): index `c * MB_DCLP_PER_COMP + p`
    /// (c ∈ 0..nc, p ∈ 0..16).
    pub mb_dclp: Vec<i32>,
    /// `MBCBPHP[component]`
    pub mb_cbphp: Vec<i32>,
    /// ModelBitsMBHP[chroma 0..2]
    pub model_bits_mb_hp: [i32; 2],
    pub mb_qp_index_lp: usize,
    /// This MB's HP QP-set index (DQUANT). Stored per MB because in
    pub mb_qp_index_hp: usize,

    /// MBBuffer flat across components: `mb_buffer[c * MB_BUF_PER_COMP + pos]`
    pub mb_buffer: Vec<i32>,
}

/// Stride between adjacent components inside [`MB::mb_buffer`].
pub const MB_BUF_PER_COMP: usize = 256;
/// Stride between adjacent components inside [`MB::hp_input_vlc`] /
/// [`MB::hp_input_flex`] (16 blocks × 16 positions).
pub const HP_INPUT_PER_COMP: usize = 16 * 16;
/// Stride between adjacent components inside [`MB::mb_dclp`].
pub const MB_DCLP_PER_COMP: usize = 16;

impl MB {
    pub fn new(
        mbx: usize,
        mby: usize,
        mbxt: usize,
        mbyt: usize,
        tile_width_mb: usize,
        left_mb: Option<(usize, usize)>,
        top_mb: Option<(usize, usize)>,
        top_left_mb: Option<(usize, usize)>,
    ) -> Self {
        let is_left_edge = mbxt == 0;
        let is_top_edge = mbyt == 0;
        let initialize_context = is_left_edge && is_top_edge;
        let reset_totals = mbxt.is_multiple_of(16);
        let reset_context = reset_totals || mbxt == tile_width_mb - 1;

        Self {
            mbx,
            mby,
            mbxt,
            mbyt,
            left_mb,
            top_mb,
            top_left_mb,
            is_left_edge,
            is_top_edge,
            initialize_context,
            reset_totals,
            reset_context,
            mb_dc_mode: NO_PREDICTION,
            mb_lp_mode: NO_PREDICTION,
            mb_hp_mode: NO_PREDICTION,
            // Coefficient/sample buffers are allocated lazily by
            hp_input_vlc: Vec::new(),
            hp_input_flex: Vec::new(),
            mb_dclp: Vec::new(),
            mb_cbphp: Vec::new(),
            model_bits_mb_hp: [0, 0],
            mb_qp_index_lp: 0,
            mb_qp_index_hp: 0,
            mb_buffer: Vec::new(),
        }
    }

    /// Allocate this MB's coefficient/sample buffers (idempotent). Called by
    pub fn alloc_buffers(&mut self, num_components: usize) {
        if self.mb_buffer.is_empty() {
            self.hp_input_vlc = vec![0; num_components * HP_INPUT_PER_COMP];
            self.hp_input_flex = vec![0; num_components * HP_INPUT_PER_COMP];
            self.mb_dclp = vec![0; num_components * MB_DCLP_PER_COMP];
            self.mb_cbphp = vec![0; num_components];
            self.mb_buffer = vec![0; num_components * MB_BUF_PER_COMP];
        }
    }
}

/// One image plane (primary or alpha).
pub struct Plane {
    pub is_alpha: bool,

    // header fields
    pub internal_clr_fmt: u8,
    pub scaled_flag: u8,
    pub bands_present: u8,
    pub lp_present: bool,
    pub hp_present: bool,
    pub flexbits_present: bool,
    pub num_bands: usize,
    pub num_components: usize,
    pub chroma_per_blk: usize,
    pub num_lp_coeff: usize,
    #[allow(dead_code)]
    pub chroma_centering_x: u32,
    #[allow(dead_code)]
    pub chroma_centering_y: u32,
    pub shift_bits: u32,
    #[allow(dead_code)]
    pub len_mantissa: u32,
    #[allow(dead_code)]
    pub exp_bias: i32,

    pub dc_image_plane_uniform: bool,
    pub lp_image_plane_uniform: bool,
    pub hp_image_plane_uniform: bool,

    pub dc_qp: Option<QP>,
    pub lp_qp: Option<QP>,
    pub hp_qp: Option<QP>,
    /// True if lp_qp is currently aliased to dc_qp (Python `is` check).
    pub lp_qp_eq_dc: bool,
    /// True if hp_qp is currently aliased to lp_qp.
    pub hp_qp_eq_lp: bool,

    // DC band state
    pub model_dc: Model,
    pub abs_level_ind_dc_lum: AdaptiveVLC,
    pub abs_level_ind_dc_chr: AdaptiveVLC,

    // LP band state
    pub model_lp: Model,
    pub dec_first_ind_lp_lum: AdaptiveVLC,
    pub dec_ind_lp_lum0: AdaptiveVLC,
    pub dec_ind_lp_lum1: AdaptiveVLC,
    pub dec_first_ind_lp_chr: AdaptiveVLC,
    pub dec_ind_lp_chr0: AdaptiveVLC,
    pub dec_ind_lp_chr1: AdaptiveVLC,
    pub abs_level_ind_lp0: AdaptiveVLC,
    pub abs_level_ind_lp1: AdaptiveVLC,
    pub lowpass_scan: Option<AdaptiveScan>,
    pub count_zero_cbplp: i32,
    pub count_max_cbplp: i32,

    // HP band state
    pub model_hp: Model,
    pub dec_num_cbphp: AdaptiveVLC,
    pub dec_num_blk_cbphp: AdaptiveVLC,
    pub dec_first_ind_hp_lum: AdaptiveVLC,
    pub dec_ind_hp_lum0: AdaptiveVLC,
    pub dec_ind_hp_lum1: AdaptiveVLC,
    pub dec_first_ind_hp_chr: AdaptiveVLC,
    pub dec_ind_hp_chr0: AdaptiveVLC,
    pub dec_ind_hp_chr1: AdaptiveVLC,
    pub abs_level_ind_hp0: AdaptiveVLC,
    pub abs_level_ind_hp1: AdaptiveVLC,
    pub cbphp_model_hp: CBPHPModel,
    pub highpass_hor_scan: Option<AdaptiveScan>,
    pub highpass_ver_scan: Option<AdaptiveScan>,

    /// MB grid: `mb[MBx][MBy]`.
    pub mb: Vec<Vec<MB>>,

    /// Reconstructed pixels per component, one [`Plane2D`] per channel.
    /// Sized to padded `width × height` until `clipping_and_packing_stage`
    /// trims to the final image size.
    pub image_plane: Vec<Plane2D>,
}

/// 2D image plane stored flat row-major in a single `Vec<i32>`. Replaces the
#[derive(Debug, Default, Clone)]
pub struct Plane2D {
    pub data: Vec<i32>,
    /// Row pitch in pixels. Equals the stored width (no extra padding).
    pub stride: usize,
    pub height: usize,
}

impl Plane2D {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            data: vec![0; width * height],
            stride: width,
            height,
        }
    }

    #[inline]
    #[allow(dead_code)] // port-parity accessor (callers index `data` directly)
    pub fn width(&self) -> usize {
        self.stride
    }

    #[inline]
    #[allow(dead_code)] // port-parity accessor
    pub fn get(&self, x: usize, y: usize) -> i32 {
        self.data[y * self.stride + x]
    }

    #[inline]
    #[allow(dead_code)] // port-parity accessor
    pub fn set(&mut self, x: usize, y: usize, v: i32) {
        self.data[y * self.stride + x] = v;
    }
}

impl Plane {
    pub fn new(is_alpha: bool) -> Self {
        Self {
            is_alpha,
            internal_clr_fmt: INT_YONLY,
            scaled_flag: 0,
            bands_present: DCONLY,
            lp_present: false,
            hp_present: false,
            flexbits_present: false,
            num_bands: 1,
            num_components: 1,
            chroma_per_blk: 4,
            num_lp_coeff: 16,
            chroma_centering_x: 0,
            chroma_centering_y: 0,
            shift_bits: 0,
            len_mantissa: 0,
            exp_bias: 0,
            dc_image_plane_uniform: false,
            lp_image_plane_uniform: false,
            hp_image_plane_uniform: false,
            dc_qp: None,
            lp_qp: None,
            hp_qp: None,
            lp_qp_eq_dc: false,
            hp_qp_eq_lp: false,
            model_dc: Model::default(),
            abs_level_ind_dc_lum: AdaptiveVLC::default(),
            abs_level_ind_dc_chr: AdaptiveVLC::default(),
            model_lp: Model::default(),
            dec_first_ind_lp_lum: AdaptiveVLC::default(),
            dec_ind_lp_lum0: AdaptiveVLC::default(),
            dec_ind_lp_lum1: AdaptiveVLC::default(),
            dec_first_ind_lp_chr: AdaptiveVLC::default(),
            dec_ind_lp_chr0: AdaptiveVLC::default(),
            dec_ind_lp_chr1: AdaptiveVLC::default(),
            abs_level_ind_lp0: AdaptiveVLC::default(),
            abs_level_ind_lp1: AdaptiveVLC::default(),
            lowpass_scan: None,
            count_zero_cbplp: 1,
            count_max_cbplp: 1,
            model_hp: Model::default(),
            dec_num_cbphp: AdaptiveVLC::default(),
            dec_num_blk_cbphp: AdaptiveVLC::default(),
            dec_first_ind_hp_lum: AdaptiveVLC::default(),
            dec_ind_hp_lum0: AdaptiveVLC::default(),
            dec_ind_hp_lum1: AdaptiveVLC::default(),
            dec_first_ind_hp_chr: AdaptiveVLC::default(),
            dec_ind_hp_chr0: AdaptiveVLC::default(),
            dec_ind_hp_chr1: AdaptiveVLC::default(),
            abs_level_ind_hp0: AdaptiveVLC::default(),
            abs_level_ind_hp1: AdaptiveVLC::default(),
            cbphp_model_hp: CBPHPModel::default(),
            highpass_hor_scan: None,
            highpass_ver_scan: None,
            mb: Vec::new(),
            image_plane: Vec::new(),
        }
    }
}

/// Image-wide decoder header.
pub struct ImageHeader {
    pub hard_tiling_flag: u32,
    pub tiling_flag: u32,
    pub frequency_mode: u32,
    pub spatial_xfrm_subordinate: u32,
    pub index_table_present_flag: u32,
    pub overlap_mode: u8,
    #[allow(dead_code)]
    pub short_header_flag: bool,
    #[allow(dead_code)]
    pub long_word_flag: bool,
    pub windowing_flag: u32,
    pub trim_flexbits_flag: u32,
    #[allow(dead_code)]
    pub red_blue_not_swapped_flag: u32,
    pub premultiplied_alpha_flag: u32,
    pub alpha_image_plane_flag: u32,

    pub output_clr_fmt: u8,
    pub output_bitdepth: u8,

    /// `image_width`/`image_height` are the *original* pre-padding sizes.
    pub image_width: u32,
    pub image_height: u32,
    /// `width`/`height` include extra padding pixels to a 16-pixel multiple.
    pub width: u32,
    pub height: u32,

    pub num_ver_tiles_minus1: u32,
    pub num_hor_tiles_minus1: u32,
    pub num_tile_cols: usize,
    pub num_tile_rows: usize,

    /// `tile_width_in_mb[NumTileCols]` after the trailing "rest" entry is
    /// appended; same for height.
    pub tile_width_in_mb: Vec<usize>,
    pub tile_height_in_mb: Vec<usize>,
    pub left_mb_index_of_tile: Vec<usize>,
    pub top_mb_index_of_tile: Vec<usize>,

    pub extra_pixels_top: u32,
    pub extra_pixels_left: u32,
    pub extra_pixels_bottom: u32,
    pub extra_pixels_right: u32,

    pub mb_width: usize,
    pub mb_height: usize,
}
