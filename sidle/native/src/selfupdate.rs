//! LAN self-update — pull the fleet's current bytes from sidle-server.
//!
//! `sidle-server` serves `<data-dir>/device-dist/` over `/device/...`. This is
//! the device-side client: fetch the manifest, settle each file against the
//! receipt, download what differs, verify its sha256, write it.
//!
//! A write is direct, or `<path>.new` for the two files sidle is executing. A
//! manifest `path` is mount-relative, reaching `documents/` and every other
//! app's directory.
//!
//! Triggered by the in-app **Update** button in the picker's search bar (inline
//! in `main::run`), or by the `--update` recovery launch (`main::run_update`).
//! The HTTP plumbing reuses `api::get_with_token`; the decide/verify/write
//! logic here is pure `std`, host-testable in the `sidle_native` lib.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::api::Result;
use crate::api::{JSON_MAX_BYTES, read_text};
use crate::config::ServerConfig;
use crate::receipt::{FileReceipt, InstallState};

/// The app id whose files this binary is itself executing.
pub const PICKER_ID: &str = "sidle";

/// How a write lands. Mirrors the desktop's `apps::policy::Apply`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Apply {
    /// Write `path` itself.
    #[default]
    Direct,
    /// Write `<path>.new`, for the process one level up to swap in.
    Staged,
}

/// One file the manifest declares. `path` is **mount-relative** — the
/// `/device/file/<path>` route AND the destination under `/mnt/us`. `sha256` is
/// hex, from the desktop's `hex::encode`.
#[derive(Debug, Clone, Deserialize)]
pub struct DistFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
    #[serde(default)]
    pub apply: Apply,
}

/// One app's slice of the served tree.
#[derive(Debug, Clone, Deserialize)]
pub struct DistApp {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    /// Unix seconds the app was built. Guards this binary against a stale
    /// `device-dist` (see [`decide`]).
    #[serde(default)]
    pub built_at: u64,
    pub files: Vec<DistFile>,
}

/// `device-dist/manifest.json`. Mirrors the desktop's `dist::DistManifest`;
/// serde drops the keys this build does not read.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DistManifest {
    #[serde(default)]
    pub apps: Vec<DistApp>,
}

impl DistManifest {
    pub fn file_count(&self) -> usize {
        self.apps.iter().map(|a| a.files.len()).sum()
    }
}

/// What a pull did, for the result toast.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateReport {
    /// Paths written where they install.
    pub written: Vec<String>,
    /// Paths written as `<path>.new`, pending a relaunch.
    pub staged: Vec<String>,
    /// Paths the device changed out from under sidle, left as they are.
    pub kept: Vec<String>,
    /// Paths whose offered build is not newer than this binary's.
    pub refused: Vec<String>,
}

impl UpdateReport {
    /// Whether the pull found nothing to do at all.
    pub fn quiet(&self) -> bool {
        self.written.is_empty()
            && self.staged.is_empty()
            && self.kept.is_empty()
            && self.refused.is_empty()
    }
}

/// This binary's build time (unix seconds), baked by `build.rs` from
/// `build.sh`'s `SIDLE_BUILD_TS`. `0` when built without the stamp, which
/// leaves [`decide`] on the sha-only rule for this binary.
pub fn self_build_ts() -> u64 {
    option_env!("SIDLE_BUILD_TS")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Tiny JSON — keep it snappy like the boot list fetch.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(5);
/// The largest file in the fleet is a few MB. A generous LAN window, for a
/// sleepy radio.
const FILE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// Bound a runaway response. 64 MB is wildly over any file in the fleet
/// (mirrors the KFX cap rationale in `api.rs`).
const FILE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// `GET /device/manifest.json`. 401/403 →
/// [`crate::api::SidleError::TokenMismatch`], via `get_with_token`.
pub fn fetch_manifest(agent: &ureq::Agent, cfg: &ServerConfig) -> Result<DistManifest> {
    let url = format!("https://{}:{}/device/manifest.json", cfg.host, cfg.port);
    let mut res = crate::api::get_with_token(agent, &url, &cfg.token, MANIFEST_TIMEOUT)?;
    let body =
        read_text(&mut res, JSON_MAX_BYTES).with_context(|| format!("read body of {url}"))?;
    let manifest: DistManifest =
        serde_json::from_str(&body).with_context(|| format!("parse {url}"))?;
    Ok(manifest)
}

/// `GET /device/file/<path>` → the file's bytes (capped). `path` carries
/// literal `/` and goes through unencoded, for the server's catch-all `{*name}`
/// to reassemble. Manifest paths are ASCII path-safe.
pub fn download_file(agent: &ureq::Agent, cfg: &ServerConfig, path: &str) -> Result<Vec<u8>> {
    let url = format!("https://{}:{}/device/file/{}", cfg.host, cfg.port, path);
    let res = crate::api::get_with_token(agent, &url, &cfg.token, FILE_DOWNLOAD_TIMEOUT)?;
    let mut bytes = Vec::new();
    res.into_body()
        .into_reader()
        .take(FILE_MAX_BYTES as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read body of {url}"))?;
    Ok(bytes)
}

/// First 8 hex chars (or fewer) of a hash, for readable logs — never the full
/// digest.
fn short(hex: &str) -> &str {
    &hex[..hex.len().min(8)]
}

/// Hex sha256 of `bytes`. Same recipe as the desktop's `deploy::sha256_bytes`
/// (`Sha256` + `hex::encode`): a device-computed hash equals the manifest's.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// What [`run_pull`] should do with one manifest file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The device holds these bytes.
    UpToDate,
    /// Download and write.
    Write,
    /// The offered build is not strictly newer than this binary's.
    RefuseOlder,
    /// The device copy is neither the offered bytes nor what sidle wrote.
    KeepDeviceCopy,
}

/// Whether the receipt alone settles `file`: the served bytes are the ones
/// sidle recorded writing here, and the device is not read.
pub fn settled_by_receipt(file: &DistFile, receipt: Option<&FileReceipt>) -> bool {
    receipt.is_some_and(|r| r.sha256 == file.sha256 && r.size == file.size)
}

/// What to do with `file`, given the bytes on the device and what sidle
/// recorded writing there.
///
/// A device copy matching neither is kept: it carries an edit made here. With
/// no receipt entry sidle has never written this path, and this update starts
/// the record.
///
/// `self_build_ts` non-zero applies the downgrade guard. Callers pass it only
/// for [`PICKER_ID`], the one app whose files this binary is executing.
pub fn decide(
    on_device: Option<&[u8]>,
    file: &DistFile,
    receipt: Option<&FileReceipt>,
    built_at: u64,
    self_build_ts: u64,
) -> Decision {
    let Some(bytes) = on_device else {
        return Decision::Write;
    };
    let device = sha256_hex(bytes);
    if device == file.sha256 {
        return Decision::UpToDate;
    }
    let sidle_wrote_it = match receipt {
        Some(r) => device == r.sha256,
        None => true,
    };
    if !sidle_wrote_it {
        return Decision::KeepDeviceCopy;
    }
    if self_build_ts != 0 && built_at != 0 && built_at <= self_build_ts {
        return Decision::RefuseOlder;
    }
    Decision::Write
}

/// Whether a freshly downloaded blob matches the manifest — **both** size and
/// sha256. The gate before anything lands: the launcher swaps `.new` in
/// unconditionally, and a truncated download must never become the next binary.
pub fn verify_download(bytes: &[u8], file: &DistFile) -> bool {
    bytes.len() as u64 == file.size && sha256_hex(bytes) == file.sha256
}

/// Append `.<suffix>` to a path's filename (`bin/sidle` + `new` →
/// `bin/sidle.new`), keeping whatever extension the file carries. Mirrors the
/// desktop `deploy::with_suffix`.
pub fn with_dot_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

/// Write verified `bytes` for `file` under `mount`, and return where they
/// landed.
///
/// [`Apply::Direct`] lands on the path itself; [`Apply::Staged`] lands on
/// `<path>.new`. Both pass through `<path>.download` and a `rename(2)` inside
/// the one `/mnt/us` mount.
pub fn write_file(mount: &Path, file: &DistFile, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    let dest = mount.join(&file.path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let landing = match file.apply {
        Apply::Direct => dest.clone(),
        Apply::Staged => with_dot_suffix(&dest, "new"),
    };
    let download = with_dot_suffix(&dest, "download");
    std::fs::write(&download, bytes).with_context(|| format!("write {}", download.display()))?;
    std::fs::rename(&download, &landing)
        .with_context(|| format!("rename {} -> {}", download.display(), landing.display()))?;
    Ok(landing)
}

/// Fetch the manifest and bring `mount` up to it.
///
/// Per file: settle against the receipt → read the device copy → [`decide`] →
/// download → verify → write. `mount` is `/mnt/us`, a verify failure aborts the
/// pull, and `log` takes step breadcrumbs for `sidle-update.log`.
pub fn run_pull(
    agent: &ureq::Agent,
    cfg: &ServerConfig,
    mount: &Path,
    self_build_ts: u64,
    log: impl Fn(&str),
) -> Result<UpdateReport> {
    let manifest = fetch_manifest(agent, cfg)?;
    log(&format!(
        "manifest: {} app(s), {} file(s)",
        manifest.apps.len(),
        manifest.file_count()
    ));
    let mut state = InstallState::read(mount);
    let mut report = UpdateReport::default();
    let mut dirty = false;

    for app in &manifest.apps {
        // The guard protects the binary this process is running from a stale
        // `device-dist`; no other app has a file open here.
        let guard = if app.id == PICKER_ID {
            self_build_ts
        } else {
            0
        };
        let mut wrote_any = false;
        log(&format!(
            "{} ({}): {} file(s)",
            app.name,
            app.id,
            app.files.len()
        ));

        for file in &app.files {
            let dest = mount.join(&file.path);
            if settled_by_receipt(file, state.file(&app.id, &file.path)) {
                continue;
            }
            if file.apply == Apply::Staged && staged_matches(&dest, file) {
                log(&format!("{}: already staged", file.path));
                report.staged.push(file.path.clone());
                continue;
            }
            let on_device = std::fs::read(&dest).ok();
            log(&format!(
                "{}: device={} manifest={} ({} bytes, built_at={} vs self={})",
                file.path,
                on_device
                    .as_deref()
                    .map(sha256_hex)
                    .as_deref()
                    .map(short)
                    .unwrap_or("absent"),
                short(&file.sha256),
                file.size,
                app.built_at,
                guard,
            ));
            match decide(
                on_device.as_deref(),
                file,
                state.file(&app.id, &file.path),
                app.built_at,
                guard,
            ) {
                Decision::UpToDate => {
                    // `dest` holds these bytes. `state.record` settles this
                    // path by receipt on the next pull, unread.
                    state.record(&app.id, &file.path, receipt_for(file));
                    dirty = true;
                    continue;
                }
                Decision::RefuseOlder => {
                    log(&format!(
                        "{}: build {} not newer than installed {} — refusing downgrade",
                        file.path, app.built_at, guard,
                    ));
                    report.refused.push(file.path.clone());
                    continue;
                }
                Decision::KeepDeviceCopy => {
                    log(&format!(
                        "{}: changed on this device — keeping it",
                        file.path
                    ));
                    report.kept.push(file.path.clone());
                    continue;
                }
                Decision::Write => {}
            }
            log(&format!("{}: downloading…", file.path));
            let bytes = download_file(agent, cfg, &file.path)?;
            if !verify_download(&bytes, file) {
                log(&format!(
                    "{}: VERIFY FAILED — got {} bytes sha={} — not writing",
                    file.path,
                    bytes.len(),
                    short(&sha256_hex(&bytes)),
                ));
                return Err(anyhow!(
                    "downloaded {} failed its sha256/size check ({} bytes vs {} expected) \
                     — not writing",
                    file.path,
                    bytes.len(),
                    file.size,
                )
                .into());
            }
            let landed = write_file(mount, file, &bytes)?;
            log(&format!(
                "{}: verified + wrote {}",
                file.path,
                landed.display()
            ));
            state.record(&app.id, &file.path, receipt_for(file));
            dirty = true;
            wrote_any = true;
            match file.apply {
                Apply::Direct => report.written.push(file.path.clone()),
                Apply::Staged => report.staged.push(file.path.clone()),
            }
        }

        if wrote_any {
            state.describe(&app.id, app.version.clone(), app.built_at);
        }
    }

    if dirty && let Err(e) = state.write(mount) {
        log(&format!("receipt: write failed — {e}"));
    }
    Ok(report)
}

/// What the receipt records for `file`: its `sha256` and `size`.
fn receipt_for(file: &DistFile) -> FileReceipt {
    FileReceipt {
        sha256: file.sha256.clone(),
        size: file.size,
    }
}

/// Whether `<dest>.new` holds the offered bytes.
fn staged_matches(dest: &Path, file: &DistFile) -> bool {
    std::fs::read(with_dot_suffix(dest, "new"))
        .map(|b| sha256_hex(&b) == file.sha256)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_for(bytes: &[u8]) -> DistFile {
        DistFile {
            path: "extensions/sidle/bin/sidle".into(),
            sha256: sha256_hex(bytes),
            size: bytes.len() as u64,
            apply: Apply::Staged,
        }
    }

    fn receipt_for(bytes: &[u8]) -> FileReceipt {
        FileReceipt {
            sha256: sha256_hex(bytes),
            size: bytes.len() as u64,
        }
    }

    /// A unique scratch dir under the system temp (the crate avoids a `tempfile`
    /// dev-dep — see `api.rs`'s tests).
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sidle-selfupd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // sha256("") — the vector `deploy::sha256_bytes`'s own test asserts.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_receipt_naming_the_offered_bytes_settles_a_file_unread() {
        let v2 = b"binary-v2";
        let file = file_for(v2);
        assert!(settled_by_receipt(&file, Some(&receipt_for(v2))));
        assert!(!settled_by_receipt(&file, Some(&receipt_for(b"binary-v1"))));
        assert!(!settled_by_receipt(&file, None));
    }

    #[test]
    fn decide_covers_missing_changed_and_equal() {
        let v2 = b"binary-v2";
        let file = file_for(v2);
        assert_eq!(decide(None, &file, None, 0, 0), Decision::Write);
        assert_eq!(
            decide(Some(b"binary-v1"), &file, None, 0, 0),
            Decision::Write
        );
        assert_eq!(decide(Some(v2), &file, None, 0, 0), Decision::UpToDate);
    }

    #[test]
    fn a_file_the_device_changed_is_kept() {
        let offered = b"karyll-v2";
        let file = DistFile {
            path: "extensions/karyll/hid/config.ini".into(),
            sha256: sha256_hex(offered),
            size: offered.len() as u64,
            apply: Apply::Direct,
        };
        // The receipt names what sidle wrote; the device holds something else.
        let written = receipt_for(b"karyll-v1");
        assert_eq!(
            decide(Some(b"edited-on-device"), &file, Some(&written), 0, 0),
            Decision::KeepDeviceCopy,
        );
        // The bytes sidle wrote → sidle's to replace.
        assert_eq!(
            decide(Some(b"karyll-v1"), &file, Some(&written), 0, 0),
            Decision::Write,
        );
    }

    #[test]
    fn the_guard_refuses_an_older_or_equal_build() {
        let installed = b"running-v2";
        let newer = file_for(b"newer-v3");
        let older = file_for(b"older-v1");
        let written = receipt_for(installed);

        assert_eq!(
            decide(Some(installed), &newer, Some(&written), 200, 100),
            Decision::Write,
        );
        assert_eq!(
            decide(Some(installed), &older, Some(&written), 50, 100),
            Decision::RefuseOlder,
        );
        // Equal build, different bytes → no forward progress.
        assert_eq!(
            decide(Some(installed), &older, Some(&written), 100, 100),
            Decision::RefuseOlder,
        );
        // Another app's files carry no guard: the caller passes 0.
        assert_eq!(
            decide(Some(installed), &older, Some(&written), 50, 0),
            Decision::Write,
        );
    }

    #[test]
    fn verify_download_requires_both_size_and_hash() {
        let v2 = b"new-binary-v2";
        let file = file_for(v2);
        assert!(verify_download(v2, &file), "exact bytes pass");
        assert!(!verify_download(b"short", &file), "wrong size fails");
        // Same length, one bit flipped → hash mismatch must fail.
        let mut wrong = v2.to_vec();
        let last = wrong.len() - 1;
        wrong[last] ^= 0xFF;
        assert!(
            !verify_download(&wrong, &file),
            "same size + wrong hash fails"
        );
    }

    #[test]
    fn with_dot_suffix_appends_rather_than_replaces() {
        assert_eq!(
            with_dot_suffix(Path::new("/x/bin/sidle"), "new"),
            PathBuf::from("/x/bin/sidle.new")
        );
        assert_eq!(
            with_dot_suffix(Path::new("/x/bin/sidle"), "download"),
            PathBuf::from("/x/bin/sidle.download")
        );
    }

    #[test]
    fn a_staged_write_leaves_the_running_binary_alone() {
        let mount = scratch("staged");
        std::fs::create_dir_all(mount.join("extensions/sidle/bin")).unwrap();
        std::fs::write(mount.join("extensions/sidle/bin/sidle"), b"running-v1").unwrap();

        let file = file_for(b"staged-v2");
        let landed = write_file(&mount, &file, b"staged-v2").unwrap();

        assert_eq!(landed, mount.join("extensions/sidle/bin/sidle.new"));
        assert_eq!(std::fs::read(&landed).unwrap(), b"staged-v2");
        assert_eq!(
            std::fs::read(mount.join("extensions/sidle/bin/sidle")).unwrap(),
            b"running-v1",
            "the launcher swaps it later"
        );
        assert!(!mount.join("extensions/sidle/bin/sidle.download").exists());
        assert!(staged_matches(
            &mount.join("extensions/sidle/bin/sidle"),
            &file
        ));
        let _ = std::fs::remove_dir_all(&mount);
    }

    #[test]
    fn a_direct_write_lands_on_the_path_itself() {
        let mount = scratch("direct");
        let file = DistFile {
            path: "documents/Karyll.sh".into(),
            sha256: sha256_hex(b"# Name: Karyll\n"),
            size: 15,
            apply: Apply::Direct,
        };

        let landed = write_file(&mount, &file, b"# Name: Karyll\n").unwrap();

        assert_eq!(landed, mount.join("documents/Karyll.sh"));
        assert_eq!(std::fs::read(&landed).unwrap(), b"# Name: Karyll\n");
        assert!(!mount.join("documents/Karyll.sh.new").exists());
        let _ = std::fs::remove_dir_all(&mount);
    }

    #[test]
    fn a_manifest_parses_into_apps_and_their_files() {
        let json = r#"{
          "version": 2,
          "files": [ { "name": "bin/sidle", "sha256": "aa", "size": 3, "built_at": 7 } ],
          "apps": [
            { "id": "sidle", "name": "Sidle", "version": "0.1.9", "built_at": 7, "files": [
              { "path": "extensions/sidle/bin/sidle", "sha256": "aa", "size": 3, "apply": "staged" },
              { "path": "documents/Sidle.sh", "sha256": "bb", "size": 4, "apply": "direct" } ] },
            { "id": "karyll", "name": "Karyll", "built_at": 9, "files": [
              { "path": "extensions/karyll/bin/karyll", "sha256": "cc", "size": 5 } ] }
          ] }"#;
        let manifest: DistManifest = serde_json::from_str(json).unwrap();

        assert_eq!(manifest.apps.len(), 2);
        assert_eq!(manifest.file_count(), 3);
        assert_eq!(manifest.apps[0].version.as_deref(), Some("0.1.9"));
        assert_eq!(manifest.apps[0].files[0].apply, Apply::Staged);
        assert_eq!(manifest.apps[0].files[1].apply, Apply::Direct);
        assert_eq!(manifest.apps[1].version, None);
        assert_eq!(
            manifest.apps[1].files[0].apply,
            Apply::Direct,
            "an omitted apply is a plain write"
        );
    }
}
