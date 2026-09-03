//! rusqlite-backed library database. `AppState` holds one `Connection` behind
//! an `Arc<Mutex>`, and `sidle-server` opens its own per request; WAL and
//! `busy_timeout` (see [`open`]) carry both.

use std::collections::BTreeMap;
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
    /// OPF `<meta name="primary-writing-mode">`: `horizontal-lr`,
    /// `horizontal-rl`, `vertical-rl`, `vertical-lr`, or `None` for derive. It
    /// bakes into `document_data.writing_mode`, with `ppd` its derived mirror.
    pub writing_mode: Option<String>,
    /// Path to the EPUB on disk, `None` until a KFX-imported book's background
    /// conversion lands.
    pub epub_path: Option<String>,
    pub cover_path: Option<String>,
    /// **Derived, not a column.** The thumbnail sidecar at
    /// `books/<sha>/cover.thumb.jpg` (see [`super::thumbnail`]), which the
    /// gallery prefers over `cover_path`. A `None` falls back to `cover_path`.
    pub cover_thumb_path: Option<String>,
    /// **Derived, not a column.** Cache-bust token: the ms mtime of the image
    /// served, or 0 with no cover. The gallery appends it as `?v=`, and the
    /// picker folds it into its on-device cover-cache filename.
    pub cover_rev: i64,
    /// Path to the KFX on disk, `None` until an EPUB-imported book's background
    /// conversion lands.
    pub kfx_path: Option<String>,
    /// SHA-256 of the KFX file's bytes, apart from `sha256`, which hashes the
    /// imported source. The on-device filename infix derives from this one.
    /// `Some(_)` iff `kfx_path` is `Some(_)`.
    pub kfx_sha256: Option<String>,
    /// Path to the PDF on disk, for a PDF-backed (container) book. This is the
    /// non-KFX side of a PDF↔KFX book (the EPUB↔KFX analogue of `epub_path`);
    /// `None` for reflowable (EPUB↔KFX) books.
    pub pdf_path: Option<String>,
    pub file_size: i64,
    pub imported_at: String,
    pub status: String,
    pub error: Option<String>,
    /// Direction of the background conversion job: `"epub_to_kfx"` or
    /// `"kfx_to_epub"`. `None` on a row without a job, which `LEFT JOIN` makes
    /// representable.
    pub kind: Option<String>,
    /// The identifier baked into the exported KFX, keying the Kindle's catalog,
    /// `.sdr` directory and `.notebooks/<id>!!PDOC!!` dir.
    /// `resolve_export_asin` synthesizes it from the publication identifier.
    pub asin: Option<String>,
    /// The real Amazon catalogue ASIN, fetching the colour cover from
    /// `/images/P/<asin>` where the KFX ships a grayscale one. It reaches no
    /// file this crate produces; see [`Self::asin`].
    pub amazon_asin: Option<String>,
    /// The format that arrived at import (`azw3`, `mobi`, `epub`, …).
    /// `conversion_jobs.kind` names a reconvert's direction, not this.
    pub source_format: Option<String>,
    /// Publisher imprint — pulled from EPUB `<dc:publisher>` or KFX
    /// metadata field `publisher` (symbol 232). Optional; many self-pub or
    /// indie books have no publisher. Editable via the metadata modal.
    pub publisher: Option<String>,
    /// Publication date as the source states it (EPUB `<dc:date>` or the KFX
    /// equivalent), stored verbatim. Sort runs on the string, which orders ISO
    /// 8601 chronologically.
    pub published_at: Option<String>,
    pub series_name: Option<String>,
    /// Position within the series. REAL so half-numbers (1.5, 2.5) common
    /// in fiction series numbering work without coercion.
    pub series_index: Option<f64>,
    /// User-defined tags. Stored as a JSON array TEXT in SQLite; canonicalized
    /// (trimmed, lowercased, deduped in-order, empties dropped) at write time.
    pub tags: Vec<String>,
    /// Metadata last-edit time (ISO 8601), stamped at insert and bumped by the
    /// curation mutators. Library merge takes it as the newest-wins tiebreak,
    /// reading `COALESCE(updated_at, imported_at)`.
    pub updated_at: String,
    /// Human-readable romaji of the title, rendered yomigana-aware at import and
    /// correctable by hand. [`Self::search_key`] folds it in, and
    /// `COALESCE(…, '')` reads a NULL row as `""`.
    pub title_romaji: String,
    /// Human-readable romaji of the author line. See [`Self::title_romaji`].
    pub author_romaji: String,
    /// **Derived, not a column.** The space-free, ASCII-folded match key the
    /// picker substring-searches, assembled in [`row_to_book`] from the romaji
    /// columns, the raw fields and [`super::romaji::search_key`].
    pub search_key: String,
}

/// Schema version stamped into `PRAGMA user_version` by [`migrate`]. Backups
/// record it, and restore refuses an archive stamped past the running app.
pub const SCHEMA_VERSION: i64 = 25;

/// A borrowable handle to the library database. A device sync borrows it a
/// moment at a time between USB transfers, leaving the window's share of the
/// desktop's one connection free.
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
    // WAL shares this file between the `sidle-server` daemon and the desktop
    // app. `busy_timeout` holds a second writer at the lock past SQLITE_BUSY,
    // and every write is idempotent under UNIQUE dedup_hash.
    conn.pragma_update(None, "busy_timeout", 5000)?;
    migrate(&conn)?;
    relativize_existing_paths(&conn, path)?;
    Ok(conn)
}

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

/// Relativize a managed path for storage: strip the `root` prefix, else slice
/// from the last `books`/`notebooks` component, which every managed file
/// carries. A foreign path and a `None` root store unchanged.
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
/// absolute value. It lives in `open()`, which holds the db path and its root,
/// where `migrate()` takes only the `Connection`.
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

/// Schema setup. An artefact of a prior schema (`source_epub_path`, the
/// `device_history` table) drops the lot for a rebuild from the CREATE block
/// below, the one source of truth.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    // The version this DB arrives at. The steps below are idempotent, apart
    // from the few rewriting rows in place, which gate on this.
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
        // A row whose `asin` carries a real Amazon shape — 10 uppercase
        // alphanumerics — holds the library's one colour-cover key. The file
        // carries it until a re-key, and `asin` stays untouched here.
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
    // `finished_at` is when a book was marked read. NULL is unmarked.
    if !has_column(conn, "books", "finished_at")? {
        conn.execute("ALTER TABLE books ADD COLUMN finished_at TEXT", [])?;
    }
    // Reading layout / writing mode, the axis the generated KFX bakes into
    // `document_data.writing_mode`, with `ppd` mirroring its page-turn. NULL
    // derives it.
    if !has_column(conn, "books", "writing_mode")? {
        conn.execute("ALTER TABLE books ADD COLUMN writing_mode TEXT", [])?;
    }
    // The exclusive end of the book's position axis, cached against a whole-KFX
    // parse. NULL marks it uncomputed, for [`books_missing_max_position`] to
    // fill. A device reports the last valid position, one less than this.
    if !has_column(conn, "books", "max_position")? {
        conn.execute("ALTER TABLE books ADD COLUMN max_position INTEGER", [])?;
    }
    // Metadata last-edit time, seeded from `imported_at`. Newest-wins reads
    // COALESCE it, and `insert_book` stamps a new row.
    if !has_column(conn, "books", "updated_at")? {
        conn.execute("ALTER TABLE books ADD COLUMN updated_at TEXT", [])?;
        conn.execute(
            "UPDATE books SET updated_at = imported_at WHERE updated_at IS NULL",
            [],
        )?;
    }

    // The file that came in. `conversion_jobs.kind` names the direction a
    // reconvert runs, which for an `.azw3` or `.mobi` import is `epub_to_kfx`
    // over the EPUB the import exported — the arriving format survives here.
    if !has_column(conn, "books", "source_format")? {
        conn.execute("ALTER TABLE books ADD COLUMN source_format TEXT", [])?;
        // The two directions whose arriving format the job kind states. An
        // `epub_to_kfx` row stays NULL, its EPUB being the arriving file or an
        // export of one.
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

    // `yjr_sync` keys the last imported `.yjr` by (device_serial, book_id),
    // holding two devices' markers apart. A table keyed by book_id alone is a
    // cache to drop and recreate.
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

    // `reading_position` is keyed `(book_id, source, device_serial)`, holding
    // one `source='sidle'` position beside one per Kindle. Each is a Resume
    // jump target, and the destructive reset leaves the table alone.
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

    // v5: the notebook's on-device "Date Modified" (source `nbk` mtime, captured
    // at import). Nullable — rows imported before v5 backfill on next re-import.
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

    // Which devices assert each ink page — the ink analogue of
    // `annotation_device`, provenance only. A page erased on the Scribe drops
    // its presence row and keeps its `book_ink` backup row.
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

    // Per-(device, asin) content checkpoint: the sha of the nbk last decoded,
    // short-circuiting an unchanged decode and raster re-render. Row identity
    // stays `(asin, container_id)`, whose upsert holds old pages stable.
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

    // v9: reversible "hide from the reader" flag on annotations + ink (kept in the
    // backup, just not painted / not listed by default). Additive columns, default
    // 0. Both tables exist by here (created above), so the ALTERs are safe.
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

    // Deletion records. The additive device sync skips a recorded key, holding a
    // manual delete. `key` is the annotation `dedup_hash`, the ink
    // `asin\x1fcontainer_id`, or the notebook `uuid`.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS artifact_deletions (
            kind       TEXT NOT NULL,
            key        TEXT NOT NULL,
            deleted_at TEXT NOT NULL,
            PRIMARY KEY (kind, key)
        )"#,
        [],
    )?;

    // Reading sessions recovered from a Kindle's own system logs.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS reading_sessions (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            device_serial TEXT NOT NULL DEFAULT '',
            started_at    TEXT NOT NULL,
            ended_at      TEXT NOT NULL,
            day           TEXT NOT NULL,
            end_position  INTEGER NOT NULL,
            book_id       INTEGER REFERENCES books(id) ON DELETE SET NULL,
            seconds       INTEGER NOT NULL,
            page_turns    INTEGER NOT NULL DEFAULT 0,
            words         INTEGER NOT NULL DEFAULT 0,
            start_counter_ms INTEGER,
            end_counter_ms   INTEGER,
            start_words      INTEGER,
            end_words        INTEGER,
            measure          TEXT NOT NULL DEFAULT 'counted',
            tz_offset_s      INTEGER
        )"#,
        [],
    )?;
    // The count of forward page events the device logged, a function of font
    // size and screen.
    if has_column(conn, "reading_sessions", "pages")? {
        conn.execute(
            "ALTER TABLE reading_sessions RENAME COLUMN pages TO page_turns",
            [],
        )?;
    }
    // Both ends of the counters the row's totals are the difference of,
    // carrying a sitting across the sync that interrupted it. Null on a row
    // stored before the columns.
    for column in [
        "start_counter_ms",
        "end_counter_ms",
        "start_words",
        "end_words",
    ] {
        if !has_column(conn, "reading_sessions", column)? {
            conn.execute(
                &format!("ALTER TABLE reading_sessions ADD COLUMN {column} INTEGER"),
                [],
            )?;
        }
    }
    // Which of three regimes produced `seconds`: `counted` from the device's
    // `TotalTime`, `dwell` from the reader shell's page records, `awake` from
    // the power records. The row names it, and they sum apart.
    if !has_column(conn, "reading_sessions", "measure")? {
        conn.execute(
            "ALTER TABLE reading_sessions ADD COLUMN measure TEXT NOT NULL DEFAULT 'counted'",
            [],
        )?;
    }
    // v24: seconds the reader's clock stood ahead of UTC, where a reader-shell
    // record stated an instant the syslog prefix also stated. Null on every row
    // from a firmware writing no such record, and on every row stored before.
    if !has_column(conn, "reading_sessions", "tz_offset_s")? {
        conn.execute(
            "ALTER TABLE reading_sessions ADD COLUMN tz_offset_s INTEGER",
            [],
        )?;
    }
    // The v22 boolean this replaces. An estimate was the awake bound; every
    // other row was the counter, which is the column's default.
    if has_column(conn, "reading_sessions", "estimated")? {
        conn.execute(
            "UPDATE reading_sessions SET measure = 'awake' WHERE estimated = 1",
            [],
        )?;
        conn.execute("ALTER TABLE reading_sessions DROP COLUMN estimated", [])?;
    }
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS reading_sessions_identity
           ON reading_sessions (device_serial, started_at, end_position)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS reading_sessions_day ON reading_sessions (day)",
        [],
    )?;

    // Which hours of its day a sitting's reading fell in. A session's window and
    // total yield no distribution inside them; only the log's own intervals do,
    // and the parser alone sees them, at parse time.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS reading_session_hours (
            session_id INTEGER NOT NULL
                       REFERENCES reading_sessions (id) ON DELETE CASCADE,
            hour       INTEGER NOT NULL,
            seconds    INTEGER NOT NULL,
            PRIMARY KEY (session_id, hour)
        )"#,
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS reading_sessions_book ON reading_sessions (book_id)",
        [],
    )?;

    // The log snapshots read, per device. A
    // `log_backup_<YYMMDDHHMMSS>.txt.gz` is immutable and names its own moment;
    // one device's name means a different file on another.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS reading_log_dumps (
            device_serial TEXT NOT NULL DEFAULT '',
            name          TEXT NOT NULL,
            read_at       TEXT NOT NULL,
            PRIMARY KEY (device_serial, name)
        )"#,
        [],
    )?;

    // The two end-of-book constants a device states for one book, kept so the
    // pairing outlives the archive it came from. A session must never be stored
    // twice under them: the identity index counts the position, so it would double.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS reading_log_book_ends (
            last_word_position INTEGER PRIMARY KEY,
            from_book          INTEGER NOT NULL
        )"#,
        [],
    )?;

    // The catalog key a log fingerprint was named by, where the reader shell
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS reading_log_asins (
            end_position INTEGER NOT NULL,
            asin         TEXT NOT NULL,
            PRIMARY KEY (end_position, asin)
        )"#,
        [],
    )?;

    // The points a log fingerprint was seen at — the evidence for naming it. A
    // session can arrive before the sidecar naming its book, and the device
    // re-parses nothing it holds.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS reading_log_points (
            end_position INTEGER NOT NULL,
            eid          INTEGER NOT NULL,
            "offset"     INTEGER NOT NULL,
            linear_pos   INTEGER NOT NULL,
            PRIMARY KEY (end_position, eid, "offset", linear_pos)
        )"#,
        [],
    )?;

    // Where an on-device app's mount tree sits on this machine, a location
    // alone. `root` is the mount root inside `source`, naming which of a repo's
    // several apps a row is. The picker's own tree composes in unconditionally.
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

    // Scrub rows from the `My Clippings.txt` ingest path, which wrote orphans
    // (`book_id IS NULL`) and touches no linked annotation. Unconditional and
    // idempotent, reaching a stray fixture or a restored backup.
    conn.execute("DELETE FROM annotations WHERE source = 'clippings'", [])?;

    // Handwritten ink belongs in `book_ink`, outside the text `annotations`
    // table. This scrubs a `Kind::Other` text row carrying an nbk container id
    // as its body, under both record names a device writes.
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

    // Re-key annotation identity without the linear position. A `dedup_hash`
    // salted with `loc_start` gives one passage two hashes, the two origins
    // measuring on different scales;
    if from_version < 12 {
        // Order matters: the colour repair rewrites `note_body`, which the hash
        // is computed over, so it has to land before the re-key.
        repair_colors_read_as_notes(conn)?;
        rekey_annotation_hashes(conn)?;
    }

    // Separate the highlight from a note fused into it.
    if from_version < 13 {
        split_fused_notes(conn)?;
    }

    // A bookmark's identity is its start alone: a device repeats the start as
    if from_version < 14 {
        rekey_annotation_hashes(conn)?;

        // Ink drawn over book content records as
        // `handwritten_on_content_note`. Dropping the checkpoints re-decodes an
        // unchanged nbk, whose host-page anchors reach the ink join.
        conn.execute("DELETE FROM ink_sync", [])?;

        // The same shape for the capture date. The sidecars hold
        // `creationTime`, and import backfills a missing `added_at` from a
        // sidecar it re-reads; dropping the checkpoints re-reads each `.yjr`.
        conn.execute("DELETE FROM yjr_sync", [])?;
    }

    // A snapshot recorded for a partly decoded file claims a name that is never
    // re-read, the claim being the name. The whole table goes: which file was
    // short is a fact about the filesystem, and re-reading costs one pass.
    if from_version < 18 {
        conn.execute("DELETE FROM reading_log_dumps", [])?;
    }

    // v19: `split_sessions_at_midnight` over rows whose `started_at` and
    // `ended_at` fall on two days.
    if from_version < 19 {
        split_sessions_at_midnight(conn)?;
    }

    // v25: `widen_windows_to_their_reading` over rows whose `seconds` exceed
    // `ended_at` minus `started_at`.
    if from_version < 25 {
        widen_windows_to_their_reading(conn)?;
    }

    // `user_version` carries `SCHEMA_VERSION`; `backup` gates a restore on it.
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

    Ok(())
}

/// Split a Sidle-written `note` row into the highlight and the note fused into
/// it. A Kindle keeps the two apart, a highlight record and a note record
/// grouped by span, and a fused row matches neither.
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

        // The fused row is the note and is keyed as one, the hash folding in
        // `kind` and the body. It keeps its id, its capture time, and anything
        // referring to it; the highlight underneath is what goes missing.
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
            // The device synced that highlight back as its own row, the
            // duplicate this split resolves. It becomes the highlight the note
            // hangs off, inheriting the note's colour where it has none.
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
            // Nothing to attach to: the highlight mints from the note's own
            // anchors, matching what a device holds for it.
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

        // A colour describes the marked passage, so it lives on the highlight.
        // Not part of the hash, so moving it off the note changes no identity.
        tx.execute(
            "UPDATE annotations SET color = NULL WHERE id = ?1",
            params![fused.id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Move a highlight colour out of `note_body` into `color`. A colour-capable
/// Kindle writes the colour as a bare string after the annotation's template,
/// the shape a note body takes.
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
            // An orphan (no book yet) hashes against an empty key, exactly as
            // the import path does for a device book not in the library.
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
        return Ok(()); // already re-keyed; nothing to do
    }

    // One survivor per new hash — the earliest, since `rows` is ordered by id.
    // Everything later sharing that hash is a duplicate of it.
    let mut winner: std::collections::HashMap<&str, &Row> = std::collections::HashMap::new();
    for row in &rows {
        winner.entry(row.new.as_str()).or_insert(row);
    }

    let tx = conn.unchecked_transaction()?;

    // Duplicates go before any survivor is rewritten. The hash a survivor takes
    // can be held by the row it absorbs, and `dedup_hash` is UNIQUE; a failure
    // here fails `open()`, which migrates.
    for row in &rows {
        let survivor = winner[row.new.as_str()];
        if survivor.id == row.id {
            continue;
        }
        // Hand device presence to the survivor before dropping the row. A
        // duplicate carrying the final hash keys its presence records
        // correctly, and the delete below takes any that move.
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

    // Then the survivors, whose new hashes are unique among themselves by
    // construction. A row holding one as a stored hash was a duplicate of the
    // same passage.
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

    // Tombstones are keyed by hash and outlive their row, so re-key every old
    // hash we saw — including a duplicate's, whose deletion intent applies to
    // the passage, not to whichever row happened to carry it.
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

/// Current schema version (`PRAGMA user_version`). Backups record it in the
/// manifest; restore refuses an archive whose version exceeds [`SCHEMA_VERSION`].
pub fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

/// Compact the database file, returning freed pages to the OS. A `DELETE` moves
/// pages onto SQLite's free-list, and `VACUUM` reclaims the disk. Runs outside
/// any transaction, over a metadata-only file.
pub fn vacuum(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("VACUUM")
}

/// Look up the book whose KFX hash starts with `prefix`, linking an on-device
/// `<basename>.<sha8>.kfx` to a library row. The sha8 comes from the KFX bytes
/// (`kfx_sha256`), which an .epub-imported book's `sha256` differs from.
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
/// `LIKE ?1 ESCAPE '\'`. A title carrying `_` matches any single character
/// unescaped, mis-linking two near-identical basenames.
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
    // Store the three file paths root-relative so the library is movable;
    // `row_to_book` resolves them back to absolute on read.
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
            // A fresh book's last-edit time is its import time; the curation
            // mutators move it forward later.
            book.imported_at,
            book.title_romaji,
            book.author_romaji,
            book.amazon_asin,
            book.source_format,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

// ---------------------------------------------------------------------------
// Position extent (`books.max_position`).

/// Books that have a KFX but no cached extent yet, as `(id, absolute kfx path)`.
/// The caller parses each and calls [`set_max_position`]; until then the row is
/// simply unattributable, never wrong.
pub fn books_missing_max_position(conn: &Connection) -> rusqlite::Result<Vec<(i64, String)>> {
    let root = conn_root(conn);
    let mut stmt = conn.prepare(
        "SELECT id, kfx_path FROM books
          WHERE kfx_path IS NOT NULL AND max_position IS NULL
          ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    rows.map(|r| r.map(|(id, p)| (id, resolve_one(root.as_deref(), &p))))
        .collect()
}

/// Cache one book's axis extent. `None` records "this file has no position map"
/// so a book that cannot produce one is not retried on every pass; it is stored
/// as 0, which no device can report as a last position.
pub fn set_max_position(
    conn: &Connection,
    book_id: i64,
    max_position: Option<i64>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE books SET max_position = ?2 WHERE id = ?1",
        params![book_id, max_position.unwrap_or(0)],
    )?;
    Ok(())
}

/// Book ids whose `max_position` is `last_position` plus one.
pub fn books_with_last_position(
    conn: &Connection,
    last_position: i64,
) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM books WHERE max_position = ?1 ORDER BY id")?;
    let rows = stmt.query_map([last_position + 1], |r| r.get::<_, i64>(0))?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Reading sessions.

/// Every device serial the library has recorded, from any sync surface — the
/// list a host-side reading-log import picks provenance from, the logs naming
/// no device.
pub fn known_device_serials(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT device_serial FROM annotation_device
         UNION SELECT device_serial FROM yjr_sync
         UNION SELECT device_serial FROM ink_sync
         UNION SELECT device_serial FROM book_ink_device
         UNION SELECT device_serial FROM reading_sessions
          WHERE device_serial <> ''",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.filter(|s| s.as_ref().map(|v| !v.is_empty()).unwrap_or(true))
        .collect()
}

/// One reading session as stored. `book_id` is `None` while the fingerprint
/// matches no book in the library — such a session is reported nowhere; see
/// [`resolve_reading_sessions`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReadingSession {
    pub device_serial: String,
    pub started_at: String,
    pub ended_at: String,
    pub day: String,
    pub end_position: i64,
    pub book_id: Option<i64>,
    pub seconds: i64,
    /// Forward page events the device logged. Not a page count: it moves with
    /// font size and screen, and a converted book has no pagination to count.
    pub page_turns: i64,
    pub words: i64,
    /// Both ends of the device's running counters over this sitting, in
    /// milliseconds and in words, whose differences are `seconds` and `words`.
    pub start_counter_ms: Option<i64>,
    pub end_counter_ms: Option<i64>,
    pub start_words: Option<i64>,
    pub end_words: Option<i64>,
    /// Which regime produced `seconds`. The timer is words-and-WPM driven, and
    /// content it counts no words in earns no time from it; the two regimes
    /// below `counted` answer for that content, and stay apart.
    pub measure: super::reading_log::Measure,
    /// Seconds the reader's own clock stood ahead of UTC. `started_at` and
    /// `ended_at` stay local wall clock; this is what places them.
    pub tz_offset_s: Option<i64>,
}

/// What storing one session did to the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// A sitting the library did not hold.
    Added,
    /// A sitting the library holds, measured past the last batch of events.
    Extended,
    /// Held, with nothing further to say about it.
    Unchanged,
}

/// Store one session, keyed by (device, start, book).
pub fn insert_reading_session(conn: &Connection, s: &ReadingSession) -> rusqlite::Result<Stored> {
    if !s.device_serial.is_empty() {
        conn.execute(
            "UPDATE reading_sessions SET device_serial = ?1
              WHERE device_serial = '' AND started_at = ?2 AND end_position = ?3",
            params![s.device_serial, s.started_at, s.end_position],
        )?;
    }
    let n = conn.execute(
        r#"INSERT OR IGNORE INTO reading_sessions
            (device_serial, started_at, ended_at, day, end_position, book_id,
             seconds, page_turns, words,
             start_counter_ms, end_counter_ms, start_words, end_words, measure,
             tz_offset_s)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"#,
        params![
            s.device_serial,
            s.started_at,
            s.ended_at,
            s.day,
            s.end_position,
            s.book_id,
            s.seconds,
            s.page_turns,
            s.words,
            s.start_counter_ms,
            s.end_counter_ms,
            s.start_words,
            s.end_words,
            s.measure.as_str(),
            s.tz_offset_s,
        ],
    )?;
    if n > 0 {
        return Ok(Stored::Added);
    }
    // A better-ranked measure replaces a worse one at any value, answering the
    // question the worse one stood in for. An awake bound leaves a counted
    // figure alone. At equal rank the longer, later reading wins.
    let extended = conn.execute(
        r#"UPDATE reading_sessions
              SET ended_at = ?4, seconds = ?5, page_turns = ?6, words = ?7,
                  start_counter_ms = ?8, end_counter_ms = ?9,
                  start_words = ?10, end_words = ?11, measure = ?12,
                  tz_offset_s = COALESCE(?14, tz_offset_s)
            WHERE device_serial = ?1 AND started_at = ?2 AND end_position = ?3
              AND (
                    (CASE measure WHEN 'counted' THEN 0 WHEN 'dwell' THEN 1
                                  ELSE 2 END) > ?13
                 OR (measure = ?12
                     AND seconds <= ?5 AND (seconds < ?5 OR ended_at < ?4))
              )"#,
        params![
            s.device_serial,
            s.started_at,
            s.end_position,
            s.ended_at,
            s.seconds,
            s.page_turns,
            s.words,
            s.start_counter_ms,
            s.end_counter_ms,
            s.start_words,
            s.end_words,
            s.measure.as_str(),
            s.measure.rank(),
            s.tz_offset_s,
        ],
    )?;
    Ok(if extended > 0 {
        Stored::Extended
    } else {
        Stored::Unchanged
    })
}

/// Record which hours of its day a session's reading fell in, from the row the
/// same parse produced and only where that row was stored: hours and totals are
/// one measurement.
pub fn record_session_hours(
    conn: &Connection,
    s: &ReadingSession,
    hours: &[(u8, i64)],
) -> rusqlite::Result<()> {
    if hours.is_empty() {
        return Ok(());
    }
    let Some(id) = session_id(conn, s)? else {
        return Ok(());
    };
    conn.execute(
        "DELETE FROM reading_session_hours WHERE session_id = ?1",
        params![id],
    )?;
    for (hour, seconds) in hours {
        conn.execute(
            "INSERT OR REPLACE INTO reading_session_hours (session_id, hour, seconds)
             VALUES (?1, ?2, ?3)",
            params![id, hour, seconds],
        )?;
    }
    Ok(())
}

/// The row id of a stored session, by the identity its callers hold.
fn session_id(conn: &Connection, s: &ReadingSession) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM reading_sessions
          WHERE device_serial = ?1 AND started_at = ?2 AND end_position = ?3",
        params![s.device_serial, s.started_at, s.end_position],
        |r| r.get(0),
    )
    .optional()
}

/// The hours booked against a stored session, ascending.
pub fn session_hours(conn: &Connection, s: &ReadingSession) -> rusqlite::Result<Vec<(u8, i64)>> {
    let Some(id) = session_id(conn, s)? else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(
        "SELECT hour, seconds FROM reading_session_hours
          WHERE session_id = ?1 ORDER BY hour",
    )?;
    let rows = stmt.query_map(params![id], |r| Ok((r.get::<_, i64>(0)? as u8, r.get(1)?)))?;
    rows.collect()
}

/// The newest session stored for one device, naming the row a continuation
/// lands on. [`super::reading_log::parse_sessions`] weighs the events that
/// decide whether the reader is in it.
pub fn newest_reading_session(
    conn: &Connection,
    device_serial: &str,
) -> rusqlite::Result<Option<ReadingSession>> {
    conn.query_row(
        r#"SELECT device_serial, started_at, ended_at, day, end_position, book_id,
                  seconds, page_turns, words,
                  start_counter_ms, end_counter_ms, start_words, end_words, measure,
                  tz_offset_s
             FROM reading_sessions
            WHERE device_serial = ?1
            ORDER BY started_at DESC LIMIT 1"#,
        params![device_serial],
        |r| {
            Ok(ReadingSession {
                device_serial: r.get(0)?,
                started_at: r.get(1)?,
                ended_at: r.get(2)?,
                day: r.get(3)?,
                end_position: r.get(4)?,
                book_id: r.get(5)?,
                seconds: r.get(6)?,
                page_turns: r.get(7)?,
                words: r.get(8)?,
                start_counter_ms: r.get(9)?,
                end_counter_ms: r.get(10)?,
                start_words: r.get(11)?,
                end_words: r.get(12)?,
                measure: super::reading_log::Measure::from_stored(&r.get::<_, String>(13)?),
                tz_offset_s: r.get(14)?,
            })
        },
    )
    .optional()
}

/// One day's share of a session that ran across midnight.
struct Piece {
    started_at: String,
    ended_at: String,
    seconds: i64,
    page_turns: i64,
    words: i64,
}

impl Piece {
    fn day(&self) -> &str {
        &self.started_at[..10]
    }
}

/// Cut a session's wall-clock window at every midnight inside it, dividing the
/// three counters between the pieces in proportion to the time each holds. Empty
/// for a window inside one day, and for an unreadable end.
fn split_across_midnight(
    started_at: &str,
    ended_at: &str,
    seconds: i64,
    page_turns: i64,
    words: i64,
) -> Vec<Piece> {
    let fmt = "%Y-%m-%dT%H:%M:%S";
    let (Ok(from), Ok(to)) = (
        chrono::NaiveDateTime::parse_from_str(started_at, fmt),
        chrono::NaiveDateTime::parse_from_str(ended_at, fmt),
    ) else {
        return Vec::new();
    };
    let span = (to - from).num_seconds();
    if span <= 0 || from.date() == to.date() {
        return Vec::new();
    }

    let mut out: Vec<Piece> = Vec::new();
    let mut cursor = from;
    while cursor < to {
        let Some(midnight) = cursor
            .date()
            .succ_opt()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
        else {
            return Vec::new();
        };
        let end = midnight.min(to);
        let held = (end - cursor).num_seconds();
        let share = |total: i64| total * held / span;
        out.push(Piece {
            started_at: cursor.format(fmt).to_string(),
            // The last instant of the day, for every piece but the one that ends
            // where the session did: reading recorded at `T00:00:00` is the next
            // day's, and that is where the next piece starts.
            ended_at: if end == to {
                ended_at.to_string()
            } else {
                format!("{}T23:59:59", cursor.format("%Y-%m-%d"))
            },
            seconds: share(seconds),
            page_turns: share(page_turns),
            words: share(words),
        });
        cursor = end;
    }

    // Integer division loses up to a second per piece. The remainder goes to
    // the day the session began, as in [`reading_clock`].
    let placed = (
        out.iter().map(|p| p.seconds).sum::<i64>(),
        out.iter().map(|p| p.page_turns).sum::<i64>(),
        out.iter().map(|p| p.words).sum::<i64>(),
    );
    if let Some(first) = out.first_mut() {
        first.seconds += seconds - placed.0;
        first.page_turns += page_turns - placed.1;
        first.words += words - placed.2;
    }
    out
}

/// Cut every stored session spanning more than one day at the midnights it
/// crosses. A row's seconds, page turns and words divide between the days its
/// own clock covered, and all of them stay in the library.
fn split_sessions_at_midnight(conn: &Connection) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, device_serial, started_at, ended_at, end_position, book_id,
                seconds, page_turns, words
           FROM reading_sessions
          WHERE substr(started_at, 1, 10) <> substr(ended_at, 1, 10)",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let mut split = 0;
    for (id, device_serial, started_at, ended_at, end_position, book_id, seconds, turns, words) in
        rows
    {
        let pieces = split_across_midnight(&started_at, &ended_at, seconds, turns, words);
        let Some((first, rest)) = pieces.split_first() else {
            continue;
        };
        for piece in rest {
            conn.execute(
                r#"INSERT OR IGNORE INTO reading_sessions
                    (device_serial, started_at, ended_at, day, end_position, book_id,
                     seconds, page_turns, words)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"#,
                params![
                    device_serial,
                    piece.started_at,
                    piece.ended_at,
                    piece.day(),
                    end_position,
                    book_id,
                    piece.seconds,
                    piece.page_turns,
                    piece.words,
                ],
            )?;
        }
        conn.execute(
            "UPDATE reading_sessions
                SET ended_at = ?1, day = ?2, seconds = ?3, page_turns = ?4, words = ?5
              WHERE id = ?6",
            params![
                first.ended_at,
                first.day(),
                first.seconds,
                first.page_turns,
                first.words,
                id
            ],
        )?;
        split += 1;
    }
    Ok(split)
}

/// Set `ended_at` to `started_at` plus `seconds` on every `reading_sessions`
/// row whose `seconds` exceed `ended_at` minus `started_at`, capped at the last
/// second of the row's `day`. Every other column keeps its value.
fn widen_windows_to_their_reading(conn: &Connection) -> rusqlite::Result<usize> {
    let fmt = "%Y-%m-%dT%H:%M:%S";
    let mut stmt = conn.prepare(
        "SELECT id, day, started_at, ended_at, seconds FROM reading_sessions WHERE seconds > 0",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let mut widened = 0;
    for (id, day, started_at, ended_at, seconds) in rows {
        let (Ok(from), Ok(to)) = (
            chrono::NaiveDateTime::parse_from_str(&started_at, fmt),
            chrono::NaiveDateTime::parse_from_str(&ended_at, fmt),
        ) else {
            continue;
        };
        if (to - from).num_seconds() >= seconds {
            continue;
        }
        let end = (from + chrono::Duration::seconds(seconds))
            .format(fmt)
            .to_string()
            .min(format!("{day}T23:59:59"));
        if end <= ended_at {
            continue;
        }
        conn.execute(
            "UPDATE reading_sessions SET ended_at = ?1 WHERE id = ?2",
            params![end, id],
        )?;
        widened += 1;
    }
    Ok(widened)
}

/// `book_id` per `Point`, over the `reading_position` rows whose `source` is
/// `device` and whose `eid`, `offset` and `linear_pos` are all set. A `Point`
/// two books claim is dropped.
pub fn device_positions(
    conn: &Connection,
) -> rusqlite::Result<std::collections::HashMap<Point, i64>> {
    let mut stmt = conn.prepare(
        r#"SELECT eid, "offset", linear_pos, book_id FROM reading_position
            WHERE source = 'device'
              AND eid IS NOT NULL AND "offset" IS NOT NULL AND linear_pos IS NOT NULL"#,
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(((r.get(0)?, r.get(1)?, r.get(2)?), r.get::<_, i64>(3)?))
    })?;
    let mut out: std::collections::HashMap<Point, i64> = Default::default();
    let mut ambiguous = Vec::new();
    for row in rows {
        let (key, book) = row?;
        match out.get(&key) {
            Some(held) if *held != book => ambiguous.push(key),
            _ => {
                out.insert(key, book);
            }
        }
    }
    for key in ambiguous {
        out.remove(&key);
    }
    Ok(out)
}

/// A point in a book: its source element id, the offset into that element, and
/// the same place on the linear axis. Every book shares a bare coordinate near
/// its front.
pub type Point = (i64, i64, i64);

/// The single book every one of these points belongs to, or `None`. One
/// agreement suffices, a point carrying the book's own element id. Two
/// different books agreeing is a contradiction, and names nothing.
fn sole_book_at(points: &[Point], anchors: &std::collections::HashMap<Point, i64>) -> Option<i64> {
    let mut found: Option<i64> = None;
    for point in points {
        let Some(&book) = anchors.get(point) else {
            continue;
        };
        match found {
            Some(held) if held != book => return None,
            _ => found = Some(book),
        }
    }
    found
}

/// Remember where a fingerprint's reader was seen standing — evidence, the book
/// being unnameable yet and the lines carrying it coming once.
pub fn record_log_points(
    conn: &Connection,
    end_position: i64,
    points: &[Point],
) -> rusqlite::Result<()> {
    for (eid, offset, linear_pos) in points {
        conn.execute(
            r#"INSERT OR IGNORE INTO reading_log_points (end_position, eid, "offset", linear_pos)
                VALUES (?1, ?2, ?3, ?4)"#,
            params![end_position, eid, offset, linear_pos],
        )?;
    }
    Ok(())
}

/// Every point recorded against each fingerprint.
fn log_points(conn: &Connection) -> rusqlite::Result<std::collections::HashMap<i64, Vec<Point>>> {
    let mut stmt =
        conn.prepare(r#"SELECT end_position, eid, "offset", linear_pos FROM reading_log_points"#)?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?)))
    })?;
    let mut out: std::collections::HashMap<i64, Vec<Point>> = Default::default();
    for row in rows {
        let (fingerprint, point) = row?;
        out.entry(fingerprint).or_default().push(point);
    }
    Ok(out)
}

/// Remember which last position goes with which last-word position. First
/// sighting wins, as in the parser: two builds of one title differ in both
/// numbers together.
pub fn record_book_ends(conn: &Connection, pairs: &[(i64, i64)]) -> rusqlite::Result<()> {
    for (last_word_position, from_book) in pairs {
        conn.execute(
            "INSERT OR IGNORE INTO reading_log_book_ends (last_word_position, from_book)
             VALUES (?1, ?2)",
            params![last_word_position, from_book],
        )?;
    }
    Ok(())
}

/// Record that a fingerprint was named by a catalog key — every key seen. A
/// fingerprint naming two is a question a later pass answers by which the
/// library holds.
pub fn record_log_asin(conn: &Connection, end_position: i64, asin: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO reading_log_asins (end_position, asin) VALUES (?1, ?2)",
        params![end_position, asin],
    )?;
    Ok(())
}

/// The sole `books` row matching `end_position`'s `reading_log_asins` keys on
/// `asin` or `amazon_asin`. `None` for none and for more than one.
fn book_named_by_log_asin(conn: &Connection, end_position: i64) -> rusqlite::Result<Option<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT b.id
           FROM reading_log_asins a
           JOIN books b ON b.asin = a.asin OR b.amazon_asin = a.asin
          WHERE a.end_position = ?1",
    )?;
    let ids: Vec<i64> = stmt
        .query_map(params![end_position], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(match ids[..] {
        [only] => Some(only),
        _ => None,
    })
}

/// Every pairing learned so far.
pub fn known_book_ends(conn: &Connection) -> rusqlite::Result<std::collections::HashMap<i64, i64>> {
    let mut stmt =
        conn.prepare("SELECT last_word_position, from_book FROM reading_log_book_ends")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// The newest reading event this library holds for one device, the watermark it
/// syncs against: the device skips whole log dumps by filename timestamp.
/// Per-serial, two Kindles being read independently.
pub fn reading_watermark(
    conn: &Connection,
    device_serial: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT MAX(ended_at) FROM reading_sessions WHERE device_serial = ?1",
        params![device_serial],
        |r| r.get(0),
    )
}

/// The log snapshots read from one device, by filename. A name in this set is a
/// file whose every event is stored, skipped unopened, and a snapshot is
/// immutable.
pub fn seen_dumps(
    conn: &Connection,
    device_serial: &str,
) -> rusqlite::Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT name FROM reading_log_dumps WHERE device_serial = ?1")?;
    let rows = stmt.query_map(params![device_serial], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// Which device these log snapshots belong to. A snapshot's name encodes the
/// second it was written, which two Kindles do not share. `Ok(None)` marks
/// files never seen; `Err(names)` marks a folder mixing two Kindles' logs.
pub fn dumps_owner(
    conn: &Connection,
    names: &[String],
) -> rusqlite::Result<Result<Option<String>, Vec<String>>> {
    let mut stmt =
        conn.prepare("SELECT DISTINCT device_serial FROM reading_log_dumps WHERE name = ?1")?;
    let mut owners: Vec<String> = Vec::new();
    for name in names {
        for owner in stmt.query_map(params![name], |r| r.get::<_, String>(0))? {
            let owner = owner?;
            if !owner.is_empty() && !owners.contains(&owner) {
                owners.push(owner);
            }
        }
    }
    Ok(match owners.len() {
        0 => Ok(None),
        1 => Ok(Some(owners.remove(0))),
        _ => Err(owners),
    })
}

/// Erase the reading log completely — every session and every record of which
/// snapshots have been read, together, leaving the archives re-importable.
/// Returns how many sessions went.
pub fn clear_reading_log(conn: &Connection) -> rusqlite::Result<usize> {
    // Explicit, past the cascade: the foreign key fires under `PRAGMA
    // foreign_keys`, and a connection opened without it leaves these rows for
    // the next row id to adopt.
    conn.execute("DELETE FROM reading_session_hours", [])?;
    let sessions = conn.execute("DELETE FROM reading_sessions", [])?;
    conn.execute("DELETE FROM reading_log_dumps", [])?;
    Ok(sessions)
}

/// Record that a snapshot has been read in full.
pub fn mark_dump_read(conn: &Connection, device_serial: &str, name: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO reading_log_dumps (device_serial, name, read_at)
         VALUES (?1, ?2, ?3)",
        params![device_serial, name, now_iso()],
    )?;
    Ok(())
}

/// Seconds read per day over `[from, to]` (inclusive, `YYYY-MM-DD`), skipping
/// empty days, for the calendar heatmap. Unattributed sessions are excluded
/// here as everywhere; [`resolve_reading_sessions`] keeps the rows.
pub fn reading_days(
    conn: &Connection,
    from: &str,
    to: &str,
) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT day, SUM(seconds) FROM reading_sessions
          WHERE day BETWEEN ?1 AND ?2 AND book_id IS NOT NULL
          GROUP BY day ORDER BY day",
    )?;
    let rows = stmt.query_map(params![from, to], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// One cell of the reading clock: seconds read in a given hour of the day, on
/// the days of one month falling on one weekday.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClockCell {
    /// `YYYY-MM`. A year is a prefix of it, which is how the page windows this
    /// to whatever the heatmap is showing.
    pub month: String,
    /// Days since Sunday, 0–6 — matching the heatmap's own week, which starts
    /// there.
    pub dow: u8,
    pub hour: u8,
    pub seconds: i64,
}

/// Beyond this much wall clock, a session is no sitting to spread reading
/// across. Sessions cut at a half-hour silence, and the widest the parser
/// produced over a month of one device's archives was 6.2 h.
const SPREADABLE_SPAN_SECS: i64 = 6 * 3600;

/// When reading happened, by hour of the day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Booked {
    /// `reading_session_hours` stated the interval.
    Measured,
    /// Shared across the window in proportion to each hour's overlap.
    Spread,
    /// Placed whole on the starting hour, past [`SPREADABLE_SPAN_SECS`].
    Whole,
}

/// Every attributed session's seconds, on the true clock hours of its own day,
/// past midnight included. `f` takes the day, the hour, the seconds, and the
/// [`Booked`] regime. [`reading_clock`] and [`reading_day_hours`] consume it.
fn walk_clock_hours(
    conn: &Connection,
    mut f: impl FnMut(&str, u8, i64, Booked),
) -> rusqlite::Result<()> {
    let parses = |day: &str| chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok();

    let mut measured = conn.prepare(
        "SELECT s.day, h.hour, h.seconds
           FROM reading_session_hours h
           JOIN reading_sessions s ON s.id = h.session_id
          WHERE s.book_id IS NOT NULL",
    )?;
    for row in measured.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, u8>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (day, hour, seconds) = row?;
        if parses(&day) {
            f(&day, hour.min(23), seconds, Booked::Measured);
        }
    }

    let mut stmt = conn.prepare(
        "SELECT day, started_at, ended_at, seconds FROM reading_sessions
          WHERE book_id IS NOT NULL AND seconds > 0
            AND id NOT IN (SELECT session_id FROM reading_session_hours)",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;

    for row in rows {
        let (day, started_at, ended_at, seconds) = row?;
        if !parses(&day) {
            continue;
        }
        let (Some(from), Some(to)) = (clock_secs(&started_at), clock_secs(&ended_at)) else {
            continue;
        };
        // `to` below `from` places the end past midnight.
        let span = if to >= from {
            to - from
        } else {
            to + 86400 - from
        };
        if span == 0 || span > SPREADABLE_SPAN_SECS {
            f(&day, (from / 3600) as u8, seconds, Booked::Whole);
            continue;
        }
        // The division's remainder goes to the starting hour; the shares sum
        // to `seconds`.
        let mut placed = 0;
        for h in (from / 3600)..=((from + span) / 3600) {
            let (lo, hi) = (h * 3600, (h + 1) * 3600);
            let overlap = (from + span).min(hi) - from.max(lo);
            if overlap <= 0 {
                continue;
            }
            let share = seconds * overlap / span;
            placed += share;
            f(&day, ((h % 24) as u8).min(23), share, Booked::Spread);
        }
        f(&day, (from / 3600) as u8, seconds - placed, Booked::Spread);
    }
    Ok(())
}

pub fn reading_clock(conn: &Connection) -> rusqlite::Result<Vec<ClockCell>> {
    let mut cells: BTreeMap<(String, u8, u8), i64> = BTreeMap::new();
    walk_clock_hours(conn, |day, hour, seconds, _| {
        let date = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").expect("walk parses the day");
        let key = (
            day[..7].to_string(),
            chrono::Datelike::weekday(&date).num_days_from_sunday() as u8,
            hour,
        );
        *cells.entry(key).or_default() += seconds;
    })?;

    Ok(cells
        .into_iter()
        .filter(|(_, seconds)| *seconds > 0)
        .map(|((month, dow, hour), seconds)| ClockCell {
            month,
            dow,
            hour,
            seconds,
        })
        .collect())
}

/// The shape of one day: how its seconds fall across the 24 clock hours.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DayShape {
    /// `YYYY-MM-DD`, device-local.
    pub day: String,
    /// 24 entries, seconds per clock hour, summing to the day's total.
    pub hours: Vec<i64>,
    /// Seconds placed whole on one hour ([`Booked::Whole`]). `hours` built from
    /// such a row carries one saturated hour and 23 empty ones, and is drawn or
    /// suppressed on this figure.
    pub unplaced_seconds: i64,
}

/// The hours of each day over `[from, to]` (inclusive, `YYYY-MM-DD`). A
/// marginal of [`walk_clock_hours`] keyed by day, matching [`reading_days`]
/// over the same window.
pub fn reading_day_hours(
    conn: &Connection,
    from: &str,
    to: &str,
) -> rusqlite::Result<Vec<DayShape>> {
    let mut days: BTreeMap<String, (Vec<i64>, i64)> = BTreeMap::new();
    walk_clock_hours(conn, |day, hour, seconds, how| {
        if day < from || day > to {
            return;
        }
        let entry = days
            .entry(day.to_string())
            .or_insert_with(|| (vec![0; 24], 0));
        entry.0[hour as usize] += seconds;
        if how == Booked::Whole {
            entry.1 += seconds;
        }
    })?;
    Ok(days
        .into_iter()
        .map(|(day, (hours, unplaced_seconds))| DayShape {
            day,
            hours,
            unplaced_seconds,
        })
        .collect())
}

/// Seconds into the day of a `YYYY-MM-DDTHH:MM:SS` stamp.
fn clock_secs(iso: &str) -> Option<i64> {
    if iso.len() < 19 {
        return None;
    }
    let h: i64 = iso[11..13].parse().ok()?;
    let m: i64 = iso[14..16].parse().ok()?;
    let s: i64 = iso[17..19].parse().ok()?;
    Some(h * 3600 + m * 60 + s)
}

/// One stored sitting, with the book it is attributed to.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionRow {
    pub id: i64,
    pub day: String,
    /// Device-local wall clock, `YYYY-MM-DDTHH:MM:SS`.
    pub started_at: String,
    pub ended_at: String,
    /// Counted reading, which differs from the width of
    /// `[started_at, ended_at]`.
    pub seconds: i64,
    pub book_id: i64,
    pub title: String,
    pub page_turns: i64,
    pub words: i64,
    /// `counted` | `dwell` | `awake` — see [`super::reading_log::Measure`].
    pub measure: String,
    pub device_serial: String,
}

/// The sittings over `[from, to]` (inclusive, `YYYY-MM-DD`), earliest first.
///
/// Rows carrying a `book_id`, matching every other query here.
pub fn reading_sessions_on(
    conn: &Connection,
    from: &str,
    to: &str,
) -> rusqlite::Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.day, s.started_at, s.ended_at, s.seconds, s.book_id, b.title,
                s.page_turns, s.words, s.measure, s.device_serial
           FROM reading_sessions s JOIN books b ON b.id = s.book_id
          WHERE s.day BETWEEN ?1 AND ?2
          ORDER BY s.started_at",
    )?;
    let rows = stmt.query_map(params![from, to], |r| {
        Ok(SessionRow {
            id: r.get(0)?,
            day: r.get(1)?,
            started_at: r.get(2)?,
            ended_at: r.get(3)?,
            seconds: r.get(4)?,
            book_id: r.get(5)?,
            title: r.get(6)?,
            page_turns: r.get(7)?,
            words: r.get(8)?,
            measure: r.get(9)?,
            device_serial: r.get(10)?,
        })
    })?;
    rows.collect()
}

/// How far into a book its last-read position sits.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BookProgress {
    /// The furthest `reading_position.linear_pos` across every source.
    pub linear_pos: i64,
    /// `books.max_position` — the exclusive end of the same axis.
    pub max_position: i64,
    /// The `reading_position.source` holding that furthest position.
    pub source: String,
    pub updated_at: String,
    /// [`progress_fraction`] of the two positions above.
    pub fraction: f64,
}

/// True for a `finished_at` mark, and for a `fraction` rounding to 100% — the
/// rounding the reading log's percentage prints.
pub fn is_finished(fraction: Option<f64>, finished_at: Option<&str>) -> bool {
    if finished_at.is_some_and(|s| !s.is_empty()) {
        return true;
    }
    fraction.is_some_and(|f| (f * 100.0).round() >= 100.0)
}

/// `books.finished_at` for one book, `None` when unmarked or absent.
pub fn book_finished_at(conn: &Connection, book_id: i64) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT finished_at FROM books WHERE id = ?1",
        params![book_id],
        |r| r.get(0),
    )
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(other),
    })
}

/// Mark a book read, or take the mark off. Stores the moment it was marked.
pub fn set_book_finished(conn: &Connection, book_id: i64, finished: bool) -> rusqlite::Result<()> {
    let at = finished.then(now_iso);
    conn.execute(
        "UPDATE books SET finished_at = ?2 WHERE id = ?1",
        params![book_id, at],
    )?;
    Ok(())
}

/// The share of a book's axis read, in `[0, 1]`, or `None` without an axis.
/// `linear_pos` past `max_position` clamps.
pub fn progress_fraction(linear_pos: i64, max_position: i64) -> Option<f64> {
    if max_position <= 0 {
        return None;
    }
    Some((linear_pos as f64 / max_position as f64).clamp(0.0, 1.0))
}

/// One book's place on its own position axis. `None` where `max_position` or
/// `reading_position` is absent. `linear_pos` can exceed `max_position` where
/// the stored KFX differs from the build read on the device, and clamps.
pub fn book_progress(conn: &Connection, book_id: i64) -> rusqlite::Result<Option<BookProgress>> {
    let max: Option<i64> = conn.query_row(
        "SELECT max_position FROM books WHERE id = ?1",
        params![book_id],
        |r| r.get(0),
    )?;
    let Some(max_position) = max.filter(|m| *m > 0) else {
        return Ok(None);
    };
    conn.query_row(
        r#"SELECT linear_pos, source, updated_at FROM reading_position
            WHERE book_id = ?1 AND linear_pos IS NOT NULL
            ORDER BY linear_pos DESC LIMIT 1"#,
        params![book_id],
        |r| {
            let linear_pos: i64 = r.get(0)?;
            Ok(Some(BookProgress {
                linear_pos,
                max_position,
                source: r.get(1)?,
                updated_at: r.get(2)?,
                fraction: progress_fraction(linear_pos, max_position).unwrap_or(0.0),
            }))
        },
    )
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(other),
    })
}

/// Per-day totals for one book, oldest first — the book page's calendar.
pub fn reading_book_days(conn: &Connection, book_id: i64) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT day, SUM(seconds) FROM reading_sessions
          WHERE book_id = ?1 GROUP BY day ORDER BY day",
    )?;
    let rows = stmt.query_map(params![book_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// How [`reading_books`] orders what it returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadingSort {
    #[default]
    LastRead,
    Seconds,
    Sessions,
    Words,
}

impl ReadingSort {
    pub fn from_name(name: &str) -> Self {
        match name {
            "seconds" => Self::Seconds,
            "sessions" => Self::Sessions,
            "words" => Self::Words,
            _ => Self::LastRead,
        }
    }

    fn expr(self) -> &'static str {
        match self {
            Self::LastRead => "MAX(s.ended_at)",
            Self::Seconds => "SUM(s.seconds)",
            Self::Sessions => "COUNT(*)",
            Self::Words => "SUM(s.words)",
        }
    }
}

/// How finely [`reading_books`] slices the window it sums over: one row per book
/// per slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadingBucket {
    #[default]
    Total,
    Year,
    Month,
    Day,
}

impl ReadingBucket {
    pub fn from_name(name: &str) -> Self {
        match name {
            "year" => Self::Year,
            "month" => Self::Month,
            "day" => Self::Day,
            _ => Self::Total,
        }
    }

    /// The slice key, as SQL over a `YYYY-MM-DD` day. Prefixes of that key sort
    /// chronologically as text, so ordering by it needs no date arithmetic.
    fn expr(self) -> &'static str {
        match self {
            Self::Total => "''",
            Self::Year => "substr(s.day, 1, 4)",
            Self::Month => "substr(s.day, 1, 7)",
            Self::Day => "s.day",
        }
    }
}

/// Books read over `[from, to]` (inclusive, `YYYY-MM-DD`), with every figure
/// summed **within that window** — a book's total for one day, or for one year,
/// is a different number from its total ever.
pub fn reading_books(
    conn: &Connection,
    from: &str,
    to: &str,
    sort: ReadingSort,
    asc: bool,
    bucket: ReadingBucket,
) -> rusqlite::Result<Vec<ReadingEntry>> {
    let root = conn_root(conn);
    let dir = if asc { "ASC" } else { "DESC" };
    let bucket = bucket.expr();
    let mut stmt = conn.prepare(&format!(
        r#"SELECT s.book_id, b.title, b.author, b.sha256, b.cover_path,
                  SUM(s.seconds), SUM(s.page_turns), SUM(s.words), COUNT(*),
                  MIN(s.started_at), MAX(s.ended_at),
                  GROUP_CONCAT(DISTINCT s.device_serial), {bucket},
                  SUM(CASE WHEN s.measure = 'dwell' THEN s.seconds ELSE 0 END),
                  SUM(CASE WHEN s.measure = 'awake' THEN s.seconds ELSE 0 END),
                  b.max_position,
                  (SELECT MAX(rp.linear_pos) FROM reading_position rp
                    WHERE rp.book_id = s.book_id),
                  b.finished_at
             FROM reading_sessions s JOIN books b ON b.id = s.book_id
            WHERE s.day BETWEEN ?1 AND ?2
            GROUP BY {bucket}, s.book_id
            ORDER BY {bucket} {dir}, {} {dir}, MAX(s.ended_at) DESC"#,
        sort.expr()
    ))?;
    let rows = stmt.query_map(params![from, to], |r| row_to_entry(r, root.as_deref()))?;
    rows.collect()
}

/// How many books ever read [`is_finished`] holds for. The rows fold through
/// it, keeping one rule for what finished means.
pub fn reading_finished_count(conn: &Connection) -> rusqlite::Result<i64> {
    let mut stmt = conn.prepare(
        r#"SELECT b.max_position, b.finished_at,
                  (SELECT MAX(rp.linear_pos) FROM reading_position rp
                    WHERE rp.book_id = b.id)
             FROM books b
            WHERE b.id IN (SELECT DISTINCT book_id FROM reading_sessions
                            WHERE book_id IS NOT NULL)"#,
    )?;
    let rows = stmt.query_map([], |r| {
        let max_position: Option<i64> = r.get(0)?;
        let finished_at: Option<String> = r.get(1)?;
        let linear_pos: Option<i64> = r.get(2)?;
        let fraction = match (linear_pos, max_position) {
            (Some(pos), Some(max)) => progress_fraction(pos, max),
            _ => None,
        };
        Ok(is_finished(fraction, finished_at.as_deref()))
    })?;
    let mut n = 0;
    for done in rows {
        if done? {
            n += 1;
        }
    }
    Ok(n)
}

/// How many distinct books have ever been read. A count, not a list: the
/// headline figure needs no titles and no cover stats.
pub fn reading_book_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(DISTINCT book_id) FROM reading_sessions WHERE book_id IS NOT NULL",
        [],
        |r| r.get(0),
    )
}

/// An aggregate over sessions — a book on one day, or a book across all time.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReadingEntry {
    /// Which slice of the window every figure below covers: `""` for the whole
    /// of it, else `YYYY`, `YYYY-MM` or `YYYY-MM-DD` per the [`ReadingBucket`]
    /// asked for.
    pub bucket: String,
    pub book_id: i64,
    pub title: String,
    pub author: String,
    /// Cover fields carry the same meaning as on [`BookRow`], so the Reading
    /// Log's cards render through the gallery's own cover path.
    pub cover_path: Option<String>,
    pub cover_thumb_path: Option<String>,
    pub cover_rev: i64,
    pub seconds: i64,
    /// How much of [`Self::seconds`] came from the page dwell, apart from the
    /// device's own counter — see [`super::reading_log::Measure::Dwell`]. A
    /// measurement of the same kind, over content the counter refuses.
    pub dwell_seconds: i64,
    /// How much of [`Self::seconds`] is the awake bound — see
    /// [`super::reading_log::Measure::Awake`]. A bound, not a measurement.
    pub awake_seconds: i64,
    /// See [`ReadingSession::page_turns`] — device page events, not pagination.
    pub page_turns: i64,
    pub words: i64,
    pub sessions: i64,
    pub first_at: String,
    pub last_at: String,
    /// Which devices this reading happened on. Empty when the sessions predate
    /// knowing — an archive imported without saying where it came from — never
    /// a guess.
    pub devices: Vec<String>,
    /// [`progress_fraction`] over this book's furthest position, `None` where
    /// either half is unstored. The same figure [`book_progress`] reports.
    pub progress: Option<f64>,
    /// [`is_finished`] of that fraction and `books.finished_at`.
    pub finished: bool,
}

fn row_to_entry(r: &rusqlite::Row<'_>, root: Option<&Path>) -> rusqlite::Result<ReadingEntry> {
    let sha256: String = r.get(3)?;
    let cover_path = resolve_opt(root, r.get(4)?);
    let (cover_thumb_path, cover_rev) = served_cover(root, &sha256, cover_path.as_deref());
    let devices: Option<String> = r.get(11)?;
    let max_position: Option<i64> = r.get(15)?;
    let linear_pos: Option<i64> = r.get(16)?;
    let progress = match (linear_pos, max_position) {
        (Some(pos), Some(max)) => progress_fraction(pos, max),
        _ => None,
    };
    let finished_at: Option<String> = r.get(17)?;
    let finished = is_finished(progress, finished_at.as_deref());
    Ok(ReadingEntry {
        bucket: r.get(12)?,
        book_id: r.get(0)?,
        title: r.get(1)?,
        author: r.get(2)?,
        cover_path,
        cover_thumb_path,
        cover_rev,
        seconds: r.get(5)?,
        dwell_seconds: r.get(13)?,
        awake_seconds: r.get(14)?,
        page_turns: r.get(6)?,
        words: r.get(7)?,
        sessions: r.get(8)?,
        first_at: r.get(9)?,
        last_at: r.get(10)?,
        // The `''` a provenance-less row carries is not a device; drop it rather
        // than render it as one.
        devices: devices
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        progress,
        finished,
    })
}

/// Attach `book_id` to every session whose fingerprint resolves to exactly one
/// book, and return how many gained one.
pub fn resolve_reading_sessions(conn: &Connection) -> rusqlite::Result<usize> {
    let mut stmt =
        conn.prepare("SELECT DISTINCT end_position FROM reading_sessions WHERE book_id IS NULL")?;
    let pending: Vec<i64> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    // A session stored before its book's two end constants were paired is keyed
    // by the last-word position, which matches no book. A known pairing re-keys
    // it.
    let ends = known_book_ends(conn)?;
    for position in &pending {
        if let Some(from_book) = ends.get(position) {
            conn.execute(
                "UPDATE OR IGNORE reading_sessions SET end_position = ?2
                  WHERE book_id IS NULL AND end_position = ?1",
                params![position, from_book],
            )?;
        }
    }
    let pending: Vec<i64> = pending
        .into_iter()
        .map(|p| ends.get(&p).copied().unwrap_or(p))
        .collect();

    // Where each Kindle says it left each book. The log never names a book, but
    // a point it does name is one a sidecar can also name — see
    // [`device_positions`].
    let anchors = device_positions(conn)?;
    let points = log_points(conn)?;

    let mut resolved = 0;
    for position in pending {
        // The catalog key first. It is a name, where the two below are
        // inferences from geometry, and it holds when the library's build of a
        // title ends at a different position than the device's.
        let book_id = match book_named_by_log_asin(conn, position)? {
            Some(named) => Some(named),
            None => match books_with_last_position(conn, position)?[..] {
                [only] => Some(only),
                // The axis decided nothing — no book ends there, or several do.
                // Where the reader stood is a different question, and it
                // answers some the axis cannot.
                _ => sole_book_at(points.get(&position).map_or(&[], Vec::as_slice), &anchors),
            },
        };
        if let Some(book_id) = book_id {
            resolved += conn.execute(
                "UPDATE reading_sessions SET book_id = ?2
                  WHERE book_id IS NULL AND end_position = ?1",
                params![position, book_id],
            )?;
        }
    }
    Ok(resolved)
}

/// Reading that resolved to no book, one row per position it stopped at.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UnmatchedReading {
    /// The device's last-valid position: this group's whole identity, and what
    /// [`books_with_last_position`] is asked about.
    pub end_position: i64,
    pub sessions: i64,
    pub seconds: i64,
    pub page_turns: i64,
    pub words: i64,
    pub first_at: String,
    pub last_at: String,
    /// Which Kindles this was read on; empty for sessions imported without
    /// provenance. Same meaning as on [`ReadingEntry`].
    pub devices: Vec<String>,
}

/// Every position with reading against it that belongs to no book, newest last
/// read first.
pub fn unmatched_reading(conn: &Connection) -> rusqlite::Result<Vec<UnmatchedReading>> {
    let mut stmt = conn.prepare(
        "SELECT end_position, COUNT(*), SUM(seconds), SUM(page_turns), SUM(words),
                MIN(started_at), MAX(ended_at), GROUP_CONCAT(DISTINCT device_serial)
           FROM reading_sessions
          WHERE book_id IS NULL
          GROUP BY end_position
          ORDER BY MAX(ended_at) DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        let devices: Option<String> = r.get(7)?;
        Ok(UnmatchedReading {
            end_position: r.get(0)?,
            sessions: r.get(1)?,
            seconds: r.get(2)?,
            page_turns: r.get(3)?,
            words: r.get(4)?,
            first_at: r.get(5)?,
            last_at: r.get(6)?,
            devices: devices
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        })
    })?;
    rows.collect()
}

/// Settle a position by hand: attach `book_id` to every unattributed session
/// that stopped there.
pub fn attribute_reading_position(
    conn: &Connection,
    end_position: i64,
    book_id: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE reading_sessions SET book_id = ?2
          WHERE book_id IS NULL AND end_position = ?1",
        params![end_position, book_id],
    )
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

/// Record the format a book arrived in. Provenance, not metadata: it names no
/// value a produced file carries, and a change reconverts nothing.
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
    /// Page progression direction: `"rtl"` | `"ltr"` | `None` (Auto). Baked into
    /// the generated KFX's reading order; a change triggers a force-reconvert.
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

/// Sparse patch for bulk metadata editing across many books. Unlike
/// [`MetadataPatch`] (a full replacement), every scalar here means `None` =
/// "leave unchanged on every book", `Some(v)` = "set to v on every book".
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

/// Apply a sparse [`BulkMetadataPatch`] to one book. Only present scalars
/// change; tags merge additively. Returns `Ok(false)` when the book id no
/// longer exists (caller skips it), `Ok(true)` when a row was written.
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
    // Handwritten ink + presence + checkpoint (asin-keyed). A book with no asin
    // can have no ink, so skip these when it's absent/empty.
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
    /// Hash of the KFX bytes — required iff `kfx_path` is `Some`.
    /// See `BookRow::kfx_sha256` for the reasoning.
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
    /// The caller passes the canonical tag list, trimmed, lowercased and
    /// deduped. `insert_book` serializes it to a JSON array TEXT.
    pub tags: &'a [String],
    /// Editable romaji of the title/author, rendered yomigana-aware at import
    /// (see [`super::romaji`] and `import::extract_meta`). The picker searches it.
    pub title_romaji: &'a str,
    pub author_romaji: &'a str,
    /// The format that arrived, as `SourceKind::as_str` names it. An `.azw3`
    /// or `.mobi` import records its own extension, not the EPUB it exported.
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

/// Match by the **basename** of `kfx_path` — used by `device_list_ours` to
/// recognize on-device files pushed before the `.<sha8>.kfx` naming convention
/// existed (their device filename is just the library row's kfx basename).
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

/// `(cover_thumb(sha), its mtime)` where that file exists, otherwise
/// `(None, cover_path`'s mtime)`.
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
    // No thumb on disk: no gallery thumb, and the rev tracks the full cover,
    // which a cover swap busts through `?v=`.
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
    // Defensive parse: we control writes and only emit canonical JSON
    // arrays, but a corrupt column shouldn't take down the whole list.
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    // Bind the fields the derived `search_key` reads up front, so it can borrow
    // them before they're moved into the struct.
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
    // Resolve the stored root-relative cover path up front — it feeds
    // both the struct field and the served-image rev's fallback stat.
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
        // Appended after the original column list, past `asin`: every reader
        // here indexes positionally.
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
// The table lives in `migrate` outside the destructive reset.

/// One stored Scribe notebook. Files live at `notebooks/<uuid>/` (derived from
/// `uuid` via [`crate::library::LibraryPaths`], not stored here).
#[derive(Debug, Clone, Serialize)]
pub struct NotebookRow {
    pub id: i64,
    pub uuid: String,
    pub title: String,
    pub page_count: i64,
    /// SHA-256 of the backed-up `nbk` bytes — change-detects an edited notebook.
    pub nbk_sha256: Option<String>,
    pub imported_at: String,
    /// On-device "Date Modified" of the source `nbk` (RFC 3339), captured at
    /// import. `None` for notebooks imported before this column existed.
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

/// Backfill `updated_at` for a notebook carrying NULL, without re-extracting
/// it. The import "unchanged" fast path calls it, handing the row its
/// on-device Date Modified.
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

/// Delete a notebook row, returning its uuid (so the caller removes the files).
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
        // Tombstone it so a device/folder re-import won't resurrect it (Restore
        // from device clears this).
        record_deletion(conn, DELETION_NOTEBOOK, u)?;
    }
    Ok(uuid)
}

// ---------------------------------------------------------------------------
// Handwritten ink on a sideloaded doc (`book_ink`). Tables live in `migrate`
// outside the destructive reset.

/// One imported ink page — the user's handwriting on a single host PDF page.
/// Cached SVGs live at `books/<sha>/ink/<asin>/<container>.{overlay,plain}.svg`
/// (derived from `asin` + `container_id` via [`crate::library::LibraryPaths`]).
#[derive(Debug, Clone, Serialize)]
pub struct BookInkRow {
    pub id: i64,
    pub book_id: Option<i64>,
    pub asin: String,
    pub container_id: String,
    /// 0-based host PDF page the ink overlays. `None` where the `.yjr` anchor
    /// eid resolves to no page — no KFX text layer, or a yjr/book mismatch. The
    /// ink stores anyway, surfacing in the gallery until an anchor lands.
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

/// Upsert one ink page, keyed by `(asin, container_id)` — the stable per-page
/// identity. Re-importing the same page refreshes its host anchor / book link /
/// content hash in place, never a duplicate, even as the device grows the nbk.
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

/// The nbk content sha last decoded for `(device_serial, asin)`, if that device
/// has ever synced this book's ink. The ink import compares it against the freshly
/// pulled nbk to skip an unchanged decode+render. Mirrors [`get_yjr_sync_sha`].
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
    // Drop this (device, asin)'s presence rows untouched by the pass, which the
    // device has dropped. Side-table only: the ink page it referenced stays.
    conn.execute(
        "DELETE FROM book_ink_device \
         WHERE device_serial = ?1 AND asin = ?2 AND last_seen <> ?3",
        params![device_serial, asin, now],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Annotations + last-read position (imported off the Kindle). The tables live in `migrate` outside the
// destructive reset.

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
    /// Reversible "hidden from the reader" flag (kept in the backup).
    pub hidden: bool,
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
/// `true` for a new row and `false` for one the library holds, which an import
/// counts as a duplicate.
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

/// Whether a device has ever shown us a coloured highlight.
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
/// `ON CONFLICT(dedup_hash) DO NOTHING`, and a created annotation can collide
/// with a passage the library holds from a Kindle.
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
        // Tombstone it so the additive device sync won't re-add it (Restore from
        // device clears this).
        record_deletion(conn, DELETION_ANNOTATION, &h)?;
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

/// Record a Sidle-side deletion so the additive sync won't re-add this artifact.
pub fn record_deletion(conn: &Connection, kind: &str, key: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO artifact_deletions (kind, key, deleted_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(kind, key) DO UPDATE SET deleted_at = excluded.deleted_at",
        params![kind, key, now_iso()],
    )?;
    Ok(())
}

/// Whether the user deleted this artifact in Sidle (so sync must not re-add it).
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

/// Clear every deletion record. "Restore from device" un-suppresses the
/// Sidle-side deletions, and a device's remaining items re-import. Returns how
/// many records cleared.
pub fn clear_all_deletions(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM artifact_deletions", [])
}

/// Set the reversible "hidden from the reader" flag on one annotation. Hidden
/// rows stay in the backup; the reader just doesn't paint or default-list them.
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

/// Drop a device's import checkpoints (`yjr_sync` + `ink_sync`), and the next
/// sync re-pulls everything. "Restore from device" calls it, bypassing the
/// unchanged fast-path for a Sidle-deleted item the device holds.
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

/// Delete one ink page by id: removes the row, its device-presence rows, and
/// tombstones it (so a re-sync won't re-add it; Restore clears it). Returns its
/// `(asin, container_id)` so the caller can drop the cached SVGs.
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

    // Drop this (device, book)'s presence rows untouched by the pass, which the
    // device has dropped. Side-table only: the annotation it referenced stays.
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
    /// The mount root inside `source` — the directory `extensions/` sits in.
    /// One source holds several apps, and `source` alone names no tree.
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
    #[test]
    fn progress_fraction_clamps_a_position_past_the_end() {
        // A stored KFX differing from the build the device read puts
        // `linear_pos` beyond `max_position`.
        assert_eq!(super::progress_fraction(147665, 147652), Some(1.0));
        assert_eq!(super::progress_fraction(-5, 100), Some(0.0));
    }

    #[test]
    fn the_finished_count_takes_both_routes() {
        let conn = fresh_db();
        // A distinct start per book: `reading_sessions` ignores a duplicate row.
        let mut hour = 9;
        let mut read = |sha: &str, title: &str| {
            let id = insert_minimal(&conn, sha, title);
            let from = format!("{hour:02}:00:00");
            let to = format!("{hour:02}:30:00");
            insert_reading_session(&conn, &sitting("2026-08-11", &from, &to, 1800, id)).unwrap();
            hour += 1;
            id
        };
        // `done` sits at the end of its axis.
        let done = read("sha-c1", "Done");
        set_max_position(&conn, done, Some(1000)).unwrap();
        set_reading_position(&conn, done, Some(9), Some(0), Some(1000), "device", "G").unwrap();
        // `marked` sits short of it, carrying a mark.
        let marked = read("sha-c2", "Marked");
        set_max_position(&conn, marked, Some(1000)).unwrap();
        set_reading_position(&conn, marked, Some(9), Some(0), Some(620), "device", "G").unwrap();
        set_book_finished(&conn, marked, true).unwrap();
        // `open` sits short of it, carrying none.
        let open = read("sha-c3", "Open");
        set_max_position(&conn, open, Some(1000)).unwrap();
        set_reading_position(&conn, open, Some(9), Some(0), Some(620), "device", "G").unwrap();

        assert_eq!(reading_book_count(&conn).unwrap(), 3);
        assert_eq!(reading_finished_count(&conn).unwrap(), 2);
        // `set_book_finished` off drops `marked` back out.
        set_book_finished(&conn, marked, false).unwrap();
        assert_eq!(reading_finished_count(&conn).unwrap(), 1);
    }

    #[test]
    fn a_mark_finishes_a_book_the_axis_does_not() {
        // `finished_at` set on a book the axis leaves at 0.62.
        assert!(!super::is_finished(Some(0.62), None));
        assert!(super::is_finished(Some(0.62), Some("2026-08-30T00:00:00Z")));
        // A book with no axis is markable.
        assert!(super::is_finished(None, Some("2026-08-30T00:00:00Z")));
        assert!(!super::is_finished(None, None));
    }

    #[test]
    fn the_end_of_the_axis_finishes_a_book_unmarked() {
        assert!(super::is_finished(Some(1.0), None));
        // `is_finished` rounds as the reading log's percentage does.
        assert!(super::is_finished(Some(0.9986), None));
        assert!(!super::is_finished(Some(0.99), None));
    }

    #[test]
    fn progress_fraction_needs_an_axis() {
        assert_eq!(super::progress_fraction(500, 0), None);
        assert_eq!(super::progress_fraction(500, 1000), Some(0.5));
    }

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
        // First call mints the identity from the fresh KFX bytes.
        set_kfx_path_and_sha(&conn, id, "books/src-sha/Book.kfx", "aaaa1111").unwrap();
        assert_eq!(
            get_book(&conn, id).unwrap().unwrap().kfx_sha256.as_deref(),
            Some("aaaa1111")
        );
        // A reconvert (or cover swap) re-runs the setter with the NEW output hash.
        // The path may be rewritten, but the identity is frozen — the on-device
        // filename embeds it and the Kindle binds each `.sdr` to that exact name.
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
        // Two basenames differing only where one has `_`: without escaping, LIKE
        // treats `_` as "any char" and could mis-link them.
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
        // Stand-ins for pre-v10 rows: `insert_book` writes the language verbatim
        // (harmonization lived only in the import layer), so these mimic old data
        // stored before the canonicalization existed.
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

        // Re-open: migrate() is idempotent and backfills NULL romaji from the raw
        // title/author via the engine.
        migrate(&conn).expect("re-migrate");

        let book = get_book(&conn, id).unwrap().unwrap();
        assert_eq!(
            book.title_romaji, "sekai",
            "kakasi backfilled the title romaji"
        );
        // The derived search key folds the backfilled romaji in.
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
        // imported_at must NEVER change on re-import — it's the first-import
        // bookkeeping time (and the list-sort tiebreaker).
        assert_eq!(row.imported_at, "2026-06-01T10:00:00Z");
        // Everything else reflects the re-import.
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

        // An edit (new content + a newer mtime) must NOT rewrite the title.
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
        // A later edit must leave the user's chosen title intact.
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
        // The import "unchanged" fast path uses backfill_notebook_default_title
        // directly (it never calls upsert_notebook), so cover it too.
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
        // A real title is left untouched by the backfill.
        rename_notebook(&conn, row.id, "Kept").unwrap();
        backfill_notebook_default_title(&conn, "uuid-u", "2026-06-09T19:30:00").unwrap();
        let row = get_notebook_by_uuid(&conn, "uuid-u").unwrap().unwrap();
        assert_eq!(row.title, "Kept");
    }

    #[test]
    fn default_notebook_title_handles_naive_and_rfc3339() {
        // Naive local wall-clock (the common import shape) reflects its digits,
        // dropping seconds — deterministic regardless of the host timezone.
        assert_eq!(
            default_notebook_title("2026-06-01T09:05:45"),
            "2026-06-01 09:05"
        );
        // RFC 3339 (the now_iso fallback) parses + converts to local without
        // panicking; the exact minute depends on the host tz, so assert shape.
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
        // A device asserts it → a row lands in `annotation_device`.
        record_device_book_presence(
            &conn,
            "DEV",
            book_id,
            &["k1".to_string()],
            "2026-05-25T00:00:01Z",
        )
        .expect("record presence");

        remove_book(&conn, book_id).expect("remove");

        // The annotation is destroyed outright — not moved to the orphan inbox.
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
        // ...and its per-device presence row is gone with it.
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

        // Sidle's own row AND a separate row PER device all coexist — composite PK
        // `(book_id, source, device_serial)`, so none clobbers another.
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

        // A second write for the SAME (source, serial) overwrites just that row…
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

        // A different device keeps its own checkpoint — no clobber.
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

        // The device's set holds `keep` alone. Sidle is a backup, and `gone`
        // stays.
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

        // Fresh books have no catalogue ASIN, so the column is free.
        assert_eq!(
            book_id_with_amazon_asin(&conn, "B07PXGQC1Q", a).expect("query"),
            None
        );

        set_amazon_asin(&conn, a, Some("B07PXGQC1Q")).expect("set asin a");
        assert_eq!(
            get_book(&conn, a).unwrap().unwrap().amazon_asin.as_deref(),
            Some("B07PXGQC1Q")
        );

        // Another book asking for the same ASIN finds the collision.
        assert_eq!(
            book_id_with_amazon_asin(&conn, "B07PXGQC1Q", b).expect("query"),
            Some(a)
        );
        // ...but the owner itself is excluded (re-saving the same ASIN is fine).
        assert_eq!(
            book_id_with_amazon_asin(&conn, "B07PXGQC1Q", a).expect("query"),
            None
        );

        // The file's own key is a separate column and untouched by any of it.
        assert_eq!(get_book(&conn, a).unwrap().unwrap().asin, None);
        set_amazon_asin(&conn, a, None).expect("clear");
        assert_eq!(get_book(&conn, a).unwrap().unwrap().amazon_asin, None);
    }

    #[test]
    fn the_migration_lifts_a_catalogue_asin_out_of_the_file_key() {
        // An install from before the split: `asin` held whatever the export
        // stamped, which for a store-bought source was the catalogue ASIN.
        // That value is the only colour-cover key the library has.
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
        assert!(!apply_bulk_patch(&conn, 9999, &BulkMetadataPatch::default()).expect("apply"));
    }

    /// `epub_path` stores relative and reads back absolute, across a `root`
    /// that moves.
    #[test]
    fn paths_stored_relative_resolved_absolute_and_migrated() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Canonicalize: a macOS tempdir resolves /var → /private/var, which
        // SQLite reports through `conn.path()`. Matching it holds the test on a
        // non-symlinked root.
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

            // Read-modify-write invariant: feeding the resolved ABSOLUTE path back
            // into the setter must NOT re-absolutize the column. set_cover,
            // recrawl and the worker all do exactly this.
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
        // Notebooks are managed the same way.
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
        // A foreign path with no managed component is left untouched.
        assert_eq!(
            relativize_for_store(Some(live), "/Users/x/Pictures/my-cover.png"),
            "/Users/x/Pictures/my-cover.png",
        );
    }

    /// End-to-end: a row whose paths are absolute under an old, case-mismatched
    /// root (exactly the on-disk state after a relocate that predates this fix)
    /// is healed on the next `open` so it resolves under the new live root.
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

        // Reopen at the live root: migration relativizes, and a read resolves
        // under this root, where the relocate put the files.
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

    /// A Colorsoft names a highlight's colour with a bare string in the slot a
    /// note body also uses, so an earlier reader filed it as the note. The
    /// repair must move it and leave a genuine note alone.
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
        row("d", 4, Some("blue"), Some("yellow")); // already coloured — hands off

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
        // Same passage, same covered text — only the linear scale differs: the
        // reader's synthesized axis vs the pid the device wrote.
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

        // The device's presence record followed the survivor, past the row it
        // was attached to.
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM annotation_device WHERE dedup_hash = ?1",
                params![kept.dedup_hash],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "presence re-keyed onto the survivor");

        // The tombstone names the passage it was written for, and the next
        // device sync leaves it deleted.
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

    /// The state a sync leaves behind landing *before* the re-key: the new
    /// rule's hash sits on a row newer than the one it collapses into.
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
        // What the import path computes today — the hash the re-key will land on.
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
        // The fused row the pre-split reader wrote, carrying the hash a v12 DB
        // holds for it. This test measures the split.
        let fused = hash("note", "test on sidle");
        insert_annotation(
            &conn,
            &span(book, &fused, "note", Some("test on sidle"), "sidle"),
        )
        .unwrap();
        // ...and the device's highlight record for the same passage, which had
        // nothing to collide with and so came back as its own row.
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
        // And they group, which is what the reader renders from.
        assert_eq!(
            crate::library::notes::attachments(&rows),
            vec![(note.id, hl.id)],
        );

        // Idempotent: re-running finds the work done and changes nothing.
        migrate(&conn).unwrap();
        assert_eq!(list_annotations_for_book(&conn, book).unwrap().len(), 2);
    }

    /// The same split with no device `highlight` present mints one and keeps
    /// the `note`.
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
        // The hash a v12 DB holds for this record. The assertion below measures
        // the split leaving it alone.
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

        // First sync: the device asserts both pages.
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

        // Next sync: c1 erased on the device. Its presence row goes, but the ink
        // BACKUP page is kept (Sidle is the durable backup).
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
        // Visible: the painter sees it (host_pages + on_page) and it's in the panel.
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
        // A device asserts the page + records a decode checkpoint.
        record_ink_device_presence(&conn, "DEV", "AS1", Some(book), &["c0".to_string()], "t1")
            .unwrap();
        set_ink_sync_sha(&conn, "DEV", "AS1", "nbksha", "t1").unwrap();

        remove_book(&conn, book).unwrap();

        // Ink, its per-device presence, and its checkpoint are all destroyed —
        // nothing is left orphaned.
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
        // The DB is queryable after compaction.
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
        // A re-key changes the identity the device names its `.notebooks/<asin>!!PDOC!!`
        // dir after, so ink collected under the old one has to come along.
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

        // A no-op key change leaves everything where it is.
        relink_ink(&conn, "", "IRRELEVANT").expect("empty old key");
        assert_eq!(list_book_ink(&conn, book).unwrap().len(), 1);
    }

    fn session(day: &str, end_position: i64, seconds: i64) -> ReadingSession {
        ReadingSession {
            device_serial: "DEV".into(),
            started_at: format!("{day}T20:00:00"),
            ended_at: format!("{day}T21:00:00"),
            day: day.into(),
            end_position,
            book_id: None,
            seconds,
            page_turns: 40,
            words: 9000,
            start_counter_ms: Some(0),
            end_counter_ms: Some(seconds * 1000),
            start_words: Some(0),
            end_words: Some(9000),
            measure: Default::default(),
            tz_offset_s: None,
        }
    }

    /// A session with an explicit clock window, for [`reading_clock`].
    fn sitting(day: &str, from: &str, to: &str, seconds: i64, book: i64) -> ReadingSession {
        ReadingSession {
            started_at: format!("{day}T{from}"),
            ended_at: format!("{day}T{to}"),
            book_id: Some(book),
            ..session(day, 1, seconds)
        }
    }

    #[test]
    fn the_clock_reports_the_hours_the_log_recorded_not_the_ones_it_could_guess() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-hours", "A book");
        // A sitting two hours wide whose reading was nearly all in the first
        // hour. Nothing about the row says so — only the log's own intervals did,
        // and this is where they were kept.
        let row = ReadingSession {
            started_at: "2026-08-11T20:00:00".into(),
            ended_at: "2026-08-11T22:00:00".into(),
            book_id: Some(book),
            ..session("2026-08-11", 1, 3000)
        };
        insert_reading_session(&conn, &row).unwrap();
        record_session_hours(&conn, &row, &[(20, 2900), (21, 100)]).unwrap();

        let cells = reading_clock(&conn).unwrap();
        let at = |h: u8| cells.iter().find(|c| c.hour == h).map_or(0, |c| c.seconds);
        // Spreading the window reports 1500 s in each hour.
        assert_eq!((at(20), at(21)), (2900, 100));
        // Counted once: a session with recorded hours must not also be spread.
        assert_eq!(cells.iter().map(|c| c.seconds).sum::<i64>(), 3000);
        assert_eq!(
            reading_days(&conn, "0000-00-00", "9999-99-99").unwrap(),
            vec![("2026-08-11".to_string(), 3000)],
            "and the clock still agrees with the calendar beside it",
        );

        // Clearing takes the hours with the sessions, past the next row id to
        // adopt them.
        clear_reading_log(&conn).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM reading_session_hours", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn a_sitting_without_recorded_hours_is_spread_over_the_ones_it_covered() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-clock", "A book");
        // The fallback, for rows stored before the parser kept its intervals.
        insert_reading_session(
            &conn,
            &sitting("2026-08-11", "20:30:00", "22:30:00", 5400, book),
        )
        .unwrap();
        let cells = reading_clock(&conn).unwrap();
        let at = |h: u8| cells.iter().find(|c| c.hour == h).map_or(0, |c| c.seconds);
        assert_eq!((at(20), at(21), at(22)), (1350, 2700, 1350));
        // Nothing may be lost to the division: the hours of a day have to add
        // back up to what the heatmap reports for it.
        assert_eq!(cells.iter().map(|c| c.seconds).sum::<i64>(), 5400);
        // 2026-08-11 is a Tuesday.
        assert!(cells.iter().all(|c| c.dow == 2 && c.month == "2026-08"));
    }

    #[test]
    fn a_row_too_wide_to_be_a_sitting_is_not_smeared_across_the_night() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-legacy", "A book");
        // One stored session covering a whole day and a bit, several sittings
        // glued together. Even spreading reports reading in every hour,
        // including 04:00.
        insert_reading_session(
            &conn,
            &ReadingSession {
                started_at: "2026-08-11T23:38:00".into(),
                ended_at: "2026-08-12T23:16:28".into(),
                book_id: Some(book),
                ..session("2026-08-11", 1, 3600)
            },
        )
        .unwrap();
        let cells = reading_clock(&conn).unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!((cells[0].hour, cells[0].seconds), (23, 3600));
    }

    #[test]
    fn a_sitting_past_midnight_keeps_its_own_hours_and_its_own_day() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-midnight", "A book");
        insert_reading_session(
            &conn,
            &ReadingSession {
                started_at: "2026-08-11T23:30:00".into(),
                ended_at: "2026-08-12T00:30:00".into(),
                book_id: Some(book),
                ..session("2026-08-11", 1, 3600)
            },
        )
        .unwrap();
        let cells = reading_clock(&conn).unwrap();
        // The hours are the real clock hours, on both sides of midnight...
        let hours: Vec<u8> = cells.iter().map(|c| c.hour).collect();
        assert_eq!(hours, vec![0, 23]);
        // Both belong to the day the session began, summing to what
        // `reading_days` gives for that day, and landing in no day the heatmap
        // draws empty.
        assert!(cells.iter().all(|c| c.month == "2026-08" && c.dow == 2));
    }

    #[test]
    fn a_day_shape_sums_to_what_the_calendar_reports_for_that_day() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-shape", "A book");
        // One session with `reading_session_hours`, one without, same `day`.
        let kept = ReadingSession {
            started_at: "2026-08-11T20:00:00".into(),
            ended_at: "2026-08-11T22:00:00".into(),
            book_id: Some(book),
            ..session("2026-08-11", 1, 3000)
        };
        insert_reading_session(&conn, &kept).unwrap();
        record_session_hours(&conn, &kept, &[(20, 2900), (21, 100)]).unwrap();
        insert_reading_session(
            &conn,
            &sitting("2026-08-11", "08:30:00", "09:30:00", 1800, book),
        )
        .unwrap();

        let shapes = reading_day_hours(&conn, "0000-00-00", "9999-99-99").unwrap();
        assert_eq!(shapes.len(), 1);
        let day = &shapes[0];
        assert_eq!(day.day, "2026-08-11");
        assert_eq!(day.hours.len(), 24);
        assert_eq!((day.hours[20], day.hours[21]), (2900, 100));
        assert_eq!(day.hours[8] + day.hours[9], 1800);
        // `hours` sums to what `reading_days` reports for the same day.
        let total: i64 = day.hours.iter().sum();
        assert_eq!(
            total,
            reading_days(&conn, "0000-00-00", "9999-99-99").unwrap()[0].1
        );
        assert_eq!(day.unplaced_seconds, 0);
    }

    #[test]
    fn a_row_too_wide_to_be_a_sitting_reports_its_seconds_as_unplaced() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-wide", "A book");
        insert_reading_session(
            &conn,
            &ReadingSession {
                started_at: "2026-08-11T23:38:00".into(),
                ended_at: "2026-08-12T23:16:28".into(),
                book_id: Some(book),
                ..session("2026-08-11", 1, 3600)
            },
        )
        .unwrap();
        let shapes = reading_day_hours(&conn, "0000-00-00", "9999-99-99").unwrap();
        // `seconds` lands on one hour, and `unplaced_seconds` states it.
        assert_eq!(shapes[0].hours[23], 3600);
        assert_eq!(shapes[0].unplaced_seconds, 3600);
    }

    #[test]
    fn progress_needs_both_an_extent_and_a_position() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-prog", "A book");
        // No `max_position`, no `reading_position`.
        assert!(book_progress(&conn, book).unwrap().is_none());
        // `max_position` without `reading_position`.
        set_max_position(&conn, book, Some(1000)).unwrap();
        assert!(book_progress(&conn, book).unwrap().is_none());
        set_reading_position(&conn, book, Some(2), Some(0), Some(250), "sidle", "").unwrap();
        let p = book_progress(&conn, book).unwrap().unwrap();
        assert_eq!((p.linear_pos, p.max_position), (250, 1000));
    }

    #[test]
    fn a_position_past_the_axis_is_reported_as_stored() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-drift", "A book");
        // `linear_pos` past `max_position`, as a differing build stores it.
        set_max_position(&conn, book, Some(146_732)).unwrap();
        set_reading_position(
            &conn,
            book,
            Some(9),
            Some(0),
            Some(146_736),
            "device",
            "GN43",
        )
        .unwrap();
        let p = book_progress(&conn, book).unwrap().unwrap();
        assert!(p.linear_pos > p.max_position);
        assert_eq!(p.source, "device");
    }

    #[test]
    fn sittings_come_back_earliest_first_with_their_book() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-sit", "A book");
        insert_reading_session(
            &conn,
            &sitting("2026-08-11", "21:00:00", "21:30:00", 1800, book),
        )
        .unwrap();
        insert_reading_session(
            &conn,
            &sitting("2026-08-11", "08:00:00", "08:20:00", 1200, book),
        )
        .unwrap();
        // A session with no `book_id` is absent from the result.
        insert_reading_session(&conn, &session("2026-08-11", 99, 600)).unwrap();

        let rows = reading_sessions_on(&conn, "2026-08-11", "2026-08-11").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].started_at, "2026-08-11T08:00:00");
        assert_eq!(rows[0].title, "A book");
        assert_eq!(rows[1].seconds, 1800);
    }

    /// Every session in the library, oldest first, as
    /// `(day, started_at, ended_at, seconds)`.
    fn stored_sessions(conn: &Connection) -> Vec<(String, String, String, i64)> {
        let mut stmt = conn
            .prepare("SELECT day, started_at, ended_at, seconds FROM reading_sessions ORDER BY started_at")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap();
        rows.map(Result::unwrap).collect()
    }

    #[test]
    fn v19_cuts_a_stored_session_at_the_midnight_it_crossed() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-v19", "A book");
        // A pre-v19 row: one sitting that ran from late evening into the small
        // hours, counted wholly to the evening. Three quarters of its clock fell
        // before midnight.
        insert_reading_session(
            &conn,
            &ReadingSession {
                started_at: "2026-08-11T23:00:00".into(),
                ended_at: "2026-08-12T00:20:00".into(),
                book_id: Some(book),
                seconds: 4000,
                page_turns: 40,
                words: 8000,
                ..session("2026-08-11", 1, 4000)
            },
        )
        .unwrap();

        conn.pragma_update(None, "user_version", 18).unwrap();
        migrate(&conn).unwrap();

        assert_eq!(
            stored_sessions(&conn),
            vec![
                (
                    "2026-08-11".into(),
                    "2026-08-11T23:00:00".into(),
                    "2026-08-11T23:59:59".into(),
                    3000
                ),
                (
                    "2026-08-12".into(),
                    "2026-08-12T00:00:00".into(),
                    "2026-08-12T00:20:00".into(),
                    1000
                ),
            ],
        );
        // The night sits on the night's own day, to the second.
        let days = reading_days(&conn, "0000-00-00", "9999-99-99").unwrap();
        assert_eq!(days.iter().map(|(_, s)| s).sum::<i64>(), 4000);

        // Running again changes nothing: the rows cross no midnight.
        conn.pragma_update(None, "user_version", 18).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(stored_sessions(&conn).len(), 2);
    }

    #[test]
    fn v25_widens_a_window_narrower_than_the_reading_it_counts() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-v25", "A book");
        // `seconds` 1562 against an `ended_at` five seconds past `started_at`.
        insert_reading_session(
            &conn,
            &ReadingSession {
                started_at: "2026-08-29T20:05:07".into(),
                ended_at: "2026-08-29T20:05:12".into(),
                book_id: Some(book),
                seconds: 1562,
                measure: crate::library::reading_log::Measure::Dwell,
                ..session("2026-08-29", 1, 1562)
            },
        )
        .unwrap();
        // `seconds` 1200 against an `ended_at` 2400 seconds past `started_at`.
        insert_reading_session(
            &conn,
            &ReadingSession {
                started_at: "2026-08-29T21:00:00".into(),
                ended_at: "2026-08-29T21:40:00".into(),
                book_id: Some(book),
                seconds: 1200,
                ..session("2026-08-29", 2, 1200)
            },
        )
        .unwrap();

        conn.pragma_update(None, "user_version", 24).unwrap();
        migrate(&conn).unwrap();

        assert_eq!(
            stored_sessions(&conn),
            vec![
                (
                    "2026-08-29".into(),
                    "2026-08-29T20:05:07".into(),
                    "2026-08-29T20:31:09".into(),
                    1562
                ),
                (
                    "2026-08-29".into(),
                    "2026-08-29T21:00:00".into(),
                    "2026-08-29T21:40:00".into(),
                    1200
                ),
            ],
        );

        // A second `migrate` leaves `ended_at` at 20:31:09.
        conn.pragma_update(None, "user_version", 24).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(
            stored_sessions(&conn)[0].2,
            "2026-08-29T20:31:09".to_string()
        );
    }

    #[test]
    fn v25_keeps_a_widened_window_inside_its_own_day() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-v25-midnight", "A book");
        insert_reading_session(
            &conn,
            &ReadingSession {
                started_at: "2026-08-29T23:50:00".into(),
                ended_at: "2026-08-29T23:50:04".into(),
                book_id: Some(book),
                seconds: 1200,
                measure: crate::library::reading_log::Measure::Dwell,
                ..session("2026-08-29", 1, 1200)
            },
        )
        .unwrap();

        conn.pragma_update(None, "user_version", 24).unwrap();
        migrate(&conn).unwrap();

        // `ended_at` stops at `day`'s last second, short of `started_at` plus
        // `seconds`.
        assert_eq!(
            stored_sessions(&conn)[0].2,
            "2026-08-29T23:59:59".to_string()
        );
    }

    #[test]
    fn a_row_spanning_two_midnights_becomes_three_days() {
        // The widest kind a row can hold: several sittings glued into one. Divided by its
        // own clock, the only thing the row still says.
        let pieces =
            split_across_midnight("2026-06-20T23:06:41", "2026-06-22T00:40:06", 360, 36, 7200);
        assert_eq!(pieces.len(), 3);
        assert_eq!(
            pieces
                .iter()
                .map(|p| p.day().to_string())
                .collect::<Vec<_>>(),
            vec!["2026-06-20", "2026-06-21", "2026-06-22"],
        );
        assert_eq!(pieces[1].started_at, "2026-06-21T00:00:00");
        assert_eq!(pieces[1].ended_at, "2026-06-21T23:59:59");
        assert_eq!(pieces[2].ended_at, "2026-06-22T00:40:06");
        // The middle day holds all 24 of its hours and takes the bulk of it.
        assert!(pieces[1].seconds > pieces[0].seconds + pieces[2].seconds);
        // Integer division loses nothing: the pieces add back up to the row.
        assert_eq!(pieces.iter().map(|p| p.seconds).sum::<i64>(), 360);
        assert_eq!(pieces.iter().map(|p| p.page_turns).sum::<i64>(), 36);
        assert_eq!(pieces.iter().map(|p| p.words).sum::<i64>(), 7200);
    }

    #[test]
    fn a_session_inside_one_day_is_not_cut() {
        assert!(
            split_across_midnight("2026-08-11T20:00:00", "2026-08-11T21:00:00", 3600, 1, 1)
                .is_empty()
        );
        // Nor is one whose stamps cannot be read, past a midnight guessed from
        // a string.
        assert!(split_across_midnight("2026-08-11", "2026-08-12", 3600, 1, 1).is_empty());
    }

    #[test]
    fn a_point_two_books_disagree_about_names_neither() {
        let a = (1566_i64, 0_i64, 12_i64);
        let b = (99_i64, 0_i64, 500_i64);
        let mut anchors = std::collections::HashMap::new();
        anchors.insert(a, 7_i64);
        assert_eq!(sole_book_at(&[a, b], &anchors), Some(7));

        // A second book claiming another of the same fingerprint's points is a
        // contradiction, not a tie-break: one of them is a coincidence and
        // nothing here can say which.
        anchors.insert(b, 9_i64);
        assert_eq!(sole_book_at(&[a, b], &anchors), None);
        assert_eq!(sole_book_at(&[], &anchors), None);
    }

    #[test]
    fn a_point_the_sidecars_know_names_a_book_the_axis_cannot() {
        let conn = fresh_db();
        // The library holds a *newer* build than the device read, so the axis
        // the device logged ends nowhere near this book's.
        let book = insert_minimal(&conn, "sha-pkd", "The Novels of Philip K. Dick");
        set_max_position(&conn, book, Some(425_357)).unwrap();
        set_reading_position(
            &conn,
            book,
            Some(4728),
            Some(0),
            Some(246_466),
            "device",
            "DEV",
        )
        .unwrap();

        insert_reading_session(&conn, &session("2026-08-11", 419_504, 60)).unwrap();
        // Nothing ends at 419505, and no tolerance is going to reach 425357.
        assert_eq!(resolve_reading_sessions(&conn).unwrap(), 0);

        // The reader stood at a point the book's own sidecar also records.
        record_log_points(&conn, 419_504, &[(4728, 0, 246_466)]).unwrap();
        assert_eq!(resolve_reading_sessions(&conn).unwrap(), 1);
        let named: i64 = conn
            .query_row("SELECT book_id FROM reading_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(named, book);
    }

    #[test]
    fn a_session_is_named_once_its_two_end_constants_are_paired() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-ends", "聖書");
        set_max_position(&conn, book, Some(9_464_649)).unwrap();

        // The archive that held this sitting never stated the book's last
        // position, so it is keyed by the last-*word* position every line
        // carries. Nothing in the library ends there.
        insert_reading_session(&conn, &session("2026-08-11", 9_464_647, 30)).unwrap();
        assert_eq!(resolve_reading_sessions(&conn).unwrap(), 0);

        // A later archive states both constants together.
        record_book_ends(&conn, &[(9_464_647, 9_464_648)]).unwrap();
        assert_eq!(resolve_reading_sessions(&conn).unwrap(), 1);

        // Re-keyed in place: the identity index counts the position, and a
        // second insert under the other constant makes a second row counting the
        // sitting twice.
        let (rows, named): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COUNT(book_id) FROM reading_sessions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((rows, named), (1, 1));
        assert_eq!(
            reading_days(&conn, "0000-00-00", "9999-99-99")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn time_on_a_book_the_library_lacks_is_counted_nowhere() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-read", "読んだ本");
        set_max_position(&conn, book, Some(1000)).unwrap();

        // One session on a book that is here, one on a fingerprint that matches
        // nothing — a book deleted since the device logged it.
        insert_reading_session(&conn, &session("2026-08-01", 999, 3600)).unwrap();
        insert_reading_session(&conn, &session("2026-08-02", 555_555, 7200)).unwrap();
        assert_eq!(resolve_reading_sessions(&conn).unwrap(), 1);

        // The orphan sits on disk, nameable the day its book comes back.
        let stored: i64 = conn
            .query_row("SELECT COUNT(*) FROM reading_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, 2);

        // …but it is in no total, on no day, and in no list.
        let days = reading_days(&conn, "0000-00-00", "9999-99-99").unwrap();
        assert_eq!(days, vec![("2026-08-01".to_string(), 3600)]);
        assert!(
            reading_books(
                &conn,
                "2026-08-02",
                "2026-08-02",
                ReadingSort::default(),
                false,
                ReadingBucket::default()
            )
            .unwrap()
            .is_empty()
        );
        let books = reading_books(
            &conn,
            "0000-00-00",
            "9999-99-99",
            ReadingSort::default(),
            false,
            ReadingBucket::default(),
        )
        .unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].book_id, book);
        assert_eq!(books[0].title, "読んだ本");
        assert_eq!(reading_book_count(&conn).unwrap(), 1);
    }

    #[test]
    fn importing_the_missing_book_names_time_already_logged() {
        let conn = fresh_db();
        // The log arrives before the book does, which is what an unattributed
        // row is kept for.
        insert_reading_session(&conn, &session("2026-08-01", 999, 3600)).unwrap();
        assert_eq!(resolve_reading_sessions(&conn).unwrap(), 0);
        assert!(
            reading_books(
                &conn,
                "0000-00-00",
                "9999-99-99",
                ReadingSort::default(),
                false,
                ReadingBucket::default()
            )
            .unwrap()
            .is_empty()
        );

        let book = insert_minimal(&conn, "sha-late", "後から来た本");
        set_max_position(&conn, book, Some(1000)).unwrap();
        assert_eq!(resolve_reading_sessions(&conn).unwrap(), 1);

        let books = reading_books(
            &conn,
            "0000-00-00",
            "9999-99-99",
            ReadingSort::default(),
            false,
            ReadingBucket::default(),
        )
        .unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].seconds, 3600);
    }

    /// The tie the automatic pass refuses to break, and the way out of it: two
    /// books of identical length end at the same position, so the reading could
    /// be either and a person says which.
    #[test]
    fn reading_two_books_could_explain_is_named_by_hand_and_can_be_unnamed() {
        let conn = fresh_db();
        let a = insert_minimal(&conn, "sha-tie-a", "花束は毒");
        let b = insert_minimal(&conn, "sha-tie-b", "恋物語");
        set_max_position(&conn, a, Some(1000)).unwrap();
        set_max_position(&conn, b, Some(1000)).unwrap();
        insert_reading_session(&conn, &session("2026-06-22", 999, 300)).unwrap();
        insert_reading_session(&conn, &session("2026-06-23", 999, 225)).unwrap();

        // Two candidates, so nothing is attributed and nothing is counted.
        assert_eq!(resolve_reading_sessions(&conn).unwrap(), 0);
        assert!(
            reading_days(&conn, "0000-00-00", "9999-99-99")
                .unwrap()
                .is_empty()
        );

        // It surfaces as one group — the position is the fingerprint, so every
        // session at it is the same book — carrying the whole of what was read.
        let groups = unmatched_reading(&conn).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].end_position, 999);
        assert_eq!(groups[0].sessions, 2);
        assert_eq!(groups[0].seconds, 525);
        assert_eq!(groups[0].first_at, "2026-06-22T20:00:00");
        assert_eq!(groups[0].last_at, "2026-06-23T21:00:00");
        assert_eq!(groups[0].devices, vec!["DEV".to_string()]);
        assert_eq!(books_with_last_position(&conn, 999).unwrap(), vec![a, b]);

        // Settled: both sessions move, and the time appears where it never was.
        assert_eq!(attribute_reading_position(&conn, 999, a).unwrap(), 2);
        let named = reading_books(
            &conn,
            "0000-00-00",
            "9999-99-99",
            ReadingSort::default(),
            false,
            ReadingBucket::default(),
        )
        .unwrap();
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].book_id, a);
        assert_eq!(named[0].seconds, 525);

        // And the question is over: the position is not asked about again.
        assert!(unmatched_reading(&conn).unwrap().is_empty());
        assert_eq!(resolve_reading_sessions(&conn).unwrap(), 0);
    }

    /// Naming takes no reading from the book holding it: the write touches
    /// unattributed rows alone.
    #[test]
    fn naming_a_position_leaves_sessions_another_book_already_holds() {
        let conn = fresh_db();
        let held = insert_minimal(&conn, "sha-held", "先に名前がついた本");
        let other = insert_minimal(&conn, "sha-other", "別の本");
        set_max_position(&conn, held, Some(1000)).unwrap();
        insert_reading_session(&conn, &session("2026-06-22", 999, 300)).unwrap();
        assert_eq!(resolve_reading_sessions(&conn).unwrap(), 1);

        // A later session at the same position, unattributed.
        insert_reading_session(&conn, &session("2026-06-23", 999, 225)).unwrap();
        conn.execute(
            "UPDATE reading_sessions SET book_id = NULL WHERE day = '2026-06-23'",
            [],
        )
        .unwrap();

        assert_eq!(attribute_reading_position(&conn, 999, other).unwrap(), 1);
        let rows: Vec<(String, Option<i64>)> = conn
            .prepare("SELECT day, book_id FROM reading_sessions ORDER BY day")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("2026-06-22".to_string(), Some(held)),
                ("2026-06-23".to_string(), Some(other)),
            ]
        );
    }

    #[test]
    fn a_books_totals_are_summed_within_the_window_asked_for() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-window", "毎日読む本");
        set_max_position(&conn, book, Some(1000)).unwrap();
        insert_reading_session(&conn, &session("2025-12-31", 999, 600)).unwrap();
        insert_reading_session(&conn, &session("2026-08-01", 999, 3600)).unwrap();
        insert_reading_session(&conn, &session("2026-08-02", 999, 1800)).unwrap();
        resolve_reading_sessions(&conn).unwrap();

        // One book throughout, with the figure differing per window. A client
        // filtering an all-time list gets a different answer.
        let day = reading_books(
            &conn,
            "2026-08-01",
            "2026-08-01",
            ReadingSort::default(),
            false,
            ReadingBucket::default(),
        )
        .unwrap();
        assert_eq!((day.len(), day[0].seconds, day[0].sessions), (1, 3600, 1));
        let year = reading_books(
            &conn,
            "2026-01-01",
            "2026-12-31",
            ReadingSort::default(),
            false,
            ReadingBucket::default(),
        )
        .unwrap();
        assert_eq!(
            (year.len(), year[0].seconds, year[0].sessions),
            (1, 5400, 2)
        );
        let ever = reading_books(
            &conn,
            "0000-00-00",
            "9999-99-99",
            ReadingSort::default(),
            false,
            ReadingBucket::default(),
        )
        .unwrap();
        assert_eq!(
            (ever.len(), ever[0].seconds, ever[0].sessions),
            (1, 6000, 3)
        );
    }

    #[test]
    fn a_window_asked_for_by_month_gives_each_month_its_own_figures() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-months", "何ヶ月も読む本");
        set_max_position(&conn, book, Some(1000)).unwrap();
        insert_reading_session(&conn, &session("2026-07-30", 999, 600)).unwrap();
        insert_reading_session(&conn, &session("2026-08-01", 999, 3600)).unwrap();
        insert_reading_session(&conn, &session("2026-08-02", 999, 1800)).unwrap();
        resolve_reading_sessions(&conn).unwrap();

        // One book, two months, and neither row carries the other's hours,
        // which is what the split happens here for.
        let (f, t) = ("2026-01-01", "2026-12-31");
        let months = reading_books(
            &conn,
            f,
            t,
            ReadingSort::default(),
            false,
            ReadingBucket::Month,
        )
        .unwrap();
        let seen: Vec<(&str, i64, i64)> = months
            .iter()
            .map(|b| (b.bucket.as_str(), b.seconds, b.sessions))
            .collect();
        assert_eq!(seen, vec![("2026-08", 5400, 2), ("2026-07", 600, 1)]);

        // The same window, undivided, is the sum of them.
        let whole = reading_books(
            &conn,
            f,
            t,
            ReadingSort::default(),
            false,
            ReadingBucket::Total,
        )
        .unwrap();
        assert_eq!((whole.len(), whole[0].seconds), (1, 6000));
        assert_eq!(whole[0].bucket, "", "an undivided window names no slice");

        // Days are the same idea one step finer, and the direction reverses the
        // slices along with the books inside them.
        let days = reading_books(
            &conn,
            f,
            t,
            ReadingSort::default(),
            true,
            ReadingBucket::Day,
        )
        .unwrap();
        let keys: Vec<&str> = days.iter().map(|b| b.bucket.as_str()).collect();
        assert_eq!(keys, vec!["2026-07-30", "2026-08-01", "2026-08-02"]);
    }

    #[test]
    fn the_default_order_is_most_recently_read_first() {
        let conn = fresh_db();
        // The long read is months old; the short one is recent.
        let old = insert_minimal(&conn, "sha-old", "先に読んだ本");
        let new = insert_minimal(&conn, "sha-new", "後で読んだ本");
        set_max_position(&conn, old, Some(1000)).unwrap();
        set_max_position(&conn, new, Some(2000)).unwrap();
        insert_reading_session(&conn, &session("2026-01-05", 999, 7200)).unwrap();
        insert_reading_session(&conn, &session("2026-08-01", 1999, 600)).unwrap();
        resolve_reading_sessions(&conn).unwrap();

        let (f, t) = ("0000-00-00", "9999-99-99");
        let all = ReadingBucket::default();
        let recent = reading_books(&conn, f, t, ReadingSort::default(), false, all).unwrap();
        assert_eq!(recent[0].book_id, new, "the newest read leads by default");

        // Every other order is offered, and every one reverses.
        let longest = reading_books(&conn, f, t, ReadingSort::Seconds, false, all).unwrap();
        assert_eq!(longest[0].book_id, old);
        let shortest = reading_books(&conn, f, t, ReadingSort::Seconds, true, all).unwrap();
        assert_eq!(shortest[0].book_id, new);
        let oldest = reading_books(&conn, f, t, ReadingSort::LastRead, true, all).unwrap();
        assert_eq!(oldest[0].book_id, old);
    }

    #[test]
    fn the_device_claims_a_session_imported_before_it_was_named() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-claim", "本");
        set_max_position(&conn, book, Some(1000)).unwrap();

        // The archive was copied off the device by hand and imported without
        // saying which device it came from.
        let mut anon = session("2026-08-01", 999, 3600);
        anon.device_serial = String::new();
        assert_eq!(insert_reading_session(&conn, &anon).unwrap(), Stored::Added);

        // The device later syncs the very same session, this time stating its
        // serial. One session was read, so one row must remain.
        let mut owned = session("2026-08-01", 999, 3600);
        owned.device_serial = "GN43H2076045001X".into();
        assert_eq!(
            insert_reading_session(&conn, &owned).unwrap(),
            Stored::Unchanged,
            "the device's copy is the same session, not a new one"
        );

        let rows: Vec<(String, i64)> = conn
            .prepare("SELECT device_serial, seconds FROM reading_sessions")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows, vec![("GN43H2076045001X".to_string(), 3600)]);
    }

    #[test]
    fn one_device_never_claims_another_devices_session() {
        let conn = fresh_db();
        let mut a = session("2026-08-01", 999, 3600);
        a.device_serial = "DEVICE-A".into();
        let mut b = session("2026-08-01", 999, 3600);
        b.device_serial = "DEVICE-B".into();
        assert_eq!(insert_reading_session(&conn, &a).unwrap(), Stored::Added);
        // Same instant, same book, different device: two devices, two sessions.
        // Only a serial-less row is up for adoption.
        assert_eq!(insert_reading_session(&conn, &b).unwrap(), Stored::Added);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM reading_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn an_unknown_sort_name_falls_back_instead_of_reaching_the_query() {
        assert_eq!(ReadingSort::from_name("seconds"), ReadingSort::Seconds);
        assert_eq!(
            ReadingSort::from_name("; DROP TABLE books--"),
            ReadingSort::LastRead
        );
    }

    #[test]
    fn an_unknown_bucket_name_falls_back_instead_of_reaching_the_query() {
        assert_eq!(ReadingBucket::from_name("month"), ReadingBucket::Month);
        assert_eq!(
            ReadingBucket::from_name("s.day; DROP TABLE books--"),
            ReadingBucket::Total
        );
    }

    #[test]
    fn the_v15_pages_column_is_renamed_without_losing_its_counts() {
        // A v15 library, created before the column was called what it measures.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            r#"CREATE TABLE reading_sessions (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                device_serial TEXT NOT NULL DEFAULT '',
                started_at    TEXT NOT NULL,
                ended_at      TEXT NOT NULL,
                day           TEXT NOT NULL,
                end_position  INTEGER NOT NULL,
                book_id       INTEGER,
                seconds       INTEGER NOT NULL,
                pages         INTEGER NOT NULL DEFAULT 0,
                words         INTEGER NOT NULL DEFAULT 0
            )"#,
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO reading_sessions
               (started_at, ended_at, day, end_position, seconds, pages, words)
             VALUES ('t0', 't1', '2026-08-01', 999, 3600, 41, 9000)",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let turns: i64 = conn
            .query_row("SELECT page_turns FROM reading_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(turns, 41);
        assert!(!has_column(&conn, "reading_sessions", "pages").unwrap());
    }
}
