//! evdev touchscreen reader.
//!
//! Multi-touch protocol B: the kernel emits a stream of `input_event`
//! records with absolute X/Y plus a tracking-ID lifecycle. We collapse
//! that down to a single `Tap` event at finger-up (TRACKING_ID == -1)
//! with the last-seen X/Y. Good enough for M3 + M5 (list pick / cover
//! pick); pinch/scroll are out of scope.
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
const EV_ABS: u16 = 0x03;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;

const EVENT_BYTES: usize = 16;

// _IOW('E', 0x90, int): direction(W=1)<<30 | size(4)<<16 | type('E'=0x45)<<8 | nr(0x90)
// = 0x40000000 | 0x40000 | 0x4500 | 0x90 = 0x40044590
const EVIOCGRAB: libc::c_int = 0x40044590;

pub struct Touch {
    file: File,
    cur_x: i32,
    cur_y: i32,
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
            grabbed,
            orientation,
            fb_xres,
            fb_yres,
        })
    }

    /// Blocks until the next finger-up. Returns the absolute (x, y) in
    /// user-visible framebuffer coordinates (orientation-corrected).
    pub fn next_tap(&mut self) -> Result<(u32, u32)> {
        let mut buf = [0u8; EVENT_BYTES];
        loop {
            self.file
                .read_exact(&mut buf)
                .context("read /dev/input/eventN")?;

            // Bytes 0..8 are the timestamp; we don't need it.
            let type_ = u16::from_ne_bytes([buf[8], buf[9]]);
            let code = u16::from_ne_bytes([buf[10], buf[11]]);
            let value = i32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]);

            if type_ != EV_ABS {
                continue;
            }
            match code {
                ABS_MT_POSITION_X => self.cur_x = value,
                ABS_MT_POSITION_Y => self.cur_y = value,
                ABS_MT_TRACKING_ID if value == -1 => {
                    let raw_x = self.cur_x.max(0) as u32;
                    let raw_y = self.cur_y.max(0) as u32;
                    let (x, y) = match self.orientation {
                        Orientation::Up => (raw_x, raw_y),
                        // Mirror both axes so the touch coordinate space
                        // matches the orientation-transformed fb writes.
                        Orientation::Down => (
                            self.fb_xres.saturating_sub(1).saturating_sub(raw_x),
                            self.fb_yres.saturating_sub(1).saturating_sub(raw_y),
                        ),
                    };
                    return Ok((x, y));
                }
                _ => {}
            }
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
