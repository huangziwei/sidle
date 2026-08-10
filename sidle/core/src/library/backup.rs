//! Library backup & restore — a single `.sidlebak` archive (a zip) of the whole
//! library, restorable on this or another machine.
//!
//! Archive layout:
//! ```text
//! manifest.json              # format/version, counts, db_sha256 (integrity + schema gate)
//! library.db                 # consistent VACUUM INTO snapshot (deflate)
//! books/<sha>/...            # the whole tree, verbatim (store — already-compressed media)
//! notebooks/<uuid>/...       # Scribe handwriting, same treatment
//! <everything else>          # every other root entry, swept (see `excluded_from_archive`)
//! ```
//!
//! `books/` and `notebooks/` are enumerated from the DB snapshot, so the archived
//! file set matches the archived rows exactly. Everything else at the root is
//! swept in by default and only a named exclusion list keeps anything out — the
//! opposite default from the first two versions of this format, which named what
//! to include and silently dropped user data twice for it (notebook files until
//! v2, `device-backup/` until v3).
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
///
/// v3: every other root entry is swept in too — `device-backup/` (the Kindle
/// screenshots and picker logs the Misc tab shows, which are files on disk and
/// nowhere in the DB, so v2 archives carried no trace of them), `cover-thumb.fmt`,
/// `.server-token`, and anything added later. An older archive restores into a
/// v3 app unchanged; it simply has fewer entries to extract.
const FORMAT_VERSION: u32 = 3;

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

/// What becomes of the library a restore replaces.
///
/// The swap sets it aside either way — that is a rename, so it is instant and
/// costs no space, and it means every original file is still intact if the move
/// of the restored payload fails partway. This decides only what happens after
/// that move succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviousLibrary {
    /// Leave it at `<root>.bak-<ts>` as the undo. Nothing removes it later, so
    /// the disk carries both libraries until the user deletes it.
    Keep,
    /// Delete it once the restored library is in place, freeing the space it
    /// held. The restore is then final: the archive is the only other copy.
    Discard,
}

/// Result of a restore, surfaced to the UI: what came in, and where the
/// pre-restore library still sits.
#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    pub books: i64,
    pub annotations: i64,
    /// The set-aside library's directory, or `None` once it has been removed.
    /// This reports the disk, not the request — a [`PreviousLibrary::Discard`]
    /// whose removal failed still names the directory, because the space is
    /// still taken and the caller has something to say about it.
    pub safety_copy: Option<PathBuf>,
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
    create_archive_with_progress(
        snapshot,
        books_dir,
        source_root,
        app_version,
        db_user_version,
        dest_zip,
        &|_, _| {},
    )
}

/// Like [`create_archive`], but ticks `on_progress(dirs_done, dirs_total)` after
/// each book/notebook directory is zipped — drives the footer "Backing up …"
/// counter. Zipping the book tree is the long pole; the manifest + DB write
/// ahead of it are quick.
pub fn create_archive_with_progress(
    snapshot: &TempSnapshot,
    books_dir: &Path,
    source_root: &Path,
    app_version: &str,
    db_user_version: i64,
    dest_zip: &Path,
    on_progress: &dyn Fn(u64, u64),
) -> Result<Manifest> {
    // (1) Counts + the dir keys read FROM THE SNAPSHOT (read-only, so we don't
    //     mutate the bytes we're about to hash), so the manifest is internally
    //     consistent with the archived DB + file set.
    let inv = read_snapshot_inventory(&snapshot.path)?;
    let book_dirs = inv
        .shas
        .iter()
        .filter(|s| books_dir.join(s).is_dir())
        .count() as i64;
    // Notebook dirs live a level up from `books/` — a `notebooks/` sibling under
    // the same root.
    let notebooks_dir = source_root.join("notebooks");
    let notebook_dirs = inv
        .notebook_uuids
        .iter()
        .filter(|u| notebooks_dir.join(u).is_dir())
        .count() as i64;

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
    let file = File::create(dest_zip).with_context(|| format!("create {}", dest_zip.display()))?;
    let mut zw = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let manifest_json = serde_json::to_vec_pretty(&manifest).context("serialize manifest")?;
    zw.start_file("manifest.json", deflated)
        .context("zip manifest.json")?;
    zw.write_all(&manifest_json)
        .context("write manifest.json")?;

    add_file(&mut zw, "library.db", &snapshot.path, deflated)?;

    // Progress is per archived entry (books + notebooks + the swept remainder) —
    // the unit that dominates wall-clock. `book_dirs`/`notebook_dirs` already
    // counted the ones that exist on disk, so the loop bodies and the total agree.
    let extras = root_extras(source_root)?;
    let total_dirs = (book_dirs + notebook_dirs).max(0) as u64 + extras.len() as u64;
    let mut done_dirs = 0u64;
    for sha in &inv.shas {
        let dir = books_dir.join(sha);
        if dir.is_dir() {
            add_dir(&mut zw, &format!("books/{sha}"), &dir, stored)?;
            done_dirs += 1;
            on_progress(done_dirs, total_dirs);
        }
    }

    // The notebook tree (Scribe handwriting) — a `notebooks/<uuid>/` sibling of
    // `books/`. Stored, like the book tree: the page SVGs are text but small,
    // and `nbk`/`cover.png` are already compact. v2 of the archive format.
    for uuid in &inv.notebook_uuids {
        let dir = notebooks_dir.join(uuid);
        if dir.is_dir() {
            add_dir(&mut zw, &format!("notebooks/{uuid}"), &dir, stored)?;
            done_dirs += 1;
            on_progress(done_dirs, total_dirs);
        }
    }

    // Everything else the root holds, at its own name: `device-backup/` above
    // all — the Misc tab's screenshots and picker logs, which exist only as
    // files and would otherwise be in no backup at all — plus `cover-thumb.fmt`,
    // `.server-token`, and whatever lands there next. Deflated rather than
    // stored: this is small and mostly text (logs, markers, a token), and the
    // screenshots are the only already-compressed thing in it. v3.
    for path in &extras {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            add_dir(&mut zw, &name, path, deflated)?;
        } else if path.is_file() {
            add_file(&mut zw, &name, path, deflated)?;
        }
        done_dirs += 1;
        on_progress(done_dirs, total_dirs);
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
    create_archive(
        &snap,
        books_dir,
        source_root,
        app_version,
        db_user_version,
        dest_zip,
    )
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
    on_progress: &dyn Fn(u64, u64),
) -> Result<Manifest> {
    let file = File::open(src_zip).with_context(|| format!("open {}", src_zip.display()))?;
    let mut archive =
        ZipArchive::new(file).with_context(|| format!("{} is not a zip", src_zip.display()))?;

    // Manifest: validate shape, then gate on format + schema version — all before
    // any disk mutation.
    let manifest = read_manifest(&mut archive)?;
    if manifest.format != FORMAT_TAG {
        bail!(
            "not a sidle library backup (format = {:?})",
            manifest.format
        );
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
    extract_all(&mut archive, staging, on_progress)?;

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
/// with the manifest's book count), then swaps: everything the current root
/// holds is moved aside to a `<root>.bak-<ts>` directory and the staged payload
/// moved into place — renames only (staging + safety are siblings of
/// `dest_root`, hence same volume). `previous` decides whether that set-aside
/// library is then kept as the undo or deleted. `config.json` (if `dest_root` is
/// the state dir) is the one thing left where it is. The caller relaunches
/// afterward (H5).
pub fn restore(
    src_zip: &Path,
    dest_root: &Path,
    app_user_version: i64,
    previous: PreviousLibrary,
) -> Result<RestoreOutcome> {
    restore_with_progress(src_zip, dest_root, app_user_version, previous, &|_, _| {})
}

/// Like [`restore`], but ticks `on_progress(entries_done, entries_total)` over
/// the archive extraction — the slow phase (the verify + rename swap that follow
/// are quick). Drives the footer "Restoring …" counter.
pub fn restore_with_progress(
    src_zip: &Path,
    dest_root: &Path,
    app_user_version: i64,
    previous: PreviousLibrary,
    on_progress: &dyn Fn(u64, u64),
) -> Result<RestoreOutcome> {
    // (1)+(2) Validate + gate the manifest, extract into a sibling staging dir,
    //     verify the DB checksum — the shared front-half, identical to merge
    //     (see [`stage_archive`]). Same volume → the later swap is a rename.
    let staging = sibling(dest_root, "restoring")?;
    let manifest = stage_archive(src_zip, &staging, app_user_version, on_progress)?;

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

    // (4) Swap. Move the current payload aside, then the staged payload into
    //     place. The live app's open Connection points at the old library.db
    //     inode (now under the set-aside dir); we relaunch right after, so the
    //     next process opens the restored files.
    //
    //     Aside first even when the caller wants it gone: the move is a rename,
    //     so it costs nothing, and it means a failure in the second move leaves
    //     every original file intact rather than half-deleted.
    let safety = sibling(
        dest_root,
        &format!("bak-{}", Utc::now().format("%Y%m%d-%H%M%S")),
    )?;
    fs::create_dir_all(&safety).with_context(|| format!("create {}", safety.display()))?;
    move_payload(dest_root, &safety).context("set current library aside")?;
    fs::create_dir_all(dest_root).with_context(|| format!("recreate {}", dest_root.display()))?;
    move_payload(&staging, dest_root).context("move restored library into place")?;
    let _ = fs::remove_dir(&staging); // now empty

    // (5) Only now, with the restored library live, is the old one expendable.
    //     A removal that fails is not a failed restore — the restore is done and
    //     correct — so it is reported as a directory still on disk, not an error.
    let safety_copy = match previous {
        PreviousLibrary::Keep => Some(safety),
        PreviousLibrary::Discard => match fs::remove_dir_all(&safety) {
            Ok(()) => None,
            Err(_) => Some(safety),
        },
    };

    Ok(RestoreOutcome {
        books: manifest.counts.books,
        annotations: manifest.counts.annotations,
        safety_copy,
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
        Ok(Self {
            path: std::env::temp_dir().join(name),
        })
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
/// Root entries the archive leaves out, and the only reason anything is left
/// out: the default is to carry it. An allow-list is what let this format ship
/// twice without user data it should have had, so a tree nobody here has heard
/// of is archived rather than dropped.
fn excluded_from_archive(name: &str) -> bool {
    matches!(
        name,
        // Already in the archive, enumerated from the DB's own inventory.
        "books" | "notebooks"
        // `library.db` in the archive IS the snapshot. The live file and its WAL
        // sidecars are a different, mid-write moment of the same DB.
        | "library.db" | "library.db-wal" | "library.db-shm"
        // The root pointer says where THIS machine keeps its library. It belongs
        // to the machine, not to the copy of the library.
        | "config.json"
        // The LAN daemon's runtime, valid only for the process running now: a
        // PID naming one of this machine's processes, its log, and the two pulse
        // files it writes to poke the app.
        | "server.pid" | "server.log" | ".sync-pulse.json" | ".book-pulse.json"
        // Staged out of the app bundle whenever a device needs it.
        | "device-dist"
        // Finder droppings, not ours.
        | ".DS_Store"
    )
}

/// Every root entry [`excluded_from_archive`] does not name, sorted so the
/// archive is deterministic. A root that is not there yet has nothing to sweep;
/// a root that refuses to be listed fails the backup rather than quietly
/// producing an archive missing everything this sweep exists to carry.
fn root_extras(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("list {}", root.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("list {}", root.display()))?;
        if !excluded_from_archive(&entry.file_name().to_string_lossy()) {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

fn read_snapshot_inventory(db_path: &Path) -> Result<SnapshotInventory> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open snapshot {}", db_path.display()))?;
    let books: i64 = conn
        .query_row("SELECT COUNT(*) FROM books", [], |r| r.get(0))
        .context("count books")?;
    let annotations: i64 = conn
        .query_row("SELECT COUNT(*) FROM annotations", [], |r| r.get(0))
        .context("count annotations")?;
    let mut stmt = conn
        .prepare("SELECT sha256 FROM books")
        .context("prepare sha select")?;
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
    Ok(SnapshotInventory {
        books,
        annotations,
        shas,
        notebook_uuids,
    })
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
fn add_file(
    zw: &mut ZipWriter<File>,
    name: &str,
    path: &Path,
    opts: SimpleFileOptions,
) -> Result<()> {
    zw.start_file(name, opts)
        .with_context(|| format!("zip entry {name}"))?;
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    io::copy(&mut f, zw).with_context(|| format!("write {name} into zip"))?;
    Ok(())
}

/// Recursively add the contents of `dir` into the zip under `prefix`, entries
/// sorted for a deterministic archive.
fn add_dir(
    zw: &mut ZipWriter<File>,
    prefix: &str,
    dir: &Path,
    opts: SimpleFileOptions,
) -> Result<()> {
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
    entry
        .read_to_string(&mut buf)
        .context("read manifest.json")?;
    serde_json::from_str(&buf).context("parse manifest.json")
}

/// Extract every entry into `dest`, sanitizing names against zip-slip via
/// `enclosed_name` (anything that escapes is skipped).
fn extract_all(
    archive: &mut ZipArchive<File>,
    dest: &Path,
    on_progress: &dyn Fn(u64, u64),
) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    let total = archive.len() as u64;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("zip entry {i}"))?;
        // `enclosed_name() == None` is the zip-slip guard (rejects `../` and
        // absolute paths); skip those entries but still count them so progress
        // reaches `total`.
        if let Some(rel) = entry.enclosed_name().map(|p| p.to_path_buf()) {
            let out = dest.join(&rel);
            if entry.is_dir() {
                fs::create_dir_all(&out).with_context(|| format!("create {}", out.display()))?;
            } else {
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create {}", parent.display()))?;
                }
                let mut f =
                    File::create(&out).with_context(|| format!("create {}", out.display()))?;
                io::copy(&mut entry, &mut f)
                    .with_context(|| format!("extract {}", out.display()))?;
            }
        }
        on_progress(i as u64 + 1, total);
    }
    Ok(())
}

/// A sibling path of `root` named `<root_name>.<suffix>`. Same parent → same
/// volume, so moving between it and `root` is a rename, not a copy. `pub(crate)`
/// so merge can stage alongside the live root the same way restore does.
pub(crate) fn sibling(root: &Path, suffix: &str) -> Result<PathBuf> {
    let parent = root
        .parent()
        .ok_or_else(|| anyhow!("root {} has no parent", root.display()))?;
    let base = root
        .file_name()
        .ok_or_else(|| anyhow!("root {} has no name", root.display()))?;
    let mut name = base.to_os_string();
    name.push(".");
    name.push(suffix);
    Ok(parent.join(name))
}

/// Move the library *payload* from `from` to `to` by rename — everything the
/// directory holds except `config.json`, the root pointer, which stays behind
/// when `from` is the state dir.
///
/// Everything, not a list of trees: the set-aside has to empty the root of the
/// old library, or a swept entry that exists on both sides (`device-backup/` is
/// the first) meets its counterpart when the restored payload moves in, and
/// renaming a directory onto a non-empty one fails.
fn move_payload(from: &Path, to: &Path) -> Result<()> {
    let entries = match fs::read_dir(from) {
        Ok(entries) => entries,
        // A root that isn't there holds nothing to set aside.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("list {}", from.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("list {}", from.display()))?;
        let name = entry.file_name();
        if name == "config.json" {
            continue;
        }
        let src = entry.path();
        let dst = to.join(&name);
        fs::rename(&src, &dst)
            .with_context(|| format!("move {} -> {}", src.display(), dst.display()))?;
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
                    title_romaji: "",
                    author_romaji: "",
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
        db::upsert_notebook(
            &conn,
            "nb-1",
            1,
            "nbk-sha",
            "t",
            "2026-02-02T00:00:00+00:00",
        )
        .unwrap();
        // The swept remainder (v3). `device-backup/` is the case that motivated
        // it — screenshots and picker logs exist as files and are named nowhere
        // in the DB, so an inventory-driven archive could not see them at all.
        // `cover-thumb.fmt` stands for the small root markers beside it.
        let shots = root.join("device-backup").join("G000").join("screenshots");
        fs::create_dir_all(&shots).unwrap();
        fs::write(shots.join("screenshot_1.png"), "png-bytes").unwrap();
        fs::write(root.join("cover-thumb.fmt"), "j").unwrap();
        // Excluded, and seeded here so the exclusions are exercised rather than
        // asserted about an empty root: this machine's daemon PID and Finder junk.
        fs::write(root.join("server.pid"), "4242").unwrap();
        fs::write(root.join(".DS_Store"), "finder").unwrap();
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
        assert_eq!(
            manifest.counts.notebooks, 1,
            "notebook tree archived (format v2)"
        );
        drop(conn);

        // Restore into a fresh root with a DIFFERENT absolute path.
        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().join("Relocated");
        fs::create_dir_all(&dst_root).unwrap();
        let dst_root = dst_root.canonicalize().unwrap();
        let outcome = restore(&zip, &dst_root, db::SCHEMA_VERSION, PreviousLibrary::Keep).unwrap();
        assert_eq!(outcome.books, 2);
        assert!(
            outcome.safety_copy.as_ref().expect("kept").exists(),
            "safety copy kept as undo"
        );

        // Restored DB opens, counts + precious rows survive, files byte-identical.
        let rconn = db::open(&dst_root.join("library.db")).unwrap();
        let rows = db::list_books(&rconn).unwrap();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.tags, vec!["fav".to_string()], "user tag carried");
            let epub = row.epub_path.as_ref().expect("epub path");
            // Resolved to absolute UNDER the new root (cross-root portability, §4a).
            assert!(
                Path::new(epub).starts_with(&dst_root),
                "{epub} under {dst_root:?}"
            );
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
        let nb = db::get_notebook_by_uuid(&rconn, "nb-1")
            .unwrap()
            .expect("notebook row carried");
        assert_eq!(nb.page_count, 1);
        assert_eq!(
            fs::read_to_string(dst_root.join("notebooks/nb-1/pages/page-0.svg")).unwrap(),
            "svg-nb-1",
            "notebook page bytes identical after roundtrip"
        );

        // The swept remainder (v3): the Misc tab's screenshots — files the DB
        // never names, so no inventory-driven archive could have found them —
        // and the root markers beside them.
        assert_eq!(
            fs::read_to_string(dst_root.join("device-backup/G000/screenshots/screenshot_1.png"))
                .unwrap(),
            "png-bytes",
            "device-backup carried, bytes identical"
        );
        assert_eq!(
            fs::read_to_string(dst_root.join("cover-thumb.fmt")).unwrap(),
            "j"
        );

        // …and only the remainder: this machine's daemon PID would name one of
        // its processes on whatever machine restored the archive, and the Finder
        // junk is not ours to carry.
        assert!(!dst_root.join("server.pid").exists(), "server.pid excluded");
        assert!(!dst_root.join(".DS_Store").exists(), ".DS_Store excluded");

        // Relativization invariant: the stored columns remain root-relative after
        // a backup→restore roundtrip (a regression here would dangle on the next move).
        let raw = Connection::open_with_flags(
            dst_root.join("library.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut stmt = raw.prepare("SELECT epub_path FROM books").unwrap();
        let stored: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
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
                    title_romaji: "",
                    author_romaji: "",
                },
            )
            .unwrap();
        }
        // The destination has its own `device-backup/` — the entry that exists
        // on BOTH sides of the swap. Setting the old root aside wholesale is
        // what keeps the staged one's rename from meeting a non-empty directory.
        let dst_shots = dst_root.join("device-backup/G999/screenshots");
        fs::create_dir_all(&dst_shots).unwrap();
        fs::write(dst_shots.join("old.png"), "OLD-png").unwrap();
        // And its own root pointer, which is the machine's and must survive.
        fs::write(dst_root.join("config.json"), "{}").unwrap();

        let outcome = restore(&zip, &dst_root, db::SCHEMA_VERSION, PreviousLibrary::Keep).unwrap();
        assert_eq!(outcome.books, 2);

        // The archive's device-backup replaced the destination's, and the root
        // pointer stayed where it was.
        assert!(
            dst_root
                .join("device-backup/G000/screenshots/screenshot_1.png")
                .is_file()
        );
        assert!(
            !dst_root.join("device-backup/G999").exists(),
            "old device-backup replaced, not merged into"
        );
        assert_eq!(
            fs::read_to_string(dst_root.join("config.json")).unwrap(),
            "{}",
            "root pointer belongs to the machine and is left alone"
        );

        // The restored library is live; the old book is gone from the root.
        let rconn = db::open(&dst_root.join("library.db")).unwrap();
        let shas: Vec<String> = db::list_books(&rconn)
            .unwrap()
            .into_iter()
            .map(|b| b.sha256)
            .collect();
        assert!(shas.contains(&"aaa".to_string()) && shas.contains(&"bbb".to_string()));
        assert!(!shas.contains(&"zzz".to_string()), "old book replaced");
        assert!(dst_root.join("books/aaa/book.epub").is_file());
        assert!(
            !dst_root.join("books/zzz").exists(),
            "old book dir gone from live root"
        );

        // The pre-restore library is preserved intact in the safety copy (the
        // undo) — all of it, not just the DB and the books.
        let safety = outcome.safety_copy.expect("kept");
        assert!(safety.join("library.db").is_file());
        assert_eq!(
            fs::read_to_string(safety.join("books/zzz/book.epub")).unwrap(),
            "OLD-zzz"
        );
        assert_eq!(
            fs::read_to_string(safety.join("device-backup/G999/screenshots/old.png")).unwrap(),
            "OLD-png",
            "the undo holds the screenshots it replaced too"
        );
    }

    /// The other half of the choice: `Discard` leaves no set-aside library
    /// behind, so the space the replaced one held comes back.
    #[test]
    fn restore_discarding_previous_removes_the_old_library() {
        let src = tempfile::tempdir().unwrap();
        let src_root = src.path().canonicalize().unwrap();
        let (conn, books_dir) = seed_library(&src_root);
        let out = tempfile::tempdir().unwrap();
        let zip = out.path().join("library.sidlebak");
        create(&conn, &books_dir, &src_root, "test-1.0", &zip).unwrap();
        drop(conn);

        // Destination: a different, populated library, so there is something to
        // discard rather than an empty root.
        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().join("Live");
        fs::create_dir_all(dst_root.join("books/zzz")).unwrap();
        let dst_root = dst_root.canonicalize().unwrap();
        db::open(&dst_root.join("library.db")).unwrap();
        fs::write(dst_root.join("books/zzz/book.epub"), "OLD-zzz").unwrap();

        let outcome = restore(
            &zip,
            &dst_root,
            db::SCHEMA_VERSION,
            PreviousLibrary::Discard,
        )
        .unwrap();
        assert_eq!(outcome.books, 2);
        assert!(
            outcome.safety_copy.is_none(),
            "nothing set aside once discarded"
        );

        // The restored library is live and the old payload is gone from disk —
        // no `<root>.bak-*` sibling left holding it.
        assert!(dst_root.join("books/aaa/book.epub").is_file());
        assert!(!dst_root.join("books/zzz").exists());
        let leftovers: Vec<_> = fs::read_dir(dst_root.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("Live.bak-"))
            .collect();
        assert!(leftovers.is_empty(), "set-aside dirs left: {leftovers:?}");
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
        let err = restore(
            &zip,
            &dst_root,
            manifest.db_user_version - 1,
            PreviousLibrary::Keep,
        )
        .unwrap_err();
        assert!(err.to_string().contains("schema"), "got: {err}");
        // Gate runs before any disk mutation → target untouched, no staging left.
        assert!(!dst_root.join("library.db").exists(), "target untouched");
        assert!(
            !sibling(&dst_root, "restoring").unwrap().exists(),
            "no staging left"
        );
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
                ar.by_name("manifest.json")
                    .unwrap()
                    .read_to_string(&mut s)
                    .unwrap();
                serde_json::from_str(&s).unwrap()
            };
            manifest.db_sha256 = "0".repeat(64); // not the real hash
            let mut zw = ZipWriter::new(File::create(&tampered).unwrap());
            let opts = SimpleFileOptions::default();
            zw.start_file("manifest.json", opts).unwrap();
            zw.write_all(&serde_json::to_vec_pretty(&manifest).unwrap())
                .unwrap();
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
        let err = restore(
            &tampered,
            &dst_root,
            db::SCHEMA_VERSION,
            PreviousLibrary::Keep,
        )
        .unwrap_err();
        assert!(err.to_string().contains("checksum"), "got: {err}");
        // Verify fails AFTER extraction, so the swap never runs: target untouched
        // and the staging dir is cleaned up.
        assert!(!dst_root.join("library.db").exists(), "target untouched");
        assert!(
            !sibling(&dst_root, "restoring").unwrap().exists(),
            "staging cleaned up"
        );
    }

    #[test]
    fn restore_rejects_foreign_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let zpath = tmp.path().join("foreign.zip");
        {
            let f = File::create(&zpath).unwrap();
            let mut zw = ZipWriter::new(f);
            zw.start_file("hello.txt", SimpleFileOptions::default())
                .unwrap();
            zw.write_all(b"not a backup").unwrap();
            zw.finish().unwrap();
        }
        let dst = tempfile::tempdir().unwrap();
        let dst_root = dst.path().join("Relocated");
        fs::create_dir_all(&dst_root).unwrap();
        assert!(restore(&zpath, &dst_root, db::SCHEMA_VERSION, PreviousLibrary::Keep).is_err());
        assert!(!dst_root.join("library.db").exists(), "target untouched");
    }
}
