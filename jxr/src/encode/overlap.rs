//! Forward overlap (PRE-)filtering passes — the encode-side inverses of the
//! decoder's `first_level_overlap_filtering` / `second_level_overlap_filtering`
//! (`decode/decoder.rs`). Every filter window in a pass is pairwise disjoint,
//! so each pass mirrors the decoder's traversal with the pre-filter
//! primitives from [`super::transform`]; only the chroma corner ± phases are
//! order-sensitive, and those run as subtract → filters → add (the exact
//! inverse of the decoder's subtract → filters → add composition).
//!
//! Soft tiles only: the encoder never emits `hard_tiling_flag = 1`, so the
//! cross-tile continuation arms are always active, exactly like the files
//! jxrencapp produces.

use super::transform::{
    overlap_pre_filter_2, overlap_pre_filter_2x2, overlap_pre_filter_4, overlap_pre_filter_4x4,
    str_pre_4x4_stage2_split_alternate,
};
use crate::decode::consts::{X4, XY_TRANSPOSE, XY2, XY4, Y4};

/// Tile boundaries (len `ntiles + 1`) from a tile-size list in MB units;
/// an empty list is a single tile spanning `total_mb`.
pub(super) fn bounds(list: &[usize], total_mb: usize) -> Vec<usize> {
    let mut b = vec![0usize];
    if list.is_empty() {
        b.push(total_mb);
    } else {
        for &v in list {
            b.push(b.last().unwrap() + v);
        }
    }
    b
}

/// Access to one component's per-MB coefficient buffers (`[i32; 256]`-shaped
/// `mb_buffer` mirrors) for the first-level (block-DC domain) pass.
pub(super) trait DcGrid {
    fn dc(&self, mbx: usize, mby: usize, off: usize) -> i32;
    fn set_dc(&mut self, mbx: usize, mby: usize, off: usize, v: i32);
}

// ---------------------------------------------------------------------------
// Second-level (sample domain) pre-filter
// ---------------------------------------------------------------------------

fn sp16(data: &mut [i32], stride: usize, x: usize, y: usize) {
    let mut arr = [0i32; 16];
    for (k, (xx, yy)) in XY4.iter().enumerate() {
        arr[k] = data[(y + *yy as usize) * stride + x + *xx as usize];
    }
    let out = overlap_pre_filter_4x4(arr);
    for (k, (xx, yy)) in XY4.iter().enumerate() {
        data[(y + *yy as usize) * stride + x + *xx as usize] = out[k];
    }
}

fn sp4(data: &mut [i32], stride: usize, x: usize, y: usize, list: &[(i32, i32); 4]) {
    let mut arr = [0i32; 4];
    for (k, (xx, yy)) in list.iter().enumerate() {
        arr[k] = data[(y + *yy as usize) * stride + x + *xx as usize];
    }
    let out = overlap_pre_filter_4(arr);
    for (k, (xx, yy)) in list.iter().enumerate() {
        data[(y + *yy as usize) * stride + x + *xx as usize] = out[k];
    }
}

/// Sample-domain overlap PRE-filter over one component's padded plane
/// (overlap modes 1 and 2). `tile_x`/`tile_y` are the tile boundaries in
/// THIS component's pixels (MB boundaries ×16, already divided by the chroma
/// subsampling factors), length `ntiles + 1`. Mirrors the decoder's
/// `second_level_overlap_filtering` window-for-window.
pub(super) fn sample_pre_filter(
    data: &mut [i32],
    stride: usize,
    tile_x: &[usize],
    tile_y: &[usize],
) {
    let cols = tile_x.len() - 1;
    let rows = tile_y.len() - 1;
    for tx in 0..cols {
        for ty in 0..rows {
            let first_x = tile_x[tx];
            let next_x = tile_x[tx + 1];
            let first_y = tile_y[ty];
            let next_y = tile_y[ty + 1];

            let mut x = first_x + 2;
            while x < next_x.saturating_sub(2) {
                let mut y = first_y + 2;
                while y < next_y.saturating_sub(2) {
                    sp16(data, stride, x, y);
                    y += 4;
                }
                x += 4;
            }

            if tx == 0 {
                let mut y = first_y + 2;
                while y < next_y.saturating_sub(2) {
                    for xx in [0usize, 1] {
                        sp4(data, stride, first_x + xx, y, &Y4);
                    }
                    y += 4;
                }
            }
            if ty == 0 {
                let mut x = first_x + 2;
                while x < next_x.saturating_sub(2) {
                    for yy in [0usize, 1] {
                        sp4(data, stride, x, first_y + yy, &X4);
                    }
                    x += 4;
                }
            }
            if tx == cols - 1 {
                let mut y = first_y + 2;
                while y < next_y.saturating_sub(2) {
                    for xx in [2usize, 1] {
                        sp4(data, stride, next_x - xx, y, &Y4);
                    }
                    y += 4;
                }
            }
            if ty == rows - 1 {
                let mut x = first_x + 2;
                while x < next_x.saturating_sub(2) {
                    for yy in [2usize, 1] {
                        sp4(data, stride, x, next_y - yy, &X4);
                    }
                    x += 4;
                }
            }
            if tx == 0 && ty == 0 {
                sp4(data, stride, first_x, first_y, &XY2);
            }
            if tx == cols - 1 && ty == 0 {
                sp4(data, stride, next_x - 2, first_y, &XY2);
            }
            if tx == 0 && ty == rows - 1 {
                sp4(data, stride, first_x, next_y - 2, &XY2);
            }
            if tx == cols - 1 && ty == rows - 1 {
                sp4(data, stride, next_x - 2, next_y - 2, &XY2);
            }
            // Soft-tile continuations across the right/bottom boundaries.
            if tx != cols - 1 {
                let mut y = first_y + 2;
                while y < next_y.saturating_sub(2) {
                    sp16(data, stride, next_x - 2, y);
                    y += 4;
                }
            }
            if ty != rows - 1 {
                let mut x = first_x + 2;
                while x < next_x.saturating_sub(2) {
                    sp16(data, stride, x, next_y - 2);
                    x += 4;
                }
            }
            if tx != cols - 1 && ty != rows - 1 {
                sp16(data, stride, next_x - 2, next_y - 2);
            }
            if tx == 0 && ty != rows - 1 {
                for xx in [0usize, 1] {
                    sp4(data, stride, first_x + xx, next_y - 2, &Y4);
                }
            }
            if tx != cols - 1 && ty == 0 {
                for yy in [0usize, 1] {
                    sp4(data, stride, next_x - 2, first_y + yy, &X4);
                }
            }
            if tx == cols - 1 && ty != rows - 1 {
                for xx in [2usize, 1] {
                    sp4(data, stride, next_x - xx, next_y - 2, &Y4);
                }
            }
            if tx != cols - 1 && ty == rows - 1 {
                for yy in [2usize, 1] {
                    sp4(data, stride, next_x - 2, next_y - yy, &X4);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// First-level (block-DC domain) pre-filter, luma / 4:4:4 components
// ---------------------------------------------------------------------------

fn f16(g: &mut dyn DcGrid, x: usize, y: usize, list: &[(i32, i32, usize); 16]) {
    let mut arr = [0i32; 16];
    for (k, (xx, yy, zz)) in list.iter().enumerate() {
        arr[k] = g.dc(x + *xx as usize, y + *yy as usize, XY_TRANSPOSE[*zz] * 16);
    }
    let out = str_pre_4x4_stage2_split_alternate(&arr);
    for (k, (xx, yy, zz)) in list.iter().enumerate() {
        g.set_dc(
            x + *xx as usize,
            y + *yy as usize,
            XY_TRANSPOSE[*zz] * 16,
            out[k],
        );
    }
}

fn f4(g: &mut dyn DcGrid, x: usize, y: usize, list: &[(i32, i32, usize); 4]) {
    let mut arr = [0i32; 4];
    for (k, (xx, yy, zz)) in list.iter().enumerate() {
        arr[k] = g.dc(x + *xx as usize, y + *yy as usize, XY_TRANSPOSE[*zz] * 16);
    }
    let out = overlap_pre_filter_4(arr);
    for (k, (xx, yy, zz)) in list.iter().enumerate() {
        g.set_dc(
            x + *xx as usize,
            y + *yy as usize,
            XY_TRANSPOSE[*zz] * 16,
            out[k],
        );
    }
}

/// Block-DC-domain overlap PRE-filter for a luma / 4:4:4 component
/// (overlap mode 2 only), between forward stage 1 and stage 2. `left_mb` /
/// `top_mb` are the tile boundaries in MB units, length `ntiles + 1`.
/// Geometry lists verbatim from the decoder's `first_level_overlap_filtering`.
pub(super) fn dc_pre_filter_luma(g: &mut dyn DcGrid, left_mb: &[usize], top_mb: &[usize]) {
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

    let cols = left_mb.len() - 1;
    let rows = top_mb.len() - 1;
    for tx in 0..cols {
        for ty in 0..rows {
            let first_mbx = left_mb[tx];
            let last_mbx = left_mb[tx + 1] - 1;
            let first_mby = top_mb[ty];
            let last_mby = top_mb[ty + 1] - 1;

            for y in first_mby..last_mby {
                for x in first_mbx..last_mbx {
                    f16(g, x, y, &flc);
                }
            }
            if tx == 0 {
                for y in first_mby..last_mby {
                    f4(g, first_mbx, y, &le1);
                    f4(g, first_mbx, y, &le2);
                }
            }
            if ty == 0 {
                for x in first_mbx..last_mbx {
                    f4(g, x, first_mby, &te1);
                    f4(g, x, first_mby, &te2);
                }
            }
            if tx == cols - 1 {
                for y in first_mby..last_mby {
                    f4(g, last_mbx, y, &re1);
                    f4(g, last_mbx, y, &re2);
                }
            }
            if ty == rows - 1 {
                for x in first_mbx..last_mbx {
                    f4(g, x, last_mby, &be1);
                    f4(g, x, last_mby, &be2);
                }
            }
            if tx == 0 && ty == 0 {
                f4(g, first_mbx, first_mby, &tlc);
            }
            if tx == cols - 1 && ty == 0 {
                f4(g, last_mbx, first_mby, &trc);
            }
            if tx == 0 && ty == rows - 1 {
                f4(g, first_mbx, last_mby, &blc);
            }
            if tx == cols - 1 && ty == rows - 1 {
                f4(g, last_mbx, last_mby, &brc);
            }
            // Soft-tile continuations.
            if tx != cols - 1 {
                for y in first_mby..last_mby {
                    f16(g, last_mbx, y, &flc);
                }
            }
            if ty != rows - 1 {
                for x in first_mbx..last_mbx {
                    f16(g, x, last_mby, &flc);
                }
            }
            if tx != cols - 1 && ty != rows - 1 {
                f16(g, last_mbx, last_mby, &flc);
            }
            if tx == 0 && ty != rows - 1 {
                f4(g, first_mbx, last_mby, &le1);
                f4(g, first_mbx, last_mby, &le2);
            }
            if tx != cols - 1 && ty == 0 {
                f4(g, last_mbx, first_mby, &te1);
                f4(g, last_mbx, first_mby, &te2);
            }
            if tx == cols - 1 && ty != rows - 1 {
                f4(g, last_mbx, last_mby, &re1);
                f4(g, last_mbx, last_mby, &re2);
            }
            if tx != cols - 1 && ty == rows - 1 {
                f4(g, last_mbx, last_mby, &be1);
                f4(g, last_mbx, last_mby, &be2);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// First-level (block-DC domain) pre-filter, 4:2:0 / 4:2:2 chroma
// ---------------------------------------------------------------------------

fn cf2x2(g: &mut dyn DcGrid, cells: [(usize, usize, usize); 4]) {
    let arr = [
        g.dc(cells[0].0, cells[0].1, 16 * cells[0].2),
        g.dc(cells[1].0, cells[1].1, 16 * cells[1].2),
        g.dc(cells[2].0, cells[2].1, 16 * cells[2].2),
        g.dc(cells[3].0, cells[3].1, 16 * cells[3].2),
    ];
    let out = overlap_pre_filter_2x2(arr);
    for (k, &(x, y, b)) in cells.iter().enumerate() {
        g.set_dc(x, y, 16 * b, out[k]);
    }
}

fn cf2(g: &mut dyn DcGrid, cells: [(usize, usize, usize); 2]) {
    let arr = [
        g.dc(cells[0].0, cells[0].1, 16 * cells[0].2),
        g.dc(cells[1].0, cells[1].1, 16 * cells[1].2),
    ];
    let out = overlap_pre_filter_2(arr);
    for (k, &(x, y, b)) in cells.iter().enumerate() {
        g.set_dc(x, y, 16 * b, out[k]);
    }
}

/// Block-DC-domain overlap PRE-filter for one 4:2:0 / 4:2:2 chroma component
/// (overlap mode 2 only). Inverse of the decoder's
/// `first_level_overlap_chroma` (Tables 154/155): the decoder runs corner
/// differences (−=) → junction/edge filters → corner additions (+=), so the
/// inverse runs corner subtractions (the additions' inverse, SAME cells) →
/// PRE-filters over the same windows → corner additions (the differences'
/// inverse). Soft tiles only.
pub(super) fn dc_pre_filter_chroma(
    g: &mut dyn DcGrid,
    is420: bool,
    left_mb: &[usize],
    top_mb: &[usize],
) {
    let cols = left_mb.len() - 1;
    let rows = top_mb.len() - 1;
    let (bl, br) = if is420 {
        (2usize, 3usize)
    } else {
        (6usize, 7usize)
    };
    let x_left = left_mb[0];
    let x_right = left_mb[cols] - 1;
    let y_top = top_mb[0];
    let y_bot = top_mb[rows] - 1;

    // Phase A — inverse of the decoder's trailing corner ADDITIONS: subtract
    // at the same four image-corner cells.
    {
        let v = g.dc(x_left, y_top, 0) - g.dc(x_left, y_top, 16);
        g.set_dc(x_left, y_top, 0, v);
        let v = g.dc(x_right, y_top, 16) - g.dc(x_right, y_top, 0);
        g.set_dc(x_right, y_top, 16, v);
        let v = g.dc(x_left, y_bot, 16 * bl) - g.dc(x_left, y_bot, 16 * br);
        g.set_dc(x_left, y_bot, 16 * bl, v);
        let v = g.dc(x_right, y_bot, 16 * br) - g.dc(x_right, y_bot, 16 * bl);
        g.set_dc(x_right, y_bot, 16 * br, v);
    }

    // Phase B — PRE-filters over the decoder's (disjoint) windows.
    for ty in 0..rows {
        for tx in 0..cols {
            let (x0, x1) = (left_mb[tx], left_mb[tx + 1]);
            let (y0, y1) = (top_mb[ty], top_mb[ty + 1]);
            if is420 {
                for y in y0..y1.saturating_sub(1) {
                    for x in x0..x1.saturating_sub(1) {
                        cf2x2(
                            g,
                            [(x, y, 3), (x + 1, y, 2), (x, y + 1, 1), (x + 1, y + 1, 0)],
                        );
                    }
                }
            } else {
                for y in y0..y1 {
                    for x in x0..x1.saturating_sub(1) {
                        cf2x2(g, [(x, y, 3), (x + 1, y, 2), (x, y, 5), (x + 1, y, 4)]);
                        if y != y1 - 1 {
                            cf2x2(
                                g,
                                [(x, y, 7), (x + 1, y, 6), (x, y + 1, 1), (x + 1, y + 1, 0)],
                            );
                        }
                    }
                }
            }
            if tx == 0 {
                let x = x0;
                if is420 {
                    for y in y0..y1.saturating_sub(1) {
                        cf2(g, [(x, y, 2), (x, y + 1, 0)]);
                    }
                } else {
                    for y in y0..y1 {
                        cf2(g, [(x, y, 2), (x, y, 4)]);
                        if y != y1 - 1 {
                            cf2(g, [(x, y, 6), (x, y + 1, 0)]);
                        }
                    }
                }
            }
            if tx == cols - 1 {
                let x = x1 - 1;
                if is420 {
                    for y in y0..y1.saturating_sub(1) {
                        cf2(g, [(x, y, 3), (x, y + 1, 1)]);
                    }
                } else {
                    for y in y0..y1 {
                        cf2(g, [(x, y, 3), (x, y, 5)]);
                        if y != y1 - 1 {
                            cf2(g, [(x, y, 7), (x, y + 1, 1)]);
                        }
                    }
                }
            }
            if ty == 0 {
                let y = y0;
                for x in x0..x1.saturating_sub(1) {
                    cf2(g, [(x, y, 1), (x + 1, y, 0)]);
                }
            }
            if ty == rows - 1 {
                let y = y1 - 1;
                for x in x0..x1.saturating_sub(1) {
                    cf2(g, [(x, y, br), (x + 1, y, bl)]);
                }
            }
            if tx != cols - 1 {
                let x = x1 - 1;
                for y in y0..y1.saturating_sub(1) {
                    if is420 {
                        cf2x2(
                            g,
                            [(x, y, 3), (x + 1, y, 2), (x, y + 1, 1), (x + 1, y + 1, 0)],
                        );
                    } else {
                        cf2x2(g, [(x, y, 3), (x + 1, y, 2), (x, y, 5), (x + 1, y, 4)]);
                        cf2x2(
                            g,
                            [(x, y, 7), (x + 1, y, 6), (x, y + 1, 1), (x + 1, y + 1, 0)],
                        );
                    }
                }
            }
            if ty != rows - 1 {
                let y = y1 - 1;
                for x in x0..x1.saturating_sub(1) {
                    if is420 {
                        cf2x2(
                            g,
                            [(x, y, 3), (x + 1, y, 2), (x, y + 1, 1), (x + 1, y + 1, 0)],
                        );
                    } else {
                        cf2x2(g, [(x, y, 3), (x + 1, y, 2), (x, y, 5), (x + 1, y, 4)]);
                        cf2x2(
                            g,
                            [(x, y, 7), (x + 1, y, 6), (x, y + 1, 1), (x + 1, y + 1, 0)],
                        );
                    }
                }
            }
            if tx != cols - 1 && ty != rows - 1 {
                let (x, y) = (x1 - 1, y1 - 1);
                if is420 {
                    cf2x2(
                        g,
                        [(x, y, 3), (x + 1, y, 2), (x, y + 1, 1), (x + 1, y + 1, 0)],
                    );
                } else {
                    cf2x2(g, [(x, y, 3), (x + 1, y, 2), (x, y, 5), (x + 1, y, 4)]);
                    cf2x2(
                        g,
                        [(x, y, 7), (x + 1, y, 6), (x, y + 1, 1), (x + 1, y + 1, 0)],
                    );
                }
            }
            if tx == 0 && ty != rows - 1 {
                let (x, y) = (x0, y1 - 1);
                if is420 {
                    cf2(g, [(x, y, 2), (x, y + 1, 0)]);
                } else {
                    cf2(g, [(x, y, 2), (x, y, 4)]);
                    cf2(g, [(x, y, 6), (x, y + 1, 0)]);
                }
            }
            if tx == cols - 1 && ty != rows - 1 {
                let (x, y) = (x1 - 1, y1 - 1);
                if is420 {
                    cf2(g, [(x, y, 3), (x, y + 1, 1)]);
                } else {
                    cf2(g, [(x, y, 3), (x, y, 5)]);
                    cf2(g, [(x, y, 7), (x, y + 1, 1)]);
                }
            }
            if tx != cols - 1 && ty == 0 {
                let (x, y) = (x1 - 1, y0);
                cf2(g, [(x, y, 1), (x + 1, y, 0)]);
            }
            if tx != cols - 1 && ty == rows - 1 {
                let (x, y) = (x1 - 1, y1 - 1);
                cf2(g, [(x, y, br), (x + 1, y, bl)]);
            }
        }
    }

    // Phase C — inverse of the decoder's leading corner DIFFERENCES: add at
    // the same four image-corner cells.
    {
        let v = g.dc(x_left, y_top, 0) + g.dc(x_left, y_top, 16);
        g.set_dc(x_left, y_top, 0, v);
        let v = g.dc(x_right, y_top, 16) + g.dc(x_right, y_top, 0);
        g.set_dc(x_right, y_top, 16, v);
        let v = g.dc(x_left, y_bot, 16 * bl) + g.dc(x_left, y_bot, 16 * br);
        g.set_dc(x_left, y_bot, 16 * bl, v);
        let v = g.dc(x_right, y_bot, 16 * br) + g.dc(x_right, y_bot, 16 * bl);
        g.set_dc(x_right, y_bot, 16 * br, v);
    }
}
