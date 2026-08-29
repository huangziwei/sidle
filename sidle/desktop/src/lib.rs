//! The sidle desktop app: library, KFX conversion, Kindle sync.

mod commands;

mod device_monitor;

mod queue;
mod server;
mod state;
mod sync_pulse;

// `sidle_core::library` bound as `crate::library`.
use sidle_core::library;

use tauri::Manager;

use crate::state::AppState;

/// Opt the whole process out of macOS App Nap. The leaked `token` holds an
/// `UserInitiatedAllowingIdleSystemSleep` activity assertion for the life of
/// the process.
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

            // `realign_on_launch` restarts a daemon left by an earlier process,
            // off the setup path.
            let server = state.server.clone();
            let paths = state.paths.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = server
                    .realign_on_launch(paths, crate::server::DEFAULT_PORT)
                    .await
                {
                    tracing::warn!(error = %format!("{e:#}"), "could not restart the LAN server");
                }
            });

            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::library::library_list,
            commands::library::library_import,
            commands::library::library_update_metadata,
            commands::library::library_romanize,
            commands::library::library_set_asin,
            commands::library::library_bulk_update_metadata,
            commands::library::library_remove,
            commands::library::library_compact,
            commands::library::library_open_in_finder,
            commands::library::library_export_books,
            commands::library::library_amazon_search,
            commands::library::library_cover_path,
            commands::library::library_recrawl_cover,
            commands::library::library_recrawl_covers,
            commands::library::library_set_cover,
            commands::library::library_pick_files,
            commands::library::library_pick_image,
            commands::library::library_location,
            commands::library::library_pick_folder,
            commands::apps::apps_overview,
            commands::apps::apps_device_status,
            commands::apps::apps_add,
            commands::apps::apps_add_release,
            commands::apps::apps_remove,
            commands::library::library_relocate_move,
            commands::library::library_relocate_use,
            commands::library::library_backup_pick_dest,
            commands::library::library_backup,
            commands::library::library_restore_pick_src,
            commands::library::library_restore,
            commands::library::library_merge_pick_src,
            commands::library::library_merge,
            commands::omnibus::omnibus_propose,
            commands::omnibus::omnibus_split,
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
            commands::device::device_restore,
            commands::device::device_app_install,
            commands::device::device_app_uninstall,
            commands::server::server_status,
            commands::server::server_start,
            commands::server::server_stop,
            commands::editor::editor_open,
            commands::editor::editor_save_metadata,
            commands::editor::editor_toc,
            commands::editor::editor_set_toc,
            commands::editor::editor_repair_toc,
            commands::editor::editor_spine,
            commands::editor::editor_set_spine,
            commands::editor::editor_set_pdf_cover,
            commands::editor::editor_images,
            commands::editor::editor_export_image,
            commands::editor::editor_export_images,
            commands::editor::editor_pdf_pages,
            commands::editor::editor_export_pdf_page,
            commands::editor::editor_export_pdf_pages,
            commands::reader::reader_open,
            commands::reader::reader_fetch_resources,
            commands::reader::reader_fetch_sections,
            commands::reader::reader_eid_section,
            commands::reader::reader_release,
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
            commands::reader::annotation_set_note,
            commands::reader::book_ink_for_book,
            commands::reader::book_ink_delete,
            commands::reader::annotation_set_hidden,
            commands::reader::book_ink_set_hidden,
            commands::reader::open_external_url,
            commands::notebook::notebook_list,
            commands::notebook::notebook_page_svg,
            commands::notebook::notebook_thumbnail,
            commands::notebook::notebook_rename,
            commands::notebook::notebook_remove,
            commands::notebook::notebook_import_folder,
            commands::notebook::notebook_import_device,
            commands::notebook::notebook_export_pdf,
            commands::misc::misc_list,
            commands::misc::misc_read_text,
            commands::misc::misc_reveal,
            commands::misc::misc_delete,
            commands::misc::misc_collections_get,
            commands::misc::misc_collections_set,
            commands::reading_log::reading_log_overview,
            commands::reading_log::reading_log_books,
            commands::reading_log::reading_log_clear,
            commands::reading_log::reading_log_book,
            commands::reading_log::reading_log_import,
            commands::reading_log::reading_log_pick_folders,
            commands::reading_log::reading_log_cancel,
            commands::reading_log::reading_log_ambiguous,
            commands::reading_log::reading_log_attribute,
            commands::reading_log::reading_log_sessions,
            commands::reading_log::reading_log_day_hours,
            commands::reading_log::reading_log_set_finished,
        ])
        .on_window_event(|window, event| {
            // `prevent_close` keeps the process alive with its window hidden.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|app, event| {
            // `Reopen` shows and focuses every window.
            if let tauri::RunEvent::Reopen { .. } = event {
                for (_, window) in app.webview_windows() {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        });
}
