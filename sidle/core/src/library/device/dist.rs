//! What the Wi-Fi route offers, and where its bytes are read from.
//!
//! [`refresh`] describes a [`DevicePlan`] in two files under
//! `<data-dir>/device-dist/`, and copies nothing. `manifest.json` is what
//! `sidle-server` serves and `sidle-native` reads; `sources.json` stays on this
//! machine and points each path at the file it is served from.
//!
//! A registered app lives where its own build put it. Its bytes are read there
//! on the way out, the way a cable push reads them.
//!
//! A manifest `path` is mount-relative: one string keys the served route, the
//! on-device destination, and the `sources.json` entry.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::deploy::{DeploySource, atomic_write};
use super::digest::DigestCache;
use crate::library::apps::{Apply, DevicePlan};

/// Schema of `manifest.json`.
pub const MANIFEST_VERSION: u32 = 2;

pub const MANIFEST_NAME: &str = "manifest.json";

/// Host-side index of where each served path's bytes are read from. Never
/// served: it holds absolute paths on this machine.
pub const SOURCES_NAME: &str = "sources.json";

/// The key a picker built before `apps[]` reads, resolved against its own
/// bundle dir `/mnt/us/extensions/sidle`.
pub const LEGACY_BINARY_NAME: &str = "bin/sidle";

/// One file the manifest declares. `apply` is [`Apply::Direct`] (write `path`)
/// or [`Apply::Staged`] (write `<path>.new` for the applier one level up).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DistFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
    pub apply: Apply,
}

/// One app's slice of the offered tree. `built_at` is unix seconds, from the
/// tree's `.build-ts` sidecar or its newest mtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DistApp {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    pub built_at: u64,
    pub files: Vec<DistFile>,
}

/// The picker's binary under the key a picker built before `apps[]` reads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyFile {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    pub built_at: u64,
}

/// The `device-dist/manifest.json` contract. `sidle-server` and `sidle-native`
/// each mirror the part of it they read.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DistManifest {
    pub version: u32,
    pub files: Vec<LegacyFile>,
    pub apps: Vec<DistApp>,
}

impl DistManifest {
    /// Every mount-relative path the manifest declares.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.apps
            .iter()
            .flat_map(|a| a.files.iter())
            .map(|f| f.path.as_str())
    }
}

/// Where one served path's bytes are read from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceEntry {
    pub source: PathBuf,
}

/// `device-dist/sources.json`: served path → the file on this machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceIndex {
    pub files: BTreeMap<String, SourceEntry>,
}

impl SourceIndex {
    /// The file `name` is served from, or `None` for a path this index does not
    /// declare.
    pub fn source_of(&self, name: &str) -> Option<&Path> {
        self.files.get(name).map(|e| e.source.as_path())
    }
}

/// What a [`refresh`] call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// Hashed this many files, and wrote the manifest.
    Indexed(usize),
    /// Every source matches what the index recorded.
    UpToDate,
    /// `DeploySource::binary_path` does not exist — the armv7 cross-build has
    /// not run.
    SourceMissing,
}

pub fn manifest_path(dist_dir: &Path) -> PathBuf {
    dist_dir.join(MANIFEST_NAME)
}

pub fn sources_path(dist_dir: &Path) -> PathBuf {
    dist_dir.join(SOURCES_NAME)
}

/// The manifest, or `None` when it is absent or unparseable.
pub fn read_manifest(dist_dir: &Path) -> Option<DistManifest> {
    let bytes = std::fs::read(manifest_path(dist_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The source index, or `None` when it is absent or unparseable.
pub fn read_sources(dist_dir: &Path) -> Option<SourceIndex> {
    let bytes = std::fs::read(sources_path(dist_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Describe `plan` in `dist_dir`: the manifest a device reads, and the index
/// naming the file behind each path.
///
/// No app's bytes are copied, and `digests` hashes a file once per version of
/// it — a warm call costs one stat per file and reads nothing. Anything else
/// under `dist_dir` is deleted.
pub fn refresh(
    plan: &DevicePlan,
    source: &DeploySource,
    dist_dir: &Path,
    digests: &mut DigestCache,
) -> Result<RefreshOutcome> {
    let before = digests.hashed();
    let Some(binary) = digests.of(&source.binary_path)? else {
        return Ok(RefreshOutcome::SourceMissing);
    };
    let mut sources = SourceIndex::default();
    let mut apps = Vec::with_capacity(plan.apps.len());

    for tree in &plan.apps {
        let mut files = Vec::new();
        for planned in plan.files.iter().filter(|f| f.app_id == tree.app.id) {
            let digest = digests
                .of(&planned.source)?
                .with_context(|| format!("{} left the tree", planned.source.display()))?;
            files.push(DistFile {
                path: planned.path.clone(),
                sha256: digest.sha256,
                size: digest.size,
                apply: planned.apply,
            });
            sources.files.insert(
                planned.path.clone(),
                SourceEntry {
                    source: planned.source.clone(),
                },
            );
        }
        apps.push(DistApp {
            id: tree.app.id.clone(),
            name: tree.app.name.clone(),
            version: tree.app.version.clone(),
            built_at: tree.built_at(),
            files,
        });
    }

    // The picker's binary is served from the same file under a second key.
    let manifest = DistManifest {
        version: MANIFEST_VERSION,
        files: vec![LegacyFile {
            name: LEGACY_BINARY_NAME.to_string(),
            sha256: binary.sha256,
            size: binary.size,
            built_at: source.build_ts(),
        }],
        apps,
    };
    sources.files.insert(
        LEGACY_BINARY_NAME.to_string(),
        SourceEntry {
            source: source.binary_path.clone(),
        },
    );

    prune(dist_dir)?;
    let hashed = digests.hashed() - before;
    if hashed == 0
        && read_manifest(dist_dir).as_ref() == Some(&manifest)
        && read_sources(dist_dir).as_ref() == Some(&sources)
    {
        return Ok(RefreshOutcome::UpToDate);
    }
    write_json(&manifest_path(dist_dir), &manifest)?;
    write_json(&sources_path(dist_dir), &sources)?;
    Ok(RefreshOutcome::Indexed(hashed))
}

fn write_json<T: Serialize>(dest: &Path, value: &T) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(value).context("serialize a dist file")?;
    atomic_write(dest, &json)
}

/// Delete everything under `dist_dir` outside the two index files, then every
/// directory left empty.
fn prune(dist_dir: &Path) -> Result<()> {
    let keep: HashSet<&str> = [MANIFEST_NAME, SOURCES_NAME].into_iter().collect();
    let mut dirs = Vec::new();
    let mut stack = vec![dist_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path.clone());
                stack.push(path);
                continue;
            }
            let Ok(rel) = path.strip_prefix(dist_dir) else {
                continue;
            };
            if !keep.contains(rel.to_string_lossy().as_ref()) {
                std::fs::remove_file(&path)
                    .with_context(|| format!("remove {}", path.display()))?;
            }
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for dir in dirs {
        let _ = std::fs::remove_dir(&dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::deploy::sha256_bytes;
    use super::*;
    use crate::library::apps::plan_from;
    use std::time::{Duration, SystemTime};

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// Move a file's mtime forward, past what a previous `refresh` recorded.
    fn touch_ahead(path: &Path) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        let ahead = SystemTime::now() + Duration::from_secs(60);
        f.set_times(std::fs::FileTimes::new().set_modified(ahead))
            .unwrap();
    }

    /// A mount mirror holding the picker and one other app, plus the
    /// cross-built binary `DeploySource` points at.
    fn fixture(root: &Path) -> (DevicePlan, DeploySource) {
        let mount = root.join("device");
        write(&mount.join("extensions/sidle/bin/sidle"), b"picker-v1");
        write(&mount.join("extensions/sidle/bin/sidle.sh"), b"launcher");
        write(
            &mount.join("documents/Sidle.sh"),
            b"# Name: Sidle\nexec /mnt/us/extensions/sidle/bin/sidle.sh\n",
        );
        write(&mount.join("extensions/steb/bin/steb"), b"steb-v1");
        let binary = root.join("cross/sidle-native");
        write(&binary, b"picker-v1");

        let source = DeploySource {
            mount_dir: mount.clone(),
            binary_path: binary,
        };
        (plan_from(&mount, &[]), source)
    }

    #[test]
    fn refresh_describes_every_app_and_copies_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tempfile::tempdir().unwrap();
        let (plan, source) = fixture(tmp.path());

        assert_eq!(
            refresh(&plan, &source, dist.path(), &mut DigestCache::ephemeral()).unwrap(),
            RefreshOutcome::Indexed(5)
        );

        let staged: Vec<String> = std::fs::read_dir(dist.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            staged.len(),
            2,
            "two index files and no app bytes: {staged:?}"
        );

        let manifest = read_manifest(dist.path()).unwrap();
        assert_eq!(manifest.version, MANIFEST_VERSION);
        assert_eq!(manifest.files[0].name, LEGACY_BINARY_NAME);
        let ids: Vec<&str> = manifest.apps.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["sidle", "steb"]);

        let picker = manifest.apps.iter().find(|a| a.id == "sidle").unwrap();
        let launcher = picker
            .files
            .iter()
            .find(|f| f.path == "extensions/sidle/bin/sidle.sh")
            .unwrap();
        assert_eq!(launcher.apply, Apply::Staged, "the picker executes it");
        assert_eq!(launcher.sha256, sha256_bytes(b"launcher"));
        let tile = picker
            .files
            .iter()
            .find(|f| f.path == "documents/Sidle.sh")
            .unwrap();
        assert_eq!(
            tile.apply,
            Apply::Direct,
            "the tile execs away before the picker runs"
        );
    }

    #[test]
    fn every_served_path_points_at_the_file_it_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tempfile::tempdir().unwrap();
        let (plan, source) = fixture(tmp.path());
        refresh(&plan, &source, dist.path(), &mut DigestCache::ephemeral()).unwrap();

        let sources = read_sources(dist.path()).unwrap();
        assert_eq!(
            sources.source_of("extensions/steb/bin/steb"),
            Some(tmp.path().join("device/extensions/steb/bin/steb").as_path())
        );
        assert_eq!(
            sources.source_of(LEGACY_BINARY_NAME),
            Some(source.binary_path.as_path()),
            "the legacy key reads the picker's own binary"
        );
        assert_eq!(sources.source_of("documents/Nothing.sh"), None);
        for path in read_manifest(dist.path()).unwrap().paths() {
            assert!(sources.source_of(path).is_some(), "{path} has no source");
        }
    }

    #[test]
    fn a_second_call_hashes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tempfile::tempdir().unwrap();
        let (plan, source) = fixture(tmp.path());
        let mut digests = DigestCache::ephemeral();

        refresh(&plan, &source, dist.path(), &mut digests).unwrap();
        assert_eq!(
            refresh(&plan, &source, dist.path(), &mut digests).unwrap(),
            RefreshOutcome::UpToDate
        );
    }

    #[test]
    fn a_changed_source_rehashes_that_file_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tempfile::tempdir().unwrap();
        let (plan, source) = fixture(tmp.path());
        let mut digests = DigestCache::ephemeral();
        refresh(&plan, &source, dist.path(), &mut digests).unwrap();

        let steb = tmp.path().join("device/extensions/steb/bin/steb");
        std::fs::write(&steb, b"steb-v2-longer").unwrap();
        touch_ahead(&steb);
        let plan = plan_from(&source.mount_dir, &[]);

        assert_eq!(
            refresh(&plan, &source, dist.path(), &mut digests).unwrap(),
            RefreshOutcome::Indexed(1),
            "only the changed file is read again"
        );
        let entry = read_manifest(dist.path())
            .unwrap()
            .apps
            .into_iter()
            .find(|a| a.id == "steb")
            .and_then(|a| a.files.into_iter().next())
            .unwrap();
        assert_eq!(entry.sha256, sha256_bytes(b"steb-v2-longer"));
        assert_eq!(entry.size, 14);
    }

    #[test]
    fn a_dropped_app_stops_being_offered() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tempfile::tempdir().unwrap();
        let (plan, source) = fixture(tmp.path());
        refresh(&plan, &source, dist.path(), &mut DigestCache::ephemeral()).unwrap();

        std::fs::remove_dir_all(tmp.path().join("device/extensions/steb")).unwrap();
        let plan = plan_from(&source.mount_dir, &[]);
        refresh(&plan, &source, dist.path(), &mut DigestCache::ephemeral()).unwrap();

        let manifest = read_manifest(dist.path()).unwrap();
        assert!(manifest.apps.iter().all(|a| a.id != "steb"));
        assert!(
            read_sources(dist.path())
                .unwrap()
                .source_of("extensions/steb/bin/steb")
                .is_none()
        );
    }

    #[test]
    fn copies_an_earlier_build_left_behind_are_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tempfile::tempdir().unwrap();
        let (plan, source) = fixture(tmp.path());
        write(&dist.path().join("extensions/karyll/hid/big.so"), b"49MB");
        write(&dist.path().join(LEGACY_BINARY_NAME), b"picker-v1");

        refresh(&plan, &source, dist.path(), &mut DigestCache::ephemeral()).unwrap();

        assert!(!dist.path().join("extensions").exists());
        assert!(!dist.path().join("bin").exists());
        assert!(manifest_path(dist.path()).exists());
        assert!(sources_path(dist.path()).exists());
    }

    #[test]
    fn no_cross_built_binary_describes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tempfile::tempdir().unwrap();
        let (plan, mut source) = fixture(tmp.path());
        source.binary_path = tmp.path().join("cross/never-built");

        assert_eq!(
            refresh(&plan, &source, dist.path(), &mut DigestCache::ephemeral()).unwrap(),
            RefreshOutcome::SourceMissing
        );
        assert!(!manifest_path(dist.path()).exists());
    }
}
