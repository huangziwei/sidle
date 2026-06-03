//! Library backup & restore — a single `.sidlebak` archive (a zip) of the whole
//! library, restorable on this or another machine. See
//! `.claude/plans/library-backup-and-portability.md` §5.
//!
//! Archive layout:
//! ```text
//! manifest.json              # format/version, counts, db_sha256 (integrity + schema gate)
//! library.db                 # consistent VACUUM INTO snapshot (deflate)
//! books/<sha>/...            # the whole tree, verbatim (store — already-compressed media)
//! ```
//!
//! Two hazards drive the design (§3): **H1** — a live WAL DB can't be copied
//! file-by-file, so we snapshot with `VACUUM INTO` (shared with
//! [`crate::library::relocate`]); **H5** — the running app holds an open
//! `Connection`, so restore does restore-then-relaunch: it swaps files on disk
//! and the command layer calls `app.restart()` so the next process opens them.
//! Cross-root portability is free because book paths are stored root-relative
//! (§4a) — the restored DB resolves under whatever root it lands in.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::library::{db, import, relocate};

/// `manifest.json/format` tag — distinguishes our archives from arbitrary zips.
const FORMAT_TAG: &str = "sidle-library-backup";
/// Archive layout version (independent of the DB schema's `user_version`). Bump
/// only if the archive *shape* changes; restore refuses a newer one.
///
/// v2: the archive now also carries the `notebooks/<uuid>/` tree (Scribe
/// handwriting). v1 archives stored only `library.db` + `books/`, so a restore
/// of one silently dropped notebook files; v2 closes that. A v1 archive still
/// restores into a v2 app — it simply has no `notebooks/` entries to extract.
const FORMAT_VERSION: u32 = 2;

/// The archive's `manifest.json`: enough to validate integrity, gate the schema
/// version on restore, and report what's inside without unzipping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub format_version: u32,
    pub created_at: String,
    pub app_version: String,
    /// DB schema version (`PRAGMA user_version`) at backup time; restore refuses
    /// an archive whose value exceeds the running app's (§4c).
    pub db_user_version: i64,
    /// Absolute library root at backup time — informational; restore needs no
    /// path rewrite because stored paths are already root-relative (§4a).
    pub source_root: String,
    pub counts: Counts,
    /// SHA-256 of the archived `library.db` bytes; verified on restore.
    pub db_sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Counts {
    pub books: i64,
    pub annotations: i64,
    /// Number of `books/<sha>/` directories actually archived (a freshly
    /// inserted row whose files aren't on disk yet contributes a book but no dir).
    pub book_dirs: i64,
    /// Scribe notebooks (`notebooks/<uuid>/` dirs) archived. `#[serde(default)]`
    /// so a v1 manifest (which predates this field) still parses — it reads 0.
    #[serde(default)]
    pub notebooks: i64,
}

/// Result of a restore, surfaced to the UI: what came in, and where the
/// pre-restore library was set aside (the undo).
#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    pub books: i64,
    pub annotations: i64,
    pub safety_copy: PathBuf,
}

/// Take the consistent DB snapshot — the only step that needs the live
/// `Connection` (`VACUUM INTO`, H1). Fast: the snapshot is just the metadata DB
/// (tens of MB at most), not the `books/` tree. The caller holds the DB lock
/// across only this, then releases it before the (potentially long) zip in
/// [`create_archive`] — so a multi-GB backup never stalls the app's other DB
/// users. The returned guard removes the temp file on drop.
pub fn snapshot(conn: &Connection) -> Result<TempSnapshot> {
    let tmp = TempSnapshot::new()?;
    relocate::snapshot_db(conn, &tmp.path)?;
    Ok(tmp)
}

/// Build the `.sidlebak` from a [`snapshot`] plus the on-disk `books/` tree — no
/// live `Connection`, so the caller can drop the DB lock first. `db_user_version`
/// is read from the live DB by the caller (under the same lock as the snapshot)
/// and recorded for the restore-time schema gate. Returns the manifest.
pub fn create_archive(
    snapshot: &TempSnapshot,
    books_dir: &Path,
    source_root: &Path,
    app_version: &str,
    db_user_version: i64,
    dest_zip: &Path,
) -> Result<Manifest> {
    // (1) Counts + the dir keys read FROM THE SNAPSHOT (read-only, so we don't
    //     mutate the bytes we're about to hash), so the manifest is internally
    //     consistent with the archived DB + file set.
    let inv = read_snapshot_inventory(&snapshot.path)?;
    let book_dirs = inv.shas.iter().filter(|s| books_dir.join(s).is_dir()).count() as i64;
    // Notebook dirs live a level up from `books/` — a `notebooks/` sibling under
    // the same root.
    let notebooks_dir = source_root.join("notebooks");
    let notebook_dirs =
        inv.notebook_uuids.iter().filter(|u| notebooks_dir.join(u).is_dir()).count() as i64;

    // (2) Integrity hash over the snapshot bytes.
    let db_sha256 = import::sha256_of_file(&snapshot.path)
        .with_context(|| format!("hash snapshot {}", snapshot.path.display()))?;

    let manifest = Manifest {
        format: FORMAT_TAG.to_string(),
        format_version: FORMAT_VERSION,
        created_at: Utc::now().to_rfc3339(),
        app_version: app_version.to_string(),
        db_user_version,
        source_root: source_root.to_string_lossy().into_owned(),
        counts: Counts {
            books: inv.books,
            annotations: inv.annotations,
            book_dirs,
            notebooks: notebook_dirs,
        },
        db_sha256,
    };

    // (3) Stream into the zip: manifest first (cheap to inspect), then the DB
    //     (deflate — DBs compress well), then the book tree (store — EPUB/KFX
    //     are already compressed, so deflate only burns CPU).
    let file = File::create(dest_zip)
        .with_context(|| format!("create {}", dest_zip.display()))?;
    let mut zw = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let manifest_json = serde_json::to_vec_pretty(&manifest).context("serialize manifest")?;
    zw.start_file("manifest.json", deflated).context("zip manifest.json")?;
    zw.write_all(&manifest_json).context("write manifest.json")?;

    add_file(&mut zw, "library.db", &snapshot.path, deflated)?;

    for sha in &inv.shas {
        let dir = books_dir.join(sha);
        if dir.is_dir() {
            add_dir(&mut zw, &format!("books/{sha}"), &dir, stored)?;
        }
    }

    // The notebook tree (Scribe handwriting) — a `notebooks/<uuid>/` sibling of
    // `books/`. Stored, like the book tree: the page SVGs are text but small,
    // and `nbk`/`cover.png` are already compact. v2 of the archive format.
    for uuid in &inv.notebook_uuids {
        let dir = notebooks_dir.join(uuid);
        if dir.is_dir() {
            add_dir(&mut zw, &format!("notebooks/{uuid}"), &dir, stored)?;
        }
    }

    zw.finish().context("finalize backup zip")?;
    Ok(manifest)
}

/// Snapshot + archive in one call, holding `conn` throughout — convenient for
/// tests and any non-interactive caller. The live app instead uses [`snapshot`]
/// then [`create_archive`] so it can release the DB lock before the long zip.
pub fn create(
    conn: &Connection,
    books_dir: &Path,
    source_root: &Path,
    app_version: &str,
    dest_zip: &Path,
) -> Result<Manifest> {
    let snap = snapshot(conn)?;
    let db_user_version = db::user_version(conn).context("read user_version")?;
    create_archive(&snap, books_dir, source_root, app_version, db_user_version, dest_zip)
}

/// The shared front-half of any consumer of a `.sidlebak` (restore and merge):
/// validate + version-gate the manifest BEFORE any disk mutation, extract into
/// `staging`, and verify the extracted `library.db` checksum. Returns the
/// manifest. On any failure after extraction the staging dir is cleared, so the
/// caller's target is never touched. The gates run before extraction, so a
/// foreign / forward-incompatible archive leaves no staging behind either.
///
/// Factored out so merge reuses the exact validation/extraction restore uses,
/// rather than a parallel copy that could drift (the gate is security-relevant:
/// zip-slip is handled in [`extract_all`], the schema gate refuses a
/// forward-incompatible DB).
pub(crate) fn stage_archive(
    src_zip: &Path,
    staging: &Path,
    app_user_version: i64,
) -> Result<Manifest> {
    let file = File::open(src_zip).with_context(|| format!("open {}", src_zip.display()))?;
    let mut archive =
        ZipArchive::new(file).with_context(|| format!("{} is not a zip", src_zip.display()))?;

    // Manifest: validate shape, then gate on format + schema version — all before
    // any disk mutation.
    let manifest = read_manifest(&mut archive)?;
    if manifest.format != FORMAT_TAG {
        bail!("not a sidle library backup (format = {:?})", manifest.format);
    }
    if manifest.format_version > FORMAT_VERSION {
        bail!(
            "backup archive format v{} is newer than this app supports (v{}) — update sidle",
            manifest.format_version,
            FORMAT_VERSION
        );
    }
    if manifest.db_user_version > app_user_version {
        bail!(
            "backup's library schema (v{}) is newer than this app's (v{}) — update sidle before restoring",
            manifest.db_user_version,
            app_user_version
        );
    }

    // Extract into the (freshly cleared) staging dir.
    if staging.exists() {
        fs::remove_dir_all(staging)
            .with_context(|| format!("clear stale staging {}", staging.display()))?;
    }
    extract_all(&mut archive, staging)?;

    // Integrity: the extracted DB's bytes must match the manifest's hash.
    let staged_db = staging.join("library.db");
    let got = import::sha256_of_file(&staged_db)
        .with_context(|| format!("hash extracted {}", staged_db.display()))?;
    if got != manifest.db_sha256 {
        let _ = fs::remove_dir_all(staging);
        bail!("backup is corrupt: library.db checksum mismatch");
    }

    Ok(manifest)
}

/// Restore a `.sidlebak` into `dest_root` (the current library root), replacing
/// its contents. `app_user_version` is the running app's [`db::SCHEMA_VERSION`].
///
/// Validates the manifest and gates the schema BEFORE touching disk, extracts to
/// a sibling staging dir, verifies the DB (checksum + opens as a sidle library
/// with the manifest's book count), then swaps: the current `library.db*` +
/// `books/` are moved aside to a `<root>.bak-<ts>` safety copy (the undo) and the
/// staged payload moved into place — renames only (staging + safety are siblings
/// of `dest_root`, hence same volume). `config.json` (if `dest_root` is the state
/// dir) is left untouched. The caller relaunches afterward (H5).
pub fn restore(src_zip: &Path, dest_root: &Path, app_user_version: i64) -> Result<RestoreOutcome> {
    // (1)+(2) Validate + gate the manifest, extract into a sibling staging dir,
    //     verify the DB checksum — the shared front-half, identical to merge
    //     (see [`stage_archive`]). Same volume → the later swap is a rename.
    let staging = sibling(dest_root, "restoring")?;
    let manifest = stage_archive(src_zip, &staging, app_user_version)?;

    // (3) Restore-specific verify: the staged DB opens as a sidle library with
    //     the manifest's book count. Paths are already relative (§4a), so there
    //     is nothing to rewrite. Failure clears staging, leaving the target
    //     untouched.
    let staged_books = match relocate::validate_existing(&staging) {
        Ok(n) => n,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(e.context("extracted library.db is not a usable sidle library"));
        }
    };
    if staged_books != manifest.counts.books {
        let _ = fs::remove_dir_all(&staging);
        bail!(
            "backup inconsistent: manifest says {} books, db has {}",
            manifest.counts.books,
            staged_books
        );
    }

    // (4) Swap. Move the current payload aside (undo), then the staged payload
    //     into place. The live app's open Connection points at the old
    //     library.db inode (now under the safety copy); we relaunch right after,
    //     so the next process opens the restored files.
    let safety = sibling(dest_root, &format!("bak-{}", Utc::now().format("%Y%m%d-%H%M%S")))?;
    fs::create_dir_all(&safety)
        .with_context(|| format!("create safety copy {}", safety.display()))?;
    move_payload(dest_root, &safety).context("set current library aside")?;
    fs::create_dir_all(dest_root).with_context(|| format!("recreate {}", dest_root.display()))?;
    move_payload(&staging, dest_root).context("move restored library into place")?;
    let _ = fs::remove_dir(&staging); // now empty

    Ok(RestoreOutcome {
        books: manifest.counts.books,
        annotations: manifest.counts.annotations,
        safety_copy: safety,
    })
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

/// A temp file holding the `VACUUM INTO` snapshot, removed on drop (success or
/// error). The OS temp dir is always writable; we read the file twice (hash +
/// zip) then discard it. Opaque to callers — produced by [`snapshot`], consumed
/// by [`create_archive`].
pub struct TempSnapshot {
    path: PathBuf,
}

impl TempSnapshot {
    fn new() -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // A process-wide counter guarantees uniqueness even when two snapshots
        // start within the same clock tick (parallel callers, or the test suite
        // running many `create`s at once): `{nanos}` alone can collide, and a
        // collision would let one `TempSnapshot`'s `Drop` delete a sibling's
        // file mid-read.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("sidle-backup-{}-{nanos}-{seq}.db", std::process::id());
        Ok(Self { path: std::env::temp_dir().join(name) })
    }
}

impl Drop for TempSnapshot {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        // VACUUM INTO + a read-only reader create no sidecars, but be defensive.
        for suffix in ["-wal", "-shm"] {
            let mut s = self.path.clone().into_os_string();
            s.push(suffix);
            let _ = fs::remove_file(PathBuf::from(s));
        }
    }
}

/// Open the snapshot read-only and read book/annotation counts + the full sha
/// list + the notebook uuid list, so the file set we archive matches the DB
/// snapshot exactly.
fn read_snapshot_inventory(db_path: &Path) -> Result<SnapshotInventory> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open snapshot {}", db_path.display()))?;
    let books: i64 = conn
        .query_row("SELECT COUNT(*) FROM books", [], |r| r.get(0))
        .context("count books")?;
    let annotations: i64 = conn
        .query_row("SELECT COUNT(*) FROM annotations", [], |r| r.get(0))
        .context("count annotations")?;
    let mut stmt = conn.prepare("SELECT sha256 FROM books").context("prepare sha select")?;
    let shas = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .context("query shas")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("collect shas")?;
    // `notebooks` is additive and may be absent on a very old snapshot; tolerate
    // a missing table by reporting no uuids rather than erroring.
    let notebook_uuids = match conn.prepare("SELECT uuid FROM notebooks") {
        Ok(mut stmt) => stmt
            .query_map([], |r| r.get::<_, String>(0))
            .context("query notebook uuids")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("collect notebook uuids")?,
        Err(_) => Vec::new(),
    };
    Ok(SnapshotInventory { books, annotations, shas, notebook_uuids })
}

/// What [`read_snapshot_inventory`] returns: the counts plus the on-disk
/// directory keys (book shas, notebook uuids) to archive.
struct SnapshotInventory {
    books: i64,
    annotations: i64,
    shas: Vec<String>,
    notebook_uuids: Vec<String>,
}

/// Stream a single file into the zip under `name`.
fn add_file(zw: &mut ZipWriter<File>, name: &str, path: &Path, opts: SimpleFileOptions) -> Result<()> {
    zw.start_file(name, opts).with_context(|| format!("zip entry {name}"))?;
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    io::copy(&mut f, zw).with_context(|| format!("write {name} into zip"))?;
    Ok(())
}

/// Recursively add the contents of `dir` into the zip under `prefix`, entries
/// sorted for a deterministic archive.
fn add_dir(zw: &mut ZipWriter<File>, prefix: &str, dir: &Path, opts: SimpleFileOptions) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .collect::<io::Result<Vec<_>>>()
        .with_context(|| format!("list {}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let child = format!("{prefix}/{name}");
        let ft = entry.file_type()?;
        if ft.is_dir() {
            add_dir(zw, &child, &entry.path(), opts)?;
        } else if ft.is_file() {
            add_file(zw, &child, &entry.path(), opts)?;
        }
        // symlinks / specials are skipped — managed libraries hold only regular files.
    }
    Ok(())
}

fn read_manifest(archive: &mut ZipArchive<File>) -> Result<Manifest> {
    let mut entry = archive
        .by_name("manifest.json")
        .context("archive has no manifest.json — not a sidle backup")?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf).context("read manifest.json")?;
    serde_json::from_str(&buf).context("parse manifest.json")
}

/// Extract every entry into `dest`, sanitizing names against zip-slip via
/// `enclosed_name` (anything that escapes is skipped).
fn extract_all(archive: &mut ZipArchive<File>, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).with_context(|| format!("zip entry {i}"))?;
        let Some(rel) = entry.enclosed_name().map(|p| p.to_path_buf()) else { continue };
        let out = dest.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out).with_context(|| format!("create {}", out.display()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let mut f = File::create(&out).with_context(|| format!("create {}", out.display()))?;
        io::copy(&mut entry, &mut f).with_context(|| format!("extract {}", out.display()))?;
    }
    Ok(())
}

/// A sibling path of `root` named `<root_name>.<suffix>`. Same parent → same
/// volume, so moving between it and `root` is a rename, not a copy. `pub(crate)`
/// so merge can stage alongside the live root the same way restore does.
pub(crate) fn sibling(root: &Path, suffix: &str) -> Result<PathBuf> {
    let parent = root.parent().ok_or_else(|| anyhow!("root {} has no parent", root.display()))?;
    let base = root.file_name().ok_or_else(|| anyhow!("root {} has no name", root.display()))?;
    let mut name = base.to_os_string();
    name.push(".");
    name.push(suffix);
    Ok(parent.join(name))
}

/// Move the library *payload* — `library.db` (+ WAL sidecars), `books/`, and
/// `notebooks/` — from `from` to `to` by rename. Leaves anything else (notably
/// `config.json`, the root pointer when `from` is the state dir) in place.
fn move_payload(from: &Path, to: &Path) -> Result<()> {
    for name in ["library.db", "library.db-wal", "library.db-shm"] {
        let src = from.join(name);
        if src.exists() {
            let dst = to.join(name);
            fs::rename(&src, &dst)
                .with_context(|| format!("move {} -> {}", src.display(), dst.display()))?;
        }
    }
    for tree in ["books", "notebooks"] {
        let src = from.join(tree);
        if src.exists() {
            let dst = to.join(tree);
            fs::rename(&src, &dst)
                .with_context(|| format!("move {} -> {}", src.display(), dst.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 2-book library under `root` (canonicalized — see the §4a symlink
    /// caveat: a `/var`→`/private/var` root would make relativization a no-op).
    /// Returns the open connection and the `books/` dir.
    fn seed_library(root: &Path) -> (Connection, PathBuf) {
        let conn = db::open(&root.join("library.db")).unwrap();
        let books = root.join("books");
        for (sha, title) in [("aaa", "One"), ("bbb", "Two")] {
            let dir = books.join(sha);
            fs::create_dir_all(&dir).unwrap();
            let epub = dir.join("book.epub");
            fs::write(&epub, format!("epub-bytes-{sha}")).unwrap();
            let cover = dir.join("cover.jpg");
            fs::write(&cover, format!("cover-bytes-{sha}")).unwrap();
            let epub_s = epub.to_string_lossy().into_owned();
            let cover_s = cover.to_string_lossy().into_owned();
            let id = db::insert_book(
                &conn,
                &db::NewBook {
                    sha256: sha,
                    title,
                    author: "Author",
                    language: "en",
                    ppd: None,
                    epub_path: Some(&epub_s),
                    cover_path: Some(&cover_s),
                    kfx_path: None,
                    kfx_sha256: None,
                    pdf_path: None,
                    file_size: 0,
                    imported_at: "t",
                    asin: None,
                    publisher: None,
                    published_at: None,
                    series_name: None,
                    series_index: None,
                    tags: &["fav".to_string()],
                },
            )
            .unwrap();
            // one precious annotation + a sidle reading position per book.
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
                    text: "precious",
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
            .unwrap();
            db::set_reading_position(&conn, id, Some(2), Some(7), Some(42), "sidle", "").unwrap();
        }
        // One Scribe notebook with a rendered page — guards the v2 archive: the
        // `notebooks/` tree was previously omitted, so a restore silently dropped
        // notebook files. It lives a level up from `books/`, under the same root.
        let pages = root.join("notebooks").join("nb-1").join("pages");
        fs::create_dir_all(&pages).unwrap();
        fs::write(pages.join("page-0.svg"), "svg-nb-1").unwrap();
        db::upsert_notebook(&conn, "nb-1", 1, "nbk-sha", "t", "2026-02-02T00:00:00+00:00")
            .unwrap();
        (conn, books)
    }

    #[test]
    fn create_then_restore_roundtrips() {
        // Source library on a canonicalized root.
        let src = tempfile::tempdir().unwrap();
        let src_root = src.path().canonicalize().unwrap();
        let (conn, books_dir) = seed_library(&src_root);

        let out = tempfile::tempdir().unwrap();
        let zip = out.path().join("library.sidlebak");
        let manifest = create(&conn, &books_dir, &src_root, "test-1.0", &zip).unwrap();
        assert_eq!(manifest.counts.books, 2);
        assert_eq!(manifest.counts.annotations, 2);
        assert_eq!(manifest.counts.book_dirs, 2);
        assert_eq!(manifest.counts.notebooks, 1, "notebook tree archived (format v2)");
        drop(conn);

        // Restore into a fresh root with a DIFFERENT absolute path.
        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().join("Relocated");
        fs::create_dir_all(&dst_root).unwrap();
        let dst_root = dst_root.canonicalize().unwrap();
        let outcome = restore(&zip, &dst_root, db::SCHEMA_VERSION).unwrap();
        assert_eq!(outcome.books, 2);
        assert!(outcome.safety_copy.exists(), "safety copy kept as undo");

        // Restored DB opens, counts + precious rows survive, files byte-identical.
        let rconn = db::open(&dst_root.join("library.db")).unwrap();
        let rows = db::list_books(&rconn).unwrap();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.tags, vec!["fav".to_string()], "user tag carried");
            let epub = row.epub_path.as_ref().expect("epub path");
            // Resolved to absolute UNDER the new root (cross-root portability, §4a).
            assert!(Path::new(epub).starts_with(&dst_root), "{epub} under {dst_root:?}");
            assert_eq!(
                fs::read_to_string(epub).unwrap(),
                format!("epub-bytes-{}", row.sha256),
                "epub bytes identical"
            );
            let cover = row.cover_path.as_ref().expect("cover path");
            assert_eq!(
                fs::read_to_string(cover).unwrap(),
                format!("cover-bytes-{}", row.sha256)
            );
            let anns = db::list_annotations_for_book(&rconn, row.id).unwrap();
            assert_eq!(anns.len(), 1, "annotation carried");
            assert_eq!(anns[0].text, "precious");
            let pos = db::list_reading_positions(&rconn, row.id).unwrap();
            assert_eq!(pos.len(), 1, "reading position carried");
            assert_eq!(pos[0].eid, Some(2));
        }

        // The notebook row AND its rendered page survive — the v2 gap fix (a v1
        // archive carried the row in the DB but lost the files).
        let nb = db::get_notebook_by_uuid(&rconn, "nb-1").unwrap().expect("notebook row carried");
        assert_eq!(nb.page_count, 1);
        assert_eq!(
            fs::read_to_string(dst_root.join("notebooks/nb-1/pages/page-0.svg")).unwrap(),
            "svg-nb-1",
            "notebook page bytes identical after roundtrip"
        );

        // Relativization invariant: the stored columns remain root-relative after
        // a backup→restore roundtrip (a regression here would dangle on the next move).
        let raw = Connection::open_with_flags(
            dst_root.join("library.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut stmt = raw.prepare("SELECT epub_path FROM books").unwrap();
        let stored: Vec<String> =
            stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        assert!(
            stored.iter().all(|p| !p.starts_with('/')),
            "stored paths stay relative: {stored:?}"
        );
    }

    #[test]
    fn restore_over_existing_library_moves_old_aside() {
        // Source: the standard 2-book library, backed up.
        let src = tempfile::tempdir().unwrap();
        let src_root = src.path().canonicalize().unwrap();
        let (conn, books_dir) = seed_library(&src_root);
        let out = tempfile::tempdir().unwrap();
        let zip = out.path().join("library.sidlebak");
        create(&conn, &books_dir, &src_root, "test-1.0", &zip).unwrap();
        drop(conn);

        // Destination: a DIFFERENT, populated library (one book "zzz").
        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().join("Live");
        fs::create_dir_all(&dst_root).unwrap();
        let dst_root = dst_root.canonicalize().unwrap();
        {
            let dconn = db::open(&dst_root.join("library.db")).unwrap();
            let dir = dst_root.join("books/zzz");
            fs::create_dir_all(&dir).unwrap();
            let epub = dir.join("book.epub");
            fs::write(&epub, "OLD-zzz").unwrap();
            let epub_s = epub.to_string_lossy().into_owned();
            db::insert_book(
                &dconn,
                &db::NewBook {
                    sha256: "zzz",
                    title: "Old",
                    author: "",
                    language: "",
                    ppd: None,
                    epub_path: Some(&epub_s),
                    cover_path: None,
                    kfx_path: None,
                    kfx_sha256: None,
                    pdf_path: None,
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
            .unwrap();
        }

        let outcome = restore(&zip, &dst_root, db::SCHEMA_VERSION).unwrap();
        assert_eq!(outcome.books, 2);

        // The restored library is live; the old book is gone from the root.
        let rconn = db::open(&dst_root.join("library.db")).unwrap();
        let shas: Vec<String> =
            db::list_books(&rconn).unwrap().into_iter().map(|b| b.sha256).collect();
        assert!(shas.contains(&"aaa".to_string()) && shas.contains(&"bbb".to_string()));
        assert!(!shas.contains(&"zzz".to_string()), "old book replaced");
        assert!(dst_root.join("books/aaa/book.epub").is_file());
        assert!(!dst_root.join("books/zzz").exists(), "old book dir gone from live root");

        // The pre-restore library is preserved intact in the safety copy (the undo).
        assert!(outcome.safety_copy.join("library.db").is_file());
        assert_eq!(
            fs::read_to_string(outcome.safety_copy.join("books/zzz/book.epub")).unwrap(),
            "OLD-zzz"
        );
    }

    #[test]
    fn restore_refuses_newer_schema() {
        let src = tempfile::tempdir().unwrap();
        let src_root = src.path().canonicalize().unwrap();
        let (conn, books_dir) = seed_library(&src_root);
        let out = tempfile::tempdir().unwrap();
        let zip = out.path().join("library.sidlebak");
        let manifest = create(&conn, &books_dir, &src_root, "test-1.0", &zip).unwrap();

        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().join("Relocated");
        fs::create_dir_all(&dst_root).unwrap();
        // Pretend the app is one schema version behind the backup.
        let err = restore(&zip, &dst_root, manifest.db_user_version - 1).unwrap_err();
        assert!(err.to_string().contains("schema"), "got: {err}");
        // Gate runs before any disk mutation → target untouched, no staging left.
        assert!(!dst_root.join("library.db").exists(), "target untouched");
        assert!(!sibling(&dst_root, "restoring").unwrap().exists(), "no staging left");
    }

    #[test]
    fn restore_rejects_corrupt_db_checksum() {
        let src = tempfile::tempdir().unwrap();
        let src_root = src.path().canonicalize().unwrap();
        let (conn, books_dir) = seed_library(&src_root);
        let out = tempfile::tempdir().unwrap();
        let good = out.path().join("good.sidlebak");
        create(&conn, &books_dir, &src_root, "test-1.0", &good).unwrap();
        drop(conn);

        // Repackage with a manifest whose db_sha256 is wrong but everything else
        // valid (format/version/schema/counts all fine), so the only thing that
        // can reject it is the integrity check.
        let tampered = out.path().join("tampered.sidlebak");
        {
            let mut ar = ZipArchive::new(File::open(&good).unwrap()).unwrap();
            let mut manifest: Manifest = {
                let mut s = String::new();
                ar.by_name("manifest.json").unwrap().read_to_string(&mut s).unwrap();
                serde_json::from_str(&s).unwrap()
            };
            manifest.db_sha256 = "0".repeat(64); // not the real hash
            let mut zw = ZipWriter::new(File::create(&tampered).unwrap());
            let opts = SimpleFileOptions::default();
            zw.start_file("manifest.json", opts).unwrap();
            zw.write_all(&serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
            for i in 0..ar.len() {
                let mut e = ar.by_index(i).unwrap();
                let name = e.name().to_string();
                if name == "manifest.json" {
                    continue;
                }
                zw.start_file(name, opts).unwrap();
                io::copy(&mut e, &mut zw).unwrap();
            }
            zw.finish().unwrap();
        }

        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().join("Relocated");
        fs::create_dir_all(&dst_root).unwrap();
        let err = restore(&tampered, &dst_root, db::SCHEMA_VERSION).unwrap_err();
        assert!(err.to_string().contains("checksum"), "got: {err}");
        // Verify fails AFTER extraction, so the swap never runs: target untouched
        // and the staging dir is cleaned up.
        assert!(!dst_root.join("library.db").exists(), "target untouched");
        assert!(!sibling(&dst_root, "restoring").unwrap().exists(), "staging cleaned up");
    }

    #[test]
    fn restore_rejects_foreign_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let zpath = tmp.path().join("foreign.zip");
        {
            let f = File::create(&zpath).unwrap();
            let mut zw = ZipWriter::new(f);
            zw.start_file("hello.txt", SimpleFileOptions::default()).unwrap();
            zw.write_all(b"not a backup").unwrap();
            zw.finish().unwrap();
        }
        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().join("Relocated");
        fs::create_dir_all(&dst_root).unwrap();
        assert!(restore(&zpath, &dst_root, db::SCHEMA_VERSION).is_err());
        assert!(!dst_root.join("library.db").exists(), "target untouched");
    }
}
