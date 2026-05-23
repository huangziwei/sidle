//! sidle desktop app — library + KFX conversion + (later) Kindle sync.

mod commands;
mod cover_fetch;
mod device;
mod queue;
mod server;
mod state;

// The on-disk library — db, paths, import pipeline — lives in `sidle-core`
// so the LAN server crate can share it without pulling Tauri. Re-bind as
// `crate::library` so the existing `use crate::library::...` sites keep working.
use sidle_core::library;

use tauri::Manager;

use crate::state::AppState;

/// Opt the whole process out of macOS App Nap.
///
/// When the app window is in the background (the user is reading on the
/// Kindle, not looking at the Mac), App Nap throttles the process: the tokio
/// reactor and timers get coalesced by tens of seconds. The embedded LAN
/// server then answers a request ~30s late, well past the KUAL picker's 3s
/// `LIST_TIMEOUT`, so the Kindle shows "can't reach server" even though the
/// server is healthy (it answers in ~50ms once the app is foregrounded).
///
/// We hold an `NSProcessInfo` activity assertion for the life of the process.
/// `UserInitiatedAllowingIdleSystemSleep` disables App Nap but still lets the
/// Mac sleep normally when idle. The token is intentionally leaked: the
/// assertion must last as long as the app runs, and we never want to end it.
#[cfg(target_os = "macos")]
fn disable_app_nap() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
    let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
        &NSString::from_str("sidle embedded LAN server stays responsive while backgrounded"),
    );
    std::mem::forget(token);
}

#[cfg(not(target_os = "macos"))]
fn disable_app_nap() {}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    disable_app_nap();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|app| {
            let state = AppState::bootstrap(app.handle().clone())
                .map_err(|e| format!("failed to bootstrap app state: {e:#}"))?;
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::library::library_list,
            commands::library::library_import,
            commands::library::library_update_metadata,
            commands::library::library_remove,
            commands::library::library_open_in_finder,
            commands::library::library_cover_path,
            commands::library::library_recrawl_cover,
            commands::library::library_set_cover,
            commands::library::library_pick_files,
            commands::library::library_pick_image,
            commands::queue::conversion_status,
            commands::queue::conversion_retry,
            commands::queue::conversion_set_workers,
            commands::device::device_status,
            commands::device::device_eject,
            commands::device::device_list_ours,
            commands::device::device_send,
            commands::device::device_delete,
            commands::device::device_import_orphan,
            commands::device::kual_status,
            commands::device::kual_install,
            commands::server::server_status,
            commands::server::server_start,
            commands::server::server_stop,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
