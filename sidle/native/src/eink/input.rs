//! Input multiplexer: wait on the touchscreen and the bezel page-button device
//! at once via `poll(2)`, surfacing a unified event so the main loop handles
//! both without threads or channels.

use std::os::fd::RawFd;
use std::time::Instant;

use anyhow::{Context, Result};

use super::buttons::{Buttons, PageButton};
use super::touch::{Touch, TouchEvent};
use crate::orientation::Orientation;

/// How long `next` blocks before surfacing a `Tick`. Bounds how quickly the
/// main loop notices a device rotation (it re-reads the framework orientation
/// on each `Tick`); only fires when idle, since real input returns first.
const TICK_MS: libc::c_int = 500;

/// A unified input event from either device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    /// Latest primary touch position, user-visible coords (see
    /// [`Touch::current_pos`]). Read at the arm deadline for the long-press slop
    /// guard — a hold that has drifted off its landing point is a drag, not a hold.
    pub fn touch_pos(&self) -> (u32, u32) {
        self.touch.current_pos()
    }

    /// Non-blocking check for a pending event (zero-timeout `poll`). Returns
    pub fn poll_now(&mut self) -> Result<Option<InputEvent>> {
        let touch_fd: RawFd = self.touch.raw_fd();
        let button_fd: RawFd = self.buttons.as_ref().map(|b| b.raw_fd()).unwrap_or(-1);
        let mut fds = [
            libc::pollfd {
                fd: touch_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: button_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let nfds: libc::nfds_t = if self.buttons.is_some() { 2 } else { 1 };
        // Zero timeout → return immediately. A negative rc (EINTR/error) is
        // treated as "no input this tick"; the caller polls again next chunk.
        if unsafe { libc::poll(fds.as_mut_ptr(), nfds, 0) } <= 0 {
            return Ok(None);
        }
        // Touch first, unlike `next` (which prioritizes bezel presses for
        if fds[0].revents & libc::POLLIN != 0
            && let Some(ev) = self.touch.next_event()?
        {
            return Ok(Some(InputEvent::Touch(ev)));
        }
        if let Some(buttons) = self.buttons.as_mut()
            && fds[1].revents & libc::POLLIN != 0
            && let Some(page) = buttons.read_one()?
        {
            return Ok(Some(InputEvent::Page(page)));
        }
        Ok(None)
    }

    /// Block until the next event from either device (see
    /// [`Self::next_deadline`]); the everyday call, with only the idle
    /// [`TICK_MS`] wake and no arm deadline.
    pub fn next(&mut self) -> Result<InputEvent> {
        self.next_deadline(None)
    }

    /// Like [`Self::next`], but when `deadline` is `Some`, surfaces an
    /// [`InputEvent::Tick`] the instant that time is reached — even while the
    /// touch fd stays busy.
    pub fn next_deadline(&mut self, deadline: Option<Instant>) -> Result<InputEvent> {
        let touch_fd: RawFd = self.touch.raw_fd();
        loop {
            // At/past the deadline: surface the wake now, even if move-jitter kept
            // `poll` busy right up to it (a fixed TICK_MS reset can't guarantee this).
            if let Some(d) = deadline
                && Instant::now() >= d
            {
                return Ok(InputEvent::Tick);
            }
            let button_fd: RawFd = self.buttons.as_ref().map(|b| b.raw_fd()).unwrap_or(-1);
            let mut fds = [
                libc::pollfd {
                    fd: touch_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: button_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let nfds: libc::nfds_t = if self.buttons.is_some() { 2 } else { 1 };

            // Remaining time to the deadline (≥1ms so a sub-ms remainder can't
            // spin), else the idle TICK_MS. poll still wakes early on fd
            // readiness; the timeout only bounds the idle wake.
            let timeout = match deadline {
                Some(d) => (d
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(i32::MAX as u128) as libc::c_int)
                    .max(1),
                None => TICK_MS,
            };
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), nfds, timeout) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue; // EINTR — re-arm the poll.
                }
                return Err(err).context("poll(touch, buttons)");
            }
            if rc == 0 {
                return Ok(InputEvent::Tick); // deadline reached, or idle timeout.
            }
            // The deadline passed while poll was blocked and an event happened to
            if let Some(d) = deadline
                && Instant::now() >= d
            {
                return Ok(InputEvent::Tick);
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
                if let Some(ev) = self.touch.next_event()? {
                    return Ok(InputEvent::Touch(ev));
                }
                continue;
            }

            // Spurious wake with no POLLIN — poll again.
        }
    }
}
