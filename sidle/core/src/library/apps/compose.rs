//! The one list both routes install from.
//!
//! A cable push and a Wi-Fi pull deliver the same tree under the same per-path
//! rules: both read a [`DevicePlan`], every registered app's tree flattened
//! into mount-relative paths.
//!
//! # Where the trees come from
//!
//! Two sources, resolved identically once found. The **built-in** tree ships
//! with the desktop app — the `device/` mirror in a dev checkout, the staged
//! resources in a packaged one — and holds the picker and bokai. **Registered**
//! trees are rows in the `apps` table: a repo checkout, or an unpacked release
//! bundle. Where the built-in tree sits follows this binary's build, and it is
//! not a row.
//!
//! # Nothing is materialised
//!
//! A plan is paths and their sources, not a copy of the bytes: a cable push
//! reads each file once on its way to the device. `device::dist` materialises
//! *from this plan* for the server, which serves files out of a directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use super::policy::Apply;
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
    pub apply: Apply,
    /// The app whose tree this came from.
    pub app_id: String,
}

/// Two apps claiming one mount path, reported and not resolved. The dropped
/// app's file never installs, and row order picks which one it is.
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
        self.apps.iter().find(|a| a.app.id == id)
    }

    /// The same plan narrowed to the apps `ids` names, for a per-row install
    /// and for a push scoped to what a device holds.
    ///
    /// Conflicts and errors do not carry across: both are facts about the whole
    /// fleet, and name apps this install leaves alone.
    pub fn only<S: AsRef<str>>(&self, ids: &[S]) -> DevicePlan {
        let wanted = |id: &str| ids.iter().any(|w| w.as_ref() == id);
        DevicePlan {
            apps: self
                .apps
                .iter()
                .filter(|a| wanted(&a.app.id))
                .cloned()
                .collect(),
            files: self
                .files
                .iter()
                .filter(|f| wanted(&f.app_id))
                .cloned()
                .collect(),
            conflicts: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// [`DevicePlan::only`], refusing an id the fleet holds no tree for. An id
    /// standing in [`DevicePlan::errors`] is refused with the error its source
    /// produced.
    pub fn narrow<S: AsRef<str>>(&self, ids: &[S]) -> Result<DevicePlan> {
        for id in ids {
            let id = id.as_ref();
            if self.app(id).is_some() {
                continue;
            }
            match self.errors.iter().find(|e| e.id == id) {
                Some(e) => anyhow::bail!("{id}: {}", e.error),
                None => anyhow::bail!("no app named {id} in the fleet"),
            }
        }
        Ok(self.only(ids))
    }

    /// Which app owns a mount-relative path, if any.
    pub fn owner_of(&self, path: &str) -> Option<&str> {
        self.files
            .iter()
            .find(|f| f.path == path)
            .map(|f| f.app_id.as_str())
    }

    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Paths written as `<path>.new` for the process one level up to swap in.
    /// The device is executing them at the moment of the update.
    pub fn staged_paths(&self) -> impl Iterator<Item = &str> {
        self.files
            .iter()
            .filter(|f| f.apply == Apply::Staged)
            .map(|f| f.path.as_str())
    }
}

/// Compose the built-in tree with every registered app.
///
/// `builtin` is the mount root that ships with the desktop app. An error
/// reading one registered source lands in [`DevicePlan::errors`], costing that
/// app alone. An error reading the built-in tree is returned: it holds the
/// picker.
pub fn plan(conn: &rusqlite::Connection, builtin: &Path) -> Result<DevicePlan> {
    let rows = db::list_app_sources(conn).context("read the registered apps")?;
    Ok(plan_from(builtin, &rows))
}

/// [`plan`] against an explicit row list, for a caller holding one.
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

    // An id the built-in tree provides wins over a row claiming it: the picker
    // shipping with this binary is the one it renders a `server.conf` for.
    for row in rows {
        if trees.iter().any(|t: &AppTree| t.app.id == row.id) {
            continue;
        }
        match walk(Path::new(&row.root), &row.id) {
            Ok(tree) => trees.push(tree),
            Err(e) => errors.push(AppSourceError {
                id: row.id.clone(),
                source: row.source.clone(),
                error: format!("{e:#}"),
            }),
        }
    }
    trees.sort_by(|a, b| a.app.id.cmp(&b.app.id));

    let (files, conflicts) = flatten(&trees);
    DevicePlan {
        apps: trees,
        files,
        conflicts,
        errors,
    }
}

/// Merge every tree's files into one path-keyed list.
fn flatten(trees: &[AppTree]) -> (Vec<PlannedFile>, Vec<PathConflict>) {
    let mut by_path: HashMap<&str, PlannedFile> = HashMap::new();
    let mut conflicts = Vec::new();
    for tree in trees {
        for f in &tree.files {
            let planned = to_planned(f, &tree.app.id);
            match by_path.get(f.path.as_str()) {
                Some(kept) => conflicts.push(PathConflict {
                    path: f.path.clone(),
                    kept: kept.app_id.clone(),
                    dropped: tree.app.id.clone(),
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
        apply: f.apply,
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
            &dev.join("extensions/sidle/config.xml"),
            br#"<extension><information><name>Sidle</name>
                <version>0.1.9</version></information></extension>"#,
        );
        write(&dev.join("extensions/sidle/bin/sidle"), b"picker");
        write(&dev.join("extensions/sidle/bin/sidle.sh"), b"#!/bin/sh\n");
        write(
            &dev.join("documents/Sidle.sh"),
            b"#!/bin/sh\n# Name: Sidle\nexec /mnt/us/extensions/sidle/bin/sidle.sh\n",
        );
        write(&dev.join("extensions/bokai/bin/bokai"), b"engine");
        dev
    }

    fn sprocket_repo(root: &Path) -> AppSourceRow {
        let out = root.join("deploy").join("out");
        write(&out.join("extensions/sprocket/bin/sprocket"), b"armhf");
        write(
            &out.join("extensions/sprocket/hid/config.ini"),
            b"[device]\n",
        );
        write(
            &out.join("documents/Sprocket.sh"),
            b"#!/bin/sh\n# Name: Sprocket\nexec /mnt/us/extensions/sprocket/bin/sprocket.sh\n",
        );
        AppSourceRow {
            id: "sprocket".into(),
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
        let ids: Vec<&str> = plan.apps.iter().map(|a| a.app.id.as_str()).collect();
        assert_eq!(ids, vec!["bokai", "sidle"]);
        assert!(plan.errors.is_empty());
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn a_registered_app_joins_the_same_list() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let repo = tempfile::tempdir().unwrap();
        let plan = plan_from(&dev, &[sprocket_repo(repo.path())]);
        let paths: Vec<&str> = plan.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "documents/Sidle.sh",
                "documents/Sprocket.sh",
                "extensions/bokai/bin/bokai",
                "extensions/sidle/bin/sidle",
                "extensions/sidle/bin/sidle.sh",
                "extensions/sidle/config.xml",
                "extensions/sprocket/bin/sprocket",
                "extensions/sprocket/hid/config.ini",
            ],
            "one list, sorted by mount path, regardless of which tree each came from"
        );
        assert_eq!(
            plan.total_size(),
            plan.files.iter().map(|f| f.size).sum::<u64>()
        );
        assert_eq!(plan.app("sprocket").unwrap().app.name, "Sprocket");
    }

    #[test]
    fn the_staged_pair_survives_the_flatten() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let repo = tempfile::tempdir().unwrap();
        let plan = plan_from(&dev, &[sprocket_repo(repo.path())]);
        let staged: Vec<&str> = plan.staged_paths().collect();
        assert_eq!(
            staged,
            vec![
                "extensions/sidle/bin/sidle",
                "extensions/sidle/bin/sidle.sh"
            ],
            "only the two files the picker executes are staged"
        );
        assert_eq!(
            plan.owner_of("extensions/sprocket/hid/config.ini"),
            Some("sprocket")
        );
    }

    /// A moved repo costs that app, not the push: the picker installs.
    #[test]
    fn one_unreadable_source_does_not_take_the_others_down() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let gone = AppSourceRow {
            id: "gadget".into(),
            source_kind: db::APP_SOURCE_LOCAL.into(),
            source: "/nowhere/gadget".into(),
            root: "/nowhere/gadget/device".into(),
            added_at: 0,
        };
        let plan = plan_from(&dev, &[gone]);
        assert_eq!(plan.errors.len(), 1);
        assert_eq!(plan.errors[0].id, "gadget");
        assert!(plan.app("sidle").is_some());
        assert!(plan.app("gadget").is_none());
    }

    /// A row claiming `sidle` does not displace the picker shipping with this
    /// binary, the one it renders a `server.conf` for.
    #[test]
    fn the_builtin_tree_wins_an_id_a_row_also_claims() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let other = tempfile::tempdir().unwrap();
        let out = other.path().join("device");
        write(&out.join("extensions/sidle/bin/sidle"), b"not the picker");
        let row = AppSourceRow {
            id: "sidle".into(),
            source_kind: db::APP_SOURCE_LOCAL.into(),
            source: other.path().display().to_string(),
            root: out.display().to_string(),
            added_at: 0,
        };
        let plan = plan_from(&dev, &[row]);
        let picker = plan.app("sidle").unwrap();
        assert_eq!(picker.app.name, "Sidle");
        assert_eq!(picker.root, dev);
    }

    /// Two apps claiming one path is reported, not resolved: the dropped app's
    /// file never installs, and row order picks it.
    #[test]
    fn a_contested_path_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let other = tempfile::tempdir().unwrap();
        let out = other.path().join("device");
        write(&out.join("extensions/rogue/bin/rogue"), b"elf");
        // A tile that launches both apps belongs to whichever the plan reaches
        // first, and the other one loses a file it thought it shipped.
        write(
            &out.join("documents/Sidle.sh"),
            b"#!/bin/sh\n# Name: Rogue\nexec /mnt/us/extensions/rogue/bin/rogue\n",
        );
        write(&out.join("extensions/sidle/bin/sidle"), b"decoy");
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

    #[test]
    fn narrowing_keeps_one_app_and_its_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let repo = tempfile::tempdir().unwrap();
        let plan = plan_from(&dev, &[sprocket_repo(repo.path())])
            .narrow(&["sprocket"])
            .unwrap();
        let ids: Vec<&str> = plan.apps.iter().map(|a| a.app.id.as_str()).collect();
        assert_eq!(ids, vec!["sprocket"]);
        let paths: Vec<&str> = plan.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "documents/Sprocket.sh",
                "extensions/sprocket/bin/sprocket",
                "extensions/sprocket/hid/config.ini",
            ]
        );
    }

    /// A push scoped to what a device holds names several ids at once, and one
    /// it leaves out contributes no file.
    #[test]
    fn narrowing_takes_a_set_of_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let repo = tempfile::tempdir().unwrap();
        let fleet = plan_from(&dev, &[sprocket_repo(repo.path())]);

        let both = fleet.narrow(&["bokai", "sprocket"]).unwrap();
        let ids: Vec<&str> = both.apps.iter().map(|a| a.app.id.as_str()).collect();
        assert_eq!(ids, vec!["bokai", "sprocket"]);
        assert!(both.files.iter().all(|f| f.app_id != "sidle"));

        let none = fleet.narrow::<String>(&[]).unwrap();
        assert!(none.apps.is_empty());
        assert!(none.files.is_empty());
    }

    /// An id the fleet holds no tree for is refused, and one whose source
    /// failed to read is refused with the error that source produced.
    #[test]
    fn narrowing_names_why_an_id_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = builtin(tmp.path());
        let gone = AppSourceRow {
            id: "gadget".into(),
            source_kind: db::APP_SOURCE_LOCAL.into(),
            source: "/nowhere/gadget".into(),
            root: "/nowhere/gadget/device".into(),
            added_at: 0,
        };
        let plan = plan_from(&dev, &[gone]);
        let refused = plan.narrow(&["gadget"]).unwrap_err().to_string();
        assert!(refused.starts_with("gadget: "), "{refused}");
        assert!(!refused.contains("no app named"), "{refused}");
        assert_eq!(
            plan.narrow(&["sprocket"]).unwrap_err().to_string(),
            "no app named sprocket in the fleet"
        );
    }
}
