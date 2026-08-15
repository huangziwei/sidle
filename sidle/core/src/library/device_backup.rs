//! What Sidle copies off a Kindle besides books: a configurable list of
//! **sync collections**, each one a folder on the device backed up into
//! `device-backup/<serial>/<collection-id>/` under the library root.
//!
//! A collection says which device folders to scan, which filenames to take,
//! whether to descend into subfolders, whether a second copy overwrites the
//! first, and what the picker may delete from the device once the push landed.
//! Screenshots and the picker's own logs are ordinary entries in that list
//! ([`SyncCollections::defaults`]) rather than a special case, so adding a
//! folder — a writing app's drafts, say — needs no new code on either side.
//!
//! The config is the library's, stored beside it as `device-sync.json`
//! ([`LibraryPaths::device_sync_config`](crate::library::LibraryPaths::device_sync_config)).
//! Three readers share it, so a file backed up over WiFi is byte-identical to
//! one backed up over USB:
//! - `sidle-server` serves it over `GET /sync/misc` and applies it to the
//!   `POST /sync/misc` the on-device picker sends when the user taps **Sync** —
//!   the primary path.
//! - the desktop app's USB pull (`device::misc`), when a Kindle is plugged in.
//! - the desktop app's settings editor, which is what writes the file.
//!
//! The picker holds only the copy it fetched, and mirrors the glob matching in
//! its own crate — `sidle-native` cross-compiles for the device and does not
//! depend on this one.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::library::LibraryPaths;

/// What a second copy of an already-backed-up file does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdatePolicy {
    /// Copy-if-absent. The device's file is immutable once written (a
    /// screenshot), so the first copy is the only one we need — and the USB
    /// pull can skip the read entirely, which is the expensive part over MTP.
    Once,
    /// Overwrite with the device's current bytes. The file grows or is edited
    /// in place (a log, a draft), so the newest copy is the one worth holding.
    #[default]
    Always,
}

/// One folder on the Kindle and what to do with the files in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCollection {
    /// Storage key: names the `device-backup/<serial>/<id>/` subdir and groups
    /// the files in the desktop's Files tab. Reduced to its final path
    /// component before use, so a crafted id can't escape the backup tree.
    pub id: String,
    /// Heading shown above this collection's files.
    pub label: String,
    /// Device folders to scan, relative to `/mnt/us`. `"."` (or `""`) is the
    /// USB root. More than one when the same kind of file lands in different
    /// places by firmware generation — stock screenshots are in `screenshots/`
    /// on newer Kindles and loose in the root on a KOA2.
    pub dirs: Vec<String>,
    /// Filenames to back up, as [`glob_match`] patterns. Empty takes nothing.
    pub include: Vec<String>,
    /// Descend into subfolders, keeping each file's path relative to the
    /// scanned folder.
    #[serde(default)]
    pub recursive: bool,
    #[serde(default)]
    pub update: UpdatePolicy,
    /// Delete the backed-up files off the Kindle once the push succeeded. For
    /// scratch the device re-creates at will; the library holds the only copy
    /// afterwards. Honoured by the picker's WiFi push only — the desktop's USB
    /// pull never writes to a device.
    #[serde(default)]
    pub clear_device: bool,
    /// Filenames deleted off the Kindle after a successful push but never
    /// uploaded — the firmware's `wininfo_screenshot_*.txt` companions, which
    /// are worth clearing with the screenshot they describe and worth nothing
    /// in the library. Same matching as [`include`](Self::include), same
    /// picker-only reach as [`clear_device`](Self::clear_device).
    #[serde(default)]
    pub purge: Vec<String>,
}

impl SyncCollection {
    /// Does this collection want a file called `name`?
    pub fn includes(&self, name: &str) -> bool {
        !is_never_backed_up(name) && self.include.iter().any(|p| glob_match(p, name))
    }

    /// Should `name` be deleted off the device without being uploaded?
    pub fn purges(&self, name: &str) -> bool {
        self.purge.iter().any(|p| glob_match(p, name))
    }

    /// The scanned folders as device-relative paths, with `"."` / `""`
    /// normalized to the empty root path.
    pub fn scan_dirs(&self) -> impl Iterator<Item = &str> {
        self.dirs
            .iter()
            .map(|d| if d == "." { "" } else { d.as_str() })
    }
}

/// Names no collection ever backs up, whatever its patterns say: an in-flight
/// write, and the dotfiles a desktop OS leaves behind (`.DS_Store`, `._*`
/// resource forks) after someone opens the device in a file browser.
fn is_never_backed_up(name: &str) -> bool {
    name.starts_with('.') || name.to_ascii_lowercase().ends_with(".partial")
}

/// The library's device-sync config: which folders a Sync brings across.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCollections {
    pub collections: Vec<SyncCollection>,
}

impl Default for SyncCollections {
    fn default() -> Self {
        Self::defaults()
    }
}

impl SyncCollections {
    /// What a library syncs before anyone edits the config: the Kindle's
    /// screenshots and the logs under `/mnt/us/logs/` (the picker's own two,
    /// plus whatever else on the device writes there).
    pub fn defaults() -> Self {
        Self {
            collections: vec![
                SyncCollection {
                    id: "screenshots".into(),
                    label: "Screenshots".into(),
                    // `screenshots/` holds Sidle's own two-corner captures and
                    // newer stock firmware's; the root holds a KOA2's.
                    dirs: vec!["screenshots".into(), ".".into()],
                    include: vec!["screenshot*".into()],
                    recursive: false,
                    update: UpdatePolicy::Once,
                    // A screenshot folder is scratch space, and the backup is
                    // additive — clearing it is what keeps each Sync to the
                    // captures taken since the last one.
                    clear_device: true,
                    purge: vec!["wininfo_screenshot*".into()],
                },
                SyncCollection {
                    id: "logs".into(),
                    label: "Logs".into(),
                    dirs: vec!["logs".into()],
                    include: vec!["*.log".into()],
                    recursive: true,
                    update: UpdatePolicy::Always,
                    clear_device: false,
                    purge: Vec::new(),
                },
            ],
        }
    }

    pub fn get(&self, id: &str) -> Option<&SyncCollection> {
        self.collections.iter().find(|c| c.id == id)
    }

    /// Read the library's config, falling back to [`defaults`](Self::defaults)
    /// when the file isn't there yet. A file that exists but doesn't parse is
    /// an error rather than a silent reset — it is hand-editable, and a typo
    /// must not look like "you never configured anything".
    pub fn load(paths: &LibraryPaths) -> anyhow::Result<Self> {
        let path = paths.device_sync_config();
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::defaults()),
            Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
        };
        serde_json::from_slice(&raw).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))
    }

    /// Write the config back, ids normalized to a bare path component so the
    /// storage key can never point outside the backup tree.
    pub fn save(&self, paths: &LibraryPaths) -> anyhow::Result<()> {
        let mut clean = self.clone();
        for c in &mut clean.collections {
            c.id = sanitize_id(&c.id).unwrap_or_default();
        }
        clean.collections.retain(|c| !c.id.is_empty());
        let path = paths.device_sync_config();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(&clean)?;
        std::fs::write(&path, json).map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))
    }
}

/// Case-insensitive glob over a bare filename, `*` being the only metacharacter
/// (matching any run of characters, including none). Enough for the patterns a
/// collection needs — `screenshot*`, `*.log`, `*`, `draft-*.md` — and small
/// enough to mirror exactly in the picker, which can't link this crate.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.to_lowercase().chars().collect();
    let text: Vec<char> = name.to_lowercase().chars().collect();

    // Two-pointer walk with a single backtrack point: on a mismatch after a
    // `*`, rewind the text by one and retry, which is what lets that `*` absorb
    // another character.
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut retry) = (None, 0usize);
    while t < text.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            retry = t;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            retry += 1;
            t = retry;
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&c| c == '*')
}

/// Reduce a collection id to a single safe path component, or `None` when
/// nothing usable is left (`..`, `/`, empty). The id names a directory under
/// `device-backup/<serial>/`, so this is what keeps a hand-edited config — or a
/// pushed one — inside the backup tree.
pub fn sanitize_id(id: &str) -> Option<String> {
    let base = Path::new(id).file_name()?.to_str()?;
    if base.is_empty() || base == "." || base == ".." {
        None
    } else {
        Some(base.to_string())
    }
}

/// Split a device-relative path (`2026/draft.md`) into safe components, or
/// `None` if any component is unusable. Rejects absolute paths, `..`, and the
/// names no collection backs up. This is the guard on a path a network client
/// chose: the picker sends the file's path relative to its collection folder,
/// and the server writes it under the backup dir.
pub fn sanitize_rel_path(rel: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for seg in rel.split(['/', '\\']) {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." || seg.contains(':') {
            return None;
        }
        out.push(seg.to_string());
    }
    match out.last() {
        Some(name) if !is_never_backed_up(name) => Some(out),
        _ => None,
    }
}

/// Store one file for `serial` under `device-backup/<serial>/<collection>/`,
/// at `rel` beneath it. Empty `bytes` are skipped so a truncated source can't
/// clobber a good prior backup, and [`UpdatePolicy::Once`] leaves an existing
/// copy alone. Returns `Ok(true)` when a file was written, `Ok(false)` when
/// skipped (including a rejected path).
pub fn store_collection_file(
    paths: &LibraryPaths,
    serial: &str,
    collection: &SyncCollection,
    rel: &str,
    bytes: &[u8],
) -> std::io::Result<bool> {
    if bytes.is_empty() {
        return Ok(false);
    }
    let (Some(id), Some(segments)) = (sanitize_id(&collection.id), sanitize_rel_path(rel)) else {
        return Ok(false);
    };
    let mut dest = paths.device_backup_collection(serial, &id);
    for seg in &segments {
        dest.push(seg);
    }
    if collection.update == UpdatePolicy::Once && dest.exists() {
        return Ok(false);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest, bytes)?;
    Ok(true)
}

/// Move everything already stored for `old_id` to `new_id`, on every device.
///
/// A collection's id names the folder its files live in, so the id has to be
/// free to change: the folder on the Kindle it reads from may be renamed, and
/// what the user calls it certainly may. Renaming without this would strand the
/// old folder under a name that exists nowhere else — the files still there, but
/// filed under something the library no longer knows about.
///
/// Merges rather than fails when `new_id` already holds files, and never
/// overwrites one that is already there. Returns how many devices moved
/// anything. A no-op when the two ids are the same or either is unusable.
pub fn rename_collection_storage(
    paths: &LibraryPaths,
    old_id: &str,
    new_id: &str,
) -> std::io::Result<usize> {
    let (Some(old), Some(new)) = (sanitize_id(old_id), sanitize_id(new_id)) else {
        return Ok(0);
    };
    if old == new {
        return Ok(0);
    }
    let mut moved = 0usize;
    let devices = match std::fs::read_dir(paths.device_backup_dir()) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for device in devices.flatten() {
        if !device.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let (src, dest) = (device.path().join(&old), device.path().join(&new));
        if !src.is_dir() {
            continue;
        }
        if dest.exists() {
            merge_dir(&src, &dest)?;
            // Succeeds only once nothing is left. Whatever remains collided with
            // a file already under the new id, and is kept rather than deleted.
            let _ = std::fs::remove_dir(&src);
        } else {
            std::fs::rename(&src, &dest)?;
        }
        moved += 1;
    }
    Ok(moved)
}

/// Move every file under `src` into `dest` at the same relative path, leaving
/// any whose destination already exists where it is. Directories are created as
/// needed and emptied ones removed.
fn merge_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let (from, to) = (entry.path(), dest.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            merge_dir(&from, &to)?;
            let _ = std::fs::remove_dir(&from);
        } else if !to.exists() {
            std::fs::rename(&from, &to)?;
        }
    }
    Ok(())
}

/// Is this file already backed up? Lets a caller skip the read before paying
/// for it — the point of [`UpdatePolicy::Once`] over MTP, where every read
/// re-walks the object tree from the root.
pub fn already_stored(
    paths: &LibraryPaths,
    serial: &str,
    collection: &SyncCollection,
    rel: &str,
) -> bool {
    if collection.update != UpdatePolicy::Once {
        return false;
    }
    let (Some(id), Some(segments)) = (sanitize_id(&collection.id), sanitize_rel_path(rel)) else {
        return false;
    };
    let mut dest = paths.device_backup_collection(serial, &id);
    for seg in &segments {
        dest.push(seg);
    }
    dest.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &tempfile::TempDir) -> LibraryPaths {
        LibraryPaths {
            root: dir.path().to_path_buf(),
        }
    }

    #[test]
    fn glob_matches_the_patterns_a_collection_needs() {
        assert!(glob_match("screenshot*", "screenshot_100.png"));
        assert!(glob_match("screenshot*", "Screenshot_ROOT.PNG"));
        assert!(!glob_match("screenshot*", "wininfo_screenshot_1.txt"));
        assert!(glob_match(
            "wininfo_screenshot*",
            "wininfo_screenshot_1.txt"
        ));
        assert!(glob_match("*.log", "sidle-native.log"));
        assert!(!glob_match("*.log", "book.kfx"));
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("draft-*.md", "draft-2026.md"));
        assert!(!glob_match("draft-*.md", "draft-2026.txt"));
        // A `*` absorbing more than one run is where a naive matcher fails.
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(glob_match("*", ""));
        assert!(!glob_match("x", ""));
    }

    #[test]
    fn defaults_classify_the_files_they_replaced() {
        let cfg = SyncCollections::defaults();
        let shots = cfg.get("screenshots").unwrap();
        let logs = cfg.get("logs").unwrap();

        assert!(shots.includes("screenshot_100.png"));
        assert!(shots.includes("Screenshot_ROOT.PNG"));
        assert!(!shots.includes("screenshot_1.png.partial"));
        assert!(!shots.includes("book.kfx"));
        assert!(shots.purges("wininfo_screenshot_2026_08_15T01_47_50+0200.txt"));
        assert!(!shots.includes("wininfo_screenshot_2026_08_15T01_47_50+0200.txt"));

        assert!(logs.includes("sidle-native.log"));
        assert!(!logs.includes("version.txt"));
        // macOS junk is never a backup, whatever the pattern says.
        assert!(!logs.includes(".DS_Store"));

        // The root is scanned for screenshots but not for logs.
        assert_eq!(
            shots.scan_dirs().collect::<Vec<_>>(),
            vec!["screenshots", ""]
        );
        assert_eq!(logs.scan_dirs().collect::<Vec<_>>(), vec!["logs"]);
    }

    #[test]
    fn once_copies_if_absent_and_always_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(&tmp);
        let cfg = SyncCollections::defaults();
        let shots = cfg.get("screenshots").unwrap();
        let logs = cfg.get("logs").unwrap();
        let s = "G000TEST";

        assert!(store_collection_file(&paths, s, shots, "screenshot_1.png", b"A").unwrap());
        assert!(store_collection_file(&paths, s, logs, "sidle-native.log", b"v1\n").unwrap());

        // Once: the re-write is skipped and the stored bytes are untouched.
        assert!(!store_collection_file(&paths, s, shots, "screenshot_1.png", b"B").unwrap());
        assert!(already_stored(&paths, s, shots, "screenshot_1.png"));
        assert_eq!(
            std::fs::read(
                paths
                    .device_backup_collection(s, "screenshots")
                    .join("screenshot_1.png")
            )
            .unwrap(),
            b"A"
        );

        // Always: the file grew, so the newer copy wins.
        assert!(store_collection_file(&paths, s, logs, "sidle-native.log", b"v1\nv2\n").unwrap());
        assert!(!already_stored(&paths, s, logs, "sidle-native.log"));
        assert_eq!(
            std::fs::read(
                paths
                    .device_backup_collection(s, "logs")
                    .join("sidle-native.log")
            )
            .unwrap(),
            b"v1\nv2\n"
        );

        // An empty source never clobbers a good backup.
        assert!(!store_collection_file(&paths, s, logs, "sidle-native.log", b"").unwrap());
    }

    #[test]
    fn subfolders_are_kept_as_written() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(&tmp);
        let drafts = SyncCollection {
            id: "drafts".into(),
            label: "Drafts".into(),
            dirs: vec!["writing".into()],
            include: vec!["*.md".into()],
            recursive: true,
            update: UpdatePolicy::Always,
            clear_device: false,
            purge: Vec::new(),
        };
        assert!(store_collection_file(&paths, "S", &drafts, "2026/draft.md", b"# hi").unwrap());
        assert_eq!(
            std::fs::read_to_string(
                paths
                    .device_backup_collection("S", "drafts")
                    .join("2026/draft.md")
            )
            .unwrap(),
            "# hi"
        );
    }

    /// Renaming a collection carries what it already synced. A collection's id
    /// names a real folder in the library, and the thing it is named after — the
    /// folder on the Kindle, or just what the user calls it — is free to change;
    /// leaving the files behind under the old name is how a library grows
    /// directories nothing refers to.
    #[test]
    fn renaming_a_collection_carries_its_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(&tmp);
        let old = SyncCollection {
            id: "old-name".into(),
            label: "Drafts".into(),
            dirs: vec!["writing".into()],
            include: vec!["*".into()],
            recursive: true,
            update: UpdatePolicy::Always,
            clear_device: false,
            purge: Vec::new(),
        };
        // Two devices hold files for it; a third holds nothing.
        store_collection_file(&paths, "DEV1", &old, "draft-1.md", b"one").unwrap();
        store_collection_file(&paths, "DEV1", &old, "2026/draft-2.md", b"two").unwrap();
        store_collection_file(&paths, "DEV2", &old, "draft-3.md", b"three").unwrap();
        paths.ensure_device_backup("DEV3").unwrap();

        assert_eq!(
            rename_collection_storage(&paths, "old-name", "drafts").unwrap(),
            2,
            "both devices that held files moved"
        );
        assert!(!paths.device_backup_collection("DEV1", "old-name").exists());
        let moved = paths.device_backup_collection("DEV1", "drafts");
        assert_eq!(
            std::fs::read_to_string(moved.join("draft-1.md")).unwrap(),
            "one"
        );
        assert_eq!(
            std::fs::read_to_string(moved.join("2026/draft-2.md")).unwrap(),
            "two",
            "subfolders carried too"
        );
        assert_eq!(
            std::fs::read_to_string(
                paths
                    .device_backup_collection("DEV2", "drafts")
                    .join("draft-3.md")
            )
            .unwrap(),
            "three"
        );

        // Renaming onto a name that already holds files merges, and never
        // overwrites one that is already there.
        let newer = SyncCollection {
            id: "drafts".into(),
            ..old.clone()
        };
        store_collection_file(&paths, "DEV1", &old, "draft-1.md", b"OLD COPY").unwrap();
        store_collection_file(&paths, "DEV1", &newer, "draft-4.md", b"four").unwrap();
        rename_collection_storage(&paths, "old-name", "drafts").unwrap();
        assert_eq!(
            std::fs::read_to_string(moved.join("draft-1.md")).unwrap(),
            "one",
            "the copy already under the new name wins"
        );
        assert_eq!(
            std::fs::read_to_string(moved.join("draft-4.md")).unwrap(),
            "four"
        );

        // A rename to the same name, or with an unusable id, does nothing.
        assert_eq!(
            rename_collection_storage(&paths, "drafts", "drafts").unwrap(),
            0
        );
        assert_eq!(
            rename_collection_storage(&paths, "drafts", "..").unwrap(),
            0
        );
        assert!(moved.join("draft-1.md").is_file());
    }

    #[test]
    fn crafted_paths_and_ids_stay_inside_the_backup_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(&tmp);
        let cfg = SyncCollections::defaults();
        let shots = cfg.get("screenshots").unwrap();

        // A traversal in the file's path is refused outright — unlike a bare
        // filename there is no safe reading of it.
        assert!(!store_collection_file(&paths, "S", shots, "../../evil.png", b"X").unwrap());
        assert!(!tmp.path().join("evil.png").exists());
        assert_eq!(sanitize_rel_path("a/../b"), None);
        assert_eq!(
            sanitize_rel_path("a//b/c.md"),
            Some(vec!["a".into(), "b".into(), "c.md".into()])
        );
        assert_eq!(sanitize_rel_path(".DS_Store"), None);

        // A traversal in the id lands in the id's final component.
        let evil = SyncCollection {
            id: "../../escape".into(),
            ..shots.clone()
        };
        store_collection_file(&paths, "S", &evil, "screenshot_1.png", b"X").unwrap();
        assert!(!tmp.path().join("escape").exists());
        assert!(
            paths
                .device_backup_collection("S", "escape")
                .join("screenshot_1.png")
                .is_file()
        );
        assert_eq!(sanitize_id("../.."), None);
        assert_eq!(sanitize_id("drafts"), Some("drafts".into()));
    }

    #[test]
    fn missing_config_reads_as_defaults_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths(&tmp);
        assert_eq!(
            SyncCollections::load(&paths).unwrap(),
            SyncCollections::defaults()
        );

        let mut cfg = SyncCollections::defaults();
        cfg.collections.push(SyncCollection {
            id: "../drafts".into(),
            label: "Drafts".into(),
            dirs: vec!["writing".into()],
            include: vec!["*".into()],
            recursive: true,
            update: UpdatePolicy::Always,
            clear_device: false,
            purge: Vec::new(),
        });
        cfg.save(&paths).unwrap();
        let back = SyncCollections::load(&paths).unwrap();
        assert_eq!(back.collections.len(), 3);
        assert_eq!(back.collections[2].id, "drafts", "id normalized on save");

        // A hand-edited file that doesn't parse is an error, not a silent reset.
        std::fs::write(paths.device_sync_config(), b"{ not json").unwrap();
        assert!(SyncCollections::load(&paths).is_err());
    }
}
