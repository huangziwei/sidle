//! Display surface — a real WM-managed X11 window (was raw `/dev/fb0`).
//!
//! Sidle draws through a fullscreen X11 window instead of mmap'ing `/dev/fb0`,
//! so the lab126 compositor *owns* the surface: it shows us fullscreen and,
//! crucially, recomposites the whole screen (home library + status bar) when
//! our window is torn down on exit — the kterm model. This removes the
//! windowless-exit bug (dead status bar) and the cvm freeze that used to mask
//! the framework drawing over us. See [[project_kual_statusbar_x11]].
//!
//! The compositor ALSO auto-rotates our window 180° to the framework
//! orientation (page-bezel side), so we render identity here and never rotate
//! pixels ourselves — doing so would double-rotate. Touch/buttons are read raw
//! from evdev (panel-fixed), so the main loop re-orients *those* on rotation;
//! see [`crate::eink::input`]. The renderer draws into a packed-RGB backing
//! ([`CH`] bytes/pixel, white=255). The chrome (text, frames, bands) writes gray
//! via [`Framebuffer::put_pixel`] / [`Framebuffer::fill_rect`] (R=G=B), while
//! cover art writes true color via [`Framebuffer::put_pixel_rgb`] — so the
//! Colorsoft shows color covers under a grayscale UI.
//!
//! How the backing reaches the server depends on the window depth: a color panel
//! (Colorsoft — `/dev/fb0` is 32bpp, X runs depth 24/32) takes the RGB reordered
//! to the visual's channel masks plus a pad byte; a grayscale panel (KOA2, depth
//! 8) takes a per-pixel luma collapse (a gray UI pixel passes through exactly;
//! the rare color cover desaturates). `send_update` does that conversion per
//! band. The dirty rows are chunked under the server's max request length. Type/
//! method names (`Framebuffer`, `MxcfbRect`, `send_update`) are kept so the
//! renderer is unchanged; the X server drives the eink refresh, so the waveform
//! is ignored.

use std::path::Path;

use anyhow::{Context, Result};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, BackingStore, ConnectionExt, CreateGCAux, CreateWindowAux, EventMask, Gcontext,
    ImageFormat, ImageOrder, PropMode, Screen, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
// `change_property8` lives in the wrapper `ConnectionExt`.
use x11rb::wrapper::ConnectionExt as _;

// Waveform constants kept for call-site compatibility — the X server now picks
// the eink waveform, so `send_update` accepts and ignores these.
#[allow(dead_code)]
pub const WAVEFORM_MODE_INIT: u32 = 0;
pub const WAVEFORM_MODE_DU: u32 = 1;
pub const WAVEFORM_MODE_GC16: u32 = 2;

/// Bytes per pixel in the backing store: packed RGB (no alpha). The wire format
/// is derived per-depth in `send_update` (luma for depth-8, masked RGBX for
/// depth-24/32), so the backing stays a compact device-independent RGB.
pub const CH: usize = 3;

/// Rec. 601 luma of an RGB pixel (the depth-8 wire collapse). A gray UI pixel
/// (R=G=B) maps to itself exactly; a color cover desaturates. `>> 8` with these
/// weights summing to 256 keeps it an integer multiply-shift.
#[inline]
fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((r as u32 * 77 + g as u32 * 150 + b as u32 * 29) >> 8) as u8
}

/// Resolve the R/G/B byte offsets within a `bpp`-wide wire pixel from the root
/// visual's colour masks, honouring the server image byte order. `None` when
/// the format is sub-RGB (depth-8 — that path collapses to luma, so the masks
/// don't matter) or the visual/masks can't be read, letting the caller fall
/// back to a default layout.
fn wire_channels(conn: &RustConnection, screen: &Screen, bpp: usize) -> Option<[usize; 3]> {
    if bpp < 3 {
        return None;
    }
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == screen.root_visual)?;
    if visual.red_mask == 0 || visual.green_mask == 0 || visual.blue_mask == 0 {
        return None;
    }
    let msb = conn.setup().image_byte_order == ImageOrder::MSB_FIRST;
    // A channel's mask sits in one byte of the native-endian pixel; its byte
    // index is the mask's trailing-zero count / 8. MSBFirst wire order mirrors
    // that index across the pixel width.
    let offset = |mask: u32| -> usize {
        let idx = (mask.trailing_zeros() / 8) as usize;
        if msb { bpp - 1 - idx } else { idx }
    };
    Some([
        offset(visual.red_mask),
        offset(visual.green_mask),
        offset(visual.blue_mask),
    ])
}

/// A rectangle to present, in screen coords. Name kept (`MxcfbRect`) so the
/// renderer call sites are unchanged; it's no longer an MXCFB struct.
#[derive(Default, Debug, Clone, Copy)]
pub struct MxcfbRect {
    pub top: u32,
    #[allow(dead_code)]
    pub left: u32,
    #[allow(dead_code)]
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
    /// Server wire bytes per pixel for `depth` (from `pixmap_formats`): 1 on a
    /// depth-8 panel, 4 on depth-24/32. `send_update` converts the RGB backing
    /// to this width.
    bytes_per_pixel: usize,
    /// Byte offset of the R, G, B channels within a `bytes_per_pixel`-wide wire
    /// pixel, derived from the visual's colour masks under the server image byte
    /// order. Typical depth-24 little-endian is BGRX → `[2, 1, 0]`. Unused on
    /// depth-8 (that path collapses to luma).
    chan: [usize; 3],
    pub var: Var,
    /// Packed RGB ([`CH`] bytes/pixel), stride == `xres * CH`. All drawing writes
    /// here; `send_update` `PutImage`s the dirty rows to the window in the wire
    /// format.
    backing: Vec<u8>,
    /// Per-`PutImage` byte budget (server max request length minus header slack).
    max_req_bytes: usize,
}

impl Framebuffer {
    /// Connect to the X server (`$DISPLAY`), create + map a fullscreen window.
    pub fn open() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None).context("connect to X ($DISPLAY)")?;
        let screen = conn.setup().roots[screen_num].clone();
        let xres = screen.width_in_pixels as u32;
        let yres = screen.height_in_pixels as u32;
        let depth = screen.root_depth;
        // Wire bytes per pixel the server expects for this depth. Depth 8 → 1;
        // depth 24/32 → 4 (X pads 24-bit pixels to 32). Looked up rather than
        // assumed so `send_update` adapts to whatever the panel's X exposes.
        let bytes_per_pixel = conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .map(|f| (f.bits_per_pixel as usize / 8).max(1))
            .unwrap_or(1);
        // Channel byte offsets for the color wire format, from the root visual's
        // RGB masks (so we honour BGRX vs RGBX rather than guessing). Falls back
        // to BGRX little-endian, the usual lab126 depth-24 layout.
        let chan = wire_channels(&conn, &screen, bytes_per_pixel).unwrap_or([2, 1, 0]);
        // stderr → sidle.sh's log: confirms geometry + the format we picked.
        eprintln!(
            "fb: xres={xres} yres={yres} depth={depth} bytes_per_pixel={bytes_per_pixel} \
             chan=[{},{},{}] root_visual=0x{:x}",
            chan[0], chan[1], chan[2], screen.root_visual,
        );

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

        let backing = vec![0xFFu8; xres as usize * yres as usize * CH];

        Ok(Self {
            conn,
            win,
            gc,
            depth,
            bytes_per_pixel,
            chan,
            var: Var { xres, yres },
            backing,
            max_req_bytes,
        })
    }

    /// Single gray-pixel write in screen coords (0=black, 255=white), stored as
    /// `(v,v,v)`. Out-of-range silently no-ops.
    #[inline]
    pub fn put_pixel(&mut self, x: i32, y: i32, value: u8) {
        self.put_pixel_rgb(x, y, [value, value, value]);
    }

    /// Single color-pixel write in screen coords, `[r, g, b]`. Used for cover
    /// art; the chrome uses [`put_pixel`](Self::put_pixel). Out-of-range no-ops.
    #[inline]
    pub fn put_pixel_rgb(&mut self, x: i32, y: i32, rgb: [u8; 3]) {
        if x < 0 || y < 0 || x >= self.var.xres as i32 || y >= self.var.yres as i32 {
            return;
        }
        let idx = (y as usize * self.var.xres as usize + x as usize) * CH;
        if idx + CH <= self.backing.len() {
            self.backing[idx..idx + CH].copy_from_slice(&rgb);
        }
    }

    /// Fill a rectangle with gray `value` (0=black, 255=white). A gray fill is
    /// `(v,v,v)`, so every backing byte in the span is `value` — a single memset
    /// over the `CH`-wide range stays correct and fast.
    pub fn fill_rect(&mut self, top: u32, left: u32, width: u32, height: u32, value: u8) {
        if left >= self.var.xres {
            return;
        }
        let stride = self.var.xres as usize * CH;
        let max_y = top.saturating_add(height).min(self.var.yres);
        let max_x = left.saturating_add(width).min(self.var.xres);
        for y in top..max_y {
            let row = y as usize * stride;
            let s = row + left as usize * CH;
            let e = row + max_x as usize * CH;
            if e <= self.backing.len() {
                self.backing[s..e].fill(value);
            }
        }
    }

    /// Present the dirty rows, converting the RGB backing to the wire pixel
    /// format per band. Depth-8 collapses each pixel to a luma byte; depth-24/32
    /// places R/G/B at the visual's channel offsets with the remaining byte left
    /// 0xFF (opaque pad). Each wire scanline is a whole number of pixels and
    /// `xres` (1264) keeps both widths (×1 and ×4) 4-byte aligned, so ZPixmap's
    /// 32-bit scanline padding needs no extra slack. Chunked under the server's
    /// max request length. Identity (the X server rotates the window); waveform
    /// ignored.
    pub fn send_update(&mut self, rect: MxcfbRect, _waveform: u32) -> Result<u32> {
        let bpp = self.bytes_per_pixel;
        let xres = self.var.xres as usize;
        let bk_stride = xres * CH; // backing bytes per scanline (RGB)
        let wire_stride = xres * bpp; // wire bytes per scanline
        let width = self.var.xres as u16;
        let top = rect.top.min(self.var.yres);
        let bottom = rect.top.saturating_add(rect.height).min(self.var.yres);
        let max_rows = (self.max_req_bytes.saturating_sub(64) / wire_stride.max(1)).max(1);
        let [rb, gb, bb] = self.chan;

        // Scratch reused across bands: the backing RGB converted to the wire
        // pixel format. Pad bytes (depth-24/32) stay at the 0xFF fill.
        let mut wire: Vec<u8> = Vec::new();

        let mut y = top;
        while y < bottom {
            let h = ((bottom - y) as usize).min(max_rows);
            let s = y as usize * bk_stride;
            let e = s + h * bk_stride;
            let band = &self.backing[s..e];
            let px = h * xres;

            wire.clear();
            wire.resize(px * bpp, 0xFF);
            // Walk source RGB triples against destination wire pixels in
            // lockstep. depth-8 (`bpp == 1`) collapses to one luma byte;
            // depth-24/32 scatters R/G/B to the visual's channel offsets, the
            // remaining byte left at the 0xFF pad.
            let pairs = wire.chunks_exact_mut(bpp).zip(band.chunks_exact(CH));
            if bpp == 1 {
                for (w, src) in pairs {
                    w[0] = luma(src[0], src[1], src[2]);
                }
            } else {
                for (w, src) in pairs {
                    w[rb] = src[0];
                    w[gb] = src[1];
                    w[bb] = src[2];
                }
            }

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
                    &wire,
                )
                .context("put_image")?;
            y += h as u32;
        }
        self.conn.flush().context("flush")?;
        Ok(0)
    }

    /// Clone the backing buffer — the exact packed-RGB image currently on
    /// screen. Used to save a screenshot and to restore the screen after the
    /// capture flash overwrites it.
    pub fn backing_snapshot(&self) -> Vec<u8> {
        self.backing.clone()
    }

    /// Restore a previously snapshotted backing buffer. No-op on a size
    /// mismatch (a rotation between snapshot and restore would change `xres`).
    /// The caller still has to `send_update` to present it.
    pub fn restore_backing(&mut self, snap: Vec<u8>) {
        if snap.len() == self.backing.len() {
            self.backing = snap;
        }
    }

    /// Encode the current backing (packed RGB, white=255) as a PNG at `path`.
    /// No rotation: the backing is the upright UI as rendered — we draw identity
    /// and the lab126 compositor rotates the *display* to the grip, so the
    /// encoded file already matches what the user saw in either orientation.
    /// (Pre-X11 we rotated raw `/dev/fb0` writes ourselves and undid that here;
    /// the pre-rotation is gone, so undoing it now would flip the file upside
    /// down — that was the screenshot-bug.) The `png` encoder ships with the
    /// `image` dep (pure Rust).
    pub fn capture_png(&self, path: &Path) -> Result<()> {
        let img = image::RgbImage::from_raw(self.var.xres, self.var.yres, self.backing.clone())
            .context("backing buffer size != xres*yres*CH")?;
        img.save(path)
            .with_context(|| format!("write screenshot {}", path.display()))?;
        Ok(())
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
