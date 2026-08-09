//! Tauri commands backing the "Reading Log" page — reading time recovered from
//! a Kindle's own system logs (see [`sidle_core::library::reading_log`]).
//!
//! Read-only over `reading_sessions`, plus the one write: importing an archive
//! of `logbackup` dumps the user points at. Import is always user-initiated —
//! the logs can live anywhere on disk, and the first pass has to index the whole
//! library's position axes, which takes minutes and must never happen behind a
//! library open.

use serde::Serialize;
use tauri::State;

use sidle_core::library::{db, extent, reading_log};

use crate::state::AppState;

/// A day's total, for the calendar heatmap.
#[derive(Debug, Serialize)]
pub struct ReadingDay {
    /// `YYYY-MM-DD`, device-local.
    pub day: String,
    pub seconds: i64,
}

/// Everything the Reading Log page needs on open: the heatmap, the all-time
/// per-book table, and the headline totals.
#[derive(Debug, Serialize)]
pub struct ReadingOverview {
    pub days: Vec<ReadingDay>,
    pub books: Vec<db::ReadingEntry>,
    pub total_seconds: i64,
    /// Days with any reading at all — the denominator behind "you read on N of
    /// the last M days".
    pub days_read: i64,
    /// Longest run of consecutive days with reading, and the run ending today
    /// (or yesterday, so a day still in progress doesn't read as a broken
    /// streak).
    pub longest_streak: i64,
    pub current_streak: i64,
    /// Seconds whose book could not be identified. Shown rather than hidden:
    /// it is usually a book that was deleted or re-converted, and importing it
    /// would name this time retroactively.
    pub unattributed_seconds: i64,
}

/// One book's page: per-day totals plus the aggregate.
#[derive(Debug, Serialize)]
pub struct ReadingBook {
    pub days: Vec<ReadingDay>,
    pub entry: Option<db::ReadingEntry>,
}

#[tauri::command]
pub async fn reading_log_overview(state: State<'_, AppState>) -> Result<ReadingOverview, String> {
    let conn = state.db.lock().await;
    // An open range: the page decides which window to draw, and the whole
    // history is a few thousand rows at most.
    let days = db::reading_days(&conn, "0000-00-00", "9999-99-99").map_err(|e| e.to_string())?;
    let books = db::reading_books(&conn).map_err(|e| e.to_string())?;

    let total_seconds = days.iter().map(|(_, s)| s).sum();
    let unattributed_seconds = books
        .iter()
        .filter(|b| b.book_id.is_none())
        .map(|b| b.seconds)
        .sum();
    let (longest_streak, current_streak) = streaks(days.iter().map(|(d, _)| d.as_str()));

    Ok(ReadingOverview {
        days_read: days.len() as i64,
        days: days
            .into_iter()
            .map(|(day, seconds)| ReadingDay { day, seconds })
            .collect(),
        books,
        total_seconds,
        longest_streak,
        current_streak,
        unattributed_seconds,
    })
}

/// What was read on one day, longest first.
#[tauri::command]
pub async fn reading_log_day(
    state: State<'_, AppState>,
    day: String,
) -> Result<Vec<db::ReadingEntry>, String> {
    let conn = state.db.lock().await;
    db::reading_day_detail(&conn, &day).map_err(|e| e.to_string())
}

/// One book's reading history.
#[tauri::command]
pub async fn reading_log_book(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<ReadingBook, String> {
    let conn = state.db.lock().await;
    let days = db::reading_book_days(&conn, book_id).map_err(|e| e.to_string())?;
    let entry = db::reading_books(&conn)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|b| b.book_id == Some(book_id));
    Ok(ReadingBook {
        days: days
            .into_iter()
            .map(|(day, seconds)| ReadingDay { day, seconds })
            .collect(),
        entry,
    })
}

/// Import `logbackup` dumps the user picked, then attribute what can be
/// attributed.
///
/// The extent index is built first because attribution is meaningless without
/// it. That is the slow half — minutes across a large library on the first run,
/// nothing on every run after — which is exactly why this is a button and not
/// something that happens at startup.
#[tauri::command]
pub async fn reading_log_import(
    state: State<'_, AppState>,
    paths: Vec<String>,
    device_serial: Option<String>,
) -> Result<reading_log::Imported, String> {
    let conn = state.db.lock().await;
    extent::backfill(&conn).map_err(|e| e.to_string())?;
    reading_log::import(&conn, &paths, device_serial.as_deref().unwrap_or_default())
        .map_err(|e| e.to_string())
}

/// Ask for the folders holding `logbackup` dumps.
///
/// A folder picker rather than a file one: `logbackup` is a directory of daily
/// dumps and pointing at it is the natural gesture — [`reading_log_import`]
/// walks whatever it is given. Lives in Rust for the same reason
/// `library_pick_folder` does: vanilla JS cannot import the dialog plugin's
/// module.
#[tauri::command]
pub async fn reading_log_pick_folders(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folders(move |paths| {
        let _ = tx.send(paths);
    });
    let picked = rx.await.map_err(|e| e.to_string())?;
    Ok(picked
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.to_string())
        .collect())
}

/// Longest and current run of consecutive days, from an ascending list of
/// `YYYY-MM-DD`.
///
/// "Current" counts back from the most recent day read rather than from today,
/// and is reported only when that day is today or yesterday — a streak should
/// not break at midnight while the evening's reading is still unsynced.
fn streaks<'a>(days: impl IntoIterator<Item = &'a str>) -> (i64, i64) {
    let parsed: Vec<chrono::NaiveDate> = days
        .into_iter()
        .filter_map(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .collect();
    let (mut longest, mut run) = (0, 0);
    let mut prev: Option<chrono::NaiveDate> = None;
    for day in &parsed {
        run = match prev {
            Some(p) if (*day - p).num_days() == 1 => run + 1,
            _ => 1,
        };
        longest = longest.max(run);
        prev = Some(*day);
    }
    let today = chrono::Local::now().date_naive();
    let current = match prev {
        Some(last) if (today - last).num_days() <= 1 => run,
        _ => 0,
    };
    (longest, current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streak_needs_consecutive_days() {
        let (longest, _) = streaks(["2026-08-01", "2026-08-02", "2026-08-03", "2026-08-05"]);
        assert_eq!(longest, 3);
    }

    #[test]
    fn a_month_boundary_does_not_break_a_streak() {
        let (longest, _) = streaks(["2026-07-30", "2026-07-31", "2026-08-01"]);
        assert_eq!(longest, 3);
    }

    #[test]
    fn a_stale_history_reports_no_current_streak() {
        // Days long past are a streak that ended, not one in progress.
        let (longest, current) = streaks(["2020-01-01", "2020-01-02"]);
        assert_eq!((longest, current), (2, 0));
    }

    #[test]
    fn yesterday_still_counts_as_current() {
        // Today's reading may not have synced yet, so the streak must survive
        // midnight rather than resetting every morning.
        let y = chrono::Local::now().date_naive() - chrono::Duration::days(1);
        let d = y.format("%Y-%m-%d").to_string();
        assert_eq!(streaks([d.as_str()]).1, 1);
    }
}
