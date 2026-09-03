//! Find the Scribe's pen content on disk — the `.notebooks/` walk.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// `.notebooks/` child suffix marking a sideloaded-doc ink notebook.
const PDOC_SUFFIX: &str = "!!PDOC!!notebook";

/// One `nbk` on the device: what names it, where it is, and what's in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nbk {
    /// The host book's `asin` for ink, the notebook's `uuid` for a standalone —
    /// whichever identifies it to the library.
    pub id: String,
    pub path: PathBuf,
    /// sha256 of the `nbk`, hex. Compared against the library's manifest to
    /// decide whether this one needs sending.
    pub sha: String,
}

/// A standalone notebook: its `nbk` plus the two things only the device knows —
/// the cover thumbnail it keeps in a separate directory, and the file's own
/// modification time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standalone {
    pub nbk: Nbk,
    pub cover: Option<PathBuf>,
    /// The `nbk`'s mtime as naive local ISO, the shape the desktop's device pull
    /// produces. Local: the picker runs on the device, so this is the Kindle's clock.
    pub updated_at: String,
}

/// What one walk of `.notebooks/` found.
#[derive(Debug, Default)]
pub struct Scan {
    pub ink: Vec<Nbk>,
    pub notebooks: Vec<Standalone>,
    /// `!!PDOC!!` dirs whose content_id isn't in the library — Amazon's own
    /// cloud documents. Counted rather than pulled; a non-zero value is normal
    /// and only interesting in the log.
    pub foreign: usize,
}

/// Walk `.notebooks/` and classify every child.
pub fn scan(root: &Path, known_asins: &HashSet<String>) -> Scan {
    let mut out = Scan::default();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    let thumbs = root.join("thumbnails");
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(asin) = pdoc_asin(name) {
            if !known_asins.contains(&asin) {
                out.foreign += 1;
                continue;
            }
            if let Some(nbk) = hash_nbk(&dir, asin) {
                out.ink.push(nbk);
            }
            continue;
        }
        if !is_notebook_uuid(name) {
            continue; // thumbnails/, page_cache/, .backups/, !!EBOK!!, …
        }
        let Some(nbk) = hash_nbk(&dir, name.to_string()) else {
            continue;
        };
        let cover = thumbs.join(format!("{name}.png"));
        out.notebooks.push(Standalone {
            updated_at: mtime_iso(&nbk.path),
            cover: cover.is_file().then_some(cover),
            nbk,
        });
    }
    // `read_dir` order is whatever the filesystem gives; sort so a sync's
    // progress and its log read the same way twice in a row.
    out.ink.sort_by(|a, b| a.id.cmp(&b.id));
    out.notebooks.sort_by(|a, b| a.nbk.id.cmp(&b.nbk.id));
    out
}

/// Hash `<dir>/nbk`. `None` when the dir holds no `nbk` (so it isn't a notebook
/// at all) or the file can't be read — a directory we can't hash is one we can't
/// honestly say has changed, so it's left for the next sync.
fn hash_nbk(dir: &Path, id: String) -> Option<Nbk> {
    let path = dir.join("nbk");
    let bytes = std::fs::read(&path).ok()?;
    Some(Nbk {
        id,
        sha: sha256_hex(&bytes),
        path,
    })
}

/// The content_id of a `<id>!!PDOC!!notebook` dir, or `None` for anything else.
/// Whether that id is OURS is the caller's asin-set test — never a shape test.
fn pdoc_asin(dir_name: &str) -> Option<String> {
    dir_name
        .strip_suffix(PDOC_SUFFIX)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// A standalone notebook dir is named by a dashed UUID (`8-4-4-4-12` hex), e.g.
/// `da85e6f7-9672-2e2b-ef94-e57fc3502e45`. None of the firmware's bookkeeping
/// dirs or annotation-notebook dirs match that shape.
fn is_notebook_uuid(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// A file's mtime as naive local ISO. Falls back to the epoch's rendering if the
/// file has no readable mtime — a wrong-but-shaped value the library will
/// overwrite on the next sync, rather than a failure that loses the notebook.
fn mtime_iso(path: &Path) -> String {
    let t = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(UNIX_EPOCH);
    naive_local_iso(t)
}

/// `SystemTime` → `YYYY-MM-DDTHH:MM:SS` in the machine's local zone.
fn naive_local_iso(t: SystemTime) -> String {
    let secs: i64 = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    // SAFETY: `localtime_r` writes into our own `tm` and is the reentrant form
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(std::ptr::from_ref(&secs).cast(), &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &[u8]) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("nbk"), body).unwrap();
    }

    /// The whole point of the walk: four kinds of directory sit next to each
    /// other and only two of them are ours to sync.
    #[test]
    fn scan_separates_our_ink_from_notebooks_and_leaves_the_rest() {
        let tmp = std::env::temp_dir().join("sidle-hw-scan");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join(".notebooks");
        std::fs::create_dir_all(&root).unwrap();

        let uuid = "da85e6f7-9672-2e2b-ef94-e57fc3502e45";
        // Ours: one hex content_id, one Crockford-base32 — both real shapes.
        write(
            &root,
            "97870D063206CBA0CDD733367F356508!!PDOC!!notebook",
            b"HEX",
        );
        write(
            &root,
            "LXOGKNCCHUP7BXFVEMCJPWBQHRP6HXOP!!PDOC!!notebook",
            b"B32",
        );
        // Amazon's: a PDOC id the library has never heard of.
        write(
            &root,
            "42OY5GRNMOZLFZAAJQJISFR4KHOYIPW2!!PDOC!!notebook",
            b"X",
        );
        // An annotation notebook for a purchased book — never ink of ours.
        write(&root, "B009KA3Y6I!!EBOK!!notebook", b"X");
        // A standalone notebook, with its cover in the sibling dir.
        write(&root, uuid, b"NBK");
        std::fs::create_dir_all(root.join("thumbnails")).unwrap();
        std::fs::write(root.join("thumbnails").join(format!("{uuid}.png")), b"PNG").unwrap();
        // Firmware bookkeeping.
        std::fs::create_dir_all(root.join("page_cache")).unwrap();

        let known: HashSet<String> = [
            "97870D063206CBA0CDD733367F356508".to_string(),
            "LXOGKNCCHUP7BXFVEMCJPWBQHRP6HXOP".to_string(),
        ]
        .into_iter()
        .collect();

        let got = scan(&root, &known);

        assert_eq!(
            got.ink.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            [
                "97870D063206CBA0CDD733367F356508",
                "LXOGKNCCHUP7BXFVEMCJPWBQHRP6HXOP"
            ],
            "both library asins, regardless of content_id alphabet"
        );
        assert_eq!(got.foreign, 1, "the cloud PDOC is counted, not pulled");
        assert_eq!(got.notebooks.len(), 1, "only the dashed-uuid dir");
        assert_eq!(got.notebooks[0].nbk.id, uuid);
        assert!(got.notebooks[0].cover.is_some(), "cover from thumbnails/");
        assert_eq!(
            got.ink[0].sha,
            sha256_hex(b"HEX"),
            "sha is of the nbk bytes, so an edit on the device changes it"
        );
        // A real mtime, not the epoch fallback.
        assert!(got.notebooks[0].updated_at.starts_with("20"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_device_with_no_notebooks_dir_scans_empty() {
        let got = scan(Path::new("/nonexistent/.notebooks"), &HashSet::new());
        assert!(got.ink.is_empty() && got.notebooks.is_empty() && got.foreign == 0);
    }

    /// A `!!PDOC!!` dir without its `nbk` is not a notebook — the walk must not
    /// invent an entry for it (which would upload nothing under a real asin and
    /// let the library record an empty sync).
    #[test]
    fn a_pdoc_dir_with_no_nbk_yields_nothing() {
        let tmp = std::env::temp_dir().join("sidle-hw-empty");
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join(".notebooks");
        std::fs::create_dir_all(root.join("B00TEST!!PDOC!!notebook")).unwrap();
        let known: HashSet<String> = ["B00TEST".to_string()].into_iter().collect();
        assert!(scan(&root, &known).ink.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pdoc_asin_extracts_the_id_and_ignores_everything_else() {
        assert_eq!(
            pdoc_asin("97870D063206CBA0CDD733367F356508!!PDOC!!notebook").as_deref(),
            Some("97870D063206CBA0CDD733367F356508")
        );
        assert_eq!(pdoc_asin("!!PDOC!!notebook"), None, "empty id is not an id");
        assert_eq!(pdoc_asin("B009KA3Y6I!!EBOK!!notebook"), None);
        assert_eq!(pdoc_asin("da85e6f7-9672-2e2b-ef94-e57fc3502e45"), None);
    }

    #[test]
    fn uuid_shape_distinguishes_notebooks_from_bookkeeping_dirs() {
        assert!(is_notebook_uuid("da85e6f7-9672-2e2b-ef94-e57fc3502e45"));
        assert!(is_notebook_uuid("7507C10C-D7EB-A652-C030-2090B7BB1660")); // uppercase ok
        for junk in [
            "thumbnails",
            "page_cache",
            "clipboard",
            ".backups",
            ".tmp",
            "B009KA3Y6I!!EBOK!!notebook",
            "da85e6f7-9672-2e2b-ef94-e57fc3502e4",  // 35 chars
            "da85e6f7_9672_2e2b_ef94_e57fc3502e45", // underscores, not dashes
        ] {
            assert!(!is_notebook_uuid(junk), "{junk} must not look like a uuid");
        }
    }

    #[test]
    fn naive_local_iso_has_the_shape_the_library_stores() {
        let s = naive_local_iso(UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000));
        assert_eq!(s.len(), 19, "YYYY-MM-DDTHH:MM:SS");
        // 2023-11-14 22:13:20 UTC — still 2023-11-1x in every real zone.
        assert!(s.starts_with("2023-11-1"), "got {s}");
        assert_eq!(&s[10..11], "T");
    }

    /// Past 2038 the seconds no longer fit in 32 bits, which is the whole reason
    /// the value is carried as an `i64` rather than through `libc::time_t`. A
    /// truncating conversion turns this into 1970-something.
    #[test]
    fn naive_local_iso_survives_a_post_2038_timestamp() {
        let s = naive_local_iso(UNIX_EPOCH + std::time::Duration::from_secs(4_000_000_000));
        assert!(s.starts_with("2096-10-0"), "truncated to 32 bits? got {s}");
    }
}
