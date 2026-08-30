//! Top-level device fan-out.

use crate::library::device::{DeviceInfo, mass_storage, mtp};

pub fn detect() -> Option<DeviceInfo> {
    // Mass-storage first: cheap (`read_dir` over `/Volumes`), and it covers
    if let Some(info) = mass_storage::detect::detect() {
        return Some(info);
    }
    if let Some(info) = mtp::detect::detect() {
        return Some(info);
    }
    None
}
