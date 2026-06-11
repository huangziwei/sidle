//! T.832 constants: the public spec-code vocabulary for the raw `u8` fields
//! on [`DecodedImage`](crate::decode::decoder::DecodedImage) and
//! [`HeaderSummary`](crate::decode::decoder::HeaderSummary), plus the
//! crate-internal permutation/delta tables.
//!
//! Direct port of the constant block at the top of calibre's `jxr_image.py`.
//! Names preserved verbatim so cross-referencing against the Python source
//! and the ITU-T T.832 spec is straightforward.

#![allow(non_upper_case_globals)]

// --- Bands ---

pub(crate) const DC: u8 = 0;
pub(crate) const LP: u8 = 1;
pub(crate) const HP: u8 = 2;
pub(crate) const FLEX: u8 = 3;

// --- Internal color formats ---

/// Internal color format: single luma plane.
pub const INT_YONLY: u8 = 0;
/// Internal color format: YUV, chroma halved both ways.
pub const INT_YUV420: u8 = 1;
/// Internal color format: YUV, chroma halved horizontally.
pub const INT_YUV422: u8 = 2;
/// Internal color format: YUV at full chroma resolution.
pub const INT_YUV444: u8 = 3;
/// Internal color format: YUVK (the CMYK ink transform).
pub const INT_YUVK: u8 = 4;
/// Internal color format: N independent components.
pub const INT_NCOMPONENT: u8 = 6;

// --- Output color formats ---

/// Output color format: grayscale.
pub const OUT_YONLY: u8 = 0;
/// Output color format: YUV 4:2:0 (emitted as-is).
pub const OUT_YUV420: u8 = 1;
/// Output color format: YUV 4:2:2 (emitted as-is).
pub const OUT_YUV422: u8 = 2;
/// Output color format: YUV 4:4:4 (emitted as-is).
pub const OUT_YUV444: u8 = 3;
/// Output color format: CMYK ink (via the YUVK inverse).
pub const OUT_CMYK: u8 = 4;
/// Output color format: CMYK ink, coded directly.
pub const OUT_CMYKDIRECT: u8 = 5;
/// Output color format: N independent channels.
pub const OUT_NCOMPONENT: u8 = 6;
/// Output color format: RGB.
pub const OUT_RGB: u8 = 7;
/// Output color format: Radiance shared-exponent HDR.
pub const OUT_RGBE: u8 = 8;

// --- Output bit depths ---

/// Output bit depth: bi-level, 1 = white.
pub const BD1WHITE1: u8 = 0;
/// Output bit depth: 8-bit unsigned.
pub const BD8: u8 = 1;
/// Output bit depth: 16-bit unsigned.
pub const BD16: u8 = 2;
/// Output bit depth: 16-bit signed fixed point.
pub const BD16S: u8 = 3;
/// Output bit depth: IEEE-754 half float (bit pattern).
pub const BD16F: u8 = 4;
/// Output bit depth: 32-bit signed fixed point.
pub const BD32S: u8 = 6;
/// Output bit depth: IEEE-754 single float (bit pattern).
pub const BD32F: u8 = 7;
/// Output bit depth: packed 5-5-5 RGB.
pub const BD5: u8 = 8;
/// Output bit depth: packed 10-10-10 RGB.
pub const BD10: u8 = 9;
/// Output bit depth: packed 5-6-5 RGB.
pub const BD565: u8 = 10;
/// Output bit depth: bi-level, 1 = black.
pub const BD1BLACK1: u8 = 15;

// --- Component modes ---

pub(crate) const COMP_UNIFORM: u8 = 0;
pub(crate) const COMP_SEPARATE: u8 = 1;
pub(crate) const COMP_INDEPENDENT: u8 = 2;

// --- Band-presence flags ---

/// Bands present: DC + LP + HP + flexbits (everything).
pub const ALL_BANDS: u8 = 0;
/// Bands present: DC + LP + HP without flexbits.
pub const NOFLEXBITS: u8 = 1;
/// Bands present: DC + LP only.
pub const NOHIGHPASS: u8 = 2;
/// Bands present: DC only.
pub const DCONLY: u8 = 3;

// --- Prediction modes ---

pub(crate) const PREDICT_FROM_LEFT: u8 = 0;
pub(crate) const PREDICT_FROM_TOP: u8 = 1;
pub(crate) const PREDICT_FROM_TOP_LEFT: u8 = 2;
pub(crate) const NO_PREDICTION: u8 = 3;

// --- Overlap-filter modes ---

/// Overlap mode 0: no overlap filtering.
pub const NO_OVERLAP_FILTERING: u8 = 0;
/// Overlap mode 1: sample-domain filtering only.
pub const SECOND_LEVEL_OVERLAP_FILTERING: u8 = 1;
/// Overlap mode 2: sample- and block-DC-domain filtering.
pub const FIRST_AND_SECOND_LEVEL_OVERLAP_FILTERING: u8 = 2;

// --- Permutation / scan tables ---

pub(crate) const ICT4X4_INV_PERM: [usize; 16] = [
    0, 8, 4, 13, 2, 15, 3, 14, 1, 12, 5, 9, 7, 11, 6, 10,
];

// Port-parity table (jxr_image.py carries the forward permutation too).
#[allow(dead_code)]
pub(crate) const ICT4X4_PERM: [usize; 16] = [
    0, 8, 4, 6, 2, 10, 14, 12, 1, 11, 15, 13, 9, 3, 7, 5,
];

pub(crate) const I_HIER_SCAN_ORDER: [usize; 16] = [
    0, 4, 1, 5, 8, 12, 9, 13, 2, 6, 3, 7, 10, 14, 11, 15,
];

/// Zigzag inverse scan for 4x4 LP block (horizontal).
/// First entry is `None` in Python (unused); we shift by one so index 1..=15.
pub(crate) const GRGI_ZIGZAG_INV_4X4_H: [usize; 16] = [
    0, // index 0 unused
    1, 4, 5, 2, 8, 6, 9, 3, 12, 10, 7, 13, 11, 14, 15,
];

#[allow(dead_code)]
pub(crate) const GRGI_ZIGZAG_INV_4X4_V: [usize; 16] = [
    0, 4, 8, 5, 1, 12, 9, 6, 2, 13, 3, 15, 7, 10, 14, 11,
];

pub(crate) const GRGI_ZIGZAG_INV_4X4_H_PRIME: [usize; 16] = [
    0, 5, 10, 12, 1, 2, 8, 4, 6, 9, 3, 14, 13, 7, 11, 15,
];

pub(crate) const GRGI_ZIGZAG_INV_4X4_V_PRIME: [usize; 16] = [
    0, 10, 2, 12, 5, 9, 4, 8, 1, 13, 6, 15, 14, 3, 11, 7,
];

pub(crate) const I_TRANSPOSE_FLEX: [usize; 16] = [
    0, 5, 1, 6, 10, 12, 8, 14, 2, 4, 3, 7, 9, 13, 11, 15,
];

pub(crate) const MB_PIXEL_MAP: [usize; 16] = [
    0, 1, 5, 4, 2, 3, 7, 6, 10, 11, 15, 14, 8, 9, 13, 12,
];

pub(crate) const XY_TRANSPOSE: [usize; 16] = [
    0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15,
];

// --- CBP delta tables ---

pub(crate) const NUM_BLK_CBPHP_DELTA1: [[i32; 5]; 1] = [[0, -1, 0, 1, 1]];

pub(crate) const NUM_BLK_CBPHP_DELTA2: [[i32; 9]; 1] = [[2, 2, 1, 1, -1, -2, -2, -2, -3]];

pub(crate) const NUM_CBPHP_DELTA: [[i32; 5]; 1] = [[0, -1, 0, 1, 1]];

pub(crate) const FIRST_INDEX_DELTA: [[i32; 12]; 4] = [
    [1, 1, 1, 1, 1, 0, 0, -1, 2, 1, 0, 0],
    [2, 2, -1, -1, -1, 0, -2, -1, 0, 0, -2, -1],
    [-1, 1, 0, 2, 0, 0, 0, 0, -2, 0, 1, 1],
    [0, 1, 0, 1, -2, 0, -1, -1, -2, -1, -2, -2],
];

pub(crate) const INDEX1_DELTA: [[i32; 6]; 3] = [
    [-1, 1, 1, 1, 0, 1],
    [-2, 0, 0, 2, 0, 0],
    [-1, -1, 0, 1, -2, 0],
];

pub(crate) const ABS_LEVEL_INDEX_DELTA: [[i32; 7]; 1] = [[1, 0, -1, -1, -1, -1, -1]];

// --- Coordinate lists used by the overlap filters ---

pub(crate) const XY4: [(i32, i32); 16] = [
    (0, 0), (0, 1), (0, 2), (0, 3),
    (1, 0), (1, 1), (1, 2), (1, 3),
    (2, 0), (2, 1), (2, 2), (2, 3),
    (3, 0), (3, 1), (3, 2), (3, 3),
];

// Port-parity coordinate list (jxr_image.py keeps both orders).
#[allow(dead_code)]
pub(crate) const YX2: [(i32, i32); 4] = [(0, 0), (0, 1), (1, 0), (1, 1)];

pub(crate) const XY2: [(i32, i32); 4] = [(0, 0), (1, 0), (0, 1), (1, 1)];

pub(crate) const X4: [(i32, i32); 4] = [(0, 0), (1, 0), (2, 0), (3, 0)];

pub(crate) const Y4: [(i32, i32); 4] = [(0, 0), (0, 1), (0, 2), (0, 3)];
