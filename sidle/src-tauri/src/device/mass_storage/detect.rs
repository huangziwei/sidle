//! Detect a mounted Kindle by scanning `/Volumes` (macOS) or `/media` (Linux)
//! for `system/version.txt`. Lifted verbatim from the pre-Transport
//! `device::detect` module so the KOA2 path stays byte-identical.

use std::ffi::CString;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use nusb::MaybeFuture;

use crate::device::{DeviceInfo, TransportKind};

#[cfg(target_os = "macos")]
const VOLUMES_ROOT: &str = "/Volumes";
#[cfg(target_os = "linux")]
const VOLUMES_ROOT_FALLBACK: &str = "/media";

/// Amazon's USB vendor ID — every Kindle, mass-storage or MTP.
const AMAZON_VID: u16 = 0x1949;
/// USB Mass Storage class code (`bInterfaceClass`). Distinguishes a mounted
/// KOA2-class Kindle from a Scribe (MTP, image/PTP class) when both are plugged.
const USB_CLASS_MASS_STORAGE: u8 = 0x08;

/// Scan mount points for the first connected mass-storage Kindle. None if
/// none found. MTP-class Kindles (Scribe, 2024+) never show up here — they
/// don't mount.
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
    // version.txt is the Kindle-only marker + firmware/model line; it carries
    // NO serial. The serial lives in the USB `iSerial` descriptor (read via
    // nusb), with the persisted `.sidle/device_id` and then an anon id as
    // fallbacks if USB enumeration is unavailable.
    let serial = usb_kindle_serial()
        .or_else(|| ensure_device_id(mount))
        .unwrap_or_else(|| anon_serial(mount));
    let model = parse_model(&raw);
    let firmware = crate::device::parse_firmware(&raw);
    let (free, total) = fs_usage(mount).unwrap_or((None, None));
    Some(DeviceInfo {
        serial,
        model,
        firmware,
        free_bytes: free,
        total_bytes: total,
        transport: TransportKind::MassStorage {
            mount: mount.to_string_lossy().to_string(),
        },
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

/// The Kindle's real serial, read from its USB `iSerial` descriptor — the same
/// identity Amazon exposes over MTP, and the only on-device source (it is NOT in
/// `system/version.txt`). `nusb::list_devices` enumerates without opening the
/// device, so the serial comes straight from the cached descriptor.
///
/// Filters to Amazon (VID 0x1949). With one Kindle attached that's
/// unambiguous; with two (a KOA2 *and* a Scribe), the mounted mass-storage one
/// is the device presenting a Mass Storage interface, so we prefer that to
/// avoid reading the Scribe's serial for the mounted volume. `None` if USB
/// enumeration is unavailable or no Amazon device is present.
fn usb_kindle_serial() -> Option<String> {
    let amazon: Vec<nusb::DeviceInfo> = nusb::list_devices()
        .wait()
        .ok()?
        .filter(|d| d.vendor_id() == AMAZON_VID)
        .collect();
    let pick = match amazon.as_slice() {
        [] => return None,
        [only] => only,
        many => many
            .iter()
            .find(|d| d.interfaces().any(|i| i.class() == USB_CLASS_MASS_STORAGE))
            .unwrap_or(&many[0]),
    };
    pick.serial_number()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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
        let frsize = s.f_frsize;
        let free = (s.f_bavail as u64).checked_mul(frsize);
        let total = (s.f_blocks as u64).checked_mul(frsize);
        Some((free, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let info = inspect(tmp.path()).expect("should detect even without a serial in version.txt");
        // Serial comes from USB (real device) or the device_id/anon fallback —
        // never from version.txt. Either way it must be non-empty.
        assert!(!info.serial.is_empty());
        assert_eq!(
            info.model.as_deref(),
            Some("Kindle 5.16.2.1.1 (409745 002)")
        );
        assert_eq!(info.firmware.as_deref(), Some("5.16.2.1.1"));
        match info.transport {
            TransportKind::MassStorage { mount } => {
                assert_eq!(mount, tmp.path().to_string_lossy());
            }
            other => panic!("expected MassStorage, got {other:?}"),
        }
    }
}
