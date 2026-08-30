//! Tauri commands backing the "Reading Log" page — reading time recovered from
//! a Kindle's own system logs (see [`sidle_core::library::reading_log`]).
//!
//! Read-only over `reading_sessions`, apart from `books.finished_at` and the
//! attribution a tie settles.

use serde::Serialize;
use tauri::State;

use sidle_core::library::db;

use crate::state::AppState;

/// A day's total, for the calendar heatmap.
#[derive(Debug, Serialize)]
pub struct ReadingDay {
    /// `YYYY-MM-DD`, device-local.
    pub day: String,
    pub seconds: i64,
}

/// Everything the Reading Log page needs on open: every day ever read, and the
/// all-time headline totals.
///
/// The book grid is **not** here — it is scoped to whatever the heatmap is
/// showing (a year, or one day of it) and comes from [`reading_log_books`],
/// because a book's time within a window is a different number from its time
/// ever and cannot be derived by filtering an all-time list.
///
/// Every figure here covers books the library actually holds. Sessions whose
/// book is gone are counted nowhere — see
/// [`db::resolve_reading_sessions`].
#[derive(Debug, Serialize)]
pub struct ReadingOverview {
    /// All of them, every year: the heatmap draws one year at a time, but which
    /// years exist at all is what the year arrows navigate by.
    pub days: Vec<ReadingDay>,
    /// Distinct books ever read — the headline count, which stays all-time
    /// while the grid below it follows the selected window.
    pub books_total: i64,
    /// Of `books_total`, how many [`db::is_finished`] holds for.
    pub finished_total: i64,
    pub total_seconds: i64,
    /// Days with any reading at all — the denominator behind "you read on N of
    /// the last M days".
    pub days_read: i64,
    /// Longest run of consecutive days with reading, and the run ending today
    /// (or yesterday, so a day still in progress doesn't read as a broken
    /// streak).
    pub longest_streak: i64,
    pub current_streak: i64,
    /// When in the day the reading happened, as a (month, weekday, hour) cube —
    /// see [`db::reading_clock`]. All-time like `days`, and for the same reason:
    /// hour-seconds are additive, so the page sums the months of whichever year
    /// the heatmap is showing rather than asking again per year.
    pub clock: Vec<db::ClockCell>,
}

/// One book's page: per-day totals plus the aggregate.
#[derive(Debug, Serialize)]
pub struct ReadingBook {
    pub days: Vec<ReadingDay>,
    pub entry: Option<db::ReadingEntry>,
    /// The book's place on its own axis, absent where either half is unstored.
    pub progress: Option<db::BookProgress>,
    /// [`db::is_finished`] for this book.
    pub finished: bool,
    /// `books.finished_at`, set only by [`reading_log_set_finished`].
    pub finished_at: Option<String>,
}

/// Mark a book read, or take the mark off. `books.finished_at` outranks the
/// position, which an index or an afterword leaves short of the axis end.
#[tauri::command]
pub async fn reading_log_set_finished(
    state: State<'_, AppState>,
    book_id: i64,
    finished: bool,
) -> Result<(), String> {
    let conn = state.db.lock().await;
    db::set_book_finished(&conn, book_id, finished).map_err(|e| e.to_string())
}

/// The sittings over `[from, to]`, earliest first.
#[tauri::command]
pub async fn reading_log_sessions(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<Vec<db::SessionRow>, String> {
    let conn = state.db.lock().await;
    db::reading_sessions_on(&conn, &from, &to).map_err(|e| e.to_string())
}

/// The clock hours of each day over `[from, to]`.
#[tauri::command]
pub async fn reading_log_day_hours(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<Vec<db::DayShape>, String> {
    let conn = state.db.lock().await;
    db::reading_day_hours(&conn, &from, &to).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn reading_log_overview(state: State<'_, AppState>) -> Result<ReadingOverview, String> {
    let conn = state.db.lock().await;
    // An open range: the page decides which window to draw, and the whole
    // history is a few thousand rows at most.
    let days = db::reading_days(&conn, ALL_TIME.0, ALL_TIME.1).map_err(|e| e.to_string())?;
    let books_total = db::reading_book_count(&conn).map_err(|e| e.to_string())?;
    let finished_total = db::reading_finished_count(&conn).map_err(|e| e.to_string())?;
    let clock = db::reading_clock(&conn).map_err(|e| e.to_string())?;

    let total_seconds = days.iter().map(|(_, s)| s).sum();
    let (longest_streak, current_streak) = streaks(days.iter().map(|(d, _)| d.as_str()));

    Ok(ReadingOverview {
        days_read: days.len() as i64,
        days: days
            .into_iter()
            .map(|(day, seconds)| ReadingDay { day, seconds })
            .collect(),
        books_total,
        finished_total,
        total_seconds,
        longest_streak,
        current_streak,
        clock,
    })
}

/// A range wide enough to mean "ever" against `YYYY-MM-DD` day keys.
const ALL_TIME: (&str, &str) = ("0000-00-00", "9999-99-99");

/// What was read over `[from, to]` (inclusive, `YYYY-MM-DD`).
///
/// The page's one book query, at whatever scope is selected: a year while the
/// heatmap is showing one, a single day once a square is clicked. `sort` names
/// a column ([`db::ReadingSort`]) and defaults, like the page, to most recently
/// read first — a reading log is a record of what you are reading now.
///
/// `bucket` ([`db::ReadingBucket`]) subdivides the window so the same year can
/// be shown whole or split into months or days; it defaults to undivided.
#[tauri::command]
pub async fn reading_log_books(
    state: State<'_, AppState>,
    from: String,
    to: String,
    sort: Option<String>,
    asc: Option<bool>,
    bucket: Option<String>,
) -> Result<Vec<db::ReadingEntry>, String> {
    let conn = state.db.lock().await;
    let sort = db::ReadingSort::from_name(sort.as_deref().unwrap_or_default());
    let bucket = db::ReadingBucket::from_name(bucket.as_deref().unwrap_or_default());
    db::reading_books(&conn, &from, &to, sort, asc.unwrap_or(false), bucket)
        .map_err(|e| e.to_string())
}

/// One book's reading history.
#[tauri::command]
pub async fn reading_log_book(
    state: State<'_, AppState>,
    book_id: i64,
) -> Result<ReadingBook, String> {
    let conn = state.db.lock().await;
    let days = db::reading_book_days(&conn, book_id).map_err(|e| e.to_string())?;
    // Undivided: this page wants the book's whole history as one aggregate, not
    // a row per year of it.
    let entry = db::reading_books(
        &conn,
        ALL_TIME.0,
        ALL_TIME.1,
        db::ReadingSort::default(),
        false,
        db::ReadingBucket::Total,
    )
    .map_err(|e| e.to_string())?
    .into_iter()
    .find(|b| b.book_id == book_id);
    let progress = db::book_progress(&conn, book_id).map_err(|e| e.to_string())?;
    let finished_at = db::book_finished_at(&conn, book_id).map_err(|e| e.to_string())?;
    let finished = db::is_finished(
        progress.as_ref().map(|p| p.fraction),
        finished_at.as_deref(),
    );
    Ok(ReadingBook {
        days: days
            .into_iter()
            .map(|(day, seconds)| ReadingDay { day, seconds })
            .collect(),
        entry,
        progress,
        finished,
        finished_at,
    })
}

/// Reading that several books could equally be, and those books.
///
/// `candidates` are the books whose axis ends exactly where this reading
/// stopped. There are always at least two — that tie is the only reason the
/// automatic pass left it alone, and it is the whole of what a person needs to
/// answer it: the covers of two or three books, one of which they read.
#[derive(Debug, Serialize)]
pub struct AmbiguousReading {
    #[serde(flatten)]
    pub reading: db::UnmatchedReading,
    pub candidates: Vec<db::BookRow>,
}

/// The reading a person can still resolve: every unattributed position that
/// **several** library books end at.
///
/// A position no book ends at is deliberately absent, and is not a lesser
/// version of this case. It means the book is not in the library — deleted, or
/// never imported — and nothing about the group says which book it was: a
/// duration, a date span and a word count identify nothing. Offering it as a
/// choice would be inviting a guess, so it stays where it already is: kept as a
/// row, counted nowhere, and named the moment its book comes back and
/// [`db::resolve_reading_sessions`] can see it.
#[tauri::command]
pub async fn reading_log_ambiguous(
    state: State<'_, AppState>,
) -> Result<Vec<AmbiguousReading>, String> {
    let conn = state.db.lock().await;
    let groups = db::unmatched_reading(&conn).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for reading in groups {
        let ids =
            db::books_with_last_position(&conn, reading.end_position).map_err(|e| e.to_string())?;
        if ids.len() < 2 {
            continue;
        }
        let mut candidates = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(book) = db::get_book(&conn, id).map_err(|e| e.to_string())? {
                candidates.push(book);
            }
        }
        // A candidate whose row vanished between the two queries would leave a
        // tie of one, which is not a question worth asking.
        if candidates.len() > 1 {
            out.push(AmbiguousReading {
                reading,
                candidates,
            });
        }
    }
    Ok(out)
}

/// Settle a tied position: `book_id` takes every unattributed session that
/// stopped there. Returns how many sessions moved.
///
/// The user's answer to a question only they can answer, so it is final — the
/// position leaves [`reading_log_ambiguous`] and the reading is that book's from
/// then on.
#[tauri::command]
pub async fn reading_log_attribute(
    state: State<'_, AppState>,
    end_position: i64,
    book_id: i64,
) -> Result<usize, String> {
    let conn = state.db.lock().await;
    db::attribute_reading_position(&conn, end_position, book_id).map_err(|e| e.to_string())
}

/// Throw the whole reading log away.
///
/// Everything, both tables: sessions and the record of which snapshots produced
/// them. Anything short of that leaves a library that shows no reading and also
/// refuses to import the archives back.
///
/// Not recoverable, whatever archives happen to be on disk. A device sends only
/// what is newer than the newest session stored and clears its own copy at that
/// mark, so reading that arrived from a Kindle goes with these rows; only days a
/// `logbackup` snapshot still covers can be read again.
#[tauri::command]
pub async fn reading_log_clear(state: State<'_, AppState>) -> Result<usize, String> {
    let conn = state.db.lock().await;
    db::clear_reading_log(&conn).map_err(|e| e.to_string())
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
