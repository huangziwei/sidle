//! Exit-repaint guard — no framework freeze.
//!
//! Sidle draws raw to `/dev/fb0`, which is invisible to the window manager
//! (unlike kterm, a real WM window). Two consequences drive this design:
//!
//!  * We do NOT `SIGSTOP cvm`. The freeze was redundant — the framework only
//!    repainted over the gallery in RESPONSE to input, and we `EVIOCGRAB` both
//!    the touchscreen (`touch.rs`) and the bezel buttons (`buttons.rs`), so it
//!    sees none and stays quiescent. Worse, a frozen cvm stranded the chrome
//!    state machine (`winmgr chromeState` stuck at 0), leaving the home status
//!    bar (wifi/battery/clock) dead after exit. No freeze ⇒ chrome isn't
//!    corrupted ⇒ the home repaint brings its status bar back.
//!
//!  * On `Drop` we `appmgrd start app://com.lab126.booklet.home`. Since the WM
//!    never saw our raw writes, nothing repaints over our last frame unless we
//!    transition to a *different* app. `booklet.home` does that (on this
//!    firmware it resolves to the real KPP home and forces a full repaint).
//!    Starting `KPPMainApp` directly is a no-op — it's already the active app,
//!    so there's no transition and the screen stays stuck on our gallery frame.
//!
//! Risk: a stray system popup could in principle draw over the gallery mid-use
//! (the freeze used to mask that). Not observed in practice; an explicit
//! chrome-dismiss-on-entry can hang here if it ever happens.

use std::process::Command;

use anyhow::Result;

pub struct Pillow {
    _private: (),
}

impl Pillow {
    pub fn disable() -> Result<Self> {
        // No cvm freeze — see module docs.
        Ok(Self { _private: () })
    }
}

impl Drop for Pillow {
    fn drop(&mut self) {
        // Transition to a different app to force a repaint over our raw frame.
        // booklet.home resolves to the real home here; without the cvm freeze
        // its chrome (status bar) repaints with it. Best effort — Drop can't
        // propagate, and a missing lipc-set-prop just means no repaint nudge.
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
