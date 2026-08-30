//! evdev page-button reader (KOA2 bezel buttons).
//!
//! The page-turn buttons are a *separate* input device from the touchscreen.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::orientation::Orientation;

const EV_KEY: u16 = 0x01;
const KEY_PAGEUP: u16 = 104;
const KEY_PAGEDOWN: u16 = 109;
const EVENT_BYTES: usize = 16;

// Same EVIOCGRAB as touch.rs — _IOW('E', 0x90, int) = 0x40044590. The call
const EVIOCGRAB: libc::c_int = 0x40044590;

/// Which bezel button fired. Hardware-confirmed KOA2 mapping: top button
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageButton {
    Prev,
    Next,
}

pub struct Buttons {
    file: File,
    /// Whether the `EVIOCGRAB` succeeded. If it didn't, the framework still
    /// sees presses (UI may tear), but we still read + act on them.
    grabbed: bool,
    /// Current framework orientation. On `Down` (180° flip) the physical top/
    /// bottom buttons swap sides, so we swap `Prev`/`Next` to keep "forward"
    /// under the same thumb. Updated at runtime by [`Buttons::set_orientation`].
    orientation: Orientation,
}

impl Buttons {
    /// Open and grab the page-button device. `Ok(None)` when no `gpio-keys`
    /// device exists (a different Kindle generation / firmware) — the picker
    /// still works on touch alone, just without bezel navigation.
    pub fn open() -> Result<Option<Self>> {
        let Some(path) = find_button_device()? else {
            return Ok(None);
        };
        let file = OpenOptions::new()
            .read(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let grabbed = unsafe { libc::ioctl(file.as_raw_fd(), EVIOCGRAB as _, 1) } == 0;
        Ok(Some(Self {
            file,
            grabbed,
            orientation: Orientation::Up,
        }))
    }

    /// Raw fd for `poll(2)` multiplexing alongside the touchscreen — see
    /// [`crate::eink::input`].
    pub fn raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// Update orientation so a 180° flip swaps prev/next (see field docs).
    pub fn set_orientation(&mut self, orientation: Orientation) {
        self.orientation = orientation;
    }

    /// Read one event record. The caller polls first, so a record is available
    /// and this won't block. Returns `Some` only on a key *press* (`value==1`)
    pub fn read_one(&mut self) -> Result<Option<PageButton>> {
        let mut buf = [0u8; EVENT_BYTES];
        self.file
            .read_exact(&mut buf)
            .context("read /dev/input/eventN (buttons)")?;
        let type_ = u16::from_ne_bytes([buf[8], buf[9]]);
        let code = u16::from_ne_bytes([buf[10], buf[11]]);
        let value = i32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]);
        if type_ == EV_KEY && value == 1 {
            let btn = match code {
                // Hardware-confirmed on KOA2: the *top* button emits
                KEY_PAGEUP => Some(PageButton::Next),
                KEY_PAGEDOWN => Some(PageButton::Prev),
                _ => None,
            };
            // On a 180° flip the physical buttons swap sides; swap prev/next so
            // "forward" stays under the same thumb as the rotated display.
            return Ok(match (btn, self.orientation) {
                (Some(PageButton::Next), Orientation::Down) => Some(PageButton::Prev),
                (Some(PageButton::Prev), Orientation::Down) => Some(PageButton::Next),
                (other, _) => other,
            });
        }
        Ok(None)
    }
}

impl Drop for Buttons {
    fn drop(&mut self) {
        if self.grabbed {
            unsafe {
                libc::ioctl(self.file.as_raw_fd(), EVIOCGRAB as _, 0);
            }
        }
    }
}

/// Locate the bezel page-button device by exact `Name="gpio-keys"` in
/// `/proc/bus/input/devices`, returning its `/dev/input/eventN` path.
fn find_button_device() -> Result<Option<PathBuf>> {
    let raw = std::fs::read_to_string("/proc/bus/input/devices")
        .context("read /proc/bus/input/devices")?;
    for block in raw.split("\n\n") {
        let is_buttons = block
            .lines()
            .any(|l| l.starts_with("N: Name=") && l.contains("\"gpio-keys\""));
        if !is_buttons {
            continue;
        }
        for line in block.lines() {
            let Some(rest) = line.strip_prefix("H: Handlers=") else {
                continue;
            };
            if let Some(ev) = rest.split_whitespace().find(|w| w.starts_with("event")) {
                return Ok(Some(PathBuf::from(format!("/dev/input/{ev}"))));
            }
        }
    }
    Ok(None)
}
