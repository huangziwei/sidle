//! Detect a mounted Kindle by scanning `/Volumes` for `system/version.txt`.

use std::ffi::CString;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Mount point, e.g. `/Volumes/Kindle`.
    pub mount: String,
    /// First line of `system/version.txt`, e.g. `Kindle 5.16.10.4.0`.
    pub model: Option<String>,
    /// Serial parsed from `version.txt`. Stable identifier for the device.
    pub serial: String,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
}

impl DeviceInfo {
    pub fn mount_path(&self) -> PathBuf {
        PathBuf::from(&self.mount)
    }
    /// Where we write KFX. The `Sidle` subdir keeps our pushes namespaced so
    /// the Kindle's `/documents` root stays whatever the user had before, and
    /// our deletes can't ever touch unrelated files.
    pub fn documents_dir(&self) -> PathBuf {
        self.mount_path().join("documents").join("Sidle")
    }
}

#[cfg(target_os = "macos")]
const VOLUMES_ROOT: &str = "/Volumes";
#[cfg(target_os = "linux")]
const VOLUMES_ROOT_FALLBACK: &str = "/media";

/// Scan mount points for the first connected Kindle. None if none found.
pub fn detect() -> Option<DeviceInfo> {
    for root in candidate_roots() {
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(info) = inspect(&path) {
                    return Some(info);
                }
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn candidate_roots() -> Vec<PathBuf> {
    vec![PathBuf::from(VOLUMES_ROOT)]
}

#[cfg(target_os = "linux")]
fn candidate_roots() -> Vec<PathBuf> {
    // Linux mass-storage automount layout varies by distro/desktop.
    // /media/$USER is typical (GNOME, GVfs); /run/media/$USER for newer udisks2.
    let mut roots = Vec::new();
    if let Ok(user) = std::env::var("USER") {
        roots.push(PathBuf::from(format!("/media/{user}")));
        roots.push(PathBuf::from(format!("/run/media/{user}")));
    }
    roots.push(PathBuf::from(VOLUMES_ROOT_FALLBACK));
    roots
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn candidate_roots() -> Vec<PathBuf> {
    Vec::new()
}

fn inspect(mount: &Path) -> Option<DeviceInfo> {
    let version_path = mount.join("system").join("version.txt");
    let raw = std::fs::read_to_string(&version_path).ok()?;
    // version.txt is the Kindle-only marker; serial is best-effort.
    let serial = parse_serial(&raw)
        .or_else(|| ensure_device_id(mount))
        .unwrap_or_else(|| anon_serial(mount));
    let model = parse_model(&raw);
    let (free, total) = fs_usage(mount).unwrap_or((None, None));
    Some(DeviceInfo {
        mount: mount.to_string_lossy().to_string(),
        model,
        serial,
        free_bytes: free,
        total_bytes: total,
    })
}

/// Read or create `<kindle>/.sidle/device_id`. We use this as a stable
/// per-device identity when the firmware's `version.txt` doesn't include
/// `S/N:` (the case on Paperwhite 11+ and similar). Generated once per
/// Kindle; survives firmware updates because it lives on the data partition.
fn ensure_device_id(mount: &Path) -> Option<String> {
    let dir = mount.join(".sidle");
    let id_path = dir.join("device_id");
    if let Ok(content) = std::fs::read_to_string(&id_path) {
        let id = content.trim().to_string();
        if !id.is_empty() {
            return Some(id);
        }
    }
    let _ = std::fs::create_dir_all(&dir);
    let id = generate_id();
    if std::fs::write(&id_path, &id).is_ok() {
        Some(id)
    } else {
        None
    }
}

fn generate_id() -> String {
    use sha2::{Digest, Sha256};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().to_le_bytes())
        .unwrap_or([0u8; 16]);
    let pid = std::process::id().to_le_bytes();
    let mut h = Sha256::new();
    h.update(now);
    h.update(pid);
    let hash = h.finalize();
    hash[..16].iter().map(|b| format!("{:02x}", b)).collect()
}

/// Last-resort identity if we couldn't even write to the device (read-only
/// mount). Tied to the mount-point name so multiple anon Kindles at least
/// don't collide trivially.
fn anon_serial(mount: &Path) -> String {
    let name = mount
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Kindle".to_string());
    format!("anon-{name}")
}

fn parse_serial(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let line = line.trim();
        for prefix in ["S/N:", "Serial Number:", "Serial:"] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let s = rest.trim();
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

fn parse_model(raw: &str) -> Option<String> {
    let first = raw.lines().next()?.trim();
    if first.is_empty() {
        return None;
    }
    Some(first.to_string())
}

fn fs_usage(path: &Path) -> Option<(Option<u64>, Option<u64>)> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    unsafe {
        let mut s = MaybeUninit::<libc::statvfs>::uninit();
        if libc::statvfs(c.as_ptr(), s.as_mut_ptr()) != 0 {
            return None;
        }
        let s = s.assume_init();
        let frsize = s.f_frsize as u64;
        let free = (s.f_bavail as u64).checked_mul(frsize);
        let total = (s.f_blocks as u64).checked_mul(frsize);
        Some((free, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_serial_from_sn_line() {
        let raw = "Kindle 5.16.10.4.0\nS/N: G090G105073700XX\n";
        assert_eq!(parse_serial(raw).as_deref(), Some("G090G105073700XX"));
    }

    #[test]
    fn parses_serial_with_serial_number_label() {
        let raw = "Kindle\nSerial Number:  G09XXX\n";
        assert_eq!(parse_serial(raw).as_deref(), Some("G09XXX"));
    }

    #[test]
    fn no_serial_returns_none() {
        let raw = "Kindle 5.16.10.4.0\nOther garbage\n";
        assert_eq!(parse_serial(raw), None);
    }

    #[test]
    fn parses_model_from_first_line() {
        let raw = "Kindle 5.16.10.4.0\nS/N: X\n";
        assert_eq!(parse_model(raw).as_deref(), Some("Kindle 5.16.10.4.0"));
    }

    #[test]
    fn ensure_device_id_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let a = ensure_device_id(tmp.path()).expect("write should succeed");
        let b = ensure_device_id(tmp.path()).expect("second read should succeed");
        assert_eq!(a, b);
        assert!(a.len() >= 16);
    }

    #[test]
    fn inspect_detects_kindle_without_serial_in_version_txt() {
        // Replicates this user's Paperwhite — version.txt has no S/N: line.
        let tmp = tempfile::tempdir().unwrap();
        let sys = tmp.path().join("system");
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::write(sys.join("version.txt"), "Kindle 5.16.2.1.1 (409745 002)\n").unwrap();

        let info = inspect(tmp.path()).expect("should detect even without S/N:");
        assert!(!info.serial.is_empty(), "fell back to device_id or anon");
        assert_eq!(info.model.as_deref(), Some("Kindle 5.16.2.1.1 (409745 002)"));
    }
}
