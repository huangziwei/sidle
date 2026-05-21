//! evdev touchscreen reader.
//!
//! Multi-touch protocol B: the kernel emits a stream of `input_event`
//! records with absolute X/Y plus a tracking-ID lifecycle. We surface
//! two boundary events per contact — `Down` (finger lands) and `Up`
//! (finger lifts) — so the long-press path can time the gap. Move
//! events between the boundaries silently update `cur_x/cur_y`;
//! pinch/scroll are out of scope.
//!
//! Per-contact wire ordering inside one `SYN_REPORT` packet:
//!   ABS_MT_SLOT 0
//!   ABS_MT_TRACKING_ID <id≥0>   ← contact begins (Down packet)
//!   ABS_MT_POSITION_X / Y
//!   SYN_REPORT
//!   …move packets…
//!   ABS_MT_SLOT 0
//!   ABS_MT_TRACKING_ID -1       ← contact ends (Up packet)
//!   SYN_REPORT
//!
//! We collect `down_pending` / `up_pending` flags as the events stream
//! in and emit the boundary at `SYN_REPORT` so the position fields are
//! correctly populated for the matching event.
//!
//! Device discovery: `/proc/bus/input/devices` is text; we match the
//! device whose Name field contains "touch", "cyttsp" (KOA2's driver),
//! or "zforce" (older Kindles) and extract its `eventN` handler. No
//! EVIOCGNAME ioctl needed.
//!
//! Wire format: on the KOA2's kernel (4.1.15, 32-bit ARM), each event is
//! 16 bytes — `struct timeval` is 8 bytes, then u16 type, u16 code,
//! i32 value. We parse from raw bytes so the host (cargo check on macOS,
//! where libc::c_long is 8 bytes) and the target stay byte-compatible.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::orientation::Orientation;

// evdev type/code constants (linux/input-event-codes.h). Stable.
const EV_SYN: u16 = 0x00;
const SYN_REPORT: u16 = 0x00;
const EV_ABS: u16 = 0x03;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;

const EVENT_BYTES: usize = 16;

/// Boundary touch events surfaced to the main loop. `Down` fires when
/// the user's finger first lands; `Up` fires when it lifts. Move
/// events between the two update internal `cur_x/cur_y` but don't
/// emit — for v1 of long-press, only the timing between Down and Up
/// matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchEvent {
    Down { x: u32, y: u32 },
    Up { x: u32, y: u32 },
}

// _IOW('E', 0x90, int): direction(W=1)<<30 | size(4)<<16 | type('E'=0x45)<<8 | nr(0x90)
// = 0x40000000 | 0x40000 | 0x4500 | 0x90 = 0x40044590
const EVIOCGRAB: libc::c_int = 0x40044590;

pub struct Touch {
    file: File,
    cur_x: i32,
    cur_y: i32,
    /// Set when a new contact starts (`ABS_MT_TRACKING_ID >= 0`) and
    /// cleared when the matching `SYN_REPORT` flushes a `Down` event.
    /// Persists across `next_event` calls because Down/Up live in
    /// different packets — the state machine straddles boundaries.
    down_pending: bool,
    up_pending: bool,
    /// Once grabbed, no other reader (framework included) sees events from
    /// this device. Belt-and-braces alongside the framework SIGSTOP.
    grabbed: bool,
    /// Same orientation the framebuffer was opened with. We mirror the
    /// raw touch coords by the same amount so caller-visible coords match
    /// what's drawn on screen.
    orientation: Orientation,
    fb_xres: u32,
    fb_yres: u32,
}

impl Touch {
    pub fn open(orientation: Orientation, fb_xres: u32, fb_yres: u32) -> Result<Self> {
        let path = find_touch_device()?;
        let file = OpenOptions::new()
            .read(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        // The kernel treats the arg as a "non-NULL = grab, NULL = ungrab"
        // boolean (see drivers/input/evdev.c). Pass 1.
        let grab_res = unsafe { libc::ioctl(file.as_raw_fd(), EVIOCGRAB, 1) };
        let grabbed = grab_res == 0;
        Ok(Self {
            file,
            cur_x: 0,
            cur_y: 0,
            down_pending: false,
            up_pending: false,
            grabbed,
            orientation,
            fb_xres,
            fb_yres,
        })
    }

    /// Blocks until the next `Down` or `Up` boundary. Returns the
    /// boundary's (x, y) in user-visible framebuffer coordinates
    /// (orientation-corrected). Move events between boundaries silently
    /// update `cur_x/cur_y`.
    pub fn next_event(&mut self) -> Result<TouchEvent> {
        let mut buf = [0u8; EVENT_BYTES];
        loop {
            self.file
                .read_exact(&mut buf)
                .context("read /dev/input/eventN")?;

            // Bytes 0..8 are the timestamp; we don't need it.
            let type_ = u16::from_ne_bytes([buf[8], buf[9]]);
            let code = u16::from_ne_bytes([buf[10], buf[11]]);
            let value = i32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]);

            match (type_, code) {
                (EV_SYN, SYN_REPORT) => {
                    // Packet boundary — flush whichever pending state we
                    // accumulated. If both fired in the same packet
                    // (shouldn't happen — Down and Up are separate
                    // contacts), Up wins because it's the more recent
                    // intent.
                    if self.up_pending {
                        self.up_pending = false;
                        self.down_pending = false;
                        let (x, y) = self.transform_coords();
                        return Ok(TouchEvent::Up { x, y });
                    }
                    if self.down_pending {
                        self.down_pending = false;
                        let (x, y) = self.transform_coords();
                        return Ok(TouchEvent::Down { x, y });
                    }
                    // Move-only packet — keep reading.
                }
                (EV_ABS, ABS_MT_TRACKING_ID) => {
                    if value >= 0 {
                        self.down_pending = true;
                    } else if value == -1 {
                        self.up_pending = true;
                    }
                }
                (EV_ABS, ABS_MT_POSITION_X) => self.cur_x = value,
                (EV_ABS, ABS_MT_POSITION_Y) => self.cur_y = value,
                _ => {}
            }
        }
    }

    fn transform_coords(&self) -> (u32, u32) {
        let raw_x = self.cur_x.max(0) as u32;
        let raw_y = self.cur_y.max(0) as u32;
        match self.orientation {
            Orientation::Up => (raw_x, raw_y),
            // Mirror both axes so the touch coordinate space
            // matches the orientation-transformed fb writes.
            Orientation::Down => (
                self.fb_xres.saturating_sub(1).saturating_sub(raw_x),
                self.fb_yres.saturating_sub(1).saturating_sub(raw_y),
            ),
        }
    }
}

impl Drop for Touch {
    fn drop(&mut self) {
        if self.grabbed {
            unsafe {
                libc::ioctl(self.file.as_raw_fd(), EVIOCGRAB, 0);
            }
        }
    }
}

fn find_touch_device() -> Result<PathBuf> {
    let raw = std::fs::read_to_string("/proc/bus/input/devices")
        .context("read /proc/bus/input/devices")?;

    for block in raw.split("\n\n") {
        let name_line = block.lines().find(|l| l.starts_with("N: Name="));
        let lowered = name_line.unwrap_or("").to_lowercase();
        let is_touch = ["touch", "cyttsp", "zforce", "atmel"]
            .iter()
            .any(|needle| lowered.contains(needle));
        if !is_touch {
            continue;
        }
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("H: Handlers=") {
                if let Some(ev) = rest.split_whitespace().find(|w| w.starts_with("event")) {
                    return Ok(PathBuf::from(format!("/dev/input/{ev}")));
                }
            }
        }
    }
    bail!("no touchscreen entry in /proc/bus/input/devices");
}
