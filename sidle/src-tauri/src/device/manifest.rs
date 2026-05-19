//! On-device manifest at `<device>/.sidle/sent.json`.
//!
//! Tracks which KFXes we put there so:
//! - Deletes from the app only ever touch files we sent.
//! - Plugging the same Kindle into a different Mac still knows what's "ours".
//!
//! Read/written through [`Transport`] rather than `std::fs` so it works
//! identically across mass-storage and MTP — both transports treat
//! `.sidle/sent.json` as a regular object under the storage root.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::device::transport::{TPath, Transport};

const MANIFEST_DIR: &str = ".sidle";
const MANIFEST_FILE: &str = "sent.json";
const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    #[serde(default)]
    pub sent: BTreeMap<String, SentEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            sent: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentEntry {
    pub title: String,
    pub author: String,
    pub filename: String,
    pub sent_at: String,
}

pub fn manifest_path() -> TPath {
    TPath::parse(MANIFEST_DIR).join(MANIFEST_FILE)
}

pub fn load(transport: &dyn Transport) -> Result<Manifest> {
    let path = manifest_path();
    if !transport.exists(&path).unwrap_or(false) {
        return Ok(Manifest::default());
    }
    let bytes = transport
        .read(&path)
        .with_context(|| format!("read {}", transport.display_path(&path)))?;
    let m: Manifest = serde_json::from_slice(&bytes).context("parse sent.json")?;
    Ok(m)
}

pub fn save(transport: &dyn Transport, manifest: &Manifest) -> Result<()> {
    let path = manifest_path();
    let bytes = serde_json::to_vec_pretty(manifest)?;
    transport
        .write_atomic(&path, &bytes)
        .with_context(|| format!("write {}", transport.display_path(&path)))?;
    Ok(())
}
