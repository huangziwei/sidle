//! rusqlite-backed library database, in WAL mode with a `busy_timeout` (see [`open`]).

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

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
    /// `horizontal-lr`, `horizontal-rl`, `vertical-rl`, `vertical-lr`, or `None` to derive.
    pub writing_mode: Option<String>,
    /// Path to the EPUB on disk, `None` until a KFX import's conversion lands.
    pub epub_path: Option<String>,
    pub cover_path: Option<String>,
    /// **Derived, not a column.** `books/<sha>/cover.thumb.jpg`, preferred over `cover_path`.
    pub cover_thumb_path: Option<String>,
    /// **Derived, not a column.** The served image's ms mtime, 0 with no cover.
    pub cover_rev: i64,
    /// Path to the KFX on disk, `None` until an EPUB import's conversion lands.
    pub kfx_path: Option<String>,
    /// SHA-256 of the KFX bytes, keying the on-device filename; `Some` iff `kfx_path` is.
    pub kfx_sha256: Option<String>,
    /// Path to the PDF of a PDF-backed (container) book, `None` for EPUB↔KFX books.
    pub pdf_path: Option<String>,
    pub file_size: i64,
    pub imported_at: String,
    pub status: String,
    pub error: Option<String>,
    /// The conversion job's direction, `"epub_to_kfx"` or `"kfx_to_epub"`; `None` without one.
    pub kind: Option<String>,
    /// The identifier baked into the exported KFX, keying its `.sdr` and `.notebooks` dirs.
    pub asin: Option<String>,
    /// The real Amazon catalogue ASIN, keying the colour cover at `/images/P/<asin>`.
    pub amazon_asin: Option<String>,
    /// The format that arrived at import (`azw3`, `mobi`, `epub`, …).
    pub source_format: Option<String>,
    /// Publisher imprint, from EPUB `<dc:publisher>` or the KFX `publisher` field.
    pub publisher: Option<String>,
    /// Publication date as the source states it, stored verbatim.
    pub published_at: Option<String>,
    pub series_name: Option<String>,
    /// Position within the series; REAL for half-numbers (1.5, 2.5).
    pub series_index: Option<f64>,
    /// User-defined tags, a JSON array TEXT, canonicalized at write time.
    pub tags: Vec<String>,
    /// Metadata last-edit time (ISO 8601), the newest-wins tiebreak of a library merge.
    pub updated_at: String,
    /// Human-readable romaji of the title, yomigana-aware and correctable by hand.
    pub title_romaji: String,
    /// Human-readable romaji of the author line. See [`Self::title_romaji`].
    pub author_romaji: String,
    /// **Derived, not a column.** The space-free, ASCII-folded key the picker searches.
    pub search_key: String,
}

/// Schema version [`migrate`] stamps into `PRAGMA user_version`.
pub const SCHEMA_VERSION: i64 = 26;

/// A borrowable handle to the library database.
pub trait Access {
    /// Borrow the connection for the duration of `f`.
    fn with<R>(&self, f: impl FnOnce(&Connection) -> R) -> R;
}

/// A caller with exclusive use of a connection — a one-shot tool.
impl Access for Connection {
    fn with<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        f(self)
    }
}

/// A caller whose worker threads share one connection.
impl Access for std::sync::Mutex<Connection> {
    fn with<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        f(&self.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // `WAL` shares this file between `sidle-server` and the desktop app; `busy_timeout`
    // holds a second writer at the lock past `SQLITE_BUSY`.
    conn.pragma_update(None, "busy_timeout", 5000)?;
    migrate(&conn)?;
    relativize_existing_paths(&conn, path)?;
    Ok(conn)
}

// ---------------------------------------------------------------------------

/// Library root for `conn`, the directory holding `library.db`; `None` for an
/// in-memory connection, whose stored paths are taken verbatim.
fn conn_root(conn: &Connection) -> Option<PathBuf> {
    let p = conn.path()?;
    if p.is_empty() || p == ":memory:" {
        return None;
    }
    Path::new(p).parent().map(Path::to_path_buf)
}

/// Resolve a stored path to absolute against `root`. A `None` root, and an
/// absolute value, pass through unchanged.
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

/// Relativize `abs` for storage: strip the `root` prefix, else slice from the last
/// `books`/`notebooks` component; a `None` root, or neither, stores `abs` unchanged.
fn relativize_for_store(root: Option<&Path>, abs: &str) -> String {
    let Some(r) = root else {
        return abs.to_string();
    };
    let p = Path::new(abs);
    if let Ok(rel) = p.strip_prefix(r) {
        return rel.to_string_lossy().into_owned();
    }
    managed_relative_tail(p).unwrap_or_else(|| abs.to_string())
}

/// The `books/<sha>/…` or `notebooks/<uuid>/…` tail of a managed path, from its
/// last `books`/`notebooks` component. `None` for a path carrying neither.
fn managed_relative_tail(p: &Path) -> Option<String> {
    let comps: Vec<Component<'_>> = p.components().collect();
    let idx = comps.iter().rposition(|c| {
        matches!(c, Component::Normal(s) if *s == OsStr::new("books") || *s == OsStr::new("notebooks"))
    })?;
    let tail: PathBuf = comps[idx..].iter().collect();
    Some(tail.to_string_lossy().into_owned())
}

/// `(id, epub_path, cover_path, kfx_path, pdf_path)` for the path-relativization
/// migration sweep.
type PathColumns = (
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Rewrite absolute `*_path` columns to root-relative, gated on finding an
/// absolute value; [`open`] calls it with the db path.
fn relativize_existing_paths(conn: &Connection, db_path: &Path) -> rusqlite::Result<()> {
    let Some(root) = db_path.parent() else {
        return Ok(());
    };
    let any_absolute: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM books WHERE epub_path LIKE '/%' \
         OR cover_path LIKE '/%' OR kfx_path LIKE '/%' OR pdf_path LIKE '/%')",
        [],
        |r| r.get(0),
    )?;
    if !any_absolute {
        return Ok(());
    }
    let rows: Vec<PathColumns> = {
        let mut stmt =
            conn.prepare("SELECT id, epub_path, cover_path, kfx_path, pdf_path FROM books")?;
        stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    for (id, epub, cover, kfx, pdf) in rows {
        let e = epub.map(|s| relativize_for_store(Some(root), &s));
        let c = cover.map(|s| relativize_for_store(Some(root), &s));
        let k = kfx.map(|s| relativize_for_store(Some(root), &s));
        let p = pdf.map(|s| relativize_for_store(Some(root), &s));
        conn.execute(
            "UPDATE books SET epub_path = ?1, cover_path = ?2, kfx_path = ?3, pdf_path = ?4 \
             WHERE id = ?5",
            params![e, c, k, p, id],
        )?;
    }
    Ok(())
}

/// Schema setup. A `source_epub_path` column or a `device_history` table drops
/// every table for a rebuild from the CREATE block below.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    // `from_version` gates the few steps below that rewrite rows in place.
    let from_version = user_version(conn)?;
    let needs_reset =
        has_column(conn, "books", "source_epub_path")? || has_table(conn, "device_history")?;
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
            writing_mode      TEXT,
            epub_path         TEXT,
            cover_path        TEXT,
            kfx_path          TEXT,
            kfx_sha256        TEXT,
            pdf_path          TEXT,
            file_size         INTEGER NOT NULL,
            imported_at       TEXT NOT NULL,
            asin              TEXT,
            amazon_asin       TEXT,
            publisher         TEXT,
            published_at      TEXT,
            series_name       TEXT,
            series_index      REAL,
            tags              TEXT NOT NULL DEFAULT '[]',
            updated_at        TEXT
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

    // Annotations, additive and outside the destructive reset above. `book_id`
    // is nullable with ON DELETE SET NULL, unlinking a deleted book's
    // annotations, and `dedup_hash UNIQUE` makes re-import idempotent.
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
        -- USB serial) — PROVENANCE only. On sync we mark the current ones seen
        -- and drop this device's stale rows, so the table mirrors what each
        -- device holds now. It never drives deletion of an `annotations` row:
        -- Sidle is the durable backup, so a delete on the device keeps its Sidle
        -- copy. Additive; never
        -- part of the destructive reset.
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

    // Idempotent column adds. `CREATE IF NOT EXISTS` above is a no-op on an
    // existing table, leaving these to ALTER out-of-band.
    if !has_column(conn, "books", "asin")? {
        conn.execute("ALTER TABLE books ADD COLUMN asin TEXT", [])?;
    }
    if !has_column(conn, "books", "amazon_asin")? {
        conn.execute("ALTER TABLE books ADD COLUMN amazon_asin TEXT", [])?;
        // An `asin` of the Amazon shape (10 uppercase alphanumerics) seeds `amazon_asin`;
        // `asin` stays untouched.
        conn.execute(
            "UPDATE books SET amazon_asin = asin
             WHERE amazon_asin IS NULL AND asin IS NOT NULL AND length(asin) = 10
               AND asin = upper(asin) AND asin GLOB '[A-Z0-9]*'",
            [],
        )?;
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
    // Reading layout / writing mode, the axis the generated KFX bakes into
    // `document_data.writing_mode`, with `ppd` mirroring its page-turn. NULL
    // derives it.
    if !has_column(conn, "books", "writing_mode")? {
        conn.execute("ALTER TABLE books ADD COLUMN writing_mode TEXT", [])?;
    }
    // `updated_at`, seeded from `imported_at`; `insert_book` stamps a new row.
    if !has_column(conn, "books", "updated_at")? {
        conn.execute("ALTER TABLE books ADD COLUMN updated_at TEXT", [])?;
        conn.execute(
            "UPDATE books SET updated_at = imported_at WHERE updated_at IS NULL",
            [],
        )?;
    }

    // `source_format` names the file that came in, apart from `conversion_jobs.kind`.
    if !has_column(conn, "books", "source_format")? {
        conn.execute("ALTER TABLE books ADD COLUMN source_format TEXT", [])?;
        // The `kfx_*` and `pdf_to_kfx` kinds state the arriving format; an `epub_to_kfx`
        // row stays NULL.
        conn.execute(
            "UPDATE books SET source_format = 'kfx' WHERE id IN
               (SELECT book_id FROM conversion_jobs WHERE kind IN ('kfx_to_epub','kfx_to_pdf'))",
            [],
        )?;
        conn.execute(
            "UPDATE books SET source_format = 'pdf' WHERE id IN
               (SELECT book_id FROM conversion_jobs WHERE kind = 'pdf_to_kfx')",
            [],
        )?;
    }

    // `yjr_sync` keys the last imported `.yjr` by (device_serial, book_id); one keyed
    // by book_id alone is dropped and recreated.
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

    // `reading_position` is keyed `(book_id, source, device_serial)`: one
    // `source='sidle'` position beside one per Kindle.
    if has_table(conn, "reading_position")?
        && !has_column(conn, "reading_position", "device_serial")?
    {
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

    // Scribe handwritten notebooks (.nbk → SVG), additive and outside the
    // destructive reset. Keyed by the device `.notebooks/<uuid>/` dir name, with
    // an editable title and `nbk_sha256` change-detecting an edit.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS notebooks (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            uuid        TEXT NOT NULL UNIQUE,
            title       TEXT NOT NULL DEFAULT 'Notebook',
            page_count  INTEGER NOT NULL DEFAULT 0,
            nbk_sha256  TEXT,
            imported_at TEXT NOT NULL
        )"#,
        [],
    )?;

    // The notebook's on-device "Date Modified" (source `nbk` mtime), nullable.
    if !has_column(conn, "notebooks", "updated_at")? {
        conn.execute("ALTER TABLE notebooks ADD COLUMN updated_at TEXT", [])?;
    }

    // Handwritten ink drawn on a sideloaded doc (PDOC), one row per drawn page,
    // keyed by the stable `(asin, container_id)`. `book_id` is nullable with ON
    // DELETE SET NULL; `host_*` come from the `.yjr` `handwritten_note` anchor.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS book_ink (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            book_id      INTEGER REFERENCES books(id) ON DELETE SET NULL,
            asin         TEXT NOT NULL,
            container_id TEXT NOT NULL,
            host_page    INTEGER,
            host_eid     INTEGER,
            host_linear  INTEGER,
            nbk_sha256   TEXT,
            imported_at  TEXT NOT NULL,
            UNIQUE(asin, container_id)
        )"#,
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_book_ink_book ON book_ink(book_id)",
        [],
    )?;

    // Which devices assert each ink page, the ink analogue of `annotation_device`.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS book_ink_device (
            asin          TEXT NOT NULL,
            container_id  TEXT NOT NULL,
            device_serial TEXT NOT NULL,
            book_id       INTEGER,
            last_seen     TEXT NOT NULL,
            PRIMARY KEY (asin, container_id, device_serial)
        )"#,
        [],
    )?;

    // Per-(device, asin) checkpoint: the sha of the nbk last decoded, skipping an
    // unchanged decode and re-render.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS ink_sync (
            device_serial TEXT NOT NULL,
            asin          TEXT NOT NULL,
            nbk_sha       TEXT NOT NULL,
            synced_at     TEXT NOT NULL,
            PRIMARY KEY (device_serial, asin)
        )"#,
        [],
    )?;

    // `hidden`: a reversible "hide from the reader" flag on annotations and ink, default 0.
    if !has_column(conn, "annotations", "hidden")? {
        conn.execute(
            "ALTER TABLE annotations ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column(conn, "book_ink", "hidden")? {
        conn.execute(
            "ALTER TABLE book_ink ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }

    // Deletion records device sync skips: `key` is the annotation `dedup_hash`, the
    // ink `asin\x1fcontainer_id`, or the notebook `uuid`.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS artifact_deletions (
            kind       TEXT NOT NULL,
            key        TEXT NOT NULL,
            deleted_at TEXT NOT NULL,
            PRIMARY KEY (kind, key)
        )"#,
        [],
    )?;

    // Where an on-device app's mount tree sits on this machine: `root` is the mount
    // root inside `source`.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS apps (
            id          TEXT PRIMARY KEY,
            source_kind TEXT NOT NULL,
            source      TEXT NOT NULL,
            root        TEXT NOT NULL,
            added_at    INTEGER NOT NULL
        )"#,
        [],
    )?;

    // `source = 'clippings'` rows are orphans (`book_id IS NULL`).
    conn.execute("DELETE FROM annotations WHERE source = 'clippings'", [])?;

    // `book_ink` holds handwritten ink; an `annotations` row of either handwritten kind is a stray.
    conn.execute(
        "DELETE FROM annotations
          WHERE kind IN ('handwritten_note', 'handwritten_on_content_note')",
        [],
    )?;

    // Harmonize language tags (en-US, eng, ZH_cn → en / zh-Hans / zh-Hant) over
    // the distinct values, keeping the BCP-47 logic in [`super::lang`]. A bare
    // UPDATE, leaving `updated_at` untouched.
    {
        let raws: Vec<String> = {
            let mut stmt = conn.prepare("SELECT DISTINCT language FROM books")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for raw in raws {
            let canon = super::lang::normalize(&raw);
            if canon != raw {
                conn.execute(
                    "UPDATE books SET language = ?1 WHERE language = ?2",
                    params![canon, raw],
                )?;
            }
        }
    }

    // Searchable romaji metadata: two editable columns rendered from the
    // title/author (see [`super::romaji`]). Pure CPU and NULL-guarded, running
    // on every `open()`, and a bare backfill that leaves `updated_at` alone.
    if !has_column(conn, "books", "title_romaji")? {
        conn.execute("ALTER TABLE books ADD COLUMN title_romaji TEXT", [])?;
    }
    if !has_column(conn, "books", "author_romaji")? {
        conn.execute("ALTER TABLE books ADD COLUMN author_romaji TEXT", [])?;
    }
    {
        let rows: Vec<(i64, String, String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, title, author, language FROM books WHERE title_romaji IS NULL",
            )?;
            let mapped =
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            mapped.collect::<rusqlite::Result<_>>()?
        };
        for (id, title, author, language) in rows {
            let title_romaji = super::romaji::romanize_field(&title, None, &language);
            let author_romaji = super::romaji::romanize_field(&author, None, &language);
            conn.execute(
                "UPDATE books SET title_romaji = ?1, author_romaji = ?2 WHERE id = ?3",
                params![title_romaji, author_romaji, id],
            )?;
        }
    }

    // `rekey_annotation_hashes` drops `loc_start` from annotation identity.
    if from_version < 12 {
        // `repair_colors_read_as_notes` rewrites the hashed `note_body`, ahead of the re-key.
        repair_colors_read_as_notes(conn)?;
        rekey_annotation_hashes(conn)?;
    }

    // Separate the highlight from a note fused into it.
    if from_version < 13 {
        split_fused_notes(conn)?;
    }

    // `rekey_annotation_hashes` keys a bookmark on its start alone.
    if from_version < 14 {
        rekey_annotation_hashes(conn)?;

        // Emptying `ink_sync` re-decodes each nbk for its `handwritten_on_content_note` anchors.
        conn.execute("DELETE FROM ink_sync", [])?;

        // Emptying `yjr_sync` re-reads each `.yjr`, backfilling a missing `added_at`.
        conn.execute("DELETE FROM yjr_sync", [])?;
    }

    if from_version < 26 {
        drop_reading_log(conn)?;
    }

    // `user_version` carries `SCHEMA_VERSION`; `backup` gates a restore on it.
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

    Ok(())
}

/// Drop the reading-log tables and the `books` columns only they read, and
/// remove the `reading-log/` archive and `.reading-pulse.json` beside the file.
fn drop_reading_log(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS reading_session_hours;
         DROP TABLE IF EXISTS reading_sessions;
         DROP TABLE IF EXISTS reading_log_dumps;
         DROP TABLE IF EXISTS reading_log_book_ends;
         DROP TABLE IF EXISTS reading_log_asins;
         DROP TABLE IF EXISTS reading_log_points;",
    )?;
    for column in ["finished_at", "max_position"] {
        if has_column(conn, "books", column)? {
            conn.execute(&format!("ALTER TABLE books DROP COLUMN {column}"), [])?;
        }
    }
    if let Some(root) = conn_root(conn) {
        let _ = std::fs::remove_dir_all(root.join("reading-log"));
        let _ = std::fs::remove_file(root.join(".reading-pulse.json"));
    }
    Ok(())
}

/// Split a Sidle-written `note` row into the highlight and the note fused into it.
fn split_fused_notes(conn: &Connection) -> rusqlite::Result<()> {
    struct Fused {
        id: i64,
        title: String,
    }

    let rows: Vec<Fused> = {
        let mut stmt = conn.prepare(
            r#"SELECT a.id, COALESCE(b.title, '')
               FROM annotations a LEFT JOIN books b ON b.id = a.book_id
               WHERE a.kind = 'note' AND a.source = ?1
                 AND a.note_body IS NOT NULL AND a.note_body <> ''
               ORDER BY a.id"#,
        )?;
        let mapped = stmt.query_map(params![super::ingest::SOURCE_SIDLE], |r| {
            Ok(Fused {
                id: r.get(0)?,
                title: r.get(1)?,
            })
        })?;
        mapped.collect::<rusqlite::Result<_>>()?
    };
    if rows.is_empty() {
        return Ok(());
    }

    let tx = conn.unchecked_transaction()?;
    for fused in &rows {
        let Some(row) = get_annotation(&tx, fused.id)? else {
            continue;
        };
        let book_key = if fused.title.is_empty() {
            String::new()
        } else {
            super::ingest::book_match_key(&fused.title)
        };

        // `hl_hash` keys the highlight under the fused row, which keeps its id as the note.
        let hl_hash = super::ingest::annotation_dedup_hash(
            &book_key,
            "highlight",
            row.eid_start,
            row.off_start,
            row.eid_end,
            row.off_end,
            &row.text,
            "",
        );
        match get_annotation_by_hash(&tx, &hl_hash)? {
            // `existing` becomes the note's highlight, taking its colour where it has none.
            Some(existing) => {
                if existing.color.as_deref().unwrap_or("").is_empty()
                    && !row.color.as_deref().unwrap_or("").is_empty()
                {
                    tx.execute(
                        "UPDATE annotations SET color = ?1 WHERE id = ?2",
                        params![row.color, existing.id],
                    )?;
                }
            }
            // `None`: the highlight mints from the note's own anchors.
            None => {
                tx.execute(
                    r#"INSERT INTO annotations
                       (dedup_hash, book_id, kind, eid_start, off_start, eid_end, off_end,
                        loc_start, loc_end, linear_pos, text, note_body, color,
                        clip_title, clip_author, added_at, added_raw, imported_at, source)
                       SELECT ?1, book_id, 'highlight', eid_start, off_start, eid_end, off_end,
                              loc_start, loc_end, linear_pos, text, NULL, color,
                              clip_title, clip_author, added_at, added_raw, imported_at, source
                       FROM annotations WHERE id = ?2"#,
                    params![hl_hash, fused.id],
                )?;
            }
        }

        // `color` lives on the highlight, outside the hash.
        tx.execute(
            "UPDATE annotations SET color = NULL WHERE id = ?1",
            params![fused.id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Move a highlight colour out of `note_body` into `color`.
fn repair_colors_read_as_notes(conn: &Connection) -> rusqlite::Result<()> {
    for color in bokai::formats::krds::COLORS {
        conn.execute(
            "UPDATE annotations SET color = ?1, note_body = NULL \
             WHERE note_body = ?1 AND COALESCE(color, '') = ''",
            params![color],
        )?;
    }
    Ok(())
}

/// Recompute every `annotations` row's `dedup_hash` through
/// [`super::ingest::annotation_dedup_hash`], keeping the lowest `id` per hash
/// and deleting the rest with their `annotation_device` rows.
fn rekey_annotation_hashes(conn: &Connection) -> rusqlite::Result<()> {
    struct Row {
        id: i64,
        old: String,
        new: String,
    }

    let rows: Vec<Row> = {
        let mut stmt = conn.prepare(
            r#"SELECT a.id, a.dedup_hash, a.kind, a.eid_start, a.off_start, a.eid_end,
                      a.off_end, a.text, COALESCE(a.note_body, ''),
                      COALESCE(b.title, '')
               FROM annotations a LEFT JOIN books b ON b.id = a.book_id
               ORDER BY a.id"#,
        )?;
        let mapped = stmt.query_map([], |r| {
            let text: String = r.get(7)?;
            let title: String = r.get(9)?;
            // An orphan's `book_key` is empty, as on the import path.
            let book_key = if title.is_empty() {
                String::new()
            } else {
                super::ingest::book_match_key(&title)
            };
            let new = super::ingest::annotation_dedup_hash(
                &book_key,
                &r.get::<_, String>(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                &text,
                &r.get::<_, String>(8)?,
            );
            Ok(Row {
                id: r.get(0)?,
                old: r.get(1)?,
                new,
            })
        })?;
        mapped.collect::<rusqlite::Result<_>>()?
    };

    if rows.iter().all(|r| r.old == r.new) {
        return Ok(());
    }

    // `winner` holds the earliest row per new hash, `rows` being ordered by id.
    let mut winner: std::collections::HashMap<&str, &Row> = std::collections::HashMap::new();
    for row in &rows {
        winner.entry(row.new.as_str()).or_insert(row);
    }

    let tx = conn.unchecked_transaction()?;

    // `rows` duplicates are deleted ahead of the survivors' rewrite; `dedup_hash` is UNIQUE.
    for row in &rows {
        let survivor = winner[row.new.as_str()];
        if survivor.id == row.id {
            continue;
        }
        // `annotation_device` rows move to the survivor ahead of the delete.
        if row.old != survivor.new {
            tx.execute(
                "UPDATE OR IGNORE annotation_device SET dedup_hash = ?1 WHERE dedup_hash = ?2",
                params![survivor.new, row.old],
            )?;
            tx.execute(
                "DELETE FROM annotation_device WHERE dedup_hash = ?1",
                params![row.old],
            )?;
        }
        tx.execute("DELETE FROM annotations WHERE id = ?1", params![row.id])?;
    }

    // Then the survivors, whose new hashes are unique among themselves.
    for row in &rows {
        if winner[row.new.as_str()].id != row.id || row.old == row.new {
            continue;
        }
        tx.execute(
            "UPDATE annotations SET dedup_hash = ?1 WHERE id = ?2",
            params![row.new, row.id],
        )?;
        tx.execute(
            "UPDATE OR IGNORE annotation_device SET dedup_hash = ?1 WHERE dedup_hash = ?2",
            params![row.new, row.old],
        )?;
        tx.execute(
            "DELETE FROM annotation_device WHERE dedup_hash = ?1",
            params![row.old],
        )?;
    }

    // `artifact_deletions` keys outlive their row: every old hash re-keys, a duplicate's included.
    for row in &rows {
        if row.old == row.new {
            continue;
        }
        tx.execute(
            "UPDATE OR IGNORE artifact_deletions SET key = ?1 WHERE kind = ?2 AND key = ?3",
            params![row.new, DELETION_ANNOTATION, row.old],
        )?;
        tx.execute(
            "DELETE FROM artifact_deletions WHERE kind = ?1 AND key = ?2",
            params![DELETION_ANNOTATION, row.old],
        )?;
    }

    tx.commit()?;
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

/// Current schema version (`PRAGMA user_version`), which a backup records and a
/// restore checks against [`SCHEMA_VERSION`].
pub fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

/// `VACUUM` the database file, returning freed pages to the OS.
pub fn vacuum(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("VACUUM")
}

/// Look up the book whose `kfx_sha256` starts with `prefix`, linking an on-device
/// `<basename>.<sha8>.kfx` to its row.
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

/// Escape SQL `LIKE` metacharacters (`\`, `%`, `_`) for a literal match under
/// `LIKE ?1 ESCAPE '\'`.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Look up the book whose `kfx_path` ends in `/<filename>` — the fallback for
/// on-device files carrying the library row's kfx basename verbatim, matched by
/// suffix against `books/<sha>/<basename>`. Returns the first row that matches.
pub fn find_by_kfx_filename(
    conn: &Connection,
    filename: &str,
) -> rusqlite::Result<Option<BookRow>> {
    let root = conn_root(conn);
    let pattern = format!("%/{}", like_escape(filename));
    conn.query_row(
        SELECT_BOOK_WITH_JOB_BY_KFX_FILENAME,
        params![pattern],
        |row| row_to_book(row, root.as_deref()),
    )
    .optional()
}

/// The book whose `kfx_path` leaf is `<stem>.kfx`.
pub fn find_by_kfx_basename(conn: &Connection, stem: &str) -> rusqlite::Result<Option<BookRow>> {
    let root = conn_root(conn);
    let pattern = format!("%/{}.kfx", like_escape(stem));
    conn.query_row(
        SELECT_BOOK_WITH_JOB_BY_KFX_FILENAME,
        params![pattern],
        |row| row_to_book(row, root.as_deref()),
    )
    .optional()
}

/// True for a book with a pending or converting job, gating device
/// send/delete.
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

/// The book at `index` in series `name`, if any.
pub fn find_in_series(
    conn: &Connection,
    name: &str,
    index: f64,
) -> rusqlite::Result<Option<BookRow>> {
    let root = conn_root(conn);
    conn.query_row(
        SELECT_BOOK_WITH_JOB_BY_SERIES_POSITION,
        params![name, index],
        |row| row_to_book(row, root.as_deref()),
    )
    .optional()
}

/// How many books other than `book_id` sit in series `name`.
pub fn others_in_series(conn: &Connection, name: &str, book_id: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM books WHERE series_name = ?1 AND id <> ?2",
        params![name, book_id],
        |row| row.get(0),
    )
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
    // The three file paths store root-relative; `row_to_book` resolves them on read.
    let root = conn_root(conn);
    let epub_rel = book
        .epub_path
        .map(|p| relativize_for_store(root.as_deref(), p));
    let cover_rel = book
        .cover_path
        .map(|p| relativize_for_store(root.as_deref(), p));
    let kfx_rel = book
        .kfx_path
        .map(|p| relativize_for_store(root.as_deref(), p));
    let pdf_rel = book
        .pdf_path
        .map(|p| relativize_for_store(root.as_deref(), p));
    conn.execute(
        r#"INSERT INTO books
            (sha256, title, author, language, ppd, epub_path, cover_path, kfx_path,
             file_size, imported_at, asin, publisher, published_at,
             series_name, series_index, tags, kfx_sha256, pdf_path, updated_at,
             title_romaji, author_romaji, amazon_asin, source_format)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)"#,
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
            // `updated_at` starts at `imported_at`.
            book.imported_at,
            book.title_romaji,
            book.author_romaji,
            book.amazon_asin,
            book.source_format,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Create or replace the job row for a book. `kind` is set on first insert and kept
/// through later status updates.
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

/// Set `books.kfx_path` for `book_id`, and `kfx_sha256` where it is NULL.
pub fn set_kfx_path_and_sha(
    conn: &Connection,
    book_id: i64,
    kfx_path: &str,
    kfx_sha256: &str,
) -> rusqlite::Result<()> {
    let kfx_rel = relativize_for_store(conn_root(conn).as_deref(), kfx_path);
    conn.execute(
        "UPDATE books SET kfx_path = ?1, kfx_sha256 = COALESCE(kfx_sha256, ?2) WHERE id = ?3",
        params![kfx_rel, kfx_sha256, book_id],
    )?;
    Ok(())
}

/// `(id, kfx_path)` for `books` rows holding a `kfx_path` and no `kfx_sha256`.
pub fn books_missing_kfx_sha(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let root = conn_root(conn);
    let mut stmt = conn.prepare(
        "SELECT id, kfx_path FROM books \
         WHERE kfx_path IS NOT NULL AND kfx_sha256 IS NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            resolve_one(root.as_deref(), &r.get::<_, String>(1)?),
        ))
    })?;
    rows.collect()
}

/// `(id, kfx_path)` for `books` rows holding a `kfx_path` and no `asin`.
pub fn books_missing_asin(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let root = conn_root(conn);
    let mut stmt = conn.prepare(
        "SELECT id, kfx_path FROM books \
         WHERE kfx_path IS NOT NULL AND (asin IS NULL OR asin = '')",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            resolve_one(root.as_deref(), &r.get::<_, String>(1)?),
        ))
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

/// Set `books.asin` for `book_id`. See [`BookRow::asin`].
pub fn set_asin(conn: &Connection, book_id: i64, asin: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE books SET asin = ?1 WHERE id = ?2",
        params![asin, book_id],
    )?;
    Ok(())
}

/// Record the real Amazon catalogue ASIN, the colour-cover key. `None` clears
/// it. Never reaches a produced file — see [`BookRow::amazon_asin`].
pub fn set_amazon_asin(
    conn: &Connection,
    book_id: i64,
    asin: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE books SET amazon_asin = ?1 WHERE id = ?2",
        params![asin, book_id],
    )?;
    Ok(())
}

/// Record the format a book arrived in: provenance no produced file carries.
pub fn set_source_format(
    conn: &Connection,
    book_id: i64,
    format: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE books SET source_format = ?1 WHERE id = ?2",
        params![format, book_id],
    )?;
    Ok(())
}

/// Set `books.updated_at` for `book_id`.
pub fn set_book_updated_at(
    conn: &Connection,
    book_id: i64,
    updated_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE books SET updated_at = ?1 WHERE id = ?2",
        params![updated_at, book_id],
    )?;
    Ok(())
}

/// The id of a `books` row holding `amazon_asin`, other than `except_id`.
pub fn book_id_with_amazon_asin(
    conn: &Connection,
    asin: &str,
    except_id: i64,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM books WHERE amazon_asin = ?1 AND id != ?2 LIMIT 1",
        params![asin, except_id],
        |r| r.get(0),
    )
    .optional()
}

/// The id of the book holding `asin`, if any, linking handwritten ink to its
/// host book: the ink's `.notebooks/<asin>!!PDOC!!` dir name is the content_id
/// stamped into `books.asin`.
pub fn book_id_by_asin(conn: &Connection, asin: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM books WHERE asin = ?1 LIMIT 1",
        params![asin],
        |r| r.get(0),
    )
    .optional()
}

/// Every non-empty `books.asin`.
pub fn book_asins(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT asin FROM books WHERE asin IS NOT NULL AND asin != ''")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// Every editable column of a `books` row, as one whole value.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct MetadataPatch {
    pub title: String,
    pub author: String,
    pub language: String,
    /// Page progression direction: `"rtl"` | `"ltr"` | `None` (Auto), baked into the
    /// generated KFX's reading order.
    #[serde(default)]
    pub ppd: Option<String>,
    /// `horizontal-lr` | `horizontal-rl` | `vertical-rl` | `vertical-lr` |
    /// `None`. See [`BookRow::writing_mode`].
    #[serde(default)]
    pub writing_mode: Option<String>,
    pub publisher: Option<String>,
    pub published_at: Option<String>,
    pub series_name: Option<String>,
    pub series_index: Option<f64>,
    pub tags: Vec<String>,
    /// Editable romaji of `title` and `author`. See [`super::romaji`].
    #[serde(default)]
    pub title_romaji: String,
    #[serde(default)]
    pub author_romaji: String,
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
                  ppd           = ?4,
                  writing_mode  = ?5,
                  publisher     = ?6,
                  published_at  = ?7,
                  series_name   = ?8,
                  series_index  = ?9,
                  tags          = ?10,
                  title_romaji  = ?11,
                  author_romaji = ?12,
                  updated_at    = ?13
              WHERE id = ?14"#,
        params![
            patch.title,
            patch.author,
            patch.language,
            patch.ppd,
            patch.writing_mode,
            patch.publisher,
            patch.published_at,
            patch.series_name,
            patch.series_index,
            tags_json,
            patch.title_romaji,
            patch.author_romaji,
            now_iso(),
            book_id,
        ],
    )?;
    Ok(())
}

/// Sparse patch for bulk metadata editing: `None` leaves a field unchanged on every
/// book, `Some(v)` sets `v` on every book.
#[derive(Debug, Default, Deserialize)]
pub struct BulkMetadataPatch {
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub ppd: Option<String>,
    #[serde(default)]
    pub writing_mode: Option<String>,
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

/// Apply a sparse [`BulkMetadataPatch`] to one book, merging tags additively. Returns
/// `Ok(false)` for an absent book id, `Ok(true)` when a row was written.
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
    let ppd = patch.ppd.clone().or(row.ppd);
    let writing_mode = patch.writing_mode.clone().or(row.writing_mode);
    let publisher = patch.publisher.clone().or(row.publisher);
    let published_at = patch.published_at.clone().or(row.published_at);
    let series_name = patch.series_name.clone().or(row.series_name);

    // Take the patch index if given, else keep the row's; then enforce the
    // "no name ⇒ no index" invariant.
    let mut series_index = patch.series_index.or(row.series_index);
    if series_name.is_none() {
        series_index = None;
    }

    // Additive tag merge: the canonical existing set ∪ add, minus remove, in
    // first-seen order.
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
                  ppd          = ?3,
                  writing_mode = ?4,
                  publisher    = ?5,
                  published_at = ?6,
                  series_name  = ?7,
                  series_index = ?8,
                  tags         = ?9,
                  updated_at   = ?10
              WHERE id = ?11"#,
        params![
            author,
            language,
            ppd,
            writing_mode,
            publisher,
            published_at,
            series_name,
            series_index,
            tags_json,
            now_iso(),
            book_id,
        ],
    )?;
    Ok(true)
}

/// Remove a book and *everything* tied to it, returning its `sha256` for the
/// caller's delete of the on-disk `books/<sha>/` dir, or `None` for an absent
/// id.
pub fn remove_book(conn: &Connection, book_id: i64) -> rusqlite::Result<Option<String>> {
    // Read sha (for the caller) and asin (the ink key) before anything is gone.
    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT sha256, asin FROM books WHERE id = ?1",
            params![book_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((sha, asin)) = row else {
        return Ok(None);
    };

    let tx = conn.unchecked_transaction()?;
    // Text annotations + their per-device presence (book_id-keyed).
    tx.execute(
        "DELETE FROM annotation_device WHERE book_id = ?1",
        params![book_id],
    )?;
    tx.execute(
        "DELETE FROM annotations WHERE book_id = ?1",
        params![book_id],
    )?;
    // Handwritten ink, presence and checkpoint, keyed by `asin`.
    if let Some(asin) = asin.as_deref().filter(|a| !a.is_empty()) {
        tx.execute("DELETE FROM book_ink_device WHERE asin = ?1", params![asin])?;
        tx.execute("DELETE FROM ink_sync WHERE asin = ?1", params![asin])?;
        tx.execute("DELETE FROM book_ink WHERE asin = ?1", params![asin])?;
    }
    // The book row last; its CASCADE clears conversion_jobs / reading_position /
    // yjr_sync.
    tx.execute("DELETE FROM books WHERE id = ?1", params![book_id])?;
    tx.commit()?;
    Ok(Some(sha))
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
    /// Hash of the KFX bytes, required iff `kfx_path` is `Some`.
    pub kfx_sha256: Option<&'a str>,
    /// PDF side of a PDF↔KFX book; `None` for EPUB↔KFX books.
    pub pdf_path: Option<&'a str>,
    pub file_size: i64,
    pub imported_at: &'a str,
    pub asin: Option<&'a str>,
    pub amazon_asin: Option<&'a str>,
    pub publisher: Option<&'a str>,
    pub published_at: Option<&'a str>,
    pub series_name: Option<&'a str>,
    pub series_index: Option<f64>,
    /// The canonical tag list; `insert_book` serializes it to a JSON array TEXT.
    pub tags: &'a [String],
    /// Editable romaji of the title and author, rendered yomigana-aware at import.
    pub title_romaji: &'a str,
    pub author_romaji: &'a str,
    /// The format that arrived, as `SourceKind::as_str` names it.
    pub source_format: Option<&'a str>,
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
           b.kfx_sha256, b.pdf_path,
           COALESCE(b.updated_at, b.imported_at),
           COALESCE(b.title_romaji, ''), COALESCE(b.author_romaji, ''),
           b.writing_mode, b.amazon_asin, b.source_format
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
           b.kfx_sha256, b.pdf_path,
           COALESCE(b.updated_at, b.imported_at),
           COALESCE(b.title_romaji, ''), COALESCE(b.author_romaji, ''),
           b.writing_mode, b.amazon_asin, b.source_format
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
           b.kfx_sha256, b.pdf_path,
           COALESCE(b.updated_at, b.imported_at),
           COALESCE(b.title_romaji, ''), COALESCE(b.author_romaji, ''),
           b.writing_mode, b.amazon_asin, b.source_format
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    WHERE b.id = ?1
"#;

/// One book per `(series_name, series_index)` pair.
const SELECT_BOOK_WITH_JOB_BY_SERIES_POSITION: &str = r#"
    SELECT b.id, b.sha256, b.title, b.author, b.language, b.ppd,
           b.epub_path, b.cover_path, b.kfx_path,
           b.file_size, b.imported_at, b.asin,
           COALESCE(j.status, 'pending') AS status, j.error, j.kind,
           b.publisher, b.published_at, b.series_name, b.series_index, b.tags,
           b.kfx_sha256, b.pdf_path,
           COALESCE(b.updated_at, b.imported_at),
           COALESCE(b.title_romaji, ''), COALESCE(b.author_romaji, ''),
           b.writing_mode, b.amazon_asin, b.source_format
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    WHERE b.series_name = ?1 AND b.series_index = ?2
    ORDER BY b.id
    LIMIT 1
"#;

const SELECT_BOOK_WITH_JOB_BY_KFX_SHA_PREFIX: &str = r#"
    SELECT b.id, b.sha256, b.title, b.author, b.language, b.ppd,
           b.epub_path, b.cover_path, b.kfx_path,
           b.file_size, b.imported_at, b.asin,
           COALESCE(j.status, 'pending') AS status, j.error, j.kind,
           b.publisher, b.published_at, b.series_name, b.series_index, b.tags,
           b.kfx_sha256, b.pdf_path,
           COALESCE(b.updated_at, b.imported_at),
           COALESCE(b.title_romaji, ''), COALESCE(b.author_romaji, ''),
           b.writing_mode, b.amazon_asin, b.source_format
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    WHERE b.kfx_sha256 LIKE ?1
    LIMIT 1
"#;

/// Match by the **basename** of `kfx_path`: an on-device file named for the
/// library row's kfx basename.
const SELECT_BOOK_WITH_JOB_BY_KFX_FILENAME: &str = r#"
    SELECT b.id, b.sha256, b.title, b.author, b.language, b.ppd,
           b.epub_path, b.cover_path, b.kfx_path,
           b.file_size, b.imported_at, b.asin,
           COALESCE(j.status, 'pending') AS status, j.error, j.kind,
           b.publisher, b.published_at, b.series_name, b.series_index, b.tags,
           b.kfx_sha256, b.pdf_path,
           COALESCE(b.updated_at, b.imported_at),
           COALESCE(b.title_romaji, ''), COALESCE(b.author_romaji, ''),
           b.writing_mode, b.amazon_asin, b.source_format
    FROM books b
    LEFT JOIN conversion_jobs j ON j.book_id = b.id
    WHERE b.kfx_path LIKE ?1 ESCAPE '\'
    LIMIT 1
"#;

/// `(cover_thumb(sha), its mtime)` where that file exists, else
/// `(None, cover_path's mtime)`.
fn served_cover(root: Option<&Path>, sha: &str, cover_path: Option<&str>) -> (Option<String>, i64) {
    if let Some(r) = root {
        let thumb = super::LibraryPaths {
            root: r.to_path_buf(),
        }
        .cover_thumb(sha);
        if let Ok(meta) = std::fs::metadata(&thumb) {
            return (
                Some(thumb.to_string_lossy().into_owned()),
                mtime_millis(&meta),
            );
        }
    }
    // No thumb on disk: `rev` tracks the full cover's mtime.
    let rev = cover_path
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| mtime_millis(&m))
        .unwrap_or(0);
    (None, rev)
}

/// Milliseconds since the Unix epoch for a file's modified time; 0 if it
/// predates the epoch or the mtime is unreadable.
fn mtime_millis(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Millisecond mtime of the file at `path`, or 0 for an unreadable one.
pub fn path_mtime_millis(path: &str) -> i64 {
    std::fs::metadata(path)
        .ok()
        .map(|m| mtime_millis(&m))
        .unwrap_or(0)
}

fn row_to_book(row: &rusqlite::Row<'_>, root: Option<&Path>) -> rusqlite::Result<BookRow> {
    let tags_json: String = row.get(19)?;
    // A corrupt `tags_json` parses to no tags.
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    // `search_key` borrows these ahead of the struct taking them.
    let sha256: String = row.get(1)?;
    let title: String = row.get(2)?;
    let author: String = row.get(3)?;
    let language: String = row.get(4)?;
    let publisher: Option<String> = row.get(15)?;
    let series_name: Option<String> = row.get(17)?;
    let title_romaji: String = row.get(23)?;
    let author_romaji: String = row.get(24)?;
    // Derived (not a column): the device match key — curated romaji (or a live
    // fallback when a column is empty) + auto-romanized series/publisher/tags +
    // the raw fields. Recomputed fresh on every read.
    let search_key = super::romaji::search_key(
        &title,
        &author,
        publisher.as_deref(),
        series_name.as_deref(),
        &tags,
        &language,
        &title_romaji,
        &author_romaji,
    );
    // `cover_path` feeds both the struct field and [`served_cover`].
    let cover_path = resolve_opt(root, row.get(7)?);
    // Derived, not columns: the thumbnail sidecar and the served-image cache token,
    // from one stat. A `None` root or a missing thumb yields `(None, ..)`.
    let (cover_thumb_path, cover_rev) = served_cover(root, &sha256, cover_path.as_deref());
    Ok(BookRow {
        id: row.get(0)?,
        sha256,
        title,
        author,
        language,
        ppd: row.get(5)?,
        writing_mode: row.get(25)?,
        // Stored root-relative; resolve to absolute against the live root.
        epub_path: resolve_opt(root, row.get(6)?),
        cover_path,
        cover_thumb_path,
        cover_rev,
        kfx_path: resolve_opt(root, row.get(8)?),
        file_size: row.get(9)?,
        imported_at: row.get(10)?,
        asin: row.get(11)?,
        // `row.get` reads positionally; these columns follow `asin`.
        amazon_asin: row.get(26)?,
        source_format: row.get(27)?,
        status: row.get(12)?,
        error: row.get(13)?,
        kind: row.get(14)?,
        publisher,
        published_at: row.get(16)?,
        series_name,
        series_index: row.get(18)?,
        tags,
        kfx_sha256: row.get(20)?,
        pdf_path: resolve_opt(root, row.get(21)?),
        updated_at: row.get(22)?,
        title_romaji,
        author_romaji,
        search_key,
    })
}

// ---------------------------------------------------------------------------
// Notebooks (Scribe handwriting).

/// One stored Scribe notebook; its files live at `notebooks/<uuid>/`.
#[derive(Debug, Clone, Serialize)]
pub struct NotebookRow {
    pub id: i64,
    pub uuid: String,
    pub title: String,
    pub page_count: i64,
    /// SHA-256 of the backed-up `nbk` bytes — change-detects an edited notebook.
    pub nbk_sha256: Option<String>,
    pub imported_at: String,
    /// On-device "Date Modified" of the source `nbk` (RFC 3339), captured at import.
    pub updated_at: Option<String>,
}

const SELECT_NOTEBOOKS: &str =
    "SELECT id, uuid, title, page_count, nbk_sha256, imported_at, updated_at FROM notebooks";

fn row_to_notebook(row: &rusqlite::Row<'_>) -> rusqlite::Result<NotebookRow> {
    Ok(NotebookRow {
        id: row.get(0)?,
        uuid: row.get(1)?,
        title: row.get(2)?,
        page_count: row.get(3)?,
        nbk_sha256: row.get(4)?,
        imported_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

pub fn list_notebooks(conn: &Connection) -> rusqlite::Result<Vec<NotebookRow>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT_NOTEBOOKS} ORDER BY imported_at DESC, id DESC"
    ))?;
    stmt.query_map([], row_to_notebook)?.collect()
}

pub fn get_notebook(conn: &Connection, id: i64) -> rusqlite::Result<Option<NotebookRow>> {
    conn.query_row(
        &format!("{SELECT_NOTEBOOKS} WHERE id = ?1"),
        params![id],
        row_to_notebook,
    )
    .optional()
}

pub fn get_notebook_by_uuid(
    conn: &Connection,
    uuid: &str,
) -> rusqlite::Result<Option<NotebookRow>> {
    conn.query_row(
        &format!("{SELECT_NOTEBOOKS} WHERE uuid = ?1"),
        params![uuid],
        row_to_notebook,
    )
    .optional()
}

/// `updated_at` as `%Y-%m-%d %H:%M` in the local zone, falling back to its own
/// first 16 characters with the `T` replaced.
fn default_notebook_title(updated_at: &str) -> String {
    use chrono::{DateTime, Local, NaiveDateTime};
    if let Ok(dt) = DateTime::parse_from_rfc3339(updated_at) {
        return dt
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M")
            .to_string();
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(updated_at, "%Y-%m-%dT%H:%M:%S") {
        return ndt.format("%Y-%m-%d %H:%M").to_string();
    }
    updated_at
        .get(..16)
        .map(|s| s.replace('T', " "))
        .unwrap_or_else(|| updated_at.to_string())
}

/// Insert a notebook, defaulting its title to the first-import datetime
/// ([`default_notebook_title`]). A known uuid updates page count, content hash
/// and `updated_at`. Returns the row id.
pub fn upsert_notebook(
    conn: &Connection,
    uuid: &str,
    page_count: i64,
    nbk_sha256: &str,
    imported_at: &str,
    updated_at: &str,
) -> rusqlite::Result<i64> {
    let title = default_notebook_title(updated_at);
    conn.execute(
        r#"INSERT INTO notebooks (uuid, title, page_count, nbk_sha256, imported_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(uuid) DO UPDATE SET
                page_count  = excluded.page_count,
                nbk_sha256  = excluded.nbk_sha256,
                updated_at  = excluded.updated_at,
                title       = CASE WHEN notebooks.title IN ('Notebook', '')
                                   THEN excluded.title ELSE notebooks.title END"#,
        params![uuid, title, page_count, nbk_sha256, imported_at, updated_at],
    )?;
    conn.query_row(
        "SELECT id FROM notebooks WHERE uuid = ?1",
        params![uuid],
        |r| r.get(0),
    )
}

/// Backfill `updated_at` for a notebook carrying NULL, without re-extracting it.
pub fn backfill_notebook_updated_at(
    conn: &Connection,
    uuid: &str,
    updated_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE notebooks SET updated_at = ?1 WHERE uuid = ?2 AND updated_at IS NULL",
        params![updated_at, uuid],
    )?;
    Ok(())
}

/// Set [`default_notebook_title`] on the `notebooks` row `uuid`, where its
/// `title` is `'Notebook'` or empty.
pub fn backfill_notebook_default_title(
    conn: &Connection,
    uuid: &str,
    updated_at: &str,
) -> rusqlite::Result<()> {
    let title = default_notebook_title(updated_at);
    conn.execute(
        "UPDATE notebooks SET title = ?1 WHERE uuid = ?2 AND title IN ('Notebook', '')",
        params![title, uuid],
    )?;
    Ok(())
}

pub fn rename_notebook(conn: &Connection, id: i64, title: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE notebooks SET title = ?1 WHERE id = ?2",
        params![title, id],
    )?;
    Ok(())
}

/// Delete a notebook row, returning its uuid for the caller's file removal.
pub fn remove_notebook(conn: &Connection, id: i64) -> rusqlite::Result<Option<String>> {
    let uuid: Option<String> = conn
        .query_row(
            "SELECT uuid FROM notebooks WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(u) = &uuid {
        conn.execute("DELETE FROM notebooks WHERE id = ?1", params![id])?;
        // `record_deletion` keeps a device or folder re-import from resurrecting it.
        record_deletion(conn, DELETION_NOTEBOOK, u)?;
    }
    Ok(uuid)
}

// ---------------------------------------------------------------------------
// Handwritten ink on a sideloaded doc (`book_ink`).

/// One imported ink page: handwriting on a single host PDF page.
#[derive(Debug, Clone, Serialize)]
pub struct BookInkRow {
    pub id: i64,
    pub book_id: Option<i64>,
    pub asin: String,
    pub container_id: String,
    /// 0-based host PDF page the ink overlays; `None` where the anchor eid resolves to no page.
    pub host_page: Option<i64>,
    pub host_eid: Option<i64>,
    /// Device linear position of the host anchor — the display sort across pages.
    pub host_linear: Option<i64>,
    pub nbk_sha256: Option<String>,
    pub imported_at: String,
    /// Reversible "hidden from the reader" flag (kept in the backup).
    pub hidden: bool,
}

const SELECT_BOOK_INK: &str = "SELECT id, book_id, asin, container_id, host_page, \
    host_eid, host_linear, nbk_sha256, imported_at, hidden FROM book_ink";

fn row_to_book_ink(row: &rusqlite::Row<'_>) -> rusqlite::Result<BookInkRow> {
    Ok(BookInkRow {
        id: row.get(0)?,
        book_id: row.get(1)?,
        asin: row.get(2)?,
        container_id: row.get(3)?,
        host_page: row.get(4)?,
        host_eid: row.get(5)?,
        host_linear: row.get(6)?,
        nbk_sha256: row.get(7)?,
        imported_at: row.get(8)?,
        hidden: row.get::<_, i64>(9)? != 0,
    })
}

/// Insert payload for one ink page.
pub struct NewBookInk<'a> {
    pub book_id: Option<i64>,
    pub asin: &'a str,
    pub container_id: &'a str,
    pub host_page: Option<i64>,
    pub host_eid: Option<i64>,
    pub host_linear: Option<i64>,
    pub nbk_sha256: Option<&'a str>,
    pub imported_at: &'a str,
}

/// Upsert one ink page, keyed by the stable `(asin, container_id)`; a re-import
/// refreshes its anchor, book link and hash in place.
pub fn upsert_book_ink(conn: &Connection, ink: &NewBookInk<'_>) -> rusqlite::Result<i64> {
    conn.execute(
        r#"INSERT INTO book_ink
            (book_id, asin, container_id, host_page, host_eid, host_linear, nbk_sha256, imported_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(asin, container_id) DO UPDATE SET
                book_id     = excluded.book_id,
                host_page   = excluded.host_page,
                host_eid    = excluded.host_eid,
                host_linear = excluded.host_linear,
                nbk_sha256  = excluded.nbk_sha256"#,
        params![
            ink.book_id,
            ink.asin,
            ink.container_id,
            ink.host_page,
            ink.host_eid,
            ink.host_linear,
            ink.nbk_sha256,
            ink.imported_at,
        ],
    )?;
    conn.query_row(
        "SELECT id FROM book_ink WHERE asin = ?1 AND container_id = ?2",
        params![ink.asin, ink.container_id],
        |r| r.get(0),
    )
}

/// A book's ink pages, display-ordered by the host anchor linear position (then
/// page, then id) — the device's reading order, NOT the nbk's creation order.
pub fn list_book_ink(conn: &Connection, book_id: i64) -> rusqlite::Result<Vec<BookInkRow>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT_BOOK_INK} WHERE book_id = ?1 ORDER BY host_linear, host_page, id"
    ))?;
    stmt.query_map(params![book_id], row_to_book_ink)?.collect()
}

/// One ink row by id (for the reader panel's delete + cache cleanup).
pub fn get_book_ink(conn: &Connection, id: i64) -> rusqlite::Result<Option<BookInkRow>> {
    conn.query_row(
        &format!("{SELECT_BOOK_INK} WHERE id = ?1"),
        params![id],
        row_to_book_ink,
    )
    .optional()
}

/// Ink pages overlaying one 0-based host PDF page (usually one; possibly more).
pub fn list_book_ink_on_page(
    conn: &Connection,
    book_id: i64,
    host_page: i64,
) -> rusqlite::Result<Vec<BookInkRow>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT_BOOK_INK} WHERE book_id = ?1 AND host_page = ?2 AND hidden = 0 ORDER BY host_linear, id"
    ))?;
    stmt.query_map(params![book_id, host_page], row_to_book_ink)?
        .collect()
}

/// Distinct host pages that carry anchored ink for a book — the reader chrome's
/// "these pages have ink" set (drives the per-page overlay fetch + a marker).
pub fn book_ink_host_pages(conn: &Connection, book_id: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT host_page FROM book_ink \
         WHERE book_id = ?1 AND host_page IS NOT NULL AND hidden = 0 ORDER BY host_page",
    )?;
    stmt.query_map(params![book_id], |r| r.get(0))?.collect()
}

/// Unlinked ink pages (book not in the library) — the orphan inbox, for relink.
pub fn list_unlinked_book_ink(conn: &Connection) -> rusqlite::Result<Vec<BookInkRow>> {
    let mut stmt = conn.prepare(&format!("{SELECT_BOOK_INK} WHERE book_id IS NULL"))?;
    stmt.query_map([], row_to_book_ink)?.collect()
}

/// Move `book_ink`, `book_ink_device` and `ink_sync` rows from `old_key` to
/// `new_key`.
pub fn relink_ink(conn: &Connection, old_key: &str, new_key: &str) -> rusqlite::Result<()> {
    if old_key.is_empty() || old_key == new_key {
        return Ok(());
    }
    for table in ["book_ink", "book_ink_device", "ink_sync"] {
        conn.execute(
            &format!("UPDATE OR IGNORE {table} SET asin = ?1 WHERE asin = ?2"),
            params![new_key, old_key],
        )?;
    }
    Ok(())
}

/// Point an ink page at a (re-)matched book. Used by ingest's relink pass.
pub fn set_book_ink_book_id(conn: &Connection, id: i64, book_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE book_ink SET book_id = ?1 WHERE id = ?2",
        params![book_id, id],
    )?;
    Ok(())
}

/// The nbk content sha last decoded for `(device_serial, asin)`, if that device has
/// synced this book's ink. Mirrors [`get_yjr_sync_sha`].
pub fn get_ink_sync_sha(
    conn: &Connection,
    device_serial: &str,
    asin: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT nbk_sha FROM ink_sync WHERE device_serial = ?1 AND asin = ?2",
        params![device_serial, asin],
        |row| row.get(0),
    )
    .optional()
}

/// Every `(asin, nbk_sha)` this device has synced ink for — [`get_ink_sync_sha`]
/// for the whole device at once.
pub fn ink_sync_shas(
    conn: &Connection,
    device_serial: &str,
) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT asin, nbk_sha FROM ink_sync WHERE device_serial = ?1")?;
    let rows = stmt.query_map(params![device_serial], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// `(uuid, nbk_sha256)` for every `notebooks` row whose `nbk_sha256` is set.
pub fn notebook_shas(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt =
        conn.prepare("SELECT uuid, nbk_sha256 FROM notebooks WHERE nbk_sha256 IS NOT NULL")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// Record the nbk content sha decoded for `(device_serial, asin)` (upsert).
pub fn set_ink_sync_sha(
    conn: &Connection,
    device_serial: &str,
    asin: &str,
    nbk_sha: &str,
    now: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        r#"INSERT INTO ink_sync (device_serial, asin, nbk_sha, synced_at) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(device_serial, asin) DO UPDATE SET
                nbk_sha = excluded.nbk_sha,
                synced_at = excluded.synced_at"#,
        params![device_serial, asin, nbk_sha, now],
    )?;
    Ok(())
}

/// Upsert a `book_ink_device` row per `current_containers` entry under
/// `(asin, device_serial)`, and delete that pair's rows outside the list.
pub fn record_ink_device_presence(
    conn: &Connection,
    device_serial: &str,
    asin: &str,
    book_id: Option<i64>,
    current_containers: &[String],
    now: &str,
) -> rusqlite::Result<()> {
    for cid in current_containers {
        conn.execute(
            r#"INSERT INTO book_ink_device (asin, container_id, device_serial, book_id, last_seen)
                VALUES (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(asin, container_id, device_serial) DO UPDATE SET
                    book_id = excluded.book_id,
                    last_seen = excluded.last_seen"#,
            params![asin, cid, device_serial, book_id, now],
        )?;
    }
    // Drop this (device, asin)'s presence rows the pass left untouched.
    conn.execute(
        "DELETE FROM book_ink_device \
         WHERE device_serial = ?1 AND asin = ?2 AND last_seen <> ?3",
        params![device_serial, asin, now],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Annotations and last-read positions imported off the Kindle.

/// One stored annotation; `book_id == None` marks an orphan-inbox entry.
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
    /// Reversible "hidden from the reader" flag (kept in the backup).
    pub hidden: bool,
}

/// Insert payload for one annotation, borrowed from parsed records.
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

/// Insert an annotation, ignoring exact duplicates (same `dedup_hash`): `true` for a
/// new row, `false` for one the library holds.
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

/// Give an annotation the capture date it has none for, returning whether a row
/// changed.
pub fn fill_missing_added_at(
    conn: &Connection,
    dedup_hash: &str,
    added_at: &str,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE annotations SET added_at = ?1 \
         WHERE dedup_hash = ?2 AND (added_at IS NULL OR added_at = '')",
        params![added_at, dedup_hash],
    )?;
    Ok(n > 0)
}

const SELECT_ANNOTATION: &str = r#"
    SELECT id, dedup_hash, book_id, kind, eid_start, off_start, eid_end, off_end,
           loc_start, loc_end, linear_pos, text, note_body, color,
           clip_title, clip_author, added_at, added_raw, imported_at, source, hidden
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
        hidden: row.get::<_, i64>(20)? != 0,
    })
}

/// Whether `device_serial` has synced a coloured highlight.
pub fn device_uses_colors(conn: &Connection, device_serial: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM annotations a
             JOIN annotation_device d ON d.dedup_hash = a.dedup_hash
             WHERE d.device_serial = ?1 AND a.color IS NOT NULL AND a.color <> '')",
        params![device_serial],
        |r| r.get(0),
    )
}

/// Annotations for one book, ordered by reading position.
pub fn list_annotations_for_book(
    conn: &Connection,
    book_id: i64,
) -> rusqlite::Result<Vec<AnnotationRow>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT_ANNOTATION} WHERE book_id = ?1 ORDER BY linear_pos, loc_start, id"
    ))?;
    stmt.query_map(params![book_id], row_to_annotation)?
        .collect()
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

/// One annotation by id.
pub fn get_annotation(conn: &Connection, id: i64) -> rusqlite::Result<Option<AnnotationRow>> {
    conn.query_row(
        &format!("{SELECT_ANNOTATION} WHERE id = ?1"),
        params![id],
        row_to_annotation,
    )
    .optional()
}

/// One annotation by its `dedup_hash`.
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

/// Set `kind`, `note_body`, `color` and `dedup_hash` on the `annotations` row
/// `id`.
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

/// Delete the `annotations` row `id` and the `annotation_device` rows on its
/// `dedup_hash`.
pub fn delete_annotation(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    // `hash` keys the presence rows cleaned below.
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
        // `record_deletion` keeps device sync from re-adding it.
        record_deletion(conn, DELETION_ANNOTATION, &h)?;
    }
    Ok(n > 0)
}

/// Last-read position for a book. `device_serial` is `""` for the Sidle-native row
/// (`source='sidle'`) and the Kindle's serial for an imported one (`source='device'`).
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

/// Every `reading_position` row for `book_id`, one per `(source,
/// device_serial)`.
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

/// Upsert a `reading_position` row on `(book_id, source, device_serial)`.
/// `source` `"sidle"` takes `device_serial` `""`.
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
        params![
            book_id,
            eid,
            offset,
            linear_pos,
            source,
            device_serial,
            now_iso()
        ],
    )?;
    Ok(())
}

/// `yjr_sync.yjr_sha` for `(device_serial, book_id)`.
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

// ---------------------------------------------------------------------------

/// `artifact_deletions.kind` discriminators.
pub const DELETION_ANNOTATION: &str = "annotation";
pub const DELETION_INK: &str = "ink";
pub const DELETION_NOTEBOOK: &str = "notebook";

/// The `artifact_deletions` key for an ink page: `asin` + US (0x1f) + `container_id`.
pub fn ink_deletion_key(asin: &str, container_id: &str) -> String {
    format!("{asin}\u{1f}{container_id}")
}

/// Record a Sidle-side deletion, which the additive sync honours.
pub fn record_deletion(conn: &Connection, kind: &str, key: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO artifact_deletions (kind, key, deleted_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(kind, key) DO UPDATE SET deleted_at = excluded.deleted_at",
        params![kind, key, now_iso()],
    )?;
    Ok(())
}

/// Whether this artifact was deleted in Sidle.
pub fn is_deleted(conn: &Connection, kind: &str, key: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM artifact_deletions WHERE kind = ?1 AND key = ?2",
        params![kind, key],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
}

/// Clear one deletion record (un-delete a single artifact).
pub fn clear_deletion(conn: &Connection, kind: &str, key: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM artifact_deletions WHERE kind = ?1 AND key = ?2",
        params![kind, key],
    )?;
    Ok(())
}

/// Clear every deletion record, returning how many cleared.
pub fn clear_all_deletions(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM artifact_deletions", [])
}

/// Set the reversible "hidden from the reader" flag on one annotation.
pub fn set_annotation_hidden(conn: &Connection, id: i64, hidden: bool) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE annotations SET hidden = ?1 WHERE id = ?2",
        params![hidden as i64, id],
    )?;
    Ok(())
}

/// Set the reversible "hidden from the reader" flag on one ink page.
pub fn set_book_ink_hidden(conn: &Connection, id: i64, hidden: bool) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE book_ink SET hidden = ?1 WHERE id = ?2",
        params![hidden as i64, id],
    )?;
    Ok(())
}

/// Drop a device's `yjr_sync` and `ink_sync` checkpoints; its next sync re-pulls everything.
pub fn clear_device_sync_checkpoints(
    conn: &Connection,
    device_serial: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM yjr_sync WHERE device_serial = ?1",
        params![device_serial],
    )?;
    conn.execute(
        "DELETE FROM ink_sync WHERE device_serial = ?1",
        params![device_serial],
    )?;
    Ok(())
}

/// Delete one ink page by id with its device-presence rows, recording the deletion.
/// Returns its `(asin, container_id)` for the caller's SVG cleanup.
pub fn delete_book_ink(conn: &Connection, id: i64) -> rusqlite::Result<Option<(String, String)>> {
    let key: Option<(String, String)> = conn
        .query_row(
            "SELECT asin, container_id FROM book_ink WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((asin, cid)) = &key {
        conn.execute("DELETE FROM book_ink WHERE id = ?1", params![id])?;
        conn.execute(
            "DELETE FROM book_ink_device WHERE asin = ?1 AND container_id = ?2",
            params![asin, cid],
        )?;
        record_deletion(conn, DELETION_INK, &ink_deletion_key(asin, cid))?;
    }
    Ok(key)
}

/// Upsert an `annotation_device` row per `current_hashes` entry under
/// `(device_serial, book_id)`, and delete that pair's rows outside the list.
pub fn record_device_book_presence(
    conn: &Connection,
    device_serial: &str,
    book_id: i64,
    current_hashes: &[String],
    now: &str,
) -> rusqlite::Result<()> {
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

    // Drop this (device, book)'s presence rows the pass left untouched.
    conn.execute(
        "DELETE FROM annotation_device \
         WHERE device_serial = ?1 AND book_id = ?2 AND last_seen <> ?3",
        params![device_serial, book_id, now],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// On-device apps — where each app's mount tree lives on this machine.
// ---------------------------------------------------------------------------

/// [`AppSourceRow::source_kind`] discriminators.
pub const APP_SOURCE_LOCAL: &str = "local";
pub const APP_SOURCE_RELEASE: &str = "release";

/// One registered app: where to find its tree, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppSourceRow {
    /// The app's id — its directory name under `extensions/`.
    pub id: String,
    /// [`APP_SOURCE_LOCAL`] or [`APP_SOURCE_RELEASE`].
    pub source_kind: String,
    /// The repo path or `owner/repo` the user named.
    pub source: String,
    /// The mount root inside `source`, the directory `extensions/` sits in.
    pub root: String,
    pub added_at: i64,
}

fn row_to_app_source(row: &rusqlite::Row<'_>) -> rusqlite::Result<AppSourceRow> {
    Ok(AppSourceRow {
        id: row.get("id")?,
        source_kind: row.get("source_kind")?,
        source: row.get("source")?,
        root: row.get("root")?,
        added_at: row.get("added_at")?,
    })
}

/// Every registered app, by id.
pub fn list_app_sources(conn: &Connection) -> rusqlite::Result<Vec<AppSourceRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, source_kind, source, root, added_at FROM apps ORDER BY id COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], row_to_app_source)?;
    rows.collect()
}

pub fn get_app_source(conn: &Connection, id: &str) -> rusqlite::Result<Option<AppSourceRow>> {
    let mut stmt =
        conn.prepare("SELECT id, source_kind, source, root, added_at FROM apps WHERE id = ?1")?;
    let mut rows = stmt.query_map(params![id], row_to_app_source)?;
    rows.next().transpose()
}

/// Register an app, or repoint a registered one.
pub fn upsert_app_source(
    conn: &Connection,
    id: &str,
    source_kind: &str,
    source: &str,
    root: &str,
) -> rusqlite::Result<()> {
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT INTO apps (id, source_kind, source, root, added_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
             source_kind = excluded.source_kind,
             source      = excluded.source,
             root        = excluded.root",
        params![id, source_kind, source, root, now],
    )?;
    Ok(())
}

/// Forget an app, `true` for a row that went. It unregisters the source alone,
/// touching neither the tree on disk nor a device's copy.
pub fn remove_app_source(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM apps WHERE id = ?1", params![id])? > 0)
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
                amazon_asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
                title_romaji: "",
                author_romaji: "",
                source_format: None,
            },
        )
        .expect("insert")
    }

    #[test]
    fn set_kfx_path_and_sha_freezes_identity() {
        let conn = fresh_db();
        let id = insert_minimal(&conn, "src-sha", "Book");
        // `aaaa1111` mints the identity.
        set_kfx_path_and_sha(&conn, id, "books/src-sha/Book.kfx", "aaaa1111").unwrap();
        assert_eq!(
            get_book(&conn, id).unwrap().unwrap().kfx_sha256.as_deref(),
            Some("aaaa1111")
        );
        // A reconvert passes `bbbb2222`; `kfx_sha256` stays frozen.
        set_kfx_path_and_sha(&conn, id, "books/src-sha/Book.kfx", "bbbb2222").unwrap();
        assert_eq!(
            get_book(&conn, id).unwrap().unwrap().kfx_sha256.as_deref(),
            Some("aaaa1111"),
            "kfx_sha256 is frozen once minted; a reconvert must not re-stamp it"
        );
    }

    #[test]
    fn find_by_kfx_basename_matches_stem_and_escapes_metachars() {
        let conn = fresh_db();
        // Two basenames differing only where one has `_`, a `LIKE` wildcard.
        let a = insert_minimal(&conn, "sha-a", "A");
        set_kfx_path_and_sha(&conn, a, "books/sha-a/Title_ Intro.kfx", "aaaa0000").unwrap();
        let b = insert_minimal(&conn, "sha-b", "B");
        set_kfx_path_and_sha(&conn, b, "books/sha-b/TitleX Intro.kfx", "bbbb0000").unwrap();

        let hit = find_by_kfx_basename(&conn, "Title_ Intro")
            .unwrap()
            .expect("underscore basename resolves");
        assert_eq!(hit.id, a, "`_` matched literally, not as a wildcard");
        assert!(
            find_by_kfx_basename(&conn, "TitleX Intro")
                .unwrap()
                .is_some()
        );
        assert!(
            find_by_kfx_basename(&conn, "No Such Book")
                .unwrap()
                .is_none()
        );
    }

    fn insert_with_language(conn: &Connection, sha: &str, language: &str) -> i64 {
        insert_book(
            conn,
            &NewBook {
                sha256: sha,
                title: "T",
                author: "",
                language,
                ppd: None,
                epub_path: None,
                cover_path: None,
                kfx_path: None,
                kfx_sha256: None,
                pdf_path: None,
                file_size: 0,
                imported_at: "2026-05-19T00:00:00Z",
                asin: None,
                amazon_asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
                title_romaji: "",
                author_romaji: "",
                source_format: None,
            },
        )
        .expect("insert")
    }

    #[test]
    fn migrate_harmonizes_existing_language_tags() {
        let conn = fresh_db();
        // `insert_book` writes the language verbatim, as a pre-v10 row holds it.
        let en = insert_with_language(&conn, "sha-en", "en-US");
        let zh = insert_with_language(&conn, "sha-zh", "zh-hant");
        let ja = insert_with_language(&conn, "sha-ja", "jpn");
        let blank = insert_with_language(&conn, "sha-blank", "");

        // Re-open the library: migrate() is idempotent and runs the backfill.
        migrate(&conn).expect("re-migrate");

        let lang = |id| get_book(&conn, id).unwrap().unwrap().language;
        assert_eq!(lang(en), "en");
        assert_eq!(lang(zh), "zh-Hant");
        assert_eq!(lang(ja), "ja");
        assert_eq!(lang(blank), "", "blank language stays blank");
    }

    #[test]
    fn migrate_backfills_null_romaji() {
        let conn = fresh_db();
        // Mimic a pre-v11 row. `insert_book` writes the romaji columns, which
        // this NULLs out beside a JP language for the upgrade path.
        let id = insert_minimal(&conn, "sha-jp", "世界");
        conn.execute(
            "UPDATE books SET title_romaji = NULL, author_romaji = NULL, language = 'ja' WHERE id = ?1",
            params![id],
        )
        .unwrap();

        // `migrate` backfills NULL romaji from the raw title and author.
        migrate(&conn).expect("re-migrate");

        let book = get_book(&conn, id).unwrap().unwrap();
        assert_eq!(
            book.title_romaji, "sekai",
            "kakasi backfilled the title romaji"
        );
        // `search_key` folds the backfilled romaji in.
        assert!(
            book.search_key.contains("sekai"),
            "search_key: {}",
            book.search_key
        );

        // Idempotent: a second migrate finds no NULL rows and changes nothing.
        migrate(&conn).expect("re-migrate idempotent");
        assert_eq!(get_book(&conn, id).unwrap().unwrap().title_romaji, "sekai");
    }

    #[test]
    fn notebook_imported_at_is_frozen_on_reimport() {
        let conn = fresh_db();
        // First import.
        let id = upsert_notebook(
            &conn,
            "uuid-1",
            3,
            "sha-a",
            "2026-06-01T10:00:00Z",
            "2026-06-01T09:00:00",
        )
        .expect("first import");
        // Re-import of the SAME notebook with edited content + a later import time.
        let id2 = upsert_notebook(
            &conn,
            "uuid-1",
            5,
            "sha-b",
            "2026-06-09T20:00:00Z",
            "2026-06-09T19:30:00",
        )
        .expect("re-import");
        assert_eq!(id, id2, "re-import upserts the same row");

        let row = get_notebook_by_uuid(&conn, "uuid-1")
            .expect("query")
            .expect("row");
        // `imported_at` keeps the first-import time.
        assert_eq!(row.imported_at, "2026-06-01T10:00:00Z");
        assert_eq!(row.page_count, 5);
        assert_eq!(row.nbk_sha256.as_deref(), Some("sha-b"));
        assert_eq!(row.updated_at.as_deref(), Some("2026-06-09T19:30:00"));
    }

    #[test]
    fn notebook_title_defaults_to_first_import_datetime_and_is_frozen() {
        let conn = fresh_db();
        // First import: the title is the on-device Date Modified, minute-precise.
        upsert_notebook(
            &conn,
            "uuid-t",
            1,
            "sha-a",
            "2026-06-01T10:00:00Z",
            "2026-06-01T09:05:00",
        )
        .expect("first import");
        let row = get_notebook_by_uuid(&conn, "uuid-t").unwrap().unwrap();
        assert_eq!(row.title, "2026-06-01 09:05");

        // `upsert_notebook` with new content and a newer mtime keeps the title.
        upsert_notebook(
            &conn,
            "uuid-t",
            2,
            "sha-b",
            "2026-06-09T20:00:00Z",
            "2026-06-09T19:30:00",
        )
        .expect("re-import");
        let row = get_notebook_by_uuid(&conn, "uuid-t").unwrap().unwrap();
        assert_eq!(
            row.title, "2026-06-01 09:05",
            "title frozen at first import"
        );
        assert_eq!(row.updated_at.as_deref(), Some("2026-06-09T19:30:00"));
    }

    #[test]
    fn notebook_rename_survives_reimport() {
        let conn = fresh_db();
        let id = upsert_notebook(
            &conn,
            "uuid-r",
            1,
            "sha-a",
            "2026-06-01T10:00:00Z",
            "2026-06-01T09:05:00",
        )
        .expect("import");
        rename_notebook(&conn, id, "Meeting notes").expect("rename");
        // `upsert_notebook` keeps a renamed title.
        upsert_notebook(
            &conn,
            "uuid-r",
            2,
            "sha-b",
            "2026-06-09T20:00:00Z",
            "2026-06-09T19:30:00",
        )
        .expect("re-import");
        let row = get_notebook_by_uuid(&conn, "uuid-r").unwrap().unwrap();
        assert_eq!(row.title, "Meeting notes");
    }

    #[test]
    fn notebook_legacy_sentinel_title_upgrades_once_then_freezes() {
        let conn = fresh_db();
        // A pre-feature row: the old literal-'Notebook' default, no updated_at.
        conn.execute(
            "INSERT INTO notebooks (uuid, title, page_count, nbk_sha256, imported_at, updated_at)
             VALUES ('uuid-l', 'Notebook', 1, 'sha-a', '2026-06-01T10:00:00Z', NULL)",
            [],
        )
        .unwrap();
        // upsert (changed-content path) upgrades the sentinel to the datetime…
        upsert_notebook(
            &conn,
            "uuid-l",
            1,
            "sha-a",
            "2026-06-01T10:00:00Z",
            "2026-06-02T08:15:00",
        )
        .expect("re-import");
        let row = get_notebook_by_uuid(&conn, "uuid-l").unwrap().unwrap();
        assert_eq!(row.title, "2026-06-02 08:15");
        // …and it's then frozen like any other default.
        upsert_notebook(
            &conn,
            "uuid-l",
            2,
            "sha-b",
            "2026-06-01T10:00:00Z",
            "2026-06-09T19:30:00",
        )
        .expect("edit");
        let row = get_notebook_by_uuid(&conn, "uuid-l").unwrap().unwrap();
        assert_eq!(row.title, "2026-06-02 08:15", "upgraded title now frozen");
    }

    #[test]
    fn notebook_legacy_sentinel_title_backfills_on_unchanged_path() {
        // `backfill_notebook_default_title` alone, apart from `upsert_notebook`.
        let conn = fresh_db();
        conn.execute(
            "INSERT INTO notebooks (uuid, title, page_count, nbk_sha256, imported_at, updated_at)
             VALUES ('uuid-u', 'Notebook', 1, 'sha-a', '2026-06-01T10:00:00Z', '2026-06-03T07:00:00')",
            [],
        )
        .unwrap();
        backfill_notebook_default_title(&conn, "uuid-u", "2026-06-03T07:00:00").unwrap();
        let row = get_notebook_by_uuid(&conn, "uuid-u").unwrap().unwrap();
        assert_eq!(row.title, "2026-06-03 07:00");
        // `backfill_notebook_default_title` leaves a real title alone.
        rename_notebook(&conn, row.id, "Kept").unwrap();
        backfill_notebook_default_title(&conn, "uuid-u", "2026-06-09T19:30:00").unwrap();
        let row = get_notebook_by_uuid(&conn, "uuid-u").unwrap().unwrap();
        assert_eq!(row.title, "Kept");
    }

    #[test]
    fn default_notebook_title_handles_naive_and_rfc3339() {
        // `2026-06-01T09:05:45` (naive) keeps its digits, seconds dropped, in any timezone.
        assert_eq!(
            default_notebook_title("2026-06-01T09:05:45"),
            "2026-06-01 09:05"
        );
        // RFC 3339 converts to local; the minute depends on the host tz, the shape does not.
        let got = default_notebook_title("2026-06-01T09:05:45Z");
        assert_eq!(got.len(), 16, "YYYY-MM-DD HH:MM");
        assert!(got.starts_with("2026-"));
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
                amazon_asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
                title_romaji: "",
                author_romaji: "",
                source_format: None,
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
    fn deleting_book_destroys_its_annotations_and_presence() {
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
        // `record_device_book_presence` lands a row in `annotation_device`.
        record_device_book_presence(
            &conn,
            "DEV",
            book_id,
            &["k1".to_string()],
            "2026-05-25T00:00:01Z",
        )
        .expect("record presence");

        remove_book(&conn, book_id).expect("remove");

        // `remove_book` destroys the annotation outright, leaving no orphan.
        assert!(
            list_annotations_for_book(&conn, book_id)
                .expect("by book")
                .is_empty()
        );
        assert!(
            list_unlinked_annotations(&conn)
                .expect("orphans")
                .is_empty()
        );
        // ...and its `annotation_device` row with it.
        let presence: i64 = conn
            .query_row("SELECT COUNT(*) FROM annotation_device", [], |r| r.get(0))
            .unwrap();
        assert_eq!(presence, 0);
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
        assert!(
            get_annotation_by_hash(&conn, "nh1-v2")
                .expect("new")
                .is_some()
        );

        // Delete removes it; deleting again is a no-op.
        assert!(delete_annotation(&conn, id).expect("delete"));
        assert!(get_annotation(&conn, id).expect("gone").is_none());
        assert!(!delete_annotation(&conn, id).expect("delete-again"));
        assert!(
            list_annotations_for_book(&conn, book_id)
                .expect("list")
                .is_empty()
        );
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
        assert!(
            list_reading_positions(&conn, book_id)
                .expect("list")
                .is_empty()
        );

        // One `'sidle'` row and one row per device coexist under the composite PK
        // `(book_id, source, device_serial)`.
        set_reading_position(&conn, book_id, Some(200), Some(0), Some(3000), "sidle", "")
            .expect("s");
        set_reading_position(
            &conn,
            book_id,
            Some(100),
            Some(5),
            Some(2000),
            "device",
            "KOA2",
        )
        .expect("a");
        set_reading_position(
            &conn,
            book_id,
            Some(140),
            Some(9),
            Some(2500),
            "device",
            "SCRIBE",
        )
        .expect("b");
        assert_eq!(
            list_reading_positions(&conn, book_id).expect("list").len(),
            3
        );
        assert_eq!(find(&conn, "sidle", "").unwrap().eid, Some(200));
        assert_eq!(
            find(&conn, "device", "KOA2").unwrap().linear_pos,
            Some(2000)
        );
        assert_eq!(
            find(&conn, "device", "SCRIBE").unwrap().linear_pos,
            Some(2500)
        );

        // `set_reading_position` on the same (source, serial) overwrites that row alone…
        set_reading_position(
            &conn,
            book_id,
            Some(160),
            Some(3),
            Some(2800),
            "device",
            "KOA2",
        )
        .expect("a2");
        assert_eq!(
            list_reading_positions(&conn, book_id).expect("list").len(),
            3
        );
        assert_eq!(find(&conn, "device", "KOA2").unwrap().eid, Some(160));
        // …leaving the other device and Sidle untouched.
        assert_eq!(find(&conn, "device", "SCRIBE").unwrap().eid, Some(140));
        assert_eq!(find(&conn, "sidle", "").unwrap().eid, Some(200));
    }

    #[test]
    fn yjr_sync_sha_upserts() {
        let conn = fresh_db();
        let book_id = insert_minimal(&conn, "sha-yjr", "栞本");
        assert!(
            get_yjr_sync_sha(&conn, "DEV1", book_id)
                .expect("get")
                .is_none()
        );

        set_yjr_sync_sha(&conn, "DEV1", book_id, "abc123", "t1").expect("set");
        assert_eq!(
            get_yjr_sync_sha(&conn, "DEV1", book_id)
                .expect("get")
                .as_deref(),
            Some("abc123")
        );

        // A changed `.yjr` overwrites this device's checkpoint (composite-PK upsert).
        set_yjr_sync_sha(&conn, "DEV1", book_id, "def456", "t2").expect("set2");
        assert_eq!(
            get_yjr_sync_sha(&conn, "DEV1", book_id)
                .expect("get")
                .as_deref(),
            Some("def456")
        );

        // `DEV2` keeps its own checkpoint.
        assert!(
            get_yjr_sync_sha(&conn, "DEV2", book_id)
                .expect("get")
                .is_none()
        );
        set_yjr_sync_sha(&conn, "DEV2", book_id, "zzz", "t3").expect("set3");
        assert_eq!(
            get_yjr_sync_sha(&conn, "DEV1", book_id)
                .expect("get")
                .as_deref(),
            Some("def456")
        );
        assert_eq!(
            get_yjr_sync_sha(&conn, "DEV2", book_id)
                .expect("get")
                .as_deref(),
            Some("zzz")
        );
    }

    #[test]
    fn presence_tracks_devices_without_deleting_backup() {
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
            let mut v: Vec<String> = list_annotations_for_book(conn, book_id)
                .unwrap()
                .into_iter()
                .map(|r| r.dedup_hash)
                .collect();
            v.sort();
            v
        };
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let presence = |conn: &Connection, dev: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM annotation_device WHERE device_serial = ?1",
                params![dev],
                |r| r.get(0),
            )
            .unwrap()
        };

        // DEV1 asserts all three; DEV2 also holds h3.
        record_device_book_presence(&conn, "DEV1", book_id, &v(&["h1", "h2", "h3"]), "t1").unwrap();
        record_device_book_presence(&conn, "DEV2", book_id, &v(&["h3"]), "t1b").unwrap();
        assert_eq!(presence(&conn, "DEV1"), 3);

        // h2 dropped on DEV1: its presence row goes, but the BACKUP row stays.
        record_device_book_presence(&conn, "DEV1", book_id, &v(&["h1", "h3"]), "t2").unwrap();
        assert_eq!(
            presence(&conn, "DEV1"),
            2,
            "DEV1 presence mirrors the device"
        );
        assert_eq!(
            hashes(&conn),
            v(&["h1", "h2", "h3"]),
            "no backup row is ever deleted by a device dropping it"
        );

        // Dropped on both devices, with the backup intact.
        record_device_book_presence(&conn, "DEV1", book_id, &v(&[]), "t3").unwrap();
        record_device_book_presence(&conn, "DEV2", book_id, &v(&[]), "t4").unwrap();
        assert_eq!(presence(&conn, "DEV1"), 0);
        assert_eq!(presence(&conn, "DEV2"), 0);
        assert_eq!(hashes(&conn), v(&["h1", "h2", "h3"]), "backup survives");
    }

    #[test]
    fn presence_never_deletes_legacy_or_native() {
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
        // `native` is a Sidle-made annotation. None may be deleted by a sync.
        mk("keep", "yjr");
        mk("gone", "yjr");
        mk("native", "sidle");

        // The device's set holds `keep` alone; `gone` stays.
        record_device_book_presence(&conn, "DEV1", book_id, &["keep".to_string()], "t1").unwrap();

        let mut left: Vec<String> = list_annotations_for_book(&conn, book_id)
            .unwrap()
            .into_iter()
            .map(|r| r.dedup_hash)
            .collect();
        left.sort();
        // All survive — including the device-dropped `gone` and the native row.
        assert_eq!(
            left,
            vec!["gone".to_string(), "keep".to_string(), "native".to_string()]
        );
    }

    /// `migrate` scrubs every `source='clippings'` row on `open()`.
    #[test]
    fn migrate_purges_legacy_clipping_rows() {
        let conn = fresh_db();
        let book_id = insert_minimal(&conn, "sha-mig", "本");
        // A v1-shaped clippings orphan, inserted directly.
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

        // Re-run migrate() directly, on this in-memory connection.
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
            ppd: None,
            writing_mode: None,
            publisher: Some("新潮文庫".into()),
            published_at: Some("2024-03-15".into()),
            series_name: Some("ハルキ三部作".into()),
            series_index: Some(2.5),
            tags: vec!["小説".into(), "ライトノベル".into()],
            title_romaji: String::new(),
            author_romaji: String::new(),
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
                ppd: None,
                writing_mode: None,
                publisher: None,
                published_at: None,
                series_name: Some("Foundation".into()),
                series_index: Some(1.0),
                tags: vec![],
                title_romaji: String::new(),
                author_romaji: String::new(),
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
                ppd: None,
                writing_mode: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: vec![],
                title_romaji: String::new(),
                author_romaji: String::new(),
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
                ppd: None,
                writing_mode: None,
                publisher: Some("講談社文庫".into()),
                published_at: None,
                series_name: None,
                series_index: None,
                tags: vec![],
                title_romaji: String::new(),
                author_romaji: String::new(),
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
                ppd: None,
                writing_mode: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: vec![],
                title_romaji: String::new(),
                author_romaji: String::new(),
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
                ppd: None,
                writing_mode: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: tags.clone(),
                title_romaji: String::new(),
                author_romaji: String::new(),
            },
        )
        .expect("update");

        let row = get_book(&conn, id).expect("get").expect("present");
        assert_eq!(row.tags, tags);
    }

    #[test]
    fn set_amazon_asin_and_uniqueness() {
        let conn = fresh_db();
        let a = insert_minimal(&conn, "sha-asin-a", "A");
        let b = insert_minimal(&conn, "sha-asin-b", "B");

        assert_eq!(
            book_id_with_amazon_asin(&conn, "B07PXGQC1Q", a).expect("query"),
            None
        );

        set_amazon_asin(&conn, a, Some("B07PXGQC1Q")).expect("set asin a");
        assert_eq!(
            get_book(&conn, a).unwrap().unwrap().amazon_asin.as_deref(),
            Some("B07PXGQC1Q")
        );

        // `book_id_with_amazon_asin` finds the collision from book `b`.
        assert_eq!(
            book_id_with_amazon_asin(&conn, "B07PXGQC1Q", b).expect("query"),
            Some(a)
        );
        // ...and excludes `a`, the owner.
        assert_eq!(
            book_id_with_amazon_asin(&conn, "B07PXGQC1Q", a).expect("query"),
            None
        );

        // `asin`, the file's own key, stays untouched.
        assert_eq!(get_book(&conn, a).unwrap().unwrap().asin, None);
        set_amazon_asin(&conn, a, None).expect("clear");
        assert_eq!(get_book(&conn, a).unwrap().unwrap().amazon_asin, None);
    }

    #[test]
    fn the_migration_lifts_a_catalogue_asin_out_of_the_file_key() {
        // A pre-split library: `asin` holds what the export stamped, for a store-bought
        // source the catalogue ASIN.
        let conn = Connection::open_in_memory().expect("open");
        migrate(&conn).expect("migrate");
        conn.execute("ALTER TABLE books DROP COLUMN amazon_asin", [])
            .expect("undo the split");
        let seed = |sha: &str, asin: &str| {
            conn.execute(
                "INSERT INTO books (sha256, title, author, language, file_size,
                     imported_at, asin, tags)
                 VALUES (?1, ?1, '', '', 0, '2026-01-01T00:00:00+00:00', ?2, '[]')",
                params![sha, asin],
            )
            .expect("seed a pre-split row");
            conn.last_insert_rowid()
        };
        let real = seed("sha-real", "B07PXGQC1Q");
        let made_up = seed("sha-made-up", "GPAAHSEAGDCDOFL5OHPUACEIJSCLNRF2");

        migrate(&conn).expect("re-migrate");

        let from_store = get_book(&conn, real).unwrap().unwrap();
        assert_eq!(from_store.amazon_asin.as_deref(), Some("B07PXGQC1Q"));
        assert_eq!(
            from_store.asin.as_deref(),
            Some("B07PXGQC1Q"),
            "the file still carries it until it is re-keyed"
        );

        let converted = get_book(&conn, made_up).unwrap().unwrap();
        assert_eq!(
            converted.amazon_asin, None,
            "a synthesized key is not a catalogue ASIN"
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
                ppd: None,
                writing_mode: None,
                publisher: Some("Spectra".into()),
                published_at: None,
                series_name: None,
                series_index: None,
                tags,
                title_romaji: String::new(),
                author_romaji: String::new(),
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
            add_tags: vec!["sci-fi".into(), "classic".into()], // sci-fi present
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
        assert!(!apply_bulk_patch(&conn, 9999, &BulkMetadataPatch::default()).expect("apply"));
    }

    /// `epub_path` stores relative and reads back absolute, across a `root`
    /// that moves.
    #[test]
    fn paths_stored_relative_resolved_absolute_and_migrated() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `canonicalize`: a macOS tempdir resolves /var → /private/var, as `conn.path()` does.
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
                    amazon_asin: None,
                    publisher: None,
                    published_at: None,
                    series_name: None,
                    series_index: None,
                    tags: &[],
                    title_romaji: "",
                    author_romaji: "",
                    source_format: None,
                },
            )
            .expect("insert");

            // Stored relative…
            assert_eq!(stored(&conn, "epub_path").as_deref(), Some(rel_epub));
            assert_eq!(stored(&conn, "kfx_path").as_deref(), Some(rel_kfx));
            assert_eq!(
                stored(&conn, "cover_path").as_deref(),
                Some("books/abc123/cover.jpg")
            );

            // …resolved to absolute on read.
            let row = get_book(&conn, id).expect("get").expect("present");
            assert_eq!(
                row.epub_path.as_deref(),
                Some(epub_abs.to_string_lossy().as_ref())
            );
            assert_eq!(
                row.kfx_path.as_deref(),
                Some(kfx_abs.to_string_lossy().as_ref())
            );

            // `resolved_kfx` fed back into the setter keeps the column relative.
            let resolved_kfx = row.kfx_path.clone().unwrap();
            set_kfx_path_and_sha(&conn, id, &resolved_kfx, "cafe").expect("set kfx");
            assert_eq!(stored(&conn, "kfx_path").as_deref(), Some(rel_kfx));

            // Simulate an older absolute row via a raw UPDATE (bypasses the setter).
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
        assert_eq!(
            row.epub_path.as_deref(),
            Some(epub_abs.to_string_lossy().as_ref())
        );
    }

    /// `relativize_for_store` cuts a managed path at its deepest `books` or
    /// `notebooks` component, under any `root`.
    #[test]
    fn relativize_handles_foreign_and_legacy_roots() {
        let live = Path::new("/Users/x/Documents/Sidle");

        // Legacy lowercase root, case-mismatched against the live root.
        assert_eq!(
            relativize_for_store(
                Some(live),
                "/Users/x/Library/Application Support/sidle/books/abc/[A] T.epub",
            ),
            "books/abc/[A] T.epub",
        );
        // A wholly different pre-move folder.
        assert_eq!(
            relativize_for_store(Some(live), "/Volumes/Ext/OldLib/books/def/cover.jpg"),
            "books/def/cover.jpg",
        );
        // `notebooks/` paths cut the same way.
        assert_eq!(
            relativize_for_store(Some(live), "/old/root/notebooks/uuid-1/pages/p0.svg"),
            "notebooks/uuid-1/pages/p0.svg",
        );
        // The fast path, under the live root.
        assert_eq!(
            relativize_for_store(Some(live), "/Users/x/Documents/Sidle/books/ghi/x.kfx"),
            "books/ghi/x.kfx",
        );
        // Idempotent on a relative value.
        assert_eq!(
            relativize_for_store(Some(live), "books/abc/x.epub"),
            "books/abc/x.epub"
        );
        // A root that itself contains a `books` component resolves to the deepest
        // (managed) one, not the root's.
        assert_eq!(
            relativize_for_store(
                Some(Path::new("/srv/books/lib")),
                "/elsewhere/books/jkl/x.epub"
            ),
            "books/jkl/x.epub",
        );
        // `relativize_for_store` leaves a path with no managed component untouched.
        assert_eq!(
            relativize_for_store(Some(live), "/Users/x/Pictures/my-cover.png"),
            "/Users/x/Pictures/my-cover.png",
        );
    }

    /// End-to-end: a row whose paths are absolute under an old, case-mismatched root
    /// resolves under the live root after the next `open`.
    #[test]
    fn legacy_absolute_paths_migrate_under_relocated_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        let db_path = root.join("library.db");
        let sha = "b600ad79";

        let stored = |conn: &Connection, col: &str| -> Option<String> {
            conn.query_row(
                &format!("SELECT {col} FROM books WHERE sha256 = ?1"),
                rusqlite::params![sha],
                |r| r.get::<_, Option<String>>(0),
            )
            .expect("query")
        };

        let id = {
            let conn = open(&db_path).expect("open");
            let id = insert_minimal(&conn, sha, "01 文学少女");
            // Stamp in the legacy on-disk state: absolute paths under the
            // lowercase app-support root, which holds none of the files.
            let legacy = "/Users/x/Library/Application Support/sidle/books/b600ad79";
            conn.execute(
                "UPDATE books SET epub_path = ?1, cover_path = ?2, kfx_path = ?3 WHERE id = ?4",
                rusqlite::params![
                    format!("{legacy}/[N] book.epub"),
                    format!("{legacy}/cover.jpg"),
                    format!("{legacy}/[N] book.kfx"),
                    id,
                ],
            )
            .expect("stamp legacy");
            id
        };

        // `open` at the live root relativizes; `get_book` resolves under it.
        let conn = open(&db_path).expect("reopen");
        assert_eq!(
            stored(&conn, "epub_path").as_deref(),
            Some("books/b600ad79/[N] book.epub")
        );
        assert_eq!(
            stored(&conn, "cover_path").as_deref(),
            Some("books/b600ad79/cover.jpg")
        );
        let row = get_book(&conn, id).expect("get").expect("present");
        assert_eq!(
            row.cover_path.as_deref(),
            Some(
                root.join("books/b600ad79/cover.jpg")
                    .to_string_lossy()
                    .as_ref()
            ),
        );
    }

    #[test]
    fn migrate_stamps_schema_version() {
        let conn = fresh_db();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    /// [`repair_colors_read_as_notes`] moves a bare colour string out of `note_body`
    /// and leaves a genuine note alone.
    #[test]
    fn migrate_v12_moves_a_highlight_colour_out_of_the_note_body() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-color", "透明な夜");
        let row = |dedup: &str, eid: i64, note: Option<&str>, color: Option<&str>| {
            insert_annotation(
                &conn,
                &NewAnnotation {
                    dedup_hash: dedup,
                    book_id: Some(book),
                    kind: "highlight",
                    eid_start: Some(eid),
                    off_start: Some(0),
                    eid_end: Some(eid),
                    off_end: Some(5),
                    loc_start: Some(eid),
                    loc_end: Some(eid),
                    linear_pos: Some(eid),
                    text: "窓辺には花が飾られていた。",
                    note_body: note,
                    color,
                    clip_title: None,
                    clip_author: None,
                    added_at: None,
                    added_raw: None,
                    imported_at: "t",
                    source: "yjr",
                },
            )
            .unwrap()
        };
        row("a", 1, Some("pink"), None); // colour mistaken for a note
        row("b", 2, Some("orange"), Some("")); // same, empty-string colour
        row("c", 3, Some("a real thought"), None); // a genuine note
        row("d", 4, Some("blue"), Some("yellow")); // coloured: hands off

        conn.pragma_update(None, "user_version", 11).unwrap();
        migrate(&conn).unwrap();

        let by_eid: std::collections::HashMap<i64, AnnotationRow> =
            list_annotations_for_book(&conn, book)
                .unwrap()
                .into_iter()
                .map(|r| (r.eid_start.unwrap(), r))
                .collect();

        assert_eq!(by_eid[&1].color.as_deref(), Some("pink"));
        assert_eq!(by_eid[&1].note_body, None, "no longer a note");
        assert_eq!(by_eid[&2].color.as_deref(), Some("orange"));
        assert_eq!(by_eid[&2].note_body, None);
        assert_eq!(
            by_eid[&3].note_body.as_deref(),
            Some("a real thought"),
            "a genuine note is untouched",
        );
        assert_eq!(by_eid[&3].color.as_deref().unwrap_or(""), "");
        assert_eq!(
            by_eid[&4].color.as_deref(),
            Some("yellow"),
            "a row that already had a colour keeps it",
        );
        assert_eq!(by_eid[&4].note_body.as_deref(), Some("blue"));
    }

    /// Two `annotations` rows on one passage, differing only in `loc_start`,
    /// `loc_end` and `linear_pos`, become one.
    #[test]
    fn migrate_v12_collapses_annotations_that_differed_only_by_position() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-dup", "Fragments");
        fn same_passage<'a>(
            book: i64,
            dedup: &'a str,
            loc: i64,
            text: &'a str,
        ) -> NewAnnotation<'a> {
            NewAnnotation {
                dedup_hash: dedup,
                book_id: Some(book),
                kind: "highlight",
                eid_start: Some(902),
                off_start: Some(0),
                eid_end: Some(902),
                off_end: Some(104),
                loc_start: Some(loc),
                loc_end: Some(loc),
                linear_pos: Some(loc),
                text,
                note_body: None,
                color: None,
                clip_title: None,
                clip_author: None,
                added_at: None,
                added_raw: None,
                imported_at: "t",
                source: "sidle",
            }
        }
        // Same passage and text, `loc_start` on another scale.
        insert_annotation(
            &conn,
            &same_passage(book, "old-sidle", 25, "a pertinent question"),
        )
        .unwrap();
        insert_annotation(
            &conn,
            &same_passage(book, "old-device", 1586, "a pertinent question"),
        )
        .unwrap();
        // A different passage, to prove the collapse is scoped.
        let mut other = same_passage(book, "old-other", 99, "elsewhere");
        other.eid_start = Some(1073);
        insert_annotation(&conn, &other).unwrap();

        // Presence on a Kindle, recorded against the doomed duplicate's hash.
        record_device_book_presence(&conn, "SERIAL1", book, &["old-device".to_string()], "t")
            .unwrap();
        // A Sidle-side deletion of the other passage, keyed by its old hash.
        record_deletion(&conn, DELETION_ANNOTATION, "old-other").unwrap();

        conn.pragma_update(None, "user_version", 11).unwrap();
        migrate(&conn).unwrap();

        let rows = list_annotations_for_book(&conn, book).unwrap();
        assert_eq!(rows.len(), 2, "the duplicate pair collapsed to one row");
        let kept = rows.iter().find(|r| r.eid_start == Some(902)).unwrap();
        assert_eq!(kept.text, "a pertinent question");
        assert_eq!(
            kept.loc_start,
            Some(25),
            "the earliest row survives, keeping the original capture",
        );
        assert_ne!(kept.dedup_hash, "old-sidle", "re-keyed under the new rule");

        // `annotation_device` followed the survivor.
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM annotation_device WHERE dedup_hash = ?1",
                params![kept.dedup_hash],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "presence re-keyed onto the survivor");

        // `artifact_deletions` names the passage it was written for.
        let other_row = rows.iter().find(|r| r.eid_start == Some(1073)).unwrap();
        assert!(
            is_deleted(&conn, DELETION_ANNOTATION, &other_row.dedup_hash).unwrap(),
            "tombstone re-keyed with its annotation",
        );
        assert!(!is_deleted(&conn, DELETION_ANNOTATION, "old-other").unwrap());

        // Idempotent: a second open finds the version stamped and the rows
        // keyed, and changes nothing.
        let before: Vec<String> = rows.iter().map(|r| r.dedup_hash.clone()).collect();
        migrate(&conn).unwrap();
        let after: Vec<String> = list_annotations_for_book(&conn, book)
            .unwrap()
            .iter()
            .map(|r| r.dedup_hash.clone())
            .collect();
        assert_eq!(after, before);
    }

    /// A sync landing before the re-key: the new rule's hash on a row newer than the
    /// one it collapses into.
    #[test]
    fn migrate_v12_rekeys_a_survivor_whose_hash_a_later_duplicate_already_holds() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-mid", "Fragments");
        fn passage<'a>(book: i64, dedup: &'a str, loc: i64) -> NewAnnotation<'a> {
            NewAnnotation {
                dedup_hash: dedup,
                book_id: Some(book),
                kind: "highlight",
                eid_start: Some(902),
                off_start: Some(0),
                eid_end: Some(902),
                off_end: Some(104),
                loc_start: Some(loc),
                loc_end: Some(loc),
                linear_pos: Some(loc),
                text: "a pertinent question",
                note_body: None,
                color: None,
                clip_title: None,
                clip_author: None,
                added_at: None,
                added_raw: None,
                imported_at: "t",
                source: "sidle",
            }
        }
        // `canonical`: the hash the import path computes, which the re-key lands on.
        let canonical = crate::library::ingest::annotation_dedup_hash(
            &crate::library::ingest::book_match_key("Fragments"),
            "highlight",
            Some(902),
            Some(0),
            Some(902),
            Some(104),
            "a pertinent question",
            "",
        );
        // Captured under the old rule, then re-inserted by a sync running the new
        // code against a library not yet re-keyed.
        insert_annotation(&conn, &passage(book, "old-rule", 25)).unwrap();
        insert_annotation(&conn, &passage(book, &canonical, 1586)).unwrap();
        record_device_book_presence(
            &conn,
            "SERIAL1",
            book,
            std::slice::from_ref(&canonical),
            "t",
        )
        .unwrap();

        conn.pragma_update(None, "user_version", 11).unwrap();
        migrate(&conn).expect("re-key must not collide with the row it absorbs");

        let rows = list_annotations_for_book(&conn, book).unwrap();
        assert_eq!(rows.len(), 1, "the pair collapsed");
        assert_eq!(rows[0].dedup_hash, canonical);
        assert_eq!(
            rows[0].loc_start,
            Some(25),
            "the earliest row survives, keeping the original capture",
        );
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM annotation_device WHERE dedup_hash = ?1",
                params![canonical],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "the device's presence record survives intact");
    }

    /// A `kind` `note` row carrying a `note_body` over the same span as a
    /// device `highlight` becomes the two rows, keeping the device's.
    #[test]
    fn migrate_v13_splits_a_fused_note_and_absorbs_the_devices_highlight() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-note", "Perfect Insider");
        let key = crate::library::ingest::book_match_key("Perfect Insider");
        let hash = |kind: &str, body: &str| {
            crate::library::ingest::annotation_dedup_hash(
                &key,
                kind,
                Some(918),
                Some(311),
                Some(918),
                Some(327),
                "この余剰が",
                body,
            )
        };
        fn span<'a>(
            book: i64,
            dedup: &'a str,
            kind: &'a str,
            body: Option<&'a str>,
            source: &'a str,
        ) -> NewAnnotation<'a> {
            NewAnnotation {
                dedup_hash: dedup,
                book_id: Some(book),
                kind,
                eid_start: Some(918),
                off_start: Some(311),
                eid_end: Some(918),
                off_end: Some(327),
                loc_start: Some(527),
                loc_end: Some(543),
                linear_pos: Some(527),
                text: "この余剰が",
                note_body: body,
                color: Some("orange"),
                clip_title: None,
                clip_author: None,
                added_at: None,
                added_raw: None,
                imported_at: "t",
                source,
            }
        }
        // `fused`: the row a pre-split reader wrote, with the hash a v12 DB holds for it.
        let fused = hash("note", "test on sidle");
        insert_annotation(
            &conn,
            &span(book, &fused, "note", Some("test on sidle"), "sidle"),
        )
        .unwrap();
        // ...and the device's highlight record for the same passage, as its own row.
        insert_annotation(
            &conn,
            &span(book, &hash("highlight", ""), "highlight", None, "yjr"),
        )
        .unwrap();

        conn.pragma_update(None, "user_version", 12).unwrap();
        migrate(&conn).unwrap();

        let rows = list_annotations_for_book(&conn, book).unwrap();
        assert_eq!(rows.len(), 2, "one highlight and one note, not three rows");
        let hl = rows.iter().find(|r| r.kind == "highlight").unwrap();
        let note = rows.iter().find(|r| r.kind == "note").unwrap();
        assert_eq!(hl.dedup_hash, hash("highlight", ""), "matches the device");
        assert_eq!(hl.note_body, None, "the body moved off the highlight");
        assert_eq!(
            note.dedup_hash, fused,
            "the note keeps its identity — nothing referring to it breaks",
        );
        assert_eq!(note.note_body.as_deref(), Some("test on sidle"));
        assert_eq!(
            hl.color.as_deref(),
            Some("orange"),
            "the colour describes the passage, so it moves to the highlight",
        );
        assert_eq!(note.color, None, "and off the note");
        assert_eq!(
            (note.eid_start, note.off_start, note.eid_end, note.off_end),
            (hl.eid_start, hl.off_start, hl.eid_end, hl.off_end),
            "the note keeps the highlight's span, which is what a device accepts",
        );
        // `notes::attachments` groups them.
        assert_eq!(
            crate::library::notes::attachments(&rows),
            vec![(note.id, hl.id)],
        );

        // Idempotent: re-running finds the work done and changes nothing.
        migrate(&conn).unwrap();
        assert_eq!(list_annotations_for_book(&conn, book).unwrap().len(), 2);
    }

    /// `migrate` (v13) with no device `highlight` present mints one and keeps the `note`.
    #[test]
    fn migrate_v13_mints_the_missing_highlight_without_losing_the_note() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-lone", "Perfect Insider");
        insert_annotation(
            &conn,
            &NewAnnotation {
                dedup_hash: "fused-lone",
                book_id: Some(book),
                kind: "note",
                eid_start: Some(918),
                off_start: Some(311),
                eid_end: Some(918),
                off_end: Some(327),
                loc_start: Some(527),
                loc_end: Some(543),
                linear_pos: Some(527),
                text: "この余剰が",
                note_body: Some("test on sidle"),
                color: Some("orange"),
                clip_title: None,
                clip_author: None,
                added_at: None,
                added_raw: None,
                imported_at: "t",
                source: "sidle",
            },
        )
        .unwrap();

        conn.pragma_update(None, "user_version", 12).unwrap();
        migrate(&conn).unwrap();

        let rows = list_annotations_for_book(&conn, book).unwrap();
        assert_eq!(rows.len(), 2, "the highlight was minted alongside the note");
        let note = rows.iter().find(|r| r.kind == "note").unwrap();
        let hl = rows.iter().find(|r| r.kind == "highlight").unwrap();
        assert_eq!(
            note.note_body.as_deref(),
            Some("test on sidle"),
            "the note body must survive the split",
        );
        assert_eq!(hl.color.as_deref(), Some("orange"));
        assert_eq!(hl.text, note.text, "same passage");
        assert_eq!(
            crate::library::notes::attachments(&rows),
            vec![(note.id, hl.id)]
        );
    }

    /// A `note` row a Kindle wrote is a real note record, and splitting one
    /// invents a highlight the device never had.
    #[test]
    fn migrate_v13_leaves_device_written_notes_alone() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-dev-note", "Perfect Insider");
        // `device_note`: the hash a v12 DB holds for this record.
        let device_note = crate::library::ingest::annotation_dedup_hash(
            &crate::library::ingest::book_match_key("Perfect Insider"),
            "note",
            Some(918),
            Some(327),
            Some(918),
            Some(327),
            "",
            "Test",
        );
        insert_annotation(
            &conn,
            &NewAnnotation {
                dedup_hash: &device_note,
                book_id: Some(book),
                kind: "note",
                eid_start: Some(918),
                off_start: Some(327),
                eid_end: Some(918),
                off_end: Some(327),
                loc_start: Some(543),
                loc_end: Some(543),
                linear_pos: Some(543),
                text: "",
                note_body: Some("Test"),
                color: None,
                clip_title: None,
                clip_author: None,
                added_at: None,
                added_raw: None,
                imported_at: "t",
                source: "yjr",
            },
        )
        .unwrap();

        conn.pragma_update(None, "user_version", 12).unwrap();
        migrate(&conn).unwrap();

        let rows = list_annotations_for_book(&conn, book).unwrap();
        assert_eq!(rows.len(), 1, "no highlight invented");
        assert_eq!(rows[0].kind, "note");
        assert_eq!(rows[0].dedup_hash, device_note, "identity untouched");
    }

    // ── book_ink (handwritten ink on a sideloaded doc) ──────────────────────

    fn insert_with_asin(conn: &Connection, sha: &str, title: &str, asin: &str) -> i64 {
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
                imported_at: "t0",
                asin: Some(asin),
                amazon_asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
                title_romaji: "",
                author_romaji: "",
                source_format: None,
            },
        )
        .expect("insert")
    }

    #[test]
    fn book_ink_upsert_is_idempotent_on_asin_container() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha", "B");
        let mk = |nbk: &'static str| NewBookInk {
            book_id: Some(book),
            asin: "AS1",
            container_id: "cidA",
            host_page: Some(8),
            host_eid: Some(1158),
            host_linear: Some(9782),
            nbk_sha256: Some(nbk),
            imported_at: "t0",
        };
        let id1 = upsert_book_ink(&conn, &mk("sha1")).unwrap();
        // Re-import the same page from a grown nbk (new whole-file sha): same row,
        // refreshed content hash — NOT a duplicate.
        let id2 = upsert_book_ink(&conn, &mk("sha2")).unwrap();
        assert_eq!(id1, id2);
        let rows = list_book_ink(&conn, book).unwrap();
        assert_eq!(rows.len(), 1, "same (asin, container) → one row");
        assert_eq!(rows[0].nbk_sha256.as_deref(), Some("sha2"));
    }

    #[test]
    fn book_ink_lists_in_linear_order_with_distinct_host_pages() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha", "B");
        // Insert OUT of reading order; display must sort by the device linear pos.
        for (cid, page, lin) in [("c2", 2, 17274), ("c0", 8, 9782), ("c1", 1, 14938)] {
            upsert_book_ink(
                &conn,
                &NewBookInk {
                    book_id: Some(book),
                    asin: "AS1",
                    container_id: cid,
                    host_page: Some(page),
                    host_eid: Some(page),
                    host_linear: Some(lin),
                    nbk_sha256: Some("s"),
                    imported_at: "t0",
                },
            )
            .unwrap();
        }
        let order: Vec<String> = list_book_ink(&conn, book)
            .unwrap()
            .into_iter()
            .map(|r| r.container_id)
            .collect();
        assert_eq!(
            order,
            ["c0", "c1", "c2"],
            "sorted by host_linear, not insert order"
        );
        assert_eq!(book_ink_host_pages(&conn, book).unwrap(), vec![1, 2, 8]);
        let on8 = list_book_ink_on_page(&conn, book, 8).unwrap();
        assert_eq!(on8.len(), 1);
        assert_eq!(on8[0].container_id, "c0");
    }

    #[test]
    fn book_ink_presence_tracks_device_without_deleting_backup() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha", "B");
        let mk = |cid: &'static str| NewBookInk {
            book_id: Some(book),
            asin: "AS1",
            container_id: cid,
            host_page: Some(1),
            host_eid: Some(1),
            host_linear: Some(1),
            nbk_sha256: Some("s"),
            imported_at: "t0",
        };
        upsert_book_ink(&conn, &mk("c0")).unwrap();
        upsert_book_ink(&conn, &mk("c1")).unwrap();

        // First sync: `DEV` asserts both pages.
        record_ink_device_presence(
            &conn,
            "DEV",
            "AS1",
            Some(book),
            &["c0".to_string(), "c1".to_string()],
            "t1",
        )
        .unwrap();
        assert_eq!(list_book_ink(&conn, book).unwrap().len(), 2);

        // Next sync: c1 erased on `DEV`; its presence row goes, its `book_ink` row stays.
        record_ink_device_presence(&conn, "DEV", "AS1", Some(book), &["c0".to_string()], "t2")
            .unwrap();
        assert_eq!(
            list_book_ink(&conn, book).unwrap().len(),
            2,
            "an erased-on-device page keeps its backup"
        );
        let presence: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM book_ink_device WHERE device_serial = 'DEV'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(presence, 1, "presence mirrors the device (only c0 left)");
    }

    #[test]
    fn delete_book_ink_records_tombstone() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha", "B");
        upsert_book_ink(
            &conn,
            &NewBookInk {
                book_id: Some(book),
                asin: "AS1",
                container_id: "c0",
                host_page: Some(1),
                host_eid: Some(1),
                host_linear: Some(1),
                nbk_sha256: Some("s"),
                imported_at: "t0",
            },
        )
        .unwrap();
        let id = list_book_ink(&conn, book).unwrap()[0].id;

        let key = delete_book_ink(&conn, id).unwrap();
        assert_eq!(key, Some(("AS1".to_string(), "c0".to_string())));
        assert!(list_book_ink(&conn, book).unwrap().is_empty());
        assert!(
            is_deleted(&conn, DELETION_INK, &ink_deletion_key("AS1", "c0")).unwrap(),
            "deleting an ink page tombstones it"
        );
    }

    #[test]
    fn hidden_ink_is_excluded_from_painting_but_kept_in_backup() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha", "B");
        upsert_book_ink(
            &conn,
            &NewBookInk {
                book_id: Some(book),
                asin: "AS1",
                container_id: "c0",
                host_page: Some(2),
                host_eid: Some(1),
                host_linear: Some(1),
                nbk_sha256: Some("s"),
                imported_at: "t0",
            },
        )
        .unwrap();
        let id = list_book_ink(&conn, book).unwrap()[0].id;
        // `book_ink_host_pages` and `list_book_ink_on_page` both carry it.
        assert_eq!(book_ink_host_pages(&conn, book).unwrap(), vec![2]);
        assert_eq!(list_book_ink_on_page(&conn, book, 2).unwrap().len(), 1);

        set_book_ink_hidden(&conn, id, true).unwrap();
        assert!(
            book_ink_host_pages(&conn, book).unwrap().is_empty(),
            "hidden ink not painted"
        );
        assert!(list_book_ink_on_page(&conn, book, 2).unwrap().is_empty());
        let rows = list_book_ink(&conn, book).unwrap();
        assert_eq!(rows.len(), 1, "hidden ink stays in the backup list");
        assert!(rows[0].hidden, "flag reflects hidden state");

        set_book_ink_hidden(&conn, id, false).unwrap();
        assert_eq!(
            book_ink_host_pages(&conn, book).unwrap(),
            vec![2],
            "unhide repaints"
        );
    }

    #[test]
    fn hidden_annotation_stays_listed_with_flag() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-h", "本");
        insert_annotation(
            &conn,
            &NewAnnotation {
                dedup_hash: "h1",
                book_id: Some(book),
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
                source: "yjr",
            },
        )
        .unwrap();
        let id = list_annotations_for_book(&conn, book).unwrap()[0].id;
        assert!(!list_annotations_for_book(&conn, book).unwrap()[0].hidden);

        set_annotation_hidden(&conn, id, true).unwrap();
        let rows = list_annotations_for_book(&conn, book).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "a hidden annotation stays in the backup list"
        );
        assert!(rows[0].hidden, "flag reflects hidden state");
    }

    #[test]
    fn remove_notebook_records_tombstone_and_clear_undoes_it() {
        let conn = fresh_db();
        let id = upsert_notebook(&conn, "uuid-1", 3, "sha", "t0", "t0").unwrap();
        let uuid = remove_notebook(&conn, id).unwrap();
        assert_eq!(uuid.as_deref(), Some("uuid-1"));
        assert!(is_deleted(&conn, DELETION_NOTEBOOK, "uuid-1").unwrap());

        clear_deletion(&conn, DELETION_NOTEBOOK, "uuid-1").unwrap();
        assert!(!is_deleted(&conn, DELETION_NOTEBOOK, "uuid-1").unwrap());
    }

    #[test]
    fn deleting_book_destroys_its_ink_presence_and_checkpoint() {
        let conn = fresh_db();
        let book = insert_with_asin(&conn, "sha", "Linear", "AS1");
        upsert_book_ink(
            &conn,
            &NewBookInk {
                book_id: Some(book),
                asin: "AS1",
                container_id: "c0",
                host_page: Some(1),
                host_eid: Some(1),
                host_linear: Some(1),
                nbk_sha256: Some("s"),
                imported_at: "t0",
            },
        )
        .unwrap();
        // `DEV` asserts the page and records a decode checkpoint.
        record_ink_device_presence(&conn, "DEV", "AS1", Some(book), &["c0".to_string()], "t1")
            .unwrap();
        set_ink_sync_sha(&conn, "DEV", "AS1", "nbksha", "t1").unwrap();

        remove_book(&conn, book).unwrap();

        // `remove_book` destroys the ink, its per-device presence and its checkpoint.
        assert!(list_book_ink(&conn, book).unwrap().is_empty());
        assert!(list_unlinked_book_ink(&conn).unwrap().is_empty());
        let presence: i64 = conn
            .query_row("SELECT COUNT(*) FROM book_ink_device", [], |r| r.get(0))
            .unwrap();
        assert_eq!(presence, 0);
        assert_eq!(get_ink_sync_sha(&conn, "DEV", "AS1").unwrap(), None);
    }

    #[test]
    fn vacuum_runs_after_removal_and_leaves_db_usable() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-v", "本");
        remove_book(&conn, book).expect("remove");
        vacuum(&conn).expect("vacuum");
        // `list_books` works after `vacuum`.
        assert!(list_books(&conn).expect("list").is_empty());
    }

    #[test]
    fn ink_sync_checkpoint_round_trips() {
        let conn = fresh_db();
        assert_eq!(get_ink_sync_sha(&conn, "DEV", "AS1").unwrap(), None);
        set_ink_sync_sha(&conn, "DEV", "AS1", "nbksha", "t0").unwrap();
        assert_eq!(
            get_ink_sync_sha(&conn, "DEV", "AS1").unwrap().as_deref(),
            Some("nbksha")
        );
        // Upsert overwrites in place.
        set_ink_sync_sha(&conn, "DEV", "AS1", "nbksha2", "t1").unwrap();
        assert_eq!(
            get_ink_sync_sha(&conn, "DEV", "AS1").unwrap().as_deref(),
            Some("nbksha2")
        );
    }

    #[test]
    fn relink_ink_carries_every_table_to_the_new_key() {
        // A re-key changes the `<asin>` of the device's `.notebooks/<asin>!!PDOC!!` dir;
        // `relink_ink` carries the ink across.
        let conn = fresh_db();
        let book = insert_with_asin(&conn, "sha-ink", "Has Ink", "B07PXGQC1Q");
        upsert_book_ink(
            &conn,
            &NewBookInk {
                book_id: Some(book),
                asin: "B07PXGQC1Q",
                container_id: "c1",
                host_page: Some(3),
                host_eid: None,
                host_linear: None,
                nbk_sha256: Some("nbk1"),
                imported_at: "t0",
            },
        )
        .expect("ink");
        set_ink_sync_sha(&conn, "DEV", "B07PXGQC1Q", "nbk1", "t0").expect("checkpoint");

        relink_ink(&conn, "B07PXGQC1Q", "NEWKEYNEWKEYNEWKEYNEWKEYNEWKEY12").expect("relink");

        let ink = list_book_ink(&conn, book).expect("read ink");
        assert_eq!(ink.len(), 1);
        assert_eq!(ink[0].asin, "NEWKEYNEWKEYNEWKEYNEWKEYNEWKEY12");
        assert_eq!(
            get_ink_sync_sha(&conn, "DEV", "NEWKEYNEWKEYNEWKEYNEWKEYNEWKEY12")
                .unwrap()
                .as_deref(),
            Some("nbk1"),
            "the per-device checkpoint moved with it"
        );
        assert_eq!(
            get_ink_sync_sha(&conn, "DEV", "B07PXGQC1Q").unwrap(),
            None,
            "and nothing is left under the old key"
        );

        // `relink_ink` with an empty old key leaves everything where it is.
        relink_ink(&conn, "", "IRRELEVANT").expect("empty old key");
        assert_eq!(list_book_ink(&conn, book).unwrap().len(), 1);
    }

    #[test]
    fn v26_removes_the_reading_log_tables_columns_and_archive() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("library.db");
        let tables = [
            "reading_sessions",
            "reading_session_hours",
            "reading_log_dumps",
            "reading_log_book_ends",
            "reading_log_asins",
            "reading_log_points",
        ];
        {
            let conn = open(&db_path).unwrap();
            for table in tables {
                conn.execute(&format!("CREATE TABLE {table} (id INTEGER)"), [])
                    .unwrap();
            }
            conn.execute_batch(
                "ALTER TABLE books ADD COLUMN finished_at TEXT;
                 ALTER TABLE books ADD COLUMN max_position INTEGER;
                 PRAGMA user_version = 25;",
            )
            .unwrap();
        }
        let archive = dir.path().join("reading-log").join("DEV");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::write(archive.join("rl_20260801.txt.gz"), b"x").unwrap();
        std::fs::write(dir.path().join(".reading-pulse.json"), b"{}").unwrap();

        let conn = open(&db_path).unwrap();
        for table in tables {
            assert!(!has_table(&conn, table).unwrap(), "{table} dropped");
        }
        assert!(!has_column(&conn, "books", "finished_at").unwrap());
        assert!(!has_column(&conn, "books", "max_position").unwrap());
        assert!(!dir.path().join("reading-log").exists());
        assert!(!dir.path().join(".reading-pulse.json").exists());
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }
}
