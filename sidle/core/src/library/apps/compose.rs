//! The one list both routes install from.
//!
//! A cable push and a Wi-Fi pull have to deliver the same tree under the same
//! per-path rules, so neither reads the fleet for itself: both read a
//! [`DevicePlan`], which is every registered app's tree flattened into
//! mount-relative paths.
//!
//! # Where the trees come from
//!
//! Two sources, resolved identically once found. The **built-in** tree ships
//! with the desktop app — the `device/` mirror in a dev checkout, the staged
//! resources in a packaged one — and holds the picker and bokai. **Registered**
//! trees are rows in the `apps` table: a repo checkout the user pointed at, or
//! an unpacked release bundle. The built-in tree is not a row, because where it
//! is depends on how this binary was built, not on which library is open.
//!
//! # Nothing is materialised
//!
//! A plan is paths and their sources, not a copy of the bytes. karyll alone is
//! 51 MB, and a cable push reads each file once on its way to the device —
//! staging it into a second directory first would double the IO to no end. The
//! LAN route does materialise, because a server serves files out of a
//! directory; it materialises *from this plan*, so the two routes still carry
//! one list.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use super::spec::{Apply, FileClass, PathPolicy};
use super::tree::{AppFile, AppTree, discover, walk};
use crate::library::db::{self, AppSourceRow};

/// One file in the composed tree, and which app put it there.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// Mount-relative — the manifest key, the served route, and the on-device
    /// destination, all one string.
    pub path: String,
    pub source: PathBuf,
    pub size: u64,
    pub policy: PathPolicy,
    /// The app whose tree this came from.
    pub app_id: String,
}

/// Two apps claiming one mount path. Reported rather than resolved: the loser's
/// file would silently never install, and which one lost would depend on the
/// order rows came back in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PathConflict {
    pub path: String,
    pub kept: String,
    pub dropped: String,
}

/// A source that could not be read. One bad row does not stop the rest: a repo
/// the user moved should cost that app, not the push.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppSourceError {
    pub id: String,
    pub source: String,
    pub error: String,
}

/// Every app the fleet should have, flattened.
#[derive(Debug, Clone, Default)]
pub struct DevicePlan {
    /// One per app, sorted by id.
    pub apps: Vec<AppTree>,
    /// Every installable file, sorted by mount-relative path.
    pub files: Vec<PlannedFile>,
    pub conflicts: Vec<PathConflict>,
    pub errors: Vec<AppSourceError>,
}

impl DevicePlan {
    pub fn app(&self, id: &str) -> Option<&AppTree> {
        self.apps.iter().find(|a| a.spec.id == id)
    }

    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// The files an update decides by hash, which is what a status check has to
    /// read. `seed` files are decided by existence alone.
    pub fn sync_files(&self) -> impl Iterator<Item = &PlannedFile> {
        self.files
            .iter()
            .filter(|f| f.policy.class == FileClass::Sync)
    }

    /// Paths written as `<path>.new` for something one level up to swap in,
    /// because the device is executing them at the moment of the update.
    pub fn staged_paths(&self) -> impl Iterator<Item = &str> {
        self.files
            .iter()
            .filter(|f| f.policy.apply == Apply::Staged)
            .map(|f| f.path.as_str())
    }
}

/// Compose the built-in tree with every registered app.
///
/// `builtin` is the mount root that ships with the desktop app. Errors reading
/// one registered source are collected into [`DevicePlan::errors`] rather than
/// returned, so a repo that moved costs its own app and nothing else. A failure
/// to read the built-in tree *is* returned: without it there is no picker, and
/// a push that quietly omitted it would leave a device with no way in.
pub fn plan(conn: &rusqlite::Connection, builtin: &Path) -> Result<DevicePlan> {
    let rows = db::list_app_sources(conn).context("read the registered apps")?;
    Ok(plan_from(builtin, &rows))
}

/// [`plan`] against an explicit row list, for callers that already hold one.
pub fn plan_from(builtin: &Path, rows: &[AppSourceRow]) -> DevicePlan {
    let mut trees = Vec::new();
    let mut errors = Vec::new();

    match discover(builtin) {
        Ok(found) => trees.extend(found),
        Err(e) => errors.push(AppSourceError {
            id: String::new(),
            source: builtin.display().to_string(),
            error: format!("{e:#}"),
        }),
    }

    // Ids the built-in tree already provides win over a row claiming the same
    // one: the picker that ships with this binary is the picker this binary
    // knows how to render a `server.conf` for.
    for row in rows {
        if trees.iter().any(|t: &AppTree| t.spec.id == row.id) {
            continue;
        }
        match read_row(row) {
            Ok(tree) => trees.push(tree),
            Err(e) => errors.push(AppSourceError {
                id: row.id.clone(),
                source: row.source.clone(),
                error: format!("{e:#}"),
            }),
        }
    }
    trees.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));

    let (files, conflicts) = flatten(&trees);
    DevicePlan {
        apps: trees,
        files,
        conflicts,
        errors,
    }
}

/// Read one registered app's tree from the root its row records.
fn read_row(row: &AppSourceRow) -> Result<AppTree> {
    let root = PathBuf::from(&row.root);
    let spec_path = root
        .join("extensions")
        .join(&row.id)
        .join(super::spec::APP_SPEC_FILE);
    let spec = super::spec::AppSpec::load(&spec_path)?;
    walk(&root, &spec)
}

/// Merge every tree's files into one path-keyed list.
fn flatten(trees: &[AppTree]) -> (Vec<PlannedFile>, Vec<PathConflict>) {
    let mut by_path: HashMap<&str, PlannedFile> = HashMap::new();
    let mut conflicts = Vec::new();
    for tree in trees {
        for f in &tree.files {
            let planned = to_planned(f, &tree.spec.id);
            match by_path.get(f.path.as_str()) {
                Some(kept) => conflicts.push(PathConflict {
                    path: f.path.clone(),
                    kept: kept.app_id.clone(),
                    dropped: tree.spec.id.clone(),
                }),
                None => {
                    by_path.insert(f.path.as_str(), planned);
                }
            }
        }
    }
    let mut files: Vec<PlannedFile> = by_path.into_values().collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    conflicts.sort_by(|a, b| a.path.cmp(&b.path));
    (files, conflicts)
}

fn to_planned(f: &AppFile, app_id: &str) -> PlannedFile {
    PlannedFile {
        path: f.path.clone(),
        source: f.source.clone(),
        size: f.size,
        policy: f.policy,
        app_id: app_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// The tree that ships with the desktop app: the picker and bokai.
    fn builtin(root: &Path) -> PathBuf {
        let dev = root.join("device");
        write(
            &dev.join("extensions/sidle/app.json"),
            br#"{"schema":1,"id":"sidle","name":"Sidle","version":"0.1.9",
                 "tile":"documents/Sidle.sh","pidof":"sidle",
                 "paths":[{"match":"extensions/sidle/bin/sidle","apply":"staged"},
                          {"match":"extensions/sidle/bin/sidle.sh","apply":"staged"}]}"#,
        );
        write(&dev.join("extensions/sidle/bin/sidle"), b"picker");
        write(&dev.join("extensions/sidle/bin/sidle.sh"), b"#!/bin/sh\n");
        write(&dev.join("documents/Sidle.sh"), b"# Name: Sidle\n");
        write(
            &dev.join("extensions/bokai/app.json"),
            br#"{"schema":1,"id":"bokai","name":"bokai","version":"0.1.4"}"#,
        );
        write(&dev.join("extensions/bokai/bin/bokai"), b"engine");
        dev
    }

    fn karyll_repo(root: &Path) -> AppSourceRow {
        let out = root.join("deploy").join("out");
        write(
            &out.join("extensions/karyll/app.json"),
            br#"{"schema":1,"id":"karyll","name":"Karyll","version":"0.2.4",
                 "tile":"documents/Karyll.sh",
                 "paths":[{"match":"extensions/karyll/hid/","class":"seed","seed_gen":1}]}"#,
        );
        write(&out.join("extensions/karyll/bin/karyll"), b"armhf");
        write(&out.join("extensions/karyll/hid/config.ini"), b"[device]\n");
        write(&out.join("documents/Karyll.sh"), b"# Name: Karyll\n");
        AppSourceRow {
            id: "karyll".into(),
            source_kind: db::APP_SOURCE_LOCAL.into(),
            source: root.display().to_string(),
            root: out.display().to_string(),
            added_at: 0,
        }
    }

    #[test]
    fn the_builtin_tree_alone_is_a_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let plan = plan_from(&dev, &[]);
        let ids: Vec<&str> = plan.apps.iter().map(|a| a.spec.id.as_str()).collect();
        assert_eq!(ids, vec!["bokai", "sidle"]);
        assert!(plan.errors.is_empty());
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn a_registered_app_joins_the_same_list() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let repo = tempfile::tempdir().unwrap();
        let plan = plan_from(&dev, &[karyll_repo(repo.path())]);
        let paths: Vec<&str> = plan.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "documents/Karyll.sh",
                "documents/Sidle.sh",
                "extensions/bokai/app.json",
                "extensions/bokai/bin/bokai",
                "extensions/karyll/app.json",
                "extensions/karyll/bin/karyll",
                "extensions/karyll/hid/config.ini",
                "extensions/sidle/app.json",
                "extensions/sidle/bin/sidle",
                "extensions/sidle/bin/sidle.sh",
            ],
            "one list, sorted by mount path, regardless of which tree each came from"
        );
        assert_eq!(
            plan.total_size(),
            plan.files.iter().map(|f| f.size).sum::<u64>()
        );
    }

    #[test]
    fn per_path_classes_survive_the_flatten() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let repo = tempfile::tempdir().unwrap();
        let plan = plan_from(&dev, &[karyll_repo(repo.path())]);

        let hid = plan
            .files
            .iter()
            .find(|f| f.path == "extensions/karyll/hid/config.ini")
            .unwrap();
        assert_eq!(hid.policy.class, FileClass::Seed);
        assert_eq!(hid.app_id, "karyll");

        let staged: Vec<&str> = plan.staged_paths().collect();
        assert_eq!(
            staged,
            vec![
                "extensions/sidle/bin/sidle",
                "extensions/sidle/bin/sidle.sh"
            ],
            "only the two files the device is executing are staged"
        );
        assert!(
            plan.sync_files()
                .all(|f| f.path != "extensions/karyll/hid/config.ini")
        );
    }

    /// A repo the user moved costs that app, not the push. The picker still
    /// installs, which is the file a stranded device most needs.
    #[test]
    fn one_unreadable_source_does_not_take_the_others_down() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let gone = AppSourceRow {
            id: "steb".into(),
            source_kind: db::APP_SOURCE_LOCAL.into(),
            source: "/nowhere/steb".into(),
            root: "/nowhere/steb/device".into(),
            added_at: 0,
        };
        let plan = plan_from(&dev, &[gone]);
        assert_eq!(plan.errors.len(), 1);
        assert_eq!(plan.errors[0].id, "steb");
        assert!(plan.app("sidle").is_some());
        assert!(plan.app("steb").is_none());
    }

    /// The picker that ships with this binary is the one it knows how to render
    /// a `server.conf` for, so a row claiming `sidle` does not displace it.
    #[test]
    fn the_builtin_tree_wins_an_id_a_row_also_claims() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let other = tempfile::tempdir().unwrap();
        let out = other.path().join("device");
        write(
            &out.join("extensions/sidle/app.json"),
            br#"{"schema":1,"id":"sidle","name":"Impostor","version":"9.9.9"}"#,
        );
        write(&out.join("extensions/sidle/bin/sidle"), b"not the picker");
        let row = AppSourceRow {
            id: "sidle".into(),
            source_kind: db::APP_SOURCE_LOCAL.into(),
            source: other.path().display().to_string(),
            root: out.display().to_string(),
            added_at: 0,
        };
        let plan = plan_from(&dev, &[row]);
        assert_eq!(plan.app("sidle").unwrap().spec.name, "Sidle");
        assert_eq!(
            std::fs::read(&plan.app("sidle").unwrap().files[0].source).unwrap(),
            b"# Name: Sidle\n"
        );
    }

    /// Two apps claiming one path is reported, not silently resolved — the
    /// loser's file would never install and which one lost would depend on row
    /// order.
    #[test]
    fn a_contested_path_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let other = tempfile::tempdir().unwrap();
        let out = other.path().join("device");
        write(
            &out.join("extensions/rogue/app.json"),
            br#"{"schema":1,"id":"rogue","name":"Rogue","version":"1",
                 "tile":"documents/Sidle.sh"}"#,
        );
        write(&out.join("documents/Sidle.sh"), b"# Name: Rogue\n");
        let row = AppSourceRow {
            id: "rogue".into(),
            source_kind: db::APP_SOURCE_LOCAL.into(),
            source: other.path().display().to_string(),
            root: out.display().to_string(),
            added_at: 0,
        };
        let plan = plan_from(&dev, &[row]);
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].path, "documents/Sidle.sh");
        assert_eq!(
            plan.files
                .iter()
                .filter(|f| f.path == "documents/Sidle.sh")
                .count(),
            1
        );
    }
}
