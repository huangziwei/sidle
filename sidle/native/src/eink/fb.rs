//! Display surface — a real WM-managed X11 window (was raw `/dev/fb0`).

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use x11rb::connection::Connection;
// `maximum_request_bytes` (BIG-REQUESTS-aware) lives on this trait.
use x11rb::connection::RequestConnection as _;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, CreateGCAux, CreateWindowAux, EventMask, Gcontext, ImageFormat,
    ImageOrder, PropMode, Screen, Visibility, Window, WindowClass,
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

/// `events` folded into a [`Pump`] against `covered` and the `size` drawn. The
/// last event wins, an unchanged state answers `None`, and `VisibilityNotify`
/// sets `covered` either way where a [`SCREENSAVER_MESSAGE`] sets it true alone.
fn fold(events: &[Event], screensaver: Atom, covered: bool, size: (u32, u32)) -> Pump {
    let mut pump = Pump::default();
    let mut folded = covered;
    for event in events {
        match event {
            Event::Expose(_) => pump.repaint = true,
            // `ConfigureNotify` carries the size the window is laid out at.
            Event::ConfigureNotify(ev) => {
                let laid = (u32::from(ev.width), u32::from(ev.height));
                pump.resized = (laid != size && laid.0 > 0 && laid.1 > 0).then_some(laid);
            }
            // `FULLY_OBSCURED` is the whole panel; anything less is a share of it.
            Event::VisibilityNotify(ev) => folded = ev.state == Visibility::FULLY_OBSCURED,
            // `data8[0]` of 1 covers; a 0 leaves `folded` alone.
            Event::ClientMessage(ev) if screensaver != 0 && ev.type_ == screensaver => {
                folded |= ev.data.as_data8()[0] != 0;
            }
            Event::Error(e) => {
                // A dropped update leaves the panel stale, so treat it as damage
                // too — retrying costs one repaint and may well succeed, where
                // doing nothing certainly stays wrong.
                eprintln!("x11: WARNING request failed: {e:?}");
                pump.repaint = true;
            }
            _ => {}
        }
    }
    if folded != covered {
        pump.covered = Some(folded);
    }
    pump
}

/// `pixel_bytes` rounded up to a multiple of `pad`: the bytes `put_image` takes
/// per ZPixmap scanline. A row whose pixels do not fill a whole multiple is
/// padded out, and a server reading an unpadded row shears every row after the
/// first.
fn wire_stride(pixel_bytes: usize, pad: usize) -> usize {
    let pad = pad.max(1);
    pixel_bytes.div_ceil(pad) * pad
}

/// `band`'s packed-RGB rows, `xres` wide, into `wire`: one `bpp`-byte wire pixel
/// each, rows `wire_stride` apart with the pad left at 0xFF. `bpp == 1` collapses
/// to one luma byte; wider scatters R/G/B to `chan`.
fn pack_band(
    wire: &mut Vec<u8>,
    band: &[u8],
    xres: usize,
    bpp: usize,
    wire_stride: usize,
    chan: [usize; 3],
) {
    wire.clear();
    let bk_stride = xres * CH;
    if bk_stride == 0 || wire_stride == 0 {
        return;
    }
    let pixel_bytes = xres * bpp;
    let [rb, gb, bb] = chan;
    wire.resize(band.len() / bk_stride * wire_stride, 0xFF);
    for (out, row) in wire
        .chunks_exact_mut(wire_stride)
        .zip(band.chunks_exact(bk_stride))
    {
        let (triples, _) = row.as_chunks::<CH>();
        let pairs = out[..pixel_bytes].chunks_exact_mut(bpp).zip(triples);
        if bpp == 1 {
            for (w, rgb) in pairs {
                w[0] = luma(rgb[0], rgb[1], rgb[2]);
            }
        } else {
            for (w, rgb) in pairs {
                w[rb] = rgb[0];
                w[gb] = rgb[1];
                w[bb] = rgb[2];
            }
        }
    }
}

/// A rectangle to present, in screen coords. Despite the name it is not a
/// kernel ABI struct and must not be handed to an ioctl.
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

/// What [`Framebuffer::pump_events`] answers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pump {
    /// Set by `Expose` and by `Event::Error`.
    pub repaint: bool,
    /// `Some(true)` covered, `Some(false)` uncovered, `None` unchanged.
    pub covered: Option<bool>,
    /// A `ConfigureNotify` size differing from the one being drawn.
    pub resized: Option<(u32, u32)>,
}

/// How long [`Framebuffer::open`] waits for `MapNotify`.
const LAYOUT_WAIT: Duration = Duration::from_millis(500);

/// Atom naming the message a `CMS~E:ss` `WM_NAME` subscribes to.
const SCREENSAVER_MESSAGE: &[u8] = b"lab126_screen_saver";

pub struct Framebuffer {
    conn: RustConnection,
    win: Window,
    gc: Gcontext,
    depth: u8,
    /// Server wire bytes per pixel for `depth` (from `pixmap_formats`): 1 on a
    /// depth-8 panel, 4 on depth-24/32. `send_update` converts the RGB backing
    /// to this width.
    bytes_per_pixel: usize,
    /// Wire bytes per scanline, from [`wire_stride`] and `scanline_pad`.
    wire_stride: usize,
    /// The format's `scanline_pad`, in bytes: [`wire_stride`]'s second half.
    scanline_pad: usize,
    /// Byte offset of the R, G, B channels within a `bytes_per_pixel`-wide wire
    chan: [usize; 3],
    pub var: Var,
    /// Packed RGB ([`CH`] bytes/pixel), stride == `xres * CH`. All drawing writes
    /// here; `send_update` `PutImage`s the dirty rows to the window in the wire
    /// format.
    backing: Vec<u8>,
    /// Per-`PutImage` byte budget (server max request length minus header slack).
    max_req_bytes: usize,
    /// The interned [`SCREENSAVER_MESSAGE`] atom, or 0.
    screensaver: Atom,
    /// Whether another window covers this one. [`fold`] reports its changes.
    covered: bool,
}

impl Framebuffer {
    /// Connect to the X server (`$DISPLAY`), create + map a fullscreen window.
    pub fn open() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None).context("connect to X ($DISPLAY)")?;
        let screen = conn.setup().roots[screen_num].clone();
        // The root size, until `get_geometry` answers below.
        let mut xres = screen.width_in_pixels as u32;
        let mut yres = screen.height_in_pixels as u32;
        let depth = screen.root_depth;
        let format = conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth);
        // Wire bytes per pixel the server expects for this depth. Depth 8 → 1;
        // depth 24/32 → 4 (X pads 24-bit pixels to 32). Looked up rather than
        // assumed so `send_update` adapts to whatever the panel's X exposes.
        let bytes_per_pixel = format
            .map(|f| (f.bits_per_pixel as usize / 8).max(1))
            .unwrap_or(1);
        // `scanline_pad` is 32 bits on every standard format.
        let scanline_pad = format.map(|f| f.scanline_pad as usize / 8).unwrap_or(4);
        // Channel byte offsets for the color wire format, from the root visual's
        // RGB masks (so we honour BGRX vs RGBX rather than guessing). Falls back
        // to BGRX little-endian, the usual lab126 depth-24 layout.
        let chan = wire_channels(&conn, &screen, bytes_per_pixel).unwrap_or([2, 1, 0]);
        // stderr → sidle.sh's log: confirms geometry + the format we picked.
        eprintln!(
            "fb: xres={xres} yres={yres} depth={depth} bytes_per_pixel={bytes_per_pixel} \
             scanline_pad={scanline_pad} chan=[{},{},{}] root_visual=0x{:x}",
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
            // No `backing_store`. Asking for it costs panel updates on this
            // `STRUCTURE_NOTIFY` carries `MapNotify` and `ConfigureNotify`, and
            // `VISIBILITY_CHANGE` a window put over this one.
            &CreateWindowAux::new()
                .background_pixel(screen.white_pixel)
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::VISIBILITY_CHANGE
                        | EventMask::STRUCTURE_NOTIFY,
                ),
        )
        .context("create_window")?;

        // The lab126 WM reads the window name as a layout spec: Application
        // layer, no chrome, fullscreen (the booklet/KUAL shape). `CMS~E:ss`
        // subscribes to [`SCREENSAVER_MESSAGE`].
        let name = b"L:A_N:application_ID:com.sidle.picker_PC:N_O:U_CMS~E:ss";
        conn.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            name,
        )
        .context("set WM_NAME")?;

        conn.map_window(win).context("map_window")?;

        let gc = conn.generate_id().context("generate_id gc")?;
        conn.create_gc(gc, win, &CreateGCAux::new())
            .context("create_gc")?;
        conn.flush().context("flush after map")?;

        // `MapNotify` marks the layout done. Ahead of it `get_geometry` answers
        // with the size that was asked for, not the one the WM laid out.
        let deadline = Instant::now() + LAYOUT_WAIT;
        let mut mapped = false;
        while !mapped && Instant::now() < deadline {
            while let Ok(Some(event)) = conn.poll_for_event() {
                mapped |= matches!(event, Event::MapNotify(_));
            }
            if !mapped {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        // What the WM gave us. `get_geometry` outranks the root read above:
        // drawing at a size the window does not have clips every edge-anchored
        // thing on the screen.
        match conn
            .get_geometry(win)
            .map_err(|e| e.to_string())
            .and_then(|c| c.reply().map_err(|e| e.to_string()))
        {
            Ok(g) if u32::from(g.width) != xres || u32::from(g.height) != yres => {
                eprintln!(
                    "fb: window is {}x{}, root read {xres}x{yres} — drawing {}x{}",
                    g.width, g.height, g.width, g.height
                );
                (xres, yres) = (u32::from(g.width), u32::from(g.height));
            }
            Ok(_) => {}
            Err(e) => eprintln!("fb: could not read window geometry: {e}"),
        }

        // `wire_stride` follows `xres`, which `get_geometry` above sets.
        let wire_stride = wire_stride(xres as usize * bytes_per_pixel, scanline_pad);

        // Ask the connection, not the setup block. `setup().maximum_request_length`
        let max_req_bytes = conn.maximum_request_bytes().max(4096);
        eprintln!(
            "fb: mapped={mapped} drawing {xres}x{yres} stride={wire_stride} \
             max request {max_req_bytes} bytes ({} rows/band)",
            max_req_bytes / wire_stride.max(1),
        );

        // `only_if_exists` false creates the atom. [`fold`] matches no
        // `ClientMessage` against the 0 an unanswered reply leaves.
        let screensaver = conn
            .intern_atom(false, SCREENSAVER_MESSAGE)
            .map_err(|e| e.to_string())
            .and_then(|c| c.reply().map_err(|e| e.to_string()))
            .map(|r| r.atom)
            .unwrap_or_else(|e| {
                eprintln!("fb: could not intern lab126_screen_saver: {e}");
                0
            });

        let backing = vec![0xFFu8; xres as usize * yres as usize * CH];

        Ok(Self {
            conn,
            win,
            gc,
            depth,
            bytes_per_pixel,
            wire_stride,
            scanline_pad,
            chan,
            var: Var { xres, yres },
            backing,
            max_req_bytes,
            screensaver,
            covered: false,
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

    /// Drain the X event queue, through [`fold`]. A reported resize is applied
    /// before it is answered, so `var` and `backing` match the screen the
    /// caller is about to draw.
    pub fn pump_events(&mut self) -> Pump {
        let size = (self.var.xres, self.var.yres);
        let mut events = Vec::new();
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            events.push(event);
        }
        let pump = fold(&events, self.screensaver, self.covered, size);
        if let Some(covered) = pump.covered {
            self.covered = covered;
        }
        if let Some((w, h)) = pump.resized {
            self.resize(w, h);
        }
        pump
    }

    /// Draws `w` by `h`: `var`, `backing` and `wire_stride` follow, and
    /// `backing` is white.
    fn resize(&mut self, w: u32, h: u32) {
        eprintln!(
            "fb: laid out {w}x{h}, was {}x{}",
            self.var.xres, self.var.yres
        );
        self.var = Var { xres: w, yres: h };
        self.backing = vec![0xFFu8; w as usize * h as usize * CH];
        self.wire_stride = wire_stride(w as usize * self.bytes_per_pixel, self.scanline_pad);
    }

    /// Whether another window covers this one.
    pub fn covered(&self) -> bool {
        self.covered
    }

    /// The X connection's descriptor, for `poll(2)`.
    /// [`Framebuffer::pump_events`] drains what lands on it.
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.conn.stream().as_raw_fd()
    }

    /// Present the dirty rows, converting the RGB backing to the wire pixel
    pub fn send_update(&mut self, rect: MxcfbRect, _waveform: u32) -> Result<u32> {
        let bpp = self.bytes_per_pixel;
        let xres = self.var.xres as usize;
        let bk_stride = xres * CH; // backing bytes per scanline (RGB)
        let wire_stride = self.wire_stride; // wire bytes per scanline, padded
        let width = self.var.xres as u16;
        let top = rect.top.min(self.var.yres);
        let bottom = rect.top.saturating_add(rect.height).min(self.var.yres);
        let max_rows = (self.max_req_bytes.saturating_sub(64) / wire_stride.max(1)).max(1);

        // Scratch reused across bands: the backing RGB converted to the wire
        // pixel format. Pad bytes (depth-24/32) stay at the 0xFF fill.
        let mut wire: Vec<u8> = Vec::new();

        let mut y = top;
        while y < bottom {
            let h = ((bottom - y) as usize).min(max_rows);
            let s = y as usize * bk_stride;
            let e = s + h * bk_stride;
            pack_band(
                &mut wire,
                &self.backing[s..e],
                xres,
                bpp,
                wire_stride,
                self.chan,
            );

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
        // Round-trip, not a bare flush. `flush` only guarantees the bytes left
        self.conn
            .get_input_focus()
            .context("sync round-trip")?
            .reply()
            .context("sync reply")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::{
        ClientMessageEvent, ConfigureNotifyEvent, ExposeEvent, VisibilityNotifyEvent,
    };

    /// The atom [`message`] sends under, and the one a stray message uses.
    const SCREENSAVER: Atom = 42;
    const OTHER: Atom = 43;

    fn configure(w: u16, h: u16) -> Event {
        Event::ConfigureNotify(ConfigureNotifyEvent {
            width: w,
            height: h,
            ..Default::default()
        })
    }

    fn expose() -> Event {
        Event::Expose(ExposeEvent::default())
    }

    fn visibility(state: Visibility) -> Event {
        Event::VisibilityNotify(VisibilityNotifyEvent {
            state,
            ..Default::default()
        })
    }

    fn message(type_: Atom, up: u8) -> Event {
        Event::ClientMessage(ClientMessageEvent::new(
            8,
            0u32,
            type_,
            [up, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        ))
    }

    #[test]
    fn nothing_drained_changes_nothing() {
        assert_eq!(fold(&[], SCREENSAVER, false, (100, 200)), Pump::default());
    }

    #[test]
    fn either_signal_covers_the_window() {
        let by_message = fold(&[message(SCREENSAVER, 1)], SCREENSAVER, false, (100, 200));
        assert_eq!(by_message.covered, Some(true));
        let by_visibility = fold(
            &[visibility(Visibility::FULLY_OBSCURED)],
            SCREENSAVER,
            false,
            (100, 200),
        );
        assert_eq!(by_visibility.covered, Some(true));
    }

    #[test]
    fn only_visibility_uncovers() {
        // A screensaver message of 0 leaves a covered window covered.
        assert_eq!(
            fold(&[message(SCREENSAVER, 0)], SCREENSAVER, true, (100, 200)).covered,
            None
        );
        assert_eq!(
            fold(
                &[visibility(Visibility::UNOBSCURED)],
                SCREENSAVER,
                true,
                (100, 200)
            )
            .covered,
            Some(false)
        );
    }

    #[test]
    fn a_partly_covered_window_keeps_the_screen() {
        assert_eq!(
            fold(
                &[visibility(Visibility::PARTIALLY_OBSCURED)],
                SCREENSAVER,
                false,
                (100, 200)
            )
            .covered,
            None
        );
    }

    #[test]
    fn a_message_under_another_atom_is_ignored() {
        assert_eq!(
            fold(&[message(OTHER, 1)], SCREENSAVER, false, (100, 200)).covered,
            None
        );
        // An atom that never interned matches nothing at all.
        assert_eq!(fold(&[message(0, 1)], 0, false, (100, 200)).covered, None);
    }

    #[test]
    fn only_a_new_layout_is_reported() {
        assert_eq!(
            fold(&[configure(100, 200)], SCREENSAVER, false, (100, 200)).resized,
            None
        );
        assert_eq!(
            fold(&[configure(300, 400)], SCREENSAVER, false, (100, 200)).resized,
            Some((300, 400))
        );
        // A zero dimension is the window being unmapped, not a layout.
        assert_eq!(
            fold(&[configure(0, 400)], SCREENSAVER, false, (100, 200)).resized,
            None
        );
    }

    #[test]
    fn the_last_layout_of_a_drain_wins() {
        let pump = fold(
            &[configure(300, 400), configure(500, 600), expose()],
            SCREENSAVER,
            false,
            (100, 200),
        );
        assert_eq!(pump.resized, Some((500, 600)));
        assert!(pump.repaint);
    }

    #[test]
    fn a_scanline_reaches_the_pad() {
        // 758 px at one byte each is the Paperwhite's depth-8 row: 758 bytes of
        // pixels in a 760-byte scanline.
        assert_eq!(wire_stride(758, 4), 760);
        assert_eq!(wire_stride(760, 4), 760);
        assert_eq!(wire_stride(1860, 4), 1860);
        assert_eq!(wire_stride(1264 * 4, 4), 1264 * 4);
    }

    #[test]
    fn a_pad_of_zero_is_one_byte() {
        assert_eq!(wire_stride(758, 0), 758);
        assert_eq!(wire_stride(0, 4), 0);
    }

    #[test]
    fn a_short_row_keeps_its_pad() {
        // Three grey pixels, depth 8, in a 4-byte scanline: the fourth byte is
        // the 0xFF fill and never a pixel.
        let mut wire = Vec::new();
        let band = [10, 10, 10, 20, 20, 20, 30, 30, 30];
        pack_band(&mut wire, &band, 3, 1, 4, [0, 1, 2]);
        assert_eq!(wire, [10, 20, 30, 0xFF]);
    }

    #[test]
    fn a_wide_pixel_scatters_to_its_channels() {
        // One BGRX pixel: R at 2, G at 1, B at 0, the fourth byte padding.
        let mut wire = Vec::new();
        pack_band(&mut wire, &[1, 2, 3], 1, 4, 4, [2, 1, 0]);
        assert_eq!(wire, [3, 2, 1, 0xFF]);
    }

    #[test]
    fn two_rows_land_a_stride_apart() {
        let mut wire = Vec::new();
        let band = [0, 0, 0, 255, 255, 255];
        pack_band(&mut wire, &band, 1, 1, 4, [0, 1, 2]);
        assert_eq!(wire, [0, 0xFF, 0xFF, 0xFF, 255, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn no_columns_packs_nothing() {
        let mut wire = Vec::new();
        pack_band(&mut wire, &[], 0, 1, 4, [0, 1, 2]);
        assert!(wire.is_empty());
    }
}
