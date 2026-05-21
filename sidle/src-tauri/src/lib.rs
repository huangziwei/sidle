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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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
            commands::device::device_list_ours,
            commands::device::device_send,
            commands::device::device_delete,
            commands::device::device_import_orphan,
            commands::server::server_status,
            commands::server::server_start,
            commands::server::server_stop,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
