//! Background polling for Kindle connect/disconnect events.
//!
//! Polls every 2 seconds, dedupes against the last snapshot, emits
//! `device:status` over Tauri events when state changes (and also on the very
//! first tick so the frontend has an initial value without a separate fetch).

use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

use crate::device::detect::{self, DeviceInfo};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub type DeviceState = Arc<Mutex<Option<DeviceInfo>>>;

pub fn new_state() -> DeviceState {
    Arc::new(Mutex::new(None))
}

pub fn spawn(app: AppHandle, state: DeviceState) {
    tauri::async_runtime::spawn(async move {
        let mut first_tick = true;
        loop {
            let next = detect::detect();
            let changed = {
                let mut guard = state.lock().await;
                let prev = guard.clone();
                let changed = prev != next;
                *guard = next.clone();
                changed
            };
            if changed || first_tick {
                let _ = app.emit("device:status", &next);
            }
            first_tick = false;
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}
