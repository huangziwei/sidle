//! LAN self-update — pull the picker's own next binary from sidle-server.
//!
//! The desktop app stages the freshly cross-built `bin/sidle` into
//! `<data-dir>/kual-dist/` and the server serves it over `/kual/...`. This
//! module is the device-side client: fetch the manifest, skip any file whose
//! on-device copy already matches, download the rest, **sha256-verify each
//! download before staging it** as `<name>.new`, and let the launcher
//! (`bin/sidle.sh`) swap `.new` over the running binary on the next start —
//! the one moment nothing maps it (FAT can't overwrite a running binary).
//!
//! Triggered by the in-app **Update** button in the picker's search bar (inline
//! in `main::run`), or by the `--update` recovery launch (`main::run_update`)
//! for when the gallery won't boot. The HTTP plumbing reuses `api::get_with_token`;
//! the compare/verify/stage logic here is pure `std` so it host-tests in the
//! `sidle_native` lib (the framebuffer toast lives in `main.rs`).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::api::Result;
use crate::config::ServerConfig;

/// One file in the server's `kual-dist/manifest.json`. Mirrors the desktop's
/// `KualManifestEntry` (serde drops any extra fields). `name` is the
/// device-relative path (e.g. `bin/sidle`) — it's the `/kual/file/<name>` route
/// AND the destination under the on-device bundle dir. `sha256` is hex, matching
/// the desktop's `hex::encode`, so an on-device hash compares equal byte-for-byte.
#[derive(Debug, Clone, Deserialize)]
pub struct KualManifestEntry {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    /// Unix seconds the served file was built (`build.sh`'s `BUILD_TS`, also
    /// baked into the picker binary). `0` when the manifest predates build
    /// stamping. The device stages this only if `built_at` is strictly newer
    /// than its own build time — the guard that stops a stale `kual-dist` from
    /// downgrading the picker over Wi-Fi (see [`should_stage`]).
    #[serde(default)]
    pub built_at: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KualManifest {
    pub files: Vec<KualManifestEntry>,
}

/// What a pull did, for the result toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Every manifest file already matches on device — nothing downloaded.
    UpToDate,
    /// Staged one or more `<name>.new` files (the names), pending a relaunch.
    Staged(Vec<String>),
    /// The manifest offered differing bytes, but none were strictly newer than
    /// what's installed — refused so a stale `kual-dist` can't downgrade us. The
    /// device keeps its current (newer-or-equal) binary. Carries the file names.
    RefusedOlder(Vec<String>),
}

/// This binary's build time (unix seconds), baked by `build.rs` from
/// `build.sh`'s `SIDLE_BUILD_TS`. `0` when built without the stamp (a bare
/// `cargo build`), which disables the downgrade guard for this binary — it then
/// updates on any sha difference, the pre-guard behavior.
pub fn self_build_ts() -> u64 {
    option_env!("SIDLE_BUILD_TS")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Tiny JSON — keep it snappy like the boot list fetch.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(5);
/// The binary is ~1.8 MB; allow a generous LAN window for a sleepy radio.
const BINARY_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// Bound a runaway response. 64 MB is wildly over the real ~1.8 MB (mirrors the
/// KFX cap rationale in `api.rs`).
const BINARY_MAX_BYTES: usize = 64 * 1024 * 1024;

/// `GET /kual/manifest.json`. 401/403 → [`crate::api::SidleError::TokenMismatch`]
/// (via `get_with_token`), so the caller shows the "plug into sidle" toast.
pub fn fetch_manifest(agent: &ureq::Agent, cfg: &ServerConfig) -> Result<KualManifest> {
    let url = format!("http://{}:{}/kual/manifest.json", cfg.host, cfg.port);
    let res = crate::api::get_with_token(agent, &url, &cfg.token, MANIFEST_TIMEOUT)?;
    let body = res
        .into_string()
        .with_context(|| format!("read body of {url}"))?;
    let manifest: KualManifest =
        serde_json::from_str(&body).with_context(|| format!("parse {url}"))?;
    Ok(manifest)
}

/// `GET /kual/file/<name>` → the file's bytes (capped). `name` carries literal
/// `/` (e.g. `bin/sidle`) and is passed through unencoded so the server's
/// catch-all `{*name}` reassembles the path — percent-encoding the slash would
/// break the route. v1 manifest names are ASCII path-safe.
pub fn download_file(agent: &ureq::Agent, cfg: &ServerConfig, name: &str) -> Result<Vec<u8>> {
    let url = format!("http://{}:{}/kual/file/{}", cfg.host, cfg.port, name);
    let res = crate::api::get_with_token(agent, &url, &cfg.token, BINARY_DOWNLOAD_TIMEOUT)?;
    let mut bytes = Vec::new();
    res.into_reader()
        .take(BINARY_MAX_BYTES as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read body of {url}"))?;
    Ok(bytes)
}

/// First 8 hex chars (or fewer) of a hash, for readable logs — never the full
/// digest.
fn short(hex: &str) -> &str {
    &hex[..hex.len().min(8)]
}

/// Hex sha256 of `bytes`. Same recipe as the desktop's `kual::sha256_bytes`
/// (`Sha256` + `hex::encode`), so a device-computed hash equals the manifest's
/// — the equality the "LAN deploy == USB deploy" gate hinges on.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Whether the on-device bytes must be replaced to match `entry`: missing →
/// yes; present but hash-mismatched → yes; hash-equal → no (skip the download).
pub fn needs_update(on_device: Option<&[u8]>, entry: &KualManifestEntry) -> bool {
    match on_device {
        None => true,
        Some(b) => sha256_hex(b) != entry.sha256,
    }
}

/// What [`run_pull`] should do with one manifest entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageDecision {
    /// On-device bytes already match the manifest — skip.
    UpToDate,
    /// Download, verify, and stage as `<name>.new`.
    Stage,
    /// Bytes differ, but the manifest's build is not strictly newer than ours —
    /// refuse, so a stale `kual-dist` can't downgrade the device.
    RefuseOlder,
}

/// Decide whether to replace the on-device copy, given this binary's build time.
/// Identical bytes → never. When **both** the entry and this binary carry a
/// build stamp (`built_at`/`self_build_ts` non-zero), stage only if the entry is
/// **strictly newer**; an older-or-equal stamp is refused — the downgrade guard.
/// If either side is unstamped (`0` — a pre-guard manifest or a bare `cargo
/// build`), fall back to the sha-only rule so updates still flow.
pub fn should_stage(
    on_device: Option<&[u8]>,
    entry: &KualManifestEntry,
    self_build_ts: u64,
) -> StageDecision {
    if !needs_update(on_device, entry) {
        return StageDecision::UpToDate;
    }
    // The guard only protects an *existing* on-device copy from being replaced by
    // an older-or-equal build. A missing file has nothing to downgrade, so it's
    // always installed (matches `needs_update`'s "missing → yes").
    if on_device.is_some()
        && entry.built_at != 0
        && self_build_ts != 0
        && entry.built_at <= self_build_ts
    {
        return StageDecision::RefuseOlder;
    }
    StageDecision::Stage
}

/// Whether a freshly downloaded blob matches the manifest entry — **both** size
/// and sha256. The gate before staging: the launcher swaps `.new` in
/// unconditionally, so a truncated or corrupt download must never become the
/// next binary.
pub fn verify_download(bytes: &[u8], entry: &KualManifestEntry) -> bool {
    bytes.len() as u64 == entry.size && sha256_hex(bytes) == entry.sha256
}

/// Append `.<suffix>` to a path's filename (`bin/sidle` + `new` →
/// `bin/sidle.new`). Appends rather than replacing any extension so the
/// launcher's exact `bin/sidle.new` lookup matches regardless of the file's own
/// extension. Mirrors the desktop `kual::with_suffix`.
pub fn with_dot_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

/// Stage already-downloaded, already-verified `bytes` as `<bundle>/<name>.new`
/// for the launcher to swap in on the next start. Writes a fixed-name temp
/// (`<name>.download`, so a picker killed mid-pull overwrites it next time
/// instead of littering) then renames it to `<name>.new` — both on the one
/// `/mnt/us` FAT mount, so `rename(2)` stays in-filesystem. Never touches the
/// running `<name>`. Returns the staged path.
pub fn stage_update(bundle: &Path, name: &str, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    let dest = bundle.join(name);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let download = with_dot_suffix(&dest, "download");
    let staged = with_dot_suffix(&dest, "new");
    std::fs::write(&download, bytes)
        .with_context(|| format!("write {}", download.display()))?;
    std::fs::rename(&download, &staged)
        .with_context(|| format!("rename {} -> {}", download.display(), staged.display()))?;
    Ok(staged)
}

/// Fetch the manifest and stage every file whose on-device copy differs. For
/// each: compare (skip if already current) → download → sha256/size-verify →
/// atomic-stage as `<name>.new`. `bundle` is the on-device extension dir
/// (`/mnt/us/extensions/sidle`). A verify failure aborts that file (and the
/// pull) rather than staging a bad binary. `log` receives step breadcrumbs
/// (manifest count, per-file device-vs-manifest hash, download/verify/stage) —
/// the picker routes them to the dedicated `sidle-update.log`.
pub fn run_pull(
    agent: &ureq::Agent,
    cfg: &ServerConfig,
    bundle: &Path,
    self_build_ts: u64,
    log: impl Fn(&str),
) -> Result<UpdateOutcome> {
    let manifest = fetch_manifest(agent, cfg)?;
    log(&format!("manifest: {} file(s)", manifest.files.len()));
    let mut staged = Vec::new();
    let mut refused = Vec::new();
    for entry in &manifest.files {
        let dest = bundle.join(&entry.name);
        let on_device = std::fs::read(&dest).ok();
        let have = on_device.as_deref().map(sha256_hex);
        log(&format!(
            "{}: device={} manifest={} ({} bytes, built_at={} vs self={})",
            entry.name,
            have.as_deref().map(short).unwrap_or("absent"),
            short(&entry.sha256),
            entry.size,
            entry.built_at,
            self_build_ts,
        ));
        match should_stage(on_device.as_deref(), entry, self_build_ts) {
            StageDecision::UpToDate => {
                log(&format!("{}: already current — skip", entry.name));
                continue;
            }
            StageDecision::RefuseOlder => {
                log(&format!(
                    "{}: manifest build {} not newer than installed {} — refusing downgrade",
                    entry.name, entry.built_at, self_build_ts,
                ));
                refused.push(entry.name.clone());
                continue;
            }
            StageDecision::Stage => {}
        }
        log(&format!("{}: downloading…", entry.name));
        let bytes = download_file(agent, cfg, &entry.name)?;
        if !verify_download(&bytes, entry) {
            log(&format!(
                "{}: VERIFY FAILED — got {} bytes sha={} — not staging",
                entry.name,
                bytes.len(),
                short(&sha256_hex(&bytes)),
            ));
            return Err(anyhow!(
                "downloaded {} failed its sha256/size check ({} bytes vs {} expected) \
                 — not staging",
                entry.name,
                bytes.len(),
                entry.size,
            )
            .into());
        }
        let staged_path = stage_update(bundle, &entry.name, &bytes)?;
        log(&format!("{}: verified + staged {}", entry.name, staged_path.display()));
        staged.push(entry.name.clone());
    }
    Ok(if !staged.is_empty() {
        UpdateOutcome::Staged(staged)
    } else if !refused.is_empty() {
        UpdateOutcome::RefusedOlder(refused)
    } else {
        UpdateOutcome::UpToDate
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_for(bytes: &[u8]) -> KualManifestEntry {
        entry_for_ts(bytes, 0)
    }

    fn entry_for_ts(bytes: &[u8], built_at: u64) -> KualManifestEntry {
        KualManifestEntry {
            name: "bin/sidle".into(),
            sha256: sha256_hex(bytes),
            size: bytes.len() as u64,
            built_at,
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
        // sha256("") — the SAME vector `kual::sha256_bytes`'s test asserts, so the
        // device and desktop hashers are provably identical (the LAN==USB gate).
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn needs_update_covers_missing_changed_and_equal() {
        let v2 = b"new-binary-v2";
        let e = entry_for(v2);
        assert!(needs_update(None, &e), "missing on device → needs update");
        assert!(needs_update(Some(b"old-binary-v1"), &e), "differs → needs update");
        assert!(!needs_update(Some(v2), &e), "identical → up to date");
    }

    #[test]
    fn should_stage_refuses_older_or_equal_build() {
        let installed = b"running-v2";
        let newer = b"newer-v3";
        let older = b"older-v1";

        // Strictly newer build with different bytes → stage.
        assert_eq!(
            should_stage(Some(installed), &entry_for_ts(newer, 200), 100),
            StageDecision::Stage,
        );
        // Older build → refuse (the bug we're guarding: stale kual-dist).
        assert_eq!(
            should_stage(Some(installed), &entry_for_ts(older, 50), 100),
            StageDecision::RefuseOlder,
        );
        // Equal build but different bytes → refuse (no forward progress).
        assert_eq!(
            should_stage(Some(installed), &entry_for_ts(older, 100), 100),
            StageDecision::RefuseOlder,
        );
        // Identical bytes always win, even if the stamp looks newer.
        assert_eq!(
            should_stage(Some(installed), &entry_for_ts(installed, 999), 100),
            StageDecision::UpToDate,
        );
    }

    #[test]
    fn should_stage_falls_back_to_sha_when_unstamped() {
        let installed = b"running-v2";
        let other = b"other-v9";
        // Manifest predates stamping (built_at=0) → sha-only: differing bytes stage.
        assert_eq!(
            should_stage(Some(installed), &entry_for_ts(other, 0), 100),
            StageDecision::Stage,
        );
        // This binary is unstamped (self=0) → sha-only even against a stamped entry.
        assert_eq!(
            should_stage(Some(installed), &entry_for_ts(other, 50), 0),
            StageDecision::Stage,
        );
        // Missing on device → always stage regardless of stamps.
        assert_eq!(
            should_stage(None, &entry_for_ts(other, 10), 100),
            StageDecision::Stage,
        );
    }

    #[test]
    fn verify_download_requires_both_size_and_hash() {
        let v2 = b"new-binary-v2";
        let e = entry_for(v2);
        assert!(verify_download(v2, &e), "exact bytes pass");
        assert!(!verify_download(b"short", &e), "wrong size fails");
        // Same length, one bit flipped → hash mismatch must fail.
        let mut wrong = v2.to_vec();
        let last = wrong.len() - 1;
        wrong[last] ^= 0xFF;
        assert!(!verify_download(&wrong, &e), "same size + wrong hash fails");
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
    fn stage_update_writes_new_and_leaves_running_binary_alone() {
        let base = scratch("stage");
        let bundle = base.join("extensions/sidle");
        std::fs::create_dir_all(bundle.join("bin")).unwrap();
        // The currently-running binary the launcher execs.
        std::fs::write(bundle.join("bin/sidle"), b"running-v1").unwrap();

        let staged = stage_update(&bundle, "bin/sidle", b"staged-v2").unwrap();

        assert_eq!(staged, bundle.join("bin/sidle.new"));
        assert_eq!(std::fs::read(&staged).unwrap(), b"staged-v2");
        // Running binary untouched — the swap happens later, in the launcher.
        assert_eq!(std::fs::read(bundle.join("bin/sidle")).unwrap(), b"running-v1");
        // The fixed-name temp was renamed away, not left behind.
        assert!(!bundle.join("bin/sidle.download").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn stage_update_creates_missing_bin_dir() {
        let base = scratch("mkdir");
        let bundle = base.join("extensions/sidle"); // no bin/ yet
        let staged = stage_update(&bundle, "bin/sidle", b"v2").unwrap();
        assert!(staged.exists());
        assert_eq!(std::fs::read(&staged).unwrap(), b"v2");
        let _ = std::fs::remove_dir_all(&base);
    }
}
