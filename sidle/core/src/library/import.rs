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
//! Each step touches disk; callers should run this inside `spawn_blocking`.

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
    let src_kind = SourceKind::detect(src);
    if matches!(src_kind, SourceKind::Unknown) {
        bail!(
            "unsupported file type: {} (expected .epub, .kfx, .kfx-zip, .azw3, .mobi, or .pdf)",
            src.display()
        );
    }
    import_one(conn, paths, src, src_kind)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SourceKind {
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
    src: &Path,
    src_kind: SourceKind,
) -> Result<ImportOutcome> {
    let canonical = src_kind.canonical();

    // 1. Hash + dedupe before doing expensive normalization (a `.kfx-zip`
    //    merge can be tens of ms; stream-hashing the source is microseconds).
    let sha = sha256_of_file(src).with_context(|| format!("hash {}", src.display()))?;
    let file_size = fs::metadata(src)
        .with_context(|| format!("stat {}", src.display()))?
        .len() as i64;

    if let Some(existing) = db::find_by_sha(conn, &sha)? {
        return Ok(ImportOutcome::Duplicate(existing));
    }

    // 2. Normalize → canonical bytes. An `.azw3` additionally derives its KFX
    //    sibling here (both exports come straight from the azw3's IR), carried
    //    to the persist step below.
    let mut direct_kfx: Option<DirectKfx> = None;
    let canonical_bytes: Vec<u8> = match src_kind {
        SourceKind::Epub | SourceKind::Kfx | SourceKind::Pdf => {
            fs::read(src).with_context(|| format!("read {}", src.display()))?
        }
        SourceKind::KfxZip => bokai::formats::kfx::merge::merge_kfx_zip(src)
            .with_context(|| format!("merge kfx-zip {}", src.display()))?,
        SourceKind::Azw3 => {
            let derived = convert_azw3(src).with_context(|| format!("azw3 {}", src.display()))?;
            direct_kfx = Some(derived.kfx);
            derived.epub
        }
        SourceKind::Mobi => convert_mobi(src).with_context(|| format!("mobi {}", src.display()))?,
        SourceKind::AozoraZip => {
            convert_aozora_zip(src).with_context(|| format!("aozora zip {}", src.display()))?
        }
        SourceKind::Unknown => unreachable!("filtered above"),
    };

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
                .with_context(|| format!("probe pdf {}", src.display()))?;
            extract_meta_from_pdf(&doc, src.file_stem().and_then(|s| s.to_str()))
        }
        _ => {
            let book = bokai::Book::from_bytes(&canonical_bytes, canonical.bokai_format())
                .with_context(|| format!("read metadata from {}", src.display()))?;
            extract_meta(book.metadata(), src.file_stem().and_then(|s| s.to_str()))
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

    // 6. Insert book row + job.
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
    let book_id = insert_row(
        conn,
        &sha,
        &meta,
        file_size,
        &Persisted {
            epub_path: has(Canonical::Epub).then_some(dest_epub.as_path()),
            cover_path: cover_path.as_deref(),
            kfx_path: has(Canonical::Kfx).then_some(dest_kfx.as_path()),
            pdf_path: has(Canonical::Pdf).then_some(dest_pdf.as_path()),
            kfx_sha256: kfx_bytes_sha.as_deref(),
        },
    )?;

    let job_status = if other_ready { "done" } else { "pending" };
    db::insert_job(conn, book_id, job_status, job_kind(canonical, partner))?;

    let row = db::get_book(conn, book_id)?.expect("just inserted");
    Ok(ImportOutcome::Imported {
        book: row,
        needs_enqueue: !other_ready,
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
            publisher: meta.publisher.as_deref(),
            // meta.date comes from bokai's EPUB `<dc:date>` / KFX equivalent.
            // Stored verbatim — typically `2024-03-15` or `2024`. We filter
            // empties so a missing OPF date doesn't land as `Some("")`.
            published_at: meta
                .date
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            // Series and tags aren't populated from source format yet —
            // they're set via the metadata editor (a follow-up).
            series_name: None,
            series_index: None,
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
/// The EPUB side is gated on the standalone EPUB-3 validator for the same
/// reason as the aozora path: the bytes we're about to persist were freshly
/// synthesized rather than passed through, so we'd rather fail import than
/// write a broken book. The KFX side is exported from a fresh parse of the
/// same bytes — not the handle the EPUB export ran on — keeping it the same
/// artifact as `bokai convert <azw3> <kfx>` produces.
fn convert_azw3(src: &Path) -> Result<Azw3Derived> {
    use bokai::Exporter as _;
    let azw3_bytes = fs::read(src).with_context(|| format!("read {}", src.display()))?;

    let mut book = bokai::Book::from_bytes(&azw3_bytes, bokai::Format::Azw3)
        .with_context(|| format!("parse azw3 {}", src.display()))?;
    let mut buf = Cursor::new(Vec::<u8>::new());
    bokai::EpubExporter::new()
        .export(&mut book, &mut buf)
        .context("azw3 -> epub export")?;
    let epub_bytes = buf.into_inner();
    let report = bokai::validate::source::epub::validate(&epub_bytes);
    if !report.is_clean() {
        bail!("azw3 -> epub failed validation:\n{report}");
    }

    let mut book = bokai::Book::from_bytes(&azw3_bytes, bokai::Format::Azw3)
        .with_context(|| format!("parse azw3 {}", src.display()))?;
    let mut kfx_buf = Cursor::new(Vec::<u8>::new());
    book.export(bokai::Format::Kfx, &mut kfx_buf)
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
fn convert_mobi(src: &Path) -> Result<Vec<u8>> {
    use bokai::Exporter as _;
    let mobi_bytes = fs::read(src).with_context(|| format!("read {}", src.display()))?;
    let mut book = bokai::Book::from_bytes(&mobi_bytes, bokai::Format::Mobi)
        .with_context(|| format!("parse mobi {}", src.display()))?;
    let mut buf = Cursor::new(Vec::<u8>::new());
    bokai::EpubExporter::new()
        .export(&mut book, &mut buf)
        .context("mobi -> epub export")?;
    let epub_bytes = buf.into_inner();
    let report = bokai::validate::source::epub::validate(&epub_bytes);
    if !report.is_clean() {
        bail!("mobi -> epub failed validation:\n{report}");
    }
    Ok(epub_bytes)
}

/// Open an Aozora Bunko `.zip`, sniff for the markers, run the
/// parse → cover → build_epub pipeline, return EPUB bytes. Errors out
/// (via `bail!`) for any zip that doesn't look like aozora — the caller's
/// `?` then surfaces this as a normal import failure with no special UI.
///
/// Pipeline mirrors `aozora_dispatch` in `bokai/src/main.rs:1602` so
/// the CLI and the GUI produce byte-identical EPUBs from the same input.
fn convert_aozora_zip(src: &Path) -> Result<Vec<u8>> {
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
    let cover = bokai::formats::aozora::render_cover_jpeg(&doc.title, &doc.author)
        .context("aozora cover render")?;
    let epub_bytes = bokai::formats::aozora::build_epub(bokai::formats::aozora::EpubInput {
        document: &doc,
        images: &images,
        cover_jpeg: &cover,
    })
    .context("aozora build_epub")?;
    // Gate the import on the standalone EPUB-3 validator. Bad output here
    // silently corrupts the downstream KFX conversion, so we fail fast
    // instead of writing a broken book into the library.
    let report = bokai::validate::source::epub::validate(&epub_bytes);
    if !report.is_clean() {
        bail!("aozora epub failed validation:\n{report}");
    }
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

    // The azw3 arm — deriving both sides up front, so the job lands `done` with
    // nothing enqueued — is untested here: reaching it means running a real
    // conversion over a real book. Testing the bookkeeping alone needs a seam
    // between deriving the pair and filing it.
}
