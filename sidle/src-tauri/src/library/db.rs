//! rusqlite-backed library database.
//!
//! Single-user, single-process. We hold one `Connection` behind an `Arc<Mutex>`
//! in `AppState`; rusqlite calls block but the library workload is tiny.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct BookRow {
    pub id: i64,
    pub sha256: String,
    pub title: String,
    pub author: String,
    pub language: String,
    pub ppd: Option<String>,
    /// Path to the EPUB on disk. `None` while a KFX-imported book is still
    /// awaiting its background EPUB conversion.
    pub epub_path: Option<String>,
    pub cover_path: Option<String>,
    /// Path to the KFX on disk. `None` while an EPUB-imported book is still
    /// awaiting its background KFX conversion.
    pub kfx_path: Option<String>,
    pub file_size: i64,
    pub imported_at: String,
    pub status: String,
    pub error: Option<String>,
    /// Direction of the background conversion job — `"epub_to_kfx"` for EPUB
    /// imports, `"kfx_to_epub"` for KFX imports. `None` only on a transient
    /// state where the row exists without a job (shouldn't happen in normal
    /// flow but `LEFT JOIN` makes it representable).
    pub kind: Option<String>,
}

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    migrate(&conn)?;
    Ok(conn)
}

/// Schema setup.
///
/// No production data yet, so we don't migrate v1 schemas — if we spot the
/// old `source_epub_path` column we just drop `books` + `conversion_jobs`
/// and rebuild fresh. The CREATE block below is then the source of truth.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    if has_column(conn, "books", "source_epub_path")? {
        // FK from conversion_jobs.book_id blocks the DROP order without
        // foreign_keys off. Dropping conversion_jobs first works too; we go
        // with foreign_keys=OFF for symmetry with any future similar reset.
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        conn.execute_batch(
            "DROP TABLE IF EXISTS conversion_jobs; DROP TABLE IF EXISTS books;",
        )?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
    }

    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS books (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            sha256            TEXT NOT NULL UNIQUE,
            title             TEXT NOT NULL,
            author            TEXT NOT NULL DEFAULT '',
            language          TEXT NOT NULL DEFAULT '',
            ppd               TEXT,
            epub_path         TEXT,
            cover_path        TEXT,
            kfx_path          TEXT,
            file_size         INTEGER NOT NULL,
            imported_at       TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS conversion_jobs (
            book_id     INTEGER PRIMARY KEY REFERENCES books(id) ON DELETE CASCADE,
            status      TEXT NOT NULL,
            error       TEXT,
            attempts    INTEGER NOT NULL DEFAULT 0,
            kind        TEXT NOT NULL,  -- 'epub_to_kfx' | 'kfx_to_epub'
            updated_at  TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_jobs_status ON conversion_jobs(status);

        CREATE TABLE IF NOT EXISTS device_history (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            device_serial   TEXT NOT NULL,
            sha256          TEXT NOT NULL,
            action          TEXT NOT NULL,  -- 'push' | 'delete' (P2b: 'pull')
            device_path     TEXT NOT NULL,
            ts              TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_device_history_serial
            ON device_history(device_serial);
        CREATE INDEX IF NOT EXISTS idx_device_history_sha
            ON device_history(sha256);
        "#,
    )
}

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn record_device_action(
    conn: &Connection,
    device_serial: &str,
    sha256: &str,
    action: &str,
    device_path: &str,
) -> rusqlite::Result<()> {
    let now = now_iso();
    conn.execute(
        r#"INSERT INTO device_history (device_serial, sha256, action, device_path, ts)
           VALUES (?1, ?2, ?3, ?4, ?5)"#,
        params![device_serial, sha256, action, device_path, now],
    )?;
    Ok(())
}

/// True if a job for this book is currently pending or converting.
/// Used to gate device send/delete while work is in flight.
pub fn job_in_flight(conn: &Connection, book_id: i64) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM conversion_jobs
         WHERE book_id = ?1 AND status IN ('pending', 'converting')",
        params![book_id],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

pub fn find_by_sha(conn: &Connection, sha: &str) -> rusqlite::Result<Option<BookRow>> {
    conn.query_row(
        SELECT_BOOK_WITH_JOB_BY_SHA,
        params![sha],
        row_to_book,
    )
    .optional()
}

pub fn list_books(conn: &Connection) -> rusqlite::Result<Vec<BookRow>> {
    let mut stmt = conn.prepare(SELECT_BOOKS_WITH_JOBS)?;
    let rows = stmt.query_map([], row_to_book)?;
    rows.collect()
}

pub fn insert_book(conn: &Connection, book: &NewBook<'_>) -> rusqlite::Result<i64> {
    conn.execute(
        r#"INSERT INTO books
            (sha256, title, author, language, ppd, epub_path, cover_path, kfx_path, file_size, imported_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
        params![
            book.sha256,
            book.title,
            book.author,
            book.language,
            book.ppd,
            book.epub_path,
            book.cover_path,
            book.kfx_path,
            book.file_size,
            book.imported_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Create or replace the job row for a book. `kind` is set on first insert
/// and preserved on subsequent status updates — the worker doesn't need to
/// know which direction it ran when it reports back.
pub fn insert_job(
    conn: &Connection,
    book_id: i64,
    status: &str,
    kind: &str,
) -> rusqlite::Result<()> {
    let now = now_iso();
    conn.execute(
        r#"INSERT INTO conversion_jobs (book_id, status, error, attempts, kind, updated_at)
            VALUES (?1, ?2, NULL, 0, ?3, ?4)
            ON CONFLICT(book_id) DO UPDATE SET
                status = excluded.status,
                error = NULL,
                kind = excluded.kind,
                updated_at = excluded.updated_at,
                attempts = 0
            "#,
        params![book_id, status, kind, now],
    )?;
    Ok(())
}

/// Update an existing job's status. Leaves `kind` alone.
pub fn set_job_status(
    conn: &Connection,
    book_id: i64,
    status: &str,
    error: Option<&str>,
) -> rusqlite::Result<()> {
    let now = now_iso();
    conn.execute(
        r#"UPDATE conversion_jobs
            SET status = ?2,
                error = ?3,
                updated_at = ?4,
                attempts = CASE WHEN ?2 = 'converting' THEN attempts + 1 ELSE attempts END
            WHERE book_id = ?1"#,
        params![book_id, status, error, now],
    )?;
    Ok(())
}

pub fn set_kfx_path(conn: &Connection, book_id: i64, kfx_path: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE books SET kfx_path = ?1 WHERE id = ?2",
        params![kfx_path, book_id],
    )?;
    Ok(())
}

pub fn set_epub_path(conn: &Connection, book_id: i64, epub_path: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE books SET epub_path = ?1 WHERE id = ?2",
        params![epub_path, book_id],
    )?;
    Ok(())
}

pub fn set_cover_path(conn: &Connection, book_id: i64, cover_path: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE books SET cover_path = ?1 WHERE id = ?2",
        params![cover_path, book_id],
    )?;
    Ok(())
}

pub fn remove_book(conn: &Connection, book_id: i64) -> rusqlite::Result<Option<String>> {
    let sha: Option<String> = conn
        .query_row(
            "SELECT sha256 FROM books WHERE id = ?1",
            params![book_id],
            |r| r.get(0),
        )
        .optional()?;
    if sha.is_some() {
        conn.execute("DELETE FROM books WHERE id = ?1", params![book_id])?;
    }
    Ok(sha)
}

pub fn pending_or_error_book_ids(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT book_id FROM conversion_jobs WHERE status IN ('pending', 'converting')",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    rows.collect()
}

pub fn get_book(conn: &Connection, book_id: i64) -> rusqlite::Result<Option<BookRow>> {
    conn.query_row(
        SELECT_BOOK_WITH_JOB_BY_ID,
        params![book_id],
        row_to_book,
    )
    .optional()
}

pub struct NewBook<'a> {
    pub sha256: &'a str,
    pub title: &'a str,
    pub author: &'a str,
    pub language: &'a str,
    pub ppd: Option<&'a str>,
    pub epub_path: Option<&'a str>,
    pub cover_path: Option<&'a str>,
    pub kfx_path: Option<&'a str>,
    pub file_size: i64,
    pub imported_at: &'a str,
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

const SELECT_BOOKS_WITH_JOBS: &str = r#"
    SELECT b.id, b.sha256, b.title, b.author, b.language, b.ppd,
           b.epub_path, b.cover_path, b.kfx_path,
           b.file_size, b.imported_at,
           COALESCE(j.status, 'pending') AS status, j.error, j.kind
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    ORDER BY b.imported_at DESC
"#;

const SELECT_BOOK_WITH_JOB_BY_SHA: &str = r#"
    SELECT b.id, b.sha256, b.title, b.author, b.language, b.ppd,
           b.epub_path, b.cover_path, b.kfx_path,
           b.file_size, b.imported_at,
           COALESCE(j.status, 'pending') AS status, j.error, j.kind
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    WHERE b.sha256 = ?1
"#;

const SELECT_BOOK_WITH_JOB_BY_ID: &str = r#"
    SELECT b.id, b.sha256, b.title, b.author, b.language, b.ppd,
           b.epub_path, b.cover_path, b.kfx_path,
           b.file_size, b.imported_at,
           COALESCE(j.status, 'pending') AS status, j.error, j.kind
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    WHERE b.id = ?1
"#;

fn row_to_book(row: &rusqlite::Row<'_>) -> rusqlite::Result<BookRow> {
    Ok(BookRow {
        id: row.get(0)?,
        sha256: row.get(1)?,
        title: row.get(2)?,
        author: row.get(3)?,
        language: row.get(4)?,
        ppd: row.get(5)?,
        epub_path: row.get(6)?,
        cover_path: row.get(7)?,
        kfx_path: row.get(8)?,
        file_size: row.get(9)?,
        imported_at: row.get(10)?,
        status: row.get(11)?,
        error: row.get(12)?,
        kind: row.get(13)?,
    })
}
