//! What sidle last installed on a device.
//!
//! The receipt tells an app's own update apart from an edit made on the device.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{TPath, Transport};

/// Mount-relative location of the receipt on the device.
pub const RECEIPT_PATH: &str = "extensions/sidle/var/installed.json";

/// The only `schema` this build reads. A receipt from a newer one is treated as
/// absent — every path then reports against the device itself, which is slower
/// and never wrong.
pub const RECEIPT_SCHEMA: u32 = 1;

/// One file as it was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReceipt {
    pub sha256: String,
    /// Compared before the hash: a device copy of a different length differs,
    /// and MTP charges for every byte read.
    pub size: u64,
}

/// One app's install, as of the push that wrote it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppReceipt {
    /// What the source tree stated at install time, when it stated anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The tree's build stamp, in unix seconds.
    #[serde(default)]
    pub built_at: u64,
    /// When the push ran, in unix seconds.
    #[serde(default)]
    pub installed_at: u64,
    /// Mount-relative path to what was written there.
    #[serde(default)]
    pub files: BTreeMap<String, FileReceipt>,
}

/// Every app sidle has installed on this device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallState {
    pub schema: u32,
    #[serde(default)]
    pub apps: BTreeMap<String, AppReceipt>,
}

impl Default for InstallState {
    fn default() -> Self {
        Self {
            schema: RECEIPT_SCHEMA,
            apps: BTreeMap::new(),
        }
    }
}

impl InstallState {
    /// Read the device's receipt, or an empty one.
    pub fn read(transport: &dyn Transport) -> Self {
        let path = TPath::parse(RECEIPT_PATH);
        let Ok(true) = transport.exists(&path) else {
            return Self::default();
        };
        let Ok(bytes) = transport.read(&path) else {
            return Self::default();
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(state) if state.schema == RECEIPT_SCHEMA => state,
            _ => Self::default(),
        }
    }

    pub fn write(&self, transport: &dyn Transport) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self).context("serialize the install receipt")?;
        transport
            .write_atomic(&TPath::parse(RECEIPT_PATH), &bytes)
            .with_context(|| format!("write {RECEIPT_PATH}"))
    }

    /// What was written at `path`, if this app's install wrote it.
    pub fn file(&self, app_id: &str, path: &str) -> Option<&FileReceipt> {
        self.apps.get(app_id)?.files.get(path)
    }

    pub fn app(&self, app_id: &str) -> Option<&AppReceipt> {
        self.apps.get(app_id)
    }

    /// Replace one app's record wholesale. A path the app has stopped shipping
    /// leaves the receipt with it.
    pub fn set_app(&mut self, app_id: &str, receipt: AppReceipt) {
        self.apps.insert(app_id.to_string(), receipt);
    }

    pub fn forget_app(&mut self, app_id: &str) {
        self.apps.remove(app_id);
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::device::mass_storage::transport::MassStorageTransport;

    fn transport(root: &std::path::Path) -> MassStorageTransport {
        MassStorageTransport::new(root.to_path_buf())
    }

    fn receipt(sha: &str, size: u64) -> FileReceipt {
        FileReceipt {
            sha256: sha.into(),
            size,
        }
    }

    #[test]
    fn a_device_with_no_receipt_reads_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let state = InstallState::read(&transport(tmp.path()));
        assert!(state.apps.is_empty());
        assert_eq!(state.schema, RECEIPT_SCHEMA);
    }

    #[test]
    fn a_receipt_round_trips_through_the_device() {
        let tmp = tempfile::tempdir().unwrap();
        let t = transport(tmp.path());
        let mut state = InstallState::default();
        let mut files = BTreeMap::new();
        files.insert(
            "extensions/sprocket/bin/sprocket".to_string(),
            receipt("ab", 4),
        );
        state.set_app(
            "sprocket",
            AppReceipt {
                version: Some("0.4.0".into()),
                built_at: 1755712345,
                installed_at: 1755712400,
                files,
            },
        );
        state.write(&t).unwrap();

        let read_back = InstallState::read(&t);
        assert_eq!(
            read_back.app("sprocket").unwrap().version.as_deref(),
            Some("0.4.0")
        );
        assert_eq!(
            read_back
                .file("sprocket", "extensions/sprocket/bin/sprocket")
                .unwrap()
                .sha256,
            "ab"
        );
        assert!(
            read_back
                .file("gadget", "extensions/sprocket/bin/sprocket")
                .is_none()
        );
    }

    /// A receipt this build cannot read is worth nothing and must not be worth
    /// less than nothing: every path falls back to reading the device.
    #[test]
    fn a_receipt_from_another_schema_reads_as_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let t = transport(tmp.path());
        t.write_atomic(
            &TPath::parse(RECEIPT_PATH),
            br#"{"schema":99,"apps":{"sprocket":{"files":{}}}}"#,
        )
        .unwrap();
        assert!(InstallState::read(&t).apps.is_empty());
    }

    /// A push rewrites an app's whole record: a path it has stopped shipping
    /// leaves the receipt with it.
    #[test]
    fn setting_an_app_replaces_rather_than_merges() {
        let mut state = InstallState::default();
        let mut first = BTreeMap::new();
        first.insert("extensions/gadget/bin/old".to_string(), receipt("aa", 1));
        state.set_app(
            "gadget",
            AppReceipt {
                files: first,
                ..Default::default()
            },
        );
        let mut second = BTreeMap::new();
        second.insert("extensions/gadget/bin/gadget".to_string(), receipt("bb", 2));
        state.set_app(
            "gadget",
            AppReceipt {
                files: second,
                ..Default::default()
            },
        );
        assert!(state.file("gadget", "extensions/gadget/bin/old").is_none());
        assert!(
            state
                .file("gadget", "extensions/gadget/bin/gadget")
                .is_some()
        );
    }
}
