//! DRM-books source — the on-device scan + decrypt-engine probe.
//!
//! The picker's second library source: purchased Amazon KFX books that the
//! stock reader downloaded to `Downloads/Items01/`, listed as a cover grid so a
//! tap decrypts one in place via the prebuilt **kfxdedrm** engine (a separate
//! KUAL extension; we only spawn its binary). This module is the **pure** half —
//! filesystem scan, filename parsing, the executable probe — so it builds and
//! unit-tests on the host, alongside [`crate::device_state`]. The device half
//! (spawn + toast streaming) is `main.rs`'s `decrypt_flow`, the sibling of
//! `download_flow`.
//!
//! Layout facts are from on-device recon (KOA2) + the kfxdedrm reference:
//! - Books live in `Downloads/Items01/` as `<Title>_<ASIN>.kfx` + a sibling
//!   `<Title>_<ASIN>.sdr/` whose `assets/voucher` is the decrypt key. Both the
//!   title and the ASIN come straight from the filename — no KFX parsing.
//! - Covers are `system/thumbnails/thumbnail_<ASIN>_EBOK_portrait.jpg` (the
//!   device fetches them asynchronously, so a just-downloaded book may not have
//!   one yet → placeholder).
//! - The engine's single-book mode is `<exe> dedrm <path.kfx>` → output at
//!   `/mnt/us/dedrm/<stem>.kfx-zip`; it needs the voucher beside the input.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::Book;

/// Where the stock reader stores purchased KFX downloads. Kept as one constant
/// so a different on-device path is a one-line change. (Deliberately reads
/// `Items01`, which the Sidle reader/sync never touches — a dedrm tool reading
/// DRM books is its whole purpose.)
pub const ITEMS_DIR: &str = "/mnt/us/documents/Downloads/Items01";
/// Device cover-thumbnail cache, keyed by ASIN.
pub const THUMBS_DIR: &str = "/mnt/us/system/thumbnails";
/// The kfxdedrm KUAL extension's binary dir (the decrypt engine we spawn).
pub const BIN_DIR: &str = "/mnt/us/extensions/kfxdedrm/bin";
/// Where the engine writes each decrypted book (`<stem>.kfx-zip`).
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

/// A purchased DRM book found on the device — the app-layer view-model that
/// carries the local path/cover alongside the pipeline [`Book`], so `api::Book`
/// (the server DTO) stays clean. The synthesized `book.id` is this entry's index
/// in [`scan`]'s returned `Vec`, so the cover + tap seams recover the local data
/// by `book.id` (see `main.rs`).
#[derive(Debug, Clone)]
pub struct DrmBook {
    pub book: Book,
    /// The encrypted `.kfx` under [`ITEMS_DIR`] — the decrypt input. (Its ASIN,
    /// if needed, is the filename's trailing `_B…` token.)
    pub kfx_path: PathBuf,
    /// The device thumbnail, if present and complete (a `.tmp.partial` — the
    /// device still fetching it — counts as absent → tile shows a placeholder).
    pub cover_path: Option<PathBuf>,
}

/// Is the kfxdedrm engine installed? A cheap dir check that gates entry to the
/// DRM source, so a user without it gets a clear toast instead of a gallery they
/// can't act on. The per-ABI `<exe> test` probe ([`probe_exe`]) runs later, at
/// decrypt time, to also catch a present-but-non-working install.
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

/// The engine's output path for an input `.kfx`: `OUT_DIR/<stem>.kfx-zip`. Used
/// both to skip already-decrypted books in [`scan`] and to confirm success
/// after a decrypt.
pub fn out_path(kfx_path: &Path) -> PathBuf {
    let stem = kfx_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    Path::new(OUT_DIR).join(format!("{stem}.kfx-zip"))
}

/// Every decrypted book currently in [`OUT_DIR`] (`*.kfx-zip`) — the DRM-mode
/// Sync button re-pushes all of these to the server (which dedupes, so
/// already-synced ones are no-ops). Empty (never errors) when the dir is absent.
pub fn decrypted_books() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(OUT_DIR) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?;
            (!name.starts_with("._") && name.ends_with(".kfx-zip") && path.is_file())
                .then_some(path)
        })
        .collect()
}

/// Scan [`ITEMS_DIR`] for decryptable purchased books, newest download first.
///
/// A candidate is a top-level `<stem>.kfx` file (non-recursive, so the `updates/`
/// staging dir and the `.sdr` subtrees are skipped) that:
/// - isn't a macOS `._*` AppleDouble shadow (they persist on the FAT partition);
/// - has a sibling `<stem>.sdr/assets/voucher` (the decrypt key — also the
///   "download complete enough to decrypt" gate, so mid-download books drop out);
/// - has a filename ending `_<ASIN>` with a well-formed ASIN;
/// - hasn't already been decrypted (its `.kfx-zip` output is absent).
///
/// Title and ASIN come from the filename; the cover from the ASIN-keyed
/// thumbnail cache — no KFX parsing. Returns `[]` (never errors) when the dir is
/// absent/unreadable, so the caller just shows an empty DRM view.
pub fn scan() -> Vec<DrmBook> {
    scan_in(
        Path::new(ITEMS_DIR),
        Path::new(THUMBS_DIR),
        Path::new(OUT_DIR),
    )
}

/// [`scan`] with the three dirs injected, so the rule engine is host-testable
/// against a temp tree (the public [`scan`] wires the on-device consts).
fn scan_in(items: &Path, thumbs: &Path, out_dir: &Path) -> Vec<DrmBook> {
    let Ok(entries) = std::fs::read_dir(items) else {
        return Vec::new();
    };

    // Collect (path, name, mtime) for every non-shadow *.kfx file, newest first
    // so the synthesized ids are stable and the default Date-added sort has a
    // sensible tiebreak.
    let mut kfx: Vec<(PathBuf, String, SystemTime)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_string();
            if name.starts_with("._") || !name.ends_with(".kfx") {
                return None;
            }
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
            Some((path, name, mtime))
        })
        .collect();
    kfx.sort_by_key(|k| std::cmp::Reverse(k.2));

    let mut out = Vec::new();
    for (path, name, mtime) in kfx {
        let stem = &name[..name.len() - ".kfx".len()];
        // Voucher gate: complete download + the key the engine needs beside it.
        let voucher = items.join(format!("{stem}.sdr")).join("assets/voucher");
        if !voucher.is_file() {
            continue;
        }
        let Some(asin) = parse_asin(stem) else {
            continue;
        };
        // Persistent hide: already decrypted (output present) → done, don't relist.
        if out_dir.join(format!("{stem}.kfx-zip")).exists() {
            continue;
        }

        let title = title_from_stem(stem, &asin);
        let file_size = std::fs::metadata(&path)
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        let cover = cover_for(thumbs, &asin);
        let id = out.len() as i64;
        out.push(DrmBook {
            book: synth_book(id, title, file_size, format_mtime(mtime)),
            kfx_path: path,
            cover_path: cover,
        });
    }
    out
}

/// The ASIN suffix of a filename stem: the final `_`-delimited token when it's a
/// well-formed ASIN (`B` + 9 uppercase-alphanumerics, 10 chars). `None`
/// otherwise, so a stray non-purchase filename is skipped rather than mis-listed.
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
fn title_from_stem(stem: &str, asin: &str) -> String {
    let title = stem.strip_suffix(&format!("_{asin}")).unwrap_or(stem);
    title.replace("_ ", ": ")
}

/// The device cover thumbnail for an ASIN, if the complete `.jpg` is present. A
/// `.tmp.partial` (the device still fetching it) is treated as absent.
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
/// blank — DRM books have no series/author/tags, so they render as standalone
/// tiles and an empty `search_key` makes `search` fall back to the title (Latin,
/// which every purchased title here is).
fn synth_book(id: i64, title: String, file_size: i64, imported_at: String) -> Book {
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
        file_size,
        imported_at,
        tags: Vec::new(),
        cover_rev: 0,
        search_key: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                "B00XST7S8C"
            ),
            "All of Us: The Collected Poems (Vintage Contemporaries)"
        );
        assert_eq!(
            title_from_stem("Little Children_ A Novel_B000FC1BQK", "B000FC1BQK"),
            "Little Children: A Novel"
        );
        // No sanitized colon → unchanged but for the ASIN strip.
        assert_eq!(
            title_from_stem(
                "Neuromancer (Sprawl Trilogy Book 1)_B000O76ON6",
                "B000O76ON6"
            ),
            "Neuromancer (Sprawl Trilogy Book 1)"
        );
    }

    #[test]
    fn scan_in_applies_the_recon_rules() {
        // A fake Items01 exercising every scan rule: a good+covered book, an
        // incomplete one (no voucher), a macOS shadow, a no-ASIN file, an
        // already-decrypted one, and the `updates/` staging dir.
        let base = std::env::temp_dir().join(format!("sidle-dedrm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let items = base.join("Items01");
        let thumbs = base.join("thumbnails");
        let out = base.join("dedrm");
        std::fs::create_dir_all(&items).unwrap();
        std::fs::create_dir_all(&thumbs).unwrap();
        std::fs::create_dir_all(&out).unwrap();

        let mk = |stem: &str, voucher: bool| {
            std::fs::write(items.join(format!("{stem}.kfx")), b"kfx").unwrap();
            if voucher {
                let assets = items.join(format!("{stem}.sdr")).join("assets");
                std::fs::create_dir_all(&assets).unwrap();
                std::fs::write(assets.join("voucher"), b"v").unwrap();
            }
        };
        mk("Good Book_ Subtitle_B000O76ON6", true); // kept (has a cover, below)
        mk("Coverless_B01MXXZOEW", true); // kept, but no thumbnail → placeholder
        mk("Half Book_B000FC1BQK", false); // no voucher → skipped
        mk("Done Book_B078H4RWP7", true); // already decrypted → skipped
        std::fs::write(out.join("Done Book_B078H4RWP7.kfx-zip"), b"z").unwrap();
        std::fs::write(items.join("._Good Book_ Subtitle_B000O76ON6.kfx"), b"x").unwrap();
        std::fs::write(items.join("random.kfx"), b"x").unwrap(); // no ASIN → skipped
        std::fs::create_dir_all(items.join("updates")).unwrap();
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
        // Exactly the two complete, undecrypted, ASIN-named books survive (order
        // is by mtime, which the two temp files share — so assert by title, not
        // position).
        assert_eq!(found.len(), 2);
        let by_title = |t: &str| found.iter().find(|d| d.book.title == t).unwrap();
        let good = by_title("Good Book: Subtitle");
        let coverless = by_title("Coverless");
        // The `.kfx` under Items01 is the decrypt input; its complete cover
        // matched by ASIN (a `.tmp.partial` thumbnail is treated as absent).
        assert!(
            good.kfx_path
                .ends_with("Good Book_ Subtitle_B000O76ON6.kfx")
        );
        assert!(good.cover_path.is_some());
        assert_eq!(coverless.cover_path, None);
        // No series/author → standalone tiles; empty search_key feeds the title
        // search fallback.
        assert!(good.book.series_name.is_none());
        assert!(good.book.search_key.is_empty());
        // ids are the contiguous scan indices (the cover/tap seams key off this).
        let mut ids: Vec<i64> = found.iter().map(|d| d.book.id).collect();
        ids.sort();
        assert_eq!(ids, vec![0, 1]);

        let _ = std::fs::remove_dir_all(&base);
    }
}
