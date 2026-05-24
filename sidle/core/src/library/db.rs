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
    /// SHA-256 of the KFX file's bytes. Distinct from `sha256` (which is the
    /// hash of whatever the user originally imported — `.epub`, `.kfx-zip`,
    /// `.azw3`, `.mobi`) because the generated/extracted `.kfx` is rarely byte-
    /// identical to the source. The on-device filename infix is derived
    /// from THIS hash, so when an orphan KFX is re-imported from the Kindle
    /// the new row's identity matches what's already in the filename.
    /// `Some(_)` iff `kfx_path` is `Some(_)` — they're always written
    /// together.
    pub kfx_sha256: Option<String>,
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
/// No production data yet, so we don't migrate — if we spot any artefact of
/// a prior schema (`source_epub_path` column on books, the `device_history`
/// table) we drop the lot and rebuild fresh from the CREATE block below,
/// which is the only source of truth.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let needs_reset = has_column(conn, "books", "source_epub_path")?
        || has_table(conn, "device_history")?;
    if needs_reset {
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        conn.execute_batch(
            "DROP TABLE IF EXISTS device_history;
             DROP TABLE IF EXISTS conversion_jobs;
             DROP TABLE IF EXISTS books;",
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
            kfx_sha256        TEXT,
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
    if !has_column(conn, "books", "kfx_sha256")? {
        conn.execute("ALTER TABLE books ADD COLUMN kfx_sha256 TEXT", [])?;
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

fn has_table(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        params![table],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// Look up the book whose KFX hash starts with `prefix`. Used by
/// `device_list_ours` to link an on-device `<basename>.<sha8>.kfx` back
/// to a library row: the sha8 in the filename was generated from the
/// KFX bytes (`kfx_sha256`), not from the source file (`sha256`). For
/// .epub-imported books the two differ.
pub fn find_by_kfx_sha_prefix(
    conn: &Connection,
    prefix: &str,
) -> rusqlite::Result<Option<BookRow>> {
    let pattern = format!("{prefix}%");
    conn.query_row(
        SELECT_BOOK_WITH_JOB_BY_KFX_SHA_PREFIX,
        params![pattern],
        row_to_book,
    )
    .optional()
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
             series_name, series_index, tags, kfx_sha256)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)"#,
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
            book.kfx_sha256,
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

/// Set the KFX path and its content hash atomically. Both columns are
/// always written together — the push pipeline reads `kfx_sha256` to
/// build the on-device filename, so a row that has `kfx_path` but no
/// `kfx_sha256` would be unsendable.
pub fn set_kfx_path_and_sha(
    conn: &Connection,
    book_id: i64,
    kfx_path: &str,
    kfx_sha256: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE books SET kfx_path = ?1, kfx_sha256 = ?2 WHERE id = ?3",
        params![kfx_path, kfx_sha256, book_id],
    )?;
    Ok(())
}

/// Bootstrap-time fixup. Returns `(book_id, kfx_path)` pairs for rows
/// that have a KFX file on disk but no recorded hash — the bootstrap
/// hashes each and writes it back via `set_kfx_path_and_sha`. Exists
/// purely for upgrades from a pre-`kfx_sha256` schema; new rows always
/// land with the hash already set.
pub fn books_missing_kfx_sha(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, kfx_path FROM books \
         WHERE kfx_path IS NOT NULL AND kfx_sha256 IS NULL",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    rows.collect()
}

/// Find rows with a `kfx_path` but no `asin`. Bootstrap reads each KFX's
/// metadata to recover the value boko-kai stamped at export time — the
/// device-delete path needs it to wipe Kindle's `<title>_<ASIN>.sdr/`
/// catalog sidecar. Exists purely for rows converted before the worker
/// started capturing ASIN; new rows always land with `asin` populated.
pub fn books_missing_asin(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, kfx_path FROM books \
         WHERE kfx_path IS NOT NULL AND (asin IS NULL OR asin = '')",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    rows.collect()
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

/// Stamp the ASIN that boko-kai's KFX export wrote into the produced file.
/// For PDOC sideloads the value is fabricated from the publication
/// identifier (32-char Crockford-Base32), so the row holds it only after
/// EPUB→KFX completes — the import-time value is whatever the source
/// EPUB carried (usually `None`). Sidle's device-delete path keys
/// catalog-style `<title>_<ASIN>.sdr/` cleanup on this column.
pub fn set_asin(conn: &Connection, book_id: i64, asin: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE books SET asin = ?1 WHERE id = ?2",
        params![asin, book_id],
    )?;
    Ok(())
}

/// Return the id of another book (≠ `except_id`) that already holds `asin`, if
/// any. The metadata editor uses this to keep ASIN unique across the library:
/// a duplicate would make the device-delete `_<ASIN>.sdr` catalog-sidecar scan
/// (`device::push::wipe_catalog_sdrs`) wipe *both* books' sidecars on either
/// delete. Only a non-empty ASIN is meaningful here — callers gate on the real
/// 10-char shape before calling.
pub fn book_id_with_asin(
    conn: &Connection,
    asin: &str,
    except_id: i64,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM books WHERE asin = ?1 AND id != ?2 LIMIT 1",
        params![asin, except_id],
        |r| r.get(0),
    )
    .optional()
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

/// Sparse patch for bulk metadata editing across many books. Unlike
/// [`MetadataPatch`] (a full replacement), every scalar here means `None` =
/// "leave unchanged on every book", `Some(v)` = "set to v on every book".
/// Tags are *additive*: `add_tags` merge into each book's existing set and
/// `remove_tags` are pulled out, so applying a genre tag in bulk doesn't wipe
/// per-book tags. `title` and `asin` are intentionally absent — both are
/// per-book unique.
///
/// The command layer trims/normalizes scalars (empty → `None`) and
/// canonicalizes the tag lists before calling [`apply_bulk_patch`].
#[derive(Debug, Default, Deserialize)]
pub struct BulkMetadataPatch {
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub series_name: Option<String>,
    #[serde(default)]
    pub series_index: Option<f64>,
    #[serde(default)]
    pub add_tags: Vec<String>,
    #[serde(default)]
    pub remove_tags: Vec<String>,
}

/// Apply a sparse [`BulkMetadataPatch`] to one book. Only present scalars
/// change; tags merge additively. Returns `Ok(false)` when the book id no
/// longer exists (caller skips it), `Ok(true)` when a row was written.
///
/// Series consistency mirrors [`update_metadata`]: an index is kept only when a
/// series name is present (newly set or pre-existing); a book with no series
/// name has its index forced to `NULL` so the row stays self-consistent.
/// Caller canonicalizes `add_tags`/`remove_tags` so they match the stored
/// (trimmed / lowercased) form.
pub fn apply_bulk_patch(
    conn: &Connection,
    book_id: i64,
    patch: &BulkMetadataPatch,
) -> rusqlite::Result<bool> {
    let Some(row) = get_book(conn, book_id)? else {
        return Ok(false);
    };

    let author = patch.author.clone().unwrap_or(row.author);
    let language = patch.language.clone().unwrap_or(row.language);
    let publisher = patch.publisher.clone().or(row.publisher);
    let published_at = patch.published_at.clone().or(row.published_at);
    let series_name = patch.series_name.clone().or(row.series_name);

    // Take the patch index if given, else keep the row's; then enforce the
    // "no name ⇒ no index" invariant.
    let mut series_index = patch.series_index.or(row.series_index);
    if series_name.is_none() {
        series_index = None;
    }

    // Additive tag merge: existing (already canonical) ∪ add, then minus
    // remove, preserving first-seen order.
    let mut tags = row.tags;
    for t in &patch.add_tags {
        if !tags.contains(t) {
            tags.push(t.clone());
        }
    }
    if !patch.remove_tags.is_empty() {
        tags.retain(|t| !patch.remove_tags.contains(t));
    }
    let tags_json = serde_json::to_string(&tags)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

    conn.execute(
        r#"UPDATE books
              SET author       = ?1,
                  language     = ?2,
                  publisher    = ?3,
                  published_at = ?4,
                  series_name  = ?5,
                  series_index = ?6,
                  tags         = ?7
              WHERE id = ?8"#,
        params![
            author,
            language,
            publisher,
            published_at,
            series_name,
            series_index,
            tags_json,
            book_id,
        ],
    )?;
    Ok(true)
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
    /// Hash of the KFX bytes — required iff `kfx_path` is `Some`.
    /// See `BookRow::kfx_sha256` for the reasoning.
    pub kfx_sha256: Option<&'a str>,
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
           b.publisher, b.published_at, b.series_name, b.series_index, b.tags,
           b.kfx_sha256
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    ORDER BY b.imported_at DESC
"#;

const SELECT_BOOK_WITH_JOB_BY_SHA: &str = r#"
    SELECT b.id, b.sha256, b.title, b.author, b.language, b.ppd,
           b.epub_path, b.cover_path, b.kfx_path,
           b.file_size, b.imported_at, b.asin,
           COALESCE(j.status, 'pending') AS status, j.error, j.kind,
           b.publisher, b.published_at, b.series_name, b.series_index, b.tags,
           b.kfx_sha256
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    WHERE b.sha256 = ?1
"#;

const SELECT_BOOK_WITH_JOB_BY_ID: &str = r#"
    SELECT b.id, b.sha256, b.title, b.author, b.language, b.ppd,
           b.epub_path, b.cover_path, b.kfx_path,
           b.file_size, b.imported_at, b.asin,
           COALESCE(j.status, 'pending') AS status, j.error, j.kind,
           b.publisher, b.published_at, b.series_name, b.series_index, b.tags,
           b.kfx_sha256
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    WHERE b.id = ?1
"#;

const SELECT_BOOK_WITH_JOB_BY_KFX_SHA_PREFIX: &str = r#"
    SELECT b.id, b.sha256, b.title, b.author, b.language, b.ppd,
           b.epub_path, b.cover_path, b.kfx_path,
           b.file_size, b.imported_at, b.asin,
           COALESCE(j.status, 'pending') AS status, j.error, j.kind,
           b.publisher, b.published_at, b.series_name, b.series_index, b.tags,
           b.kfx_sha256
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    WHERE b.kfx_sha256 LIKE ?1
    LIMIT 1
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
        kfx_sha256: row.get(20)?,
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
                kfx_sha256: None,
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

    #[test]
    fn set_asin_and_uniqueness() {
        let conn = fresh_db();
        let a = insert_minimal(&conn, "sha-asin-a", "A");
        let b = insert_minimal(&conn, "sha-asin-b", "B");

        // Fresh books have no ASIN, so the column is free.
        assert_eq!(
            book_id_with_asin(&conn, "B07PXGQC1Q", a).expect("query"),
            None
        );

        set_asin(&conn, a, "B07PXGQC1Q").expect("set asin a");
        assert_eq!(
            get_book(&conn, a).unwrap().unwrap().asin.as_deref(),
            Some("B07PXGQC1Q")
        );

        // Another book asking for the same ASIN now finds the collision...
        assert_eq!(
            book_id_with_asin(&conn, "B07PXGQC1Q", b).expect("query"),
            Some(a)
        );
        // ...but the owner itself is excluded (re-saving the same ASIN is fine).
        assert_eq!(
            book_id_with_asin(&conn, "B07PXGQC1Q", a).expect("query"),
            None
        );
    }

    fn seed_full(conn: &Connection, sha: &str, tags: Vec<String>) -> i64 {
        let id = insert_minimal(conn, sha, "Original");
        update_metadata(
            conn,
            id,
            &MetadataPatch {
                title: "Original".into(),
                author: "Asimov".into(),
                language: "en".into(),
                publisher: Some("Spectra".into()),
                published_at: None,
                series_name: None,
                series_index: None,
                tags,
            },
        )
        .expect("seed");
        id
    }

    #[test]
    fn bulk_patch_sparse_leaves_unset_fields() {
        let conn = fresh_db();
        let id = seed_full(&conn, "sha-bulk-a", vec!["sci-fi".into()]);

        // Set only series_name + add a tag; everything else stays None.
        let patch = BulkMetadataPatch {
            series_name: Some("Foundation".into()),
            add_tags: vec!["classic".into()],
            ..Default::default()
        };
        assert!(apply_bulk_patch(&conn, id, &patch).expect("apply"));

        let row = get_book(&conn, id).unwrap().unwrap();
        assert_eq!(row.title, "Original"); // title is never bulk-touched
        assert_eq!(row.author, "Asimov"); // None → unchanged
        assert_eq!(row.publisher.as_deref(), Some("Spectra")); // None → unchanged
        assert_eq!(row.series_name.as_deref(), Some("Foundation")); // set
        assert_eq!(row.tags, vec!["sci-fi", "classic"]); // additive
    }

    #[test]
    fn bulk_patch_tag_merge_dedupes_and_removes() {
        let conn = fresh_db();
        let id = seed_full(&conn, "sha-bulk-b", vec!["sci-fi".into(), "to-read".into()]);

        let patch = BulkMetadataPatch {
            add_tags: vec!["sci-fi".into(), "classic".into()], // sci-fi already present
            remove_tags: vec!["to-read".into()],
            ..Default::default()
        };
        apply_bulk_patch(&conn, id, &patch).expect("apply");

        let row = get_book(&conn, id).unwrap().unwrap();
        // sci-fi kept (no dupe), classic appended, to-read removed.
        assert_eq!(row.tags, vec!["sci-fi", "classic"]);
    }

    #[test]
    fn bulk_patch_index_dropped_without_series_name() {
        let conn = fresh_db();
        let id = insert_minimal(&conn, "sha-bulk-c", "x");

        // Index but no series name anywhere → index must be dropped.
        let patch = BulkMetadataPatch {
            series_index: Some(3.0),
            ..Default::default()
        };
        apply_bulk_patch(&conn, id, &patch).expect("apply");

        let row = get_book(&conn, id).unwrap().unwrap();
        assert_eq!(row.series_name, None);
        assert_eq!(row.series_index, None);
    }

    #[test]
    fn bulk_patch_skips_missing_book() {
        let conn = fresh_db();
        assert!(
            !apply_bulk_patch(&conn, 9999, &BulkMetadataPatch::default()).expect("apply")
        );
    }
}
