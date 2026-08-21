//! Finding an app's mount-rooted tree on this machine, and walking it into the
//! file list an install works from.
//!
//! # An app owns its extension directory and its tile
//!
//! That is the whole ownership rule. `extensions/<id>/**` is the app's, plus
//! the one `documents/*.sh` its [`AppSpec::tile`] names — the launcher tile the
//! library indexes, which has to sit at the mount root because that is the only
//! place the jailbreak hotfix looks. Nothing else in the tree belongs to the
//! app, so `documents/` is never walked: it is the writer's directory, and an
//! app that swept it would claim the user's own files. It also makes removal
//! answerable without a receipt — delete the extension directory and the tile.
//!
//! # Discovery is by `app.json`, not by convention
//!
//! Each repo puts its tree somewhere different — `device/` for steb, bokai,
//! kfxdedrm-fe and the picker, `deploy/out/` for karyll, whatever a zip
//! unpacked into for a release. Rather than a `root` field that every repo
//! would have to keep true, the tree announces itself: the directory holding
//! `extensions/<id>/app.json` *is* `extensions/`, so its parent is the mount
//! root. One repo can hold several — this one holds the picker and bokai — and
//! discovery returns all of them.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::spec::{APP_SPEC_FILE, AppSpec, FileClass, PathPolicy};

/// How far above `extensions/` a discovery walk looks. karyll's is the deepest
/// in the fleet at `deploy/out/extensions/`; three leaves room without turning
/// "add a repo" into a scan of a home directory.
const MAX_ROOT_DEPTH: usize = 3;

/// Directories a source walk never descends into. Build outputs and vendored
/// checkouts can hold an `extensions/` that is not an app of this repo's — and
/// `target/` in particular is large enough that walking it is the difference
/// between instant and not.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", "ref", "artifacts"];

/// Files that are never part of an install regardless of what a spec says:
/// macOS directory metadata, AppleDouble forks, and sidle's own receipt.
pub const RECEIPT_FILE: &str = ".sidle-install.json";

fn is_never_installed(name: &str) -> bool {
    name == ".DS_Store" || name == RECEIPT_FILE || name.starts_with("._")
}

/// One installable file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppFile {
    /// Mount-relative, and the one string that keys the manifest entry, the
    /// served route and the on-device destination.
    pub path: String,
    /// Where the bytes are on this machine.
    pub source: PathBuf,
    pub size: u64,
    pub policy: PathPolicy,
}

/// An app's tree as found on disk: its spec, its mount root, and every file it
/// would install. Hashes are not computed here — a status check reads only the
/// `sync` files, and composing reads all of them once.
#[derive(Debug, Clone)]
pub struct AppTree {
    /// The directory `extensions/` sits in. Every [`AppFile::path`] resolves
    /// against it.
    pub root: PathBuf,
    pub spec: AppSpec,
    /// Sorted by path, so two walks of one tree produce identical lists and a
    /// manifest diff means a real change.
    pub files: Vec<AppFile>,
}

impl AppTree {
    /// When this build was made, in unix seconds. The device compares it
    /// against its own and refuses anything not strictly newer, so a stale
    /// bundle cannot downgrade a device over Wi-Fi.
    ///
    /// A `<file>.build-ts` sidecar anywhere in the tree is the answer; mtimes
    /// are the fallback for a tree that carries none.
    ///
    /// The sidecar exists for the picker, whose `build.sh` bakes the same
    /// second into the binary. That value is what the running picker compares
    /// against, so it has to be the value the manifest carries — and a
    /// newest-mtime that merely happens to be larger is not the same number. It
    /// would make a re-push of one build read as an upgrade, and put a time in
    /// the receipt that no binary was ever built at. A deliberate statement
    /// beats an incidental one.
    ///
    /// Every other app makes no statement and has nothing compiled in, and its
    /// working copy's timestamps are exactly the truth the dev loop turns on:
    /// rebuild steb, and the tree is newer than what the device carries.
    pub fn built_at(&self) -> u64 {
        let stamped = self.files.iter().filter_map(sidecar_ts).max();
        stamped
            .or_else(|| self.files.iter().filter_map(mtime_secs).max())
            .unwrap_or(0)
    }

    /// Total bytes the app would occupy on the device.
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// The files an update decides by hash. Everything else is `seed` and is
    /// decided by existence alone, which is what keeps a status check off
    /// karyll's 49 MB of vendored Bluetooth stack.
    pub fn sync_files(&self) -> impl Iterator<Item = &AppFile> {
        self.files
            .iter()
            .filter(|f| f.policy.class == FileClass::Sync)
    }
}

/// Every app tree at or under `root`. A repo path, an unpacked bundle, or the
/// composed device tree all work — the shape is the same, only the depth
/// differs.
///
/// Returns them sorted by id. An empty result is not an error: "this folder
/// holds no app" is a thing the caller wants to say to the user, not a failure.
pub fn discover(root: &Path) -> Result<Vec<AppTree>> {
    let mut specs = Vec::new();
    find_specs(root, 0, &mut specs)?;
    let mut trees = Vec::with_capacity(specs.len());
    for spec_path in specs {
        // `<mount>/extensions/<id>/app.json` — three parents up is the mount.
        let mount = spec_path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .context("app.json is not three levels under a mount root")?
            .to_path_buf();
        trees.push(walk(&mount, &AppSpec::load(&spec_path)?)?);
    }
    trees.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
    Ok(trees)
}

/// Read one app's tree, given its mount root and its already-loaded spec.
pub fn walk(mount: &Path, spec: &AppSpec) -> Result<AppTree> {
    let ext_dir = mount.join("extensions").join(&spec.id);
    let mut files = Vec::new();
    collect(&ext_dir, mount, spec, &mut files)?;

    // The tile is the app's one file outside its extension directory. Absent on
    // disk is an error rather than a skip: a tile that a spec names and a build
    // did not produce is an app with no way to launch it, and the install would
    // otherwise look complete.
    if let Some(tile) = &spec.tile {
        let source = mount.join(tile);
        let meta = std::fs::metadata(&source).with_context(|| {
            format!(
                "{} names tile {tile}, which is not at {} — the tile is the only way \
                 to launch the app, so an install without it is not one",
                spec.id,
                source.display()
            )
        })?;
        let policy = spec.policy_for(tile);
        if policy.class != FileClass::Ignore {
            files.push(AppFile {
                path: tile.clone(),
                source,
                size: meta.len(),
                policy,
            });
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(AppTree {
        root: mount.to_path_buf(),
        spec: spec.clone(),
        files,
    })
}

/// The unix seconds in `<file>.build-ts`, when a build wrote one beside it.
fn sidecar_ts(f: &AppFile) -> Option<u64> {
    let mut sidecar = f.source.clone().into_os_string();
    sidecar.push(".build-ts");
    std::fs::read_to_string(PathBuf::from(sidecar))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn mtime_secs(f: &AppFile) -> Option<u64> {
    std::fs::metadata(&f.source)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Depth-first walk of the app's extension directory.
fn collect(dir: &Path, mount: &Path, spec: &AppSpec, out: &mut Vec<AppFile>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("read app tree {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_never_installed(&name) {
            continue;
        }
        let path = entry.path();
        // `file_type` does not follow links, which is the point: a mount has no
        // symlinks to write one to, and following one would pull bytes from
        // outside the tree under a path inside it.
        let ft = entry.file_type()?;
        if ft.is_dir() {
            collect(&path, mount, spec, out)?;
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
        // Windows separators cannot occur here (this reads a host filesystem),
        // but the string is about to become a mount-relative key, so it is
        // validated as one rather than assumed to be one.
        super::spec::validate_mount_rel(rel)?;
        let policy = spec.policy_for(rel);
        if policy.class == FileClass::Ignore {
            continue;
        }
        out.push(AppFile {
            path: rel.to_string(),
            source: path.clone(),
            size: entry.metadata()?.len(),
            policy,
        });
    }
    Ok(())
}

/// Collect every `extensions/*/app.json` at or under `dir`, no deeper than
/// [`MAX_ROOT_DEPTH`] directories above the `extensions/` that holds it.
fn find_specs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<()> {
    let ext = dir.join("extensions");
    if ext.is_dir()
        && let Ok(entries) = std::fs::read_dir(&ext)
    {
        for entry in entries.flatten() {
            let spec = entry.path().join(APP_SPEC_FILE);
            if spec.is_file() {
                out.push(spec);
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
        // `extensions/` was already handled above; descending into it again
        // would look for `extensions/extensions/`.
        if name == "extensions" || name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            find_specs(&entry.path(), depth + 1, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// A karyll-shaped repo: the tree two levels down, a seeded vendored
    /// subtree, a tile in documents/, and repo files that are not the app.
    fn karyll_repo(root: &Path) -> PathBuf {
        let out = root.join("deploy").join("out");
        write(
            &out.join("extensions/karyll/app.json"),
            br#"{"schema":1,"id":"karyll","name":"Karyll","version":"0.2.4",
                 "tile":"documents/Karyll.sh","pidof":"karyll",
                 "paths":[{"match":"extensions/karyll/hid/","class":"seed","seed_gen":1}]}"#,
        );
        write(&out.join("extensions/karyll/bin/karyll"), b"armhf");
        write(
            &out.join("extensions/karyll/bin/karyll-softfloat"),
            b"armel",
        );
        write(&out.join("extensions/karyll/hid/config.ini"), b"[device]\n");
        write(&out.join("extensions/karyll/hid/dist/libc.so.6"), b"elf");
        write(&out.join("extensions/karyll/.DS_Store"), b"junk");
        write(&out.join("documents/Karyll.sh"), b"# Name: Karyll\n");
        // Repo files that are not part of any app.
        write(&root.join("build.sh"), b"#!/bin/sh\n");
        write(&root.join("device/assets/cover.png"), b"png");
        write(&root.join("target/debug/karyll"), b"host binary");
        out
    }

    #[test]
    fn discovery_finds_a_tree_the_repo_never_names() {
        let tmp = tempfile::tempdir().unwrap();
        let out = karyll_repo(tmp.path());
        let trees = discover(tmp.path()).unwrap();
        assert_eq!(trees.len(), 1);
        assert_eq!(trees[0].spec.id, "karyll");
        assert_eq!(trees[0].root, out, "the mount root is extensions/'s parent");
    }

    #[test]
    fn the_walk_takes_the_extension_dir_and_the_tile_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        karyll_repo(tmp.path());
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        let paths: Vec<&str> = tree.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "documents/Karyll.sh",
                "extensions/karyll/app.json",
                "extensions/karyll/bin/karyll",
                "extensions/karyll/bin/karyll-softfloat",
                "extensions/karyll/hid/config.ini",
                "extensions/karyll/hid/dist/libc.so.6",
            ],
            "no .DS_Store, and nothing from the repo outside the tree"
        );
    }

    /// `seed` exists so a status check does not read what no update writes.
    #[test]
    fn seeded_subtrees_are_out_of_the_hash_set() {
        let tmp = tempfile::tempdir().unwrap();
        karyll_repo(tmp.path());
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        let hashed: Vec<&str> = tree.sync_files().map(|f| f.path.as_str()).collect();
        assert_eq!(
            hashed,
            vec![
                "documents/Karyll.sh",
                "extensions/karyll/app.json",
                "extensions/karyll/bin/karyll",
                "extensions/karyll/bin/karyll-softfloat",
            ]
        );
        for f in &tree.files {
            if f.path.starts_with("extensions/karyll/hid/") {
                assert_eq!(f.policy.class, FileClass::Seed);
                assert_eq!(f.policy.seed_gen, 1);
            }
        }
    }

    /// documents/ is the writer's directory. An app takes the one file it
    /// declares out of it and leaves everything else alone.
    #[test]
    fn documents_is_not_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let out = karyll_repo(tmp.path());
        write(&out.join("documents/My Novel.txt"), b"chapter one");
        write(&out.join("documents/SomeOtherApp.sh"), b"# Name: Other\n");
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        assert!(
            tree.files
                .iter()
                .all(|f| f.path.starts_with("extensions/") || f.path == "documents/Karyll.sh")
        );
    }

    #[test]
    fn one_repo_can_hold_several_apps() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(
            &dev.join("extensions/sidle/app.json"),
            br#"{"schema":1,"id":"sidle","name":"Sidle","version":"0.1.9",
                 "tile":"documents/Sidle.sh"}"#,
        );
        write(&dev.join("documents/Sidle.sh"), b"# Name: Sidle\n");
        write(
            &dev.join("extensions/bokai/app.json"),
            br#"{"schema":1,"id":"bokai","name":"bokai","version":"0.1.4"}"#,
        );
        write(&dev.join("extensions/bokai/bin/bokai"), b"elf");
        let ids: Vec<String> = discover(tmp.path())
            .unwrap()
            .into_iter()
            .map(|t| t.spec.id)
            .collect();
        assert_eq!(ids, vec!["bokai", "sidle"]);
    }

    /// bokai ships no tile at all — it is run over SSH. An extension without a
    /// front door is a normal app, not a broken one.
    #[test]
    fn an_app_with_no_tile_walks_fine() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(
            &dev.join("extensions/bokai/app.json"),
            br#"{"schema":1,"id":"bokai","name":"bokai","version":"0.1.4"}"#,
        );
        write(&dev.join("extensions/bokai/bin/bokai"), b"elf");
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        assert!(tree.files.iter().all(|f| f.path.starts_with("extensions/")));
        assert_eq!(
            tree.total_size(),
            tree.files.iter().map(|f| f.size).sum::<u64>()
        );
    }

    /// A tile a spec names and a build did not produce is an app that cannot be
    /// launched. Reporting the install as complete would hide it.
    #[test]
    fn a_declared_tile_that_is_not_there_fails_the_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(
            &dev.join("extensions/steb/app.json"),
            br#"{"schema":1,"id":"steb","name":"Steb","version":"0.1.0",
                 "tile":"documents/Steb.sh"}"#,
        );
        write(&dev.join("extensions/steb/bin/steb"), b"elf");
        let err = discover(tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("documents/Steb.sh"));
    }

    /// The picker's build time is compiled into its binary and the device
    /// refuses anything not strictly newer. An mtime cannot stand in for that
    /// — copying the file restamps it — so the sidecar beside it wins.
    #[test]
    fn a_build_ts_sidecar_beats_the_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(
            &dev.join("extensions/sidle/app.json"),
            br#"{"schema":1,"id":"sidle","name":"Sidle","version":"0.1.9",
                 "paths":[{"match":"extensions/sidle/bin/sidle.build-ts",
                           "class":"ignore"}]}"#,
        );
        write(&dev.join("extensions/sidle/bin/sidle"), b"elf");
        write(
            &dev.join("extensions/sidle/bin/sidle.build-ts"),
            b"1755712345\n",
        );
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        assert_eq!(
            tree.built_at(),
            1755712345,
            "app.json's mtime is `now` and larger; the stamp still wins"
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
        write(
            &dev.join("extensions/steb/app.json"),
            br#"{"schema":1,"id":"steb","name":"Steb","version":"0.1.0"}"#,
        );
        write(&dev.join("extensions/steb/bin/steb"), b"elf");
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        assert!(tree.built_at() > 0);
    }

    #[test]
    fn ignored_paths_never_reach_the_file_list() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("device");
        write(
            &dev.join("extensions/sidle/app.json"),
            br#"{"schema":1,"id":"sidle","name":"Sidle","version":"0.1.9",
                 "paths":[{"match":"extensions/sidle/etc/server.conf.example",
                           "class":"ignore"}]}"#,
        );
        write(
            &dev.join("extensions/sidle/etc/server.conf.example"),
            b"TOKEN=x\n",
        );
        write(&dev.join("extensions/sidle/config.xml"), b"<config/>");
        let tree = discover(tmp.path()).unwrap().pop().unwrap();
        let paths: Vec<&str> = tree.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["extensions/sidle/app.json", "extensions/sidle/config.xml"]
        );
    }

    /// A tree nested deeper than any repo puts one is not found, so "add this
    /// folder" cannot turn into a scan of everything below it.
    #[test]
    fn discovery_stops_before_it_becomes_a_filesystem_scan() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("a/b/c/d/extensions/deep/app.json"),
            br#"{"schema":1,"id":"deep","name":"Deep","version":"1"}"#,
        );
        assert!(discover(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn a_build_output_directory_is_not_searched() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("target/extensions/ghost/app.json"),
            br#"{"schema":1,"id":"ghost","name":"Ghost","version":"1"}"#,
        );
        assert!(discover(tmp.path()).unwrap().is_empty());
    }
}
