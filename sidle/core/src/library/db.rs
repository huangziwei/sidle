//! rusqlite-backed library database.
//!
//! Single-user, single-process. We hold one `Connection` behind an `Arc<Mutex>`
//! in `AppState`; rusqlite calls block but the library workload is tiny.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

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
    /// Amazon Standard Identification Number — populated from the KFX
    /// `book_id` field on import. Used by the color-cover fetch (the KFX
    /// itself ships with the grayscale cover Amazon serves to monochrome
    /// devices like the KOA2). `None` for EPUB-imported books and KFXes
    /// without a `book_id`.
    pub asin: Option<String>,
    /// Publisher imprint — pulled from EPUB `<dc:publisher>` or KFX
    /// metadata field `publisher` (symbol 232). Optional; many self-pub or
    /// indie books have no publisher. Editable via the metadata modal.
    pub publisher: Option<String>,
    /// Publication date as it appears in the source (EPUB `<dc:date>` or
    /// KFX equivalent). Stored verbatim — typically ISO 8601 (`2024-03-15`
    /// or just `2024`), but we don't enforce parsing. Sort works on the
    /// string, which gives correct chronological order for ISO 8601.
    pub published_at: Option<String>,
    pub series_name: Option<String>,
    /// Position within the series. REAL so half-numbers (1.5, 2.5) common
    /// in fiction series numbering work without coercion.
    pub series_index: Option<f64>,
    /// User-defined tags. Stored as a JSON array TEXT in SQLite; canonicalized
    /// (trimmed, lowercased, deduped in-order, empties dropped) at write time.
    pub tags: Vec<String>,
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
        conn.execute_batch("DROP TABLE IF EXISTS conversion_jobs; DROP TABLE IF EXISTS books;")?;
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
            imported_at       TEXT NOT NULL,
            asin              TEXT,
            publisher         TEXT,
            published_at      TEXT,
            series_name       TEXT,
            series_index      REAL,
            tags              TEXT NOT NULL DEFAULT '[]'
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
    )?;

    // Idempotent column adds for installs that already migrated past the
    // v1 schema. `CREATE IF NOT EXISTS` above is a no-op for an existing
    // table, so we have to ALTER out-of-band.
    if !has_column(conn, "books", "asin")? {
        conn.execute("ALTER TABLE books ADD COLUMN asin TEXT", [])?;
    }
    if !has_column(conn, "books", "publisher")? {
        conn.execute("ALTER TABLE books ADD COLUMN publisher TEXT", [])?;
    }
    if !has_column(conn, "books", "published_at")? {
        conn.execute("ALTER TABLE books ADD COLUMN published_at TEXT", [])?;
    }
    if !has_column(conn, "books", "series_name")? {
        conn.execute("ALTER TABLE books ADD COLUMN series_name TEXT", [])?;
    }
    if !has_column(conn, "books", "series_index")? {
        conn.execute("ALTER TABLE books ADD COLUMN series_index REAL", [])?;
    }
    if !has_column(conn, "books", "tags")? {
        conn.execute(
            "ALTER TABLE books ADD COLUMN tags TEXT NOT NULL DEFAULT '[]'",
            [],
        )?;
    }

    Ok(())
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
    conn.query_row(SELECT_BOOK_WITH_JOB_BY_SHA, params![sha], row_to_book)
        .optional()
}

pub fn list_books(conn: &Connection) -> rusqlite::Result<Vec<BookRow>> {
    let mut stmt = conn.prepare(SELECT_BOOKS_WITH_JOBS)?;
    let rows = stmt.query_map([], row_to_book)?;
    rows.collect()
}

pub fn insert_book(conn: &Connection, book: &NewBook<'_>) -> rusqlite::Result<i64> {
    let tags_json = serde_json::to_string(book.tags)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        r#"INSERT INTO books
            (sha256, title, author, language, ppd, epub_path, cover_path, kfx_path,
             file_size, imported_at, asin, publisher, published_at,
             series_name, series_index, tags)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"#,
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
            book.asin,
            book.publisher,
            book.published_at,
            book.series_name,
            book.series_index,
            tags_json,
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

/// Full-form metadata patch sent by the editor modal. Every field is
/// always present; the editor populates from the current row and the
/// user edits in place, so we don't need to distinguish "no-op" from
/// "clear" here.
///
/// Caller (the command layer) is responsible for validation (title
/// non-empty, series_index ≥ 0) and tag canonicalization (trim,
/// lowercase, dedupe, drop empties).
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct MetadataPatch {
    pub title: String,
    pub author: String,
    pub language: String,
    pub publisher: Option<String>,
    pub published_at: Option<String>,
    pub series_name: Option<String>,
    pub series_index: Option<f64>,
    pub tags: Vec<String>,
}

pub fn update_metadata(
    conn: &Connection,
    book_id: i64,
    patch: &MetadataPatch,
) -> rusqlite::Result<()> {
    let tags_json = serde_json::to_string(&patch.tags)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        r#"UPDATE books
              SET title         = ?1,
                  author        = ?2,
                  language      = ?3,
                  publisher     = ?4,
                  published_at  = ?5,
                  series_name   = ?6,
                  series_index  = ?7,
                  tags          = ?8
              WHERE id = ?9"#,
        params![
            patch.title,
            patch.author,
            patch.language,
            patch.publisher,
            patch.published_at,
            patch.series_name,
            patch.series_index,
            tags_json,
            book_id,
        ],
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
    let mut stmt = conn
        .prepare("SELECT book_id FROM conversion_jobs WHERE status IN ('pending', 'converting')")?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    rows.collect()
}

pub fn get_book(conn: &Connection, book_id: i64) -> rusqlite::Result<Option<BookRow>> {
    conn.query_row(SELECT_BOOK_WITH_JOB_BY_ID, params![book_id], row_to_book)
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
    pub asin: Option<&'a str>,
    pub publisher: Option<&'a str>,
    pub published_at: Option<&'a str>,
    pub series_name: Option<&'a str>,
    pub series_index: Option<f64>,
    /// Caller passes the canonical tag list (already trimmed / lowercased
    /// / deduped). `insert_book` serializes it to a JSON array TEXT.
    pub tags: &'a [String],
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

const SELECT_BOOKS_WITH_JOBS: &str = r#"
    SELECT b.id, b.sha256, b.title, b.author, b.language, b.ppd,
           b.epub_path, b.cover_path, b.kfx_path,
           b.file_size, b.imported_at, b.asin,
           COALESCE(j.status, 'pending') AS status, j.error, j.kind,
           b.publisher, b.published_at, b.series_name, b.series_index, b.tags
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    ORDER BY b.imported_at DESC
"#;

const SELECT_BOOK_WITH_JOB_BY_SHA: &str = r#"
    SELECT b.id, b.sha256, b.title, b.author, b.language, b.ppd,
           b.epub_path, b.cover_path, b.kfx_path,
           b.file_size, b.imported_at, b.asin,
           COALESCE(j.status, 'pending') AS status, j.error, j.kind,
           b.publisher, b.published_at, b.series_name, b.series_index, b.tags
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    WHERE b.sha256 = ?1
"#;

const SELECT_BOOK_WITH_JOB_BY_ID: &str = r#"
    SELECT b.id, b.sha256, b.title, b.author, b.language, b.ppd,
           b.epub_path, b.cover_path, b.kfx_path,
           b.file_size, b.imported_at, b.asin,
           COALESCE(j.status, 'pending') AS status, j.error, j.kind,
           b.publisher, b.published_at, b.series_name, b.series_index, b.tags
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    WHERE b.id = ?1
"#;

fn row_to_book(row: &rusqlite::Row<'_>) -> rusqlite::Result<BookRow> {
    let tags_json: String = row.get(19)?;
    // Defensive parse: we control writes and only emit canonical JSON
    // arrays, but a corrupt column shouldn't take down the whole list.
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
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
        asin: row.get(11)?,
        status: row.get(12)?,
        error: row.get(13)?,
        kind: row.get(14)?,
        publisher: row.get(15)?,
        published_at: row.get(16)?,
        series_name: row.get(17)?,
        series_index: row.get(18)?,
        tags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory");
        migrate(&conn).expect("migrate");
        conn
    }

    fn insert_minimal(conn: &Connection, sha: &str, title: &str) -> i64 {
        insert_book(
            conn,
            &NewBook {
                sha256: sha,
                title,
                author: "",
                language: "",
                ppd: None,
                epub_path: None,
                cover_path: None,
                kfx_path: None,
                file_size: 0,
                imported_at: "2026-05-19T00:00:00Z",
                asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
            },
        )
        .expect("insert")
    }

    #[test]
    fn update_metadata_sets_all_fields() {
        let conn = fresh_db();
        let id = insert_minimal(&conn, "sha-a", "original");

        let patch = MetadataPatch {
            title: "新しいタイトル".into(),
            author: "村上春樹".into(),
            language: "ja".into(),
            publisher: Some("新潮文庫".into()),
            published_at: Some("2024-03-15".into()),
            series_name: Some("ハルキ三部作".into()),
            series_index: Some(2.5),
            tags: vec!["小説".into(), "ライトノベル".into()],
        };
        update_metadata(&conn, id, &patch).expect("update");

        let row = get_book(&conn, id).expect("get").expect("present");
        assert_eq!(row.title, "新しいタイトル");
        assert_eq!(row.author, "村上春樹");
        assert_eq!(row.language, "ja");
        assert_eq!(row.publisher.as_deref(), Some("新潮文庫"));
        assert_eq!(row.published_at.as_deref(), Some("2024-03-15"));
        assert_eq!(row.series_name.as_deref(), Some("ハルキ三部作"));
        assert_eq!(row.series_index, Some(2.5));
        assert_eq!(row.tags, vec!["小説", "ライトノベル"]);
    }

    #[test]
    fn update_metadata_clears_series() {
        let conn = fresh_db();
        let id = insert_minimal(&conn, "sha-b", "x");

        // Seed with series populated.
        update_metadata(
            &conn,
            id,
            &MetadataPatch {
                title: "x".into(),
                author: "a".into(),
                language: "en".into(),
                publisher: None,
                published_at: None,
                series_name: Some("Foundation".into()),
                series_index: Some(1.0),
                tags: vec![],
            },
        )
        .expect("seed");

        // Then clear both.
        update_metadata(
            &conn,
            id,
            &MetadataPatch {
                title: "x".into(),
                author: "a".into(),
                language: "en".into(),
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: vec![],
            },
        )
        .expect("clear");

        let row = get_book(&conn, id).expect("get").expect("present");
        assert_eq!(row.series_name, None);
        assert_eq!(row.series_index, None);
    }

    #[test]
    fn publisher_set_and_clear() {
        let conn = fresh_db();
        let id = insert_minimal(&conn, "sha-d", "x");

        // Set.
        update_metadata(
            &conn,
            id,
            &MetadataPatch {
                title: "x".into(),
                author: "".into(),
                language: "".into(),
                publisher: Some("講談社文庫".into()),
                published_at: None,
                series_name: None,
                series_index: None,
                tags: vec![],
            },
        )
        .expect("set");
        let row = get_book(&conn, id).expect("get").expect("present");
        assert_eq!(row.publisher.as_deref(), Some("講談社文庫"));

        // Clear.
        update_metadata(
            &conn,
            id,
            &MetadataPatch {
                title: "x".into(),
                author: "".into(),
                language: "".into(),
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: vec![],
            },
        )
        .expect("clear");
        let row = get_book(&conn, id).expect("get").expect("present");
        assert_eq!(row.publisher, None);
    }

    #[test]
    fn tags_roundtrip_through_json_storage() {
        let conn = fresh_db();
        let id = insert_minimal(&conn, "sha-c", "x");

        // Empty default on a freshly-inserted book.
        let row = get_book(&conn, id).expect("get").expect("present");
        assert!(row.tags.is_empty());

        // CJK + emoji + ASCII; verify nothing gets escaped or lost.
        let tags = vec!["sci-fi".into(), "小説".into(), "🦀rust".into()];
        update_metadata(
            &conn,
            id,
            &MetadataPatch {
                title: "x".into(),
                author: "".into(),
                language: "".into(),
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: tags.clone(),
            },
        )
        .expect("update");

        let row = get_book(&conn, id).expect("get").expect("present");
        assert_eq!(row.tags, tags);
    }
}
