//! The apps that install to a Kindle's `/mnt/us` — the picker, bokai, and
//! whatever the user has registered (steb, karyll, kfxdedrm-fe).
//!
//! Registering an app records where its mount-rooted tree is on this machine;
//! everything shown about it — version, file count, size — is read off disk on
//! every call, because a local source is a working copy whose `build.sh`
//! rewrites it without telling anyone. The push itself lives in
//! `commands::device`, which composes these rows with the tree that ships in
//! this app bundle.

use std::path::PathBuf;

use serde::Serialize;
use sidle_core::library::apps::{self, AppTree, FileClass};
use sidle_core::library::db;
use tauri::State;

use crate::state::AppState;

/// One row of the "Apps on device" card.
#[derive(Serialize)]
pub struct AppRow {
    pub id: String,
    pub name: String,
    pub version: String,
    /// Absent for the picker and bokai, which ship with this app and have no
    /// row in the table — there is nothing to remove them from.
    pub source: Option<String>,
    pub tile: Option<String>,
    pub file_count: usize,
    pub total_bytes: u64,
    /// Bytes a status check has to read: the `sync` files. The rest is `seed`,
    /// decided by existence alone.
    pub hashed_bytes: u64,
    pub seed_count: usize,
    /// Why this app could not be read, when it could not. A moved checkout says
    /// so rather than quietly dropping out of the fleet.
    pub error: Option<String>,
}

fn row(tree: &AppTree, source: Option<String>) -> AppRow {
    AppRow {
        id: tree.spec.id.clone(),
        name: tree.spec.name.clone(),
        version: tree.spec.version.clone(),
        source,
        tile: tree.spec.tile.clone(),
        file_count: tree.files.len(),
        total_bytes: tree.total_size(),
        hashed_bytes: tree.sync_files().map(|f| f.size).sum(),
        seed_count: tree
            .files
            .iter()
            .filter(|f| f.policy.class == FileClass::Seed)
            .count(),
        error: None,
    }
}

/// Every app a push would carry, in the order it would carry them.
///
/// Built from the same composed plan the install uses, so the card cannot show
/// one set and the button push another.
#[tauri::command]
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
                .find(|r| r.id == tree.spec.id)
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
            version: "—".into(),
            source: Some(e.source.clone()),
            tile: None,
            file_count: 0,
            total_bytes: 0,
            hashed_bytes: 0,
            seed_count: 0,
            error: Some(e.error.clone()),
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
    let trees = tokio::task::spawn_blocking(move || apps::discover(&scan))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;
    if trees.is_empty() {
        return Err(format!(
            "No extensions/<id>/app.json under {}. An app declares itself with \
             one, so a folder without it is not a tree sidle can install.",
            path.display()
        ));
    }

    let conn = state.db.lock().await;
    let mut added = Vec::with_capacity(trees.len());
    for tree in &trees {
        db::upsert_app_source(
            &conn,
            &tree.spec.id,
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
