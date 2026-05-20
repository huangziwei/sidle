//! Tauri commands for the embedded LAN server toggle.

use tauri::State;

use crate::server::{DEFAULT_PORT, ServerStatus};
use crate::state::AppState;

#[tauri::command]
pub async fn server_status(state: State<'_, AppState>) -> Result<ServerStatus, String> {
    Ok(state.server.status(&state.paths).await)
}

#[tauri::command]
pub async fn server_start(
    state: State<'_, AppState>,
    port: Option<u16>,
) -> Result<ServerStatus, String> {
    let port = port.unwrap_or(DEFAULT_PORT);
    state
        .server
        .start(state.paths.clone(), port)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn server_stop(state: State<'_, AppState>) -> Result<ServerStatus, String> {
    state.server.stop().await;
    Ok(state.server.status(&state.paths).await)
}
