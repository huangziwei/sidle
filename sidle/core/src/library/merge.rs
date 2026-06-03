//! Library merge — fold a `.sidlebak`'s contents into the *live* library,
//! additively. The scenario: a canonical library on one machine and a wiped-often
//! test library on another; back the test library up, carry it over, and merge
//! its curated books (and their highlights / ink / reading position) plus any new
//! Scribe notebooks into the canonical one — without losing either side.
//! See `.claude/plans/library-merge.md`.
//!
//! ## Why this is safe (and simpler than restore)
//! Merge is purely **additive**: it only `INSERT`s rows and copies *new*
//! `books/<sha>/` and `notebooks/<uuid>/` directories. It never deletes or
//! overwrites an existing row or file, so — unlike [`super::backup::restore`] —
//! there is no file swap and no relaunch. The running app sees its own inserts;
//! the UI just re-lists. The undo is "remove the books that came in".
//!
//! ## Identity & remap
//! Every cross-machine identity key is content-derived and stable; only the local
//! autoincrement `id` collides and is remapped. Books key on `sha256`, annotations
//! on `dedup_hash` (a content hash — union for free), ink on `(asin, container_id)`,
//! notebooks on `uuid`. We iterate the source *per book*, so a book's annotations,
//! `source='sidle'` reading position, and ink land directly on that book's local
//! id — no separate id map. Source orphans (`book_id IS NULL` — a device-import
//! inbox for books not in the source library) are intentionally skipped: they
//! belong to no book that comes over, and would be orphans here too.
//!
//! ## Duplicate books — newest-metadata-wins
//! A `sha256` collision means byte-identical content, so only metadata can differ.
//! The newer side (by `books.updated_at`) wins the metadata; `asin` is the
//! exception — prefer-non-empty (never clobber a real, content-derived ASIN with
//! an absent one). The book's annotations / position / ink are unioned regardless.
//!
//! ## Notebooks — add-only
//! Unlike books, a notebook's identity (`uuid`) is *not* its content (a notebook's
//! bytes change as it's edited), so newest-wins would require re-syncing its files,
//! not just a row. We keep it simple and correct: a *new* uuid is added (row +
//! files); an existing one is left entirely alone (row and files both untouched,
//! so they never disagree).
//!
//! ## Skipped — re-syncable bookkeeping
//! `annotation_device`, `book_ink_device`, `yjr_sync`, `ink_sync`, and
//! `source='device'` reading positions are not carried; they repopulate on the
//! next device connect.
//!
//! ## Limitation
//! Merge brings whatever files the source has *at merge time* and carries the
//! source's conversion status. A book mid-conversion on the source (only one
//! format side) merges with that one side and a `pending` job that the dest's
//! bootstrap re-enqueues on next launch; merge a fully-converted library to avoid
//! this. (Once a `books/<sha>/` dir exists on the dest, a later re-merge won't
//! re-copy it — content-addressed dirs are assumed complete.)

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::library::{backup, db, relocate};

/// What a merge brought in — surfaced to the UI.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MergeOutcome {
    /// Books new to this library (inserted, files copied).
    pub books_added: i64,
    /// Duplicate books (same `sha256`) whose metadata the source won on recency.
    pub books_updated: i64,
    /// Annotations newly inserted (a shared `dedup_hash` is deduped, not counted).
    pub annotations_added: i64,
    /// Ink pages new to this library (a shared `(asin, container_id)` is not counted).
    pub ink_added: i64,
    /// Notebooks new to this library (existing uuids are left untouched).
    pub notebooks_added: i64,
}

/// The source's per-book conversion state, carried so a merged book displays the
/// right status instead of defaulting to `pending` (no job row → COALESCE).
struct SourceJob {
    status: String,
    kind: String,
}

/// One source book and everything tied to it, read from the staged DB while it
/// was open. Owned (plain) data, so a [`Prepared`] crosses a `spawn_blocking`.
struct SourceBook {
    row: db::BookRow,
    job: Option<SourceJob>,
    annotations: Vec<db::AnnotationRow>,
    /// `source='sidle'` positions only — device positions re-sync.
    positions: Vec<db::ReadingPosition>,
    ink: Vec<db::BookInkRow>,
}

/// The lock-free product of [`prepare`]: the source inventory in memory and the
/// new files already copied into the live root. [`commit`] turns it into rows.
pub struct Prepared {
    dest_root: PathBuf,
    books: Vec<SourceBook>,
    notebooks: Vec<db::NotebookRow>,
}

impl Prepared {
    /// Whether there's anything to commit — lets the command short-circuit a
    /// no-op merge (e.g. re-merging the same backup) without taking the DB lock.
    pub fn is_empty(&self) -> bool {
        self.books.is_empty() && self.notebooks.is_empty()
    }
}

/// Validate + extract the `.sidlebak`, read its inventory, and copy any new
/// `books/<sha>/` and `notebooks/<uuid>/` dirs into `dest_root` — all **without**
/// the DB connection, so the command runs this off the async runtime and holds no
/// lock during the (potentially large) file copy. Hand the result to [`commit`].
///
/// `app_user_version` is the running app's [`db::SCHEMA_VERSION`]; the manifest is
/// gated against it (a forward-incompatible backup is refused before any copy).
pub fn prepare(src_zip: &Path, dest_root: &Path, app_user_version: i64) -> Result<Prepared> {
    // Shared front-half with restore: validate, version-gate, extract, checksum.
    let staging = backup::sibling(dest_root, "merging")?;
    let _manifest = backup::stage_archive(src_zip, &staging, app_user_version)?;

    // Read the source inventory from the staged DB, migrated up to the current
    // schema so every column exists regardless of the backup's age (a pre-v7
    // backup has no `books.updated_at`; migration adds + backfills it, so its
    // books read as "older" and lose newest-wins ties to the local curated copy).
    let inventory = {
        let src_db = staging.join("library.db");
        let conn = db::open(&src_db)
            .with_context(|| format!("open staged library {}", src_db.display()))?;
        let books = read_source_books(&conn).context("read source books")?;
        let notebooks = db::list_notebooks(&conn).context("read source notebooks")?;
        (books, notebooks)
    };
    let (books, notebooks) = inventory;

    // Copy each NEW book / notebook dir into the live root. Content-addressed book
    // dirs make "already present" mean "same bytes" → skip; notebooks are add-only
    // (an existing uuid keeps its files). Files first, rows later (an orphaned dir
    // on a later failure is benign; a row pointing at a missing file is not).
    let books_root = dest_root.join("books");
    for b in &books {
        let dst = books_root.join(&b.row.sha256);
        let src = staging.join("books").join(&b.row.sha256);
        if !dst.exists() && src.is_dir() {
            relocate::copy_dir(&src, &dst)
                .with_context(|| format!("copy book dir {}", b.row.sha256))?;
        }
    }
    let notebooks_root = dest_root.join("notebooks");
    for nb in &notebooks {
        let dst = notebooks_root.join(&nb.uuid);
        let src = staging.join("notebooks").join(&nb.uuid);
        if !dst.exists() && src.is_dir() {
            relocate::copy_dir(&src, &dst)
                .with_context(|| format!("copy notebook dir {}", nb.uuid))?;
        }
    }

    // Staging is fully consumed (inventory in memory, files copied) — drop it and
    // the WAL sidecars `db::open` created.
    let _ = std::fs::remove_dir_all(&staging);

    Ok(Prepared { dest_root: dest_root.to_path_buf(), books, notebooks })
}

/// Apply a [`Prepared`] to the live DB in one transaction (the only step needing
/// the connection — fast, metadata-only, so the caller holds the lock just here).
/// Atomic: a failure rolls back to the exact pre-merge rows. The files [`prepare`]
/// copied stay (benign content-addressed dirs) even on rollback.
pub fn commit(conn: &Connection, prepared: &Prepared) -> Result<MergeOutcome> {
    let dest_root = prepared.dest_root.as_path();
    let mut out = MergeOutcome::default();
    let tx = conn.unchecked_transaction()?;

    for b in &prepared.books {
        let sha = b.row.sha256.as_str();
        let local_id = match db::find_by_sha(&tx, sha).context("find_by_sha")? {
            Some(local) => {
                // Duplicate (same content). Newest-metadata-wins; files untouched.
                if b.row.updated_at > local.updated_at {
                    overwrite_metadata(&tx, &b.row, &local).context("overwrite metadata")?;
                    out.books_updated += 1;
                }
                local.id
            }
            None => {
                // New book — preserve the source's curated metadata and edit time.
                let id = insert_source_book(&tx, dest_root, &b.row).context("insert book")?;
                db::set_book_updated_at(&tx, id, &b.row.updated_at)?;
                if let Some(job) = &b.job {
                    // A backup taken mid-conversion records `converting`; the dest
                    // isn't converting it, so re-pend so bootstrap re-enqueues.
                    let status = if job.status == "converting" { "pending" } else { job.status.as_str() };
                    db::insert_job(&tx, id, status, &job.kind)?;
                }
                out.books_added += 1;
                id
            }
        };

        for a in &b.annotations {
            if insert_annotation_for(&tx, local_id, a)? {
                out.annotations_added += 1;
            }
        }
        for p in &b.positions {
            upsert_sidle_position(&tx, local_id, p)?;
        }
        for ink in &b.ink {
            if upsert_ink_for(&tx, local_id, ink)? {
                out.ink_added += 1;
            }
        }
    }

    // Notebooks: add-only (see module doc). An existing uuid is left entirely
    // alone — its row and files never disagree.
    for nb in &prepared.notebooks {
        if db::get_notebook_by_uuid(&tx, &nb.uuid).context("get notebook")?.is_none() {
            let updated_at = nb.updated_at.as_deref().unwrap_or(&nb.imported_at);
            db::upsert_notebook(
                &tx,
                &nb.uuid,
                nb.page_count,
                nb.nbk_sha256.as_deref().unwrap_or(""),
                &nb.imported_at,
                updated_at,
            )?;
            out.notebooks_added += 1;
        }
    }

    tx.commit()?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

/// Read every source book plus its conversion job, annotations, `sidle` reading
/// positions, and ink — grouped per book, so [`commit`] needs no id map.
fn read_source_books(conn: &Connection) -> Result<Vec<SourceBook>> {
    let rows = db::list_books(conn).context("list source books")?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let job = conn
            .query_row(
                "SELECT status, kind FROM conversion_jobs WHERE book_id = ?1",
                params![row.id],
                |r| Ok(SourceJob { status: r.get(0)?, kind: r.get(1)? }),
            )
            .optional()
            .context("read source conversion job")?;
        let annotations = db::list_annotations_for_book(conn, row.id).context("source annotations")?;
        let positions = db::list_reading_positions(conn, row.id)
            .context("source reading positions")?
            .into_iter()
            .filter(|p| p.source == "sidle")
            .collect();
        let ink = db::list_book_ink(conn, row.id).context("source ink")?;
        out.push(SourceBook { row, job, annotations, positions, ink });
    }
    Ok(out)
}

/// Insert a brand-new book, preserving the source's metadata. File paths are
/// rebuilt under `dest_root` from each source path's basename + the sha, and set
/// only if the file is actually present (a book whose dir wasn't archived gets
/// NULL paths rather than a dangling one).
fn insert_source_book(conn: &Connection, dest_root: &Path, row: &db::BookRow) -> Result<i64> {
    let sha = row.sha256.as_str();
    let remap = |stored: &Option<String>| -> Option<String> {
        let p = stored.as_ref()?;
        let name = Path::new(p).file_name()?;
        let dest_abs = dest_root.join("books").join(sha).join(name);
        dest_abs.exists().then(|| dest_abs.to_string_lossy().into_owned())
    };
    let epub = remap(&row.epub_path);
    let cover = remap(&row.cover_path);
    let kfx = remap(&row.kfx_path);
    let pdf = remap(&row.pdf_path);
    // `kfx_sha256` is `Some` iff `kfx_path` is — drop it if the KFX didn't come over.
    let kfx_sha = if kfx.is_some() { row.kfx_sha256.as_deref() } else { None };

    db::insert_book(
        conn,
        &db::NewBook {
            sha256: sha,
            title: &row.title,
            author: &row.author,
            language: &row.language,
            ppd: row.ppd.as_deref(),
            epub_path: epub.as_deref(),
            cover_path: cover.as_deref(),
            kfx_path: kfx.as_deref(),
            kfx_sha256: kfx_sha,
            pdf_path: pdf.as_deref(),
            file_size: row.file_size,
            imported_at: &row.imported_at,
            asin: row.asin.as_deref(),
            publisher: row.publisher.as_deref(),
            published_at: row.published_at.as_deref(),
            series_name: row.series_name.as_deref(),
            series_index: row.series_index,
            tags: &row.tags,
        },
    )
    .context("insert merged book")
}

/// Overwrite a duplicate book's metadata with the (newer) source's — but never
/// its file paths (the dest's files are the ones on disk), and `asin` is
/// prefer-non-empty: keep the dest's real ASIN, else adopt the source's, never
/// blank it. Sets `updated_at` to the source's so a later re-merge compares right.
fn overwrite_metadata(conn: &Connection, source: &db::BookRow, local: &db::BookRow) -> Result<()> {
    let asin = local
        .asin
        .as_deref()
        .filter(|a| !a.is_empty())
        .or(source.asin.as_deref().filter(|a| !a.is_empty()));
    let tags_json = serde_json::to_string(&source.tags)
        .map_err(|e| anyhow::anyhow!("serialize tags: {e}"))?;
    conn.execute(
        r#"UPDATE books SET
               title = ?1, author = ?2, language = ?3, ppd = ?4, publisher = ?5,
               published_at = ?6, series_name = ?7, series_index = ?8, tags = ?9,
               asin = ?10, updated_at = ?11
           WHERE id = ?12"#,
        params![
            source.title,
            source.author,
            source.language,
            source.ppd,
            source.publisher,
            source.published_at,
            source.series_name,
            source.series_index,
            tags_json,
            asin,
            source.updated_at,
            local.id,
        ],
    )?;
    Ok(())
}

/// Insert one of a book's annotations under its local id; returns whether a new
/// row landed (`false` = a `dedup_hash` we already had → unioned away).
fn insert_annotation_for(conn: &Connection, book_id: i64, a: &db::AnnotationRow) -> Result<bool> {
    db::insert_annotation(
        conn,
        &db::NewAnnotation {
            dedup_hash: &a.dedup_hash,
            book_id: Some(book_id),
            kind: &a.kind,
            eid_start: a.eid_start,
            off_start: a.off_start,
            eid_end: a.eid_end,
            off_end: a.off_end,
            loc_start: a.loc_start,
            loc_end: a.loc_end,
            linear_pos: a.linear_pos,
            text: &a.text,
            note_body: a.note_body.as_deref(),
            color: a.color.as_deref(),
            clip_title: a.clip_title.as_deref(),
            clip_author: a.clip_author.as_deref(),
            added_at: a.added_at.as_deref(),
            added_raw: a.added_raw.as_deref(),
            imported_at: &a.imported_at,
            source: &a.source,
        },
    )
    .context("insert merged annotation")
}

/// Upsert a book's `source='sidle'` reading position, newest-wins on `updated_at`
/// (the `WHERE` on the conflict clause keeps the dest's if it's newer or equal).
/// Preserves the source's `updated_at` rather than stamping now, so it stays the
/// true last-read time.
fn upsert_sidle_position(conn: &Connection, book_id: i64, p: &db::ReadingPosition) -> Result<()> {
    conn.execute(
        r#"INSERT INTO reading_position
               (book_id, eid, "offset", linear_pos, source, device_serial, updated_at)
           VALUES (?1, ?2, ?3, ?4, 'sidle', '', ?5)
           ON CONFLICT(book_id, source, device_serial) DO UPDATE SET
               eid = excluded.eid,
               "offset" = excluded."offset",
               linear_pos = excluded.linear_pos,
               updated_at = excluded.updated_at
           WHERE excluded.updated_at > reading_position.updated_at"#,
        params![book_id, p.eid, p.offset, p.linear_pos, p.updated_at],
    )?;
    Ok(())
}

/// Upsert one ink page under its local id, keyed `(asin, container_id)`; returns
/// whether it was new to this library.
fn upsert_ink_for(conn: &Connection, book_id: i64, ink: &db::BookInkRow) -> Result<bool> {
    let existed = conn
        .query_row(
            "SELECT 1 FROM book_ink WHERE asin = ?1 AND container_id = ?2",
            params![ink.asin, ink.container_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    db::upsert_book_ink(
        conn,
        &db::NewBookInk {
            book_id: Some(book_id),
            asin: &ink.asin,
            container_id: &ink.container_id,
            host_page: ink.host_page,
            host_eid: ink.host_eid,
            host_linear: ink.host_linear,
            nbk_sha256: ink.nbk_sha256.as_deref(),
            imported_at: &ink.imported_at,
        },
    )
    .context("upsert merged ink")?;
    Ok(!existed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a library under `root` (canonicalized — same symlink caveat as
    /// backup's seed). Each book gets an EPUB + cover whose bytes encode the sha,
    /// one annotation (hash `h-<sha>`), and a `sidle` reading position.
    fn seed(root: &Path, books: &[(&str, &str, &str)]) -> Connection {
        let conn = db::open(&root.join("library.db")).unwrap();
        for (sha, title, updated_at) in books {
            let dir = root.join("books").join(sha);
            fs::create_dir_all(&dir).unwrap();
            let epub = dir.join("book.epub");
            fs::write(&epub, format!("epub-{sha}")).unwrap();
            let cover = dir.join("cover.jpg");
            fs::write(&cover, format!("cover-{sha}")).unwrap();
            let id = db::insert_book(
                &conn,
                &db::NewBook {
                    sha256: sha,
                    title,
                    author: "Author",
                    language: "en",
                    ppd: None,
                    epub_path: Some(&epub.to_string_lossy()),
                    cover_path: Some(&cover.to_string_lossy()),
                    kfx_path: None,
                    kfx_sha256: None,
                    pdf_path: None,
                    file_size: 0,
                    imported_at: "2026-01-01T00:00:00+00:00",
                    asin: None,
                    publisher: None,
                    published_at: None,
                    series_name: None,
                    series_index: None,
                    tags: &[],
                },
            )
            .unwrap();
            // Pin updated_at deterministically (insert stamps it = imported_at).
            db::set_book_updated_at(&conn, id, updated_at).unwrap();
            db::insert_annotation(
                &conn,
                &db::NewAnnotation {
                    dedup_hash: &format!("h-{sha}"),
                    book_id: Some(id),
                    kind: "highlight",
                    eid_start: Some(1),
                    off_start: Some(0),
                    eid_end: Some(1),
                    off_end: Some(5),
                    loc_start: None,
                    loc_end: None,
                    linear_pos: Some(10),
                    text: "passage",
                    note_body: None,
                    color: None,
                    clip_title: None,
                    clip_author: None,
                    added_at: None,
                    added_raw: None,
                    imported_at: "2026-01-01T00:00:00+00:00",
                    source: "sidle",
                },
            )
            .unwrap();
            db::set_reading_position(&conn, id, Some(2), Some(7), Some(42), "sidle", "").unwrap();
        }
        conn
    }

    /// Back `src_conn`'s library up to a `.sidlebak` under `out_dir`.
    fn backup_of(src_conn: &Connection, src_root: &Path, out_dir: &Path) -> PathBuf {
        let zip = out_dir.join("lib.sidlebak");
        backup::create(src_conn, &src_root.join("books"), src_root, "test", &zip).unwrap();
        zip
    }

    fn merge_into(dest_conn: &Connection, dest_root: &Path, zip: &Path) -> MergeOutcome {
        let prepared = prepare(zip, dest_root, db::SCHEMA_VERSION).unwrap();
        commit(dest_conn, &prepared).unwrap()
    }

    #[test]
    fn adds_new_books_unions_annotations_and_copies_files() {
        let src = tempfile::tempdir().unwrap();
        let src_root = src.path().canonicalize().unwrap();
        // Source has A (shared) + B (new). Dest has only A.
        let src_conn = seed(
            &src_root,
            &[("aaa", "Alpha", "2026-01-01T00:00:00+00:00"), ("bbb", "Beta", "2026-01-01T00:00:00+00:00")],
        );
        let out = tempfile::tempdir().unwrap();
        let zip = backup_of(&src_conn, &src_root, out.path());
        drop(src_conn);

        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().canonicalize().unwrap();
        let dst_conn = seed(&dst_root, &[("aaa", "Alpha", "2026-01-01T00:00:00+00:00")]);

        let outcome = merge_into(&dst_conn, &dst_root, &zip);
        assert_eq!(outcome.books_added, 1, "B is new");
        assert_eq!(outcome.books_updated, 0, "A not newer either way");
        assert_eq!(outcome.annotations_added, 1, "only B's highlight is new (A's dedups)");

        let books = db::list_books(&dst_conn).unwrap();
        assert_eq!(books.len(), 2);
        let beta = books.iter().find(|b| b.sha256 == "bbb").expect("B merged");
        // B's files came over, byte-identical, resolved under the dest root.
        let epub = beta.epub_path.as_ref().unwrap();
        assert!(Path::new(epub).starts_with(&dst_root), "{epub} under dest");
        assert_eq!(fs::read_to_string(epub).unwrap(), "epub-bbb");
        // B's annotation landed on B's *local* id (remap), and there are 2 total.
        let bann = db::list_annotations_for_book(&dst_conn, beta.id).unwrap();
        assert_eq!(bann.len(), 1);
        assert_eq!(bann[0].dedup_hash, "h-bbb");
        let total: i64 = dst_conn
            .query_row("SELECT COUNT(*) FROM annotations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 2, "A's shared highlight was unioned, not duplicated");
        // B's reading position came over.
        let pos = db::list_reading_positions(&dst_conn, beta.id).unwrap();
        assert_eq!(pos.len(), 1);
        assert_eq!(pos[0].eid, Some(2));
    }

    #[test]
    fn duplicate_newest_metadata_wins() {
        // Same sha both sides; source edited more recently → its title wins.
        let src = tempfile::tempdir().unwrap();
        let src_root = src.path().canonicalize().unwrap();
        let src_conn = seed(&src_root, &[("aaa", "New Title", "2026-06-01T00:00:00+00:00")]);
        // Give the source book a real ASIN to fill the dest's empty one.
        let sid = db::find_by_sha(&src_conn, "aaa").unwrap().unwrap().id;
        db::set_asin(&src_conn, sid, "B07ABCDEFG").unwrap();
        let out = tempfile::tempdir().unwrap();
        let zip = backup_of(&src_conn, &src_root, out.path());
        drop(src_conn);

        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().canonicalize().unwrap();
        let dst_conn = seed(&dst_root, &[("aaa", "Old Title", "2026-01-01T00:00:00+00:00")]);

        let outcome = merge_into(&dst_conn, &dst_root, &zip);
        assert_eq!(outcome.books_added, 0);
        assert_eq!(outcome.books_updated, 1);
        let a = db::find_by_sha(&dst_conn, "aaa").unwrap().unwrap();
        assert_eq!(a.title, "New Title", "newer source metadata won");
        assert_eq!(a.asin.as_deref(), Some("B07ABCDEFG"), "empty dest ASIN filled");
    }

    #[test]
    fn older_source_does_not_overwrite_and_keeps_present_asin() {
        let src = tempfile::tempdir().unwrap();
        let src_root = src.path().canonicalize().unwrap();
        let src_conn = seed(&src_root, &[("aaa", "Stale Title", "2026-01-01T00:00:00+00:00")]);
        let out = tempfile::tempdir().unwrap();
        let zip = backup_of(&src_conn, &src_root, out.path());
        drop(src_conn);

        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().canonicalize().unwrap();
        let dst_conn = seed(&dst_root, &[("aaa", "Current Title", "2026-06-01T00:00:00+00:00")]);
        let did = db::find_by_sha(&dst_conn, "aaa").unwrap().unwrap().id;
        db::set_asin(&dst_conn, did, "B07REALASN").unwrap();

        let outcome = merge_into(&dst_conn, &dst_root, &zip);
        assert_eq!(outcome.books_updated, 0, "older source loses the tie");
        let a = db::find_by_sha(&dst_conn, "aaa").unwrap().unwrap();
        assert_eq!(a.title, "Current Title", "newer dest metadata kept");
        assert_eq!(a.asin.as_deref(), Some("B07REALASN"), "present ASIN never blanked");
    }

    #[test]
    fn is_additive_existing_data_untouched() {
        // Dest has Z; the source doesn't. Z and its highlight + file must survive.
        let src = tempfile::tempdir().unwrap();
        let src_root = src.path().canonicalize().unwrap();
        let src_conn = seed(&src_root, &[("aaa", "Alpha", "2026-01-01T00:00:00+00:00")]);
        let out = tempfile::tempdir().unwrap();
        let zip = backup_of(&src_conn, &src_root, out.path());
        drop(src_conn);

        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().canonicalize().unwrap();
        let dst_conn = seed(&dst_root, &[("zzz", "Zeta", "2026-01-01T00:00:00+00:00")]);

        merge_into(&dst_conn, &dst_root, &zip);
        let shas: Vec<String> =
            db::list_books(&dst_conn).unwrap().into_iter().map(|b| b.sha256).collect();
        assert!(shas.contains(&"zzz".to_string()), "Z preserved");
        assert!(shas.contains(&"aaa".to_string()), "A merged in");
        let z = db::find_by_sha(&dst_conn, "zzz").unwrap().unwrap();
        assert_eq!(fs::read_to_string(z.epub_path.as_ref().unwrap()).unwrap(), "epub-zzz");
        assert_eq!(db::list_annotations_for_book(&dst_conn, z.id).unwrap().len(), 1);
    }

    #[test]
    fn unions_notebooks_and_copies_their_files() {
        // Source carries a notebook the dest doesn't have.
        let src = tempfile::tempdir().unwrap();
        let src_root = src.path().canonicalize().unwrap();
        let src_conn = seed(&src_root, &[("aaa", "Alpha", "2026-01-01T00:00:00+00:00")]);
        let nb_pages = src_root.join("notebooks").join("uuid-1").join("pages");
        fs::create_dir_all(&nb_pages).unwrap();
        fs::write(nb_pages.join("page-0.svg"), "<svg>ink</svg>").unwrap();
        db::upsert_notebook(&src_conn, "uuid-1", 1, "nbksha", "2026-02-02T00:00:00+00:00", "2026-02-02T00:00:00+00:00").unwrap();
        let out = tempfile::tempdir().unwrap();
        let zip = backup_of(&src_conn, &src_root, out.path());
        drop(src_conn);

        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().canonicalize().unwrap();
        let dst_conn = seed(&dst_root, &[]);

        let outcome = merge_into(&dst_conn, &dst_root, &zip);
        assert_eq!(outcome.notebooks_added, 1);
        let nbs = db::list_notebooks(&dst_conn).unwrap();
        assert_eq!(nbs.len(), 1);
        assert_eq!(nbs[0].uuid, "uuid-1");
        // The page SVG came over byte-identical.
        let svg = dst_root.join("notebooks/uuid-1/pages/page-0.svg");
        assert_eq!(fs::read_to_string(svg).unwrap(), "<svg>ink</svg>");

        // Re-merging the same backup is a no-op for the notebook (add-only).
        let again = merge_into(&dst_conn, &dst_root, &zip);
        assert_eq!(again.notebooks_added, 0);
        assert_eq!(db::list_notebooks(&dst_conn).unwrap().len(), 1);
    }
}
