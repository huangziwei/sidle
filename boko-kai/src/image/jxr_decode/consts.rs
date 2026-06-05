//! JPEG-XR decoder constants and permutation tables.
//!
//! Direct port of the constant block at the top of calibre's `jxr_image.py`.
//! Names preserved verbatim so cross-referencing against the Python source
//! and the ITU-T T.832 spec is straightforward.

#![allow(non_upper_case_globals)]

// --- Bands ---

pub const DC: u8 = 0;
pub const LP: u8 = 1;
pub const HP: u8 = 2;
pub const FLEX: u8 = 3;

// --- Internal color formats ---

pub const INT_YONLY: u8 = 0;
pub const INT_YUV420: u8 = 1;
pub const INT_YUV422: u8 = 2;
pub const INT_YUV444: u8 = 3;
pub const INT_YUVK: u8 = 4;
pub const INT_NCOMPONENT: u8 = 6;

// --- Output color formats ---

pub const OUT_YONLY: u8 = 0;
pub const OUT_YUV420: u8 = 1;
pub const OUT_YUV422: u8 = 2;
pub const OUT_YUV444: u8 = 3;
pub const OUT_CMYK: u8 = 4;
pub const OUT_CMYKDIRECT: u8 = 5;
pub const OUT_NCOMPONENT: u8 = 6;
pub const OUT_RGB: u8 = 7;
pub const OUT_RGBE: u8 = 8;

// --- Output bit depths ---

pub const BD1WHITE1: u8 = 0;
pub const BD8: u8 = 1;
pub const BD16: u8 = 2;
pub const BD16S: u8 = 3;
pub const BD16F: u8 = 4;
pub const BD32S: u8 = 6;
pub const BD32F: u8 = 7;
pub const BD5: u8 = 8;
pub const BD10: u8 = 9;
pub const BD565: u8 = 10;
pub const BD1BLACK1: u8 = 15;

// --- Component modes ---

pub const COMP_UNIFORM: u8 = 0;
pub const COMP_SEPARATE: u8 = 1;
pub const COMP_INDEPENDENT: u8 = 2;

// --- Band-presence flags ---

pub const ALL_BANDS: u8 = 0;
pub const NOFLEXBITS: u8 = 1;
pub const NOHIGHPASS: u8 = 2;
pub const DCONLY: u8 = 3;

// --- Prediction modes ---

pub const PREDICT_FROM_LEFT: u8 = 0;
pub const PREDICT_FROM_TOP: u8 = 1;
pub const PREDICT_FROM_TOP_LEFT: u8 = 2;
pub const NO_PREDICTION: u8 = 3;

// --- Overlap-filter modes ---

pub const NO_OVERLAP_FILTERING: u8 = 0;
pub const SECOND_LEVEL_OVERLAP_FILTERING: u8 = 1;
pub const FIRST_AND_SECOND_LEVEL_OVERLAP_FILTERING: u8 = 2;

// --- Permutation / scan tables ---

pub const ICT4X4_INV_PERM: [usize; 16] = [
    0, 8, 4, 13, 2, 15, 3, 14, 1, 12, 5, 9, 7, 11, 6, 10,
];

pub const ICT4X4_PERM: [usize; 16] = [
    0, 8, 4, 6, 2, 10, 14, 12, 1, 11, 15, 13, 9, 3, 7, 5,
];

pub const I_HIER_SCAN_ORDER: [usize; 16] = [
    0, 4, 1, 5, 8, 12, 9, 13, 2, 6, 3, 7, 10, 14, 11, 15,
];

/// Zigzag inverse scan for 4x4 LP block (horizontal).
/// First entry is `None` in Python (unused); we shift by one so index 1..=15.
pub const GRGI_ZIGZAG_INV_4X4_H: [usize; 16] = [
    0, // index 0 unused
    1, 4, 5, 2, 8, 6, 9, 3, 12, 10, 7, 13, 11, 14, 15,
];

#[allow(dead_code)]
pub const GRGI_ZIGZAG_INV_4X4_V: [usize; 16] = [
    0, 4, 8, 5, 1, 12, 9, 6, 2, 13, 3, 15, 7, 10, 14, 11,
];

pub const GRGI_ZIGZAG_INV_4X4_H_PRIME: [usize; 16] = [
    0, 5, 10, 12, 1, 2, 8, 4, 6, 9, 3, 14, 13, 7, 11, 15,
];

pub const GRGI_ZIGZAG_INV_4X4_V_PRIME: [usize; 16] = [
    0, 10, 2, 12, 5, 9, 4, 8, 1, 13, 6, 15, 14, 3, 11, 7,
];

pub const I_TRANSPOSE_FLEX: [usize; 16] = [
    0, 5, 1, 6, 10, 12, 8, 14, 2, 4, 3, 7, 9, 13, 11, 15,
];

pub const MB_PIXEL_MAP: [usize; 16] = [
    0, 1, 5, 4, 2, 3, 7, 6, 10, 11, 15, 14, 8, 9, 13, 12,
];

pub const XY_TRANSPOSE: [usize; 16] = [
    0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15,
];

// --- CBP delta tables ---

pub const NUM_BLK_CBPHP_DELTA1: [[i32; 5]; 1] = [[0, -1, 0, 1, 1]];

pub const NUM_BLK_CBPHP_DELTA2: [[i32; 9]; 1] = [[2, 2, 1, 1, -1, -2, -2, -2, -3]];

pub const NUM_CBPHP_DELTA: [[i32; 5]; 1] = [[0, -1, 0, 1, 1]];

pub const FIRST_INDEX_DELTA: [[i32; 12]; 4] = [
    [1, 1, 1, 1, 1, 0, 0, -1, 2, 1, 0, 0],
    [2, 2, -1, -1, -1, 0, -2, -1, 0, 0, -2, -1],
    [-1, 1, 0, 2, 0, 0, 0, 0, -2, 0, 1, 1],
    [0, 1, 0, 1, -2, 0, -1, -1, -2, -1, -2, -2],
];

pub const INDEX1_DELTA: [[i32; 6]; 3] = [
    [-1, 1, 1, 1, 0, 1],
    [-2, 0, 0, 2, 0, 0],
    [-1, -1, 0, 1, -2, 0],
];

pub const ABS_LEVEL_INDEX_DELTA: [[i32; 7]; 1] = [[1, 0, -1, -1, -1, -1, -1]];

// --- Coordinate lists used by the overlap filters ---

pub const XY4: [(i32, i32); 16] = [
    (0, 0), (0, 1), (0, 2), (0, 3),
    (1, 0), (1, 1), (1, 2), (1, 3),
    (2, 0), (2, 1), (2, 2), (2, 3),
    (3, 0), (3, 1), (3, 2), (3, 3),
];

pub const YX2: [(i32, i32); 4] = [(0, 0), (0, 1), (1, 0), (1, 1)];

pub const XY2: [(i32, i32); 4] = [(0, 0), (1, 0), (0, 1), (1, 1)];

pub const X4: [(i32, i32); 4] = [(0, 0), (1, 0), (2, 0), (3, 0)];

pub const Y4: [(i32, i32); 4] = [(0, 0), (0, 1), (0, 2), (0, 3)];
