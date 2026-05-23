//! Display surface — a real WM-managed X11 window (was raw `/dev/fb0`).
//!
//! Sidle draws through a fullscreen X11 window instead of mmap'ing `/dev/fb0`,
//! so the lab126 compositor *owns* the surface: it shows us fullscreen and,
//! crucially, recomposites the whole screen (home library + status bar) when
//! our window is torn down on exit — the kterm model. This removes the
//! windowless-exit bug (dead status bar) and the cvm freeze that used to mask
//! the framework drawing over us. See [[project_kual_statusbar_x11]] for why.
//!
//! The panel is 8bpp grayscale (depth 8, white=255 / black=0) — exactly our
//! backing-buffer format — so presenting is a 1:1 `PutImage` of the dirty rows,
//! no pixel conversion. `PutImage`s are chunked under the server's max request
//! length. Type/method names (`Framebuffer`, `MxcfbRect`, `send_update`) are
//! kept so the renderer is unchanged; eink refresh is now the X server's job,
//! so the waveform arg is accepted and ignored.

use anyhow::{Context, Result};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, BackingStore, ConnectionExt, CreateGCAux, CreateWindowAux, EventMask, Gcontext,
    ImageFormat, PropMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
// `change_property8` lives in the wrapper `ConnectionExt`.
use x11rb::wrapper::ConnectionExt as _;

use crate::orientation::Orientation;

// Waveform constants kept for call-site compatibility — the X server now picks
// the eink waveform, so `send_update` accepts and ignores these.
#[allow(dead_code)]
pub const WAVEFORM_MODE_INIT: u32 = 0;
pub const WAVEFORM_MODE_DU: u32 = 1;
pub const WAVEFORM_MODE_GC16: u32 = 2;

/// A rectangle to present, in screen coords. Name kept (`MxcfbRect`) so the
/// renderer call sites are unchanged; it's no longer an MXCFB struct.
#[derive(Default, Debug, Clone, Copy)]
pub struct MxcfbRect {
    pub top: u32,
    pub left: u32,
    pub width: u32,
    pub height: u32,
}

/// Minimal geometry, exposed as `fb.var.xres` / `fb.var.yres` like the old
/// fbdev `var`, so the renderer is unchanged.
pub struct Var {
    pub xres: u32,
    pub yres: u32,
}

pub struct Framebuffer {
    conn: RustConnection,
    win: Window,
    gc: Gcontext,
    depth: u8,
    pub var: Var,
    /// 8bpp grayscale, stride == `xres`. All drawing writes here; `send_update`
    /// `PutImage`s the dirty rows to the window.
    backing: Vec<u8>,
    /// Per-`PutImage` byte budget (server max request length minus header slack).
    max_req_bytes: usize,
    /// Accepted for API compatibility. The X server presents in screen
    /// orientation, so we render identity (no rotation) for now.
    #[allow(dead_code)]
    orientation: Orientation,
}

impl Framebuffer {
    /// Connect to the X server (`$DISPLAY`), create + map a fullscreen window.
    pub fn open(orientation: Orientation) -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None).context("connect to X ($DISPLAY)")?;
        let screen = conn.setup().roots[screen_num].clone();
        let xres = screen.width_in_pixels as u32;
        let yres = screen.height_in_pixels as u32;
        let depth = screen.root_depth;

        let win = conn.generate_id().context("generate_id window")?;
        conn.create_window(
            depth,
            win,
            screen.root,
            0,
            0,
            screen.width_in_pixels,
            screen.height_in_pixels,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(screen.white_pixel)
                .backing_store(BackingStore::ALWAYS)
                .event_mask(EventMask::EXPOSURE),
        )
        .context("create_window")?;

        // The lab126 WM reads the window name as a layout spec: Application
        // layer, no chrome, fullscreen (the booklet/KUAL shape).
        let name = b"L:A_N:application_ID:com.sidle.picker_PC:N_O:U";
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, name)
            .context("set WM_NAME")?;

        conn.map_window(win).context("map_window")?;

        let gc = conn.generate_id().context("generate_id gc")?;
        conn.create_gc(gc, win, &CreateGCAux::new()).context("create_gc")?;
        conn.flush().context("flush after map")?;

        let max_req_bytes = (conn.setup().maximum_request_length as usize)
            .saturating_mul(4)
            .max(4096);

        let backing = vec![0xFFu8; xres as usize * yres as usize];

        Ok(Self {
            conn,
            win,
            gc,
            depth,
            var: Var { xres, yres },
            backing,
            max_req_bytes,
            orientation,
        })
    }

    /// Single-pixel write in screen coords. Out-of-range silently no-ops.
    #[inline]
    pub fn put_pixel(&mut self, x: i32, y: i32, value: u8) {
        if x < 0 || y < 0 || x >= self.var.xres as i32 || y >= self.var.yres as i32 {
            return;
        }
        let idx = y as usize * self.var.xres as usize + x as usize;
        if idx < self.backing.len() {
            self.backing[idx] = value;
        }
    }

    /// Fill a rectangle with `value` (8bpp gray: 0=black, 255=white).
    pub fn fill_rect(&mut self, top: u32, left: u32, width: u32, height: u32, value: u8) {
        if left >= self.var.xres {
            return;
        }
        let stride = self.var.xres as usize;
        let max_y = top.saturating_add(height).min(self.var.yres);
        let max_x = left.saturating_add(width).min(self.var.xres);
        for y in top..max_y {
            let row = y as usize * stride;
            let s = row + left as usize;
            let e = row + max_x as usize;
            if e <= self.backing.len() {
                self.backing[s..e].fill(value);
            }
        }
    }

    /// Present the dirty rows. We widen each update to full rows: depth-8
    /// ZPixmap needs 32-bit scanline padding, and `xres` (1264) is already
    /// 4-byte aligned, so a full-width band is a contiguous backing slice with
    /// no per-scanline padding. Chunked under the server's max request length.
    /// Waveform ignored — the X server drives the eink refresh.
    pub fn send_update(&mut self, rect: MxcfbRect, _waveform: u32) -> Result<u32> {
        let stride = self.var.xres as usize;
        let width = self.var.xres as u16;
        let top = rect.top.min(self.var.yres);
        let bottom = rect.top.saturating_add(rect.height).min(self.var.yres);
        let max_rows = (self.max_req_bytes.saturating_sub(64) / stride.max(1)).max(1);

        let mut y = top;
        while y < bottom {
            let h = ((bottom - y) as usize).min(max_rows);
            let s = y as usize * stride;
            let e = s + h * stride;
            self.conn
                .put_image(
                    ImageFormat::Z_PIXMAP,
                    self.win,
                    self.gc,
                    width,
                    h as u16,
                    0,
                    y as i16,
                    0,
                    self.depth,
                    &self.backing[s..e],
                )
                .context("put_image")?;
            y += h as u32;
        }
        self.conn.flush().context("flush")?;
        Ok(0)
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        // Destroy the window so the WM recomposites the screen underneath (home
        // library + status bar repaint). Best effort — Drop can't propagate.
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.flush();
    }
}
