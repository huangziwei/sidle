//! Book import pipeline.
//!
//! One symmetric flow for both supported input formats — EPUB and KFX (the
//! latter possibly arriving as the multi-container `.kfx-zip` bundle Kindle
//! DeDRM produces). Whichever format comes in, the other side is filled in
//! later by the background queue:
//!
//!  - EPUB → library; pending `epub_to_kfx` job.
//!  - KFX  → library (merging `.kfx-zip` first if needed); pending a
//!    `kfx_to_epub` job.
//!
//! Three formats are converted at import time rather than stored as-is:
//!
//!  - `.azw3` → bokai's AZW3 importer feeding BOTH exporters: the EPUB
//!    and the KFX are each exported directly from the azw3's IR (the azw3
//!    itself is not persisted, so import is the only moment both can be
//!    derived without chaining). The book lands with its job already
//!    `done` — no background hop through the derived EPUB. A later
//!    metadata-edit *reconvert* re-derives the KFX from the retained EPUB
//!    (the two-hop path, keyed on the `epub_to_kfx` job kind) since the
//!    azw3 is gone by then; only the first import gets KFX straight from
//!    the azw3.
//!  - `.mobi` → bokai's MOBI importer + EPUB exporter; the KFX side is
//!    filled in later by the `epub_to_kfx` queue job like a regular EPUB
//!    drop.
//!  - `.zip`  → Aozora Bunko sniff + parse → cover → build_epub. See
//!    `convert_aozora_zip` for the sniff details.
//!
//! Steps, identical for all inputs:
//!
//!  1. Stream-hash the source file and check the dedupe index.
//!  2. Normalize to canonical bytes (`.kfx-zip` → merged `.kfx`;
//!     `.azw3`/`.mobi`/`.zip` → freshly built EPUB, with `.azw3` also
//!     deriving the KFX sibling here; other inputs are loaded verbatim).
//!  3. Read metadata from those bytes.
//!  4. Persist the canonical file into `books/<sha>/<basename>.<ext>`,
//!     plus the direct-derived KFX sibling when step 2 produced one.
//!  5. Extract the cover sidecar if we already have a readable EPUB on
//!     hand. EPUB input always does; KFX input only does on an idempotent
//!     re-import where the EPUB already exists. The fresh KFX path leaves
//!     `cover_path` empty for the worker to fill once the KFX→EPUB
//!     conversion produces an EPUB whose JXR cover has been transcoded to JPG.
//!  6. Insert book + conversion job. If the *other* side is already on
//!     disk (a direct-derived sibling, or an idempotent re-import) the job
//!     is marked `done`; otherwise it's `pending` and the caller enqueues
//!     it.
//!
//! A book the app produced rather than found — a volume carved out of a
//! collection — enters by the same steps through [`import_bytes`], which stands
//! a name in for the file it never had.
//!
//! Each step touches disk; callers should run this inside `spawn_blocking`.
//! Only steps 1 and 6 touch the library database, and the conversions in step 2
//! can run for minutes, so a caller that shares one connection behind a lock
//! should run the phases separately — [`identify_file`], then [`stage_file`]
//! with the lock released, then [`record`]. [`stage_file`] also reports what it
//! is doing, which is the only way an import of a converting format can show
//! progress.

use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::library::authors;
use crate::library::db::{self, BookRow, NewBook};
use crate::library::paths::{LibraryPaths, cover_ext_from, format_basename};

// ---------------------------------------------------------------------------
// Aozora Bunko .zip → EPUB
//
// Wires bokai's aozora pipeline into the import flow. Aozora source
// archives are unstructured .zip files: a Shift_JIS text file with
// ［＃markers］ + 底本 colophon, plus accompanying image files. We open
// the zip, sniff for those markers, parse → Document, render a
// programmatic cover JPEG (resvg), and build the EPUB. From there
// `import_one` continues with Canonical::Epub bytes, so the import is
// indistinguishable from a regular EPUB drop.
//
// "Secret feature" per user request: no UI affordance, no help text. A
// non-aozora .zip falls out as an "import failed" toast like any other
// bad input.
// ---------------------------------------------------------------------------

pub fn import_file(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    src: &Path,
) -> Result<ImportOutcome> {
    let src_kind = detect_kind(src)?;
    import_one(conn, paths, Source::File(src), src_kind, &no_progress)
}

/// The extension-detected format of a file the user is importing, or an error
/// naming what this app accepts. Exposed so a caller running the phases of an
/// import separately (see [`stage_file`]) can reject an unsupported drop before
/// hashing it.
pub fn detect_kind(src: &Path) -> Result<SourceKind> {
    let kind = SourceKind::detect(src);
    if matches!(kind, SourceKind::Unknown) {
        bail!(
            "unsupported file type: {} (expected .epub, .kfx, .kfx-zip, .azw3, .mobi, or .pdf)",
            src.display()
        );
    }
    Ok(kind)
}

/// A progress callback that reports nothing, for the callers that don't watch.
fn no_progress(_: &str, _: usize, _: usize, _: &str) {}

/// Import a book that exists only in memory — a volume carved out of a
/// collection, say. `name` stands in for a filename: its extension picks the
/// format, and its stem is the title fallback for a book whose own metadata
/// carries none.
///
/// Only the formats stored as they arrive (`.epub`, `.kfx`, `.pdf`) can be
/// imported this way; the converted ones read their source off disk.
pub fn import_bytes(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    bytes: Vec<u8>,
    name: &str,
) -> Result<ImportOutcome> {
    let src_kind = SourceKind::detect(Path::new(name));
    if matches!(src_kind, SourceKind::Unknown) {
        bail!("unsupported file type: {name} (expected .epub, .kfx, or .pdf)");
    }
    import_one(
        conn,
        paths,
        Source::Memory { name, bytes },
        src_kind,
        &no_progress,
    )
}

/// The dedupe identity of a file about to be imported: the hash of its bytes as
/// they arrived (before any conversion) and their size. Cheap — a few tens of
/// milliseconds even for a large book — and needs no database, so a caller can
/// run it before taking a lock and hand the result to [`stage_file`].
pub fn identify_file(src: &Path) -> Result<(String, i64)> {
    Source::File(src).identity()
}

/// Everything an import does that does *not* touch the library database:
/// convert the source to its canonical format, read the metadata out of it, and
/// write both sides into the library slot. The row this produced is inserted
/// separately by [`record`].
///
/// Split out because the conversion an `.azw3`, `.mobi` or `.zip` runs inline
/// can take minutes, and an app that holds one connection behind a lock must
/// not hold it for that long — every other reader would stall behind a single
/// import. `identity` is what [`identify_file`] returned, the caller having
/// already checked it against the dedupe index.
///
/// `on_progress` receives `(phase_key, current, total, human_label)`, the same
/// shape bokai's exporters report in. The keys are:
///
///  - `merge` — merging the parts of a `.kfx-zip` bundle
///  - `epub/parse`, then bokai's own EPUB-export phases under `epub/` —
///    building the canonical EPUB side
///  - `kfx/parse`, then bokai's own KFX-export phases under `kfx/` — building
///    the KFX side an `.azw3` derives at import
///  - `store` — metadata, cover sidecar, and writing into the library slot
///
/// Namespacing the two legs keeps the sequence unambiguous: both exporters end
/// in a phase called `finalize`, and a caller mapping phases to a bar needs to
/// know which one it just saw.
pub fn stage_file(
    paths: &LibraryPaths,
    src: &Path,
    identity: (String, i64),
    on_progress: &dyn Fn(&str, usize, usize, &str),
) -> Result<StagedImport> {
    let src_kind = detect_kind(src)?;
    stage(paths, Source::File(src), src_kind, identity, on_progress)
}

/// Where an import's bytes come from: a file the user dropped, or bytes the
/// app produced. Everything downstream of the normalize step works on bytes
/// either way, so the two differ only in how the source is identified and read.
enum Source<'a> {
    File(&'a Path),
    Memory { name: &'a str, bytes: Vec<u8> },
}

impl Source<'_> {
    /// How to name the source in an error message.
    fn label(&self) -> String {
        match self {
            Self::File(p) => p.display().to_string(),
            Self::Memory { name, .. } => (*name).to_string(),
        }
    }

    /// The filename without its extension — the title fallback for a book whose
    /// metadata declares none.
    fn stem(&self) -> Option<String> {
        let name = match self {
            Self::File(p) => *p,
            Self::Memory { name, .. } => Path::new(*name),
        };
        name.file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
    }

    /// The dedupe key and the size to record: the hash of the source as it
    /// arrived, before any conversion.
    fn identity(&self) -> Result<(String, i64)> {
        match self {
            Self::File(p) => {
                let sha = sha256_of_file(p).with_context(|| format!("hash {}", p.display()))?;
                let size = fs::metadata(p)
                    .with_context(|| format!("stat {}", p.display()))?
                    .len() as i64;
                Ok((sha, size))
            }
            Self::Memory { bytes, .. } => Ok((sha256_of_bytes(bytes), bytes.len() as i64)),
        }
    }

    /// The source's own bytes, for the formats stored as they arrive.
    fn read(self) -> Result<Vec<u8>> {
        match self {
            Self::File(p) => fs::read(p).with_context(|| format!("read {}", p.display())),
            Self::Memory { bytes, .. } => Ok(bytes),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Epub,
    Kfx,
    KfxZip,
    /// `.azw3` — Kindle AZW3 (decrypted). Both library sides are exported
    /// at import time, each directly from the azw3's parsed IR: EPUB (the
    /// canonical side) and KFX (the sibling normally produced later by the
    /// `epub_to_kfx` job). Detected purely by extension (no content
    /// sniff).
    Azw3,
    /// `.mobi` — older Mobipocket/KF8 hybrid (decrypted). Same shape as
    /// AZW3: bokai's MOBI importer + EPUB exporter, detected purely by
    /// extension. Japanese MOBIs carry vertical-writing-mode and PPD in
    /// EXTH 525/527, which the bokai importer propagates into the EPUB.
    Mobi,
    /// `.pdf` — wrapped verbatim into a fixed-layout PDOC KFX for the Scribe
    /// (the device renders the PDF; the pen draws over it). PDF is the
    /// canonical non-KFX side, paired with KFX (the EPUB↔KFX analogue), and
    /// the background job is `pdf_to_kfx`.
    Pdf,
    /// `.zip` extension — tentatively an Aozora Bunko archive. The actual
    /// aozora sniff (底本/［＃ markers) happens in `convert_aozora_zip`
    /// during canonical-bytes extraction; a non-aozora zip fails out
    /// there.
    AozoraZip,
    Unknown,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Canonical {
    Epub,
    Kfx,
    /// A PDF-backed book's non-KFX side (paired with KFX, never EPUB).
    Pdf,
}

impl SourceKind {
    fn detect(p: &Path) -> Self {
        match p
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("epub") => Self::Epub,
            Some("kfx") => Self::Kfx,
            Some("kfx-zip") => Self::KfxZip,
            Some("azw3") => Self::Azw3,
            Some("mobi") => Self::Mobi,
            Some("pdf") => Self::Pdf,
            Some("zip") => Self::AozoraZip,
            _ => Self::Unknown,
        }
    }

    fn canonical(self) -> Canonical {
        match self {
            Self::Epub | Self::AozoraZip | Self::Azw3 | Self::Mobi => Canonical::Epub,
            Self::Kfx | Self::KfxZip => Canonical::Kfx,
            Self::Pdf => Canonical::Pdf,
            Self::Unknown => unreachable!("filtered by import_file"),
        }
    }
}

impl Canonical {
    /// `bokai::Format` for reading metadata from the canonical bytes. PDF has
    /// none — its metadata comes from `probe_pdf` `/Info`, so this is never
    /// called for `Pdf`.
    fn bokai_format(self) -> bokai::Format {
        match self {
            Self::Epub => bokai::Format::Epub,
            Self::Kfx => bokai::Format::Kfx,
            Self::Pdf => unreachable!("PDF metadata comes from probe_pdf, not bokai::Book"),
        }
    }
}

/// The background job that converts a book imported in `from` into its `to`
/// partner side.
fn job_kind(from: Canonical, to: Canonical) -> &'static str {
    use Canonical::*;
    match (from, to) {
        (Epub, Kfx) => "epub_to_kfx",
        (Pdf, Kfx) => "pdf_to_kfx",
        (Kfx, Epub) => "kfx_to_epub",
        (Kfx, Pdf) => "kfx_to_pdf",
        _ => unreachable!("unsupported conversion {from:?} -> {to:?}"),
    }
}

fn import_one(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    src: Source<'_>,
    src_kind: SourceKind,
    on_progress: &dyn Fn(&str, usize, usize, &str),
) -> Result<ImportOutcome> {
    // 1. Hash + dedupe before doing expensive normalization (a `.kfx-zip`
    //    merge can be tens of ms; stream-hashing the source is microseconds).
    //    `record` checks again; this one is what saves the conversion.
    let identity = src.identity()?;

    if let Some(existing) = db::find_by_sha(conn, &identity.0)? {
        return Ok(ImportOutcome::Duplicate(existing));
    }

    record(conn, stage(paths, src, src_kind, identity, on_progress)?)
}

fn stage(
    paths: &LibraryPaths,
    src: Source<'_>,
    src_kind: SourceKind,
    identity: (String, i64),
    on_progress: &dyn Fn(&str, usize, usize, &str),
) -> Result<StagedImport> {
    let canonical = src_kind.canonical();
    let label = src.label();
    let stem = src.stem();
    let (sha, file_size) = identity;

    // 2. Normalize → canonical bytes. An `.azw3` additionally derives its KFX
    //    sibling here (both exports come straight from the azw3's IR), carried
    //    to the persist step below.
    let mut direct_kfx: Option<DirectKfx> = None;
    let canonical_bytes: Vec<u8> = match src_kind {
        SourceKind::Epub | SourceKind::Kfx | SourceKind::Pdf => src.read()?,
        // The converting formats all read their source off disk, so an
        // in-memory import of one has nothing to hand them. `import_bytes`
        // rejects those kinds up front; this is the belt-and-braces arm.
        _ => {
            let Source::File(path) = src else {
                bail!("{label}: this format can only be imported from a file");
            };
            match src_kind {
                SourceKind::KfxZip => {
                    on_progress("merge", 0, 1, "Merging KFX bundle");
                    bokai::formats::kfx::merge::merge_kfx_zip(path)
                        .with_context(|| format!("merge kfx-zip {label}"))?
                }
                SourceKind::Azw3 => {
                    let derived =
                        convert_azw3(path, on_progress).with_context(|| format!("azw3 {label}"))?;
                    direct_kfx = Some(derived.kfx);
                    derived.epub
                }
                SourceKind::Mobi => {
                    convert_mobi(path, on_progress).with_context(|| format!("mobi {label}"))?
                }
                SourceKind::AozoraZip => convert_aozora_zip(path, on_progress)
                    .with_context(|| format!("aozora zip {label}"))?,
                _ => unreachable!("handled above"),
            }
        }
    };
    on_progress("store", 0, 1, "Storing in the library");

    // 2b. Repair EPUBs whose producer (e.g. ScribdMpubToEpubConverter) wrote
    //     spurious ZIP64 extra fields the `zip` crate rejects. Doing it here —
    //     before metadata, cover, persist, and the downstream `epub_to_kfx`
    //     job — means the file we store is a clean archive, valid in external
    //     readers too, rather than relying on bokai's read-time repair each
    //     time. A no-op (returns the bytes unchanged) for well-formed EPUBs and
    //     for the freshly-built EPUBs the azw3/mobi/aozora paths produce.
    let canonical_bytes = if canonical == Canonical::Epub {
        bokai::formats::epub::neutralize_spurious_zip64(&canonical_bytes).unwrap_or(canonical_bytes)
    } else {
        canonical_bytes
    };

    // 3. Metadata from the canonical bytes (no file re-open). PDF metadata
    //    comes from `/Info` via `probe_pdf`; everything else from bokai.
    let mut meta = match canonical {
        Canonical::Pdf => {
            let doc = bokai::import::probe_pdf(canonical_bytes.clone())
                .with_context(|| format!("probe pdf {label}"))?;
            extract_meta_from_pdf(&doc, stem.as_deref())
        }
        _ => {
            let book = bokai::Book::from_bytes(&canonical_bytes, canonical.bokai_format())
                .with_context(|| format!("read metadata from {label}"))?;
            extract_meta(book.metadata(), stem.as_deref())
        }
    };
    // The row must carry the ASIN actually stamped inside the produced KFX —
    // device-delete keys the `.sdr` catalog cleanup on it. For an azw3 whose
    // EXTH value isn't a real Amazon ASIN, the export fabricates one, so the
    // stamped value overrides what the metadata extract saw. Same contract as
    // the worker writing `Produced::asin` back after an `epub_to_kfx` job.
    if let Some(stamped) = direct_kfx.as_ref().and_then(|k| k.asin.as_deref()) {
        meta.asin = Some(stamped.to_string());
    }
    let basename = format_basename(&meta.authors, &meta.title, meta.date.as_deref());

    // The non-KFX partner of a KFX is EPUB for a reflowable book, but PDF for a
    // PDF-backed (container) KFX — extracting that PDF, not mangling it into an
    // EPUB. EPUB and PDF imports always pair with KFX.
    let partner = match canonical {
        Canonical::Epub | Canonical::Pdf => Canonical::Kfx,
        Canonical::Kfx => {
            if bokai::formats::kfx::pdf_container::kfx_is_pdf_backed(&canonical_bytes) {
                Canonical::Pdf
            } else {
                Canonical::Epub
            }
        }
    };

    // 4. Persist canonical to the library slot.
    paths.ensure_sha(&sha)?;
    let dest_epub = paths.book_dir(&sha).join(format!("{basename}.epub"));
    let dest_kfx = paths.book_dir(&sha).join(format!("{basename}.kfx"));
    let dest_pdf = paths.book_dir(&sha).join(format!("{basename}.pdf"));
    let own_dest: &Path = match canonical {
        Canonical::Epub => &dest_epub,
        Canonical::Kfx => &dest_kfx,
        Canonical::Pdf => &dest_pdf,
    };
    let other_dest: &Path = match partner {
        Canonical::Epub => &dest_epub,
        Canonical::Kfx => &dest_kfx,
        Canonical::Pdf => &dest_pdf,
    };
    if !own_dest.exists() {
        write_bytes_atomic(own_dest, &canonical_bytes)?;
    }
    // The direct-derived KFX sibling (azw3 import). Written before the
    // `other_ready` probe below, so the row gets `kfx_path`/`kfx_sha256` and a
    // `done` job in one pass — the same shape as an idempotent re-import that
    // found the partner already on disk. An existing file wins (re-import of a
    // book whose KFX identity is already frozen must not re-stamp it).
    if let Some(kfx) = &direct_kfx
        && !dest_kfx.exists()
    {
        write_bytes_atomic(&dest_kfx, &kfx.bytes)?;
    }

    // 5. Cover sidecar. A KFX always carries its cover built-in (reflowable
    //    books embed the EPUB's cover, PDF-backed books the PDF's, and a "Change
    //    cover…" override replaces either), so pull it straight from the
    //    container — that gives a PDF-backed re-import a cover (its kfx→pdf side
    //    has no EPUB to harvest, which is why these used to land cover-less) and
    //    a reflowable one its cover immediately, instead of after the kfx→epub
    //    job. A direct PDF import has no embedded cover; its page-1 render waits
    //    on the pdf→kfx worker.
    let cover_path: Option<PathBuf> = match canonical {
        Canonical::Epub => write_cover_from_epub_bytes(paths, &sha, &canonical_bytes),
        Canonical::Kfx => write_cover_from_kfx_bytes(paths, &sha, &canonical_bytes),
        Canonical::Pdf => None,
    };

    // 6. Describe what the row will say. Every field is settled here; only the
    //    INSERT is left, and that is `record`'s job.
    let other_ready = other_dest.exists();
    // Record the KFX byte-hash whenever a KFX is on disk (canonical or the
    // already-present partner). That hash drives the on-device filename infix;
    // without it a re-import off the Kindle can't be linked back to the row.
    let kfx_bytes_sha: Option<String> = match canonical {
        Canonical::Kfx => Some(sha256_of_bytes(&canonical_bytes)),
        // EPUB/PDF canonical → partner is KFX; hash it if already present.
        _ if other_ready => fs::read(&dest_kfx).ok().map(|b| sha256_of_bytes(&b)),
        _ => None,
    };
    // Which sides are on disk now: the canonical always, the partner only on an
    // idempotent re-import where it already existed.
    let has = |c: Canonical| canonical == c || (partner == c && other_ready);
    Ok(StagedImport {
        sha,
        file_size,
        meta,
        epub_path: has(Canonical::Epub).then_some(dest_epub),
        cover_path,
        kfx_path: has(Canonical::Kfx).then_some(dest_kfx),
        pdf_path: has(Canonical::Pdf).then_some(dest_pdf),
        kfx_sha256: kfx_bytes_sha,
        job_kind: job_kind(canonical, partner),
        other_ready,
    })
}

/// A finished import waiting for its row: the files are already in the library
/// slot, and everything the row will say is settled. Opaque to the caller —
/// hand it straight to [`record`].
pub struct StagedImport {
    sha: String,
    file_size: i64,
    meta: BookMeta,
    epub_path: Option<PathBuf>,
    cover_path: Option<PathBuf>,
    kfx_path: Option<PathBuf>,
    pdf_path: Option<PathBuf>,
    kfx_sha256: Option<String>,
    /// The conversion that fills in the side this import didn't produce.
    job_kind: &'static str,
    /// Whether that side is already on disk — a direct-derived sibling, or an
    /// idempotent re-import that found it there. The job lands `done` if so.
    other_ready: bool,
}

/// Insert the book row and its conversion job for a staged import. The only
/// part of an import that needs the database, and it is a few milliseconds of
/// it — see [`stage_file`] for why the two are separable.
pub fn record(conn: &rusqlite::Connection, staged: StagedImport) -> Result<ImportOutcome> {
    // The dedupe probe that cleared this import ran before the conversion, which
    // for a large `.azw3` is minutes ago — long enough for the LAN server or a
    // device autopull to have landed the same book. Re-check inside the window
    // the insert will use: `books.sha256` is UNIQUE, so the alternative is a
    // constraint error where "already in the library" is the honest answer.
    if let Some(existing) = db::find_by_sha(conn, &staged.sha)? {
        return Ok(ImportOutcome::Duplicate(existing));
    }

    let book_id = insert_row(
        conn,
        &staged.sha,
        &staged.meta,
        staged.file_size,
        &Persisted {
            epub_path: staged.epub_path.as_deref(),
            cover_path: staged.cover_path.as_deref(),
            kfx_path: staged.kfx_path.as_deref(),
            pdf_path: staged.pdf_path.as_deref(),
            kfx_sha256: staged.kfx_sha256.as_deref(),
        },
    )?;

    let job_status = if staged.other_ready {
        "done"
    } else {
        "pending"
    };
    db::insert_job(conn, book_id, job_status, staged.job_kind)?;

    let row = db::get_book(conn, book_id)?.expect("just inserted");
    Ok(ImportOutcome::Imported {
        book: row,
        needs_enqueue: !staged.other_ready,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Open an in-memory EPUB, read its cover-image asset, and persist it as the
/// `cover.<ext>` sidecar. Returns the path written, or `None` if the EPUB
/// has no cover or the asset can't be loaded.
fn write_cover_from_epub_bytes(
    paths: &LibraryPaths,
    sha: &str,
    epub_bytes: &[u8],
) -> Option<PathBuf> {
    let (bytes, ext) = extract_cover_from_epub(epub_bytes)?;
    let out = paths.cover(sha, ext);
    fs::write(&out, &bytes).ok()?;
    // Derive the picker thumbnail now, at import. Best-effort: a thumbnail
    // failure must not fail the import — the full-res cover still works and the
    // server falls back to it (see library::thumbnail).
    let _ = super::thumbnail::ensure_thumbnail(paths, sha, &out);
    Some(out)
}

/// Pull the cover bytes (and extension) out of an in-memory EPUB. Used both
/// for direct EPUB imports and by the worker after `kfx_to_epub` produces an
/// EPUB whose JXR cover has been transcoded to JPG.
pub fn extract_cover_from_epub(epub_bytes: &[u8]) -> Option<(Vec<u8>, &'static str)> {
    let mut book = bokai::Book::from_bytes(epub_bytes, bokai::Format::Epub).ok()?;
    let cref = book.metadata().cover_image.as_deref()?.to_string();
    if cref.is_empty() {
        return None;
    }
    let bytes = book.load_asset(&PathBuf::from(&cref)).ok()?;
    Some((bytes, cover_ext_from(&cref)))
}

/// KFX parallel of [`write_cover_from_epub_bytes`]: persist the cover a KFX
/// carries built-in as the `cover.<ext>` sidecar + picker thumbnail. Best-effort
/// — returns `None` (no sidecar written) for a coverless or unparseable KFX.
fn write_cover_from_kfx_bytes(
    paths: &LibraryPaths,
    sha: &str,
    kfx_bytes: &[u8],
) -> Option<PathBuf> {
    let (bytes, ext) = extract_cover_from_kfx_bytes(kfx_bytes)?;
    let out = paths.cover(sha, ext);
    fs::write(&out, &bytes).ok()?;
    let _ = super::thumbnail::ensure_thumbnail(paths, sha, &out);
    Some(out)
}

/// Pull the cover bytes (and extension) out of an in-memory KFX. Mirrors
/// [`extract_cover_from_epub`] for the KFX side; the container surgery lives in
/// bokai (`kfx::cover_extract`). A load error or a coverless container → `None`.
pub fn extract_cover_from_kfx_bytes(kfx_bytes: &[u8]) -> Option<(Vec<u8>, &'static str)> {
    bokai::formats::kfx::cover_extract::kfx_extract_cover(kfx_bytes)
        .ok()
        .flatten()
}

pub fn write_bytes_atomic(dest: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = dest.with_file_name(format!(
        "{}.partial",
        dest.file_name()
            .expect("dest must include a filename")
            .to_string_lossy()
    ));
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, dest)
        .with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))?;
    Ok(())
}

pub enum ImportOutcome {
    Imported { book: BookRow, needs_enqueue: bool },
    Duplicate(BookRow),
}

struct BookMeta {
    title: String,
    authors: Vec<String>,
    language: String,
    ppd: Option<String>,
    date: Option<String>,
    /// Amazon catalogue id. Comes from bokai's dedicated `Metadata.asin` field
    /// (populated from KFX `kindle_title_metadata.ASIN` and from EPUB
    /// `<dc:identifier opf:scheme="ASIN">`). Distinct from `bokai::Metadata`'s
    /// generic `identifier`, which for KFX is the per-device internal
    /// `book_id` UUID — not the ASIN.
    asin: Option<String>,
    /// EPUB `<dc:publisher>` or KFX `publisher` field (symbol 232). Optional;
    /// many self-pub and indie books leave it blank.
    publisher: Option<String>,
    /// Yomigana for the title — bokai's `Metadata.title_sort`, from EPUB
    /// `opf:file-as` or KFX `title_pronunciation`. Romanized into
    /// `books.title_romaji` at [`insert_row`] when it's phonetic kana.
    title_reading: Option<String>,
    /// Yomigana for the first author — the first entry of bokai's per-author
    /// `Metadata.author_sorts`. Only used when the book has a single creator.
    author_reading: Option<String>,
    /// The series the source declares it belongs to, and its position in it —
    /// bokai's `Metadata.collection` (EPUB `belongs-to-collection` +
    /// `group-position`). A volume produced by an omnibus split carries this,
    /// which is what lands it in the same collection as its siblings with no
    /// further plumbing.
    series: Option<(String, Option<f64>)>,
}

fn extract_meta(m: &bokai::Metadata, fallback_stem: Option<&str>) -> BookMeta {
    let title = if m.title.trim().is_empty() {
        fallback_stem
            .map(str::to_string)
            .unwrap_or_else(|| "Untitled".to_string())
    } else {
        m.title.clone()
    };
    BookMeta {
        title,
        // Flip Western "Surname, Given" → "Given Surname" and unpack CJK
        // 「、」-packed creators, so the stored list (and the filename derived
        // from it) is the natural-order display form. See [`authors`].
        authors: authors::from_metadata(&m.authors),
        // Harmonize the source's language tag (en-US, eng, zh_cn, …) to one
        // canonical code so the library facet doesn't fan out into variants.
        language: super::lang::normalize(&m.language),
        ppd: m.page_progression_direction.clone(),
        date: m.date.clone(),
        asin: m.asin.clone().filter(|s| !s.is_empty()),
        publisher: m.publisher.clone().filter(|s| !s.is_empty()),
        // Yomigana bokai already pulled from EPUB `opf:file-as` / KFX
        // `*_pronunciation` — romanized at `insert_row` (yomigana-aware when kana).
        title_reading: m.title_sort.clone(),
        author_reading: m.author_sorts.first().cloned(),
        series: m.collection.as_ref().and_then(|c| {
            let name = c.name.trim();
            (!name.is_empty()).then(|| (name.to_string(), c.position))
        }),
    }
}

/// Metadata for a `.pdf` import: title/author from the PDF `/Info` dict
/// (title-cased if ALL CAPS, as Amazon's S2K does), falling back to the file
/// stem for the title. No language/date/asin/publisher in PDF `/Info`.
fn extract_meta_from_pdf(doc: &bokai::import::PdfDoc, fallback_stem: Option<&str>) -> BookMeta {
    let title = doc
        .title
        .as_deref()
        .map(title_case_if_shouting)
        .filter(|t| !t.trim().is_empty())
        .or_else(|| fallback_stem.map(str::to_string))
        .unwrap_or_else(|| "Untitled".to_string());
    let authors = match doc.author.as_deref() {
        Some(a) if !a.trim().is_empty() => authors::from_metadata(&[a.to_string()]),
        _ => Vec::new(),
    };
    BookMeta {
        title,
        authors,
        language: "en".to_string(),
        ppd: None,
        date: None,
        asin: None,
        publisher: None,
        // PDF `/Info` carries no yomi — romaji renders from the raw fields.
        title_reading: None,
        author_reading: None,
        // `/Info` has no series field.
        series: None,
    }
}

/// Title-case a string only if it is "shouting" (has letters, no lowercase),
/// matching what Amazon does to an ALL-CAPS PDF `/Info` title.
fn title_case_if_shouting(s: &str) -> String {
    let has_lower = s.chars().any(|c| c.is_lowercase());
    let has_alpha = s.chars().any(|c| c.is_alphabetic());
    if has_lower || !has_alpha {
        return s.to_string();
    }
    s.split(' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The on-disk artifacts an import produced (or found already present): the
/// paths and KFX hash to persist on the book row. Grouped into one struct so
/// `insert_row` stays under clippy's argument-count lint.
struct Persisted<'a> {
    epub_path: Option<&'a Path>,
    cover_path: Option<&'a Path>,
    kfx_path: Option<&'a Path>,
    pdf_path: Option<&'a Path>,
    kfx_sha256: Option<&'a str>,
}

fn insert_row(
    conn: &rusqlite::Connection,
    sha: &str,
    meta: &BookMeta,
    file_size: i64,
    files: &Persisted<'_>,
) -> Result<i64> {
    let epub_path_str = files.epub_path.map(|p| p.to_string_lossy().to_string());
    let cover_path_str = files.cover_path.map(|p| p.to_string_lossy().to_string());
    let kfx_path_str = files.kfx_path.map(|p| p.to_string_lossy().to_string());
    let pdf_path_str = files.pdf_path.map(|p| p.to_string_lossy().to_string());
    // `meta.authors` is already canonical (flipped, 「、」-unpacked) from
    // `extract_meta`; join with the unambiguous display separator so readers
    // split on `[&、]`, never a comma. See [`authors`].
    let authors_joined = authors::join_display(&meta.authors);
    // Render the editable, searchable romaji yomigana-aware. The author reading
    // (first of bokai's `author_sorts`) covers only the first creator, so it's
    // used only when there's a single author; otherwise the engine romanizes
    // the join.
    let title_romaji =
        super::romaji::romanize_field(&meta.title, meta.title_reading.as_deref(), &meta.language);
    let author_reading = (meta.authors.len() == 1)
        .then_some(meta.author_reading.as_deref())
        .flatten();
    let author_romaji =
        super::romaji::romanize_field(&authors_joined, author_reading, &meta.language);
    let now = db::now_iso();
    let id = db::insert_book(
        conn,
        &NewBook {
            sha256: sha,
            title: &meta.title,
            author: &authors_joined,
            language: &meta.language,
            title_romaji: &title_romaji,
            author_romaji: &author_romaji,
            ppd: meta.ppd.as_deref(),
            epub_path: epub_path_str.as_deref(),
            cover_path: cover_path_str.as_deref(),
            kfx_path: kfx_path_str.as_deref(),
            kfx_sha256: files.kfx_sha256,
            pdf_path: pdf_path_str.as_deref(),
            file_size,
            imported_at: &now,
            asin: meta.asin.as_deref(),
            // The same value, when it has the catalogue shape: a KFX Amazon
            // produced carries its own ASIN, and an EPUB can name one in
            // `<dc:identifier opf:scheme="ASIN">`. Kept for the colour-cover
            // fetch, which is the only thing that can use it — `asin` above is
            // the file's own key and the conversion will overwrite it.
            amazon_asin: meta
                .asin
                .as_deref()
                .filter(|a| bokai::formats::kfx::metadata::looks_like_real_amazon_asin(a)),
            publisher: meta.publisher.as_deref(),
            // meta.date comes from bokai's EPUB `<dc:date>` / KFX equivalent.
            // Stored verbatim — typically `2024-03-15` or `2024`. We filter
            // empties so a missing OPF date doesn't land as `Some("")`.
            published_at: meta
                .date
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            // The series the source declares, verbatim — grouping is by the
            // exact string, so a set of books that agree on it lands as one
            // collection. Tags aren't populated from the source format; they're
            // set via the metadata editor.
            series_name: meta.series.as_ref().map(|(name, _)| name.as_str()),
            series_index: meta.series.as_ref().and_then(|(_, index)| *index),
            tags: &[],
        },
    )?;
    Ok(id)
}

pub fn sha256_of_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn sha256_of_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// KFX bytes derived at import time, paired with the ASIN the export stamped
/// into them (real, or fabricated from the identifier; `None` only when the
/// source had neither).
struct DirectKfx {
    bytes: Vec<u8>,
    asin: Option<String>,
}

/// Both library sides derived from one `.azw3`.
struct Azw3Derived {
    epub: Vec<u8>,
    kfx: DirectKfx,
}

/// Convert a decrypted `.azw3` into BOTH library sides — EPUB and KFX — each
/// exported directly from the azw3's parsed IR. The azw3 itself is not
/// persisted, so import is the only moment both derivations can happen
/// without chaining (the KFX would otherwise be re-derived from the exported
/// EPUB by the background `epub_to_kfx` job).
///
/// Caller has already extension-detected `.azw3`; if the file isn't a real
/// AZW3 (bad PalmDOC header, etc.), bokai's `Book::from_bytes` returns the
/// error and the caller's `?` surfaces it as a normal import failure.
///
/// EPUB validity is NOT gated here: an import always produces a book, even an
/// imperfect one, so the user gets a usable result and a reconvert/edit target
/// rather than a failed import. Invalid output is a bokai converter bug to fix
/// (surfaced by the CLI validator and the A/B harness) or a source defect for
/// the book editor to repair — never a reason to drop the import. The KFX side
/// is exported from a fresh parse of the same bytes — not the handle the EPUB
/// export ran on — keeping it the same artifact as `bokai convert <azw3> <kfx>`
/// produces.
fn convert_azw3(src: &Path, on_progress: &dyn Fn(&str, usize, usize, &str)) -> Result<Azw3Derived> {
    let azw3_bytes = fs::read(src).with_context(|| format!("read {}", src.display()))?;

    on_progress("epub/parse", 0, 1, "Reading AZW3");
    let mut book = bokai::Book::from_bytes(&azw3_bytes, bokai::Format::Azw3)
        .with_context(|| format!("parse azw3 {}", src.display()))?;
    let mut buf = Cursor::new(Vec::<u8>::new());
    bokai::EpubExporter::new()
        .export_with_progress(&mut book, &mut buf, &leg("epub/", on_progress))
        .context("azw3 -> epub export")?;
    let epub_bytes = buf.into_inner();

    on_progress("kfx/parse", 0, 1, "Reading AZW3");
    let mut book = bokai::Book::from_bytes(&azw3_bytes, bokai::Format::Azw3)
        .with_context(|| format!("parse azw3 {}", src.display()))?;
    let mut kfx_buf = Cursor::new(Vec::<u8>::new());
    book.export_with_progress(bokai::Format::Kfx, &mut kfx_buf, &leg("kfx/", on_progress))
        .context("azw3 -> kfx export")?;
    let asin = bokai::formats::kfx::metadata::resolve_export_asin(book.metadata());
    Ok(Azw3Derived {
        epub: epub_bytes,
        kfx: DirectKfx {
            bytes: kfx_buf.into_inner(),
            asin,
        },
    })
}

/// Convert a decrypted `.mobi` to EPUB bytes via bokai's MOBI importer +
/// EPUB exporter. EPUB-only, unlike `convert_azw3`: the KFX side of a mobi
/// import is still filled by the background `epub_to_kfx` job from the
/// exported EPUB (mobi→kfx direct hasn't been through the fidelity harness
/// the azw3 pairs have).
fn convert_mobi(src: &Path, on_progress: &dyn Fn(&str, usize, usize, &str)) -> Result<Vec<u8>> {
    let mobi_bytes = fs::read(src).with_context(|| format!("read {}", src.display()))?;
    on_progress("epub/parse", 0, 1, "Reading MOBI");
    let mut book = bokai::Book::from_bytes(&mobi_bytes, bokai::Format::Mobi)
        .with_context(|| format!("parse mobi {}", src.display()))?;
    let mut buf = Cursor::new(Vec::<u8>::new());
    bokai::EpubExporter::new()
        .export_with_progress(&mut book, &mut buf, &leg("epub/", on_progress))
        .context("mobi -> epub export")?;
    Ok(buf.into_inner())
}

/// Re-report an exporter's phases under a leg of this import, so that
/// `finalize` from the EPUB export and `finalize` from the KFX export stay
/// distinguishable to whoever is drawing the bar.
fn leg<'a>(
    prefix: &'a str,
    on_progress: &'a dyn Fn(&str, usize, usize, &str),
) -> impl Fn(&str, usize, usize, &str) + 'a {
    move |phase, cur, total, label| on_progress(&format!("{prefix}{phase}"), cur, total, label)
}

/// Open an Aozora Bunko `.zip`, sniff for the markers, run the
/// parse → cover → build_epub pipeline, return EPUB bytes. Errors out
/// (via `bail!`) for any zip that doesn't look like aozora — the caller's
/// `?` then surfaces this as a normal import failure with no special UI.
///
/// Pipeline mirrors `aozora_dispatch` in `bokai/src/main.rs:1602` so
/// the CLI and the GUI produce byte-identical EPUBs from the same input.
fn convert_aozora_zip(
    src: &Path,
    on_progress: &dyn Fn(&str, usize, usize, &str),
) -> Result<Vec<u8>> {
    on_progress("epub/parse", 0, 1, "Reading Aozora archive");
    let file = fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("not a valid zip archive")?;

    let mut txt_buf: Option<Vec<u8>> = None;
    let mut images: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("zip entry {i}"))?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        let lower = name.to_ascii_lowercase();
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf).context("read zip entry")?;
        if lower.ends_with(".txt") {
            txt_buf = Some(buf);
        } else if lower.ends_with(".png")
            || lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".gif")
        {
            let basename = Path::new(&name)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or(name);
            images.push((basename, buf));
        }
    }

    let Some(txt) = txt_buf else {
        bail!("zip contains no .txt entry");
    };
    let text = bokai::formats::aozora::parser_txt::decode_bytes(&txt);

    // Aozora marker sniff. Bare zips with no aozora content fail here.
    if !text.contains("底本") && !text.contains("［＃") {
        bail!("not an Aozora Bunko archive");
    }

    let doc = bokai::formats::aozora::parse_txt(&text);
    on_progress("epub/cover", 0, 1, "Rendering cover");
    let cover = bokai::formats::aozora::render_cover_jpeg(&doc.title, &doc.author)
        .context("aozora cover render")?;
    on_progress("epub/finalize", 1, 1, "Packaging");
    // EPUB validity is not gated here (see `convert_azw3`): the import always
    // produces a book; an invalid one is a converter bug or a source defect to
    // repair, not a reason to drop the import.
    let epub_bytes = bokai::formats::aozora::build_epub(bokai::formats::aozora::EpubInput {
        document: &doc,
        images: &images,
        cover_jpeg: &cover,
    })
    .context("aozora build_epub")?;
    Ok(epub_bytes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    /// The cheapest input `import_file` will accept: a structurally valid PDF
    /// with one blank page. It is here to give the pipeline *something* to
    /// ingest — the tests below are about rows, paths and job pairing, not
    /// about PDF. Kept inline because a few hundred bytes of scaffolding is
    /// not a document worth committing. The `xref` offsets are absolute, so
    /// edit the body only by regenerating the whole literal.
    const MINIMAL_INPUT: &[u8] = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\n4 0 obj\n<< /Title (Tiny Test PDF) /Author (A. Tester) >>\nendobj\nxref\n0 5\n0000000000 65535 f \n0000000015 00000 n \n0000000064 00000 n \n0000000121 00000 n \n0000000192 00000 n \ntrailer\n<< /Size 5 /Root 1 0 R /Info 4 0 R >>\nstartxref\n256\n%%EOF\n";

    /// Drop [`MINIMAL_INPUT`] into `dir` and hand back the path to import from.
    fn minimal_input_in(dir: &Path) -> PathBuf {
        let p = dir.join("minimal.pdf");
        fs::write(&p, MINIMAL_INPUT).unwrap();
        p
    }

    #[test]
    fn pdf_detects_and_pairs_with_kfx() {
        assert_eq!(SourceKind::detect(Path::new("/x/Doc.pdf")), SourceKind::Pdf);
        assert_eq!(SourceKind::detect(Path::new("/x/DOC.PDF")), SourceKind::Pdf);
        assert_eq!(SourceKind::Pdf.canonical(), Canonical::Pdf);
        // A PDF book's canonical sibling is KFX, filled by `pdf_to_kfx`.
        assert_eq!(job_kind(Canonical::Pdf, Canonical::Kfx), "pdf_to_kfx");
        // A PDF-backed KFX returns to PDF, never EPUB.
        assert_eq!(job_kind(Canonical::Kfx, Canonical::Pdf), "kfx_to_pdf");
        // Reflowable directions are unchanged.
        assert_eq!(job_kind(Canonical::Epub, Canonical::Kfx), "epub_to_kfx");
        assert_eq!(job_kind(Canonical::Kfx, Canonical::Epub), "kfx_to_epub");
    }

    #[test]
    fn pdf_info_title_is_title_cased_else_filename() {
        // ALL-CAPS /Info title gets title-cased (Amazon's S2K behavior).
        let doc = bokai::import::PdfDoc {
            bytes: b"%PDF-1.4\n".to_vec(),
            pages: vec![bokai::import::PdfPage {
                width: 612.0,
                height: 792.0,
            }],
            title: Some("THE STREET WAS MINE".to_string()),
            author: Some("MEGAN E. ABBOTT".to_string()),
            outline: Vec::new(),
            page_labels: Vec::new(),
        };
        let meta = extract_meta_from_pdf(&doc, Some("fallback"));
        assert_eq!(meta.title, "The Street Was Mine");
        assert_eq!(meta.authors, vec!["MEGAN E. ABBOTT".to_string()]);

        // No /Info title → file stem.
        let doc2 = bokai::import::PdfDoc {
            bytes: b"%PDF-1.4\n".to_vec(),
            pages: vec![bokai::import::PdfPage {
                width: 1.0,
                height: 1.0,
            }],
            title: None,
            author: None,
            outline: Vec::new(),
            page_labels: Vec::new(),
        };
        let meta2 = extract_meta_from_pdf(&doc2, Some("My File"));
        assert_eq!(meta2.title, "My File");
        assert!(meta2.authors.is_empty());
    }

    #[test]
    fn import_persists_canonical_side_and_pairs_job() {
        use crate::library::db;
        use crate::library::paths::LibraryPaths;

        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        let conn = db::open(&paths.db()).unwrap();

        let src = minimal_input_in(tmp.path());
        let outcome = import_file(&conn, &paths, &src).unwrap();
        let ImportOutcome::Imported {
            book,
            needs_enqueue,
        } = outcome
        else {
            panic!("expected a fresh import, got a duplicate");
        };

        // The canonical side is persisted and the partner side is left to the
        // queue: one job pending, the missing side null until the worker fills
        // it. Which formats those are is `SourceKind`'s business, asserted
        // without touching disk in `pdf_detects_and_pairs_with_kfx`.
        assert_eq!(book.kind.as_deref(), Some("pdf_to_kfx"));
        assert!(needs_enqueue);
        assert!(book.pdf_path.is_some(), "canonical side must be persisted");
        assert!(
            book.kfx_path.is_none(),
            "partner side is produced later by the worker"
        );
        assert!(book.epub_path.is_none());
        assert!(
            Path::new(book.pdf_path.as_ref().unwrap()).exists(),
            "bytes landed in the library slot"
        );
    }

    #[test]
    fn running_the_phases_apart_files_the_same_book() {
        use crate::library::db;
        use crate::library::paths::LibraryPaths;

        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        let conn = db::open(&paths.db()).unwrap();

        let src = minimal_input_in(tmp.path());
        let identity = identify_file(&src).unwrap();
        let staged = stage_file(&paths, &src, identity, &no_progress).unwrap();
        let ImportOutcome::Imported {
            book,
            needs_enqueue,
        } = record(&conn, staged).unwrap()
        else {
            panic!("expected a fresh import, got a duplicate");
        };

        // Identical to what `import_file` produces in one call — the phases are
        // a seam for the lock, not a different pipeline.
        assert_eq!(book.kind.as_deref(), Some("pdf_to_kfx"));
        assert!(needs_enqueue);
        assert!(Path::new(book.pdf_path.as_ref().unwrap()).exists());
    }

    #[test]
    fn a_book_that_landed_mid_conversion_is_not_filed_twice() {
        use crate::library::db;
        use crate::library::paths::LibraryPaths;

        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        let conn = db::open(&paths.db()).unwrap();

        // Two importers clear the dedupe check on the same file, then convert.
        // Whoever finishes second must report the book as already here — the
        // alternative is a UNIQUE violation surfacing as an import failure.
        let src = minimal_input_in(tmp.path());
        let first = stage_file(&paths, &src, identify_file(&src).unwrap(), &no_progress).unwrap();
        let second = stage_file(&paths, &src, identify_file(&src).unwrap(), &no_progress).unwrap();

        let ImportOutcome::Imported { book, .. } = record(&conn, first).unwrap() else {
            panic!("the first one in files the book");
        };
        let ImportOutcome::Duplicate(existing) = record(&conn, second).unwrap() else {
            panic!("the second one must find the book already there");
        };
        assert_eq!(existing.id, book.id);
        assert_eq!(db::list_books(&conn).unwrap().len(), 1);
    }

    // The azw3 arm — deriving both sides up front, so the job lands `done` with
    // nothing enqueued — is untested here: reaching it means running a real
    // conversion over a real book. Testing the bookkeeping alone needs a seam
    // between deriving the pair and filing it.
}
