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
    pub fn documents_dir(&self) -> PathBuf {
        self.mount_path().join("documents")
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
    let serial = parse_serial(&raw)?;
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
}
