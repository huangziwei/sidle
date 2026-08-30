//! Enumerate MTP-class Kindles over USB.

use crate::library::device::{DeviceInfo, TransportKind};

/// USB vendor ID for Amazon's Lab126 (every Kindle ever).
const AMAZON_VID: u16 = 0x1949;

/// Detect the first Amazon MTP device, if any.
pub fn detect() -> Option<DeviceInfo> {
    let devices = match mtp_rs::MtpDevice::list_devices() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[sidle/mtp] list_devices failed: {e}");
            return None;
        }
    };

    let dev = devices.into_iter().find(|d| d.vendor_id == AMAZON_VID)?;

    // Identity priority: USB serial number first (survives reconnection to a
    let serial = dev
        .serial_number
        .clone()
        .unwrap_or_else(|| format!("anon-mtp-{:x}", dev.location_id));

    // Prefer the USB product descriptor for a human-readable model. The MTP
    let model = match (&dev.manufacturer, &dev.product) {
        (Some(m), Some(p)) => Some(format!("{m} {p}")),
        (None, Some(p)) => Some(p.clone()),
        (Some(m), None) => Some(m.clone()),
        (None, None) => None,
    };

    Some(DeviceInfo {
        serial,
        model,
        // Firmware (`device_version`) and free/total bytes both come from an
        // open MTP session (`GetDeviceInfo` / `GetStorageInfo`), which the 2s
        // detect poll deliberately avoids. The on-connect refresh fills them in.
        firmware: None,
        free_bytes: None,
        total_bytes: None,
        transport: TransportKind::Mtp {
            location_id: dev.location_id,
            product_id: dev.product_id,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a real Kindle Scribe plugged into the test host this just
    /// confirms `detect()` doesn't panic and returns `None` cleanly.
    #[test]
    fn detect_returns_none_when_no_amazon_device() {
        let info = detect();
        match info {
            None => {}
            Some(d) => {
                // If CI somehow has a Kindle Scribe attached, at least
                // sanity-check the variant.
                assert!(matches!(d.transport, TransportKind::Mtp { .. }));
                assert!(!d.serial.is_empty());
            }
        }
    }
}
