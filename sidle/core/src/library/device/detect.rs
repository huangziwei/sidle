//! Top-level device fan-out.
//!
//! Tries each transport's detector in order. Mass-storage first because it's
//! cheap (one `read_dir` of `/Volumes`) and represents the older, more common
//! devices; MTP requires opening a USB session so we only fall through when
//! nothing is mounted. The first hit wins — a user with two Kindles plugged in
//! at once gets the first one detected.

use crate::library::device::{DeviceInfo, mass_storage, mtp};

pub fn detect() -> Option<DeviceInfo> {
    // Mass-storage first: cheap (`read_dir` over `/Volumes`), and it covers
    // the older, more common Kindles. Only fall through to MTP when nothing
    // is mounted, since `mtp_rs::list_devices` can briefly open vendor-class
    // USB devices to inspect descriptors.
    if let Some(info) = mass_storage::detect::detect() {
        return Some(info);
    }
    if let Some(info) = mtp::detect::detect() {
        return Some(info);
    }
    None
}
