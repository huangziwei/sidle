//! Display + touch orientation handling.
//!
//! The KOA2 framework rotates the screen 180° based on which side the
//! page-turn bezel is on (accelerometer-driven). Our binary writes raw
//! framebuffer pixels and reads raw evdev touch events — neither honors
//! the framework's rotation. We detect the framework's current orientation
//! at startup and apply a 180° transform to both fb writes and touch
//! reads so the user-visible UI is right-side-up regardless of grip.
//!
//! Detection happens once, at startup. Mid-session rotation (the device
//! flipped while sidle runs) is unsupported: the framework redraws over us and
//! nothing here tracks the change.

use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// Native portrait, page-turn bezel on the right. fb + touch coords
    /// pass through unchanged.
    Up,
    /// Rotated 180°, page-turn bezel on the left. Apply mirror transform
    /// to both axes.
    Down,
}

impl Orientation {
    /// Best-effort detection via `lipc-get-prop com.lab126.winmgr orientation`.
    /// Returns "U"/"D"/"L"/"R" on stdout; we only care about U vs D for KOA2.
    /// On any error / unrecognized output, defaults to Up.
    pub fn detect() -> Self {
        let Ok(out) = Command::new("lipc-get-prop")
            .args(["com.lab126.winmgr", "orientation"])
            .output()
        else {
            return Self::Up;
        };
        if !out.status.success() {
            return Self::Up;
        }
        match String::from_utf8_lossy(&out.stdout).trim() {
            "D" => Self::Down,
            _ => Self::Up,
        }
    }
}
