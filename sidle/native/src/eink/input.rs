//! `poll(2)` over the touchscreen and the bezel page-button device at once,
//! surfacing one [`InputEvent`].

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::buttons::{Buttons, PageButton};
use super::touch::{Touch, TouchEvent};
use crate::orientation::Orientation;

/// How long [`Input::next`] blocks before surfacing [`InputEvent::Tick`].
const TICK_MS: libc::c_int = 500;

/// How long [`Input::follow_orientation`] leaves between `detect` reads.
const ORIENT_POLL: Duration = Duration::from_millis(1000);

/// A unified input event from either device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    Touch(TouchEvent),
    Page(PageButton),
    /// `poll` timed out, or an [`Input::watch`] descriptor is readable.
    Tick,
}

pub struct Input {
    touch: Touch,
    /// `None` where [`Buttons::open`] answers none.
    buttons: Option<Buttons>,
    /// The descriptors to wake on beside the input devices, from
    /// [`Input::watch`]. A slot holding -1 is skipped by `poll`.
    watched: [RawFd; 2],
    /// The orientation both devices are set to.
    orientation: Orientation,
    /// When `orientation` was read; `None` asks for a read at once.
    checked: Option<Instant>,
    /// [`Input::set_covered`]'s state.
    covered: bool,
}

impl Input {
    pub fn new(touch: Touch, buttons: Option<Buttons>) -> Self {
        Self {
            touch,
            buttons,
            watched: [-1; 2],
            orientation: Orientation::Up,
            checked: None,
            covered: false,
        }
    }

    /// Re-reads [`Orientation::detect`] past [`ORIENT_POLL`] and applies a
    /// change to both devices, answering whether one landed. A covered `Input`
    /// reads nothing.
    pub fn follow_orientation(&mut self) -> bool {
        if self.covered {
            return false;
        }
        if let Some(at) = self.checked
            && at.elapsed() < ORIENT_POLL
        {
            return false;
        }
        self.checked = Some(Instant::now());
        let seen = Orientation::detect();
        if seen == self.orientation {
            return false;
        }
        eprintln!("orientation: {:?} -> {seen:?}", self.orientation);
        self.set_orientation(seen);
        true
    }

    /// [`Input::follow_orientation`] with the [`ORIENT_POLL`] throttle skipped.
    pub fn follow_orientation_now(&mut self) -> bool {
        self.checked = None;
        self.follow_orientation()
    }

    /// The orientation both devices are set to.
    pub fn orientation(&self) -> Orientation {
        self.orientation
    }

    /// Wake on `fds` beside the input devices, answering an
    /// [`InputEvent::Tick`] where one is readable. One wait only:
    /// [`Input::next_deadline`] takes it.
    pub fn watch(&mut self, fds: [Option<RawFd>; 2]) {
        self.watched = fds.map(|fd| fd.unwrap_or(-1));
    }

    /// Sets `orientation` on `touch` and `buttons`.
    pub fn set_orientation(&mut self, orientation: Orientation) {
        self.orientation = orientation;
        self.touch.set_orientation(orientation);
        if let Some(buttons) = self.buttons.as_mut() {
            buttons.set_orientation(orientation);
        }
    }

    /// [`Touch::set_covered`] and [`Buttons::set_covered`] over both devices.
    pub fn set_covered(&mut self, covered: bool) {
        self.covered = covered;
        if !covered {
            self.checked = None;
        }
        self.touch.set_covered(covered);
        if let Some(buttons) = self.buttons.as_mut() {
            buttons.set_covered(covered);
        }
    }

    /// [`Touch::set_keyboard`] over the touchscreen. The bezel buttons keep
    /// their grab: the keyboard has no use for a page turn.
    pub fn set_keyboard(&mut self, up: bool) {
        self.touch.set_keyboard(up);
    }

    /// [`Touch::retake`] and [`Buttons::retake`] over both devices.
    pub fn retake(&mut self) {
        self.touch.retake();
        if let Some(buttons) = self.buttons.as_mut() {
            buttons.retake();
        }
    }

    /// [`Touch::current_pos`], in orientation-corrected coords.
    pub fn touch_pos(&self) -> (u32, u32) {
        self.touch.current_pos()
    }

    /// A pending [`InputEvent`], on a zero-timeout `poll`.
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
        // A zero timeout returns at once; a negative rc reads as no input.
        if unsafe { libc::poll(fds.as_mut_ptr(), nfds, 0) } <= 0 {
            return Ok(None);
        }
        // Touch first, against [`Input::next`]: this fd carries Cancel and the
        // screenshot gesture.
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

    /// [`Self::next_deadline`] with a [`TICK_MS`] wake and no deadline.
    pub fn next(&mut self) -> Result<InputEvent> {
        self.next_deadline(None)
    }

    /// [`Self::next`] with an [`InputEvent::Tick`] at `deadline`, past a busy
    /// touch fd.
    pub fn next_deadline(&mut self, deadline: Option<Instant>) -> Result<InputEvent> {
        let touch_fd: RawFd = self.touch.raw_fd();
        // [`Input::watch`] arms one wait. Taken here so a nested loop that
        // never armed a descriptor never waits on one.
        let watched = std::mem::replace(&mut self.watched, [-1; 2]);
        loop {
            // At or past `deadline`, through move-jitter that kept `poll` busy.
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
                libc::pollfd {
                    fd: watched[0],
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: watched[1],
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // A slot holding -1 is skipped by `poll`.
            let nfds: libc::nfds_t = fds.len() as libc::nfds_t;

            // Remaining time to `deadline`, floored at 1ms against a sub-ms
            // spin, else [`TICK_MS`]. `poll` wakes early on fd readiness.
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
            // `deadline` passed while `poll` blocked, with an event in the same
            // wake: the arm wins and the event surfaces next call.
            if let Some(d) = deadline
                && Instant::now() >= d
            {
                return Ok(InputEvent::Tick);
            }

            // Buttons first. `read_one` answers `None` on a release, autorepeat,
            // `SYN` or unmapped key, which re-polls.
            if let Some(buttons) = self.buttons.as_mut()
                && fds[1].revents & libc::POLLIN != 0
            {
                if let Some(page) = buttons.read_one()? {
                    return Ok(InputEvent::Page(page));
                }
                continue;
            }

            if fds[0].revents & libc::POLLIN != 0 {
                // `next_event` answers `None` short of a `Down`/`Up` boundary,
                // which re-polls: `Touch` is opened `O_NONBLOCK` for this.
                if let Some(ev) = self.touch.next_event()? {
                    return Ok(InputEvent::Touch(ev));
                }
                continue;
            }

            // A `watched` slot is readable; the caller drains it.
            if fds[2..].iter().any(|fd| fd.revents & libc::POLLIN != 0) {
                return Ok(InputEvent::Tick);
            }

            // A wake with no `POLLIN`.
        }
    }
}
