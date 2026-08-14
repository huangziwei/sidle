//! Conversion worker: runs the library's conversion pipeline on a blocking
//! thread and reports what it is doing to the window.
//!
//! The conversion itself — both directions, the cover-enrichment tail, the DB
//! write-back, the re-anchor — is `sidle_core::library::convert`, which the CLI
//! drives too. What lives here is what the desktop adds: the app's one database
//! connection, taken only for the short half of the job, and
//! `conversion:status` / `conversion:progress` events for the gallery's
//! per-row bar.

use sidle_core::library::convert::{self, Mode};
use sidle_core::library::progress;
use tauri::AppHandle;

use crate::library::LibraryPaths;
use crate::library::db::{self, BookRow};
use crate::queue::{emit_progress, emit_status};
use crate::state::DbHandle;

/// Run a single conversion job: mark `converting`, convert, write the results
/// back, mark `done`/`error`. Errors are recorded in the DB; never propagated to
/// the caller (this is a fire-and-forget worker).
///
/// `reconvert` = a forced re-run of the format conversion (the "Force
/// re-convert" button), as opposed to a first import. See [`Mode`].
pub async fn run_job(
    app: &AppHandle,
    db: &DbHandle,
    paths: &LibraryPaths,
    book_id: i64,
    reconvert: bool,
) {
    let Some(book) = lookup_book(db, book_id).await else {
        eprintln!("[sidle/queue] book {book_id} vanished before conversion");
        return;
    };
    let Some(kind) = book.kind.clone() else {
        eprintln!("[sidle/queue] book {book_id} has no job kind; skipping");
        return;
    };

    eprintln!("[sidle/queue] book {book_id} converting ({kind})");
    mark_status(db, app, book_id, "converting", None).await;

    let paths = paths.clone();
    let db_owned = db.clone();
    let app_owned = app.clone();
    let mode = if reconvert {
        Mode::Reconvert
    } else {
        Mode::Import
    };
    let started = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        // Map the pipeline's per-phase reports → a monotonic 0–1 fraction
        // (weighted per direction) and emit a throttled `conversion:progress`.
        let throttle = progress::Throttle::new();
        let on_progress = |phase: &str, cur: usize, total: usize, label: &str| {
            let f = progress::fraction(&kind, phase, cur, total);
            if throttle.worth_emitting(f) {
                emit_progress(&app_owned, book_id, f, label);
            }
        };
        // The database is taken only once the slow half is finished: `run` is
        // minutes of CPU and holds nothing, `record` is a handful of writes.
        // Both stay on this thread — the text index `run` builds for the
        // re-anchor is not `Send`.
        let produced = convert::run(&paths, &book, &kind, mode, &on_progress)?;
        let conn = db_owned.blocking_lock();
        convert::record(&conn, &book, produced)
    })
    .await;

    match result {
        Ok(Ok(done)) => {
            if done.reanchored > 0 || done.stranded > 0 {
                eprintln!(
                    "[sidle/queue] book {book_id}: re-anchored {} annotation(s), \
                     {} left where they were (text not found, or found in several places)",
                    done.reanchored, done.stranded
                );
            }
            eprintln!(
                "[sidle/queue] book {book_id} done in {:.2}s",
                started.elapsed().as_secs_f32()
            );
            mark_status(db, app, book_id, "done", None).await;
        }
        Ok(Err(e)) => {
            let msg = format!("{e:#}");
            eprintln!("[sidle/queue] book {book_id} error: {msg}");
            mark_status(db, app, book_id, "error", Some(&msg)).await;
        }
        Err(join_err) => {
            let msg = format!("worker panicked: {join_err}");
            eprintln!("[sidle/queue] book {book_id} PANIC: {msg}");
            mark_status(db, app, book_id, "error", Some(&msg)).await;
        }
    }
}

async fn lookup_book(db: &DbHandle, book_id: i64) -> Option<BookRow> {
    let conn = db.lock().await;
    db::get_book(&conn, book_id).ok().flatten()
}

async fn mark_status(
    db: &DbHandle,
    app: &AppHandle,
    book_id: i64,
    status: &str,
    error: Option<&str>,
) {
    {
        let conn = db.lock().await;
        let _ = db::set_job_status(&conn, book_id, status, error);
    }
    emit_status(app, book_id, status, error);
}
