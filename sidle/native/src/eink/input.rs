//! Input multiplexer: wait on the touchscreen and the bezel page-button device
//! at once via `poll(2)`, surfacing a unified event so the main loop handles
//! both without threads or channels.
//!
//! Why poll rather than two read loops: `Touch::next_event` and
//! `Buttons::read_one` both block on `read`, and the main loop can only block
//! in one place. `poll(2)` blocks until *either* fd is readable, then we drain
//! the ready one. The touch parser is unchanged — once its fd is readable we
//! hand off to the existing `next_event`, which completes its packet.

use std::os::fd::RawFd;

use anyhow::{Context, Result};

use super::buttons::{Buttons, PageButton};
use super::touch::{Touch, TouchEvent};

/// A unified input event from either device.
pub enum InputEvent {
    Touch(TouchEvent),
    Page(PageButton),
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

    /// Block until the next event from either device.
    ///
    /// Button presses are checked first each wake: a press is a deliberate
    /// navigation intent, and draining it promptly keeps the grabbed device's
    /// queue short. A touch readiness hands off to `Touch::next_event`, which
    /// reads through to the next Down/Up boundary as before.
    pub fn next(&mut self) -> Result<InputEvent> {
        let touch_fd: RawFd = self.touch.raw_fd();
        loop {
            let button_fd: RawFd = self.buttons.as_ref().map(|b| b.raw_fd()).unwrap_or(-1);
            let mut fds = [
                libc::pollfd { fd: touch_fd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: button_fd, events: libc::POLLIN, revents: 0 },
            ];
            let nfds: libc::nfds_t = if self.buttons.is_some() { 2 } else { 1 };

            let rc = unsafe { libc::poll(fds.as_mut_ptr(), nfds, -1) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue; // EINTR — re-arm the poll.
                }
                return Err(err).context("poll(touch, buttons)");
            }

            // Buttons first. `read_one` returns None for releases / autorepeat
            // / SYN / unmapped keys, in which case we loop and poll again
            // rather than block on a second read.
            if self.buttons.is_some() && fds[1].revents & libc::POLLIN != 0 {
                if let Some(buttons) = self.buttons.as_mut() {
                    if let Some(page) = buttons.read_one()? {
                        return Ok(InputEvent::Page(page));
                    }
                    continue;
                }
            }

            if fds[0].revents & libc::POLLIN != 0 {
                return Ok(InputEvent::Touch(self.touch.next_event()?));
            }

            // Spurious wake with no POLLIN — poll again.
        }
    }
}
