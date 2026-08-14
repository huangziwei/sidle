//! Tauri commands for splitting a collection into the series it collects.
//!
//! Two calls, because the split is a confirmed action rather than a button that
//! fires: [`omnibus_propose`] reads the book and reports what it would do, and
//! [`omnibus_split`] carries out the plan the user hands back. Between them sits
//! a form — the series name and every volume's title and number are the user's,
//! and nothing is written until they say so.
//!
//! The commit is one long job that fans out into N conversions. It emits
//! `library:split-progress` per volume while it writes them, and the
//! `epub_to_kfx` job each new volume earns then shows up in the ordinary
//! conversion queue.

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::library::db;
use crate::library::omnibus::{self, SplitPlan, VolumeOutcome};
use crate::state::AppState;

/// What a book would split into, plus what it is called now — the modal's
/// whole starting state.
#[derive(Debug, Serialize)]
pub struct SplitProposal {
    pub book_id: i64,
    /// The collection's own title, shown above the form for context.
    pub title: String,
    /// The series name the volumes will be grouped under, guessed from the
    /// title and editable.
    pub series_name: String,
    pub volumes: Vec<omnibus::VolumeCut>,
    /// How many books other than this one already sit in `series_name` — a
    /// series it was split into before, or volumes that arrived separately.
    /// Non-zero is a warning: the positions already taken there stay as they
    /// are.
    pub existing_in_series: i64,
}

/// Per-volume progress while a split runs, emitted as `library:split-progress`.
#[derive(Clone, Serialize)]
struct SplitProgress {
    book_id: i64,
    done: usize,
    total: usize,
    title: String,
}

/// How a committed split turned out, volume by volume.
#[derive(Debug, Serialize)]
pub struct SplitSummary {
    pub series_name: String,
    pub volumes: Vec<VolumeOutcome>,
}

/// Read a book and report the volumes it would split into. An empty volume list
/// is the honest answer for an ordinary book, not an error.
#[tauri::command]
pub async fn omnibus_propose(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<SplitProposal, String> {
    let row = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "book not found".to_string())?
    };
    let Some(epub_path) = row.epub_path.clone() else {
        return Err(
            "this book has no EPUB yet — the conversion queue is still working on it".to_string(),
        );
    };

    let reading = row.clone();
    let plan = tokio::task::spawn_blocking(move || -> Result<SplitPlan, String> {
        let bytes = std::fs::read(&epub_path).map_err(|e| format!("read {epub_path}: {e}"))?;
        omnibus::propose(&bytes, &reading).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())??;

    let existing_in_series = {
        let conn = state.db.lock().await;
        db::others_in_series(&conn, &plan.series_name, book_id).map_err(|e| e.to_string())?
    };

    Ok(SplitProposal {
        book_id,
        title: row.title,
        series_name: plan.series_name,
        volumes: plan.volumes,
        existing_in_series,
    })
}

/// Carry out a confirmed plan: write each volume, import it, and put the
/// omnibus in the series alongside them. Every new volume's conversion is
/// queued before this returns.
#[tauri::command]
pub async fn omnibus_split(
    app: AppHandle,
    state: State<'_, AppState>,
    book_id: i64,
    plan: SplitPlan,
) -> Result<SplitSummary, String> {
    let row = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "book not found".to_string())?
    };

    let db_handle = state.db.clone();
    let paths = state.paths.clone();
    let series_name = plan.series_name.trim().to_string();
    let total = plan.volumes.len();

    // Carving is the slow half and needs nothing from the library, so it runs
    // before the lock is taken — otherwise a large collection would hold the
    // database shut for the whole rebuild and stall every running conversion.
    let carving = row.clone();
    let carving_plan = plan.clone();
    let volumes = tokio::task::spawn_blocking(move || {
        omnibus::carve_volumes(&carving, &carving_plan).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())??;

    let app_progress = app.clone();
    let volumes = tokio::task::spawn_blocking(move || -> Result<Vec<VolumeOutcome>, String> {
        let conn = db_handle.blocking_lock();
        omnibus::add_volumes(&conn, &paths, &row, &plan, volumes, |done, title| {
            let _ = app_progress.emit(
                "library:split-progress",
                SplitProgress {
                    book_id,
                    done,
                    total,
                    title: title.to_string(),
                },
            );
        })
        .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())??;

    // Each fresh volume needs its EPUB→KFX conversion. Queued after the split
    // so the workers don't compete with it for the same cores.
    for volume in &volumes {
        if volume.needs_enqueue
            && let Some(id) = volume.book_id
        {
            let _ = state.queue.enqueue(id).await;
        }
    }

    // The omnibus's own row changed (it joined the series), and the frontend
    // reloads the whole list for the new volumes anyway.
    if let Some(updated) = {
        let conn = state.db.lock().await;
        db::get_book(&conn, book_id).map_err(|e| e.to_string())?
    } {
        let _ = app.emit("library:row-updated", &updated);
    }

    Ok(SplitSummary {
        series_name,
        volumes,
    })
}
