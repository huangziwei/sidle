//! On-device app deploy — pushes every app in the fleet onto a jailbroken
//! Kindle over a cable: the picker (its armv7 binary, launcher wrapper, KUAL
//! metadata, the `documents/Sidle.sh` tile, a freshly rendered
//! `etc/server.conf` and the CA at `etc/ca.pem`), and whatever else is
//! registered.
//!
//! The file list comes from [`crate::library::apps::DevicePlan`], which walks
//! every app's mount-rooted tree. A file joins the deploy by sitting in a tree
//! at the path it lands on.
//!
//! Two slots are in no tree: `etc/server.conf` is rendered per install, and
//! `etc/ca.pem` is copied from the library root. That CA is the picker's only
//! trust anchor — the device client compiles in no public root set.
//!
//! [`decide`] settles each path against the install receipt ([`super::receipt`])
//! ahead of the device's bytes. A path whose source hashes to the receipt's
//! entry is done, unread. A path whose device copy matches neither the source
//! nor the receipt is [`DeployFileState::Diverged`] and is kept; `force`
//! compares bytes and overwrites.
//!
//! `documents/Sidle.sh` is a jailbreak-hotfix scriptlet the library indexes as
//! a tile, and tapping it runs `extensions/sidle/bin/sidle.sh`. KUAL does not
//! run on this firmware; `config.xml` and `menu.json` are inert there.
//!
//! [`install_all`] is idempotent — content-hash equal means skip.
//! `etc/server.conf` is rendered on every push, carrying a rotated
//! `.server-token` across.
//!
//! A cable push writes every path directly: nothing on the device is executing
//! while it is mounted. Any `.new` a LAN pull staged is deleted, ahead of the
//! launcher swapping a stale binary over the fresh one at the next launch.
//!
//! Transport-agnostic: everything below drives the [`Transport`] trait, one
//! deploy over a mass-storage mount (KOA2) and MTP (Colorsoft). Callers refuse
//! a device without a jailbreak's `/extensions/` layout.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::receipt::{self, AppReceipt, FileReceipt, InstallState, now_secs};
use super::{TPath, Transport};
use crate::library::apps::{AppTree, DevicePlan};

/// Cross-compile target the native binary is built for. The picker runs on
/// armv7l Kindles.
const NATIVE_TARGET_TRIPLE: &str = "armv7-unknown-linux-musleabihf";

/// The picker's own app id. It is the one app with per-install bytes — the
/// rendered `server.conf` and the CA it pins — and a plan without it gets
/// neither.
const PICKER_ID: &str = "sidle";

/// Where the picker's binary lives inside the mount tree. Three things agree on
/// it: the mirror it is staged into, the plan entry whose absence means "not
/// built yet", and the rule that stages it.
const PICKER_BINARY_REL: &str = "extensions/sidle/bin/sidle";

/// The tree that ships with the desktop app: the `device/` mirror in a dev
/// checkout, the staged resources in a packaged one. It holds the picker and
/// bokai, and it is what [`crate::library::apps::plan`] composes with every
/// registered app.
///
/// `binary_path` is where the cross-compiled picker lands — `target/` on the
/// dev path, under a different name from the desktop app's own `sidle`.
/// [`Self::stage_binary`] puts it in the mirror at the path it installs to.
/// Both are resolved at app startup and re-evaluated per status query.
#[derive(Debug, Clone)]
pub struct DeploySource {
    pub mount_dir: PathBuf,
    pub binary_path: PathBuf,
}

impl DeploySource {
    /// Resolve the source paths from a workspace root. The workspace
    /// root is the directory containing the `[workspace]` Cargo.toml
    /// (`/Users/.../sidle/` for the developer machine).
    pub fn from_workspace_root(repo: &Path) -> Self {
        Self {
            mount_dir: repo.join("device"),
            // The bin target is `sidle-native`; the desktop app owns the
            // `sidle` name in target/release. build.sh renames it at copy time.
            binary_path: repo
                .join("target")
                .join(NATIVE_TARGET_TRIPLE)
                .join("release")
                .join("sidle-native"),
        }
    }

    /// A packaged build's on-device assets ride as Tauri bundle resources.
    /// build.sh stages them under `Contents/Resources/resources/device/`, the
    /// same mount mirror `from_workspace_root` points at, cross-compiled picker
    /// included.
    ///
    /// `res_dir` is `app.path().resource_dir()`. `binary_path` is the mirror's
    /// own copy, which leaves [`Self::stage_binary`] with nothing to move.
    pub fn from_resource_root(res_dir: &Path) -> Self {
        let staged = res_dir.join("resources").join("device");
        Self {
            binary_path: staged.join(PICKER_BINARY_REL),
            mount_dir: staged,
        }
    }

    /// Where the picker's binary sits inside the mirror.
    pub fn mirrored_binary(&self) -> PathBuf {
        self.mount_dir.join(PICKER_BINARY_REL)
    }

    /// Copy the cross-built picker into the mirror when `target/`'s copy is
    /// newer, putting current bytes at the path they install to.
    ///
    /// The picker is the one file in the fleet a build leaves outside the tree
    /// that ships it. Every other app's `build.sh` writes to its mount path
    /// directly. The `.build-ts` sidecar rides along: the device compares that
    /// value against the one compiled into the binary.
    ///
    /// A packaged build points `binary_path` at the mirror's own copy, leaving
    /// this with nothing to move. A missing source binary is the "not built
    /// yet" case [`compute_status`] reports from the plan.
    pub fn stage_binary(&self) -> Result<()> {
        let dest = self.mirrored_binary();
        if dest == self.binary_path {
            return Ok(());
        }
        let Some(src_mtime) = mtime_ms(&self.binary_path) else {
            return Ok(());
        };
        if mtime_ms(&dest).is_some_and(|d| d >= src_mtime) {
            return Ok(());
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        atomic_write(&dest, &std::fs::read(&self.binary_path)?)?;

        let mut src_sidecar = self.binary_path.clone().into_os_string();
        src_sidecar.push(".build-ts");
        if let Ok(ts) = std::fs::read(PathBuf::from(src_sidecar)) {
            atomic_write(&with_suffix(&dest, ".build-ts"), &ts)?;
        }
        Ok(())
    }

    /// Build time (unix seconds) of `binary_path`, read from the
    /// `<binary_path>.build-ts` sidecar `build.sh` writes beside the
    /// cross-built picker. Feeds the manifest's `built_at`, the device's
    /// downgrade guard. `0` with no sidecar, which leaves the device on its
    /// sha-only check.
    pub fn build_ts(&self) -> u64 {
        let mut sidecar = self.binary_path.clone().into_os_string();
        sidecar.push(".build-ts");
        std::fs::read_to_string(PathBuf::from(sidecar))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }
}

/// Fields rendered into `etc/server.conf` on the device. A struct, not one
/// rendered String: the UI shows the individual values ahead of an install.
#[derive(Debug, Clone, Serialize)]
pub struct ServerConfRender {
    pub host: String,
    pub port: u16,
    /// The Kindle's own USB iSerial (`DeviceInfo.serial`, read by the Mac at
    /// mount). The picker echoes it as `device_serial` in its `POST
    /// /sync/annotations` push, which keys those annotations to this device.
    pub serial: String,
    pub token: String,
}

impl ServerConfRender {
    /// The bytes that land at `etc/server.conf` on the device. The trailing
    /// newline is part of them: the staleness check is byte-equality.
    pub fn render(&self) -> String {
        format!(
            "# Sidle server config — read by `bin/sidle` on launch.\n\
             # DO NOT COMMIT — this file holds the bearer token for the LAN server.\n\
             # Gitignored at the repo root.\n\
             \n\
             HOST={}\n\
             PORT={}\n\
             SERIAL={}\n\
             TOKEN={}\n",
            self.host, self.port, self.serial, self.token,
        )
    }
}

/// Per-file outcome of the staleness check.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeployFileState {
    /// The device holds what this push writes: its bytes were compared, or the
    /// receipt names them at a path that is present.
    Synced,
    /// The source moved on, and the device copy is the one sidle put there —
    /// sidle's to replace.
    Stale,
    /// Not on the device: a clean install, or a file someone deleted.
    Missing,
    /// The device copy is neither the source's nor the one the receipt records.
    /// Something outside this push wrote it — a hand-drag, or an edit made on
    /// the device — and its bytes exist nowhere else. `force` overwrites it.
    Diverged,
    /// The file is missing on the *source* side. The UI surfaces "run `cargo
    /// build ...` first".
    SourceMissing,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeployFileStatus {
    /// Mount-relative device path — `extensions/sidle/bin/sidle`,
    /// `documents/Sidle.sh`. The UI shows it verbatim, the place it lands.
    pub device_path: String,
    pub state: DeployFileState,
}

/// Headline summary the UI uses to label the button + status pill.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeployOverall {
    /// No Kindle connected. Built at the command layer, where the device cell
    /// is empty; `compute_status` runs against a connected transport.
    #[allow(dead_code)]
    DeviceDisconnected,
    /// The source binary does not exist — `cargo build --release --target
    /// armv7-unknown-linux-musleabihf -p sidle-native` has not run. The button
    /// is disabled until it does.
    BinaryNotBuilt,
    /// Every device file is missing — first-time install.
    NotInstalled,
    /// At least one file differs. `stale_count` plus `missing_count` is what a
    /// push writes.
    Stale {
        stale_count: u32,
        missing_count: u32,
    },
    /// Nothing to write, and at least one file on the device is not the one
    /// sidle put there. A plain push changes nothing; `force` writes them.
    DivergedOnly { diverged_count: u32 },
    /// Every file is present and content-equal. The pill reads "In sync", and
    /// the button re-pushes.
    InSync,
}

/// One app's state on the connected device, rolled up from its files.
///
/// The Apps tab reads this: a row says `Installed 0.3.0` from
/// `installed_version`, and `Update` from an `overall` naming a file that
/// differs.
#[derive(Debug, Clone, Serialize)]
pub struct AppDeployStatus {
    pub id: String,
    pub name: String,
    /// What the source tree states, when it states anything.
    pub version: Option<String>,
    /// What the receipt says the last push put there. `None` when sidle has
    /// never installed this app on this device.
    pub installed_version: Option<String>,
    pub file_count: usize,
    pub total_bytes: u64,
    /// Files this push writes, and what they weigh. Zero on a current app.
    pub write_count: u32,
    pub write_bytes: u64,
    /// Files the device changed out from under sidle, which a push keeps.
    pub diverged_count: u32,
    pub overall: DeployOverall,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeployStatus {
    pub overall: DeployOverall,
    /// One entry per app in the plan, in plan order.
    pub apps: Vec<AppDeployStatus>,
    pub files: Vec<DeployFileStatus>,
    /// mtime of the source binary, in Unix epoch milliseconds. The frontend
    /// renders a relative timestamp off it.
    pub binary_mtime_ms: Option<u64>,
    /// mtime of the newest source file under `sidle/native/src/`.
    /// Surfaces "your binary is older than your source" *before* the
    /// user clicks anything — staleness against the device is one
    /// thing, but if your binary is also pre-dating your code edits
    /// you'd push a no-op.
    pub native_source_mtime_ms: Option<u64>,
}

/// Per-file outcome of an install: what the push did with the path. Distinct
/// from `DeployFileState`, which is what the path looked like before it.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeployFileInstallResult {
    /// Wrote new bytes, over a `Missing` or `Stale` path.
    Wrote { device_path: String },
    /// Skipped — the device holds what this push writes.
    Skipped { device_path: String },
    /// Skipped — the device copy is not the one sidle wrote, and its bytes
    /// exist nowhere else. `force` writes it anyway.
    KeptDeviceCopy { device_path: String },
    /// Nothing to write: the bytes for this slot could not be produced. Only
    /// `etc/server.conf` reaches it, with no LAN address or no server token.
    /// The device copy was never compared, which separates it from `Skipped`;
    /// nothing went wrong, which separates it from `Failed`.
    SourceMissing { device_path: String },
    /// Write failed. The rest of the install continues, leaving a recoverable
    /// state.
    Failed { device_path: String, error: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct DeployInstallReport {
    pub results: Vec<DeployFileInstallResult>,
}

/// Where a slot's bytes come from on this machine.
enum Source {
    File(PathBuf),
    /// Bytes computed live. `None` says the inputs to compute them are
    /// unavailable, and the slot reports [`DeployFileState::SourceMissing`]
    /// while the deploy carries on. Only `etc/server.conf` needs it; every
    /// other slot is independent of the LAN address.
    Rendered(Option<String>),
}

/// One file in the deploy. `device_rel` is **mount-relative** — the same string
/// that keys the manifest entry and the served route.
struct Slot {
    device_rel: String,
    source: Source,
    /// The app whose receipt records this path.
    app_id: String,
}

impl Slot {
    fn tpath(&self) -> TPath {
        TPath::parse(&self.device_rel)
    }
}

/// A directly-applied slot outside any app tree. Both belong to the picker,
/// the app whose install produces them.
fn plain(device_rel: &str, source: Source) -> Slot {
    Slot {
        device_rel: device_rel.to_string(),
        source,
        app_id: PICKER_ID.to_string(),
    }
}

/// Every file this push writes: the composed tree, plus the two slots whose
/// bytes are per-install.
///
/// A slot's `apply` is carried and not acted on. Over a cable nothing on the
/// device is executing, and a `staged` path is written directly — see
/// [`install_all`], which also clears any `.new` a LAN pull left behind.
fn slots(plan: &DevicePlan, conf: Option<&ServerConfRender>, ca_cert: &Path) -> Vec<Slot> {
    let mut out: Vec<Slot> = plan
        .files
        .iter()
        .map(|f| Slot {
            device_rel: f.path.clone(),
            source: Source::File(f.source.clone()),
            app_id: f.app_id.clone(),
        })
        .collect();

    // Both belong to the picker. A plan narrowed to any other app carries
    // neither of them.
    if plan.app(PICKER_ID).is_some() {
        // The CA lives in the library root, in no tree. It is the one root the
        // picker pins — the device client compiles in no public root set — and
        // a device without it completes no handshake. The caller guarantees the
        // file exists.
        out.push(plain(
            "extensions/sidle/etc/ca.pem",
            Source::File(ca_cert.to_path_buf()),
        ));
        // Not in any tree: per-install secret, rendered live. `None` when there
        // is no LAN address or no server token to render — the slot then
        // reports `SourceMissing` and everything else installs anyway.
        out.push(plain(
            "extensions/sidle/etc/server.conf",
            Source::Rendered(conf.map(ServerConfRender::render)),
        ));
    }

    out.sort_by(|a, b| a.device_rel.cmp(&b.device_rel));
    out
}

/// Hex sha256 of a file's bytes. Returns `None` if the path doesn't
/// exist (the common case for both source-missing and device-missing
/// files); other errors propagate.
pub fn sha256_file_opt(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(sha256_bytes(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::from(e)).with_context(|| format!("read {}", path.display())),
    }
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Hex sha256 of the on-device bytes at `path`, over either transport
/// (mass-storage `std::fs` or MTP USB).
fn device_sha(transport: &dyn Transport, path: &TPath) -> Result<String> {
    Ok(sha256_bytes(&transport.read(path)?))
}

/// What is on the device, one directory listing at a time.
///
/// Every decision starts with "is it there, and how big" — a listing answers
/// that for a whole directory at once, where an `exists` per path is a round
/// trip per path. Over MTP that is the difference between one request and a
/// hundred, and the size it also yields is often enough to settle a file
/// without reading a byte of it.
#[derive(Default)]
struct DeviceIndex {
    /// Parent directory to its files' sizes. A directory that could not be
    /// listed caches as empty, and every path under it reads as absent.
    dirs: HashMap<String, HashMap<String, u64>>,
}

impl DeviceIndex {
    /// The size of the device's copy of `mount_rel`, or `None` if it has none.
    fn size_of(&mut self, transport: &dyn Transport, mount_rel: &str) -> Option<u64> {
        let (dir, name) = match mount_rel.rsplit_once('/') {
            Some((dir, name)) => (dir, name),
            None => ("", mount_rel),
        };
        if !self.dirs.contains_key(dir) {
            let listed = transport
                .list(&TPath::parse(dir))
                .map(|entries| {
                    entries
                        .into_iter()
                        .filter(|e| !e.is_dir)
                        .map(|e| (e.name, e.size.unwrap_or(0)))
                        .collect()
                })
                .unwrap_or_default();
            self.dirs.insert(dir.to_string(), listed);
        }
        self.dirs.get(dir)?.get(name).copied()
    }
}

/// Where the device stands on one path.
///
/// Reads as little as the answer needs. The receipt says what sidle last wrote
/// there: a source hashing to it is settled by the listing alone. A path that
/// moved is pulled off the device, to tell an update apart from an edit.
///
/// `force` throws the receipt away and compares bytes, which is both how a
/// diverged file gets overwritten and how a receipt that has drifted out of
/// step with the device is corrected.
fn decide(
    transport: &dyn Transport,
    index: &mut DeviceIndex,
    slot: &Slot,
    receipt: Option<&FileReceipt>,
    source_hash: &str,
    source_size: u64,
    force: bool,
) -> Result<DeployFileState> {
    let Some(device_size) = index.size_of(transport, &slot.device_rel) else {
        return Ok(DeployFileState::Missing);
    };
    let tpath = slot.tpath();

    if force {
        return Ok(if device_sha(transport, &tpath)? == source_hash {
            DeployFileState::Synced
        } else {
            DeployFileState::Stale
        });
    }

    match receipt {
        // The source hashes to what sidle wrote, at a path the listing found.
        // Nothing is read.
        Some(r) if r.sha256 == source_hash => Ok(DeployFileState::Synced),
        // A device copy of a different length is not the one that was written,
        // and the listing settles it with no read.
        Some(r) if device_size != r.size => Ok(DeployFileState::Diverged),
        Some(r) => {
            let device_hash = device_sha(transport, &tpath)?;
            Ok(if device_hash == r.sha256 {
                DeployFileState::Stale
            } else if device_hash == source_hash {
                DeployFileState::Synced
            } else {
                DeployFileState::Diverged
            })
        }
        // No receipt: sidle has never written here, and the device copy is
        // whatever a hand-drag left. This push starts the record.
        None if device_size != source_size => Ok(DeployFileState::Stale),
        None => Ok(if device_sha(transport, &tpath)? == source_hash {
            DeployFileState::Synced
        } else {
            DeployFileState::Stale
        }),
    }
}

/// The source bytes' hash and length, or `None` for a slot whose bytes cannot
/// be produced at all.
fn source_digest(slot: &Slot) -> Result<Option<(String, u64)>> {
    match &slot.source {
        Source::Rendered(Some(text)) => {
            Ok(Some((sha256_bytes(text.as_bytes()), text.len() as u64)))
        }
        Source::Rendered(None) => Ok(None),
        Source::File(path) => {
            // Two kinds of file land here: one from an app's tree, where a miss
            // means the tree changed under the walk, and the CA in the library
            // root, where it means nobody issued one. Name the path and let it
            // say which.
            let bytes = std::fs::read(path).with_context(|| {
                format!(
                    "deploy source missing at {} — the tree changed under \
                     the walk, or TLS material was never issued",
                    path.display()
                )
            })?;
            Ok(Some((sha256_bytes(&bytes), bytes.len() as u64)))
        }
    }
}

/// Compute the per-file staleness given a source layout, a rendered
/// server.conf, and the connected device's [`Transport`] (mass-storage volume
/// or MTP — the deploy is identical over either).
///
/// `conf` is `None` when its inputs are unavailable (no routable LAN address,
/// or no server token): the `etc/server.conf` slot reports
/// [`DeployFileState::SourceMissing`], every other slot is checked, and the
/// status matches what [`install_all`] pushes.
///
/// Does no writes, no network. Reads the source side off the host filesystem
/// and the device side through `transport`.
pub fn compute_status(
    plan: &DevicePlan,
    source: &DeploySource,
    conf: Option<&ServerConfRender>,
    ca_cert: &Path,
    transport: &dyn Transport,
) -> Result<DeployStatus> {
    let slots = slots(plan, conf, ca_cert);
    let state = InstallState::read(transport);
    let mut index = DeviceIndex::default();
    let mut files = Vec::with_capacity(slots.len());

    for slot in &slots {
        let file_state = match source_digest(slot)? {
            None => DeployFileState::SourceMissing,
            Some((hash, size)) => decide(
                transport,
                &mut index,
                slot,
                state.file(&slot.app_id, &slot.device_rel),
                &hash,
                size,
                false,
            )?,
        };
        files.push(DeployFileStatus {
            device_path: slot.device_rel.clone(),
            state: file_state,
        });
    }

    // The picker's own binary reaches the plan through the built-in tree. Its
    // absence names the armv7 cross-build as the missing step.
    let overall = if !plan.files.iter().any(|f| f.path == PICKER_BINARY_REL) {
        DeployOverall::BinaryNotBuilt
    } else {
        summarize(&files)
    };

    let binary_mtime_ms = mtime_ms(&source.binary_path);
    let native_source_mtime_ms = newest_native_source_mtime_ms(source);
    let apps = roll_up_per_app(plan, &files, &state);

    Ok(DeployStatus {
        overall,
        apps,
        files,
        binary_mtime_ms,
        native_source_mtime_ms,
    })
}

/// Group the per-file states by the app that owns each path.
///
/// The two per-install slots (`etc/ca.pem`, `etc/server.conf`) are in no tree,
/// and [`DevicePlan::owner_of`] does not know them. Both sit under the picker,
/// the app whose install renders them.
fn roll_up_per_app(
    plan: &DevicePlan,
    files: &[DeployFileStatus],
    state: &InstallState,
) -> Vec<AppDeployStatus> {
    let size_of: HashMap<&str, u64> = plan
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.size))
        .collect();

    let mut out = Vec::with_capacity(plan.apps.len());
    for tree in &plan.apps {
        let id = tree.app.id.as_str();
        let mine: Vec<&DeployFileStatus> = files
            .iter()
            .filter(|f| match plan.owner_of(&f.device_path) {
                Some(owner) => owner == id,
                None => id == PICKER_ID,
            })
            .collect();

        let mut write_count = 0u32;
        let mut write_bytes = 0u64;
        let mut diverged_count = 0u32;
        for f in &mine {
            match f.state {
                DeployFileState::Stale | DeployFileState::Missing => {
                    write_count += 1;
                    write_bytes += size_of.get(f.device_path.as_str()).copied().unwrap_or(0);
                }
                DeployFileState::Diverged => diverged_count += 1,
                DeployFileState::Synced | DeployFileState::SourceMissing => {}
            }
        }

        let statuses: Vec<DeployFileStatus> = mine.into_iter().cloned().collect();
        out.push(AppDeployStatus {
            id: id.to_string(),
            name: tree.app.name.clone(),
            version: tree.app.version.clone(),
            installed_version: state.app(id).and_then(|r| r.version.clone()),
            file_count: tree.files.len(),
            total_bytes: tree.total_size(),
            write_count,
            write_bytes,
            diverged_count,
            overall: summarize(&statuses),
        });
    }
    out
}

fn summarize(files: &[DeployFileStatus]) -> DeployOverall {
    let mut stale = 0u32;
    let mut missing = 0u32;
    let mut synced = 0u32;
    let mut diverged = 0u32;
    for f in files {
        match f.state {
            DeployFileState::Synced => synced += 1,
            DeployFileState::Stale => stale += 1,
            DeployFileState::Missing => missing += 1,
            // Present, and not what sidle put there: neither work for a push
            // nor a file in sync.
            DeployFileState::Diverged => diverged += 1,
            DeployFileState::SourceMissing => {}
        }
    }
    if stale > 0 || missing > 0 {
        if synced == 0 && diverged == 0 && stale == 0 {
            DeployOverall::NotInstalled
        } else {
            DeployOverall::Stale {
                stale_count: stale,
                missing_count: missing,
            }
        }
    } else if diverged > 0 {
        DeployOverall::DivergedOnly {
            diverged_count: diverged,
        }
    } else {
        DeployOverall::InSync
    }
}

fn mtime_ms(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let dur = mtime.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    Some(dur.as_millis() as u64)
}

/// Walks `sidle/native/src/` and returns the newest mtime found. The
/// frontend uses this to flag "binary older than source" in the UI
/// without re-running cargo. Best-effort: any IO error returns `None`.
fn newest_native_source_mtime_ms(source: &DeploySource) -> Option<u64> {
    // The native crate lives at `<repo>/sidle/native/src/`, derived from
    // `mount_dir`, which is `<repo>/device`.
    let repo = source.mount_dir.parent()?;
    let native_src = repo.join("sidle").join("native").join("src");
    walk_newest_mtime(&native_src)
}

fn walk_newest_mtime(dir: &Path) -> Option<u64> {
    let mut newest: Option<u64> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let read = match std::fs::read_dir(&p) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file()
                && let Some(ms) = mtime_ms(&path)
            {
                newest = Some(newest.map_or(ms, |n| n.max(ms)));
            }
        }
    }
    newest
}

/// Install every slot, and record what landed.
///
/// Each file is written unless the device holds those bytes, or holds bytes
/// nobody here put there — see [`decide`]. A failure is recorded per file and
/// the rest continue, leaving a state the next click recovers.
///
/// `force` overwrites a device copy sidle did not write, comparing bytes
/// against the source. It is the way past a file changed on the device, and the
/// repair for a receipt out of step with what is there.
///
/// `on_progress` fires once per file with its outcome. The Tauri command wires
/// it to `device-app:install-progress`; tests pass `|_| {}`.
///
/// `conf` is `None` when `etc/server.conf` has no bytes to render — that slot
/// reports [`DeployFileInstallResult::SourceMissing`] and every other slot is
/// written. A device on a network that carries no LAN address is the device
/// that needs the cable.
pub fn install_all(
    plan: &DevicePlan,
    conf: Option<&ServerConfRender>,
    ca_cert: &Path,
    transport: &dyn Transport,
    force: bool,
    mut on_progress: impl FnMut(&DeployFileInstallResult),
) -> Result<DeployInstallReport> {
    // `Transport::write_atomic` creates the bin/ and etc/ parents on both
    // transports (mass-storage `create_dir_all`, MTP `ensure_folder`).
    let slots = slots(plan, conf, ca_cert);
    let mut state = InstallState::read(transport);
    let mut index = DeviceIndex::default();
    let mut results = Vec::with_capacity(slots.len());
    let mut recorded: HashMap<String, BTreeMap<String, FileReceipt>> = HashMap::new();

    for slot in &slots {
        let prior = state.file(&slot.app_id, &slot.device_rel).cloned();
        let (result, receipt) = install_one(transport, &mut index, slot, prior, force);
        on_progress(&result);
        results.push(result);
        if let Some(receipt) = receipt {
            recorded
                .entry(slot.app_id.clone())
                .or_default()
                .insert(slot.device_rel.clone(), receipt);
        }
    }

    // A cable push is authoritative over a LAN self-update staged as
    // `<path>.new`: the applier one level up swaps whatever it finds there at
    // the next launch. Best-effort; absent is the norm.
    for path in plan.staged_paths() {
        let _ = transport.delete(&TPath::parse(&format!("{path}.new")));
    }

    // One record per app in `plan`, replacing wholesale: a path the app has
    // stopped shipping leaves the receipt with it. An app outside `plan` keeps
    // its record, which is what a per-row install needs.
    let installed_at = now_secs();
    for tree in &plan.apps {
        let id = tree.app.id.as_str();
        state.set_app(
            id,
            AppReceipt {
                version: tree.app.version.clone(),
                built_at: tree.built_at(),
                installed_at,
                files: recorded.remove(id).unwrap_or_default(),
            },
        );
    }
    if let Err(e) = state.write(transport) {
        // The files are on the device either way. A lost receipt costs the next
        // push its shortcuts and its ability to spot an on-device edit, so it is
        // reported rather than swallowed — and not raised to an error, which
        // would call a completed install a failure.
        let result = DeployFileInstallResult::Failed {
            device_path: receipt::RECEIPT_PATH.to_string(),
            error: format!("{e:#}"),
        };
        on_progress(&result);
        results.push(result);
    }

    Ok(DeployInstallReport { results })
}

/// Install one slot, and say what the receipt should record for it.
///
/// A `None` receipt leaves the prior entry standing: this push wrote nothing at
/// that path, and the record of what sidle last wrote there holds.
fn install_one(
    transport: &dyn Transport,
    index: &mut DeviceIndex,
    slot: &Slot,
    prior: Option<FileReceipt>,
    force: bool,
) -> (DeployFileInstallResult, Option<FileReceipt>) {
    let bytes_result: Result<Vec<u8>> = match &slot.source {
        Source::Rendered(Some(text)) => Ok(text.as_bytes().to_vec()),
        Source::Rendered(None) => {
            return (
                DeployFileInstallResult::SourceMissing {
                    device_path: slot.device_rel.clone(),
                },
                prior,
            );
        }
        Source::File(path) => {
            std::fs::read(path).with_context(|| format!("read source {}", path.display()))
        }
    };
    let bytes = match bytes_result {
        Ok(b) => b,
        Err(e) => {
            return (
                DeployFileInstallResult::Failed {
                    device_path: slot.device_rel.clone(),
                    error: format!("{e:#}"),
                },
                prior,
            );
        }
    };

    let source = FileReceipt {
        sha256: sha256_bytes(&bytes),
        size: bytes.len() as u64,
    };
    let state = decide(
        transport,
        index,
        slot,
        prior.as_ref(),
        &source.sha256,
        source.size,
        force,
    );
    match state {
        Err(e) => (
            DeployFileInstallResult::Failed {
                device_path: slot.device_rel.clone(),
                error: format!("{e:#}"),
            },
            prior,
        ),
        Ok(DeployFileState::Synced) => (
            DeployFileInstallResult::Skipped {
                device_path: slot.device_rel.clone(),
            },
            Some(source),
        ),
        Ok(DeployFileState::Diverged) => (
            DeployFileInstallResult::KeptDeviceCopy {
                device_path: slot.device_rel.clone(),
            },
            prior,
        ),
        Ok(_) => match transport.write_atomic(&slot.tpath(), &bytes) {
            Ok(()) => (
                DeployFileInstallResult::Wrote {
                    device_path: slot.device_rel.clone(),
                },
                Some(source),
            ),
            Err(e) => (
                DeployFileInstallResult::Failed {
                    device_path: slot.device_rel.clone(),
                    error: format!("{e:#}"),
                },
                prior,
            ),
        },
    }
}

/// What an uninstall took off the device.
#[derive(Debug, Clone, Serialize)]
pub struct UninstallReport {
    pub id: String,
    /// Mount-relative paths that were there and are gone.
    pub removed: Vec<String>,
    pub errors: Vec<String>,
}

/// Take one app off the device: its extension directory, its tile, and its
/// record in the receipt.
///
/// The app is `extensions/<id>/**` plus the one `documents/*.sh` that launches
/// it. Nothing else in `documents/` is touched.
pub fn uninstall(tree: &AppTree, transport: &dyn Transport) -> Result<UninstallReport> {
    let id = tree.app.id.clone();
    let mut removed = Vec::new();
    let mut errors = Vec::new();

    let ext = tree.app.extension_dir();
    match transport.delete_dir(&TPath::parse(&ext)) {
        Ok(true) => removed.push(ext),
        Ok(false) => {}
        Err(e) => errors.push(format!("{ext}: {e:#}")),
    }

    if let Some(tile) = &tree.app.tile {
        match transport.delete(&TPath::parse(tile)) {
            Ok(true) => removed.push(tile.clone()),
            Ok(false) => {}
            Err(e) => errors.push(format!("{tile}: {e:#}")),
        }
    }

    // An entry naming paths that are gone reads as an edit on the device.
    let mut state = InstallState::read(transport);
    if state.app(&id).is_some() {
        state.forget_app(&id);
        if let Err(e) = state.write(transport) {
            errors.push(format!("{}: {e:#}", receipt::RECEIPT_PATH));
        }
    }

    Ok(UninstallReport {
        id,
        removed,
        errors,
    })
}

pub(crate) fn atomic_write(dest: &Path, bytes: &[u8]) -> Result<()> {
    let partial = with_suffix(dest, ".partial");
    {
        let mut f = std::fs::File::create(&partial)
            .with_context(|| format!("create {}", partial.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("write {}", partial.display()))?;
        // FAT/exFAT doesn't actually fsync the way a UNIX fs does, but
        // calling it doesn't hurt and gives the macOS VFS a hint to
        // flush before rename.
        let _ = f.sync_all();
    }
    std::fs::rename(&partial, dest)
        .with_context(|| format!("rename {} -> {}", partial.display(), dest.display()))?;
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Detect the IPv4 address sidle-server is reachable at from the LAN.
///
/// The "no-packet" UDP trick: `bind` + `connect` to a public-internet address,
/// then read `local_addr()`. The kernel picks the interface it routes through,
/// and no packet is sent.
///
/// `None` with no routable interface. A caller falls back to the HOST in the
/// on-device server.conf, or asks.
pub fn detect_lan_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    // Use a TEST-NET-3 address (RFC 5737) rather than 8.8.8.8 — we
    // don't want to leak that "is this machine using sidle?" to any
    // network observer, and TEST-NET addresses are guaranteed
    // unreachable so even a misbehaving kernel won't actually send.
    sock.set_read_timeout(Some(Duration::from_millis(50))).ok();
    sock.connect("203.0.113.1:80").ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V4(v4) => {
            let ip = *v4.ip();
            // Skip the loopback range — `bind("0.0.0.0:0").connect(...)`
            // on a fully-disconnected machine can sometimes hand back
            // 127.0.0.1, which is useless to the Kindle.
            if IpAddr::V4(ip).is_loopback() {
                None
            } else {
                Some(ip)
            }
        }
        SocketAddr::V6(_) => None,
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_conf() -> ServerConfRender {
        ServerConfRender {
            host: "192.168.0.42".into(),
            port: 8731,
            serial: "G000TESTSERIAL".into(),
            token: "abc123".into(),
        }
    }

    /// Wrap a temp dir as a mass-storage [`Transport`], so the deploy tests
    /// drive the same trait the command layer uses — byte-for-byte the prior
    /// `std::fs`-against-a-mount behavior, now also shared with the MTP path.
    fn ms(dir: &Path) -> crate::library::device::mass_storage::transport::MassStorageTransport {
        crate::library::device::mass_storage::transport::MassStorageTransport::new(
            dir.to_path_buf(),
        )
    }

    /// A stand-in CA on disk. Deliberately NOT under the `device/` mirror: the
    /// real one lives in the library root, and a test that put it in the mirror
    /// would stop exercising the fact that this slot's source comes from
    /// somewhere else entirely. Content is irrelevant here — every slot is
    /// compared by hash, so any stable bytes prove the same plumbing.
    fn ca_path(repo: &Path) -> PathBuf {
        let p = repo.join("library-root").join("tls").join("ca.pem");
        if !p.exists() {
            write_file(
                &p,
                b"-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----\n",
            );
        }
        p
    }

    #[test]
    fn render_has_trailing_newline_and_all_fields() {
        let conf = make_conf();
        let out = conf.render();
        assert!(out.ends_with('\n'));
        assert!(out.contains("HOST=192.168.0.42"));
        assert!(out.contains("PORT=8731"));
        assert!(out.contains("SERIAL=G000TESTSERIAL"));
        assert!(out.contains("TOKEN=abc123"));
    }

    #[test]
    fn render_is_byte_stable() {
        // Same inputs must produce identical bytes — staleness check is
        // byte-equality, any drift would re-trigger "stale" forever.
        let a = make_conf().render();
        let b = make_conf().render();
        assert_eq!(a, b);
    }

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    /// Build a minimal `device/` mirror under `repo_root` — every file at the
    /// relative path it lands on the device — and optionally the cross-built
    /// picker at `<repo>/target/.../release/sidle-native`.
    fn make_source(repo: &Path, include_binary: bool) -> DeploySource {
        let mirror = repo.join("device");
        write_file(
            &mirror.join("extensions/sidle/config.xml"),
            br#"<extension><information><name>Sidle</name>
                <version>0.1.9</version></information></extension>"#,
        );
        write_file(
            &mirror.join("extensions/sidle/menu.json"),
            b"{\"items\":[]}",
        );
        write_file(
            &mirror.join("extensions/sidle/bin/sidle.sh"),
            b"#!/bin/sh\nexec sidle\n",
        );
        write_file(
            &mirror.join("documents/Sidle.sh"),
            b"#!/bin/sh\n# Name: Sidle\nexec /mnt/us/extensions/sidle/bin/sidle.sh\n",
        );
        if include_binary {
            write_file(
                &repo
                    .join("target")
                    .join(NATIVE_TARGET_TRIPLE)
                    .join("release/sidle-native"),
                b"\x7fELF...fake-binary",
            );
        }
        DeploySource::from_workspace_root(repo)
    }

    /// The composed tree a push works from: the built-in mirror alone, with the
    /// cross-built picker staged into it first, and no registered apps.
    fn make_plan(source: &DeploySource) -> crate::library::apps::DevicePlan {
        source.stage_binary().unwrap();
        crate::library::apps::plan_from(&source.mount_dir, &[])
    }

    /// The composed tree alone, for a test that only installs. `compute_status`
    /// is the only caller that still needs the source, for its mtime hints.
    fn make_install_plan(repo: &Path) -> crate::library::apps::DevicePlan {
        make_both(repo, true).1
    }

    /// A source plus its plan.
    fn make_both(
        repo: &Path,
        include_binary: bool,
    ) -> (DeploySource, crate::library::apps::DevicePlan) {
        let source = make_source(repo, include_binary);
        let plan = make_plan(&source);
        (source, plan)
    }

    /// A push with nothing forced, which is every push the button makes.
    fn push(
        plan: &crate::library::apps::DevicePlan,
        conf: Option<&ServerConfRender>,
        ca: &Path,
        device: &Path,
    ) -> DeployInstallReport {
        install_all(plan, conf, ca, &ms(device), false, |_| {}).unwrap()
    }

    fn status_of(
        plan: &crate::library::apps::DevicePlan,
        source: &DeploySource,
        conf: Option<&ServerConfRender>,
        ca: &Path,
        device: &Path,
    ) -> DeployStatus {
        compute_status(plan, source, conf, ca, &ms(device)).unwrap()
    }

    fn state_of(status: &DeployStatus, path: &str) -> DeployFileState {
        status
            .files
            .iter()
            .find(|f| f.device_path == path)
            .unwrap_or_else(|| panic!("{path} is not in the plan"))
            .state
            .clone()
    }

    #[test]
    fn status_not_installed_when_device_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, plan) = make_both(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();

        let status = status_of(
            &plan,
            &source,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            device.path(),
        );
        assert_eq!(status.overall, DeployOverall::NotInstalled);
        assert!(
            status
                .files
                .iter()
                .all(|f| f.state == DeployFileState::Missing)
        );
    }

    #[test]
    fn status_in_sync_after_install() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, plan) = make_both(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();
        let conf = make_conf();
        let ca = ca_path(tmp.path());

        push(&plan, Some(&conf), &ca, device.path());
        let status = status_of(&plan, &source, Some(&conf), &ca, device.path());
        assert_eq!(status.overall, DeployOverall::InSync);
    }

    #[test]
    fn status_stale_when_token_rotated() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, plan) = make_both(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();
        let ca = ca_path(tmp.path());

        // First install with one token.
        let conf1 = make_conf();
        push(&plan, Some(&conf1), &ca, device.path());

        // Token rotates; everything else identical.
        let mut conf2 = conf1.clone();
        conf2.token = "rotated".into();
        let status = status_of(&plan, &source, Some(&conf2), &ca, device.path());

        match status.overall {
            DeployOverall::Stale {
                stale_count,
                missing_count,
            } => {
                assert_eq!(stale_count, 1, "only server.conf should be stale");
                assert_eq!(missing_count, 0);
            }
            other => panic!("expected Stale, got {other:?}"),
        }
        assert_eq!(
            state_of(&status, "extensions/sidle/etc/server.conf"),
            DeployFileState::Stale
        );
    }

    #[test]
    fn status_binary_not_built_when_missing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, plan) = make_both(tmp.path(), false); // no binary
        let device = tempfile::tempdir().unwrap();

        let status = status_of(
            &plan,
            &source,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            device.path(),
        );
        assert_eq!(status.overall, DeployOverall::BinaryNotBuilt);
        assert!(
            !status
                .files
                .iter()
                .any(|f| f.device_path == "extensions/sidle/bin/sidle"),
            "an unbuilt binary is not in the tree, so there is no slot for it — \
             the overall is what says the push cannot go"
        );
    }

    /// A machine with no routable LAN address (or no server token) must still
    /// be able to push over a cable. `etc/server.conf` is the only slot whose
    /// bytes depend on an address; the binary, the launcher, the CA, the KUAL
    /// metadata and the tile do not, and a device on a client-isolated network
    /// is exactly the device that needs them delivered by hand.
    #[test]
    fn conf_without_inputs_reports_source_missing_and_the_rest_installs() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = make_install_plan(tmp.path());
        let device = tempfile::tempdir().unwrap();

        let report = push(&plan, None, &ca_path(tmp.path()), device.path());

        let unwritten: Vec<&str> = report
            .results
            .iter()
            .filter_map(|r| match r {
                DeployFileInstallResult::SourceMissing { device_path } => {
                    Some(device_path.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(unwritten, vec!["extensions/sidle/etc/server.conf"]);

        assert!(
            !device
                .path()
                .join("extensions/sidle/etc/server.conf")
                .exists()
        );
        for landed in [
            "extensions/sidle/bin/sidle",
            "extensions/sidle/bin/sidle.sh",
            "extensions/sidle/etc/ca.pem",
            "extensions/sidle/config.xml",
            "extensions/sidle/menu.json",
            "documents/Sidle.sh",
        ] {
            assert!(
                device.path().join(landed).exists(),
                "{landed} does not depend on a LAN address and must be pushed"
            );
        }
    }

    /// The status the UI shows and the push it would run have to agree: an
    /// unrenderable conf is `SourceMissing` in both, and `summarize` already
    /// excludes that state, so every other slot being current reads "In sync"
    /// rather than a permanent one-file-stale that no push can clear.
    #[test]
    fn status_without_conf_inputs_matches_what_install_would_do() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, plan) = make_both(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();
        let ca = ca_path(tmp.path());

        push(&plan, None, &ca, device.path());
        let status = status_of(&plan, &source, None, &ca, device.path());

        assert_eq!(status.overall, DeployOverall::InSync);
        assert_eq!(
            state_of(&status, "extensions/sidle/etc/server.conf"),
            DeployFileState::SourceMissing
        );
    }

    #[test]
    fn install_skips_synced_files() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = make_install_plan(tmp.path());
        let device = tempfile::tempdir().unwrap();
        let conf = make_conf();
        let ca = ca_path(tmp.path());

        push(&plan, Some(&conf), &ca, device.path());
        let report = push(&plan, Some(&conf), &ca, device.path());

        assert!(
            report
                .results
                .iter()
                .all(|r| matches!(r, DeployFileInstallResult::Skipped { .. })),
            "second install on identical inputs should skip everything"
        );
    }

    #[test]
    fn install_rewrites_only_stale_file_after_token_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = make_install_plan(tmp.path());
        let device = tempfile::tempdir().unwrap();
        let ca = ca_path(tmp.path());

        let conf1 = make_conf();
        push(&plan, Some(&conf1), &ca, device.path());

        let mut conf2 = conf1.clone();
        conf2.token = "rotated".into();
        let report = push(&plan, Some(&conf2), &ca, device.path());

        let written: Vec<&str> = report
            .results
            .iter()
            .filter_map(|r| match r {
                DeployFileInstallResult::Wrote { device_path } => Some(device_path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(written, vec!["extensions/sidle/etc/server.conf"]);
    }

    #[test]
    fn install_creates_subdirs_on_clean_device() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = make_install_plan(tmp.path());
        let device = tempfile::tempdir().unwrap();

        push(
            &plan,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            device.path(),
        );
        assert!(device.path().join("extensions/sidle/bin/sidle").exists());
        assert!(device.path().join("extensions/sidle/bin/sidle.sh").exists());
        assert!(
            device
                .path()
                .join("extensions/sidle/etc/server.conf")
                .exists()
        );
        assert!(device.path().join("extensions/sidle/config.xml").exists());
        assert!(device.path().join("extensions/sidle/menu.json").exists());
        // The scriptlet is mount-rooted — documents/, NOT under extensions/.
        assert!(device.path().join("documents/Sidle.sh").exists());
        assert!(
            !device
                .path()
                .join("extensions/sidle/documents/Sidle.sh")
                .exists()
        );
    }

    /// The CA has to arrive, byte-identical, from outside the `device/` mirror.
    ///
    /// Worth its own test rather than leaning on the file count: the picker pins
    /// this root and carries no public root set at all, so a deploy that pushed
    /// every other file but silently skipped or corrupted this one would look
    /// completely successful and leave a device that cannot complete a single
    /// handshake. The failure would surface as "sync is broken", nowhere near
    /// this code.
    #[test]
    fn install_pushes_the_ca_the_picker_pins() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, plan) = make_both(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();
        let ca = ca_path(tmp.path());

        push(&plan, Some(&make_conf()), &ca, device.path());

        let landed = device.path().join("extensions/sidle/etc/ca.pem");
        assert!(landed.exists(), "the device never received a trust root");
        assert_eq!(
            std::fs::read(&landed).unwrap(),
            std::fs::read(&ca).unwrap(),
            "the CA must arrive byte-identical — a re-encoded or truncated PEM \
             fails verification just as completely as a missing one"
        );
        // It comes from the library root, so it must NOT be expected in the
        // repo mirror — a future `mirrored()` call for this path would compile
        // and then push the wrong bytes.
        assert!(
            !source
                .mount_dir
                .join("extensions/sidle/etc/ca.pem")
                .exists(),
            "the CA is per-install state, not a mirrored repo file"
        );
    }

    #[test]
    fn install_progress_fires_per_file() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = make_install_plan(tmp.path());
        let device = tempfile::tempdir().unwrap();

        let mut events = 0;
        install_all(
            &plan,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            &ms(device.path()),
            false,
            |_| events += 1,
        )
        .unwrap();
        // extensions/sidle/{bin/sidle, bin/sidle.sh, config.xml, menu.json,
        // etc/server.conf, etc/ca.pem} + documents/Sidle.sh
        assert_eq!(events, 7);
    }

    /// Lay down a registered app beside the built-in tree: a descriptor, a
    /// tile, a binary, and a vendored subtree.
    fn karyll_fixture(repo: &Path) -> crate::library::db::AppSourceRow {
        let out = repo.join("deploy").join("out");
        write_file(
            &out.join("extensions/karyll/config.xml"),
            br#"<extension><information><name>Karyll</name>
                <version>0.2.4</version></information></extension>"#,
        );
        write_file(&out.join("extensions/karyll/bin/karyll"), b"armhf");
        write_file(&out.join("extensions/karyll/hid/config.ini"), b"[device]\n");
        write_file(
            &out.join("documents/Karyll.sh"),
            b"#!/bin/sh\n# Name: Karyll\nexec /mnt/us/extensions/karyll/bin/karyll.sh\n",
        );
        karyll_row(repo)
    }

    /// The row alone. A test that edits a file in the tree recomposes through
    /// this, because re-laying the fixture would undo the edit.
    fn karyll_row(repo: &Path) -> crate::library::db::AppSourceRow {
        crate::library::db::AppSourceRow {
            id: "karyll".into(),
            source_kind: crate::library::db::APP_SOURCE_LOCAL.into(),
            source: repo.display().to_string(),
            root: repo.join("deploy").join("out").display().to_string(),
            added_at: 0,
        }
    }

    fn plan_with_karyll(repo: &Path, karyll: &Path) -> crate::library::apps::DevicePlan {
        let source = make_source(repo, true);
        source.stage_binary().unwrap();
        crate::library::apps::plan_from(&source.mount_dir, &[karyll_fixture(karyll)])
    }

    /// Recompose against trees already on disk.
    fn replan(repo: &Path, karyll: &Path) -> crate::library::apps::DevicePlan {
        let source = make_source(repo, true);
        crate::library::apps::plan_from(&source.mount_dir, &[karyll_row(karyll)])
    }

    const HID_CONF: &str = "extensions/karyll/hid/config.ini";

    /// One cable push installs every app, not just the picker.
    #[test]
    fn a_registered_app_installs_over_the_same_cable() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let plan = plan_with_karyll(tmp.path(), karyll.path());

        push(
            &plan,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            device.path(),
        );

        for landed in [
            "extensions/sidle/bin/sidle",
            "extensions/sidle/etc/ca.pem",
            "extensions/karyll/bin/karyll",
            HID_CONF,
            "documents/Karyll.sh",
            "documents/Sidle.sh",
        ] {
            assert!(
                device.path().join(landed).exists(),
                "{landed} never arrived"
            );
        }
    }

    /// The rule the receipt exists for. A user sets `[device] name` on the
    /// Kindle; the app then ships a new default for the same file. The push
    /// wants to write and must not: the device's bytes exist nowhere else, and
    /// no repo had to remember to mark the file for that to hold.
    #[test]
    fn a_file_changed_on_the_device_is_kept_when_the_source_moves_on() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let ca = ca_path(tmp.path());
        let conf = make_conf();
        let plan = plan_with_karyll(tmp.path(), karyll.path());
        push(&plan, Some(&conf), &ca, device.path());

        let on_device = device.path().join(HID_CONF);
        std::fs::write(&on_device, b"[device]\nname = My-Keyboard\n").unwrap();
        std::fs::write(
            karyll.path().join("deploy/out").join(HID_CONF),
            b"[device]\nrate = 9600\n",
        )
        .unwrap();

        let plan = replan(tmp.path(), karyll.path());
        let source = make_source(tmp.path(), true);
        let status = status_of(&plan, &source, Some(&conf), &ca, device.path());
        assert_eq!(state_of(&status, HID_CONF), DeployFileState::Diverged);
        assert_eq!(
            status
                .apps
                .iter()
                .find(|a| a.id == "karyll")
                .unwrap()
                .overall,
            DeployOverall::DivergedOnly { diverged_count: 1 }
        );

        let report = push(&plan, Some(&conf), &ca, device.path());
        assert_eq!(
            std::fs::read(&on_device).unwrap(),
            b"[device]\nname = My-Keyboard\n",
            "a file edited on the device must survive a re-push"
        );
        assert!(report.results.iter().any(|r| matches!(
            r,
            DeployFileInstallResult::KeptDeviceCopy { device_path } if device_path == HID_CONF
        )));
    }

    /// The way past it, and the only one: force compares bytes rather than
    /// trusting the receipt, and writes what differs.
    #[test]
    fn force_overwrites_a_file_changed_on_the_device() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let ca = ca_path(tmp.path());
        let conf = make_conf();
        let plan = plan_with_karyll(tmp.path(), karyll.path());
        push(&plan, Some(&conf), &ca, device.path());

        std::fs::write(device.path().join(HID_CONF), b"edited on device\n").unwrap();
        install_all(&plan, Some(&conf), &ca, &ms(device.path()), true, |_| {}).unwrap();

        assert_eq!(
            std::fs::read(device.path().join(HID_CONF)).unwrap(),
            b"[device]\n"
        );
        // And the receipt is back in step: an unforced push now has nothing to
        // say about the file.
        let source = make_source(tmp.path(), true);
        let status = status_of(&plan, &source, Some(&conf), &ca, device.path());
        assert_eq!(state_of(&status, HID_CONF), DeployFileState::Synced);
    }

    /// A file the device does not have is a first install and gets written like
    /// anything else, whatever the receipt does or does not say.
    #[test]
    fn a_missing_file_is_written_back() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let ca = ca_path(tmp.path());
        let conf = make_conf();
        let plan = plan_with_karyll(tmp.path(), karyll.path());

        push(&plan, Some(&conf), &ca, device.path());
        std::fs::remove_file(device.path().join(HID_CONF)).unwrap();

        let source = make_source(tmp.path(), true);
        let status = status_of(&plan, &source, Some(&conf), &ca, device.path());
        assert_eq!(state_of(&status, HID_CONF), DeployFileState::Missing);

        push(&plan, Some(&conf), &ca, device.path());
        assert_eq!(
            std::fs::read(device.path().join(HID_CONF)).unwrap(),
            b"[device]\n"
        );
    }

    /// The saving the receipt exists for: a path whose source hashes to what
    /// was written is settled without reading the device. A vendored subtree is
    /// where that pays — every file in it, never pulled back.
    #[test]
    fn an_unchanged_path_is_settled_without_reading_the_device() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let ca = ca_path(tmp.path());
        let conf = make_conf();
        let plan = plan_with_karyll(tmp.path(), karyll.path());
        let source = make_source(tmp.path(), true);

        push(&plan, Some(&conf), &ca, device.path());
        // Same length, different bytes: only a read could tell, and none is
        // made, because nothing this push would write has changed.
        std::fs::write(device.path().join(HID_CONF), b"[device]!\n").unwrap();

        let status = status_of(&plan, &source, Some(&conf), &ca, device.path());
        assert_eq!(state_of(&status, HID_CONF), DeployFileState::Synced);
        assert_eq!(status.overall, DeployOverall::InSync);
    }

    /// A device sidle has never installed to carries no receipt, so there is
    /// nothing to have diverged from: the push normalises it to the tree and
    /// starts the record.
    #[test]
    fn a_hand_dragged_file_is_overwritten_when_no_receipt_covers_it() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let plan = plan_with_karyll(tmp.path(), karyll.path());
        write_file(&device.path().join(HID_CONF), b"dragged in by hand\n");

        push(
            &plan,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            device.path(),
        );
        assert_eq!(
            std::fs::read(device.path().join(HID_CONF)).unwrap(),
            b"[device]\n"
        );
    }

    #[test]
    fn uninstall_takes_the_extension_dir_and_the_tile_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let plan = plan_with_karyll(tmp.path(), karyll.path());
        push(
            &plan,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            device.path(),
        );
        write_file(
            &device.path().join("documents/My Novel.txt"),
            b"chapter one",
        );

        let report = uninstall(plan.app("karyll").unwrap(), &ms(device.path())).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            report.removed,
            vec!["extensions/karyll", "documents/Karyll.sh"]
        );
        assert!(!device.path().join("extensions/karyll").exists());
        assert!(!device.path().join("documents/Karyll.sh").exists());
        assert!(device.path().join("documents/My Novel.txt").exists());
        assert!(device.path().join("extensions/sidle/bin/sidle").exists());
        assert!(device.path().join("documents/Sidle.sh").exists());
    }

    #[test]
    fn uninstall_drops_the_apps_receipt_so_a_repush_reinstalls() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let ca = ca_path(tmp.path());
        let conf = make_conf();
        let plan = plan_with_karyll(tmp.path(), karyll.path());
        push(&plan, Some(&conf), &ca, device.path());

        uninstall(plan.app("karyll").unwrap(), &ms(device.path())).unwrap();
        let state = InstallState::read(&ms(device.path()));
        assert!(state.app("karyll").is_none());
        assert!(state.app("sidle").is_some(), "the picker's record stands");

        push(&plan, Some(&conf), &ca, device.path());
        assert!(device.path().join(HID_CONF).exists());
        assert!(device.path().join("documents/Karyll.sh").exists());
    }

    #[test]
    fn uninstalling_what_is_not_there_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let plan = plan_with_karyll(tmp.path(), karyll.path());
        let report = uninstall(plan.app("karyll").unwrap(), &ms(device.path())).unwrap();
        assert!(report.removed.is_empty());
        assert!(report.errors.is_empty());
    }

    /// A per-row install rewrites that app's record and leaves every other
    /// app's alone, or the next full push would forget what it knew.
    #[test]
    fn a_per_app_push_leaves_the_other_apps_receipts_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let ca = ca_path(tmp.path());
        let conf = make_conf();
        let plan = plan_with_karyll(tmp.path(), karyll.path());

        push(&plan, Some(&conf), &ca, device.path());
        push(&plan.only("karyll"), Some(&conf), &ca, device.path());

        let state = InstallState::read(&ms(device.path()));
        assert!(state.app("sidle").is_some(), "the picker's record survived");
        assert!(state.file("sidle", "extensions/sidle/bin/sidle").is_some());
        assert!(state.file("karyll", HID_CONF).is_some());
    }

    /// A cable push writes every path directly — nothing on a mounted device is
    /// executing — and clears any `.new` a Wi-Fi pull staged, so the applier one
    /// level up cannot swap a stale file over the fresh one.
    #[test]
    fn a_push_writes_staged_paths_directly_and_clears_their_pending_new() {
        let tmp = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let plan = make_install_plan(tmp.path());

        let bin_dir = device.path().join("extensions/sidle/bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("sidle.new"), b"stale-lan-stage").unwrap();
        std::fs::write(bin_dir.join("sidle.sh.new"), b"stale-lan-stage").unwrap();

        push(
            &plan,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            device.path(),
        );

        assert!(bin_dir.join("sidle").exists());
        assert!(bin_dir.join("sidle.sh").exists());
        assert!(!bin_dir.join("sidle.new").exists());
        assert!(
            !bin_dir.join("sidle.sh.new").exists(),
            "every staged path is cleared, not just the binary"
        );
    }

    /// The tab's row model: one entry per app, its own files only, its installed
    /// version read from what the last push recorded.
    #[test]
    fn per_app_rollup_scopes_each_app_to_its_own_files() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let plan = plan_with_karyll(tmp.path(), karyll.path());
        let source = make_source(tmp.path(), true);
        let conf = make_conf();
        let ca = ca_path(tmp.path());

        // Nothing installed yet.
        let status = status_of(&plan, &source, Some(&conf), &ca, device.path());
        let ids: Vec<&str> = status.apps.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["karyll", "sidle"]);
        for app in &status.apps {
            assert_eq!(app.overall, DeployOverall::NotInstalled);
            assert!(app.installed_version.is_none());
            assert!(app.write_count > 0);
        }

        push(&plan, Some(&conf), &ca, device.path());

        let status = status_of(&plan, &source, Some(&conf), &ca, device.path());
        for app in &status.apps {
            assert_eq!(app.overall, DeployOverall::InSync, "{}", app.id);
            assert_eq!(app.write_count, 0);
            assert_eq!(app.write_bytes, 0);
        }
        let karyll_row = status.apps.iter().find(|a| a.id == "karyll").unwrap();
        assert_eq!(karyll_row.installed_version.as_deref(), Some("0.2.4"));
        assert_eq!(karyll_row.name, "Karyll");

        // One app going stale leaves the other alone — the rows are per-app,
        // not a share of one number.
        std::fs::write(
            karyll
                .path()
                .join("deploy/out/extensions/karyll/bin/karyll"),
            b"rebuilt",
        )
        .unwrap();
        let plan = replan(tmp.path(), karyll.path());
        let status = status_of(&plan, &source, Some(&conf), &ca, device.path());
        let karyll_row = status.apps.iter().find(|a| a.id == "karyll").unwrap();
        assert_eq!(karyll_row.write_count, 1);
        assert_eq!(karyll_row.write_bytes, b"rebuilt".len() as u64);
        assert_eq!(
            status
                .apps
                .iter()
                .find(|a| a.id == "sidle")
                .unwrap()
                .overall,
            DeployOverall::InSync
        );
    }

    /// A plan narrowed to one app installs that app and nothing else. The
    /// picker's per-install slots — its trust root and its bearer token — do
    /// not ride along.
    #[test]
    fn a_single_app_install_carries_neither_the_ca_nor_the_conf() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let plan = plan_with_karyll(tmp.path(), karyll.path()).only("karyll");

        push(
            &plan,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            device.path(),
        );

        assert!(device.path().join("extensions/karyll/bin/karyll").exists());
        assert!(device.path().join("documents/Karyll.sh").exists());
        assert!(!device.path().join("extensions/sidle/etc/ca.pem").exists());
        assert!(
            !device
                .path()
                .join("extensions/sidle/etc/server.conf")
                .exists(),
            "a per-app install must not write another app's per-install bytes"
        );
        assert!(!device.path().join("extensions/sidle/bin/sidle").exists());
    }

    #[test]
    fn sha256_bytes_known_vector() {
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            sha256_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn detect_lan_ipv4_returns_something_or_none_without_panicking() {
        // Can't assert a specific IP in CI, just that the call is
        // side-effect-free and doesn't panic on either outcome.
        let _ = detect_lan_ipv4();
    }
}
