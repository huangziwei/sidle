//! The apps that install to a Kindle's `/mnt/us` — the picker, bokai, and
//! whatever the user has registered (steb, karyll, kfxdedrm-fe).
//!
//! Registering an app records where its mount-rooted tree is on this machine;
//! everything shown about it — name, version, file count, size — is read off
//! disk on every call, because a local source is a working copy whose
//! `build.sh` rewrites it without telling anyone. The push itself lives in
//! `commands::device`, which composes these rows with the tree that ships in
//! this app bundle.

use std::path::PathBuf;

use serde::Serialize;
use sidle_core::library::apps::{self, AppTree};
use sidle_core::library::db;
use tauri::State;

use crate::state::AppState;

/// One row of the Apps tab.
#[derive(Serialize)]
pub struct AppRow {
    pub id: String,
    pub name: String,
    /// Absent for a tree that states no version — steb and karyll ship none,
    /// and the row shows what the device holds instead.
    pub version: Option<String>,
    /// Absent for the picker and bokai, which ship with this app and have no
    /// row in the table — there is nothing to remove them from.
    pub source: Option<String>,
    pub tile: Option<String>,
    pub file_count: usize,
    pub total_bytes: u64,
    /// Why this app could not be read, when it could not. A moved checkout says
    /// so rather than quietly dropping out of the fleet.
    pub error: Option<String>,
    /// This app's state on the connected Kindle. `None` when none is connected.
    pub device: Option<sidle_core::library::device::deploy::AppDeployStatus>,
}

fn row(tree: &AppTree, source: Option<String>) -> AppRow {
    AppRow {
        id: tree.app.id.clone(),
        name: tree.app.name.clone(),
        version: tree.app.version.clone(),
        source,
        tile: tree.app.tile.clone(),
        file_count: tree.files.len(),
        total_bytes: tree.total_size(),
        error: None,
        device: None,
    }
}

/// What the Apps tab renders: every app, plus its state on the connected
/// Kindle when there is one.
///
/// One call, so the tab cannot show a row list from one moment and a device
/// state from another. `device` is absent per row when no Kindle is connected
/// or the status read failed — the rows still stand, because what an app *is*
/// does not depend on a cable.
#[derive(Serialize)]
pub struct AppsOverview {
    pub apps: Vec<AppRow>,
    pub device_connected: bool,
    /// Set when a Kindle is connected but its status could not be read.
    pub device_error: Option<String>,
    /// Two apps claiming one mount path. Surfaced rather than resolved: the
    /// loser's file would silently never install.
    pub conflicts: Vec<sidle_core::library::apps::compose::PathConflict>,
}

#[tauri::command]
pub async fn apps_overview(state: State<'_, AppState>) -> Result<AppsOverview, String> {
    let mut apps = apps_list(state.clone()).await?;
    let plan = {
        let source = state.device_app_source.clone();
        crate::commands::device::compose_plan(&state, &source).await?
    };

    let device_connected = state.device.lock().await.is_some();
    let mut device_error = None;
    if device_connected {
        match crate::commands::device::device_app_status(state.clone()).await {
            Ok(status) => {
                for app in &mut apps {
                    app.device = status.apps.iter().find(|a| a.id == app.id).cloned();
                }
            }
            Err(e) => device_error = Some(e),
        }
    }

    Ok(AppsOverview {
        apps,
        device_connected,
        device_error,
        conflicts: plan.conflicts,
    })
}

/// Every app a push would carry, in the order it would carry them.
///
/// Built from the same composed plan the install uses, so the tab cannot show
/// one set and the push another.
pub async fn apps_list(state: State<'_, AppState>) -> Result<Vec<AppRow>, String> {
    let source = state.device_app_source.clone();
    let plan = crate::commands::device::compose_plan(&state, &source).await?;
    let rows = {
        let conn = state.db.lock().await;
        db::list_app_sources(&conn).map_err(|e| e.to_string())?
    };

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

    // A registered app the plan could not read still gets a row: the user needs
    // to see that the checkout moved, not watch the app disappear.
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
            file_count: 0,
            total_bytes: 0,
            error: Some(e.error.clone()),
            device: None,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Register every app under `path`. One folder can hold several — sidle's own
/// tree holds the picker and bokai — and all of them are registered. Re-adding
/// an id repoints it.
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

/// Forget an app. Unregisters the source only — nothing on disk and nothing
/// already on a device is touched.
#[tauri::command]
pub async fn apps_remove(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let conn = state.db.lock().await;
    db::remove_app_source(&conn, &id).map_err(|e| e.to_string())
}
