//! `app.json` — what a repo declares about the app it ships.
//!
//! Every app in the fleet installs the same way: a **mount-rooted tree** whose
//! entries are paths under `/mnt/us`, plus one metadata file. The metadata sits
//! inside the tree at `extensions/<id>/app.json`, so it travels with the app —
//! a zip, an unpacked bundle and a device all carry the same statement of what
//! the app is, and sidle is a reader of it rather than a registry of it. The
//! repo that owns the app owns its metadata; nothing here is sidle-specific and
//! nothing in the four app repos depends on this crate.
//!
//! ```jsonc
//! {
//!   "schema": 1,
//!   "id": "karyll",              // must equal the containing directory's name
//!   "name": "Karyll",            // for the Apps tab
//!   "version": "0.4.0",          // stamped by the repo's build.sh
//!   "tile": "documents/Karyll.sh",   // omit for an extension with no tile
//!   "pidof": "karyll",               // process name; omit if it never runs resident
//!   "paths": [
//!     { "match": "extensions/karyll/hid/", "class": "seed", "seed_gen": 1 }
//!   ]
//! }
//! ```
//!
//! # `paths` classifies, it does not enumerate
//!
//! The file list comes from walking the tree, never from `app.json`: karyll's
//! vendored Bluetooth stack is 100 files, and a hand-written list of them would
//! be wrong one vendored bump later. `paths` holds **rules**, matched in order,
//! first match wins for all of that rule's fields; anything unmatched is
//! `sync`/`direct`. `seed` and `staged` are therefore opt-in, which is the safe
//! direction — a rule someone forgot yields a file that updates too eagerly,
//! not one that never updates at all.
//!
//! A `match` ending in `/` is a subtree prefix; anything else is one exact
//! mount-relative path. That is the whole matcher — no globs. Every rule the
//! fleet needs today is one of those two shapes, and adding a glob later
//! reads the same files without invalidating any of them.
//!
//! # The classes
//!
//! `sync` is written when its hash differs from the bundle's: binaries,
//! launchers, tiles, fonts, libraries. `seed` is written only when the path is
//! **absent** on device — on-device config a user may have edited, and vendored
//! stacks whose replacement costs a re-download. A `seed` path is also checked
//! for existence alone rather than hashed, which is what keeps a status check
//! off 49 MB of files no update will ever write.
//!
//! `ignore` is the third: in the tree, not part of the app. A repo's mirror is
//! a mirror of the mount, so it holds the odd file that documents the install
//! rather than belonging to it — `etc/server.conf.example` sits beside the
//! `etc/server.conf` the desktop renders per device, and pushing the example
//! would put a fake token where the real one goes. Naming it in `app.json`
//! keeps that fact in the repo that owns the file.
//!
//! When a release genuinely has to replace a `seed` file, the rule bumps its
//! `seed_gen`. The install receipt records the generation that landed, and a
//! higher generation in the spec makes the path writable once.
//!
//! # `apply`
//!
//! `direct` writes the path. `staged` writes `<path>.new` for a process one
//! level above the one executing it to swap in — a file cannot be rewritten
//! while `sh` is reading it by offset off a mount with no inodes. Only sidle's
//! own two files (`extensions/sidle/bin/sidle` and `.../bin/sidle.sh`) are
//! `staged`, and only over Wi-Fi; a cable push has nothing running on the
//! device and writes every path directly.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// The metadata file's name, inside the app's own extension directory.
pub const APP_SPEC_FILE: &str = "app.json";

/// The only `schema` value this build reads. A spec that names a higher one is
/// refused rather than guessed at: the fields it would add are the ones that
/// decide whether a file gets overwritten.
pub const APP_SPEC_SCHEMA: u32 = 1;

/// Whether a path is kept in step with the bundle or planted once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileClass {
    /// Written whenever its hash differs from the bundle's.
    #[default]
    Sync,
    /// Written only when absent on device (or when `seed_gen` outranks the
    /// generation the receipt recorded).
    Seed,
    /// Never written, never hashed, never in the manifest. The path exists in
    /// the repo's mirror for a reader, not for a device.
    Ignore,
}

/// How a write lands on the device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Apply {
    /// Write the path itself.
    #[default]
    Direct,
    /// Write `<path>.new`; something one level up swaps it in. Only meaningful
    /// for a file the device is executing at the moment of the update.
    Staged,
}

/// One classification rule. Fields left out take their defaults, so the common
/// rule is a `match` plus the one thing it is there to say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathRule {
    /// Mount-relative. Ends with `/` to match a subtree; otherwise matches one
    /// exact path.
    #[serde(rename = "match")]
    pub pattern: String,
    #[serde(default)]
    pub class: FileClass,
    /// Generation of a `seed` path's contents. Bumping it re-authorises one
    /// overwrite. Defaults to 1; meaningless — and rejected — on a `sync` rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_gen: Option<u32>,
    #[serde(default)]
    pub apply: Apply,
}

impl PathRule {
    /// Does this rule cover `mount_rel`? A trailing `/` makes the pattern a
    /// subtree prefix; without one it is an exact path.
    fn matches(&self, mount_rel: &str) -> bool {
        if let Some(dir) = self.pattern.strip_suffix('/') {
            // `dir` itself is not a file, so a bare prefix match would only ever
            // fire on `<dir>/...` anyway — but spelling the separator keeps
            // `hid/` from matching a sibling `hidden/`.
            mount_rel.starts_with(&format!("{dir}/"))
        } else {
            mount_rel == self.pattern
        }
    }
}

/// What a rule resolves to for one concrete path, defaults filled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PathPolicy {
    pub class: FileClass,
    /// 1 for a `seed` path with no explicit generation; 0 for a `sync` path,
    /// where the concept does not apply and the receipt records nothing.
    pub seed_gen: u32,
    pub apply: Apply,
}

/// A repo's `app.json`, parsed and validated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSpec {
    pub schema: u32,
    /// Directory name under `extensions/`, and the app's identity everywhere
    /// else — the `apps` table's key, the manifest's per-app group, the receipt.
    pub id: String,
    /// Display name for the Apps tab.
    pub name: String,
    /// Whatever the repo calls this build. Compared as an opaque string; the
    /// downgrade guard uses `built_at`, not this.
    pub version: String,
    /// The app's launcher tile, mount-relative under `documents/`. Absent for
    /// an extension with no front door — bokai is run over SSH and has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tile: Option<String>,
    /// Process name to look for before overwriting a running app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pidof: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<PathRule>,
}

impl AppSpec {
    /// Parse and validate `app.json` bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let spec: AppSpec = serde_json::from_slice(bytes).context("parse app.json")?;
        spec.validate()?;
        Ok(spec)
    }

    /// Read `app.json` from `extensions/<id>/app.json` and check that the `id`
    /// it declares is the directory it was found in. The two disagreeing means
    /// every path this app owns is addressed under a name it does not install
    /// to, so it is an error rather than a preference for one side.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let spec = Self::parse(&bytes).with_context(|| path.display().to_string())?;
        let dir = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if dir != spec.id {
            bail!(
                "{}: declares id `{}` but sits in `extensions/{}` — the id names \
                 the directory the app installs to, so they cannot differ",
                path.display(),
                spec.id,
                dir
            );
        }
        Ok(spec)
    }

    /// The rule that governs `mount_rel`, defaults applied. Unmatched paths are
    /// `sync`/`direct`.
    pub fn policy_for(&self, mount_rel: &str) -> PathPolicy {
        match self.paths.iter().find(|r| r.matches(mount_rel)) {
            Some(rule) => PathPolicy {
                class: rule.class,
                seed_gen: match rule.class {
                    FileClass::Seed => rule.seed_gen.unwrap_or(1),
                    FileClass::Sync | FileClass::Ignore => 0,
                },
                apply: rule.apply,
            },
            None => PathPolicy {
                class: FileClass::Sync,
                seed_gen: 0,
                apply: Apply::Direct,
            },
        }
    }

    /// The app's own directory, mount-relative: `extensions/<id>`.
    pub fn extension_dir(&self) -> String {
        format!("extensions/{}", self.id)
    }

    fn validate(&self) -> Result<()> {
        if self.schema != APP_SPEC_SCHEMA {
            bail!(
                "app.json schema {} — this build reads {APP_SPEC_SCHEMA} only",
                self.schema
            );
        }
        validate_id(&self.id)?;
        if self.name.trim().is_empty() {
            bail!("app.json: `name` is empty");
        }
        if self.version.trim().is_empty() {
            bail!("app.json: `version` is empty");
        }
        if let Some(tile) = &self.tile {
            validate_mount_rel(tile).with_context(|| format!("app.json: `tile` {tile:?}"))?;
            if !tile.starts_with("documents/") {
                bail!(
                    "app.json: `tile` is {tile:?} — the library only indexes tiles \
                     from documents/, so a tile anywhere else is never seen"
                );
            }
        }
        for rule in &self.paths {
            let target = rule.pattern.strip_suffix('/').unwrap_or(&rule.pattern);
            validate_mount_rel(target)
                .with_context(|| format!("app.json: `match` {:?}", rule.pattern))?;
            if rule.class != FileClass::Seed && rule.seed_gen.is_some() {
                bail!(
                    "app.json: `match` {:?} sets seed_gen on a {:?} rule — seed_gen \
                     re-authorises one overwrite of a seed path and does nothing here",
                    rule.pattern,
                    rule.class
                );
            }
            if rule.seed_gen == Some(0) {
                bail!(
                    "app.json: `match` {:?} sets seed_gen 0 — generations start at 1, \
                     and 0 is what a receipt records for a path it has never seen",
                    rule.pattern
                );
            }
        }
        Ok(())
    }
}

/// An id is a directory name under `extensions/`, so it has to survive FAT and
/// a URL path segment both.
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("app.json: `id` is empty");
    }
    let ok = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
    if !ok || id.starts_with('.') {
        bail!(
            "app.json: `id` is {id:?} — it names a directory under extensions/ and a \
             segment of the file route, so it is lowercase ASCII, digits, `-`, `_`, `.`"
        );
    }
    Ok(())
}

/// A mount-relative path: what goes on the wire, what keys the manifest, and
/// what the destination is resolved against. Anything that could escape the
/// mount root or mean two things on two filesystems is refused here rather than
/// at the point it would have written outside the tree.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn karyll() -> AppSpec {
        AppSpec::parse(
            br#"{
              "schema": 1,
              "id": "karyll",
              "name": "Karyll",
              "version": "0.4.0",
              "tile": "documents/Karyll.sh",
              "pidof": "karyll",
              "paths": [
                { "match": "extensions/karyll/hid/", "class": "seed", "seed_gen": 1 }
              ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn minimal_spec_needs_only_identity() {
        let spec = AppSpec::parse(br#"{"schema":1,"id":"bokai","name":"bokai","version":"0.1.4"}"#)
            .unwrap();
        assert!(spec.tile.is_none(), "an extension may have no tile at all");
        assert!(spec.paths.is_empty());
        assert_eq!(spec.extension_dir(), "extensions/bokai");
    }

    /// The default has to be `sync`: a rule someone forgot then yields a file
    /// that updates when it should not, which a re-push fixes. The opposite
    /// default yields a file that never updates again, which nothing surfaces.
    #[test]
    fn unmatched_paths_are_sync_and_direct() {
        let p = karyll().policy_for("extensions/karyll/bin/karyll");
        assert_eq!(p.class, FileClass::Sync);
        assert_eq!(p.apply, Apply::Direct);
        assert_eq!(p.seed_gen, 0, "sync paths carry no generation");
    }

    #[test]
    fn trailing_slash_matches_the_subtree_only() {
        let spec = karyll();
        assert_eq!(
            spec.policy_for("extensions/karyll/hid/dist/libc.so.6")
                .class,
            FileClass::Seed
        );
        assert_eq!(
            spec.policy_for("extensions/karyll/hid/config.ini").class,
            FileClass::Seed
        );
        // A sibling whose name merely starts with the same letters is not in it.
        assert_eq!(
            spec.policy_for("extensions/karyll/hidden/x").class,
            FileClass::Sync
        );
        // And the prefix does not reach outside the app.
        assert_eq!(
            spec.policy_for("documents/Karyll.sh").class,
            FileClass::Sync
        );
    }

    #[test]
    fn first_matching_rule_wins_whole() {
        let spec = AppSpec::parse(
            br#"{
              "schema": 1, "id": "karyll", "name": "Karyll", "version": "0.4.0",
              "paths": [
                { "match": "extensions/karyll/hid/config.ini", "class": "seed", "seed_gen": 3 },
                { "match": "extensions/karyll/hid/", "class": "seed", "seed_gen": 1 }
              ]
            }"#,
        )
        .unwrap();
        assert_eq!(
            spec.policy_for("extensions/karyll/hid/config.ini").seed_gen,
            3
        );
        assert_eq!(spec.policy_for("extensions/karyll/hid/LICENSE").seed_gen, 1);
    }

    #[test]
    fn seed_without_a_generation_is_generation_one() {
        let spec = AppSpec::parse(
            br#"{"schema":1,"id":"a","name":"A","version":"1",
                 "paths":[{"match":"extensions/a/x","class":"seed"}]}"#,
        )
        .unwrap();
        assert_eq!(spec.policy_for("extensions/a/x").seed_gen, 1);
    }

    #[test]
    fn staged_is_opt_in_per_path() {
        let spec = AppSpec::parse(
            br#"{"schema":1,"id":"sidle","name":"Sidle","version":"0.1.9",
                 "paths":[{"match":"extensions/sidle/bin/sidle","apply":"staged"}]}"#,
        )
        .unwrap();
        assert_eq!(
            spec.policy_for("extensions/sidle/bin/sidle").apply,
            Apply::Staged
        );
        assert_eq!(
            spec.policy_for("extensions/sidle/config.xml").apply,
            Apply::Direct
        );
    }

    #[test]
    fn a_newer_schema_is_refused_not_guessed_at() {
        let err = AppSpec::parse(br#"{"schema":2,"id":"a","name":"A","version":"1"}"#).unwrap_err();
        assert!(format!("{err:#}").contains("schema 2"));
    }

    /// `match` keys the served route and resolves the on-device destination, so
    /// a traversal here would write outside the mount.
    #[test]
    fn traversal_and_absolute_paths_are_refused() {
        for bad in [
            r#"{"match":"../etc/passwd"}"#,
            r#"{"match":"/etc/passwd"}"#,
            r#"{"match":"extensions/a/../../x"}"#,
            r#"{"match":"extensions\\a\\x"}"#,
            r#"{"match":"extensions//a"}"#,
        ] {
            let json =
                format!(r#"{{"schema":1,"id":"a","name":"A","version":"1","paths":[{bad}]}}"#);
            assert!(AppSpec::parse(json.as_bytes()).is_err(), "accepted {bad}");
        }
    }

    /// The repo mirror holds `etc/server.conf.example`, and what lands at
    /// `etc/server.conf` is rendered per device with a real bearer token.
    /// Installing the example would look like a conf and authenticate nothing.
    #[test]
    fn ignore_keeps_a_mirrored_file_out_of_the_install() {
        let spec = AppSpec::parse(
            br#"{"schema":1,"id":"sidle","name":"Sidle","version":"0.1.9",
                 "paths":[{"match":"extensions/sidle/etc/server.conf.example",
                           "class":"ignore"}]}"#,
        )
        .unwrap();
        assert_eq!(
            spec.policy_for("extensions/sidle/etc/server.conf.example")
                .class,
            FileClass::Ignore
        );
        assert_eq!(
            spec.policy_for("extensions/sidle/config.xml").class,
            FileClass::Sync
        );
    }

    #[test]
    fn seed_gen_on_a_sync_rule_is_an_error_not_a_no_op() {
        let err = AppSpec::parse(
            br#"{"schema":1,"id":"a","name":"A","version":"1",
                 "paths":[{"match":"extensions/a/x","seed_gen":2}]}"#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("Sync rule"));
    }

    #[test]
    fn a_tile_outside_documents_is_never_indexed_so_it_is_refused() {
        let err = AppSpec::parse(
            br#"{"schema":1,"id":"a","name":"A","version":"1","tile":"extensions/a/A.sh"}"#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("documents/"));
    }

    #[test]
    fn id_must_be_a_usable_directory_name() {
        for bad in ["Karyll", "kar yll", "kar/yll", ".hidden", ""] {
            let json = format!(r#"{{"schema":1,"id":"{bad}","name":"A","version":"1"}}"#);
            assert!(
                AppSpec::parse(json.as_bytes()).is_err(),
                "accepted id {bad:?}"
            );
        }
    }

    #[test]
    fn load_rejects_an_id_that_disagrees_with_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("extensions").join("steb");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(APP_SPEC_FILE);
        std::fs::write(
            &path,
            br#"{"schema":1,"id":"karyll","name":"Karyll","version":"0.4.0"}"#,
        )
        .unwrap();
        let err = AppSpec::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("extensions/steb"));
    }

    #[test]
    fn round_trips_through_json() {
        let spec = karyll();
        let bytes = serde_json::to_vec(&spec).unwrap();
        assert_eq!(AppSpec::parse(&bytes).unwrap(), spec);
    }
}
