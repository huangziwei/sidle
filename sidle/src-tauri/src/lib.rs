//! sidle desktop app — library + KFX conversion + (later) Kindle sync.

mod commands;
mod cover_fetch;
mod device;
mod queue;
mod server;
mod state;
mod sync_pulse;

// The on-disk library — db, paths, import pipeline — lives in `sidle-core`
// so the LAN server crate can share it without pulling Tauri. Re-bind as
// `crate::library` so the existing `use crate::library::...` sites keep working.
use sidle_core::library;

use tauri::Manager;

use crate::state::AppState;

/// Opt the whole process out of macOS App Nap.
///
/// When the app window is in the background, App Nap throttles the process: the
/// tokio reactor and timers get coalesced by tens of seconds. The LAN server now
/// runs as a separate `sidle-server` daemon, so it's unaffected (part of why it
/// was moved out-of-process); but the app's *own* background work — the USB
/// device monitor and the conversion queue — would still stall while
/// backgrounded, so we keep this assertion for them.
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
            commands::library::library_set_asin,
            commands::library::library_bulk_update_metadata,
            commands::library::library_remove,
            commands::library::library_open_in_finder,
            commands::library::library_amazon_search,
            commands::library::library_cover_path,
            commands::library::library_recrawl_cover,
            commands::library::library_set_cover,
            commands::library::library_pick_files,
            commands::library::library_pick_image,
            commands::library::library_location,
            commands::library::library_pick_folder,
            commands::library::library_relocate_move,
            commands::library::library_relocate_use,
            commands::library::library_backup_pick_dest,
            commands::library::library_backup,
            commands::library::library_restore_pick_src,
            commands::library::library_restore,
            commands::queue::conversion_status,
            commands::queue::conversion_retry,
            commands::queue::conversion_set_workers,
            commands::device::device_status,
            commands::device::device_eject,
            commands::device::device_list_ours,
            commands::device::device_send,
            commands::device::device_delete,
            commands::device::device_import_orphan,
            commands::device::annotations_import_from_device,
            commands::device::kual_status,
            commands::device::kual_install,
            commands::device::kual_stage_dist,
            commands::server::server_status,
            commands::server::server_start,
            commands::server::server_stop,
            commands::reader::reader_open,
            commands::reader::reader_pdf_page,
            commands::reader::reader_pdf_ink,
            commands::reader::reader_pdf_ink_pages,
            commands::reader::annotations_for_book,
            commands::reader::reading_position_get,
            commands::reader::reading_position_set,
            commands::reader::book_search,
            commands::reader::annotation_create,
            commands::reader::annotation_update,
            commands::reader::annotation_delete,
            commands::notebook::notebook_list,
            commands::notebook::notebook_page_svg,
            commands::notebook::notebook_thumbnail,
            commands::notebook::notebook_rename,
            commands::notebook::notebook_remove,
            commands::notebook::notebook_import_folder,
            commands::notebook::notebook_import_device,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
