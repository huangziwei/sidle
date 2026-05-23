//! evdev page-button reader (KOA2 bezel buttons).
//!
//! The page-turn buttons are a *separate* input device from the touchscreen.
//! On the KOA2 they're `gpio-keys` (probed via `/proc/bus/input/devices`),
//! emitting `EV_KEY` with `KEY_PAGEUP` (104) and `KEY_PAGEDOWN` (109) — the two
//! bits in that device's `KEY=2100` capability mask.
//!
//! We open it and `EVIOCGRAB` it for the same reason `touch.rs` grabs the
//! touchscreen: once grabbed, the stock framework no longer sees the press, so
//! it stops repainting the native library over our gallery (the corruption
//! this fixes). Without the grab the picker UI tears on every button press.
//!
//! **Safety:** the device is matched by exact `Name="gpio-keys"`, never "any
//! key device". The power button(s) are *separate* devices — `snvs-powerkey`
//! and `max77796-key`, both `KEY_POWER` — and grabbing those would lock the
//! user out of power-off. `gpio-keys` carries only the two page codes, so the
//! grab is surgical.
//!
//! Wire format matches `touch.rs`: 16-byte records on this 32-bit kernel,
//! `type@8-9 code@10-11 value@12-15` (native-endian).

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;

use anyhow::{Context, Result};

const EV_KEY: u16 = 0x01;
const KEY_PAGEUP: u16 = 104;
const KEY_PAGEDOWN: u16 = 109;
const EVENT_BYTES: usize = 16;

// Same EVIOCGRAB as touch.rs — _IOW('E', 0x90, int) = 0x40044590.
const EVIOCGRAB: libc::c_int = 0x40044590;

/// Which bezel button fired. Mapped from keycodes: `KEY_PAGEUP` → `Prev`,
/// `KEY_PAGEDOWN` → `Next`. If the physical direction feels reversed on
/// hardware, swap these two arms in [`Buttons::read_one`].
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
        let grabbed = unsafe { libc::ioctl(file.as_raw_fd(), EVIOCGRAB, 1) } == 0;
        Ok(Some(Self { file, grabbed }))
    }

    /// Raw fd for `poll(2)` multiplexing alongside the touchscreen — see
    /// [`crate::eink::input`].
    pub fn raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// Read one event record. The caller polls first, so a record is available
    /// and this won't block. Returns `Some` only on a key *press* (`value==1`)
    /// of a mapped page key; `None` for releases, autorepeat (`value==2`),
    /// `SYN`, or any unmapped key — so a press fires exactly one page turn.
    pub fn read_one(&mut self) -> Result<Option<PageButton>> {
        let mut buf = [0u8; EVENT_BYTES];
        self.file
            .read_exact(&mut buf)
            .context("read /dev/input/eventN (buttons)")?;
        let type_ = u16::from_ne_bytes([buf[8], buf[9]]);
        let code = u16::from_ne_bytes([buf[10], buf[11]]);
        let value = i32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]);
        if type_ == EV_KEY && value == 1 {
            return Ok(match code {
                KEY_PAGEUP => Some(PageButton::Prev),
                KEY_PAGEDOWN => Some(PageButton::Next),
                _ => None,
            });
        }
        Ok(None)
    }
}

impl Drop for Buttons {
    fn drop(&mut self) {
        if self.grabbed {
            unsafe {
                libc::ioctl(self.file.as_raw_fd(), EVIOCGRAB, 0);
            }
        }
    }
}

/// Locate the bezel page-button device by exact `Name="gpio-keys"` in
/// `/proc/bus/input/devices`, returning its `/dev/input/eventN` path.
/// `Ok(None)` if absent. Deliberately strict on the name — see the safety note
/// in the module docs (never grab the power-key devices).
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
            if let Some(rest) = line.strip_prefix("H: Handlers=") {
                if let Some(ev) = rest.split_whitespace().find(|w| w.starts_with("event")) {
                    return Ok(Some(PathBuf::from(format!("/dev/input/{ev}"))));
                }
            }
        }
    }
    Ok(None)
}
