//! Finding an app's mount-rooted tree on this machine and walking it into a
//! file list.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use super::identity::AppIdentity;
use super::policy::{self, Apply, BUILD_STAMP_SUFFIX};

/// Directories above `extensions/` that `find_apps` descends through.
const MAX_ROOT_DEPTH: usize = 3;

/// Directory names `find_apps` never descends into.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", "ref", "artifacts"];

/// One installable file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppFile {
    /// Mount-relative: the manifest key, the served route, and the on-device
    /// destination.
    pub path: String,
    /// Where the bytes are on this machine.
    pub source: PathBuf,
    pub size: u64,
    pub apply: Apply,
}

/// One app's tree on disk: its identity, its mount root, and its files.
#[derive(Debug, Clone)]
pub struct AppTree {
    /// The directory `extensions/` sits in. Every [`AppFile::path`] resolves
    /// against it.
    pub root: PathBuf,
    pub app: AppIdentity,
    /// Sorted by path.
    pub files: Vec<AppFile>,
}

impl AppTree {
    /// The tree's build time in unix seconds. See [`built_at_of`].
    pub fn built_at(&self) -> u64 {
        built_at_of(self.files.iter().map(|f| f.source.as_path()))
    }

    /// Total bytes of [`AppTree::files`].
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// Every app tree at or under `root`, sorted by id. An empty result is not an
/// error.
pub fn discover(root: &Path) -> Result<Vec<AppTree>> {
    let mut found = Vec::new();
    find_apps(root, 0, &mut found)?;
    let mut trees = Vec::with_capacity(found.len());
    for (mount, id) in found {
        trees.push(walk(&mount, &id)?);
    }
    trees.sort_by(|a, b| a.app.id.cmp(&b.app.id));
    Ok(trees)
}

/// [`discover`], erroring when `root` holds no app tree.
pub fn discover_registrable(root: &Path) -> Result<Vec<AppTree>> {
    let trees = discover(root)?;
    if trees.is_empty() {
        bail!(
            "no extensions/<id>/ under {} — an app is a directory of files that \
             install to /mnt/us/extensions/, and this folder holds no such tree",
            root.display()
        );
    }
    Ok(trees)
}

/// One app's tree: `mount` plus the directory `id` under `extensions/`.
pub fn walk(mount: &Path, id: &str) -> Result<AppTree> {
    let app = AppIdentity::read(mount, id)?;
    let ext_dir = mount.join("extensions").join(id);
    let mut files = Vec::new();
    collect(&ext_dir, mount, &mut files)?;

    // `app.tile` is the one file outside `extensions/<id>`.
    if let Some(tile) = &app.tile {
        let source = mount.join(tile);
        let size = std::fs::metadata(&source)
            .with_context(|| format!("read tile {}", source.display()))?
            .len();
        files.push(AppFile {
            path: tile.clone(),
            source,
            size,
            apply: policy::apply_for(tile),
        });
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(AppTree {
        root: mount.to_path_buf(),
        app,
        files,
    })
}

/// The build time of a set of source files, in unix seconds: the largest
/// [`BUILD_STAMP_SUFFIX`] sidecar value beside any of them, else the largest
/// mtime. `0` when the set is empty or none can be read.
pub fn built_at_of<'a>(sources: impl Iterator<Item = &'a Path> + Clone) -> u64 {
    sources
        .clone()
        .filter_map(sidecar_ts)
        .max()
        .or_else(|| sources.filter_map(mtime_secs).max())
        .unwrap_or(0)
}

/// The unix seconds in `<source>.build-ts`.
fn sidecar_ts(source: &Path) -> Option<u64> {
    let mut sidecar = source.to_path_buf().into_os_string();
    sidecar.push(BUILD_STAMP_SUFFIX);
    std::fs::read_to_string(PathBuf::from(sidecar))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn mtime_secs(source: &Path) -> Option<u64> {
    std::fs::metadata(source)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Depth-first walk of the app's extension directory.
fn collect(dir: &Path, mount: &Path, out: &mut Vec<AppFile>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("read app tree {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !policy::is_payload(&name) {
            continue;
        }
        let path = entry.path();
        // `file_type` does not follow links.
        let ft = entry.file_type()?;
        if ft.is_dir() {
            collect(&path, mount, out)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(mount)
            .with_context(|| format!("{} is outside {}", path.display(), mount.display()))?;
        let Some(rel) = rel.to_str() else {
            bail!(
                "{}: path is not UTF-8, and the manifest keys files by path string",
                path.display()
            );
        };
        validate_mount_rel(rel)?;
        if policy::is_per_install(rel) {
            continue;
        }
        out.push(AppFile {
            path: rel.to_string(),
            source: path.clone(),
            size: entry.metadata()?.len(),
            apply: policy::apply_for(rel),
        });
    }
    Ok(())
}

/// Refuse a `path` that is not mount-relative: empty, absolute, backslashed,
/// with an empty component, or holding a `.` or `..` component.
pub fn validate_mount_rel(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("empty path");
    }
    if path.starts_with('/') {
        bail!("{path:?} is absolute — paths are relative to the mount root");
    }
    if path.contains('\\') {
        bail!("{path:?} contains a backslash — separators are `/` on the device");
    }
    if path.contains("//") {
        bail!("{path:?} has an empty path component");
    }
    for comp in path.split('/') {
        if comp == "." || comp == ".." {
            return Err(anyhow!("{path:?} contains a `{comp}` component"));
        }
    }
    Ok(())
}

/// Collect every `(mount root, id)` at or under `dir`, no deeper than
/// [`MAX_ROOT_DEPTH`] directories above the `extensions/` that holds it.
fn find_apps(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, String)>) -> Result<()> {
    let ext = dir.join("extensions");
    if ext.is_dir()
        && let Ok(entries) = std::fs::read_dir(&ext)
    {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') || !entry.path().is_dir() {
                continue;
            }
            if holds_payload(&entry.path()) {
                out.push((dir.to_path_buf(), name.to_string()));
            }
        }
    }
    if depth >= MAX_ROOT_DEPTH {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // `extensions/` is handled above.
        if name == "extensions" || name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            find_apps(&entry.path(), depth + 1, out)?;
        }
    }
    Ok(())
}

/// Whether `dir` holds at least one [`policy::is_payload`] file.
fn holds_payload(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !policy::is_payload(&name) {
            continue;
        }
        match entry.file_type() {
            Ok(t) if t.is_file() => return true,
            Ok(t) if t.is_dir() && holds_payload(&entry.path()) => return true,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// A repo whose tree is two levels down, with files outside it.
    fn sprocket_repo(root: &Path) -> PathBuf {
        let out = root.join("deploy").join("out");
        write(&out.join("extensions/sprocket/bin/sprocket"), b"armhf");
        write(
            &out.join("extensions/sprocket/bin/sprocket-softfloat"),
            b"armel",
        );
        write(
            &out.join("extensions/sprocket/hid/config.ini"),
            b"[device]\n",
        );
        write(&out.join("extensions/sprocket/hid/dist/libc.so.6"), b"elf");
        write(&out.join("extensions/sprocket/.DS_Store"), b"junk");
        write(
            &out.join("documents/Sprocket.sh"),
            b"#!/bin/sh\n# Name: Sprocket\nexec /mnt/us/extensions/sprocket/bin/sprocket.sh\n",
        );
        write(&root.join("build.sh"), b"#!/bin/sh\n");
        write(&root.join("device/assets/cover.png"), b"png");
        write(&root.join("target/debug/sprocket"), b"host binary");
        out
    }

    #[test]
    fn discovery_finds_a_tree_the_repo_never_names() {
        let tmp = tempfile::tempdir().unwrap();
        let out = sprocket_repo(tmp.path());
        let trees = discover(tmp.path()).unwrap();
        assert_eq!(trees.len(), 1);
        assert_eq!(trees[0].app.id, "sprocket");
        assert_eq!(trees[0].app.name, "Sprocket");
        assert_eq!(trees[0].root, out, "the mount root is extensions/'s parent");
    }

    #[test]
    fn the_walk_takes_the_extension_dir_and_the_tile_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        sprocket_repo(tmp.path());
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        let paths: Vec<&str> = tree.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "documents/Sprocket.sh",
                "extensions/sprocket/bin/sprocket",
                "extensions/sprocket/bin/sprocket-softfloat",
                "extensions/sprocket/hid/config.ini",
                "extensions/sprocket/hid/dist/libc.so.6",
            ],
            "no .DS_Store, and nothing from the repo outside the tree"
        );
    }

    #[test]
    fn documents_is_not_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let out = sprocket_repo(tmp.path());
        write(&out.join("documents/My Novel.txt"), b"chapter one");
        write(
            &out.join("documents/SomeOtherApp.sh"),
            b"# Name: Other\nexec /mnt/us/extensions/other/bin/other\n",
        );
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        assert!(
            tree.files
                .iter()
                .all(|f| f.path.starts_with("extensions/") || f.path == "documents/Sprocket.sh")
        );
    }

    #[test]
    fn one_repo_can_hold_several_apps() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/sidle/bin/sidle"), b"picker");
        write(
            &dev.join("documents/Sidle.sh"),
            b"#!/bin/sh\n# Name: Sidle\nexec /mnt/us/extensions/sidle/bin/sidle.sh\n",
        );
        write(&dev.join("extensions/bokai/bin/bokai"), b"elf");
        let ids: Vec<String> = discover(tmp.path())
            .unwrap()
            .into_iter()
            .map(|t| t.app.id)
            .collect();
        assert_eq!(ids, vec!["bokai", "sidle"]);
    }

    #[test]
    fn an_app_with_no_tile_walks_fine() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/bokai/bin/bokai"), b"elf");
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        assert!(tree.files.iter().all(|f| f.path.starts_with("extensions/")));
        assert_eq!(
            tree.total_size(),
            tree.files.iter().map(|f| f.size).sum::<u64>()
        );
    }

    #[test]
    fn a_build_ts_sidecar_beats_the_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/sidle/bin/sidle"), b"elf");
        write(
            &dev.join("extensions/sidle/bin/sidle.build-ts"),
            b"1755712345\n",
        );
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        assert_eq!(
            tree.built_at(),
            1755712345,
            "the tree's own mtimes are `now` and larger; the stamp still wins"
        );
        assert!(
            tree.files
                .iter()
                .all(|f| f.path != "extensions/sidle/bin/sidle.build-ts"),
            "the sidecar is build metadata, not something the device gets"
        );
    }

    #[test]
    fn a_tree_with_no_stamp_falls_back_to_its_own_timestamps() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/gadget/bin/gadget"), b"elf");
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        assert!(tree.built_at() > 0);
    }

    #[test]
    fn the_pickers_own_two_files_are_staged() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/sidle/bin/sidle"), b"picker");
        write(&dev.join("extensions/sidle/bin/sidle.sh"), b"#!/bin/sh\n");
        write(&dev.join("extensions/sidle/config.xml"), b"<extension/>");
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        let staged: Vec<&str> = tree
            .files
            .iter()
            .filter(|f| f.apply == Apply::Staged)
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(
            staged,
            vec![
                "extensions/sidle/bin/sidle",
                "extensions/sidle/bin/sidle.sh"
            ]
        );
    }

    #[test]
    fn a_per_install_path_left_in_a_tree_is_not_a_source() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(&dev.join("extensions/sidle/bin/sidle"), b"picker");
        write(
            &dev.join("extensions/sidle/etc/server.conf"),
            b"TOKEN=old\n",
        );
        write(&dev.join("extensions/sidle/etc/ca.pem"), b"stale root\n");
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        let paths: Vec<&str> = tree.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["extensions/sidle/bin/sidle"]);
    }

    #[test]
    fn discovery_stops_before_it_becomes_a_filesystem_scan() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("a/b/c/d/extensions/deep/bin/deep"), b"elf");
        assert!(discover(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn a_build_output_directory_is_not_searched() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("target/extensions/ghost/bin/ghost"),
            b"elf",
        );
        assert!(discover(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn a_directory_with_nothing_to_install_is_not_an_app() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("device/extensions/gadget/bin")).unwrap();
        write(
            &tmp.path().join("device/extensions/gadget/.DS_Store"),
            b"junk",
        );
        assert!(discover(tmp.path()).unwrap().is_empty());
    }
}
