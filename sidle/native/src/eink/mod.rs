//! On-device display + input plumbing.
//!
//! Display goes through a real X11 window (`fb.rs`, via the pure-Rust `x11rb`)
//! so the lab126 compositor manages + recomposites it — see
//! [[project_kual_statusbar_x11]] and §8 of `.claude/plans/kual-ui-revamp.md`.
//! Input is raw evdev (`touch.rs`, `buttons.rs`) with `EVIOCGRAB`, multiplexed
//! by `input.rs`. No `libxcb`/C dependencies. (`x11poc.rs` is a throwaway
//! window POC behind the `--x11-poc` arg.)

pub mod buttons;
pub mod fb;
pub mod input;
pub mod touch;
pub mod x11poc;
