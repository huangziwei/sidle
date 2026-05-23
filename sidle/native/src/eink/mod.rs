//! On-device eink + input plumbing.
//!
//! Direct kernel interfaces only (mmap/ioctl on `/dev/fb0`, evdev on
//! `/dev/input/eventN`, lipc-set-prop for pillow) — no fb-ink, no
//! C dependencies. See `.claude/plans/native-kindle-app.md` for the
//! rationale.

pub mod buttons;
pub mod fb;
pub mod input;
pub mod pillow;
pub mod touch;
