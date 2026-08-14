//! MTP transport for Scribe and other 2024+ Kindles that dropped USB
//! mass storage.
//!
//! - [`detect`]: lists USB MTP devices via [`mtp_rs::MtpDevice::list_devices`],
//!   filters to Amazon, builds a [`DeviceInfo`](crate::library::device::DeviceInfo)
//!   with `TransportKind::Mtp`.
//! - [`transport`]: implements [`crate::library::device::transport::Transport`] over
//!   an open MTP session — push/delete/list go through the same trait the
//!   mass-storage impl satisfies.

pub mod detect;
pub mod transport;
