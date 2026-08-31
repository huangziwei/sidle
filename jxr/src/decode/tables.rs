//! Huffman tables used by the JPEG-XR decoder.

use std::sync::OnceLock;

use super::misc::{HuffTable, hbin};

/// Helper: build-and-cache a single Huffman table.
fn cached(slot: &'static OnceLock<HuffTable>, build: fn() -> HuffTable) -> &'static HuffTable {
    slot.get_or_init(build)
}

// --- VAL_DC_YUV ---

pub fn val_dc_yuv() -> &'static HuffTable {
    static SLOT: OnceLock<HuffTable> = OnceLock::new();
    cached(&SLOT, || {
        hbin(&[
            ("10", 0),
            ("001", 1),
            ("00001", 2),
            ("0001", 3),
            ("11", 4),
            ("010", 5),
            ("00000", 6),
            ("011", 7),
        ])
    })
}

// --- NUM_CBPHP ---

pub fn num_cbphp(idx: usize) -> &'static HuffTable {
    static SLOT0: OnceLock<HuffTable> = OnceLock::new();
    static SLOT1: OnceLock<HuffTable> = OnceLock::new();
    match idx {
        0 => cached(&SLOT0, || {
            hbin(&[("1", 0), ("01", 1), ("001", 2), ("0000", 3), ("0001", 4)])
        }),
        1 => cached(&SLOT1, || {
            hbin(&[("1", 0), ("000", 1), ("001", 2), ("010", 3), ("011", 4)])
        }),
        _ => panic!("num_cbphp idx {idx} out of range"),
    }
}

// --- NUM_BLKCBPHP1 (alias of NUM_CBPHP) ---
// (handled via num_cbphp)

// --- NUM_BLKCBPHP2 ---

pub fn num_blkcbphp2(idx: usize) -> &'static HuffTable {
    static SLOT0: OnceLock<HuffTable> = OnceLock::new();
    static SLOT1: OnceLock<HuffTable> = OnceLock::new();
    match idx {
        0 => cached(&SLOT0, || {
            hbin(&[
                ("010", 0),
                ("00000", 1),
                ("0010", 2),
                ("00001", 3),
                ("00010", 4),
                ("1", 5),
                ("011", 6),
                ("00011", 7),
                ("0011", 8),
            ])
        }),
        1 => cached(&SLOT1, || {
            hbin(&[
                ("1", 0),
                ("001", 1),
                ("010", 2),
                ("0001", 3),
                ("000001", 4),
                ("011", 5),
                ("00001", 6),
                ("0000000", 7),
                ("0000001", 8),
            ])
        }),
        _ => panic!("num_blkcbphp2 idx {idx} out of range"),
    }
}

// --- FIRST_INDEX ---

pub fn first_index(idx: usize) -> &'static HuffTable {
    static SLOT0: OnceLock<HuffTable> = OnceLock::new();
    static SLOT1: OnceLock<HuffTable> = OnceLock::new();
    static SLOT2: OnceLock<HuffTable> = OnceLock::new();
    static SLOT3: OnceLock<HuffTable> = OnceLock::new();
    static SLOT4: OnceLock<HuffTable> = OnceLock::new();
    match idx {
        0 => cached(&SLOT0, || {
            hbin(&[
                ("00001", 0),
                ("000001", 1),
                ("0000000", 2),
                ("0000001", 3),
                ("00100", 4),
                ("010", 5),
                ("00101", 6),
                ("1", 7),
                ("00110", 8),
                ("0001", 9),
                ("00111", 10),
                ("011", 11),
            ])
        }),
        1 => cached(&SLOT1, || {
            hbin(&[
                ("0010", 0),
                ("00010", 1),
                ("000000", 2),
                ("000001", 3),
                ("0011", 4),
                ("010", 5),
                ("00011", 6),
                ("11", 7),
                ("011", 8),
                ("100", 9),
                ("00001", 10),
                ("101", 11),
            ])
        }),
        2 => cached(&SLOT2, || {
            hbin(&[
                ("11", 0),
                ("001", 1),
                ("0000000", 2),
                ("0000001", 3),
                ("00001", 4),
                ("010", 5),
                ("0000010", 6),
                ("011", 7),
                ("100", 8),
                ("101", 9),
                ("0000011", 10),
                ("0001", 11),
            ])
        }),
        3 => cached(&SLOT3, || {
            hbin(&[
                ("001", 0),
                ("11", 1),
                ("0000000", 2),
                ("00001", 3),
                ("00010", 4),
                ("010", 5),
                ("0000001", 6),
                ("011", 7),
                ("00011", 8),
                ("100", 9),
                ("000001", 10),
                ("101", 11),
            ])
        }),
        4 => cached(&SLOT4, || {
            hbin(&[
                ("010", 0),
                ("1", 1),
                ("0000001", 2),
                ("0001", 3),
                ("0000010", 4),
                ("011", 5),
                ("00000000", 6),
                ("0010", 7),
                ("0000011", 8),
                ("0011", 9),
                ("00000001", 10),
                ("00001", 11),
            ])
        }),
        _ => panic!("first_index {idx}"),
    }
}

// --- INDEX_A ---

pub fn index_a(idx: usize) -> &'static HuffTable {
    static SLOT0: OnceLock<HuffTable> = OnceLock::new();
    static SLOT1: OnceLock<HuffTable> = OnceLock::new();
    static SLOT2: OnceLock<HuffTable> = OnceLock::new();
    static SLOT3: OnceLock<HuffTable> = OnceLock::new();
    match idx {
        0 => cached(&SLOT0, || {
            hbin(&[
                ("1", 0),
                ("00000", 1),
                ("001", 2),
                ("00001", 3),
                ("01", 4),
                ("0001", 5),
            ])
        }),
        1 => cached(&SLOT1, || {
            hbin(&[
                ("01", 0),
                ("0000", 1),
                ("10", 2),
                ("0001", 3),
                ("11", 4),
                ("001", 5),
            ])
        }),
        2 => cached(&SLOT2, || {
            hbin(&[
                ("0000", 0),
                ("0001", 1),
                ("01", 2),
                ("10", 3),
                ("11", 4),
                ("001", 5),
            ])
        }),
        3 => cached(&SLOT3, || {
            hbin(&[
                ("00000", 0),
                ("00001", 1),
                ("01", 2),
                ("1", 3),
                ("0001", 4),
                ("001", 5),
            ])
        }),
        _ => panic!("index_a {idx}"),
    }
}

// --- INDEX_B ---

pub fn index_b() -> &'static HuffTable {
    static SLOT: OnceLock<HuffTable> = OnceLock::new();
    cached(&SLOT, || {
        hbin(&[("0", 0), ("10", 2), ("110", 1), ("111", 3)])
    })
}

// --- RUN_INDEX ---

pub fn run_index() -> &'static HuffTable {
    static SLOT: OnceLock<HuffTable> = OnceLock::new();
    cached(&SLOT, || {
        hbin(&[("1", 0), ("01", 1), ("001", 2), ("0000", 3), ("0001", 4)])
    })
}

// --- RUN_VALUE ---

/// `RUN_VALUE[max_run]`; indices 0..=1 are unused (None in Python).
pub fn run_value(max_run: usize) -> &'static HuffTable {
    static SLOT2: OnceLock<HuffTable> = OnceLock::new();
    static SLOT3: OnceLock<HuffTable> = OnceLock::new();
    static SLOT4: OnceLock<HuffTable> = OnceLock::new();
    match max_run {
        2 => cached(&SLOT2, || hbin(&[("1", 1), ("0", 2)])),
        3 => cached(&SLOT3, || hbin(&[("1", 1), ("01", 2), ("00", 3)])),
        4 => cached(&SLOT4, || {
            hbin(&[("1", 1), ("01", 2), ("001", 3), ("000", 4)])
        }),
        _ => panic!("run_value max_run {max_run}"),
    }
}

// --- ABS_LEVEL_INDEX ---

pub fn abs_level_index(idx: usize) -> &'static HuffTable {
    static SLOT0: OnceLock<HuffTable> = OnceLock::new();
    static SLOT1: OnceLock<HuffTable> = OnceLock::new();
    match idx {
        0 => cached(&SLOT0, || {
            hbin(&[
                ("01", 0),
                ("10", 1),
                ("11", 2),
                ("001", 3),
                ("0001", 4),
                ("00000", 5),
                ("00001", 6),
            ])
        }),
        1 => cached(&SLOT1, || {
            hbin(&[
                ("1", 0),
                ("01", 1),
                ("001", 2),
                ("0001", 3),
                ("00001", 4),
                ("000000", 5),
                ("000001", 6),
            ])
        }),
        _ => panic!("abs_level_index {idx}"),
    }
}

// --- REF_CBPHP1, NUM_CH_BLK, CHR_CBPHP/VAL_INC/CBPHP_CH_BLK ---

pub fn ref_cbphp1() -> &'static HuffTable {
    static SLOT: OnceLock<HuffTable> = OnceLock::new();
    cached(&SLOT, || {
        hbin(&[
            ("00", 3),
            ("01", 5),
            ("100", 6),
            ("101", 9),
            ("110", 10),
            ("111", 12),
        ])
    })
}

pub fn num_ch_blk() -> &'static HuffTable {
    static SLOT: OnceLock<HuffTable> = OnceLock::new();
    cached(&SLOT, || {
        hbin(&[("1", 0), ("01", 1), ("000", 2), ("001", 3)])
    })
}

/// Shared by CHR_CBPHP, VAL_INC, CBPHP_CH_BLK in calibre.
pub fn chr_cbphp() -> &'static HuffTable {
    static SLOT: OnceLock<HuffTable> = OnceLock::new();
    cached(&SLOT, || hbin(&[("1", 0), ("01", 1), ("00", 2)]))
}

pub fn val_inc() -> &'static HuffTable {
    chr_cbphp()
}

#[allow(dead_code)]
pub fn cbphp_ch_blk() -> &'static HuffTable {
    chr_cbphp()
}

// --- CBPLP_YUV1 ---

pub fn cbplp_yuv1_444() -> &'static HuffTable {
    static SLOT: OnceLock<HuffTable> = OnceLock::new();
    cached(&SLOT, || {
        hbin(&[
            ("0", 0),
            ("100", 1),
            ("1010", 2),
            ("1011", 3),
            ("1100", 4),
            ("1101", 5),
            ("1110", 6),
            ("1111", 7),
        ])
    })
}

pub fn cbplp_yuv1_42x() -> &'static HuffTable {
    static SLOT: OnceLock<HuffTable> = OnceLock::new();
    cached(&SLOT, || {
        hbin(&[("0", 0), ("10", 1), ("110", 2), ("111", 3)])
    })
}
