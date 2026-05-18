//! On-device manifest at `<kindle>/.sidle/sent.json`.
//!
//! Tracks which KFXes we put there so:
//! - Deletes from the app only ever touch files we sent.
//! - Plugging the same Kindle into a different Mac still knows what's "ours".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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

pub fn manifest_path(device_root: &Path) -> PathBuf {
    device_root.join(MANIFEST_DIR).join(MANIFEST_FILE)
}

pub fn load(device_root: &Path) -> Result<Manifest> {
    let path = manifest_path(device_root);
    if !path.exists() {
        return Ok(Manifest::default());
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let m: Manifest =
        serde_json::from_slice(&bytes).with_context(|| "parse sent.json")?;
    Ok(m)
}

pub fn save(device_root: &Path, manifest: &Manifest) -> Result<()> {
    let dir = device_root.join(MANIFEST_DIR);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let path = manifest_path(device_root);
    let tmp = path.with_extension("json.partial");
    let bytes = serde_json::to_vec_pretty(manifest)?;
    std::fs::write(&tmp, &bytes)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}
