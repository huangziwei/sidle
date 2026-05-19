//! Kindle device sync — discovery, transport-agnostic IO, push/delete/pull.
//!
//! - P2a: push KFX to the device's documents directory, delete what we've sent.
//! - P2b: pull `.kfx`/`.kfx-zip` from `/dedrm` and import (mass-storage only —
//!   non-jailbroken devices have no `/dedrm` folder).
//! - P2c: send/remove over MTP for Kindle Scribe and other 2024+ models that
//!   dropped USB mass storage. Detection + IO live behind the [`Transport`]
//!   trait so push/delete/manifest stay transport-agnostic.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use serde::Serialize;

pub mod dedrm;
pub mod detect;
pub mod manifest;
pub mod mass_storage;
pub mod monitor;
pub mod mtp;
pub mod push;
pub mod transport;

#[allow(unused_imports)] // TEntry used by `Transport::list` (Phase 4 wiring).
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
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    #[serde(flatten)]
    pub transport: TransportKind,
}

/// Tagged on the wire as `{"transport":"mass_storage", ...}` or
/// `{"transport":"mtp", ...}` so the frontend can branch on a single
/// discriminator. Variant-specific fields ride along in the same object
/// thanks to `#[serde(flatten)]` on `DeviceInfo.transport`.
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
    /// Open a fresh transport handle for this device. Cheap for mass-storage
    /// (just wraps the mount path); for MTP this will open a USB session, so
    /// callers should reuse the handle within a single operation rather than
    /// re-opening per IO call.
    pub fn open_transport(&self) -> Result<Box<dyn Transport>> {
        match &self.transport {
            TransportKind::MassStorage { mount } => Ok(Box::new(
                mass_storage::transport::MassStorageTransport::new(PathBuf::from(mount)),
            )),
            TransportKind::Mtp { .. } => {
                // Phase 3 lands the impl; until then push/delete on MTP
                // devices fails cleanly at the command layer.
                Err(anyhow!(
                    "MTP transport not yet implemented — Scribe push/delete arrives in P2c phase 3"
                ))
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
