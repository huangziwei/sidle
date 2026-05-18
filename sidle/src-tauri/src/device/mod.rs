//! Kindle USB mass-storage transport.
//!
//! P2a scope: detect a mounted Kindle, push KFX to `/documents`, delete what
//! we've sent. Pull from `/dedrm` is deferred to P2b (those files are
//! `.kfx-zip`, not raw KFX — needs an unpack step that doesn't exist yet).

pub mod detect;
pub mod manifest;
pub mod monitor;
pub mod push;
