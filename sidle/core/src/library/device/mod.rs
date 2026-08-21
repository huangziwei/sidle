//! Kindle device sync — discovery, transport-agnostic IO, push/delete/pull.
//!
//! - Push KFX to the device's `documents/Sidle/` directory. The filename
//!   carries an `sha8` infix (`<basename>.<sha8>.kfx`), which identifies what
//!   sidle put there from the directory alone.
//! - Delete on-device by sha: scan `documents/Sidle/` for the matching
//!   `*.<sha8>.kfx`, remove it plus the Kindle-created `.sdr/` next to it.
//! - Pull `.kfx`/`.kfx-zip` from `/dedrm` and import (mass-storage only — a
//!   device without a jailbreak has no `/dedrm` folder).
//! - Send/remove over MTP for the Kindle Scribe and other 2024+ models.
//!   Detection and IO sit behind the [`Transport`] trait, which keeps
//!   push/delete/list transport-agnostic.

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

pub mod annotations;
pub mod dedrm;
pub mod deploy;
pub mod detect;
pub mod digest;
pub mod dist;
pub mod ink;
pub mod inventory;
pub mod mass_storage;
pub mod misc;
pub mod mtp;
pub mod notebooks;
pub mod push;
pub mod receipt;
pub mod transport;

pub use transport::{TEntry, TPath, Transport};

/// What sidle knows about a connected Kindle.
///
/// `transport` carries the variant-specific bits (mount path for mass-storage;
/// USB bus/address + cached object roots for MTP). Common fields stay flat so
/// the frontend's `device:status` listener can keep reading `serial`,
/// `free_bytes`, etc. directly.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeviceInfo {
    pub serial: String,
    pub model: Option<String>,
    /// Firmware/OS version, e.g. `5.16.2.1.1`. Mass-storage parses it out of
    /// `system/version.txt` at detect time; MTP reads it from `GetDeviceInfo`
    /// at session open, and holds `None` until the on-connect refresh lands.
    pub firmware: Option<String>,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    #[serde(flatten)]
    pub transport: TransportKind,
}

/// Tagged on the wire as `{"transport":"mass_storage", ...}` or
/// `{"transport":"mtp", ...}`, one discriminator to branch on. `#[serde(flatten)]`
/// on `DeviceInfo.transport` carries the variant's fields in the same object.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum TransportKind {
    MassStorage {
        /// Filesystem mount point, e.g. `/Volumes/Kindle`.
        mount: String,
    },
    Mtp {
        /// USB location ID — stable across reconnects to the same port.
        /// Re-open key when [`DeviceInfo::serial`] is the anon fallback
        /// (device exposes no USB serial descriptor).
        location_id: u64,
        /// USB product ID — `0x000A` etc. Lets the UI distinguish Scribe
        /// from Paperwhite-11+ without opening a session.
        product_id: u16,
    },
}

impl DeviceInfo {
    /// Open a fresh transport handle for this device. Mass-storage wraps the
    /// mount path; MTP opens a USB session, one per operation.
    pub fn open_transport(&self) -> Result<Box<dyn Transport>> {
        match &self.transport {
            TransportKind::MassStorage { mount } => Ok(Box::new(
                mass_storage::transport::MassStorageTransport::new(PathBuf::from(mount)),
            )),
            TransportKind::Mtp { location_id, .. } => {
                Ok(Box::new(mtp::transport::MtpTransport::open(*location_id)?))
            }
        }
    }

    /// Mass-storage mount path, if this is a mass-storage device. `None` for
    /// MTP. Used by [`dedrm`], which is mass-storage-only (no `/dedrm` folder
    /// exists on non-jailbroken devices).
    pub fn mass_storage_mount(&self) -> Option<PathBuf> {
        match &self.transport {
            TransportKind::MassStorage { mount } => Some(PathBuf::from(mount)),
            TransportKind::Mtp { .. } => None,
        }
    }
}

/// On-device path to the firmware marker, relative to the volume/storage root.
/// Mass-storage reads it off the mount; MTP downloads the same file from the
/// object tree.
pub const VERSION_TXT_REL: &str = "system/version.txt";

/// Pull the firmware version out of `system/version.txt`'s first line, which
/// reads `Kindle <firmware> [(build)]` — e.g. `Kindle 5.19.4.0.1 (476724 003)`.
/// The firmware is its first dotted-version token. `None` when it carries none.
pub(crate) fn parse_firmware(raw: &str) -> Option<String> {
    let first = raw.lines().next()?;
    first
        .split_whitespace()
        .find(|tok| {
            tok.starts_with(|c: char| c.is_ascii_digit())
                && tok.contains('.')
                && tok.chars().all(|c| c.is_ascii_digit() || c == '.')
        })
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::parse_firmware;

    #[test]
    fn parses_firmware_with_and_without_build() {
        // No parenthesised build suffix.
        assert_eq!(
            parse_firmware("Kindle 5.16.10.4.0\n").as_deref(),
            Some("5.16.10.4.0")
        );
        // With a build suffix — only the dotted version token is taken.
        assert_eq!(
            parse_firmware("Kindle 5.19.4.0.1 (476724 003)\n").as_deref(),
            Some("5.19.4.0.1")
        );
        // Nothing version-shaped on the line.
        assert_eq!(parse_firmware("Kindle\n"), None);
    }
}
