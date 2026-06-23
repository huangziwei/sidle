//! Input multiplexer: wait on the touchscreen and the bezel page-button device
//! at once via `poll(2)`, surfacing a unified event so the main loop handles
//! both without threads or channels.
//!
//! Why poll rather than two read loops: the main loop can only block in one
//! place, and it must wake for *either* device. `poll(2)` blocks until one fd
//! is readable, then we drain the ready one without blocking on the other.
//! `Buttons::read_one` reads exactly one record (poll already guaranteed it's
//! present). `Touch::next_event` is non-blocking (its fd is `O_NONBLOCK`): it
//! drains the currently-available events and returns `None` if they don't
//! complete a Down/Up boundary, in which case we re-poll — so a touch stroke
//! mid-flight can't block the loop and starve the button fd.

use std::os::fd::RawFd;

use anyhow::{Context, Result};

use super::buttons::{Buttons, PageButton};
use super::touch::{Touch, TouchEvent};
use crate::orientation::Orientation;

/// How long `next` blocks before surfacing a `Tick`. Bounds how quickly the
/// main loop notices a device rotation (it re-reads the framework orientation
/// on each `Tick`); only fires when idle, since real input returns first.
const TICK_MS: libc::c_int = 500;

/// A unified input event from either device.
pub enum InputEvent {
    Touch(TouchEvent),
    Page(PageButton),
    /// Poll timed out with no input. The main loop re-checks the framework
    /// orientation on this and repaints + re-orients touch/buttons if it
    /// changed (the X server rotates the display; raw evdev coords don't).
    Tick,
}

pub struct Input {
    touch: Touch,
    /// `None` when no page-button device was found/openable — the picker runs
    /// touch-only and `poll` watches just the touchscreen.
    buttons: Option<Buttons>,
}

impl Input {
    pub fn new(touch: Touch, buttons: Option<Buttons>) -> Self {
        Self { touch, buttons }
    }

    /// Re-orient both devices after a detected rotation (the display is rotated
    /// by the X server; raw evdev coords/buttons are panel-fixed and need this).
    pub fn set_orientation(&mut self, orientation: Orientation) {
        self.touch.set_orientation(orientation);
        if let Some(buttons) = self.buttons.as_mut() {
            buttons.set_orientation(orientation);
        }
    }

    /// Block until the next event from either device.
    ///
    /// Button presses are checked first each wake: a press is a deliberate
    /// navigation intent, and draining it promptly keeps the grabbed device's
    /// queue short. On touch readiness we drain `Touch::next_event`
    /// non-blocking; if it returns `None` (no boundary in the available data)
    /// we re-poll rather than block, keeping the button fd serviced.
    pub fn next(&mut self) -> Result<InputEvent> {
        let touch_fd: RawFd = self.touch.raw_fd();
        loop {
            let button_fd: RawFd = self.buttons.as_ref().map(|b| b.raw_fd()).unwrap_or(-1);
            let mut fds = [
                libc::pollfd { fd: touch_fd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: button_fd, events: libc::POLLIN, revents: 0 },
            ];
            let nfds: libc::nfds_t = if self.buttons.is_some() { 2 } else { 1 };

            let rc = unsafe { libc::poll(fds.as_mut_ptr(), nfds, TICK_MS) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue; // EINTR — re-arm the poll.
                }
                return Err(err).context("poll(touch, buttons)");
            }
            if rc == 0 {
                return Ok(InputEvent::Tick); // idle timeout — see TICK_MS.
            }

            // Buttons first. `read_one` returns None for releases / autorepeat
            // / SYN / unmapped keys, in which case we loop and poll again
            // rather than block on a second read.
            if let Some(buttons) = self.buttons.as_mut()
                && fds[1].revents & libc::POLLIN != 0
            {
                if let Some(page) = buttons.read_one()? {
                    return Ok(InputEvent::Page(page));
                }
                continue;
            }

            if fds[0].revents & libc::POLLIN != 0 {
                // Drain non-blocking. `next_event` returns None when the
                // available bytes don't complete a Down/Up boundary (a
                // move-only or partial packet) — re-poll rather than block in
                // the touch read, so a concurrent bezel-button press isn't
                // starved. This is why touch is opened O_NONBLOCK.
                if let Some(ev) = self.touch.next_event()? {
                    return Ok(InputEvent::Touch(ev));
                }
                continue;
            }

            // Spurious wake with no POLLIN — poll again.
        }
    }
}
