//! rusqlite-backed library database.
//!
//! Single-user. The desktop app holds one `Connection` behind an `Arc<Mutex>`
//! in `AppState`; the standalone `sidle-server` daemon opens its own (per
//! request). WAL + `busy_timeout` (see [`open`]) let those two processes share
//! the file safely. rusqlite calls block, but the library workload is tiny.

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
    /// OPF `<meta name="primary-writing-mode">` value: one of `horizontal-lr`,
    /// `horizontal-rl`, `vertical-rl`, `vertical-lr`, or `None` (Auto/derive).
    /// Edited via the metadata modal's "Reading layout" control and baked into
    /// the generated KFX's `document_data.writing_mode` (the text axis); with
    /// `ppd` it also carries the page-turn. `ppd` is kept as its derived mirror,
    /// so both stay consistent and existing `ppd` readers are undisturbed.
    pub writing_mode: Option<String>,
    /// Path to the EPUB on disk. `None` while a KFX-imported book is still
    /// awaiting its background EPUB conversion.
    pub epub_path: Option<String>,
    pub cover_path: Option<String>,
    /// **Derived, not a column.** The small thumbnail sidecar at
    /// `books/<sha>/cover.thumb.jpg` (see [`super::thumbnail`]), if it's been
    /// generated. The desktop gallery prefers it over the full-res `cover_path`
    /// (~8× fewer bytes, ~15× less decode); a `None` — missing file, or a book
    /// imported before its thumbnail landed — makes the frontend fall back to
    /// `cover_path`. Kept out of SQL because it's a pure function of the live
    /// root + `sha256`, located exactly where `ensure_thumbnail` writes it.
    pub cover_thumb_path: Option<String>,
    /// **Derived, not a column.** Cache-bust token for the cover: the ms mtime
    /// of whatever image is actually served (the thumb if present, else the
    /// full cover), or 0 with no cover. Changes iff the file changes, so a
    /// recrawl / set-cover / worker color-fetch / thumbnail rebuild — each
    /// rewrites the sidecar — self-invalidates the stale image. The desktop
    /// gallery appends it as `?v=` per book;
    /// the Kindle picker folds it into its on-device cover-cache filename
    /// (`sidle/native/src/cover_cache.rs`). Computed alongside
    /// [`Self::cover_thumb_path`] from the same single stat.
    pub cover_rev: i64,
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
    /// `None` for reflowable (EPUB↔KFX) books.
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
    /// The identifier baked into the exported KFX — what the Kindle keys its
    /// catalog, `.sdr` state directory and `.notebooks/<id>!!PDOC!!` dir on, so
    /// annotation sync, ink sync, device-delete and series grouping all match
    /// on it. Synthesized from the publication identifier by
    /// `bokai::formats::kfx::metadata::resolve_export_asin`, never a catalogue
    /// value: a copy stamped with the original's ASIN is the same entry as the
    /// original as far as the device is concerned.
    pub asin: Option<String>,
    /// The real Amazon catalogue ASIN, when the book has one. Its only use is
    /// fetching the colour cover from `/images/P/<asin>` — the KFX itself ships
    /// the grayscale cover Amazon serves to monochrome devices. Never written
    /// into a file we produce; see [`Self::asin`]. Editable in the metadata
    /// modal, which is how a book that arrived without one gets a cover.
    pub amazon_asin: Option<String>,
    /// The format that arrived at import (`azw3`, `mobi`, `epub`, …).
    /// `conversion_jobs.kind` names a reconvert's direction, not this.
    pub source_format: Option<String>,
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
    /// Metadata last-edit time (ISO 8601). Stamped at insert (= `imported_at`)
    /// and bumped by the user-curation mutators; library merge uses it as the
    /// newest-wins tiebreak when the same book (by `sha256`) exists on both
    /// sides. Read as `COALESCE(updated_at, imported_at)` so a pre-v7 row that
    /// somehow escaped the backfill never reads NULL.
    pub updated_at: String,
    /// Human-readable romaji of the title — the searchable, **editable**
    /// rendering shown in the metadata modal. Rendered at import (yomigana-aware)
    /// and correctable by hand; the picker's [`search_key`] folds it in.
    /// `COALESCE(…, '')` so a pre-v11 row reads `""` rather than NULL.
    pub title_romaji: String,
    /// Human-readable romaji of the author line. See [`Self::title_romaji`].
    pub author_romaji: String,
    /// **Derived, not a column.** The canonical (space/punctuation-free,
    /// ASCII-folded) match key the Kindle picker substring-searches — assembled
    /// in [`row_to_book`] from the romaji columns + auto-romanized
    /// series/publisher/tags + the raw fields ([`super::romaji::search_key`]).
    /// Flows to the device via `/list.json` (`#[serde(flatten)]` on the server's
    /// list entry); recomputed fresh on every read so a metadata edit can't
    /// leave it stale.
    pub search_key: String,
}

/// Schema version stamped into `PRAGMA user_version` by [`migrate`]. Bump on
/// each schema change. Backups record it; restore refuses an archive whose
/// version exceeds the running app's.
///
/// v2: dropped the `My Clippings.txt` ingest path entirely — see the DELETE
/// near the end of [`migrate`].
/// v3: added `books.pdf_path` — the PDF side of a PDF↔KFX book (PDF-backed
/// container KFX).
/// v4: added the `notebooks` table — Scribe handwritten-notebook backup +
/// render.
/// v5: added `notebooks.updated_at` — the source `nbk`'s on-device mtime (the
/// Kindle's Date Modified, which only advances on a real edit), captured at
/// import. The displayed "when" for a notebook; `imported_at` is bookkeeping.
/// v6: added `book_ink` / `book_ink_device` / `ink_sync` — handwritten ink drawn
/// on a sideloaded doc (PDOC), keyed per ink page by `(asin, container_id)`.
/// v7: added `books.updated_at` — the metadata last-edit time (distinct from
/// `imported_at`, which is first-import and never moves). Stamped at insert,
/// bumped by the user-curation mutators (`update_metadata` / `apply_bulk_patch`
/// / the ASIN edit), and used by library merge's newest-wins tiebreak.
/// v8: added `artifact_deletions` — tombstones for Sidle-side deletes so the
/// additive device sync won't re-add a removed annotation / ink page / notebook
/// (Sidle is the curated backup).
/// v9: added `annotations.hidden` / `book_ink.hidden` — a reversible "hide from
/// the reader" flag (kept in the backup, never painted).
/// v10: harmonized `books.language` to canonical codes (en-US/eng → en, zh-TW →
/// zh-Hant). Data-only backfill via [`super::lang`]; no schema change.
/// v11: added `books.title_romaji` / `author_romaji` — the editable, searchable
/// romanization of title/author (see [`super::romaji`]). Backfilled from the raw
/// fields via kakasi/pinyin; new imports render them yomigana-aware. Drives the
/// picker's search.
/// v12: annotation identity no longer salts on the linear position, and a
/// highlight colour an earlier sidecar reader filed as a note body is moved to
/// `color`. Dropped the linear position from annotation identity. `dedup_hash` no
/// longer salts on `loc_start` (see [`super::ingest::annotation_dedup_hash`]);
/// existing rows are rehashed and the duplicate pairs that split — one device
/// copy, one Sidle copy of the same passage — collapse. Data-only.
/// v15: added `books.max_position` — the cached exclusive end of the book's
/// position axis. It is what lets a Kindle's own reading-session log, which
/// redacts every title, be attributed to a book: the device reports the last
/// valid position, exactly one less. Filled incrementally, never at migrate
/// time, because computing it parses the KFX.
/// v16: `reading_sessions.pages` → `page_turns`. The number was never a page
/// count — a converted book has no such thing — but the count of forward page
/// events the device logged, which depends on font size and screen. Renamed so
/// nothing downstream can present it as a book's pagination.
/// v17: added `reading_log_dumps` — which of a device's log snapshots have
/// already been read. A dump is an immutable snapshot with a unique name, so
/// having read it once is a fact, not an inference from timestamps; recording it
/// is what lets a re-import (and a device sync) skip ~90 MB of gzip unopened
/// instead of decompressing it to discover it holds nothing new.
/// v18: clears `reading_log_dumps`. v17 recorded truncated and empty files as
/// read, and a claim keyed by an immutable name is permanent — the complete copy
/// arrives under the same name and would be skipped unopened. Data-only.
/// v19: cuts stored sessions at the midnights they cross, so a night's reading
/// counts to the night it happened. Data-only — see
/// [`split_sessions_at_midnight`].
/// v20: added `reading_session_hours` — which clock hours a sitting's reading
/// actually fell in, recorded from the log's own intervals at parse time. The
/// events state it and are never offered twice; a session row keeps only a
/// window and a total, from which the hours can afterwards only be guessed at.
/// v21: added `reading_sessions.{start,end}_counter_ms` and
/// `{start,end}_words` — both ends of the device's own counters, where the row
/// previously kept only their difference. A sitting outlives the sync it began
/// in, and the events behind it are never sent twice, so a later batch can only
/// continue the run if the row says where it stopped counting (see
/// [`super::reading_log::Resume`]). Null on rows written before this, which are
/// finished sittings and need no continuation.
/// v22: added `reading_sessions.estimated` — whether `seconds` was counted by
/// the device or inferred from its awake time. A book the Kindle can count no
/// words in is never timed by it at all, so an estimate is the only figure
/// available; it must stay distinguishable from a counted one.
/// v23: added `apps` — where each on-device app's mount tree comes from on this
/// machine. Rows hold a location and nothing else; name, version and file list
/// are read off disk on every query, because the source is a working copy that
/// a build changes without telling anyone.
pub const SCHEMA_VERSION: i64 = 23;

/// A borrowable handle to the library database.
///
/// A device sync alternates between short bursts of database work and minutes of
/// USB IO, and must never hold the connection across the slow half — the desktop
/// shares its one connection with the window, which would freeze. So the sync
/// takes a handle rather than a connection, and borrows it a moment at a time;
/// each caller passes whatever it actually holds.
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
    // WAL lets the standalone `sidle-server` daemon share this file with the
    // desktop app (the LAN-sync writer + the GUI). `busy_timeout` makes a second
    // concurrent writer wait for the lock instead of failing immediately with
    // SQLITE_BUSY; writes are idempotent (UNIQUE dedup_hash, per-device
    // checkpoints), so serialized contention converges rather than corrupts.
    conn.pragma_update(None, "busy_timeout", 5000)?;
    migrate(&conn)?;
    relativize_existing_paths(&conn, path)?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Path portability. The three `*_path` columns are stored ROOT-RELATIVE
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

/// Relativize a managed path to root-relative for storage.
///
/// Fast path: strip the current `root` prefix. Fallback: a managed file ALWAYS
/// lives at `<some-root>/books/<sha>/…` (or `<some-root>/notebooks/<uuid>/…`), so
/// when the value was written under a *different* root — a library relocated to a
/// new folder, or the legacy lowercase `…/sidle/…` root that `strip_prefix` can't
/// match against today's case-corrected `…/Sidle` — slice from the last
/// `books`/`notebooks` component instead. Without this, such a path stays
/// absolute and dangles the moment the library moves — portability defeated.
///
/// A path with no managed component (a foreign cover, say) and a `None` root
/// (in-memory test conn) are stored unchanged. Idempotent: an already-relative
/// input keeps its `books/<sha>/…` tail.
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

/// The `books/<sha>/…` (or `notebooks/<uuid>/…`) tail of a managed path, taken
/// from its LAST `books`/`notebooks` component so a root that itself contains
/// such a component resolves to the deepest (managed) one. `None` when the path
/// has neither — i.e. it isn't a library-managed file.
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

/// One-time migration: rewrite absolute `*_path` columns (older rows stored
/// `<root>/books/<sha>/...`, including legacy rows whose `<root>` no longer
/// matches today's — a relocated library, or the old lowercase `sidle` dir) to
/// root-relative. Gated on actually finding an absolute value, so steady-state
/// opens short-circuit after the first. No-op for an in-memory DB (the path has
/// no parent). Lives in `open()` — which has the db path, hence the root — not
/// `migrate()`, which only gets the `Connection`.
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

/// Schema setup.
///
/// No production data yet, so we don't migrate — if we spot any artefact of
/// a prior schema (`source_epub_path` column on books, the `device_history`
/// table) we drop the lot and rebuild fresh from the CREATE block below,
/// which is the only source of truth.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    // The version this DB arrives at, before anything below touches it. Most
    // steps are written to be idempotent and re-run harmlessly on every open;
    // the few that rewrite existing rows in place gate on this instead.
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

    // Idempotent column adds for installs that already migrated past the
    // v1 schema. `CREATE IF NOT EXISTS` above is a no-op for an existing
    // table, so we have to ALTER out-of-band.
    if !has_column(conn, "books", "asin")? {
        conn.execute("ALTER TABLE books ADD COLUMN asin TEXT", [])?;
    }
    if !has_column(conn, "books", "amazon_asin")? {
        conn.execute("ALTER TABLE books ADD COLUMN amazon_asin TEXT", [])?;
        // `asin` used to hold whichever the export stamped, which for a book
        // converted from a store-bought source was the catalogue ASIN itself.
        // Those are the rows with a real Amazon shape — 10 uppercase
        // alphanumerics — and the value is worth keeping: it is the only
        // colour-cover key the library has. The file still carries it too
        // until it is re-keyed, which is what leaves `asin` alone here.
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
    // Reading layout / writing mode, editable in the metadata modal (the axis the
    // generated KFX bakes into `document_data.writing_mode`; `ppd` mirrors its
    // page-turn). NULL = Auto/derive, which is what every row predating the
    // column carries.
    if !has_column(conn, "books", "writing_mode")? {
        conn.execute("ALTER TABLE books ADD COLUMN writing_mode TEXT", [])?;
    }
    // v15: the exclusive end of the book's position axis, cached because
    // deriving it means parsing the whole KFX (seconds per book, minutes across
    // a library). NULL means "not computed yet" — no book legitimately has a
    // NULL extent — so [`books_missing_max_position`] can fill it incrementally.
    // A new book is measured as its KFX is produced, because a row that waits
    // for the sweep is a book whose reading is attributed to nothing in the
    // meantime. Devices report the *last valid* position, one less than this;
    // see `max_position_matches`.
    if !has_column(conn, "books", "max_position")? {
        conn.execute("ALTER TABLE books ADD COLUMN max_position INTEGER", [])?;
    }
    // v7: metadata last-edit time. Seed existing rows from `imported_at` so the
    // column is never NULL in practice (newest-wins reads still COALESCE as a
    // belt-and-braces). New rows are stamped by `insert_book`.
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
        // The two directions whose arriving format the job kind does state. An
        // `epub_to_kfx` row stays NULL: its EPUB is either the file that
        // arrived or one an `.azw3`/`.mobi`/`.zip` import exported, and the
        // arriving file is not kept.
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

    // Scribe handwritten notebooks (.nbk → SVG). ADDITIVE and precious: created
    // here and never part of the destructive reset above. Keyed by the device
    // `.notebooks/<uuid>/` dir name; files live under `notebooks/<uuid>/`. Title
    // is user-editable (titles are cloud-only, so there's no offline source);
    // `nbk_sha256` change-detects an edited notebook so re-import re-extracts.
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

    // v6: handwritten ink drawn ON a sideloaded doc (PDOC), one row per drawn
    // page. ADDITIVE and precious: created here, never part of the destructive
    // reset. Identity is `(asin, container_id)` — the book's baked content_id
    // plus the ink notebook's per-page container kfx_id — which is STABLE as the
    // device grows the nbk's `local_delta_fragments` (a new page adds a row; it
    // never re-keys old ones). `book_id` is nullable with ON DELETE SET NULL so
    // removing a book unlinks its ink instead of destroying it (relink by asin),
    // mirroring `annotations`. `host_page`/`host_eid`/`host_linear` come from the
    // `.yjr` `handwritten_note` anchor: the host PDF page the ink overlays, the
    // anchor eid, and the device linear position (the display sort across pages).
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

    // Which devices currently assert each ink page — the ink analogue of
    // `annotation_device`, PROVENANCE only. A page erased on the Scribe drops out
    // of the nbk's `document_data`, so on re-sync the device asserts fewer
    // `container_id`s and its stale presence rows are dropped — but the `book_ink`
    // backup row is kept (Sidle is the durable backup). Keyed per ink page, per
    // device. Additive; never reset.
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

    // Per-(device, asin) content checkpoint: the sha of the nbk last decoded, so
    // an unchanged ink notebook skips the (expensive) decode + raster re-render
    // on every connect — the exact analogue of `yjr_sync`. NOT a dedup key: row
    // identity is `(asin, container_id)`; this only short-circuits unchanged
    // pulls. When the nbk DOES change (a page added/edited), the decode runs and
    // the `(asin, container_id)` upsert keeps old pages stable while adding new.
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

    // Deletion records (tombstones). A Sidle-side delete of an annotation / ink
    // page / notebook records its stable identity here; the additive device sync
    // skips re-adding a recorded key, so a manual delete in Sidle sticks (Sidle is
    // the curated backup). "Restore from device" clears these. `key` is the
    // annotation `dedup_hash`, the ink `asin\x1fcontainer_id`, or the notebook
    // `uuid`. ADDITIVE and precious: never part of the destructive reset.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS artifact_deletions (
            kind       TEXT NOT NULL,
            key        TEXT NOT NULL,
            deleted_at TEXT NOT NULL,
            PRIMARY KEY (kind, key)
        )"#,
        [],
    )?;

    // v15: reading sessions recovered from a Kindle's own system logs.
    //
    // `end_position` is the fingerprint the device logs instead of a title; it is
    // stored permanently rather than resolved-and-discarded so a session that
    // matches no book today can be attributed later, when the missing book is
    // imported, without re-reading a single log file. `book_id` is therefore
    // nullable by design and NULL means "not yet attributable", not "bad row" —
    // but such a row counts towards nothing: every query below joins `books`, so
    // an unattributed session is inert until the day it resolves.
    //
    // Times are **device-local wall clock with no offset** — that is all the
    // syslog prefix carries, and it is also what the reader means by "what did I
    // read on Tuesday". Nothing converts them to the host's zone: a Kindle read
    // at 23:00 in Berlin says 23:00, and the day it counts to is the reader's
    // own, not Greenwich's.
    //
    // `day` is `started_at`'s date, denormalized because every view of this
    // table groups by it. [`super::reading_log::parse_sessions`] cuts a run at
    // local midnight, so a session lands in one day and each day keeps the share
    // of the crossing interval that fell inside it. Rows written before that
    // rule can still span two days and count wholly to the day they began.
    //
    // The uniqueness index is what makes re-importing safe: dumps overlap
    // heavily and the same session arrives many times over. `device_serial` is
    // `''` rather than NULL when unknown so those rows still collide instead of
    // multiplying (SQLite treats NULLs as distinct in a UNIQUE).
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
            estimated        INTEGER NOT NULL DEFAULT 0
        )"#,
        [],
    )?;
    // v16: the column was `pages`, which read as a book's pagination. It is the
    // count of forward page events the device logged — a function of font size
    // and screen, not of the book. Only a v15 table still carries the old name.
    if has_column(conn, "reading_sessions", "pages")? {
        conn.execute(
            "ALTER TABLE reading_sessions RENAME COLUMN pages TO page_turns",
            [],
        )?;
    }
    // v21: both ends of the counters the row's totals are the difference of, so
    // a sitting can be carried across the sync that interrupted it. Null on
    // every existing row, which is the honest answer — those sittings finished
    // before anything recorded where they stood.
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
    // v22: whether the row's `seconds` was counted by the device or inferred
    // from how long it was awake with the book open. The Kindle's reading timer
    // is words-and-WPM driven, so a book it can count no words in — manga,
    // magazines, fixed layout — is never timed at all, and the only alternative
    // is a bounded estimate. The two must not be added together silently, so
    // the row says which it is. Existing rows are counted: nothing before this
    // could produce an estimate.
    if !has_column(conn, "reading_sessions", "estimated")? {
        conn.execute(
            "ALTER TABLE reading_sessions ADD COLUMN estimated INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
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

    // v20: which hours of its day a sitting's reading fell in.
    //
    // Not a cache of anything. A session states a window and a total, and those
    // two cannot yield the distribution inside them: an hour spent on one
    // chapter and then a slow hour of glancing at the page are the same row.
    // Only the log's own intervals say, the parser is the only thing that ever
    // sees them, and a device never sends an event twice — so this is written at
    // parse time or it is not knowable afterwards.
    //
    // Hours, not finer: the device credits a whole interval at its far end, so a
    // page turn is the resolution the source actually carries, and an hour is
    // the finest unit anything here reports. Keyed to the session, which carries
    // the day and the book — a session cannot cross midnight, so every hour of
    // one belongs to one day.
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

    // v17: the log snapshots already read, per device.
    //
    // A `log_backup_<YYMMDDHHMMSS>.txt.gz` is an immutable snapshot with a name
    // that states when it was taken, so "we have read this one" is a fact worth
    // storing rather than something to re-derive. Keyed by device because the
    // same name means a different file on a different Kindle.
    //
    // Names, not a timestamp watermark: an archive can hold days *older* than
    // the newest session already stored — import a recent slice first, then the
    // whole folder — and a watermark would silently skip them. A name that has
    // not been seen is read, whatever its date.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS reading_log_dumps (
            device_serial TEXT NOT NULL DEFAULT '',
            name          TEXT NOT NULL,
            read_at       TEXT NOT NULL,
            PRIMARY KEY (device_serial, name)
        )"#,
        [],
    )?;

    // The two end-of-book constants a device states for one book, so the pairing
    // outlives the archive that revealed it.
    //
    // Every event line repeats the last *word* position, but only an occasional
    // `BookEndPosition` states the last position, which is what
    // [`books_with_last_position`] joins on. An archive that holds a book's
    // sessions need not hold that event — one reader stack cuts the field off
    // the line outright — and derived per-import, the pairing is then lost and
    // the book unnameable however many times it is read.
    //
    // Learned once, applied to every session since: [`resolve_reading_sessions`]
    // re-keys what it can already name. That is also why a session must never be
    // stored twice under the two constants — the identity index counts the
    // position, so the second key inserts a row rather than replacing one, and
    // the same sitting is counted twice.
    conn.execute(
        r#"CREATE TABLE IF NOT EXISTS reading_log_book_ends (
            last_word_position INTEGER PRIMARY KEY,
            from_book          INTEGER NOT NULL
        )"#,
        [],
    )?;

    // The points a log fingerprint was seen at — the evidence for naming it.
    //
    // Attribution is not decided once. A session can arrive before the sidecar
    // that names its book, and the log lines that carried it are gone by the
    // next sync: the device sends only what is newer than the last session
    // stored, so nothing already stored is ever re-parsed. Keeping only the
    // conclusion therefore froze the guess; keeping the evidence lets
    // [`resolve_reading_sessions`] re-decide on every pass, whenever it can.
    //
    // Keyed by fingerprint, not by session, because that is the unit of
    // attribution: every session at one fingerprint is the same book, so points
    // from all of them are evidence about the same question and duplicates
    // across sessions collapse.
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

    // v23: where an on-device app's mount tree is on this machine. Deliberately
    // just a location: a `local` source is a working copy whose version, files
    // and sizes change when its build.sh runs, so a cached copy of any of them
    // is wrong as soon as it is written. `root` is the mount root inside
    // `source` — one repo can hold several apps (the picker and bokai both live
    // in sidle's), so the source alone does not name which tree a row is.
    //
    // The picker's own tree is not in here. It ships with the desktop app and
    // its location is a property of how that app was built, so it is composed in
    // unconditionally rather than remembered per library.
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

    // v2: scrub any rows from the (now-removed) `My Clippings.txt` ingest path.
    // It only ever wrote orphans (`book_id IS NULL`), so this never touches a
    // linked annotation. Idempotent: a no-op on a DB that never had any. Kept
    // unconditional rather than version-gated so a stray test fixture or a
    // restored backup is cleaned up on next open.
    conn.execute("DELETE FROM annotations WHERE source = 'clippings'", [])?;

    // v6: handwritten ink belongs in `book_ink`, never the text `annotations`
    // table. An older build stored each ink record as a `Kind::Other` text row
    // (its body = the nbk container id, no covered text) — scrub those; import
    // now routes them to the ink path. Both record names a device writes, since
    // `handwritten_on_content_note` went unrecognized for longer than the other.
    // Unconditional + idempotent, like the clippings scrub above.
    conn.execute(
        "DELETE FROM annotations
          WHERE kind IN ('handwritten_note', 'handwritten_on_content_note')",
        [],
    )?;

    // v10: harmonize language tags (en-US, eng, ZH_cn, … → en / zh-Hans /
    // zh-Hant). Rewritten in Rust over the *distinct* values so the BCP-47 logic
    // lives in one place ([`super::lang`]) and only a handful of UPDATEs run.
    // Idempotent: canonical values map to themselves, so a re-open finds nothing
    // to change. A bare UPDATE (not the curation mutators), so `updated_at` is
    // left untouched — this is housekeeping, not a user edit.
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

    // v11: searchable romaji metadata. Two editable columns rendered from the
    // title/author and shown in the editor (see [`super::romaji`]). New imports
    // render them yomigana-aware (`import::extract_meta` from bokai's
    // `title_sort`/`author_sorts`); here we backfill existing rows from the raw
    // fields via the same engine. Pure CPU (no file I/O) and NULL-guarded, so
    // it's safe to re-run on every `open()` — including sidle-server's
    // per-request open: the first fill does ~1k tiny romanizations, every later
    // open finds zero NULL rows and the loop is a no-op. A bare backfill, not a
    // user edit, so `updated_at` is deliberately left untouched.
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

    // v12: re-key annotation identity without the linear position. A pre-v12
    // `dedup_hash` is salted with `loc_start`, which the two origins measure on
    // different scales, so one passage highlighted both in Sidle and on a
    // Kindle carries two hashes. Rehashing on the salt-free rule
    // ([`super::ingest::annotation_dedup_hash`]) collapses those pairs; the
    // `annotation_device` presence records and the deletion tombstones re-key
    // with them, or they point at hashes that no longer exist.
    //
    // Version-gated rather than idempotent-on-every-open. The hash is salted
    // with the book's title, so recomputing unconditionally would also re-key
    // any annotation whose book had been retitled since import — turning a
    // metadata edit into a silent re-identification of its highlights. This
    // migration re-keys the rule change and nothing else.
    if from_version < 12 {
        // Order matters: the colour repair rewrites `note_body`, which the hash
        // is computed over, so it has to land before the re-key.
        repair_colors_read_as_notes(conn)?;
        rekey_annotation_hashes(conn)?;
    }

    // v13: separate the highlight from the note the reader used to fuse into it.
    if from_version < 13 {
        split_fused_notes(conn)?;
    }

    // v14: a bookmark's identity is its start alone. `dedup_hash` used to fold
    // in the end anchor and the covered text, neither of which a bookmark
    // carries: a device repeats the start as its end and the importer fills the
    // text with a preview of the containing element, while the reader writes an
    // empty end and no text. So a bookmark made in Sidle came back off a Kindle
    // as a second row. See [`super::ingest::annotation_dedup_hash`]. The same
    // re-key as v12 — the rule changed, so every row is recomputed under it and
    // the pairs that split collapse.
    if from_version < 14 {
        rekey_annotation_hashes(conn)?;

        // Ink drawn over book content is recorded as `handwritten_on_content_note`,
        // which went unrecognized — so its host-page anchor never reached the ink
        // join and those pages stored gallery-only. The nbk itself hasn't changed,
        // and the sync skips an unchanged one, so drop the checkpoints: the next
        // sync re-decodes and this time finds the anchors.
        conn.execute("DELETE FROM ink_sync", [])?;

        // Same shape, for the capture date: an annotation's `creationTime` was
        // parsed and dropped, so every device-imported row has an empty
        // `added_at`. The sidecars still hold it, and import now backfills a
        // missing one — but only for a sidecar it actually re-reads, and an
        // unchanged one is skipped. Drop those checkpoints so the next sync
        // reads each `.yjr` once more and the dates land.
        conn.execute("DELETE FROM yjr_sync", [])?;
    }

    // v18: the first version to record read snapshots did so for files it had
    // only partly decoded — a truncated `.gz` yields its prefix, and that was
    // taken for the whole file — and for 0-byte ones, which it read as empty
    // text. Both then claimed a name that can never be re-read, since the claim
    // is the name and the name never changes. Same remedy as v14's sync
    // checkpoints: drop them all and let the next import re-read. Re-reading is
    // idempotent and costs one pass; a false claim is permanent. The claims are
    // not individually identifiable — which file was short is a fact about the
    // filesystem, not about this table — so the whole table goes.
    if from_version < 18 {
        conn.execute("DELETE FROM reading_log_dumps", [])?;
    }

    // v19: sessions stored before the parser cut a run at midnight.
    if from_version < 19 {
        split_sessions_at_midnight(conn)?;
    }

    // Stamp the schema version. migrate() always brings the DB up to the
    // latest schema, so set the current marker; backups gate restores on it.
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

    Ok(())
}

/// Split a Sidle-written `note` row back into the highlight and the note it was
/// fused from.
///
/// The reader used to record "highlight with a note" as a single row whose
/// `kind` flipped to `note`. A Kindle keeps the two apart — a highlight record
/// and a note record, grouped for display by span — so the fused row matched
/// neither: the device's *highlight* record for the same passage had nothing to
/// collide with and re-imported as a second row, and there was nowhere to put a
/// second note on one highlight.
///
/// Each such row becomes a `highlight` (keeping the row, its id, its colour and
/// its capture time, so nothing referring to it breaks) plus a new `note` row at
/// the same span carrying the body. The pair then re-keys onto exactly the
/// hashes the device's two records already compute to, which is what lets the
/// duplicates collapse.
///
/// **Device-origin `note` rows are left alone.** Those are real note records the
/// Kindle wrote; splitting one would invent a highlight the device never had and
/// push it back on the next sync.
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

        // The fused row already *is* the note, and is already keyed as one — the
        // hash folds in `kind` and the body, so it needs no rewrite and keeps its
        // id, its capture time, and anything referring to it. What the old model
        // never stored is the highlight underneath.
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
            // The device already synced that highlight back as its own row —
            // the very duplicate this split resolves. It becomes the highlight
            // the note hangs off, and inherits the colour the note was carrying
            // if it has none of its own.
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
            // Nothing to attach to yet: mint the highlight from the note's own
            // anchors, so the pair matches what a device would hold for it.
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

/// Move a highlight colour out of `note_body`, where an earlier sidecar reader
/// filed it, and into `color` where it belongs.
///
/// A colour-capable Kindle writes the colour as a bare string after the
/// annotation's template — the same shape a note body takes, which is how it
/// came to be read as one. The visible damage was highlights painting yellow
/// with their colour showing as a note beside them.
///
/// Scoped hard: only a row whose `note_body` is *exactly* one of the colours a
/// Kindle names and whose `color` is empty. A note that happens to say "blue"
/// and anything already carrying a colour are both left alone.
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

/// Recompute every annotation's `dedup_hash` under the current identity rule and
/// carry its dependents across.
///
/// Two rows can now collapse onto one hash — that is the point, it means the
/// same passage was recorded once from the device and once from Sidle's reader.
/// The earliest row survives, keeping the original capture. Rows sharing a hash
/// necessarily carry identical text (it is hashed), so there is nothing else to
/// choose between them.
///
/// `annotation_device` (presence per Kindle) and `artifact_deletions` (Sidle-side
/// tombstones) both key on the hash and are re-keyed here. Missing that would
/// silently resurrect deleted highlights on the next sync.
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

    // Duplicates go first, before any survivor is rewritten. A sync that ran
    // after the code changed but before this migration inserted the new rule's
    // hash as a fresh row — so the hash a survivor is about to take may still be
    // held by the very row it absorbs, and `dedup_hash` is UNIQUE. Rewriting
    // first fails the whole migration, and `open()` migrates: that is a locked
    // library, not a stray duplicate.
    for row in &rows {
        let survivor = winner[row.new.as_str()];
        if survivor.id == row.id {
            continue;
        }
        // Hand device presence to the survivor before dropping the row, so
        // "this Kindle has it" isn't lost. When the duplicate already carries
        // the final hash, its presence records are already keyed correctly and
        // must be left where they are — moving them would be a no-op and the
        // delete that follows would destroy them.
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

    // Then the survivors. Nothing can collide now: a survivor's new hash is
    // unique among survivors by construction, and any row that held it as a
    // stored hash was a duplicate of this same passage and is gone.
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

/// Compact the database file, returning freed pages to the OS. A `DELETE` only
/// moves pages onto SQLite's free-list — the file never shrinks on its own — so
/// a `VACUUM` after a removal is what actually reclaims the disk space. Must run
/// outside any transaction (callers hold none; [`remove_book`] commits before
/// returning). Cheap in practice: the library DB is metadata-only (book files
/// live on disk), so the file is small even for a large library.
pub fn vacuum(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("VACUUM")
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

/// Escape SQL `LIKE` metacharacters (`\`, `%`, `_`) so a value with those
/// characters matches literally under `LIKE ?1 ESCAPE '\'`. Titles routinely
/// carry `_` (e.g. `..._ A Very Short Introduction`); without escaping, `_`
/// matches any single character and can mis-link two near-identical basenames.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
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
    let pattern = format!("%/{}", like_escape(filename));
    conn.query_row(
        SELECT_BOOK_WITH_JOB_BY_KFX_FILENAME,
        params![pattern],
        |row| row_to_book(row, root.as_deref()),
    )
    .optional()
}

/// Look up the book whose `kfx_path` leaf is `<stem>.kfx`. The stable-stem
/// fallback for on-device `.sdr`/`.kfx` whose `<sha8>` infix has drifted from
/// the library's `kfx_sha256` after a desktop reconvert: the device filename
/// is frozen (the Kindle won't re-bind a renamed `.sdr`), but the basename is
/// unchanged by a reconvert, so it re-links a book fixed after it was pulled.
/// `stem` is the basename without the `.<sha8>.kfx` / `.<sha8>.sdr` suffix.
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

/// The book already sitting at `index` in series `name`, if any.
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

/// How many books other than `book_id` are in series `name` — whether that
/// series already exists as more than the one book asking.
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
//
// A Kindle's reading-session log names no book — every line reads
// `Title:<private>` — but each carries the book's last valid position. That
// integer identifies the book against this cache, so these three functions are
// the whole join surface: find what still needs computing, store a result, and
// look a device's number back up.

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

/// Books whose axis ends exactly where a device says the book's last position
/// is. **Exact equality, never a tolerance:** two conversions of one title shift
/// every position by a constant, so a near match means a superseded build, not
/// this book — and a fingerprint that matches nothing means the file is gone.
///
/// Returns every candidate rather than one, because unrelated books of the same
/// length do collide (a library of ~2 260 held 11 such pairs). More than one hit
/// is an ambiguous attribution the caller must resolve or leave unattributed —
/// picking arbitrarily would silently misfile the time.
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

/// Every device serial the library has ever recorded, from any sync surface.
///
/// The list a host-side reading-log import is chosen from: the logs never name
/// the device that wrote them, so provenance is picked from the devices Sidle
/// has actually seen rather than guessed at.
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
    /// Both ends of the device's own running counters over this sitting, in
    /// milliseconds and in words. `seconds` and `words` are their differences;
    /// the values themselves are what lets a later batch of events continue the
    /// run instead of starting a second one beside it — see
    /// [`super::reading_log::Resume`]. `None` on a row stored before the columns
    /// existed, and on a run whose events never stated a counter.
    pub start_counter_ms: Option<i64>,
    pub end_counter_ms: Option<i64>,
    pub start_words: Option<i64>,
    pub end_words: Option<i64>,
    /// True when `seconds` is how long the device was awake with the book open
    /// rather than what its own reading timer counted.
    ///
    /// The timer is words-and-WPM driven, so content it can count no words in
    /// earns no time from it — a fixed-layout magazine reads as zero on the
    /// Kindle's own book info too. An estimate is then the only figure there
    /// is, and it answers a different question from a counted one, so the two
    /// are kept apart rather than summed as if they were the same measurement.
    pub estimated: bool,
}

/// What storing one session did to the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// A sitting the library did not hold.
    Added,
    /// A sitting already held, now measured further — the reader carried on past
    /// where the last batch of events could see.
    Extended,
    /// Already held, with nothing new to say about it.
    Unchanged,
}

/// Store one session, keyed by (device, start, book).
///
/// A sitting the library already holds is **extended** rather than ignored when
/// the new measurement reaches further than the stored one. Reading does not
/// stop because a sync happened, and the same sitting is measured again — from
/// its own start, with events the last pass did not have — every time the reader
/// carries on past it. Ignoring the second measurement would freeze the row at
/// whatever the first sync happened to catch, and the rest of the sitting would
/// be shed. Only the measurement moves; `book_id` is left alone, being
/// [`resolve_reading_sessions`]'s to decide.
///
/// Never backwards: a shorter measurement of a sitting already known longer is
/// a partial view of it, not a correction, so it changes nothing.
///
/// A session already held against **no** device is claimed rather than
/// duplicated: the same reading, imported once from a copied archive of unknown
/// provenance and again from the device that wrote it, is one session, and
/// without this the serial in the identity would make it two.
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
             start_counter_ms, end_counter_ms, start_words, end_words, estimated)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"#,
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
            s.estimated,
        ],
    )?;
    if n > 0 {
        return Ok(Stored::Added);
    }
    // A counted measurement replaces an estimate whatever the two say, because
    // it answers the question the estimate was standing in for: the device
    // finally credited the sitting. The reverse never happens — an estimate
    // must not overwrite a figure the device itself counted, however much
    // longer the reader was awake with the book open.
    let extended = conn.execute(
        r#"UPDATE reading_sessions
              SET ended_at = ?4, seconds = ?5, page_turns = ?6, words = ?7,
                  start_counter_ms = ?8, end_counter_ms = ?9,
                  start_words = ?10, end_words = ?11, estimated = ?12
            WHERE device_serial = ?1 AND started_at = ?2 AND end_position = ?3
              AND (
                    (estimated = 1 AND ?12 = 0)
                 OR (estimated = ?12
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
            s.estimated,
        ],
    )?;
    Ok(if extended > 0 {
        Stored::Extended
    } else {
        Stored::Unchanged
    })
}

/// Record which hours of its day a session's reading fell in.
///
/// Called with the row the same parse produced, and only when that row was
/// actually stored: hours and totals are one measurement, and a set of hours
/// against someone else's totals would have the clock report a day the calendar
/// beside it does not.
///
/// The whole distribution, replacing whatever was there. A sitting extended by a
/// later batch of events is re-measured from its own start — the parser is
/// handed the hours already booked and rebuilds them — so the new set is the
/// complete one and an hour missing from it is an hour that does not belong to
/// the sitting.
///
/// Keyed by the session's identity rather than a row id the caller would have to
/// thread through. Silently does nothing when no such session is stored, which is
/// the right answer for the case that reaches here: hours against no session
/// would be reported nowhere and deleted by nothing.
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

/// The hours already booked against a stored session, ascending.
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

/// The newest session stored for one device — the sitting it may still be in.
///
/// Whether the reader is in fact still in it is not decided here: that depends
/// on the events, and [`super::reading_log::parse_sessions`] is what weighs them.
/// This only says which row a continuation would land on.
pub fn newest_reading_session(
    conn: &Connection,
    device_serial: &str,
) -> rusqlite::Result<Option<ReadingSession>> {
    conn.query_row(
        r#"SELECT device_serial, started_at, ended_at, day, end_position, book_id,
                  seconds, page_turns, words,
                  start_counter_ms, end_counter_ms, start_words, end_words, estimated
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
                estimated: r.get(13)?,
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
/// three counters between the pieces in proportion to the time each holds.
///
/// Empty when the window falls inside one day, or when either end is unreadable.
///
/// Even division is the only thing such a row still says about where inside
/// itself the reading happened — the same assumption [`reading_clock`] makes
/// spreading a session over the hours it covers. It is an estimate, and on a row
/// too wide to be one sitting it is a poor one; what it is not is the status quo,
/// where a night's reading counts wholly to the day before it.
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

    // Integer division loses up to a second per piece, and a reading log that
    // quietly shrinks when it is corrected is worse than one that is wrong in a
    // way you can see. The remainder goes to the day the session began, as it
    // does in [`reading_clock`].
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

/// Cut every stored session that spans more than one day at the midnights it
/// crosses (v19).
///
/// Rows written before [`super::reading_log::parse_sessions`] cut a run at
/// midnight counted a late night wholly to the evening it started in. Nothing is
/// dropped here and nothing is invented: a row's seconds, page turns and words
/// are divided between the days its own clock covered, and every one of them
/// stays in the library.
///
/// It works from the row alone, and has to. A stored session keeps its window
/// and its totals, not the events behind them, and those events are never
/// offered twice: a device sends only what is newer than the newest session
/// stored and purges its own archive at that watermark. So the division is by
/// wall clock, evenly — exact for a sitting that ran through midnight, an
/// estimate of the ratio for a row that is several sittings glued together by
/// the older gap-flag defect, where the reader was asleep across part of the
/// span. The totals are preserved either way.
///
/// The first piece keeps the row, which keeps its identity `(device_serial,
/// started_at, end_position)` intact; the rest are inserted. It is written in
/// that order deliberately: the update is what stops the row being seen as
/// crossing again, so an interrupted pass leaves work still to do rather than
/// work already double-counted, and the inserts ignore a collision instead of
/// adding to one.
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

/// Where one Kindle says it left each book, keyed by the point itself.
///
/// This is the bridge from a log to a library. A reading event names no book —
/// every line reads `Title:<private>` — but it does state where the reader was
/// standing. A `.yjr` sidecar states the same thing for a book, and the sidecar
/// sync files it under the `book_id` it belongs to, because a sidecar sits
/// beside a file whose identity is never in doubt. A point that appears in both
/// is one reader at one moment, so the log line is about that book.
///
/// Keyed on `(eid, offset, linear_pos)` rather than the coordinate alone: the
/// element id is the book's own vocabulary, and demanding all three is what
/// keeps two books that merely share a coordinate — near the front, they all do
/// — from being taken for each other.
///
/// Every Kindle's rows, not just the one being imported: a point is a point,
/// and a book read on two devices is the same book. Only points that name
/// exactly one book, though — an ambiguous one identifies nothing, and dropping
/// it costs an attribution where keeping it would invent one.
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
/// the same place on the linear axis. All three, because a bare coordinate near
/// the front of a book is one every book shares.
pub type Point = (i64, i64, i64);

/// The single book every one of these points belongs to, or `None`.
///
/// One agreement is enough, because a point carries the book's own element id
/// and is specific enough to belong to one book. Two *different* books agreeing
/// is not a stronger answer but a contradiction — one of the points is a
/// coincidence and nothing here can say which — so it names nothing.
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

/// Remember where a fingerprint's reader was seen standing.
///
/// Evidence, not a conclusion: the book these points belong to may not be
/// nameable yet, and the lines that carried them will not come round again.
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

/// Remember which last position goes with which last-word position.
///
/// First sighting wins, matching the rule the parser uses within one archive:
/// two builds of one title differ in both numbers together, so a pair is either
/// already right or about a different book entirely.
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

/// Every pairing learned so far.
pub fn known_book_ends(conn: &Connection) -> rusqlite::Result<std::collections::HashMap<i64, i64>> {
    let mut stmt =
        conn.prepare("SELECT last_word_position, from_book FROM reading_log_book_ends")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// The newest reading event this library holds for one device, or `None` if it
/// holds none.
///
/// The watermark a device syncs against: everything at or before this is already
/// stored, so the device can skip whole log dumps by their filename timestamp
/// without opening one. Per-serial, because two Kindles are read independently
/// and one being up to date says nothing about the other.
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

/// The log snapshots already read from one device, by filename.
///
/// What makes a re-import cheap: a name in this set is a file whose every event
/// is already stored, so it is skipped without being opened. Snapshots are
/// immutable, so this can never go stale in the dangerous direction.
pub fn seen_dumps(
    conn: &Connection,
    device_serial: &str,
) -> rusqlite::Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT name FROM reading_log_dumps WHERE device_serial = ?1")?;
    let rows = stmt.query_map(params![device_serial], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// Which device these log snapshots belong to, if the library has read any of
/// them before.
///
/// Exact identification, not a guess: a snapshot's name encodes the second it
/// was written, so two Kindles do not produce the same one, and a name already
/// recorded says which device wrote it. `Ok(None)` means none of these files has
/// been seen; `Err(names)` means they are recorded against **different** devices,
/// which is a folder holding two Kindles' logs mixed together.
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
/// snapshots have been read.
///
/// Both, together: keeping the read-snapshot records after deleting the sessions
/// would leave a library that refuses to re-import the very archives it no longer
/// holds. Returns how many sessions went.
pub fn clear_reading_log(conn: &Connection) -> rusqlite::Result<usize> {
    // Explicitly, not by cascade: the foreign key only fires where
    // `PRAGMA foreign_keys` is on, and a connection that opened without it would
    // leave these rows behind to be adopted by whatever row id came next.
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
/// days with nothing. Feeds the calendar heatmap.
///
/// Unattributed sessions are excluded, as they are from every query here: time
/// that cannot be traced to a book in the library is not reported as read. See
/// [`resolve_reading_sessions`] for why the rows survive anyway.
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
/// the days of one month that fall on one weekday.
///
/// Deliberately a cube rather than three flat tables. Hour-seconds are additive,
/// so every view worth drawing — the hours of a year, a weekday × hour grid, a
/// month × hour grid — is a marginal of this one set, and summing marginals
/// client-side cannot go wrong the way re-slicing a book aggregate can (see
/// [`ReadingBucket`]). One query, one payload, and switching views costs nothing.
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

/// Beyond this much wall clock, a session is not treated as a sitting whose
/// reading can be spread across it.
///
/// Sessions are cut at a half-hour silence, so a real one is a few minutes to a
/// few hours wide — the widest the parser produced over a month of one device's
/// archives was 6.2 h. Rows far past that are from before
/// [`super::reading_log::parse_sessions`] held its gap flag across lines that
/// observe nothing: several sittings glued into one row, up to 48 h wide. They
/// hold real reading time and are not re-derivable, so they are kept — but
/// spreading them evenly is what invents an hour of reading at 04:00 on a reader
/// who has never once read at 04:00. Measured on the live library: uniform
/// spreading put 0.6 h in *every* hour from 02:00 to 06:00, against a true zero.
const SPREADABLE_SPAN_SECS: i64 = 6 * 3600;

/// When reading happened, by hour of the day.
///
/// Read from `reading_session_hours` wherever a session has it: the log's own
/// intervals, booked to the hours they ran through when the events were still to
/// hand. That is measurement, and it is why this is not a view over the session
/// table.
///
/// The fallback below is for rows stored before those hours were kept, and it is
/// the reason they are kept. A session states a wall-clock window and the seconds
/// counted inside it, which are not the same number — the device credits reading,
/// not the clock — so all such a row supports is spreading its time evenly across
/// the hours it covers, which turns an hour of solid reading into a smear over
/// however long the sitting happened to last. A row too wide to be one sitting
/// ([`SPREADABLE_SPAN_SECS`]) is instead booked whole to the hour it began, which
/// is the one thing about such a row that is still a fact.
///
/// The *hour* is the true clock hour, including past midnight. The *day* is the
/// session's own day throughout, so these totals sum to exactly what
/// [`reading_days`] reports for the same window rather than drifting from it by
/// whatever crossed midnight.
pub fn reading_clock(conn: &Connection) -> rusqlite::Result<Vec<ClockCell>> {
    let mut cells: BTreeMap<(String, u8, u8), i64> = BTreeMap::new();
    let cell = |day: &str, hour: u8| -> Option<(String, u8, u8)> {
        let date = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
        Some((
            day[..7].to_string(),
            chrono::Datelike::weekday(&date).num_days_from_sunday() as u8,
            hour,
        ))
    };

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
        if let Some(key) = cell(&day, hour.min(23)) {
            *cells.entry(key).or_default() += seconds;
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
        if cell(&day, 0).is_none() {
            continue;
        }
        let key = |hour: u8| cell(&day, hour).expect("day parsed above");
        let (Some(from), Some(to)) = (clock_secs(&started_at), clock_secs(&ended_at)) else {
            continue;
        };
        // The end is past the start on the clock unless the session ran over
        // midnight, in which case it is a day further on.
        let span = if to >= from {
            to - from
        } else {
            to + 86400 - from
        };
        if span == 0 || span > SPREADABLE_SPAN_SECS {
            *cells.entry(key((from / 3600) as u8)).or_default() += seconds;
            continue;
        }
        // Whole seconds per hour, then the division's remainder to the hour the
        // session began: a rounding loss would quietly shrink the year's total
        // below what the heatmap beside it reports.
        let mut placed = 0;
        for h in (from / 3600)..=((from + span) / 3600) {
            let (lo, hi) = (h * 3600, (h + 1) * 3600);
            let overlap = (from + span).min(hi) - from.max(lo);
            if overlap <= 0 {
                continue;
            }
            let share = seconds * overlap / span;
            placed += share;
            *cells.entry(key(((h % 24) as u8).min(23))).or_default() += share;
        }
        *cells.entry(key((from / 3600) as u8)).or_default() += seconds - placed;
    }

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
///
/// Parsed from a name rather than taken as SQL: the ordering expression is
/// chosen here from a closed set, so no caller can reach the query text.
/// An unknown name is [`ReadingSort::LastRead`] — the reading log's natural
/// order, most recently read first.
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
///
/// A book's figures are a property of the span they are summed over, so a
/// grid that shows a year split into months has to ask for months — grouping an
/// all-time list client-side would print the same yearly total under every month
/// the book appears in. [`ReadingBucket::Total`] is the whole window as one
/// slice, which is what a caller wanting a single row per book asks for.
///
/// Parsed from a name from a closed set, for the same reason [`ReadingSort`] is.
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
///
/// One query for every scope the page offers: a single day is `from == to`, and
/// all time is an open range. Aggregating in SQL rather than filtering an
/// all-time list on the client is what keeps those per-window sums honest.
///
/// `bucket` subdivides the window — a year asked for by month returns each book
/// once per month it was read in, carrying that month's figures. Slices come
/// back in `asc` order and contiguously, so a caller renders them by walking the
/// rows once.
///
/// The last-read time breaks every tie, so books that match on the sort key
/// still come back newest-first rather than in whatever order the rows landed.
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
                  SUM(CASE WHEN s.estimated THEN s.seconds ELSE 0 END)
             FROM reading_sessions s JOIN books b ON b.id = s.book_id
            WHERE s.day BETWEEN ?1 AND ?2
            GROUP BY {bucket}, s.book_id
            ORDER BY {bucket} {dir}, {} {dir}, MAX(s.ended_at) DESC"#,
        sort.expr()
    ))?;
    let rows = stmt.query_map(params![from, to], |r| row_to_entry(r, root.as_deref()))?;
    rows.collect()
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
///
/// Always a real book: every query producing one joins `books`, so there is no
/// nameless variant to render.
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
    /// How much of [`Self::seconds`] the device did not count but was inferred
    /// from its awake time — see [`ReadingSession::estimated`].
    ///
    /// Reported beside the total rather than folded into it or split out of it.
    /// A reader wants one figure for how long they spent in a book, and a book
    /// the Kindle cannot time would otherwise read as never opened; but the two
    /// halves answer different questions, so which part is which stays visible.
    /// Zero for everything the device counted, which is most reading.
    pub estimated_seconds: i64,
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
}

fn row_to_entry(r: &rusqlite::Row<'_>, root: Option<&Path>) -> rusqlite::Result<ReadingEntry> {
    let sha256: String = r.get(3)?;
    let cover_path = resolve_opt(root, r.get(4)?);
    let (cover_thumb_path, cover_rev) = served_cover(root, &sha256, cover_path.as_deref());
    let devices: Option<String> = r.get(11)?;
    Ok(ReadingEntry {
        bucket: r.get(12)?,
        book_id: r.get(0)?,
        title: r.get(1)?,
        author: r.get(2)?,
        cover_path,
        cover_thumb_path,
        cover_rev,
        seconds: r.get(5)?,
        estimated_seconds: r.get(13)?,
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
    })
}

/// Attach `book_id` to every session whose fingerprint now resolves to exactly
/// one book, and return how many were newly attributed.
///
/// Run after any import — of logs *or* of books. A book imported today names the
/// sessions that were logged before Sidle had ever seen it, which is why
/// `end_position` is kept on the row. Fingerprints matching several books are
/// deliberately left alone: an ambiguous attribution is worse than none.
///
/// What never resolves is time on a book the library does not have, and that
/// time is not reading Sidle knows about: it is reported in no total, no day, no
/// list. The row is kept only as the seed for this function — import the book
/// and the time appears — never as a nameless entry to be shown or summed.
pub fn resolve_reading_sessions(conn: &Connection) -> rusqlite::Result<usize> {
    let mut stmt =
        conn.prepare("SELECT DISTINCT end_position FROM reading_sessions WHERE book_id IS NULL")?;
    let pending: Vec<i64> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    // A session stored before its book's two end constants were paired is keyed
    // by the last-word position, which matches no book. Re-key it now that the
    // pairing is known, rather than leaving it unnameable for good.
    //
    // `UPDATE OR IGNORE`: if a row already holds this sitting under the right
    // key, the stale one is a duplicate of it and stays as it is — unattributed
    // and so counted nowhere, which is the one outcome that cannot inflate a
    // total.
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
        let candidates = books_with_last_position(conn, position)?;
        let book_id = match candidates[..] {
            [only] => Some(only),
            // The axis decided nothing — either no book ends there, or several
            // do. Ask where the reader was instead: that is a different
            // question, and it answers some the axis cannot, including a book
            // the library holds in a different build than the device read.
            _ => sole_book_at(points.get(&position).map_or(&[], Vec::as_slice), &anchors),
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
///
/// Every session at one position is the same book — the position IS the
/// fingerprint — so the group, not the session, is the unit anything acts on.
/// What each group cannot say is *which* book: either no library book ends
/// there, or several do. Only the second is answerable, and by the candidates
/// rather than by anything in this struct — nothing here identifies a book.
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
///
/// The counterpart to [`resolve_reading_sessions`]: that function attributes
/// what the axis can decide alone, and this is everything it left. Two different
/// situations, which callers are expected to tell apart by asking
/// [`books_with_last_position`] about each — several books ending at the position
/// is a tie a person can settle; none ending there is a book that is not in the
/// library, which nothing and nobody can name from a duration and a date.
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
///
/// The one thing [`resolve_reading_sessions`] will not do on its own — choose
/// between books that end at the same position. It is a person's judgement, not
/// a derivation, and it is final: an attributed session is no longer `NULL`, so
/// no later pass reconsiders it and the position stops being a question.
///
/// Touches only unattributed rows, so it can never take reading away from the
/// book that already holds it.
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

/// Set the KFX path, and **mint** its content hash only if the row doesn't
/// have one yet. `kfx_sha256` is the book's permanent identity: the push
/// pipeline embeds its `<sha8>` prefix in the on-device filename, and the
/// Kindle binds each `.sdr` (annotations + reading progress) to that exact
/// name. Re-stamping the hash on a reconvert / cover-swap would rename the
/// file on the next pull and orphan the `.sdr` — so once set, it is frozen
/// (`COALESCE` keeps the existing value; only `kfx_path` is rewritten).
/// A book fixed after it was pulled keeps its identity; the improved bytes
/// reach the device under the same filename. Callers still pass the freshly
/// computed hash: it is used only to mint a first-time (NULL) row.
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
        Ok((
            r.get::<_, i64>(0)?,
            resolve_one(root.as_deref(), &r.get::<_, String>(1)?),
        ))
    })?;
    rows.collect()
}

/// Find rows with a `kfx_path` but no `asin`. Bootstrap reads each KFX's
/// metadata to recover the value bokai stamped at export time — the
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

/// Stamp the identifier bokai's KFX export wrote into the produced file —
/// fabricated from the publication identifier (32-char Crockford-Base32), so
/// the row holds it only after EPUB→KFX completes. Sidle's device-delete path
/// keys catalog-style `<title>_<ASIN>.sdr/` cleanup on this column, and the
/// picker groups series by it.
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

/// Stamp a book's metadata `updated_at` (v7). Two callers: the ASIN-edit command
/// (a user curation, but it routes through the mechanical-safe [`set_asin`],
/// which bootstrap/worker also call, so the bump can't live *inside* `set_asin`);
/// and library merge, which carries a source book's original edit time onto the
/// freshly inserted local row so a later re-merge compares correctly.
/// [`update_metadata`] / [`apply_bulk_patch`] bump it inline instead.
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

/// Return the id of another book (≠ `except_id`) that already holds
/// `amazon_asin`, if any. The metadata editor uses this to keep the catalogue
/// ASIN unique across the library: two books sharing one would fetch the same
/// cover, which means one of them is mislabelled. Only a non-empty ASIN is
/// meaningful here — callers gate on the real 10-char shape before calling.
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

/// The id of the book holding `asin`, if any. Used to (re)link handwritten ink to
/// its host book — the ink's `.notebooks/<asin>!!PDOC!!` dir name is the baked
/// content_id Sidle stamped into `books.asin`.
pub fn book_id_by_asin(conn: &Connection, asin: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM books WHERE asin = ?1 LIMIT 1",
        params![asin],
        |r| r.get(0),
    )
    .optional()
}

/// Every non-empty `books.asin` (the baked content_id). The ink collector uses
/// this set to recognize which `.notebooks/<id>!!PDOC!!` dirs are OURS — the id
/// is a content_id whose alphabet (hex vs Crockford-base32) varies per book, so
/// "is it one of our books' asins?" is the only reliable test (NOT a hex check).
pub fn book_asins(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT asin FROM books WHERE asin IS NOT NULL AND asin != ''")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
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
    /// Page progression direction: `"rtl"` | `"ltr"` | `None` (Auto). Baked into
    /// the generated KFX's reading order; a change triggers a force-reconvert.
    #[serde(default)]
    pub ppd: Option<String>,
    /// Reading layout / writing mode: `horizontal-lr` | `horizontal-rl` |
    /// `vertical-rl` | `vertical-lr` | `None` (Auto). When set it's authoritative
    /// for `ppd` (the command layer derives one from the other). Baked into the
    /// generated KFX; a change triggers a force-reconvert.
    #[serde(default)]
    pub writing_mode: Option<String>,
    pub publisher: Option<String>,
    pub published_at: Option<String>,
    pub series_name: Option<String>,
    pub series_index: Option<f64>,
    pub tags: Vec<String>,
    /// Editable romaji of the title/author (see [`super::romaji`]). The command
    /// layer trims + lowercases them, and self-heals a blank field by
    /// re-rendering it from the (canonicalized) title/author. `#[serde(default)]`
    /// so a caller that doesn't send them (older frontend) leaves them blank →
    /// self-healed, not wiped to a stale value.
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

/// Remove a book and *everything* tied to it, returning its `sha256` (so the
/// caller can delete the on-disk `books/<sha>/` dir) or `None` if the id was
/// already absent.
///
/// A removal is a FULL cascade — the book and all of its derived data go:
///   * `conversion_jobs`, `reading_position`, `yjr_sync` — cleared by their
///     `ON DELETE CASCADE` when the `books` row goes (foreign_keys is ON; see
///     [`open`]).
///   * `annotations` + `annotation_device` — its highlights/notes and their
///     per-device presence, deleted explicitly by `book_id`. Their FK is
///     `ON DELETE SET NULL` (which would merely *unlink* them into the orphan
///     inbox), and `annotation_device` has no FK at all, so neither is cleaned
///     by the cascade above — we delete them outright first.
///   * `book_ink` + `book_ink_device` + `ink_sync` — handwritten ink, its
///     per-device presence, and the per-asin decode checkpoint, all keyed by the
///     book's `asin` (the ink model's identity; see [`record_ink_device_presence`]).
///
/// The orphan inbox (`book_id IS NULL`) is intentionally left untouched: those
/// rows come from a *different* source — annotations/ink imported off a device
/// whose book isn't in the library yet — and were never associated with *this*
/// book, so re-linking on a later import still works.
///
/// All deletes run in one transaction so a removal is all-or-nothing.
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
    /// Caller passes the canonical tag list (already trimmed / lowercased
    /// / deduped). `insert_book` serializes it to a JSON array TEXT.
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

/// A position in a series is held by one book: the volume numbered 3 of a
/// series is *the* volume 3. Lets an operation that fills a series (an omnibus
/// split) recognize a place it has already filled instead of stacking a second
/// book on it.
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
/// LIKE `%/<filename>` so we ignore the `books/<sha>/` prefix the row stores.
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

/// The served cover image for a book — the color thumbnail if it's been
/// generated, else the full-res cover — as `(thumbnail path, ms mtime)`. One
/// `metadata` stat yields both the thumb's existence (→ the gallery prefers it)
/// and its mtime (→ the shared cache-bust rev); only a thumbnail-less book falls
/// back to stat'ing `cover_path` for the rev. A `None` root, or any unstattable
/// file, yields what the caller reads as "no thumb" / "no version" (`None` / 0).
///
/// Single source of truth for a cover's cache token, shared by the desktop
/// gallery ([`BookRow::cover_rev`]) and the Kindle picker's `/list.json`, so
/// both invalidate in lockstep with the file on disk.
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
    // No thumb on disk: no gallery thumb, and the rev tracks the full cover so a
    // cover swap on a thumbnail-less book still busts its `?v=`.
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

/// Millisecond mtime of the file at `path`, or 0 if it can't be stat'd. The
/// content-revision token for a served KFX (`/list.json` `kfx_rev`), the same
/// role `cover_rev` plays for the cover image: `kfx_sha256` is a FROZEN device
/// identity (the on-device filename embeds it and can't change), so a reconvert
/// that rewrites the bytes is invisible in the name — the picker detects
/// it by this mtime bump and re-downloads in place over the same filename.
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
    // Derived (not columns): the thumbnail sidecar (when present on disk) and
    // the served-image mtime cache token, from a single stat of the served
    // image. A `None` root (in-memory test conn) or a not-yet-generated thumb
    // yields `(None, ..)`, and the gallery falls back to the full cover.
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
        // Appended after the original column list rather than placed beside
        // `asin`: every reader here indexes positionally.
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
// ---------------------------------------------------------------------------

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

/// A notebook's default title: its first-import on-device "Date Modified"
/// (`updated_at`) as local `YYYY-MM-DD HH:MM` — the same string the "Updated"
/// column shows, frozen into the title so it never drifts. Scribe titles are
/// cloud-only, so this is the best offline name. `updated_at` is normally a
/// naive local-wall-clock ISO (`folder_updated_at` / the MTP DateModified),
/// whose digits we reflect as-is; on the `now_iso()` fallback it's an RFC 3339
/// instant, which we convert to local. An unparseable value degrades to a
/// best-effort 16-char slice.
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
/// ([`default_notebook_title`]); or, if its uuid already exists, update page
/// count / content hash / `updated_at`. Returns the row id.
///
/// `title` and `imported_at` are both frozen at first import: a re-import never
/// rewrites them, so an edit's newer mtime doesn't change the displayed name and
/// a user rename ([`rename_notebook`]) sticks. The lone exception is a legacy row
/// still on the old literal-'Notebook' sentinel, which is upgraded to the
/// datetime default once (it has no real title to protect).
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

/// Backfill `updated_at` for a notebook that predates the column (NULL only),
/// without re-extracting it. Used on the import "unchanged" fast path so an
/// existing row still gets its on-device Date Modified.
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

/// Upgrade a legacy notebook still on the literal-'Notebook' (or empty) title to
/// the first-import datetime default, without re-extracting it. Mirrors
/// [`backfill_notebook_updated_at`] for the import "unchanged" fast path; a
/// notebook with a real title (a datetime default or a user rename) is untouched.
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
// ---------------------------------------------------------------------------

/// One imported ink page — the user's handwriting on a single host PDF page.
/// Cached SVGs live at `books/<sha>/ink/<asin>/<container>.{overlay,plain}.svg`
/// (derived from `asin` + `container_id` via [`crate::library::LibraryPaths`]).
#[derive(Debug, Clone, Serialize)]
pub struct BookInkRow {
    pub id: i64,
    pub book_id: Option<i64>,
    pub asin: String,
    pub container_id: String,
    /// 0-based host PDF page the ink overlays. `None` if the `.yjr` anchor eid
    /// didn't resolve to a page (no KFX text layer, or a yjr/book mismatch) — the
    /// ink is still stored, surfaced only in the gallery until it can be anchored.
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
/// Returns the row id.
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

/// Carry every ink row keyed on `old_key` over to `new_key`, for when a book's
/// baked identity changes under it. Ink identity is `(asin, container_id)` and
/// the device names its `.notebooks/<asin>!!PDOC!!` dir after the same value,
/// so a re-key that skipped this would strand the pages already collected.
///
/// The per-device tables come too: they record what was decoded for which key,
/// and left behind they would make the next sync re-decode from scratch.
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
///
/// A device that holds its own filesystem (the on-device picker) asks for this
/// before a LAN sync so it can hash its `.notebooks/` locally and upload only
/// what changed. The USB path has no use for it: there the host does the walk
/// and checks each nbk as it goes.
pub fn ink_sync_shas(
    conn: &Connection,
    device_serial: &str,
) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT asin, nbk_sha FROM ink_sync WHERE device_serial = ?1")?;
    let rows = stmt.query_map(params![device_serial], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// Every stored notebook's `(uuid, nbk_sha256)`, skipping rows whose sha
/// predates the column. The notebook twin of [`ink_sync_shas`] — and NOT keyed
/// by device, because a notebook is one library entity no matter which Scribe
/// wrote it.
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

/// Record one device's ink-page presence for `asin`: which `container_id`s the
/// device currently asserts (decoded from the nbk just pulled). **Provenance
/// only** — it never deletes a backup row; an erased-on-device page keeps its
/// Sidle backup. The ink analogue of [`record_device_book_presence`].
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
    // Drop this (device, asin)'s presence rows not touched this pass (no longer on
    // the device). Side-table only — the ink page it referenced is preserved.
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

/// Give an annotation the capture date it has none for, returning whether a row
/// changed.
///
/// For rows imported before the device's `creationTime` was kept. Deliberately
/// narrow — it fills an absent value only, so a date already on the row is never
/// rewritten, which also makes it idempotent: once filled, later syncs find
/// nothing to do.
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
///
/// The push composer needs to know before it writes, and the usual answer comes
/// from the sidecars the device just sent. A book being written for the first
/// time has no sidecar to read that from, so the evidence comes from what this
/// device has already told us: a monochrome Kindle writes no colour at all, so
/// one coloured row from this serial settles it. A device that has never
/// reported a highlight reads as monochrome, and the first colour it does
/// report moves it over.
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

// ---------------------------------------------------------------------------
// Deletion records (tombstones) — a Sidle-side delete records the artifact's
// stable identity so the additive device sync won't re-add it (Sidle is the
// curated backup). "Restore from device" clears them.
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

/// Clear every deletion record — "Restore from device" un-suppresses all
/// Sidle-side deletions so anything still on a device is re-imported. Returns
/// how many records were cleared.
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

/// Drop a device's import checkpoints (`yjr_sync` + `ink_sync`) so the next sync
/// re-pulls everything — used by "Restore from device" to bypass the unchanged
/// fast-path, so a Sidle-deleted item still on the device is actually re-imported.
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

/// Record one device's annotation presence for a book: which `dedup_hash`es the
/// device currently asserts (`current_hashes` = the hashes in the device's `.yjr`
/// for this book, just imported). This is **provenance only** — it never deletes
/// a backup row. Sidle is the durable backup; a delete on the device must not
/// delete Sidle's copy.
///
/// 1. Mark each current hash seen-now (upsert presence with `last_seen = now`).
/// 2. Drop this `(device, book)`'s presence rows with an older `last_seen` — they
///    are no longer on the device, so the side table stays an accurate mirror of
///    what each device currently holds. (Only the presence row is dropped; the
///    `annotations` row it pointed at is kept.)
///
/// `now` must be unique per sync pass (an ISO timestamp is — two passes never
/// share one), since step 2 uses it as the "seen this pass" marker.
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

    // Drop this (device, book)'s presence rows not touched this pass (no longer on
    // the device). Side-table only — the annotation it referenced is preserved.
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
///
/// `local` is a directory on this machine — a repo checkout whose `build.sh`
/// has run, or an unpacked bundle. `release` is a GitHub `owner/repo` whose
/// release asset was fetched and unpacked; `root` then points at where it
/// landed, so both kinds resolve the same way from here on.
pub const APP_SOURCE_LOCAL: &str = "local";
pub const APP_SOURCE_RELEASE: &str = "release";

/// One registered app: where to find its tree, and nothing else.
///
/// Everything a caller wants to show — name, version, files, sizes — is read
/// from `root` when it is asked for. A `local` source is a working copy that a
/// build rewrites without telling anyone, so a cached copy of any of it would
/// be stale from the moment it was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppSourceRow {
    /// The app's id — its directory name under `extensions/`.
    pub id: String,
    /// [`APP_SOURCE_LOCAL`] or [`APP_SOURCE_RELEASE`].
    pub source_kind: String,
    /// The repo path or `owner/repo` the user named.
    pub source: String,
    /// The mount root inside `source` — the directory `extensions/` sits in.
    /// Stored because one source can hold several apps, so `source` alone does
    /// not say which tree this row is.
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

/// Register an app, or repoint one that is already registered.
///
/// Re-adding an id replaces its location rather than erroring: pointing sidle
/// at a checkout that moved is the same gesture as adding it, and an id names
/// one app in the fleet.
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

/// Forget an app. `true` if a row went. This unregisters the source only — it
/// touches neither the tree on disk nor anything already on a device.
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
        // Mimic a pre-v11 row: insert_book now always writes the romaji columns,
        // so NULL them out + set a JP language to recreate the upgrade scenario.
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

        // Dropped on both devices — backup still intact.
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

        // The device's current set has only `keep`. `gone` is no longer on the
        // device, but Sidle is a backup — nothing is deleted.
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

        // Another book asking for the same ASIN now finds the collision...
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

    /// Path portability: paths are stored root-relative, resolved to
    /// absolute on read, stay relative across a read-modify-write, and an older
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

    /// `relativize_for_store` must relativize a managed path even when it was
    /// written under a DIFFERENT root than today's, or books go missing after a
    /// relocate. `strip_prefix` alone can't: the stored paths
    /// sat under the legacy lowercase `…/sidle/…` (case-mismatched against the
    /// live `…/Sidle`) or under the pre-move folder entirely.
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
        // The fast path (already under the live root) still works…
        assert_eq!(
            relativize_for_store(Some(live), "/Users/x/Documents/Sidle/books/ghi/x.kfx"),
            "books/ghi/x.kfx",
        );
        // …and is idempotent on an already-relative value.
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
            // Stamp in the legacy on-disk state: absolute paths under the OLD
            // lowercase app-support root, which no longer holds the files.
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

        // Reopen at the live root → migration relativizes, so reads now resolve
        // under THIS root (where the relocate actually put the files).
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

    /// One passage, recorded twice under the old rule because the two origins
    /// measured its linear position differently — the exact shape a Sidle
    /// highlight came back in after a round trip through a Kindle. v12 re-keys
    /// both onto the anchor and collapses them.
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

        // The device's presence record followed the survivor rather than dying
        // with the row it was attached to.
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM annotation_device WHERE dedup_hash = ?1",
                params![kept.dedup_hash],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "presence re-keyed onto the survivor");

        // The tombstone still names the passage it was written for, so the next
        // device sync will not resurrect it.
        let other_row = rows.iter().find(|r| r.eid_start == Some(1073)).unwrap();
        assert!(
            is_deleted(&conn, DELETION_ANNOTATION, &other_row.dedup_hash).unwrap(),
            "tombstone re-keyed with its annotation",
        );
        assert!(!is_deleted(&conn, DELETION_ANNOTATION, "old-other").unwrap());

        // Idempotent: a second open finds the version already stamped and the
        // rows already keyed, and changes nothing.
        let before: Vec<String> = rows.iter().map(|r| r.dedup_hash.clone()).collect();
        migrate(&conn).unwrap();
        let after: Vec<String> = list_annotations_for_book(&conn, book)
            .unwrap()
            .iter()
            .map(|r| r.dedup_hash.clone())
            .collect();
        assert_eq!(after, before);
    }

    /// The state a sync leaves behind when it lands *before* the re-key: the new
    /// rule's hash is already on a row, and that row is newer than the one it
    /// will collapse into.
    ///
    /// Re-keying the survivor in place would then set a hash a live row still
    /// holds, and `dedup_hash` is UNIQUE — so the duplicates have to be dropped
    /// before any survivor is rewritten. Getting this wrong fails the whole
    /// migration, and since `open()` migrates, that locks the user out of their
    /// library rather than merely leaving a duplicate behind.
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

    /// The measured case: a highlight annotated in Sidle was stored as one
    /// `note` row, so when the device synced its own *highlight* record for the
    /// same passage back, it landed as a second row. Splitting the fused row
    /// gives that record something to match, and the pair becomes one highlight
    /// carrying one note.
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
        // The fused row the old reader wrote, carrying the hash a DB already at
        // v12 holds for it — so what this test measures is the split, not a
        // later re-key finding a placeholder.
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

    /// The same split with nothing synced back yet: the highlight has to be
    /// minted, and the note must survive it. The first version of this migration
    /// lost the note here — the fused row already carries the note's canonical
    /// hash, so "insert the note if absent" found the row itself and skipped,
    /// after which the row was rewritten as the highlight and the body vanished.
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

    /// A `note` row a Kindle wrote is a real note record. Splitting one would
    /// invent a highlight the device never had and push it back.
    #[test]
    fn migrate_v13_leaves_device_written_notes_alone() {
        let conn = fresh_db();
        let book = insert_minimal(&conn, "sha-dev-note", "Perfect Insider");
        // The hash a DB already at v12 holds for this record, so the assertion
        // below measures the split leaving it alone rather than a later re-key.
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
        // The DB is still queryable after compaction.
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
        // A re-key changes the identity the device names its
        // `.notebooks/<asin>!!PDOC!!` dir after. Ink already collected under
        // the old one has to come along, or the pages are stranded and the
        // next sync re-decodes from scratch.
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
            estimated: false,
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
        // Spreading the window would have reported 1500 s in each hour.
        assert_eq!((at(20), at(21)), (2900, 100));
        // Counted once: a session with recorded hours must not also be spread.
        assert_eq!(cells.iter().map(|c| c.seconds).sum::<i64>(), 3000);
        assert_eq!(
            reading_days(&conn, "0000-00-00", "9999-99-99").unwrap(),
            vec![("2026-08-11".to_string(), 3000)],
            "and the clock still agrees with the calendar beside it",
        );

        // Clearing takes the hours with the sessions rather than leaving them to
        // be adopted by whatever row id comes next.
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
        // Ninety minutes of counted reading over three hours of clock: half an
        // hour of the 20:00 hour, all of 21:00, half of 22:00 — so the middle
        // hour takes twice what either end does.
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
        // A pre-fix row: one stored session covering a whole day and a bit,
        // which is several sittings glued together. Spreading it evenly would
        // report reading in every hour including 04:00.
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
        // ...but both belong to the day the session began, so this sums to
        // exactly what `reading_days` gives for that day and nothing lands in
        // a day the heatmap would draw as empty.
        assert!(cells.iter().all(|c| c.month == "2026-08" && c.dow == 2));
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
        // The night is now on the night's own day, and not a second of it was
        // lost or invented on the way there.
        let days = reading_days(&conn, "0000-00-00", "9999-99-99").unwrap();
        assert_eq!(days.iter().map(|(_, s)| s).sum::<i64>(), 4000);

        // Running again changes nothing: the rows no longer cross a midnight.
        conn.pragma_update(None, "user_version", 18).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(stored_sessions(&conn).len(), 2);
    }

    #[test]
    fn a_row_spanning_two_midnights_becomes_three_days() {
        // The widest kind the old parser produced — several sittings glued into
        // one row. It is divided by its own clock because that is the only thing
        // the row still says; the alternative is leaving all of it on the first
        // evening, where none of it belongs.
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
        // Nor is one whose stamps cannot be read, rather than being cut at a
        // midnight guessed from a string.
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

        // Re-keyed in place, not duplicated: the identity index counts the
        // position, so a second insert under the other constant would be a
        // second row and the sitting would be counted twice.
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

        // The orphan is still on disk, ready to be named if its book comes back…
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
        // The log arrives before the book does — the whole reason an
        // unattributed row is kept rather than dropped.
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

    /// Naming never takes reading away from the book that already holds it: the
    /// write touches unattributed rows only.
    #[test]
    fn naming_a_position_leaves_sessions_another_book_already_holds() {
        let conn = fresh_db();
        let held = insert_minimal(&conn, "sha-held", "先に名前がついた本");
        let other = insert_minimal(&conn, "sha-other", "別の本");
        set_max_position(&conn, held, Some(1000)).unwrap();
        insert_reading_session(&conn, &session("2026-06-22", 999, 300)).unwrap();
        assert_eq!(resolve_reading_sessions(&conn).unwrap(), 1);

        // A later session at the same position, still unattributed.
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

        // One book throughout, but the figure differs per window — which is why
        // the client cannot filter an all-time list and get this right.
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

        // One book, two months, and neither row carries the other's hours — the
        // whole reason the split happens here rather than on the client.
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
