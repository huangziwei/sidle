//! `/dev/fb0` access: standard Linux fbdev ioctls for geometry +
//! Lab126's MXCFB ioctls for the eink refresh trigger.
//!
//! Layout notes:
//! - `fb_var_screeninfo` / `fb_fix_screeninfo` are the kernel's stable fbdev
//!   structs (linux/fb.h). Mirrored here with `#[repr(C)]` so the ioctl reads
//!   them byte-for-byte. `unsigned long` is 32-bit on armv7l, so we use
//!   `libc::c_ulong` to follow the host's wordsize when running tests on the
//!   dev machine.
//! - The MXCFB structs are Lab126's eink-refresh contract. The v2 layout
//!   (with `dither_mode` + `quant_bit`) has been stable from PW3 through
//!   KOA2/KOA3 — the firmware our KOA2 ships matches it.
//! - Ioctl number for `MXCFB_SEND_UPDATE` is computed once at build time:
//!   `_IOW('F', 0x2E, sizeof(mxcfb_update_data_v2))`. We hardcode the
//!   resulting `0x4048462E` so this is debuggable as a constant; if a
//!   future kernel changes the struct size, we'll see an `EINVAL` and know
//!   exactly what to recompute.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::ptr;

use anyhow::{Context, Result, bail};

use crate::orientation::Orientation;

// Standard Linux fbdev ioctls — won't change across kernels.
// Note: libc::ioctl takes `c_int` on Linux (not `c_ulong` like BSD), so the
// constants are typed accordingly. All our request values fit in i32.
const FBIOGET_VSCREENINFO: libc::c_int = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_int = 0x4602;

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct FbBitfield {
    pub offset: u32,
    pub length: u32,
    pub msb_right: u32,
}

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct FbVarScreeninfo {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub grayscale: u32,
    pub red: FbBitfield,
    pub green: FbBitfield,
    pub blue: FbBitfield,
    pub transp: FbBitfield,
    pub nonstd: u32,
    pub activate: u32,
    pub height: u32,
    pub width: u32,
    pub accel_flags: u32,
    pub pixclock: u32,
    pub left_margin: u32,
    pub right_margin: u32,
    pub upper_margin: u32,
    pub lower_margin: u32,
    pub hsync_len: u32,
    pub vsync_len: u32,
    pub sync: u32,
    pub vmode: u32,
    pub rotate: u32,
    pub colorspace: u32,
    pub reserved: [u32; 4],
}

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct FbFixScreeninfo {
    pub id: [u8; 16],
    pub smem_start: libc::c_ulong,
    pub smem_len: u32,
    pub type_: u32,
    pub type_aux: u32,
    pub visual: u32,
    pub xpanstep: u16,
    pub ypanstep: u16,
    pub ywrapstep: u16,
    pub line_length: u32,
    pub mmio_start: libc::c_ulong,
    pub mmio_len: u32,
    pub accel: u32,
    pub capabilities: u16,
    pub reserved: [u16; 2],
}

// MXCFB structs (Lab126 eink driver, v2 layout) ---------------------------

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct MxcfbRect {
    pub top: u32,
    pub left: u32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct MxcfbAltBufferData {
    pub phys_addr: u32,
    pub width: u32,
    pub height: u32,
    pub alt_update_region: MxcfbRect,
}

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct MxcfbUpdateDataV2 {
    pub update_region: MxcfbRect,
    pub waveform_mode: u32,
    pub update_mode: u32,
    pub update_marker: u32,
    pub temp: i32,
    pub flags: u32,
    pub dither_mode: i32,
    pub quant_bit: i32,
    pub alt_buffer_data: MxcfbAltBufferData,
}

// _IOW('F', 0x2E, mxcfb_update_data_v3): direction (W=1)<<30 | size(88)<<16 |
// type('F'=0x46)<<8 | nr(0x2E) = 0x4058462E. Confirmed at runtime on KOA2
// firmware 5.16.2.1.1 by probing v2/v3/v1 ioctl numbers — only the 88-byte
// v3 (with hist + ts trailing fields) is accepted. Bake it in; if a future
// firmware needs a different size, re-run the probe (commit 1742f55 history
// for the probe code).
pub const MXCFB_SEND_UPDATE: libc::c_int = 0x4058462E;

// V3 layout — same as V2 with four trailing u32s. We pad the in-memory
// struct out to 88B so the kernel never reads beyond what we've zeroed.
#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct MxcfbUpdateDataV3 {
    pub v2: MxcfbUpdateDataV2,
    pub hist_bw_waveform_mode: u32,
    pub hist_gray_waveform_mode: u32,
    pub ts_pxp: u32,
    pub ts_epdc: u32,
}

// Waveform modes (stable across Kindle generations per lab126 mxcfb.h).
// GC16 is the right choice for "show new full-grayscale content" — slow but
// clean. DU is fast B/W, used for tap feedback later.
#[allow(dead_code)]
pub const WAVEFORM_MODE_INIT: u32 = 0;
#[allow(dead_code)]
pub const WAVEFORM_MODE_DU: u32 = 1;
pub const WAVEFORM_MODE_GC16: u32 = 2;
#[allow(dead_code)]
pub const WAVEFORM_MODE_GC4: u32 = 3;
#[allow(dead_code)]
pub const WAVEFORM_MODE_A2: u32 = 4;
#[allow(dead_code)]
pub const WAVEFORM_MODE_GL16: u32 = 5;
#[allow(dead_code)]
pub const WAVEFORM_MODE_AUTO: u32 = 257;

pub const UPDATE_MODE_PARTIAL: u32 = 0;
#[allow(dead_code)]
pub const UPDATE_MODE_FULL: u32 = 1;

// 0x1000 = "let the panel pick based on ambient temperature sensor".
pub const TEMP_USE_AMBIENT: i32 = 0x1000;

pub struct Framebuffer {
    file: File,
    pub var: FbVarScreeninfo,
    pub fix: FbFixScreeninfo,
    mmap_ptr: *mut u8,
    mmap_len: usize,
    /// Monotonic counter for `update_marker`. The driver echoes this back via
    /// `MXCFB_WAIT_FOR_UPDATE_COMPLETE` (used later for tap-feedback paths).
    next_marker: u32,
    /// Applied to every send_update so the eink driver gets a panel-coords
    /// rect and the flush copies pixels into the right physical location.
    /// All drawing operations write into `backing` in *user* coords; the
    /// orientation transform happens once per flush.
    pub orientation: Orientation,
    /// Off-screen backing buffer (same size + stride as mmap). All UI
    /// drawing writes here in user-visible coords. `send_update` copies
    /// the affected rect into mmap, applying the orientation transform.
    /// Cost: one extra `smem_len` bytes (~4.5MB on KOA2) and one full-rect
    /// memcpy per refresh — fast enough on armv7l, hugely cleaner than
    /// per-blit orientation handling.
    backing: Vec<u8>,
}

impl Framebuffer {
    pub fn open(orientation: Orientation) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fb0")
            .context("open /dev/fb0")?;

        let mut var = FbVarScreeninfo::default();
        let mut fix = FbFixScreeninfo::default();
        let fd = file.as_raw_fd();

        // SAFETY: the structs are #[repr(C)] and match the kernel headers
        // exactly; the kernel writes them in full on success.
        unsafe {
            if libc::ioctl(fd, FBIOGET_VSCREENINFO, &mut var as *mut _) != 0 {
                bail!("FBIOGET_VSCREENINFO: {}", io::Error::last_os_error());
            }
            if libc::ioctl(fd, FBIOGET_FSCREENINFO, &mut fix as *mut _) != 0 {
                bail!("FBIOGET_FSCREENINFO: {}", io::Error::last_os_error());
            }
        }

        let mmap_len = fix.smem_len as usize;
        // SAFETY: kernel-allocated framebuffer, MAP_SHARED so writes hit the
        // panel buffer the driver will scan out.
        let mmap_ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                mmap_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if mmap_ptr == libc::MAP_FAILED {
            bail!("mmap fb: {}", io::Error::last_os_error());
        }

        let backing = vec![0xFFu8; mmap_len];
        Ok(Self {
            file,
            var,
            fix,
            mmap_ptr: mmap_ptr as *mut u8,
            mmap_len,
            next_marker: 1,
            orientation,
            backing,
        })
    }

    /// Transform a user-coords rect corner to physical-coords corner. The
    /// rect's width/height are unchanged.
    #[inline]
    fn phys_rect(&self, top: u32, left: u32, width: u32, height: u32) -> (u32, u32) {
        match self.orientation {
            Orientation::Up => (top, left),
            Orientation::Down => (
                self.var.yres.saturating_sub(top + height),
                self.var.xres.saturating_sub(left + width),
            ),
        }
    }

    /// Single-pixel write in user coords. Writes to the backing buffer —
    /// nothing hits mmap until `send_update` flushes. Out-of-range writes
    /// silently no-op (callers like the blitters pass arbitrary src/dst
    /// pairs and rely on us to clip).
    #[inline]
    pub fn put_pixel(&mut self, x: i32, y: i32, value: u8) {
        if x < 0 || y < 0 {
            return;
        }
        if x >= self.var.xres as i32 || y >= self.var.yres as i32 {
            return;
        }
        let line_length = self.fix.line_length as usize;
        let bpp = (self.var.bits_per_pixel / 8).max(1) as usize;
        let idx = y as usize * line_length + x as usize * bpp;
        if idx < self.backing.len() {
            self.backing[idx] = value;
        }
    }

    /// Copy a rect from backing → mmap, applying the orientation transform.
    /// Called from `send_update` before the eink refresh ioctl.
    fn flush_rect(&mut self, top: u32, left: u32, width: u32, height: u32) {
        let line_length = self.fix.line_length as usize;
        let bpp = (self.var.bits_per_pixel / 8).max(1) as usize;
        let xres = self.var.xres;
        let yres = self.var.yres;
        // SAFETY: backing and mmap are non-overlapping; we own the mmap
        // until Drop. Both are sized to `smem_len`.
        let mmap = unsafe { std::slice::from_raw_parts_mut(self.mmap_ptr, self.mmap_len) };
        match self.orientation {
            Orientation::Up => {
                for y in 0..height {
                    let row_start = (top + y) as usize * line_length + left as usize * bpp;
                    let row_end = row_start + width as usize * bpp;
                    if row_end <= self.backing.len() && row_end <= mmap.len() {
                        mmap[row_start..row_end]
                            .copy_from_slice(&self.backing[row_start..row_end]);
                    }
                }
            }
            Orientation::Down => {
                // 180° rotation: pixel at user (x, y) lives in mmap at
                // physical (xres-1-x, yres-1-y). Reading the user-coord
                // row in forward order means writing the physical row in
                // reverse order.
                for y in 0..height {
                    let user_y = top + y;
                    if user_y >= yres {
                        continue;
                    }
                    let phys_y = yres - 1 - user_y;
                    let src_row = user_y as usize * line_length + left as usize * bpp;
                    let dst_row_end =
                        phys_y as usize * line_length + (xres - left) as usize * bpp;
                    for x in 0..width {
                        let user_x = left + x;
                        if user_x >= xres {
                            continue;
                        }
                        let src = src_row + x as usize * bpp;
                        let dst = dst_row_end - (x as usize + 1) * bpp;
                        if src + bpp <= self.backing.len() && dst + bpp <= mmap.len() {
                            for b in 0..bpp {
                                mmap[dst + b] = self.backing[src + b];
                            }
                        }
                    }
                }
            }
        }
    }

    /// Mutable view of the backing buffer. All UI drawing operates here in
    /// user-visible coords; nothing hits the panel until `send_update`.
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        &mut self.backing
    }

    /// Fill a rectangle with `value` (8bpp grayscale: 0=black, 255=white).
    /// Writes to backing in user coords — the orientation transform happens
    /// once per refresh inside `send_update`.
    pub fn fill_rect(&mut self, top: u32, left: u32, width: u32, height: u32, value: u8) {
        let line_length = self.fix.line_length as usize;
        let bpp_bytes = (self.var.bits_per_pixel / 8).max(1) as usize;
        let max_y = top.saturating_add(height);
        let max_x = left.saturating_add(width);
        for y in top..max_y {
            let row_offset = y as usize * line_length;
            let row_start = row_offset + left as usize * bpp_bytes;
            let row_end = row_offset + max_x as usize * bpp_bytes;
            if row_end <= self.backing.len() {
                self.backing[row_start..row_end].fill(value);
            }
        }
    }

    /// Trigger an eink refresh over the given rect with the given waveform.
    /// Returns the marker the driver will use for completion-wait calls.
    pub fn send_update(&mut self, rect: MxcfbRect, waveform: u32) -> Result<u32> {
        // Push the backing rect to mmap (with orientation transform) before
        // asking the driver to refresh that area.
        self.flush_rect(rect.top, rect.left, rect.width, rect.height);

        let marker = self.next_marker;
        self.next_marker = self.next_marker.wrapping_add(1).max(1);

        // The eink driver works in physical panel coords; map our user-coords
        // rect to where the matching pixels actually live in mmap.
        let (phys_top, phys_left) = self.phys_rect(rect.top, rect.left, rect.width, rect.height);
        let rect = MxcfbRect { top: phys_top, left: phys_left, width: rect.width, height: rect.height };

        // Build a v3 payload (88 bytes, zero-padded for v1/v2). Whatever
        // size the kernel encoded into the ioctl number, it reads only that
        // many bytes from our buffer — all of which we've initialized.
        let data = MxcfbUpdateDataV3 {
            v2: MxcfbUpdateDataV2 {
                update_region: rect,
                waveform_mode: waveform,
                update_mode: UPDATE_MODE_PARTIAL,
                update_marker: marker,
                temp: TEMP_USE_AMBIENT,
                flags: 0,
                dither_mode: 0,
                quant_bit: 0,
                alt_buffer_data: MxcfbAltBufferData::default(),
            },
            hist_bw_waveform_mode: 0,
            hist_gray_waveform_mode: 0,
            ts_pxp: 0,
            ts_epdc: 0,
        };

        // SAFETY: `data` is fully initialized #[repr(C)] = 88 bytes, exactly
        // the size the v3 ioctl number encodes.
        let res = unsafe { libc::ioctl(self.file.as_raw_fd(), MXCFB_SEND_UPDATE, &data as *const _) };
        if res != 0 {
            bail!("MXCFB_SEND_UPDATE: {}", io::Error::last_os_error());
        }
        Ok(marker)
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        if !self.mmap_ptr.is_null() && self.mmap_len > 0 {
            // SAFETY: we mmap'd this region in `open`; munmap once at Drop.
            unsafe {
                libc::munmap(self.mmap_ptr as *mut _, self.mmap_len);
            }
            self.mmap_ptr = ptr::null_mut();
            self.mmap_len = 0;
        }
    }
}
