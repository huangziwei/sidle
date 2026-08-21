//! What sidle last installed on this device.
//!
//! A file that differs from what an update offers is either a version behind or
//! a value edited here; a record of what was written tells them apart. A device
//! copy matching the receipt is sidle's to replace; one that does not is kept.
//!
//! Mirrors the desktop's `receipt::InstallState` — the same JSON at the same
//! mount-relative path, written by whichever route ran last.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Mount-relative location of the receipt.
pub const RECEIPT_REL: &str = "extensions/sidle/var/installed.json";

/// The only `schema` this build reads. Anything else is treated as absent.
pub const RECEIPT_SCHEMA: u32 = 1;

/// One file as it was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReceipt {
    pub sha256: String,
    pub size: u64,
}

/// One app's install, as of the update that wrote it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppReceipt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub built_at: u64,
    #[serde(default)]
    pub installed_at: u64,
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

pub fn path_under(mount: &Path) -> PathBuf {
    mount.join(RECEIPT_REL)
}

impl InstallState {
    /// The receipt under `mount`. An absent, unreadable, unparseable or
    /// wrong-schema file reads as [`InstallState::default`].
    pub fn read(mount: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path_under(mount)) else {
            return Self::default();
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(state) if state.schema == RECEIPT_SCHEMA => state,
            _ => Self::default(),
        }
    }

    /// Write the receipt under `mount`, creating `var/` when it is absent.
    pub fn write(&self, mount: &Path) -> anyhow::Result<()> {
        let dest = path_under(mount);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        let tmp = crate::selfupdate::with_dot_suffix(&dest, "download");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &dest)?;
        Ok(())
    }

    /// What was written at `path` for `app_id`.
    pub fn file(&self, app_id: &str, path: &str) -> Option<&FileReceipt> {
        self.apps.get(app_id)?.files.get(path)
    }

    /// Record what one path holds: the bytes a pull wrote there, or the bytes
    /// it found there. Either settles the next pull without a read.
    pub fn record(&mut self, app_id: &str, path: &str, file: FileReceipt) {
        let app = self.apps.entry(app_id.to_string()).or_default();
        app.files.insert(path.to_string(), file);
    }

    /// Stamp an app as installed at the manifest's version and build.
    pub fn describe(&mut self, app_id: &str, version: Option<String>, built_at: u64) {
        let app = self.apps.entry(app_id.to_string()).or_default();
        app.version = version;
        app.built_at = built_at;
        app.installed_at = now_secs();
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

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sidle-receipt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_written_receipt_reads_back() {
        let mount = scratch("roundtrip");
        let mut state = InstallState::default();
        state.record(
            "karyll",
            "extensions/karyll/bin/karyll",
            FileReceipt {
                sha256: "abc".into(),
                size: 12,
            },
        );
        state.describe("karyll", Some("0.4.0".into()), 1_755_712_345);
        state.write(&mount).unwrap();

        let back = InstallState::read(&mount);
        assert_eq!(
            back.file("karyll", "extensions/karyll/bin/karyll")
                .map(|f| f.size),
            Some(12)
        );
        assert_eq!(back.apps["karyll"].version.as_deref(), Some("0.4.0"));
        assert_eq!(back.apps["karyll"].built_at, 1_755_712_345);
        assert!(back.apps["karyll"].installed_at > 0);
        let _ = std::fs::remove_dir_all(&mount);
    }

    #[test]
    fn an_absent_or_foreign_receipt_reads_as_empty() {
        let mount = scratch("absent");
        assert!(InstallState::read(&mount).apps.is_empty());

        std::fs::create_dir_all(path_under(&mount).parent().unwrap()).unwrap();
        std::fs::write(path_under(&mount), br#"{"schema":99,"apps":{"x":{}}}"#).unwrap();
        assert!(
            InstallState::read(&mount).apps.is_empty(),
            "a schema this build does not read is treated as absent"
        );
        let _ = std::fs::remove_dir_all(&mount);
    }

    /// `record` takes a confirmed path as well as a written one; `describe` is
    /// what stamps the time.
    #[test]
    fn recording_a_path_leaves_the_install_time_alone() {
        let mut state = InstallState::default();
        state.record(
            "steb",
            "extensions/steb/bin/steb",
            FileReceipt {
                sha256: "s".into(),
                size: 1,
            },
        );
        assert_eq!(state.apps["steb"].installed_at, 0);

        state.describe("steb", None, 42);
        assert!(state.apps["steb"].installed_at > 0);
        assert_eq!(state.apps["steb"].built_at, 42);
    }

    #[test]
    fn writing_one_app_leaves_the_others_alone() {
        let mount = scratch("merge");
        let mut state = InstallState::default();
        state.record(
            "steb",
            "extensions/steb/bin/steb",
            FileReceipt {
                sha256: "s".into(),
                size: 1,
            },
        );
        state.write(&mount).unwrap();

        let mut state = InstallState::read(&mount);
        state.record(
            "sidle",
            "extensions/sidle/bin/sidle",
            FileReceipt {
                sha256: "p".into(),
                size: 2,
            },
        );
        state.write(&mount).unwrap();

        let back = InstallState::read(&mount);
        assert!(back.file("steb", "extensions/steb/bin/steb").is_some());
        assert!(back.file("sidle", "extensions/sidle/bin/sidle").is_some());
        let _ = std::fs::remove_dir_all(&mount);
    }
}
