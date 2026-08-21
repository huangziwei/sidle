//! The Apps tab. An `apps` row records where an app's mount-rooted tree sits
//! on this machine; name, version, file count and size are read off that tree
//! on every call. `commands::device` holds the push.

use std::path::PathBuf;

use serde::Serialize;
use sidle_core::library::apps::{self, AppTree};
use sidle_core::library::db;
use sidle_core::library::device::dist;
use tauri::State;

use crate::state::AppState;

/// One row of the Apps tab.
#[derive(Serialize)]
pub struct AppRow {
    pub id: String,
    pub name: String,
    /// Absent for a tree that states no version.
    pub version: Option<String>,
    /// Absent for an app with no `apps` row, composed from the built-in tree.
    pub source: Option<String>,
    pub tile: Option<String>,
    /// The tile's own art, a `data:image/…;base64,…` URI.
    pub icon: Option<String>,
    pub file_count: usize,
    pub total_bytes: u64,
    /// Why this app's tree could not be read.
    pub error: Option<String>,
    /// What the Wi-Fi route offers. `None` for an app the manifest does not
    /// name.
    pub dist: Option<AppDist>,
}

/// One app as the LAN manifest offers it.
#[derive(Serialize)]
pub struct AppDist {
    /// Whether the manifest's entry is as new as this machine's tree.
    pub current: bool,
    pub files: usize,
}

fn row(tree: &AppTree, source: Option<String>) -> AppRow {
    AppRow {
        id: tree.app.id.clone(),
        name: tree.app.name.clone(),
        version: tree.app.version.clone(),
        source,
        tile: tree.app.tile.clone(),
        icon: tree.app.icon.clone(),
        file_count: tree.files.len(),
        total_bytes: tree.total_size(),
        error: None,
        dist: None,
    }
}

/// Every app in one `DevicePlan`, what the Wi-Fi route offers of it, and
/// whether a Kindle is connected. Per-app device state: [`AppsDeviceStatus`].
#[derive(Serialize)]
pub struct AppsOverview {
    pub apps: Vec<AppRow>,
    pub device_connected: bool,
    /// Two apps claiming one mount path, reported and not resolved.
    pub conflicts: Vec<sidle_core::library::apps::compose::PathConflict>,
}

/// Every app's state on the connected Kindle. One device read, over the single
/// USB session an annotation sync also holds.
#[derive(Serialize)]
pub struct AppsDeviceStatus {
    pub apps: Vec<sidle_core::library::device::deploy::AppDeployStatus>,
    /// Set when a Kindle is connected but its status could not be read.
    pub error: Option<String>,
}

#[tauri::command]
pub async fn apps_overview(state: State<'_, AppState>) -> Result<AppsOverview, String> {
    let source = state.device_app_source.clone();
    let plan = crate::commands::device::compose_plan(&state, &source).await?;
    let rows = {
        let conn = state.db.lock().await;
        db::list_app_sources(&conn).map_err(|e| e.to_string())?
    };
    let mut apps = apps_list(&plan, &rows);

    // The Wi-Fi half: what `device-dist/` offers a Kindle that pulls. An app
    // indexed from an older tree than this machine holds is behind.
    if let Some(manifest) = dist::read_manifest(&state.paths.device_dist()) {
        for app in &mut apps {
            let Some(staged) = manifest.apps.iter().find(|a| a.id == app.id) else {
                continue;
            };
            let built_at = plan.app(&app.id).map(|t| t.built_at()).unwrap_or(0);
            app.dist = Some(AppDist {
                current: staged.built_at >= built_at,
                files: staged.files.len(),
            });
        }
    }

    Ok(AppsOverview {
        apps,
        device_connected: state.device.lock().await.is_some(),
        conflicts: plan.conflicts,
    })
}

/// The device half of every row. Empty with no Kindle connected.
#[tauri::command]
pub async fn apps_device_status(state: State<'_, AppState>) -> Result<AppsDeviceStatus, String> {
    if state.device.lock().await.is_none() {
        return Ok(AppsDeviceStatus {
            apps: Vec::new(),
            error: None,
        });
    }
    let source = state.device_app_source.clone();
    let plan = crate::commands::device::compose_plan(&state, &source).await?;
    match crate::commands::device::device_app_status(&state, &plan, &source).await {
        Ok(status) => Ok(AppsDeviceStatus {
            apps: status.apps,
            error: None,
        }),
        // A device that cannot be read lands in `error`.
        Err(e) => Ok(AppsDeviceStatus {
            apps: Vec::new(),
            error: Some(e),
        }),
    }
}

/// One row per app in `plan`, plus one per named source in `plan.errors`.
fn apps_list(
    plan: &sidle_core::library::apps::DevicePlan,
    rows: &[db::AppSourceRow],
) -> Vec<AppRow> {
    let mut out: Vec<AppRow> = plan
        .apps
        .iter()
        .map(|tree| {
            let source = rows
                .iter()
                .find(|r| r.id == tree.app.id)
                .map(|r| r.source.clone());
            row(tree, source)
        })
        .collect();

    // A source `plan` could not read keeps its row, carrying the error.
    for e in &plan.errors {
        if e.id.is_empty() {
            continue;
        }
        out.push(AppRow {
            id: e.id.clone(),
            name: e.id.clone(),
            version: None,
            source: Some(e.source.clone()),
            tile: None,
            icon: None,
            file_count: 0,
            total_bytes: 0,
            error: Some(e.error.clone()),
            dist: None,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Register every app under `path`. Re-adding an id repoints it.
#[tauri::command]
pub async fn apps_add(state: State<'_, AppState>, path: String) -> Result<Vec<AppRow>, String> {
    let path = PathBuf::from(path);
    let path = std::path::absolute(&path).map_err(|e| e.to_string())?;
    let scan = path.clone();
    let trees = tokio::task::spawn_blocking(move || apps::discover_registrable(&scan))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;

    let conn = state.db.lock().await;
    let mut added = Vec::with_capacity(trees.len());
    for tree in &trees {
        db::upsert_app_source(
            &conn,
            &tree.app.id,
            db::APP_SOURCE_LOCAL,
            &path.display().to_string(),
            &tree.root.display().to_string(),
        )
        .map_err(|e| e.to_string())?;
        added.push(row(tree, Some(path.display().to_string())));
    }
    Ok(added)
}

/// Drop an app's `apps` row. Nothing on disk or on a device is touched.
#[tauri::command]
pub async fn apps_remove(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let conn = state.db.lock().await;
    db::remove_app_source(&conn, &id).map_err(|e| e.to_string())
}
