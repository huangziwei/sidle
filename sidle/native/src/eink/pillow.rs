//! Framework lifecycle guard.
//!
//! SIGSTOP the Kindle framework process (`cvm`) on entry, SIGCONT on exit.
//! That single move freezes all framework drawing AND its input pump (so
//! the status bar can't repaint over us, and taps don't reach the home
//! tile router). The "pillow disable" lipc property is NOT used: with cvm
//! stopped, pillow has nothing to draw anyway; re-enabling it on exit was
//! leaving the status bar in a stale half-redraw state because pillow
//! resumes without a full repaint trigger.
//!
//! Drop order: SIGCONT cvm, then `appmgrd start home` to nudge the
//! framework into redrawing its full UI (status bar + library tiles) over
//! whatever pixels we left behind.
//!
//! Risk: a sigkill of our process leaves cvm SIGSTOP'd, looking frozen
//! until the user power-holds. Signal handling joins later; for now a
//! panic still hits Drop via Rust's unwind path.
//!
//! Module name kept as `pillow` because the plan references it; the
//! struct exposed is the framework guard.

use std::process::Command;

use anyhow::Result;

pub struct Pillow {
    framework_paused: bool,
}

impl Pillow {
    pub fn disable() -> Result<Self> {
        let framework_paused = killall("STOP", "cvm");
        Ok(Self { framework_paused })
    }
}

impl Drop for Pillow {
    fn drop(&mut self) {
        if !self.framework_paused {
            return;
        }
        // Failure to resume cvm is the catastrophic case (device frozen
        // until reboot). Best effort since Drop can't propagate.
        let _ = killall("CONT", "cvm");
        // SIGCONT'd framework resumes execution but doesn't repaint — it
        // only redraws on state changes. `appmgrd start home` forces a
        // full UI redraw including the status bar.
        let _ = Command::new("lipc-set-prop")
            .args([
                "-s",
                "com.lab126.appmgrd",
                "start",
                "app://com.lab126.booklet.home",
            ])
            .output();
    }
}

fn killall(signal: &str, target: &str) -> bool {
    // BusyBox killall accepts `-SIGNAME PROC` for signal shorthand.
    Command::new("killall")
        .arg(format!("-{signal}"))
        .arg(target)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
