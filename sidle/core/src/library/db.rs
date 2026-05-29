//! rusqlite-backed library database.
//!
//! Single-user. The desktop app holds one `Connection` behind an `Arc<Mutex>`
//! in `AppState`; the standalone `sidle-server` daemon opens its own (per
//! request). WAL + `busy_timeout` (see [`open`]) let those two processes share
//! the file safely. rusqlite calls block, but the library workload is tiny.

use std::path::{Path, PathBuf};

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
    /// Path to the PDF on disk, for a PDF-backed (container) book. This is the
    /// non-KFX side of a PDF↔KFX book (the EPUB↔KFX analogue of `epub_path`);
    /// `None` for reflowable (EPUB↔KFX) books. See .claude/plans/pdf-to-kfx.md.
    pub pdf_path: Option<String>,
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

/// Schema version stamped into `PRAGMA user_version` by [`migrate`]. Bump on
/// each schema change. Backups record it; restore refuses an archive whose
/// version exceeds the running app's (§4c).
///
/// v2: dropped the `My Clippings.txt` ingest path entirely — see the DELETE
/// near the end of [`migrate`].
/// v3: added `books.pdf_path` — the PDF side of a PDF↔KFX book (PDF-backed
/// container KFX). See .claude/plans/pdf-to-kfx.md.
pub const SCHEMA_VERSION: i64 = 3;

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // WAL lets the standalone `sidle-server` daemon share this file with the
    // desktop app (P3's LAN-sync writer + the GUI). `busy_timeout` makes a second
    // concurrent writer wait for the lock instead of failing immediately with
    // SQLITE_BUSY; writes are idempotent (UNIQUE dedup_hash, per-device
    // checkpoints), so serialized contention converges rather than corrupts.
    conn.pragma_update(None, "busy_timeout", 5000)?;
    migrate(&conn)?;
    relativize_existing_paths(&conn, path)?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Path portability (§4a). The three `*_path` columns are stored ROOT-RELATIVE
// (`books/<sha>/<file>`) so the library folder can be moved; we resolve to
// absolute on read and relativize on write. The root is the directory holding
// `library.db`, derived from the connection itself — so no caller threads it in,
// and an in-memory test connection (empty filename) skips conversion entirely.
// ---------------------------------------------------------------------------

/// Library root for `conn` — the directory holding `library.db`. `None` for an
/// in-memory connection (its filename is empty / `:memory:`), where stored paths
/// are taken verbatim (tests insert `None`/relative paths, so nothing converts).
fn conn_root(conn: &Connection) -> Option<PathBuf> {
    let p = conn.path()?;
    if p.is_empty() || p == ":memory:" {
        return None;
    }
    Path::new(p).parent().map(Path::to_path_buf)
}

/// Resolve a stored path to absolute against `root`. A `None` root or an
/// already-absolute value (the pre-migration window, or a foreign path) is
/// returned unchanged.
fn resolve_one(root: Option<&Path>, stored: &str) -> String {
    match root {
        Some(r) if !Path::new(stored).is_absolute() => {
            r.join(stored).to_string_lossy().into_owned()
        }
        _ => stored.to_string(),
    }
}

fn resolve_opt(root: Option<&Path>, stored: Option<String>) -> Option<String> {
    stored.map(|s| resolve_one(root, &s))
}

/// Relativize an absolute managed path to root-relative for storage. A path
/// outside `root` (or a `None` root) is stored unchanged — defensive; library-
/// managed files always live under root. Idempotent: an already-relative input
/// isn't under `root`, so `strip_prefix` fails and it's left as-is.
fn relativize_for_store(root: Option<&Path>, abs: &str) -> String {
    match root {
        Some(r) => match Path::new(abs).strip_prefix(r) {
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => abs.to_string(),
        },
        None => abs.to_string(),
    }
}

/// `(id, epub_path, cover_path, kfx_path)` for the §4a path-relativization
/// migration sweep.
type PathColumns = (i64, Option<String>, Option<String>, Option<String>);

/// One-time migration: rewrite absolute `*_path` columns (pre-§4a rows stored
/// `<root>/books/<sha>/...`) to root-relative. Gated on actually finding an
/// absolute value, so steady-state opens short-circuit after the first. No-op
/// for an in-memory DB (the path has no parent). Lives in `open()` — which has
/// the db path, hence the root — not `migrate()`, which only gets the `Connection`.
fn relativize_existing_paths(conn: &Connection, db_path: &Path) -> rusqlite::Result<()> {
    let Some(root) = db_path.parent() else {
        return Ok(());
    };
    let any_absolute: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM books \
         WHERE epub_path LIKE '/%' OR cover_path LIKE '/%' OR kfx_path LIKE '/%')",
        [],
        |r| r.get(0),
    )?;
    if !any_absolute {
        return Ok(());
    }
    let rows: Vec<PathColumns> = {
        let mut stmt = conn.prepare("SELECT id, epub_path, cover_path, kfx_path FROM books")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (id, epub, cover, kfx) in rows {
        let e = epub.map(|s| relativize_for_store(Some(root), &s));
        let c = cover.map(|s| relativize_for_store(Some(root), &s));
        let k = kfx.map(|s| relativize_for_store(Some(root), &s));
        conn.execute(
            "UPDATE books SET epub_path = ?1, cover_path = ?2, kfx_path = ?3 WHERE id = ?4",
            params![e, c, k, id],
        )?;
    }
    Ok(())
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
            pdf_path          TEXT,
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

    // Annotations. ADDITIVE and precious: created here and NEVER dropped by the
    // destructive reset above (which only drops books / conversion_jobs /
    // device_history). `book_id` is nullable with ON DELETE SET NULL so deleting
    // a book unlinks its imported annotations instead of destroying them;
    // `dedup_hash UNIQUE` makes re-import idempotent. (`reading_position` is also
    // additive but lives out-of-band below — it needs a one-time PK migration.)
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS annotations (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            dedup_hash   TEXT NOT NULL UNIQUE,
            book_id      INTEGER REFERENCES books(id) ON DELETE SET NULL,
            kind         TEXT NOT NULL,
            eid_start    INTEGER,
            off_start    INTEGER,
            eid_end      INTEGER,
            off_end      INTEGER,
            loc_start    INTEGER,
            loc_end      INTEGER,
            linear_pos   INTEGER,
            text         TEXT NOT NULL DEFAULT '',
            note_body    TEXT,
            color        TEXT,
            clip_title   TEXT,
            clip_author  TEXT,
            added_at     TEXT,
            added_raw    TEXT,
            imported_at  TEXT NOT NULL,
            source       TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_annotations_book ON annotations(book_id);

        CREATE TABLE IF NOT EXISTS reading_position (
            book_id     INTEGER PRIMARY KEY REFERENCES books(id) ON DELETE CASCADE,
            eid         INTEGER,
            "offset"    INTEGER,
            linear_pos  INTEGER,
            source      TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );

        -- Which devices currently assert each annotation (stable dedup_hash +
        -- USB serial). A device import is a per-(device,book) authoritative
        -- set: on sync we mark the current ones seen, drop the rest, and GC any
        -- annotation no device asserts anymore — so deleting on a device (e.g. a
        -- transient bookmark) propagates, while one still present on another
        -- device survives. Additive; never part of the destructive reset.
        CREATE TABLE IF NOT EXISTS annotation_device (
            dedup_hash    TEXT NOT NULL,
            device_serial TEXT NOT NULL,
            book_id       INTEGER,
            last_seen     TEXT NOT NULL,
            PRIMARY KEY (dedup_hash, device_serial)
        );
        CREATE INDEX IF NOT EXISTS idx_annotation_device_dev
            ON annotation_device(device_serial, book_id);
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
    if !has_column(conn, "books", "pdf_path")? {
        conn.execute("ALTER TABLE books ADD COLUMN pdf_path TEXT", [])?;
    }

    // `yjr_sync` gained a per-device dimension: the checkpoint of the last
    // imported `.yjr` is now keyed by (device_serial, book_id), so two devices
    // holding the same book no longer clobber each other's "unchanged" marker.
    // The old shape was keyed by book_id alone; drop it if found (it's only a
    // cache — a lost checkpoint just re-syncs once) and recreate.
    if !has_column(conn, "yjr_sync", "device_serial")? {
        conn.execute("DROP TABLE IF EXISTS yjr_sync", [])?;
    }
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS yjr_sync (
            device_serial TEXT NOT NULL,
            book_id       INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE,
            yjr_sha       TEXT NOT NULL,
            synced_at     TEXT NOT NULL,
            PRIMARY KEY (device_serial, book_id)
        )"#,
        [],
    )?;

    // `reading_position` is keyed `(book_id, source, device_serial)` so a book can
    // hold the one Sidle-native last position (`source='sidle'`, `device_serial=''`,
    // auto-restored on open) AND a SEPARATE last position per Kindle
    // (`source='device'`, the USB/MTP serial) — parallel to `annotation_device`,
    // so two devices don't clobber each other's last-read. Each is a Resume jump
    // target; only the 'sidle' one is auto-applied. Earlier shapes were keyed
    // `(book_id)` then `(book_id, source)`; migrate by rebuilding and carrying over
    // the precious 'sidle' rows (device rows re-sync from the `.yjf` next connect).
    // ADDITIVE and precious: the destructive reset never touches it.
    if has_table(conn, "reading_position")? && !has_column(conn, "reading_position", "device_serial")? {
        conn.execute_batch(
            r#"
            ALTER TABLE reading_position RENAME TO reading_position_old;
            CREATE TABLE reading_position (
                book_id       INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE,
                eid           INTEGER,
                "offset"      INTEGER,
                linear_pos    INTEGER,
                source        TEXT NOT NULL,
                device_serial TEXT NOT NULL DEFAULT '',
                updated_at    TEXT NOT NULL,
                PRIMARY KEY (book_id, source, device_serial)
            );
            INSERT INTO reading_position
                (book_id, eid, "offset", linear_pos, source, device_serial, updated_at)
                SELECT book_id, eid, "offset", linear_pos, source, '', updated_at
                FROM reading_position_old WHERE source = 'sidle';
            DROP TABLE reading_position_old;
            "#,
        )?;
    }
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS reading_position (
            book_id       INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE,
            eid           INTEGER,
            "offset"      INTEGER,
            linear_pos    INTEGER,
            source        TEXT NOT NULL,
            device_serial TEXT NOT NULL DEFAULT '',
            updated_at    TEXT NOT NULL,
            PRIMARY KEY (book_id, source, device_serial)
        )"#,
        [],
    )?;

    // v2: scrub any rows from the (now-removed) `My Clippings.txt` ingest path.
    // It only ever wrote orphans (`book_id IS NULL`), so this never touches a
    // linked annotation. Idempotent: a no-op on a DB that never had any. Kept
    // unconditional rather than version-gated so a stray test fixture or a
    // restored backup is cleaned up on next open.
    conn.execute("DELETE FROM annotations WHERE source = 'clippings'", [])?;

    // §4c: stamp the schema version. migrate() always brings the DB up to the
    // latest schema, so set the current marker; backups gate restores on it.
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

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

/// Current schema version (`PRAGMA user_version`). Backups record it in the
/// manifest; restore refuses an archive whose version exceeds [`SCHEMA_VERSION`].
pub fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
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
    let root = conn_root(conn);
    let pattern = format!("{prefix}%");
    conn.query_row(
        SELECT_BOOK_WITH_JOB_BY_KFX_SHA_PREFIX,
        params![pattern],
        |row| row_to_book(row, root.as_deref()),
    )
    .optional()
}

/// Look up the book whose `kfx_path` ends in `/<filename>`. The fallback for
/// on-device files that predate the `.<sha8>.kfx` naming — those carry the
/// library row's kfx basename verbatim, so we match by suffix against the
/// stored relative path (`books/<sha>/<basename>`). Returns the first row
/// that matches; identical basenames across two books are extremely rare in
/// practice (each lives under its own sha-named directory) and arbitrary
/// pick is acceptable here.
pub fn find_by_kfx_filename(
    conn: &Connection,
    filename: &str,
) -> rusqlite::Result<Option<BookRow>> {
    let root = conn_root(conn);
    let pattern = format!("%/{filename}");
    conn.query_row(
        SELECT_BOOK_WITH_JOB_BY_KFX_FILENAME,
        params![pattern],
        |row| row_to_book(row, root.as_deref()),
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
    let root = conn_root(conn);
    conn.query_row(SELECT_BOOK_WITH_JOB_BY_SHA, params![sha], |row| {
        row_to_book(row, root.as_deref())
    })
    .optional()
}

pub fn list_books(conn: &Connection) -> rusqlite::Result<Vec<BookRow>> {
    let root = conn_root(conn);
    let mut stmt = conn.prepare(SELECT_BOOKS_WITH_JOBS)?;
    let rows = stmt.query_map([], |row| row_to_book(row, root.as_deref()))?;
    rows.collect()
}

pub fn insert_book(conn: &Connection, book: &NewBook<'_>) -> rusqlite::Result<i64> {
    let tags_json = serde_json::to_string(book.tags)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    // Store the three file paths root-relative (§4a) so the library is movable;
    // `row_to_book` resolves them back to absolute on read.
    let root = conn_root(conn);
    let epub_rel = book.epub_path.map(|p| relativize_for_store(root.as_deref(), p));
    let cover_rel = book.cover_path.map(|p| relativize_for_store(root.as_deref(), p));
    let kfx_rel = book.kfx_path.map(|p| relativize_for_store(root.as_deref(), p));
    let pdf_rel = book.pdf_path.map(|p| relativize_for_store(root.as_deref(), p));
    conn.execute(
        r#"INSERT INTO books
            (sha256, title, author, language, ppd, epub_path, cover_path, kfx_path,
             file_size, imported_at, asin, publisher, published_at,
             series_name, series_index, tags, kfx_sha256, pdf_path)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)"#,
        params![
            book.sha256,
            book.title,
            book.author,
            book.language,
            book.ppd,
            epub_rel,
            cover_rel,
            kfx_rel,
            book.file_size,
            book.imported_at,
            book.asin,
            book.publisher,
            book.published_at,
            book.series_name,
            book.series_index,
            tags_json,
            book.kfx_sha256,
            pdf_rel,
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
    let kfx_rel = relativize_for_store(conn_root(conn).as_deref(), kfx_path);
    conn.execute(
        "UPDATE books SET kfx_path = ?1, kfx_sha256 = ?2 WHERE id = ?3",
        params![kfx_rel, kfx_sha256, book_id],
    )?;
    Ok(())
}

/// Bootstrap-time fixup. Returns `(book_id, kfx_path)` pairs for rows
/// that have a KFX file on disk but no recorded hash — the bootstrap
/// hashes each and writes it back via `set_kfx_path_and_sha`. Exists
/// purely for upgrades from a pre-`kfx_sha256` schema; new rows always
/// land with the hash already set.
pub fn books_missing_kfx_sha(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let root = conn_root(conn);
    let mut stmt = conn.prepare(
        "SELECT id, kfx_path FROM books \
         WHERE kfx_path IS NOT NULL AND kfx_sha256 IS NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, resolve_one(root.as_deref(), &r.get::<_, String>(1)?)))
    })?;
    rows.collect()
}

/// Find rows with a `kfx_path` but no `asin`. Bootstrap reads each KFX's
/// metadata to recover the value boko-kai stamped at export time — the
/// device-delete path needs it to wipe Kindle's `<title>_<ASIN>.sdr/`
/// catalog sidecar. Exists purely for rows converted before the worker
/// started capturing ASIN; new rows always land with `asin` populated.
pub fn books_missing_asin(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let root = conn_root(conn);
    let mut stmt = conn.prepare(
        "SELECT id, kfx_path FROM books \
         WHERE kfx_path IS NOT NULL AND (asin IS NULL OR asin = '')",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, resolve_one(root.as_deref(), &r.get::<_, String>(1)?)))
    })?;
    rows.collect()
}

pub fn set_epub_path(conn: &Connection, book_id: i64, epub_path: &str) -> rusqlite::Result<()> {
    let epub_rel = relativize_for_store(conn_root(conn).as_deref(), epub_path);
    conn.execute(
        "UPDATE books SET epub_path = ?1 WHERE id = ?2",
        params![epub_rel, book_id],
    )?;
    Ok(())
}

/// Set the PDF side of a PDF↔KFX book (written by the `kfx_to_pdf` worker job
/// and by import when the PDF is the canonical side).
pub fn set_pdf_path(conn: &Connection, book_id: i64, pdf_path: &str) -> rusqlite::Result<()> {
    let pdf_rel = relativize_for_store(conn_root(conn).as_deref(), pdf_path);
    conn.execute(
        "UPDATE books SET pdf_path = ?1 WHERE id = ?2",
        params![pdf_rel, book_id],
    )?;
    Ok(())
}

pub fn set_cover_path(conn: &Connection, book_id: i64, cover_path: &str) -> rusqlite::Result<()> {
    let cover_rel = relativize_for_store(conn_root(conn).as_deref(), cover_path);
    conn.execute(
        "UPDATE books SET cover_path = ?1 WHERE id = ?2",
        params![cover_rel, book_id],
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
    let root = conn_root(conn);
    conn.query_row(SELECT_BOOK_WITH_JOB_BY_ID, params![book_id], |row| {
        row_to_book(row, root.as_deref())
    })
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
    /// PDF side of a PDF↔KFX book; `None` for EPUB↔KFX books.
    pub pdf_path: Option<&'a str>,
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
           b.kfx_sha256, b.pdf_path
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
           b.kfx_sha256, b.pdf_path
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
           b.kfx_sha256, b.pdf_path
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
           b.kfx_sha256, b.pdf_path
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    WHERE b.kfx_sha256 LIKE ?1
    LIMIT 1
"#;

/// Match by the **basename** of `kfx_path` — used by `device_list_ours` to
/// recognize on-device files pushed before the `.<sha8>.kfx` naming convention
/// existed (their device filename is just the library row's kfx basename).
/// LIKE `%/<filename>` so we ignore the `books/<sha>/` prefix the row stores.
const SELECT_BOOK_WITH_JOB_BY_KFX_FILENAME: &str = r#"
    SELECT b.id, b.sha256, b.title, b.author, b.language, b.ppd,
           b.epub_path, b.cover_path, b.kfx_path,
           b.file_size, b.imported_at, b.asin,
           COALESCE(j.status, 'pending') AS status, j.error, j.kind,
           b.publisher, b.published_at, b.series_name, b.series_index, b.tags,
           b.kfx_sha256, b.pdf_path
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    WHERE b.kfx_path LIKE ?1
    LIMIT 1
"#;

fn row_to_book(row: &rusqlite::Row<'_>, root: Option<&Path>) -> rusqlite::Result<BookRow> {
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
        // Stored root-relative (§4a); resolve to absolute against the live root.
        epub_path: resolve_opt(root, row.get(6)?),
        cover_path: resolve_opt(root, row.get(7)?),
        kfx_path: resolve_opt(root, row.get(8)?),
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
        pdf_path: resolve_opt(root, row.get(21)?),
    })
}

// ---------------------------------------------------------------------------
// Annotations + last-read position (imported off the Kindle; see
// .claude/plans/sidle-reader.md). The tables live in `migrate` outside the
// destructive reset.
// ---------------------------------------------------------------------------

/// One stored annotation. `book_id == None` means unlinked — the book isn't in
/// the library (an orphan-inbox entry).
#[derive(Debug, Clone, Serialize)]
pub struct AnnotationRow {
    pub id: i64,
    pub dedup_hash: String,
    pub book_id: Option<i64>,
    pub kind: String,
    pub eid_start: Option<i64>,
    pub off_start: Option<i64>,
    pub eid_end: Option<i64>,
    pub off_end: Option<i64>,
    pub loc_start: Option<i64>,
    pub loc_end: Option<i64>,
    pub linear_pos: Option<i64>,
    pub text: String,
    pub note_body: Option<String>,
    pub color: Option<String>,
    pub clip_title: Option<String>,
    pub clip_author: Option<String>,
    pub added_at: Option<String>,
    pub added_raw: Option<String>,
    pub imported_at: String,
    pub source: String,
}

/// Insert payload for one annotation; borrows so ingest can build these from
/// parsed records without cloning.
pub struct NewAnnotation<'a> {
    pub dedup_hash: &'a str,
    pub book_id: Option<i64>,
    pub kind: &'a str,
    pub eid_start: Option<i64>,
    pub off_start: Option<i64>,
    pub eid_end: Option<i64>,
    pub off_end: Option<i64>,
    pub loc_start: Option<i64>,
    pub loc_end: Option<i64>,
    pub linear_pos: Option<i64>,
    pub text: &'a str,
    pub note_body: Option<&'a str>,
    pub color: Option<&'a str>,
    pub clip_title: Option<&'a str>,
    pub clip_author: Option<&'a str>,
    pub added_at: Option<&'a str>,
    pub added_raw: Option<&'a str>,
    pub imported_at: &'a str,
    pub source: &'a str,
}

/// Insert an annotation, ignoring exact duplicates (same `dedup_hash`). Returns
/// `true` if a new row landed, `false` if it was already present — so an import
/// can count inserted vs duplicate.
pub fn insert_annotation(conn: &Connection, a: &NewAnnotation<'_>) -> rusqlite::Result<bool> {
    let n = conn.execute(
        r#"INSERT INTO annotations
            (dedup_hash, book_id, kind, eid_start, off_start, eid_end, off_end,
             loc_start, loc_end, linear_pos, text, note_body, color,
             clip_title, clip_author, added_at, added_raw, imported_at, source)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                    ?14, ?15, ?16, ?17, ?18, ?19)
            ON CONFLICT(dedup_hash) DO NOTHING"#,
        params![
            a.dedup_hash,
            a.book_id,
            a.kind,
            a.eid_start,
            a.off_start,
            a.eid_end,
            a.off_end,
            a.loc_start,
            a.loc_end,
            a.linear_pos,
            a.text,
            a.note_body,
            a.color,
            a.clip_title,
            a.clip_author,
            a.added_at,
            a.added_raw,
            a.imported_at,
            a.source,
        ],
    )?;
    Ok(n > 0)
}

const SELECT_ANNOTATION: &str = r#"
    SELECT id, dedup_hash, book_id, kind, eid_start, off_start, eid_end, off_end,
           loc_start, loc_end, linear_pos, text, note_body, color,
           clip_title, clip_author, added_at, added_raw, imported_at, source
    FROM annotations
"#;

fn row_to_annotation(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnnotationRow> {
    Ok(AnnotationRow {
        id: row.get(0)?,
        dedup_hash: row.get(1)?,
        book_id: row.get(2)?,
        kind: row.get(3)?,
        eid_start: row.get(4)?,
        off_start: row.get(5)?,
        eid_end: row.get(6)?,
        off_end: row.get(7)?,
        loc_start: row.get(8)?,
        loc_end: row.get(9)?,
        linear_pos: row.get(10)?,
        text: row.get(11)?,
        note_body: row.get(12)?,
        color: row.get(13)?,
        clip_title: row.get(14)?,
        clip_author: row.get(15)?,
        added_at: row.get(16)?,
        added_raw: row.get(17)?,
        imported_at: row.get(18)?,
        source: row.get(19)?,
    })
}

/// Annotations for one book, ordered by reading position.
pub fn list_annotations_for_book(
    conn: &Connection,
    book_id: i64,
) -> rusqlite::Result<Vec<AnnotationRow>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT_ANNOTATION} WHERE book_id = ?1 ORDER BY linear_pos, loc_start, id"
    ))?;
    stmt.query_map(params![book_id], row_to_annotation)?.collect()
}

/// Unlinked annotations (book not in the library) — the orphan inbox.
pub fn list_unlinked_annotations(conn: &Connection) -> rusqlite::Result<Vec<AnnotationRow>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT_ANNOTATION} WHERE book_id IS NULL ORDER BY clip_title, linear_pos, loc_start, id"
    ))?;
    stmt.query_map([], row_to_annotation)?.collect()
}

/// Point an annotation at a (newly matched) book. Used by ingest's relink pass.
pub fn set_annotation_book_id(
    conn: &Connection,
    annotation_id: i64,
    book_id: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE annotations SET book_id = ?1 WHERE id = ?2",
        params![book_id, annotation_id],
    )?;
    Ok(())
}

/// One annotation by id — the native edit/delete path loads it to recompute the
/// dedup hash and to return the refreshed row.
pub fn get_annotation(conn: &Connection, id: i64) -> rusqlite::Result<Option<AnnotationRow>> {
    conn.query_row(
        &format!("{SELECT_ANNOTATION} WHERE id = ?1"),
        params![id],
        row_to_annotation,
    )
    .optional()
}

/// One annotation by its `dedup_hash`. The native create path inserts with
/// `ON CONFLICT(dedup_hash) DO NOTHING`, so when a created annotation collides
/// with an existing one (e.g. the same passage already imported from a Kindle)
/// the command returns the row already present instead of erroring.
pub fn get_annotation_by_hash(
    conn: &Connection,
    dedup_hash: &str,
) -> rusqlite::Result<Option<AnnotationRow>> {
    conn.query_row(
        &format!("{SELECT_ANNOTATION} WHERE dedup_hash = ?1"),
        params![dedup_hash],
        row_to_annotation,
    )
    .optional()
}

/// Update a native annotation's editable fields (`kind`, `note_body`, `color`)
/// together with its recomputed `dedup_hash` (the hash folds in kind + note
/// body, so they move together). Returns rows changed (0 = no such id). A hash
/// collision with another row trips the UNIQUE constraint and surfaces as an
/// `Err` — practically unreachable, since distinct anchors hash distinctly.
pub fn update_annotation(
    conn: &Connection,
    id: i64,
    kind: &str,
    note_body: Option<&str>,
    color: Option<&str>,
    dedup_hash: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE annotations SET kind = ?1, note_body = ?2, color = ?3, dedup_hash = ?4 \
         WHERE id = ?5",
        params![kind, note_body, color, dedup_hash, id],
    )
}

/// Delete one annotation by id, also dropping any device-presence rows for its
/// hash. Native ('sidle') rows carry no `annotation_device` presence, but the
/// cleanup keeps the side table consistent if the hash was ever shared with a
/// device import. Returns true if a row was deleted.
pub fn delete_annotation(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    // Read the hash first so the presence side table can be cleaned to match.
    let hash: Option<String> = conn
        .query_row(
            "SELECT dedup_hash FROM annotations WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    let n = conn.execute("DELETE FROM annotations WHERE id = ?1", params![id])?;
    if let Some(h) = hash {
        conn.execute(
            "DELETE FROM annotation_device WHERE dedup_hash = ?1",
            params![h],
        )?;
    }
    Ok(n > 0)
}

/// Last-read position for a book. `device_serial` is `""` for the Sidle-native
/// row (`source='sidle'`) and the Kindle's USB/MTP serial for an imported one
/// (`source='device'`) — so each device keeps its own last-read.
#[derive(Debug, Clone, Serialize)]
pub struct ReadingPosition {
    pub book_id: i64,
    pub eid: Option<i64>,
    pub offset: Option<i64>,
    pub linear_pos: Option<i64>,
    pub source: String,
    pub device_serial: String,
    pub updated_at: String,
}

/// A book's stored last-read positions: the one Sidle-native row (`source='sidle'`,
/// auto-restored on open) plus one per Kindle that synced it (`source='device'`,
/// keyed by serial) — each offered as a Resume jump target, none but 'sidle'
/// auto-applied.
pub fn list_reading_positions(
    conn: &Connection,
    book_id: i64,
) -> rusqlite::Result<Vec<ReadingPosition>> {
    let mut stmt = conn.prepare(
        r#"SELECT book_id, eid, "offset", linear_pos, source, device_serial, updated_at
           FROM reading_position WHERE book_id = ?1"#,
    )?;
    let rows = stmt.query_map(params![book_id], |row| {
        Ok(ReadingPosition {
            book_id: row.get(0)?,
            eid: row.get(1)?,
            offset: row.get(2)?,
            linear_pos: row.get(3)?,
            source: row.get(4)?,
            device_serial: row.get(5)?,
            updated_at: row.get(6)?,
        })
    })?;
    rows.collect()
}

/// Upsert one of a book's last-read positions, keyed by `(book_id, source,
/// device_serial)` so the Sidle-native row and each device's row coexist instead
/// of clobbering. Pass `device_serial = ""` for the Sidle-native (`source="sidle"`)
/// position; for a device, pass its serial.
pub fn set_reading_position(
    conn: &Connection,
    book_id: i64,
    eid: Option<i64>,
    offset: Option<i64>,
    linear_pos: Option<i64>,
    source: &str,
    device_serial: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        r#"INSERT INTO reading_position
                (book_id, eid, "offset", linear_pos, source, device_serial, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(book_id, source, device_serial) DO UPDATE SET
                eid = excluded.eid,
                "offset" = excluded."offset",
                linear_pos = excluded.linear_pos,
                updated_at = excluded.updated_at"#,
        params![book_id, eid, offset, linear_pos, source, device_serial, now_iso()],
    )?;
    Ok(())
}

/// The content hash of the `.yjr` last imported for `(device_serial, book_id)`,
/// if that device has ever synced it. The device import compares this against
/// the hash of the file on the Kindle to decide whether anything changed. Keyed
/// per device so two Kindles holding the same book don't clobber each other's
/// checkpoint.
pub fn get_yjr_sync_sha(
    conn: &Connection,
    device_serial: &str,
    book_id: i64,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT yjr_sha FROM yjr_sync WHERE device_serial = ?1 AND book_id = ?2",
        params![device_serial, book_id],
        |row| row.get(0),
    )
    .optional()
}

/// Record the `.yjr` content hash imported for `(device_serial, book_id)` (upsert).
pub fn set_yjr_sync_sha(
    conn: &Connection,
    device_serial: &str,
    book_id: i64,
    yjr_sha: &str,
    now: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        r#"INSERT INTO yjr_sync (device_serial, book_id, yjr_sha, synced_at) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(device_serial, book_id) DO UPDATE SET
                yjr_sha = excluded.yjr_sha,
                synced_at = excluded.synced_at"#,
        params![device_serial, book_id, yjr_sha, now],
    )?;
    Ok(())
}

/// Reconcile one device's annotation presence for a book against the set it
/// currently asserts (`current_hashes` = the dedup_hashes in the device's
/// `.yjr` for this book, just imported). Returns the dedup_hashes of annotation
/// rows that were garbage-collected (no device asserts them anymore).
///
/// 1. Mark each current hash seen-now (upsert presence with `last_seen = now`).
/// 2. Drop this `(device, book)`'s presence rows with an older `last_seen` —
///    those were deleted on the device since the last sync.
/// 3. GC every `'yjr'` annotation **for this book** that no device asserts (no
///    presence row left). This reconciles both freshly-deleted rows *and* legacy
///    rows imported before presence tracking existed — a book still on the
///    device is mirrored exactly. A row still asserted by another device
///    survives; native (`'sidle'`) and clipping rows are never touched. Books
///    not on any connected device are never reconciled (not passed here), so
///    their annotations are preserved.
///
/// `now` must be unique per sync pass (an ISO timestamp is — two passes never
/// share one), since step 2 uses it as the "seen this pass" marker.
pub fn reconcile_device_book(
    conn: &Connection,
    device_serial: &str,
    book_id: i64,
    current_hashes: &[String],
    now: &str,
) -> rusqlite::Result<Vec<String>> {
    for hash in current_hashes {
        conn.execute(
            r#"INSERT INTO annotation_device (dedup_hash, device_serial, book_id, last_seen)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(dedup_hash, device_serial) DO UPDATE SET
                    book_id = excluded.book_id,
                    last_seen = excluded.last_seen"#,
            params![hash, device_serial, book_id, now],
        )?;
    }

    // Drop this (device, book)'s presence rows not touched this pass (deleted on
    // the device since last sync).
    conn.execute(
        "DELETE FROM annotation_device \
         WHERE device_serial = ?1 AND book_id = ?2 AND last_seen <> ?3",
        params![device_serial, book_id, now],
    )?;

    // GC every device-sourced annotation for this book that no device asserts
    // anymore — including legacy rows that predate presence tracking.
    let removed: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT dedup_hash FROM annotations \
             WHERE book_id = ?1 AND source = 'yjr' \
               AND NOT EXISTS (SELECT 1 FROM annotation_device ad WHERE ad.dedup_hash = annotations.dedup_hash)",
        )?;
        let rows = stmt.query_map(params![book_id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for hash in &removed {
        conn.execute(
            "DELETE FROM annotations WHERE dedup_hash = ?1 AND source = 'yjr'",
            params![hash],
        )?;
    }
    Ok(removed)
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
                pdf_path: None,
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
    fn pdf_path_roundtrips_through_get_book() {
        let conn = fresh_db();
        let id = insert_book(
            &conn,
            &NewBook {
                sha256: "sha-pdf",
                title: "PDF Book",
                author: "",
                language: "en",
                ppd: None,
                epub_path: None,
                cover_path: None,
                kfx_path: Some("books/sha-pdf/x.kfx"),
                kfx_sha256: Some("abc"),
                pdf_path: Some("books/sha-pdf/x.pdf"),
                file_size: 0,
                imported_at: "t",
                asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
            },
        )
        .expect("insert");
        let row = get_book(&conn, id).expect("query").expect("row");
        // PDF-backed book: PDF + KFX sides set, no EPUB side.
        assert_eq!(row.pdf_path.as_deref(), Some("books/sha-pdf/x.pdf"));
        assert_eq!(row.kfx_path.as_deref(), Some("books/sha-pdf/x.kfx"));
        assert_eq!(row.epub_path, None);

        set_pdf_path(&conn, id, "books/sha-pdf/renamed.pdf").expect("set_pdf_path");
        let row = get_book(&conn, id).expect("query").expect("row");
        assert_eq!(row.pdf_path.as_deref(), Some("books/sha-pdf/renamed.pdf"));
    }

    #[test]
    fn annotation_insert_is_idempotent_on_dedup_hash() {
        let conn = fresh_db();
        let book_id = insert_minimal(&conn, "sha-anno", "テスト本");
        let insert = |hash: &str| {
            insert_annotation(
                &conn,
                &NewAnnotation {
                    dedup_hash: hash,
                    book_id: Some(book_id),
                    kind: "highlight",
                    eid_start: Some(1254),
                    off_start: Some(44),
                    eid_end: Some(1257),
                    off_end: Some(68),
                    loc_start: None,
                    loc_end: None,
                    linear_pos: Some(12937),
                    text: "走れメロス",
                    note_body: None,
                    color: None,
                    clip_title: None,
                    clip_author: None,
                    added_at: None,
                    added_raw: None,
                    imported_at: "2026-05-25T00:00:00Z",
                    source: "yjr",
                },
            )
        };
        assert!(insert("h1").expect("insert")); // new
        assert!(!insert("h1").expect("dup")); // same dedup_hash → ignored
        assert!(insert("h2").expect("insert2")); // distinct
        let rows = list_annotations_for_book(&conn, book_id).expect("list");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "走れメロス");
        assert_eq!((rows[0].eid_start, rows[0].off_end), (Some(1254), Some(68)));
    }

    #[test]
    fn deleting_book_unlinks_annotations_rather_than_destroying() {
        let conn = fresh_db();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let book_id = insert_minimal(&conn, "sha-gone", "消える本");
        insert_annotation(
            &conn,
            &NewAnnotation {
                dedup_hash: "k1",
                book_id: Some(book_id),
                kind: "bookmark",
                eid_start: Some(1492),
                off_start: Some(0),
                eid_end: Some(1492),
                off_end: Some(0),
                loc_start: None,
                loc_end: None,
                linear_pos: Some(22364),
                text: "",
                note_body: None,
                color: None,
                clip_title: Some("消える本"),
                clip_author: None,
                added_at: None,
                added_raw: None,
                imported_at: "2026-05-25T00:00:00Z",
                source: "yjr",
            },
        )
        .expect("insert");
        remove_book(&conn, book_id).expect("remove");
        // The book row is gone, but the annotation survives — now unlinked.
        assert!(
            list_annotations_for_book(&conn, book_id)
                .expect("by book")
                .is_empty()
        );
        let orphans = list_unlinked_annotations(&conn).expect("orphans");
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].book_id, None);
        assert_eq!(orphans[0].clip_title.as_deref(), Some("消える本"));
    }

    #[test]
    fn native_annotation_get_update_delete_round_trip() {
        let conn = fresh_db();
        let book_id = insert_minimal(&conn, "sha-native", "自作本");
        insert_annotation(
            &conn,
            &NewAnnotation {
                dedup_hash: "nh1",
                book_id: Some(book_id),
                kind: "highlight",
                eid_start: Some(10),
                off_start: Some(0),
                eid_end: Some(10),
                off_end: Some(4),
                loc_start: Some(100),
                loc_end: Some(100),
                linear_pos: Some(100),
                text: "メロス",
                note_body: None,
                color: Some("yellow"),
                clip_title: None,
                clip_author: None,
                added_at: Some("2026-05-27T00:00:00Z"),
                added_raw: None,
                imported_at: "2026-05-27T00:00:00Z",
                source: "sidle",
            },
        )
        .expect("insert");
        let id = get_annotation_by_hash(&conn, "nh1")
            .expect("by hash")
            .expect("present")
            .id;

        // Promote highlight → note + recolor; the hash moves with the content.
        let changed =
            update_annotation(&conn, id, "note", Some("a thought"), Some("blue"), "nh1-v2")
                .expect("update");
        assert_eq!(changed, 1);
        let row = get_annotation(&conn, id).expect("get").expect("present");
        assert_eq!(row.kind, "note");
        assert_eq!(row.note_body.as_deref(), Some("a thought"));
        assert_eq!(row.color.as_deref(), Some("blue"));
        assert_eq!(row.dedup_hash, "nh1-v2");
        assert!(get_annotation_by_hash(&conn, "nh1").expect("old").is_none());
        assert!(get_annotation_by_hash(&conn, "nh1-v2").expect("new").is_some());

        // Delete removes it; deleting again is a no-op.
        assert!(delete_annotation(&conn, id).expect("delete"));
        assert!(get_annotation(&conn, id).expect("gone").is_none());
        assert!(!delete_annotation(&conn, id).expect("delete-again"));
        assert!(list_annotations_for_book(&conn, book_id).expect("list").is_empty());
    }

    #[test]
    fn reading_position_upserts() {
        let conn = fresh_db();
        let book_id = insert_minimal(&conn, "sha-pos", "位置本");
        let find = |c: &Connection, src: &str, serial: &str| {
            list_reading_positions(c, book_id)
                .expect("list")
                .into_iter()
                .find(|p| p.source == src && p.device_serial == serial)
        };
        assert!(list_reading_positions(&conn, book_id).expect("list").is_empty());

        // Sidle's own row AND a separate row PER device all coexist — composite PK
        // `(book_id, source, device_serial)`, so none clobbers another.
        set_reading_position(&conn, book_id, Some(200), Some(0), Some(3000), "sidle", "").expect("s");
        set_reading_position(&conn, book_id, Some(100), Some(5), Some(2000), "device", "KOA2").expect("a");
        set_reading_position(&conn, book_id, Some(140), Some(9), Some(2500), "device", "SCRIBE").expect("b");
        assert_eq!(list_reading_positions(&conn, book_id).expect("list").len(), 3);
        assert_eq!(find(&conn, "sidle", "").unwrap().eid, Some(200));
        assert_eq!(find(&conn, "device", "KOA2").unwrap().linear_pos, Some(2000));
        assert_eq!(find(&conn, "device", "SCRIBE").unwrap().linear_pos, Some(2500));

        // A second write for the SAME (source, serial) overwrites just that row…
        set_reading_position(&conn, book_id, Some(160), Some(3), Some(2800), "device", "KOA2").expect("a2");
        assert_eq!(list_reading_positions(&conn, book_id).expect("list").len(), 3);
        assert_eq!(find(&conn, "device", "KOA2").unwrap().eid, Some(160));
        // …leaving the other device and Sidle untouched.
        assert_eq!(find(&conn, "device", "SCRIBE").unwrap().eid, Some(140));
        assert_eq!(find(&conn, "sidle", "").unwrap().eid, Some(200));
    }

    #[test]
    fn yjr_sync_sha_upserts() {
        let conn = fresh_db();
        let book_id = insert_minimal(&conn, "sha-yjr", "栞本");
        assert!(get_yjr_sync_sha(&conn, "DEV1", book_id).expect("get").is_none());

        set_yjr_sync_sha(&conn, "DEV1", book_id, "abc123", "t1").expect("set");
        assert_eq!(get_yjr_sync_sha(&conn, "DEV1", book_id).expect("get").as_deref(), Some("abc123"));

        // A changed `.yjr` overwrites this device's checkpoint (composite-PK upsert).
        set_yjr_sync_sha(&conn, "DEV1", book_id, "def456", "t2").expect("set2");
        assert_eq!(get_yjr_sync_sha(&conn, "DEV1", book_id).expect("get").as_deref(), Some("def456"));

        // A different device keeps its own checkpoint — no clobber.
        assert!(get_yjr_sync_sha(&conn, "DEV2", book_id).expect("get").is_none());
        set_yjr_sync_sha(&conn, "DEV2", book_id, "zzz", "t3").expect("set3");
        assert_eq!(get_yjr_sync_sha(&conn, "DEV1", book_id).expect("get").as_deref(), Some("def456"));
        assert_eq!(get_yjr_sync_sha(&conn, "DEV2", book_id).expect("get").as_deref(), Some("zzz"));
    }

    #[test]
    fn reconcile_propagates_device_deletes_but_keeps_shared() {
        let conn = fresh_db();
        let book_id = insert_minimal(&conn, "sha-recon", "本");
        let mk = |hash: &str| {
            insert_annotation(
                &conn,
                &NewAnnotation {
                    dedup_hash: hash,
                    book_id: Some(book_id),
                    kind: "bookmark",
                    eid_start: Some(1),
                    off_start: Some(0),
                    eid_end: None,
                    off_end: None,
                    loc_start: None,
                    loc_end: None,
                    linear_pos: None,
                    text: "",
                    note_body: None,
                    color: None,
                    clip_title: None,
                    clip_author: None,
                    added_at: None,
                    added_raw: None,
                    imported_at: "t",
                    source: "yjr",
                },
            )
            .expect("insert");
        };
        mk("h1");
        mk("h2");
        mk("h3");
        let hashes = |conn: &Connection| -> Vec<String> {
            list_annotations_for_book(conn, book_id)
                .unwrap()
                .into_iter()
                .map(|r| r.dedup_hash)
                .collect()
        };
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();

        // DEV1's first sync asserts all three — nothing removed.
        let removed = reconcile_device_book(&conn, "DEV1", book_id, &v(&["h1", "h2", "h3"]), "t1").unwrap();
        assert!(removed.is_empty());
        // DEV2 also holds h3 (same bookmark on both devices).
        reconcile_device_book(&conn, "DEV2", book_id, &v(&["h3"]), "t1b").unwrap();

        // h2 deleted on DEV1 → no other device holds it → GC'd.
        let removed = reconcile_device_book(&conn, "DEV1", book_id, &v(&["h1", "h3"]), "t2").unwrap();
        assert_eq!(removed, vec!["h2".to_string()]);
        let h = hashes(&conn);
        assert!(!h.contains(&"h2".to_string()));
        assert!(h.contains(&"h1".to_string()) && h.contains(&"h3".to_string()));

        // h3 deleted on DEV1, but DEV2 still asserts it → survives.
        let removed = reconcile_device_book(&conn, "DEV1", book_id, &v(&["h1"]), "t3").unwrap();
        assert!(removed.is_empty(), "h3 is still on DEV2");
        assert!(hashes(&conn).contains(&"h3".to_string()));

        // Now DEV2 drops h3 too → no device holds it → GC'd.
        let removed = reconcile_device_book(&conn, "DEV2", book_id, &v(&[]), "t4").unwrap();
        assert_eq!(removed, vec!["h3".to_string()]);
        assert_eq!(hashes(&conn), vec!["h1".to_string()]);
    }

    #[test]
    fn reconcile_gcs_legacy_rows_but_spares_native() {
        let conn = fresh_db();
        let book_id = insert_minimal(&conn, "sha-legacy", "本");
        let mk = |hash: &str, source: &str| {
            insert_annotation(
                &conn,
                &NewAnnotation {
                    dedup_hash: hash,
                    book_id: Some(book_id),
                    kind: "highlight",
                    eid_start: Some(1),
                    off_start: Some(0),
                    eid_end: Some(1),
                    off_end: Some(2),
                    loc_start: None,
                    loc_end: None,
                    linear_pos: None,
                    text: "x",
                    note_body: None,
                    color: None,
                    clip_title: None,
                    clip_author: None,
                    added_at: None,
                    added_raw: None,
                    imported_at: "t",
                    source,
                },
            )
            .expect("insert");
        };
        // `keep`/`gone` are legacy device imports (no annotation_device rows yet);
        // `native` is a Sidle-made annotation that must survive a device GC.
        mk("keep", "yjr");
        mk("gone", "yjr");
        mk("native", "sidle");

        // The device's current set has only `keep` — `gone` was deleted on-device
        // before presence tracking existed.
        let removed =
            reconcile_device_book(&conn, "DEV1", book_id, &["keep".to_string()], "t1").unwrap();
        assert_eq!(removed, vec!["gone".to_string()], "legacy device delete reconciled");

        let mut left: Vec<String> = list_annotations_for_book(&conn, book_id)
            .unwrap()
            .into_iter()
            .map(|r| r.dedup_hash)
            .collect();
        left.sort();
        // `gone` removed; `keep` (still on device) + `native` survive.
        assert_eq!(left, vec!["keep".to_string(), "native".to_string()]);
    }

    /// Migration scrubs any legacy `source='clippings'` rows on `open()` — the
    /// `My Clippings.txt` ingest path was removed; this prevents stale orphans
    /// from a pre-v2 DB lingering in the library.
    #[test]
    fn migrate_purges_legacy_clipping_rows() {
        let conn = fresh_db();
        let book_id = insert_minimal(&conn, "sha-mig", "本");
        // Backdoor in a v1-shaped clippings orphan (no ingest path produces these
        // anymore, but a restored v1 backup or a stale test fixture might).
        insert_annotation(
            &conn,
            &NewAnnotation {
                dedup_hash: "legacy-clip",
                book_id: Some(book_id),
                kind: "highlight",
                eid_start: None,
                off_start: None,
                eid_end: None,
                off_end: None,
                loc_start: Some(42),
                loc_end: Some(43),
                linear_pos: Some(42),
                text: "from My Clippings.txt",
                note_body: None,
                color: None,
                clip_title: Some("Some Book"),
                clip_author: None,
                added_at: None,
                added_raw: None,
                imported_at: "t",
                source: "clippings",
            },
        )
        .expect("insert");
        // A native row alongside, to prove the DELETE is scoped to clippings.
        insert_annotation(
            &conn,
            &NewAnnotation {
                dedup_hash: "native-keep",
                book_id: Some(book_id),
                kind: "highlight",
                eid_start: Some(1),
                off_start: Some(0),
                eid_end: Some(1),
                off_end: Some(2),
                loc_start: None,
                loc_end: None,
                linear_pos: None,
                text: "y",
                note_body: None,
                color: None,
                clip_title: None,
                clip_author: None,
                added_at: None,
                added_raw: None,
                imported_at: "t",
                source: "sidle",
            },
        )
        .expect("insert");

        // Re-run migrate() — the next `open()` would call it; here we invoke
        // directly since the connection is already in memory.
        migrate(&conn).expect("migrate");

        let left: Vec<String> = list_annotations_for_book(&conn, book_id)
            .unwrap()
            .into_iter()
            .map(|r| r.dedup_hash)
            .collect();
        assert_eq!(left, vec!["native-keep".to_string()]);
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

    /// §4a path portability: paths are stored root-relative, resolved to
    /// absolute on read, stay relative across a read-modify-write, and a pre-§4a
    /// absolute row is migrated to relative on the next `open`. Uses an on-disk
    /// DB because the root is derived from `conn.path()` (empty for in-memory).
    #[test]
    fn paths_stored_relative_resolved_absolute_and_migrated() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Canonicalize: on macOS the tempdir resolves /var → /private/var, and
        // SQLite reports that realpath via `conn.path()`; matching it keeps the
        // test on a normal (non-symlinked) root instead of the symlink artifact.
        let root = dir.path().canonicalize().expect("canonicalize tempdir");
        let db_path = root.join("library.db");

        let sha = "abc123";
        let epub_abs = root.join("books").join(sha).join("[A] T (2024).epub");
        let kfx_abs = root.join("books").join(sha).join("[A] T (2024).kfx");
        let cover_abs = root.join("books").join(sha).join("cover.jpg");
        let rel_epub = "books/abc123/[A] T (2024).epub";
        let rel_kfx = "books/abc123/[A] T (2024).kfx";

        // Raw stored value, bypassing `row_to_book`'s resolution.
        let stored = |conn: &Connection, col: &str| -> Option<String> {
            conn.query_row(
                &format!("SELECT {col} FROM books WHERE sha256 = ?1"),
                rusqlite::params![sha],
                |r| r.get::<_, Option<String>>(0),
            )
            .expect("query")
        };

        let book_id = {
            let conn = open(&db_path).expect("open");
            let id = insert_book(
                &conn,
                &NewBook {
                    sha256: sha,
                    title: "T",
                    author: "A",
                    language: "",
                    ppd: None,
                    epub_path: Some(&epub_abs.to_string_lossy()),
                    cover_path: Some(&cover_abs.to_string_lossy()),
                    kfx_path: Some(&kfx_abs.to_string_lossy()),
                    kfx_sha256: Some("deadbeef"),
                    pdf_path: None,
                    file_size: 1,
                    imported_at: "t",
                    asin: None,
                    publisher: None,
                    published_at: None,
                    series_name: None,
                    series_index: None,
                    tags: &[],
                },
            )
            .expect("insert");

            // Stored relative…
            assert_eq!(stored(&conn, "epub_path").as_deref(), Some(rel_epub));
            assert_eq!(stored(&conn, "kfx_path").as_deref(), Some(rel_kfx));
            assert_eq!(stored(&conn, "cover_path").as_deref(), Some("books/abc123/cover.jpg"));

            // …resolved to absolute on read.
            let row = get_book(&conn, id).expect("get").expect("present");
            assert_eq!(row.epub_path.as_deref(), Some(epub_abs.to_string_lossy().as_ref()));
            assert_eq!(row.kfx_path.as_deref(), Some(kfx_abs.to_string_lossy().as_ref()));

            // Read-modify-write invariant: feeding the resolved ABSOLUTE path back
            // into the setter must NOT re-absolutize the column (this is the bug
            // the §4a centralization fixes — set_cover/recrawl/worker all do this).
            let resolved_kfx = row.kfx_path.clone().unwrap();
            set_kfx_path_and_sha(&conn, id, &resolved_kfx, "cafe").expect("set kfx");
            assert_eq!(stored(&conn, "kfx_path").as_deref(), Some(rel_kfx));

            // Simulate a pre-§4a absolute row via a raw UPDATE (bypasses the setter).
            conn.execute(
                "UPDATE books SET epub_path = ?1 WHERE id = ?2",
                rusqlite::params![epub_abs.to_string_lossy(), id],
            )
            .expect("set absolute");
            assert!(stored(&conn, "epub_path").unwrap().starts_with('/'));
            id
        };

        // Reopen → relativize_existing_paths migrates the absolute row back to relative.
        let conn = open(&db_path).expect("reopen");
        assert_eq!(stored(&conn, "epub_path").as_deref(), Some(rel_epub));
        let row = get_book(&conn, book_id).expect("get").expect("present");
        assert_eq!(row.epub_path.as_deref(), Some(epub_abs.to_string_lossy().as_ref()));
    }

    #[test]
    fn migrate_stamps_schema_version() {
        let conn = fresh_db();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }
}
