//! USB mass-storage transport (KOA2 and other pre-2024 Kindles).
//!
//! Same shape every Kindle exposed before firmware 5.16.3: a FAT/exFAT
//! partition mounted under `/Volumes` (macOS) or `/media`/`/run/media`
//! (Linux). Detection is a directory scan for `system/version.txt`;
//! transport is `std::fs` against the mount point.

pub mod detect;
pub mod transport;
