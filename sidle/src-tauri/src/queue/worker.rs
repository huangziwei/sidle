//! Conversion worker: runs boko-kai EPUB→KFX synchronously on a blocking thread.

use std::fs::File;
use std::path::PathBuf;

use tauri::AppHandle;

use crate::library::LibraryPaths;
use crate::library::db;
use crate::queue::emit_status;
use crate::state::DbHandle;

/// Run a single conversion job: mark `converting`, run boko, write file,
/// update `done` or `error`. Errors are recorded in the DB; never propagated
/// to the caller (this is a fire-and-forget worker).
pub async fn run_job(app: &AppHandle, db: &DbHandle, paths: &LibraryPaths, book_id: i64) {
    let Some((sha, source)) = lookup_paths(db, book_id).await else {
        eprintln!("[sidle/queue] book {book_id} vanished before conversion");
        return;
    };

    eprintln!("[sidle/queue] book {book_id} converting: {}", source.display());
    mark_status(db, app, book_id, "converting", None).await;

    let paths_owned = paths.clone();
    let sha_owned = sha.clone();
    let source_owned = source.clone();
    let started = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        convert_sync(&paths_owned, &sha_owned, &source_owned)
    })
    .await;

    match result {
        Ok(Ok(kfx_path)) => {
            let kfx_str = kfx_path.to_string_lossy().to_string();
            {
                let conn = db.lock().await;
                let _ = db::set_kfx_path(&conn, book_id, &kfx_str);
            }
            eprintln!(
                "[sidle/queue] book {book_id} done in {:.2}s -> {kfx_str}",
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

async fn lookup_paths(db: &DbHandle, book_id: i64) -> Option<(String, PathBuf)> {
    let conn = db.lock().await;
    let row = db::get_book(&conn, book_id).ok()??;
    Some((row.sha256, PathBuf::from(row.source_epub_path)))
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
        let _ = db::upsert_job(&conn, book_id, status, error);
    }
    emit_status(app, book_id, status, error);
}

fn convert_sync(
    paths: &LibraryPaths,
    sha: &str,
    source: &std::path::Path,
) -> anyhow::Result<PathBuf> {
    paths.ensure_sha(sha)?;
    let out_path = paths.kfx(sha);
    let tmp_path = paths.cache_dir(sha).join("book.kfx.partial");

    let mut book = boko::Book::open(source)?;
    let mut writer = File::create(&tmp_path)?;
    book.export(boko::Format::Kfx, &mut writer)?;
    writer.sync_all().ok();
    drop(writer);

    std::fs::rename(&tmp_path, &out_path)?;
    Ok(out_path)
}
