//! MTP transport for Scribe and other 2024+ Kindles that dropped USB
//! mass storage.
//!
//! - [`detect`]: lists USB MTP devices via [`mtp_rs::MtpDevice::list_devices`],
//!   filters to Amazon, builds a [`DeviceInfo`](crate::device::DeviceInfo)
//!   with `TransportKind::Mtp`.
//! - The transport impl arrives in Phase 3 (P2c). For now `open_transport`
//!   returns an error on MTP devices, which surfaces to the UI if push/delete
//!   is attempted before the impl lands.

pub mod detect;
