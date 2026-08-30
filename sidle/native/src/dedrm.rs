//! DRM-books source — the on-device scan + decrypt-engine probe.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::Book;

/// Where the stock reader stores purchased downloads. One constant: a different
/// on-device path is a one-line change. Reads `Items01`, which the Sidle
/// reader/sync never touches.
pub const ITEMS_DIR: &str = "/mnt/us/documents/Downloads/Items01";
/// Device cover-thumbnail cache, keyed by ASIN.
pub const THUMBS_DIR: &str = "/mnt/us/system/thumbnails";
/// The kfxdedrm KUAL extension's binary dir, holding `ABI_VARIANTS`.
pub const BIN_DIR: &str = "/mnt/us/extensions/kfxdedrm/bin";
/// Where the engine writes each decrypted book.
pub const OUT_DIR: &str = "/mnt/us/dedrm";

/// kfxdedrm's four ABI builds, in the probe order its own `run_cmd.sh` uses:
/// hard-float first, then soft, `_c11` vs `_old` libc within each. The first
/// whose `<exe> test` exits 0 is the one that runs on this device.
const ABI_VARIANTS: [&str; 4] = [
    "kfxdedrmhf_c11",
    "kfxdedrmhf_old",
    "kfxdedrm_old",
    "kfxdedrm_c11",
];

/// The extensions the engine's MOBI path names as candidates. `.azw` and
/// `.prc` are not among them.
pub const MOBI_EXTENSIONS: [&str; 3] = ["azw3", "azw4", "mobi"];

/// The engine's two code paths, which differ in how a book proves it carries
/// DRM and in how its output is named ([`out_path`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Kfx,
    Mobi,
}

impl Format {
    /// The family of the book at `path`, by extension. Case-insensitive: a FAT
    /// partition round-trips through desktops that upper-case one.
    pub fn of_path(path: &Path) -> Option<Format> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        if ext == "kfx" {
            Some(Format::Kfx)
        } else if MOBI_EXTENSIONS.contains(&ext.as_str()) {
            Some(Format::Mobi)
        } else {
            None
        }
    }
}

/// A purchased DRM book found on the device — the app-layer view-model that
#[derive(Debug, Clone)]
pub struct DrmBook {
    pub book: Book,
    /// The encrypted file under [`ITEMS_DIR`] — the decrypt input.
    pub path: PathBuf,
    /// Where the engine writes this book, under [`OUT_DIR`]. [`scan`] resolves
    /// it and lists no book it cannot name an output for. Its extension is the
    /// one place the book's [`Format`] shows.
    pub out_path: PathBuf,
    /// The device thumbnail, if present and complete. A half-written
    /// `.tmp.partial` counts as absent and the tile shows a placeholder.
    pub cover_path: Option<PathBuf>,
}

/// Is the kfxdedrm engine installed? A cheap dir check gating entry to the DRM
pub fn available() -> bool {
    Path::new(BIN_DIR).is_dir()
}

/// The working kfxdedrm binary for this device: the first ABI variant whose
/// `<exe> test` exits 0 (mirrors `run_cmd.sh`'s `check_exec`). `None` when none
/// runs (wrong ABI, or not installed) — the caller toasts and bails.
pub fn probe_exe() -> Option<PathBuf> {
    let dir = Path::new(BIN_DIR);
    for name in ABI_VARIANTS {
        let exe = dir.join(name);
        if !exe.is_file() {
            continue;
        }
        let ok = Command::new(&exe)
            .arg("test")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Some(exe);
        }
    }
    None
}

/// The engine's output for the book at `path`, under [`OUT_DIR`]. `None` for a
/// path of neither family.
pub fn out_path(path: &Path) -> Option<PathBuf> {
    out_in(Path::new(OUT_DIR), path, Format::of_path(path)?)
}

/// [`out_path`] with the output dir and the family injected; [`scan_in`] judges
/// a temp tree with it. [`Format::Kfx`] takes the `.kfx-zip` extension;
fn out_in(dir: &Path, path: &Path, format: Format) -> Option<PathBuf> {
    match format {
        Format::Kfx => Some(dir.join(format!("{}.kfx-zip", path.file_stem()?.to_str()?))),
        Format::Mobi => Some(dir.join(path.file_name()?)),
    }
}

/// The inverse of [`out_path`]: the encrypted book under [`ITEMS_DIR`] that
pub fn source_book(out: &Path) -> Option<PathBuf> {
    let items = Path::new(ITEMS_DIR);
    let name = out.file_name()?.to_str()?;
    match name.strip_suffix(".kfx-zip") {
        Some(stem) => Some(items.join(format!("{stem}.kfx"))),
        None => (Format::of_path(out) == Some(Format::Mobi)).then(|| items.join(name)),
    }
}

/// A purchased book's sidecar dir: the `<stem>.sdr/` sitting beside it (for a
fn sdr_dir(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    parent.join(format!("{stem}.sdr"))
}

/// Every decrypted book currently in [`OUT_DIR`] — the DRM-mode Sync button
/// re-pushes all of these to the server, which dedupes a repeat push to a no-op.
pub fn decrypted_books() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(OUT_DIR) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?;
            if name.starts_with("._") || !path.is_file() {
                return None;
            }
            is_output(&path).then_some(path)
        })
        .collect()
}

/// Whether `path` names an engine output: a merged `.kfx-zip`, or a MOBI-family
/// book under its own name.
fn is_output(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".kfx-zip"))
        || Format::of_path(path) == Some(Format::Mobi)
}

/// Remove one purchased book's entire on-device footprint once it's confirmed on
pub fn cleanup_synced(path: &Path) -> Vec<(PathBuf, std::io::Error)> {
    cleanup_paths(path, &sdr_dir(path), out_path(path).as_deref())
}

/// [`cleanup_synced`] with the three targets injected; the removal is
/// host-testable against a temp tree, the public entry wiring the name-derived
/// paths. Order is input → sidecar → output, each removed leniently.
fn cleanup_paths(book: &Path, sdr: &Path, out: Option<&Path>) -> Vec<(PathBuf, std::io::Error)> {
    let mut failures = Vec::new();
    remove_lenient(book, false, &mut failures);
    remove_lenient(sdr, true, &mut failures);
    if let Some(out) = out {
        remove_lenient(out, false, &mut failures);
    }
    failures
}

/// Remove one path — a file (`is_dir` false) or a whole dir tree (`is_dir` true)
/// — treating an absent path as success and pushing any real error onto
/// `failures` for the caller to log.
fn remove_lenient(path: &Path, is_dir: bool, failures: &mut Vec<(PathBuf, std::io::Error)>) {
    let res = if is_dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match res {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => failures.push((path.to_path_buf(), e)),
    }
}

/// Scan [`ITEMS_DIR`] for decryptable purchased books, newest download first.
pub fn scan() -> Vec<DrmBook> {
    scan_in(
        Path::new(ITEMS_DIR),
        Path::new(THUMBS_DIR),
        Path::new(OUT_DIR),
    )
}

/// [`scan`] with the three dirs injected; the rule engine is host-testable
/// against a temp tree, the public [`scan`] wiring the on-device consts.
fn scan_in(items: &Path, thumbs: &Path, out_dir: &Path) -> Vec<DrmBook> {
    let Ok(entries) = std::fs::read_dir(items) else {
        return Vec::new();
    };

    // Collect (path, format, size, mtime) for every DRM'd book, newest first so
    // the synthesized ids are stable and the default Date-added sort has a
    // sensible tiebreak.
    let mut found: Vec<(PathBuf, Format, u64, SystemTime)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.file_name()?.to_str()?.starts_with("._") {
                return None;
            }
            let format = Format::of_path(&path)?;
            let meta = e.metadata().ok()?;
            if !meta.is_file() || !is_encrypted(&path, format) {
                return None;
            }
            Some((
                path,
                format,
                meta.len(),
                meta.modified().unwrap_or(UNIX_EPOCH),
            ))
        })
        .collect();
    found.sort_by_key(|k| std::cmp::Reverse(k.3));

    let mut out = Vec::new();
    for (path, format, size, mtime) in found {
        let Some(produced) = out_in(out_dir, &path, format) else {
            continue;
        };
        // Persistent hide: an output on disk drops the book from the list.
        if produced.exists() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };

        let asin = parse_asin(stem);
        let id = out.len() as i64;
        out.push(DrmBook {
            book: synth_book(
                id,
                title_from_stem(stem, asin.as_deref()),
                format,
                size as i64,
                format_mtime(mtime),
            ),
            cover_path: asin.as_deref().and_then(|a| cover_for(thumbs, a)),
            out_path: produced,
            path,
        });
    }
    out
}

/// Whether the book at `path` carries DRM the engine can strip.
fn is_encrypted(path: &Path, format: Format) -> bool {
    match format {
        Format::Kfx => sdr_dir(path).join("assets").join("voucher").is_file(),
        Format::Mobi => matches!(palmdoc_encryption(path), Some(1 | 2)),
    }
}

/// The PalmDB `type`+`creator` pair a Mobipocket database carries, and its
/// offset in the header.
const BOOKMOBI: &[u8; 8] = b"BOOKMOBI";
const TYPE_CREATOR_OFF: usize = 60;
/// Start of the record-info list: 8 bytes per record, the first four holding
/// that record's file offset.
const RECORD_LIST_OFF: usize = 78;
/// Where `encryption_type` sits in the PalmDOC header that opens record 0.
const ENCRYPTION_OFF: usize = 12;

/// The `encryption_type` field of a MOBI-family book, as two short reads — the
fn palmdoc_encryption(path: &Path) -> Option<u16> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = std::fs::File::open(path).ok()?;
    let mut header = [0u8; RECORD_LIST_OFF + 4];
    f.read_exact(&mut header).ok()?;
    if &header[TYPE_CREATOR_OFF..TYPE_CREATOR_OFF + 8] != BOOKMOBI {
        return None;
    }
    let record0 = u32::from_be_bytes(header[RECORD_LIST_OFF..].try_into().ok()?);

    f.seek(SeekFrom::Start(record0 as u64)).ok()?;
    let mut rec0 = [0u8; ENCRYPTION_OFF + 2];
    f.read_exact(&mut rec0).ok()?;
    Some(u16::from_be_bytes([
        rec0[ENCRYPTION_OFF],
        rec0[ENCRYPTION_OFF + 1],
    ]))
}

/// The ASIN suffix of a filename stem: the final `_`-delimited token when it's a
/// well-formed ASIN (`B` + 9 uppercase-alphanumerics, 10 chars). `None` for a
/// filename carrying none, which costs the book its cover, not its tile.
fn parse_asin(stem: &str) -> Option<String> {
    let tok = stem.rsplit('_').next()?;
    let well_formed = tok.len() == 10
        && tok.starts_with('B')
        && tok
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
    well_formed.then(|| tok.to_string())
}

/// The human title: the stem with its trailing `_<ASIN>` removed, and the
/// filename-safe `_ ` (a sanitized `: `) restored — Amazon writes
/// `All of Us_ The Collected Poems` for "All of Us: The Collected Poems".
fn title_from_stem(stem: &str, asin: Option<&str>) -> String {
    let title = asin
        .and_then(|a| stem.strip_suffix(&format!("_{a}")))
        .unwrap_or(stem);
    title.replace("_ ", ": ")
}

/// The device cover thumbnail for an ASIN, if the complete `.jpg` is present. A
/// half-written `.tmp.partial` is treated as absent.
fn cover_for(thumbs: &Path, asin: &str) -> Option<PathBuf> {
    let p = thumbs.join(format!("thumbnail_{asin}_EBOK_portrait.jpg"));
    p.is_file().then_some(p)
}

/// A unix-seconds mtime as a fixed-width decimal string, so `ui::sort`'s
/// Date-added key (a plain string compare of `imported_at`) orders DRM books by
/// download time.
fn format_mtime(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs:015}")
}

/// A [`Book`] carrying only what a DRM book knows: id (its scan index), the
/// filename title, byte size, and a sortable download time. Everything else is
/// blank — DRM books have no series/author/tags, and render as standalone tiles.
fn synth_book(id: i64, title: String, format: Format, file_size: i64, imported_at: String) -> Book {
    Book {
        id,
        title,
        kfx_sha256: None,
        device_filename: None,
        author: String::new(),
        language: String::new(),
        publisher: None,
        series_name: None,
        series_index: None,
        // The conversion kind this book's library row carries once imported: a
        kind: Some(
            match format {
                Format::Kfx => "kfx_to_epub",
                Format::Mobi => "epub_to_kfx",
            }
            .to_string(),
        ),
        // The ink sync matches against the library's asins. This book is not in
        // the library, and an unset `asin` keeps it out of that match.
        asin: None,
        file_size,
        imported_at,
        tags: Vec::new(),
        cover_rev: 0,
        kfx_rev: 0,
        search_key: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal PalmDB: the 78-byte header, one record-info entry, then a
    /// record 0 whose `encryption_type` is `enc`.
    fn palmdb(type_creator: &[u8; 8], enc: u16) -> Vec<u8> {
        let record0 = (RECORD_LIST_OFF + 8) as u32;
        let mut v = vec![0u8; RECORD_LIST_OFF + 8];
        v[TYPE_CREATOR_OFF..TYPE_CREATOR_OFF + 8].copy_from_slice(type_creator);
        v[76..78].copy_from_slice(&1u16.to_be_bytes());
        v[RECORD_LIST_OFF..RECORD_LIST_OFF + 4].copy_from_slice(&record0.to_be_bytes());
        // Record 0: compression, unused, text length, record count, record
        // size, then `encryption_type`.
        v.extend_from_slice(&[0u8; ENCRYPTION_OFF]);
        v.extend_from_slice(&enc.to_be_bytes());
        v
    }

    fn mobi_book(enc: u16) -> Vec<u8> {
        palmdb(BOOKMOBI, enc)
    }

    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("sidle-dedrm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    #[test]
    fn parse_asin_accepts_real_asins_and_rejects_junk() {
        // The eight recon books all end `_B0…`.
        for asin in [
            "B00XST7S8C",
            "B000FC1BQK",
            "B000O76ON6",
            "B078H4RWP7",
            "B01LRIQ74G",
            "B0BXTBYRVC",
            "B01MXXZOEW",
            "B00ZJZGVYK",
        ] {
            assert_eq!(
                parse_asin(&format!("Some Title_{asin}")).as_deref(),
                Some(asin)
            );
        }
        // Wrong length / prefix / case → not an ASIN.
        assert_eq!(parse_asin("No trailing token"), None);
        assert_eq!(parse_asin("Title_B00XST7S8"), None); // 9 chars
        assert_eq!(parse_asin("Title_B00XST7S8CC"), None); // 11 chars
        assert_eq!(parse_asin("Title_A00XST7S8C"), None); // not B…
        assert_eq!(parse_asin("Title_b00xst7s8c"), None); // lowercase
    }

    #[test]
    fn title_strips_asin_and_restores_colon() {
        assert_eq!(
            title_from_stem(
                "All of Us_ The Collected Poems (Vintage Contemporaries)_B00XST7S8C",
                Some("B00XST7S8C")
            ),
            "All of Us: The Collected Poems (Vintage Contemporaries)"
        );
        // No sanitized colon → unchanged but for the ASIN strip.
        assert_eq!(
            title_from_stem(
                "Neuromancer (Sprawl Trilogy Book 1)_B000O76ON6",
                Some("B000O76ON6")
            ),
            "Neuromancer (Sprawl Trilogy Book 1)"
        );
        // A sideload with no ASIN keeps its whole stem.
        assert_eq!(
            title_from_stem("Some Book_ A Novel", None),
            "Some Book: A Novel"
        );
    }

    #[test]
    fn every_extension_the_engine_names_classifies() {
        assert_eq!(Format::of_path(Path::new("a.kfx")), Some(Format::Kfx));
        for ext in MOBI_EXTENSIONS {
            assert_eq!(
                Format::of_path(&PathBuf::from(format!("a.{ext}"))),
                Some(Format::Mobi),
                "{ext}"
            );
        }
        // FAT round-trips through desktops that upper-case extensions.
        assert_eq!(Format::of_path(Path::new("a.AZW3")), Some(Format::Mobi));
        assert_eq!(Format::of_path(Path::new("a.KFX")), Some(Format::Kfx));
        // The engine's scanner names neither of these.
        assert_eq!(Format::of_path(Path::new("a.azw")), None);
        assert_eq!(Format::of_path(Path::new("a.prc")), None);
        // Everything else, the engine's own KFX output included.
        assert_eq!(Format::of_path(Path::new("a.kfx-zip")), None);
        assert_eq!(Format::of_path(Path::new("a.epub")), None);
        assert_eq!(Format::of_path(Path::new("noext")), None);
    }

    #[test]
    fn kfx_output_takes_a_new_extension_and_mobi_keeps_its_own() {
        let items = Path::new(ITEMS_DIR);
        assert_eq!(
            out_path(&items.join("Book_B000O76ON6.kfx")),
            Some(Path::new(OUT_DIR).join("Book_B000O76ON6.kfx-zip"))
        );
        // The MOBI path copies the file under its own name — same name, same
        // extension, different directory.
        assert_eq!(
            out_path(&items.join("Some Book_B000O76ON6.azw3")),
            Some(Path::new(OUT_DIR).join("Some Book_B000O76ON6.azw3"))
        );
        // `file_stem` splits at the last dot; a volume number survives.
        assert_eq!(
            out_path(&items.join("All of Us_ Vol. 1_B00XST7S8C.kfx")),
            Some(Path::new(OUT_DIR).join("All of Us_ Vol. 1_B00XST7S8C.kfx-zip"))
        );
        assert_eq!(out_path(Path::new("/x/noext")), None);
    }

    #[test]
    fn source_book_inverts_out_path_for_both_families() {
        for name in ["Book_B000O76ON6.kfx", "Book_B000O76ON6.azw3", "Book.mobi"] {
            let book = Path::new(ITEMS_DIR).join(name);
            let out = out_path(&book).unwrap();
            assert_eq!(source_book(&out).as_deref(), Some(book.as_path()), "{name}");
            assert!(is_output(&out), "{name}");
        }
        // Not an engine output — `decrypted_books` never yields these.
        assert_eq!(source_book(Path::new("/x/keyfile.txt")), None);
        assert!(!is_output(Path::new("/x/keyfile.txt")));
        // A bare `.kfx` carries an input's extension, not an output's: the
        // engine writes `.kfx-zip`.
        assert_eq!(source_book(Path::new("/x/Book.kfx")), None);
        assert!(!is_output(Path::new("/x/Book.kfx")));
    }

    #[test]
    fn scan_in_applies_the_recon_rules() {
        // A fake Items01 exercising every scan rule across both families: a
        let base = scratch("scan");
        let items = base.join("Items01");
        let thumbs = base.join("thumbnails");
        let out = base.join("dedrm");
        std::fs::create_dir_all(&items).unwrap();
        std::fs::create_dir_all(&thumbs).unwrap();
        std::fs::create_dir_all(&out).unwrap();

        let kfx = |stem: &str, voucher: bool| {
            std::fs::write(items.join(format!("{stem}.kfx")), b"kfx").unwrap();
            if voucher {
                let assets = items.join(format!("{stem}.sdr")).join("assets");
                std::fs::create_dir_all(&assets).unwrap();
                std::fs::write(assets.join("voucher"), b"v").unwrap();
            }
        };
        kfx("Good Book_ Subtitle_B000O76ON6", true); // kept (has a cover, below)
        kfx("Coverless_B01MXXZOEW", true); // kept, but no thumbnail → placeholder
        kfx("Half Book_B000FC1BQK", false); // no voucher → skipped
        kfx("Done Book_B078H4RWP7", true); // already decrypted → skipped
        std::fs::write(out.join("Done Book_B078H4RWP7.kfx-zip"), b"z").unwrap();
        std::fs::write(items.join("._Good Book_ Subtitle_B000O76ON6.kfx"), b"x").unwrap();
        std::fs::create_dir_all(items.join("updates")).unwrap();

        std::fs::write(items.join("Sideload_ A Novel.azw3"), mobi_book(2)).unwrap();
        std::fs::write(items.join("Old Book_B01LRIQ74G.mobi"), mobi_book(1)).unwrap();
        std::fs::write(items.join("Free Book_B0BXTBYRVC.mobi"), mobi_book(0)).unwrap();
        std::fs::write(items.join("Weird_B00ZJZGVYK.azw3"), mobi_book(9)).unwrap();
        std::fs::write(items.join("Topaz_B00XST7S8C.azw3"), palmdb(b"TPZ3TPZ3", 2)).unwrap();
        std::fs::write(items.join("Not A Book_B000FC1BQK.azw"), mobi_book(2)).unwrap();

        // A complete thumbnail for the good book; a partial one is ignored.
        std::fs::write(
            thumbs.join("thumbnail_B000O76ON6_EBOK_portrait.jpg"),
            b"jpg",
        )
        .unwrap();
        std::fs::write(
            thumbs.join("thumbnail_B01MXXZOEW_EBOK_portrait.jpg.tmp.partial"),
            b"partial",
        )
        .unwrap();

        let found = scan_in(&items, &thumbs, &out);
        let titles: Vec<&str> = found.iter().map(|d| d.book.title.as_str()).collect();
        // Order is by mtime, which the temp files share — assert by title.
        assert_eq!(found.len(), 4, "{titles:?}");
        let by_title = |t: &str| found.iter().find(|d| d.book.title == t).unwrap();
        let good = by_title("Good Book: Subtitle");
        let coverless = by_title("Coverless");
        let sideload = by_title("Sideload: A Novel");
        let old = by_title("Old Book");

        // The `.kfx` under Items01 is the decrypt input; its complete cover
        // matched by ASIN (a `.tmp.partial` thumbnail is treated as absent).
        assert!(good.path.ends_with("Good Book_ Subtitle_B000O76ON6.kfx"));
        assert!(good.cover_path.is_some());
        assert_eq!(coverless.cover_path, None);
        // Each family's own output naming, rooted at the injected dir.
        assert_eq!(
            good.out_path,
            out.join("Good Book_ Subtitle_B000O76ON6.kfx-zip")
        );
        assert_eq!(sideload.out_path, out.join("Sideload_ A Novel.azw3"));
        assert_eq!(old.out_path, out.join("Old Book_B01LRIQ74G.mobi"));
        // A sideload with no ASIN is listed, without a cover.
        assert_eq!(sideload.cover_path, None);
        // The Format facet reads the conversion kind: KFX its own, the MOBI
        // family the EPUB side the desktop imports them through.
        assert_eq!(good.book.kind.as_deref(), Some("kfx_to_epub"));
        assert_eq!(sideload.book.kind.as_deref(), Some("epub_to_kfx"));
        // Byte size comes off the scanned file.
        assert_eq!(sideload.book.file_size, mobi_book(2).len() as i64);
        // No series/author → standalone tiles; empty search_key feeds the title
        // search fallback.
        assert!(good.book.series_name.is_none());
        assert!(good.book.search_key.is_empty());
        // ids are the contiguous scan indices (the cover/tap seams key off this).
        let mut ids: Vec<i64> = found.iter().map(|d| d.book.id).collect();
        ids.sort();
        assert_eq!(ids, vec![0, 1, 2, 3]);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cleanup_removes_only_this_books_footprint() {
        let base = scratch("cleanup");
        let items = base.join("Items01");
        let out = base.join("dedrm");
        std::fs::create_dir_all(&items).unwrap();
        std::fs::create_dir_all(&out).unwrap();

        // The synced book: encrypted `.kfx`, its `.sdr` sidecar (with voucher),
        // and the decrypted `.kfx-zip` output.
        let kfx = items.join("Good Book_B000O76ON6.kfx");
        let sdr = items.join("Good Book_B000O76ON6.sdr");
        let zip = out.join("Good Book_B000O76ON6.kfx-zip");
        std::fs::write(&kfx, b"kfx").unwrap();
        std::fs::create_dir_all(sdr.join("assets")).unwrap();
        std::fs::write(sdr.join("assets/voucher"), b"v").unwrap();
        std::fs::write(&zip, b"z").unwrap();

        // A neighbouring book that must be left fully intact.
        let other_kfx = items.join("Other_B01MXXZOEW.kfx");
        let other_sdr = items.join("Other_B01MXXZOEW.sdr");
        let other_zip = out.join("Other_B01MXXZOEW.kfx-zip");
        std::fs::write(&other_kfx, b"kfx").unwrap();
        std::fs::create_dir_all(&other_sdr).unwrap();
        std::fs::write(&other_zip, b"z").unwrap();

        // sdr_dir must resolve the sidecar from the book's own parent.
        assert_eq!(sdr_dir(&kfx), sdr);

        let failures = cleanup_paths(&kfx, &sdr_dir(&kfx), Some(&zip));
        assert!(failures.is_empty(), "unexpected failures: {failures:?}");

        // The synced book's three parts are gone…
        assert!(!kfx.exists());
        assert!(!sdr.exists());
        assert!(!zip.exists());
        // …and the neighbour is untouched.
        assert!(other_kfx.exists());
        assert!(other_sdr.exists());
        assert!(other_zip.exists());

        // Idempotent: a second pass over the absent footprint is a no-op.
        assert!(cleanup_paths(&kfx, &sdr_dir(&kfx), Some(&zip)).is_empty());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_mobi_cleanup_keeps_the_input_and_output_apart() {
        // The MOBI output carries the input's own filename; the two differ only
        // by directory, and a cleanup removes both, not one twice.
        let base = scratch("mobi-cleanup");
        let items = base.join("Items01");
        let out = base.join("dedrm");
        std::fs::create_dir_all(&items).unwrap();
        std::fs::create_dir_all(&out).unwrap();

        let book = items.join("Sideload.azw3");
        let decrypted = out.join("Sideload.azw3");
        std::fs::write(&book, mobi_book(2)).unwrap();
        std::fs::write(&decrypted, b"clear").unwrap();

        assert!(cleanup_paths(&book, &sdr_dir(&book), Some(&decrypted)).is_empty());
        assert!(!book.exists());
        assert!(!decrypted.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_palmdoc_gate_reads_the_books_own_header() {
        let base = scratch("palmdoc");
        std::fs::create_dir_all(&base).unwrap();

        let cases: [(&str, Vec<u8>, bool); 6] = [
            ("legacy.mobi", mobi_book(1), true),
            ("mobipocket.azw3", mobi_book(2), true),
            ("free.azw3", mobi_book(0), false),
            // The engine refuses any type outside 0/1/2.
            ("unknown.azw4", mobi_book(9), false),
            ("topaz.azw3", palmdb(b"TPZ3TPZ3", 2), false),
            ("junk.mobi", b"not a palm database".to_vec(), false),
        ];
        for (name, bytes, drm) in &cases {
            let path = base.join(name);
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(is_encrypted(&path, Format::Mobi), *drm, "{name}");
        }
        // A file cut short of the field must not read as DRM-free.
        let short = base.join("short.mobi");
        let mut bytes = mobi_book(2);
        bytes.truncate(RECORD_LIST_OFF + 8 + 4);
        std::fs::write(&short, &bytes).unwrap();
        assert_eq!(palmdoc_encryption(&short), None);
        assert!(!is_encrypted(&short, Format::Mobi));
        assert!(!is_encrypted(&base.join("absent.mobi"), Format::Mobi));

        let _ = std::fs::remove_dir_all(&base);
    }
}
