//! On-device app deploy — pushes every app in the fleet onto a jailbroken
//! Kindle over a cable: the picker (its armv7 binary, launcher wrapper, KUAL
//! metadata, the `documents/Sidle.sh` tile, a freshly rendered
//! `etc/server.conf` and the CA at `etc/ca.pem`), and whatever else is
//! registered — bokai, steb, karyll, kfxdedrm-fe.
//!
//! The file list is not written here. It comes from
//! [`crate::library::apps::DevicePlan`], which walks every app's mount-rooted
//! tree and carries each path's class. Adding a file to the deploy means
//! dropping it in the tree at the path it should land on; there is no slot to
//! add and no source-vs-device path mapping to keep in sync.
//!
//! Two slots are **not** in any tree, because their bytes are per-install
//! rather than per-repo: `etc/server.conf` is rendered live, and `etc/ca.pem`
//! is copied from the library root. The CA is the root the picker pins — its
//! only trust anchor, since the device client compiles in no public root set —
//! so a bundle that arrives without it leaves a device that cannot complete a
//! single handshake while looking perfectly installed.
//!
//! Every path carries a class. `sync` is written when its hash differs from the
//! source's; `seed` is written only when the path is absent on device, and is
//! never read to decide, which is what keeps a status check off 49 MB of
//! vendored files no update will ever write.
//!
//! The picker is not a KUAL app: `documents/Sidle.sh` is a jailbreak-hotfix
//! scriptlet the library indexes as a tile, and tapping it runs
//! `extensions/sidle/bin/sidle.sh` directly. That tile is the only front door;
//! KUAL does not run on this firmware, so `config.xml` and `menu.json` are
//! inert and dropping them would cost nothing.
//!
//! The button this module backs (`device_app_install` in `commands/device.rs`)
//! re-syncs every file in one click and is idempotent — content-hash equal
//! means skip. `etc/server.conf` is rendered on every push rather than
//! remembered, so a rotated `.server-token` cannot leave the picker holding a
//! stale bearer and getting `403` from `/list.json` with nothing in the UI to
//! say why.
//!
//! A cable push is authoritative and every path is a direct write: nothing on
//! the device is executing anything while it is mounted, so the `.new` staging
//! a Wi-Fi update needs does not apply. Any `.new` a previous LAN pull left is
//! deleted, or the launcher would swap a stale binary over the fresh one on the
//! next launch.
//!
//! Transport-agnostic: everything below drives the [`Transport`] trait,
//! so a deploy behaves identically over a mass-storage mount (KOA2) and
//! MTP (Colorsoft). Callers still refuse devices without a jailbreak's
//! `/extensions/` layout (e.g. stock Scribes).

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{TPath, Transport};
use crate::library::apps::{Apply, DevicePlan, FileClass, PathPolicy};

/// Cross-compile target the native binary is built for. Hard-coded
/// because the picker only runs on armv7l Kindles; if a different target ever
/// matters, that's a per-device problem, not a per-build one.
const NATIVE_TARGET_TRIPLE: &str = "armv7-unknown-linux-musleabihf";

/// Where the picker's binary lives inside the mount tree. Named here because
/// three things agree on it: the mirror it is staged into, the plan entry whose
/// absence means "not built yet", and the `app.json` rule that stages it.
const PICKER_BINARY_REL: &str = "extensions/sidle/bin/sidle";

/// The tree that ships with the desktop app: the `device/` mirror in a dev
/// checkout, the staged resources in a packaged one. It holds the picker and
/// bokai, and it is what [`crate::library::apps::plan`] composes with every
/// registered app.
///
/// `binary_path` is where the cross-compiled picker lands — `target/` on the
/// dev path, under a different name, because the desktop app already owns
/// `sidle` there. [`Self::stage_binary`] is what puts it in the mirror, so the
/// walk finds it at the path it installs to. Both are resolved at app startup
/// (see `state.rs`) and re-evaluated per status query, so a fresh `cargo build`
/// shows up without restarting the desktop app.
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
            // The bin target is `sidle-native` (the desktop app already owns
            // the `sidle` name in target/release); it ships on-device as
            // plain `sidle` via build.sh's copy rename.
            binary_path: repo
                .join("target")
                .join(NATIVE_TARGET_TRIPLE)
                .join("release")
                .join("sidle-native"),
        }
    }

    /// Packaged builds: the on-device source assets ride along as Tauri bundle
    /// resources instead of living in a dev checkout. build.sh stages them under
    /// `Contents/Resources/resources/device/`, reproducing the same mount mirror
    /// `from_workspace_root` points at, cross-compiled picker included — so the
    /// walk, `compute_status` and `stage_dist` behave identically dev vs
    /// packaged. `res_dir` is `app.path().resource_dir()`. `binary_path` is the
    /// mirror's own copy here, which makes [`Self::stage_binary`] a no-op; the
    /// "binary older than source" mtime hint silently no-ops too (no
    /// `sidle/native/src` tree alongside).
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
    /// newer, so the tree walk finds current bytes at the path they install to.
    ///
    /// The picker is the one file in the fleet a build leaves outside the tree
    /// that ships it: it is cross-compiled into `target/` under a different
    /// name. Everything else — bokai's binary, steb's, karyll's two — is
    /// already written to its mount path by its own `build.sh`. Doing this here
    /// rather than only in `build.sh` keeps the dev loop at one command:
    /// cross-build, push. Its `.build-ts` sidecar rides along, because the
    /// device compares that value against the one compiled into the binary and
    /// the two have to move together.
    ///
    /// Best-effort. A missing source binary is the "not built yet" case, which
    /// [`compute_status`] reports from the plan; a failed copy leaves whatever
    /// the mirror already had.
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
    /// `<binary_path>.build-ts` sidecar `build.sh` writes beside the cross-built
    /// picker. Feeds the manifest's `built_at` so the device can refuse a
    /// downgrade. `0` when the sidecar is absent (e.g. a bare `cargo build` that
    /// skipped `build.sh`), which disables the guard — the device then falls back
    /// to the sha-only check.
    pub fn build_ts(&self) -> u64 {
        let mut sidecar = self.binary_path.clone().into_os_string();
        sidecar.push(".build-ts");
        std::fs::read_to_string(PathBuf::from(sidecar))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }
}

/// Fields rendered into `etc/server.conf` on the device. Held as a
/// struct (rather than a single rendered String) so callers can also
/// surface the individual values to the UI for confirmation before the
/// user clicks Install.
#[derive(Debug, Clone, Serialize)]
pub struct ServerConfRender {
    pub host: String,
    pub port: u16,
    /// The Kindle's own USB iSerial (`DeviceInfo.serial`, read by the Mac at
    /// mount). The picker echoes it as `device_serial` in its `POST
    /// /sync/annotations` push, so the server keys the pushed annotations to
    /// this device — same per-device keying a USB sync uses. Resolved Mac-side
    /// (the device is mounted at install time) so the picker needs no on-device
    /// serial lookup.
    pub serial: String,
    pub token: String,
}

impl ServerConfRender {
    /// The bytes that should land at `etc/server.conf` on the device.
    /// Trailing newline is intentional and load-bearing: the staleness
    /// check is byte-equality, so omitting it would re-trigger "stale"
    /// every time a previously-installed conf is compared.
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
    /// Source bytes hash-match the on-device bytes — nothing to do.
    Synced,
    /// They differ. Hex sha256 of both surfaces so the UI can show a
    /// diff hint without re-reading the files.
    Stale {
        source_hash: String,
        device_hash: String,
    },
    /// File is missing on the device (clean install case). `source_hash` is
    /// absent for a `seed` path, whose whole point is that neither side is read
    /// to decide — hashing 49 MB of vendored files to fill a UI hint would undo
    /// the saving.
    Missing { source_hash: Option<String> },
    /// A `seed` path already on the device. Present, not compared: the
    /// on-device copy is the live one and the source's is a fresh install's
    /// starting point, so equal bytes were never the test.
    Seeded,
    /// File is missing on the *source* side. Only meaningful for the
    /// binary slot — UI surfaces "run `cargo build ...` first".
    SourceMissing,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeployFileStatus {
    /// Mount-relative device path, uniformly — `extensions/sidle/bin/sidle`,
    /// `documents/Sidle.sh`. The UI shows it verbatim, so it reads as the
    /// place the file actually lands.
    pub device_path: String,
    pub state: DeployFileState,
}

/// Headline summary the UI uses to label the button + status pill.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeployOverall {
    /// No Kindle connected at all. Section hidden. Constructed at the command
    /// layer (the device cell is empty), not by `compute_status` — which now
    /// runs against any connected device's transport, mass-storage or MTP.
    #[allow(dead_code)]
    DeviceDisconnected,
    /// Source binary doesn't exist — the user hasn't run `cargo
    /// build --release --target armv7-unknown-linux-musleabihf
    /// -p sidle-native` yet (or the rebuild failed). Other files may
    /// be checkable but the button is disabled until the binary is
    /// present.
    BinaryNotBuilt,
    /// Every device file is missing — first-time install.
    NotInstalled,
    /// At least one file differs. `stale_count` and `missing_count`
    /// add up to "files that would be written".
    Stale {
        stale_count: u32,
        missing_count: u32,
    },
    /// Every file is present and content-equal — button re-pushes
    /// anyway, but the pill reads "In sync".
    InSync,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeployStatus {
    pub overall: DeployOverall,
    pub files: Vec<DeployFileStatus>,
    /// mtime of the source binary, if it exists. Serialized as a Unix
    /// epoch milliseconds value so the frontend can render a relative
    /// timestamp without timezone gymnastics.
    pub binary_mtime_ms: Option<u64>,
    /// mtime of the newest source file under `sidle/native/src/`.
    /// Surfaces "your binary is older than your source" *before* the
    /// user clicks anything — staleness against the device is one
    /// thing, but if your binary is also pre-dating your code edits
    /// you'd push a no-op.
    pub native_source_mtime_ms: Option<u64>,
}

/// Per-file outcome of an install. Distinct from `DeployFileState`
/// because the relevant after-state is "did we write or skip", not
/// "is it stale" (it isn't, by the time we report).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeployFileInstallResult {
    /// Wrote new bytes (either because Missing or Stale).
    Wrote { device_path: String },
    /// Skipped — already content-equal to the source.
    Skipped { device_path: String },
    /// Nothing to write: the bytes for this slot could not be produced. Only
    /// `etc/server.conf` reaches this, and only with no LAN address or no
    /// server token. Distinct from `Skipped` because the device copy is *not*
    /// known to match — it was never compared — and distinct from `Failed`
    /// because nothing went wrong and the rest of the push is authoritative.
    SourceMissing { device_path: String },
    /// Write failed; the rest of the install continues so a partial
    /// failure leaves a recoverable state.
    Failed { device_path: String, error: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct DeployInstallReport {
    pub results: Vec<DeployFileInstallResult>,
}

/// Where a slot's bytes come from on this machine.
enum Source {
    File(PathBuf),
    /// Bytes computed live rather than read from a file. `None` says the
    /// inputs to compute them are unavailable, and the slot reports
    /// [`DeployFileState::SourceMissing`] rather than failing the deploy. Only
    /// `etc/server.conf` needs it, and it is exactly what a machine with no
    /// routable LAN address needs from a cable push: every other slot is
    /// independent of the address, and pushing them is the whole point.
    Rendered(Option<String>),
}

/// One file in the deploy. `device_rel` is **mount-relative** — the same string
/// that keys the manifest entry and the served route.
struct Slot {
    device_rel: String,
    source: Source,
    policy: PathPolicy,
}

impl Slot {
    fn tpath(&self) -> TPath {
        TPath::parse(&self.device_rel)
    }
}

/// A `sync`/`direct` slot — what everything outside an app tree is.
fn plain(device_rel: &str, source: Source) -> Slot {
    Slot {
        device_rel: device_rel.to_string(),
        source,
        policy: PathPolicy {
            class: FileClass::Sync,
            seed_gen: 0,
            apply: Apply::Direct,
        },
    }
}

/// Every file this push would write: the composed tree, plus the two slots
/// whose bytes are per-install rather than per-repo.
///
/// A slot's `apply` is carried but not acted on. Over a cable nothing on the
/// device is executing, so a `staged` path is written directly — see
/// [`install_all`], which also clears any `.new` a LAN pull left behind.
fn slots(plan: &DevicePlan, conf: Option<&ServerConfRender>, ca_cert: &Path) -> Vec<Slot> {
    let mut out: Vec<Slot> = plan
        .files
        .iter()
        .map(|f| Slot {
            device_rel: f.path.clone(),
            source: Source::File(f.source.clone()),
            policy: f.policy,
        })
        .collect();

    // Not in any tree: the CA lives in the library root. This is the root the
    // picker pins — its *only* trust anchor, since the device client is built
    // with no public root set compiled in — so a device without it cannot
    // complete a handshake at all. Pushed as a file rather than rendered bytes
    // because that is what it is; the caller guarantees it exists.
    out.push(plain(
        "extensions/sidle/etc/ca.pem",
        Source::File(ca_cert.to_path_buf()),
    ));
    // Not in any tree: per-install secret, rendered live. The mirror holds only
    // `etc/server.conf.example`, which its `app.json` classes `ignore`. `None`
    // when there is no LAN address or no server token to render — the slot then
    // reports `SourceMissing` and everything else installs anyway.
    out.push(plain(
        "extensions/sidle/etc/server.conf",
        Source::Rendered(conf.map(ServerConfRender::render)),
    ));

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

/// Hex sha256 of the on-device bytes at `path`, or `None` when the object is
/// absent — the transport analog of [`sha256_file_opt`]. Drives the same
/// staleness/idempotency logic over either transport (mass-storage `std::fs` or
/// MTP USB), so the deploy behaves identically on both.
fn device_sha_opt(transport: &dyn Transport, path: &TPath) -> Result<Option<String>> {
    if transport.exists(path)? {
        Ok(Some(sha256_bytes(&transport.read(path)?)))
    } else {
        Ok(None)
    }
}

/// Compute the per-file staleness given a source layout, a rendered
/// server.conf, and the connected device's [`Transport`] (mass-storage volume
/// or MTP — the deploy is identical over either).
///
/// `conf` is `None` when its inputs are unavailable (no routable LAN address,
/// or no server token): the `etc/server.conf` slot reports
/// [`DeployFileState::SourceMissing`] and every other slot is checked
/// normally, so the status matches what [`install_all`] would then push.
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
    let mut files = Vec::with_capacity(slots.len());

    for slot in slots {
        // A `seed` path is decided by existence alone — neither side is read.
        // This is the whole reason the class exists: karyll's 100 vendored
        // files are 49 MB that no update writes, and hashing them over MTP
        // would make a status check a transfer.
        if slot.policy.class == FileClass::Seed {
            let state = if transport.exists(&slot.tpath())? {
                DeployFileState::Seeded
            } else {
                DeployFileState::Missing { source_hash: None }
            };
            files.push(DeployFileStatus {
                device_path: slot.device_rel,
                state,
            });
            continue;
        }

        let device_hash = device_sha_opt(transport, &slot.tpath())?;
        let state = match slot.source {
            Source::Rendered(Some(text)) => classify(sha256_bytes(text.as_bytes()), device_hash),
            Source::Rendered(None) => DeployFileState::SourceMissing,
            Source::File(path) => {
                // Two kinds of file land here: one from an app's tree, where a
                // miss means the tree changed under the walk, and the CA in the
                // library root, where it means nobody issued one. Name the path
                // and let it say which.
                let source_hash = sha256_file_opt(&path)?.ok_or_else(|| {
                    anyhow!(
                        "deploy source missing at {} — the tree changed under \
                         the walk, or TLS material was never issued",
                        path.display()
                    )
                })?;
                classify(source_hash, device_hash)
            }
        };

        files.push(DeployFileStatus {
            device_path: slot.device_rel,
            state,
        });
    }

    // Nothing to install without the picker's own binary. It reaches the plan
    // through the built-in tree, so its absence means the armv7 cross-build has
    // not run — the one prerequisite the user has to satisfy by hand.
    let overall = if !plan.files.iter().any(|f| f.path == PICKER_BINARY_REL) {
        DeployOverall::BinaryNotBuilt
    } else {
        summarize(&files)
    };

    let binary_mtime_ms = mtime_ms(&source.binary_path);
    let native_source_mtime_ms = newest_native_source_mtime_ms(source);

    Ok(DeployStatus {
        overall,
        files,
        binary_mtime_ms,
        native_source_mtime_ms,
    })
}

fn classify(source_hash: String, device_hash: Option<String>) -> DeployFileState {
    match device_hash {
        None => DeployFileState::Missing {
            source_hash: Some(source_hash),
        },
        Some(dh) if dh == source_hash => DeployFileState::Synced,
        Some(dh) => DeployFileState::Stale {
            source_hash,
            device_hash: dh,
        },
    }
}

fn summarize(files: &[DeployFileStatus]) -> DeployOverall {
    let mut stale = 0u32;
    let mut missing = 0u32;
    let mut synced = 0u32;
    for f in files {
        match f.state {
            // A seeded path is present and deliberately not compared, so it is
            // as done as a synced one.
            DeployFileState::Synced | DeployFileState::Seeded => synced += 1,
            DeployFileState::Stale { .. } => stale += 1,
            DeployFileState::Missing { .. } => missing += 1,
            DeployFileState::SourceMissing => {}
        }
    }
    if stale == 0 && missing == 0 {
        DeployOverall::InSync
    } else if synced == 0 && stale == 0 {
        DeployOverall::NotInstalled
    } else {
        DeployOverall::Stale {
            stale_count: stale,
            missing_count: missing,
        }
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
    // The native crate lives at `<repo>/sidle/native/src/`. We don't
    // have the repo root in DeploySource, so derive it from
    // `mount_dir` which is `<repo>/device`.
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

/// Install every slot. For each: skip if Synced, write-via-temp +
/// rename otherwise. On failure, record per-file and continue — a
/// partial install is recoverable on the next click.
///
/// `on_progress` fires once per file with its outcome. The Tauri
/// command wires this to `device-app:install-progress` events for live UI
/// updates; tests can pass `|_| {}`.
///
/// `conf` is `None` when `etc/server.conf` has no bytes to render — that slot
/// reports [`DeployFileInstallResult::SourceMissing`] and the other six are
/// written. A push must not be gated on the one slot that needs a LAN address,
/// because a device on a network that cannot carry one is precisely the device
/// that needs the cable.
pub fn install_all(
    plan: &DevicePlan,
    conf: Option<&ServerConfRender>,
    ca_cert: &Path,
    transport: &dyn Transport,
    mut on_progress: impl FnMut(&DeployFileInstallResult),
) -> Result<DeployInstallReport> {
    // No explicit mkdir: `Transport::write_atomic` creates the bin/ and etc/
    // parents on both transports (mass-storage `create_dir_all`, MTP
    // `ensure_folder`), so a fresh install needs no pre-step.
    let slots = slots(plan, conf, ca_cert);
    let mut results = Vec::with_capacity(slots.len());
    for slot in &slots {
        let result = install_one(transport, slot);
        on_progress(&result);
        results.push(result);
    }

    // A cable push is authoritative: what it just wrote supersedes any pending
    // LAN self-update staged as `<path>.new`. Leaving one would let the applier
    // one level up swap a stale file over the fresh one on the next launch.
    // Best-effort; absent is the norm.
    for path in plan.staged_paths() {
        let _ = transport.delete(&TPath::parse(&format!("{path}.new")));
    }

    Ok(DeployInstallReport { results })
}

fn install_one(transport: &dyn Transport, slot: &Slot) -> DeployFileInstallResult {
    let tpath = slot.tpath();

    // A `seed` path is planted once and left alone. The on-device copy is the
    // live one — a user may have edited it, and replacing a vendored stack
    // costs a re-download — so presence, not equal bytes, is the test.
    if slot.policy.class == FileClass::Seed && matches!(transport.exists(&tpath), Ok(true)) {
        return DeployFileInstallResult::Skipped {
            device_path: slot.device_rel.clone(),
        };
    }

    let bytes_result: Result<Vec<u8>> = match &slot.source {
        Source::Rendered(Some(text)) => Ok(text.as_bytes().to_vec()),
        Source::Rendered(None) => {
            return DeployFileInstallResult::SourceMissing {
                device_path: slot.device_rel.clone(),
            };
        }
        Source::File(path) => {
            std::fs::read(path).with_context(|| format!("read source {}", path.display()))
        }
    };

    let bytes = match bytes_result {
        Ok(b) => b,
        Err(e) => {
            return DeployFileInstallResult::Failed {
                device_path: slot.device_rel.clone(),
                error: format!("{e:#}"),
            };
        }
    };

    let source_hash = sha256_bytes(&bytes);
    if let Ok(Some(device_hash)) = device_sha_opt(transport, &tpath)
        && device_hash == source_hash
    {
        return DeployFileInstallResult::Skipped {
            device_path: slot.device_rel.clone(),
        };
    }

    if let Err(e) = transport.write_atomic(&tpath, &bytes) {
        return DeployFileInstallResult::Failed {
            device_path: slot.device_rel.clone(),
            error: format!("{e:#}"),
        };
    }
    DeployFileInstallResult::Wrote {
        device_path: slot.device_rel.clone(),
    }
}

fn atomic_write(dest: &Path, bytes: &[u8]) -> Result<()> {
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
/// Uses the "no-packet" UDP trick: `bind` + `connect` to a public-
/// internet address, then read `local_addr()`. The kernel picks the
/// interface it would route via, without actually sending anything.
/// Pure-std, no extra deps.
///
/// Returns `None` if there's no routable interface (e.g. fully
/// offline). Callers fall back to whatever HOST is currently in the
/// on-device server.conf, or prompt the user.
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
// LAN self-update staging (the `device-dist/` bundle the server serves)
// ----------------------------------------------------------------------------

/// One entry in the LAN self-update manifest — a single deployable file.
///
/// `name` is **bundle-relative** (`bin/sidle`): the picker saves it under
/// `<extensions/sidle>/<name>` and `sidle-server` serves it from
/// `<device-dist>/<name>`, so the one string keys the staged file, the served
/// route, and the on-device destination. It is deliberately NOT the slot's
/// mount-relative `device_rel`: a picker resolves `name` against the bundle
/// dir, so a mount-relative value would land the binary at
/// `EXT/extensions/sidle/bin/sidle`. Changing it means the currently-installed
/// picker can't self-update and needs a USB push to recover.
///
/// `sha256` is computed with the same [`sha256_file_opt`]/[`sha256_bytes`] the
/// USB push uses, so a LAN pull and a USB push report identical hashes (the
/// gate).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DistManifestEntry {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    /// Unix seconds the staged file was built — read from the `sidle.build-ts`
    /// sidecar `build.sh` writes beside the binary (and baked into the binary
    /// itself). The device refuses to self-update to an entry whose `built_at`
    /// isn't strictly newer than its own, so a stale `device-dist` can't downgrade
    /// it over Wi-Fi. `0` when the sidecar is absent (a bare `cargo build`).
    #[serde(default)]
    pub built_at: u64,
}

/// The `device-dist/manifest.json` contract. `sidle-server` mirrors a minimal
/// `Deserialize` view of this (it only needs `name` for the served-file
/// whitelist) rather than sharing the type across the crate boundary — the
/// same mirror-struct convention `SyncReport`/`DeviceImportReport` use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DistManifest {
    pub files: Vec<DistManifestEntry>,
}

/// Outcome of a [`stage_dist`] call, surfaced so callers can log/skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageOutcome {
    /// Copied fresh bytes (staged copy was missing or older than the source).
    Staged,
    /// Staged copy already current (source not newer + manifest present).
    UpToDate,
    /// Source binary doesn't exist yet — run the armv7 cross-build first.
    SourceMissing,
}

/// Stage the deployable picker binary + a `manifest.json` into `dist_dir`
/// (`<data-dir>/device-dist/`), where `sidle-server` serves it over `/device/...`
/// for the picker's untethered in-app Update (LAN self-update) pull.
///
/// v1 stages exactly one file: `bin/sidle`, the armv7 picker. **mtime-gated** —
/// re-copies only when the freshly cross-built `DeploySource.binary_path` is newer
/// than the staged copy (or the manifest is absent), so the startup /
/// popover-open / post-install calls are near-instant no-ops once warm. Writes
/// via [`atomic_write`] (temp + rename) so a concurrent server read never sees a
/// half-written binary or manifest.
///
/// Returns [`StageOutcome::SourceMissing`] (not an error) when the binary hasn't
/// been built — the same graceful "run cargo build first" signal `compute_status`
/// gives, so a startup call on a fresh checkout doesn't fail bootstrap.
pub fn stage_dist(source: &DeploySource, dist_dir: &Path) -> Result<StageOutcome> {
    let src_bin = source.binary_path.as_path();
    let Some(src_mtime) = mtime_ms(src_bin) else {
        return Ok(StageOutcome::SourceMissing);
    };

    let dest_bin = dist_dir.join("bin").join("sidle");
    let manifest_path = dist_dir.join("manifest.json");

    // mtime-gate: skip the copy when the staged binary is already at least as new
    // as the source AND the manifest is present (a torn prior run could have
    // written the binary but not the manifest).
    if manifest_path.exists()
        && let Some(dest_mtime) = mtime_ms(&dest_bin)
        && dest_mtime >= src_mtime
    {
        return Ok(StageOutcome::UpToDate);
    }

    let bytes = std::fs::read(src_bin)
        .with_context(|| format!("read source binary {}", src_bin.display()))?;
    let sha256 = sha256_bytes(&bytes);
    let size = bytes.len() as u64;

    std::fs::create_dir_all(dist_dir.join("bin"))
        .with_context(|| format!("mkdir {}", dist_dir.join("bin").display()))?;
    atomic_write(&dest_bin, &bytes)?;

    let manifest = DistManifest {
        files: vec![DistManifestEntry {
            name: "bin/sidle".to_string(),
            sha256,
            size,
            built_at: source.build_ts(),
        }],
    };
    let json = serde_json::to_vec_pretty(&manifest).context("serialize dist manifest")?;
    atomic_write(&manifest_path, &json)?;

    Ok(StageOutcome::Staged)
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
            &mirror.join("extensions/sidle/app.json"),
            br#"{"schema":1,"id":"sidle","name":"Sidle","version":"0.1.9",
                 "tile":"documents/Sidle.sh","pidof":"sidle",
                 "paths":[{"match":"extensions/sidle/bin/sidle","apply":"staged"},
                          {"match":"extensions/sidle/bin/sidle.sh","apply":"staged"},
                          {"match":"extensions/sidle/bin/sidle.build-ts","class":"ignore"},
                          {"match":"extensions/sidle/etc/server.conf.example",
                           "class":"ignore"}]}"#,
        );
        write_file(&mirror.join("extensions/sidle/config.xml"), b"<config/>");
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
            b"#!/bin/sh\n# Name: Sidle\nexec sidle\n",
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

    #[test]
    fn status_not_installed_when_device_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, plan) = make_both(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();

        let status = compute_status(
            &plan,
            &source,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            &ms(device.path()),
        )
        .unwrap();
        assert_eq!(status.overall, DeployOverall::NotInstalled);
        assert!(
            status
                .files
                .iter()
                .all(|f| matches!(f.state, DeployFileState::Missing { .. }))
        );
    }

    #[test]
    fn status_in_sync_after_install() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, plan) = make_both(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();
        let conf = make_conf();

        install_all(
            &plan,
            Some(&conf),
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();
        let status = compute_status(
            &plan,
            &source,
            Some(&conf),
            &ca_path(tmp.path()),
            &ms(device.path()),
        )
        .unwrap();
        assert_eq!(status.overall, DeployOverall::InSync);
    }

    #[test]
    fn status_stale_when_token_rotated() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, plan) = make_both(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();

        // First install with one token.
        let conf1 = make_conf();
        install_all(
            &plan,
            Some(&conf1),
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();

        // Token rotates; everything else identical.
        let mut conf2 = conf1.clone();
        conf2.token = "rotated".into();
        let status = compute_status(
            &plan,
            &source,
            Some(&conf2),
            &ca_path(tmp.path()),
            &ms(device.path()),
        )
        .unwrap();

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

        let conf_status = status
            .files
            .iter()
            .find(|f| f.device_path == "extensions/sidle/etc/server.conf")
            .unwrap();
        assert!(matches!(conf_status.state, DeployFileState::Stale { .. }));
    }

    #[test]
    fn status_binary_not_built_when_missing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, plan) = make_both(tmp.path(), false); // no binary
        let device = tempfile::tempdir().unwrap();

        let status = compute_status(
            &plan,
            &source,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            &ms(device.path()),
        )
        .unwrap();
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

        let report = install_all(
            &plan,
            None,
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();

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
    /// excludes that state, so the other six being current reads "In sync"
    /// rather than a permanent one-file-stale that no push can clear.
    #[test]
    fn status_without_conf_inputs_matches_what_install_would_do() {
        let tmp = tempfile::tempdir().unwrap();
        let (source, plan) = make_both(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();

        install_all(
            &plan,
            None,
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();
        let status = compute_status(
            &plan,
            &source,
            None,
            &ca_path(tmp.path()),
            &ms(device.path()),
        )
        .unwrap();

        assert_eq!(status.overall, DeployOverall::InSync);
        let conf_status = status
            .files
            .iter()
            .find(|f| f.device_path == "extensions/sidle/etc/server.conf")
            .unwrap();
        assert_eq!(conf_status.state, DeployFileState::SourceMissing);
    }

    #[test]
    fn install_skips_synced_files() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = make_install_plan(tmp.path());
        let device = tempfile::tempdir().unwrap();
        let conf = make_conf();

        install_all(
            &plan,
            Some(&conf),
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();
        let report = install_all(
            &plan,
            Some(&conf),
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();

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

        let conf1 = make_conf();
        install_all(
            &plan,
            Some(&conf1),
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();

        let mut conf2 = conf1.clone();
        conf2.token = "rotated".into();
        let report = install_all(
            &plan,
            Some(&conf2),
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();

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

        install_all(
            &plan,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();
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

        install_all(&plan, Some(&make_conf()), &ca, &ms(device.path()), |_| {}).unwrap();

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
            |_| events += 1,
        )
        .unwrap();
        // extensions/sidle/{app.json, bin/sidle, bin/sidle.sh, config.xml,
        // menu.json, etc/server.conf, etc/ca.pem} + documents/Sidle.sh
        assert_eq!(events, 8);
    }

    /// Register a karyll-shaped app beside the built-in tree: a tile, a binary,
    /// and a vendored subtree classed `seed`.
    fn karyll_row(repo: &Path) -> crate::library::db::AppSourceRow {
        let out = repo.join("deploy").join("out");
        write_file(
            &out.join("extensions/karyll/app.json"),
            br#"{"schema":1,"id":"karyll","name":"Karyll","version":"0.2.4",
                 "tile":"documents/Karyll.sh",
                 "paths":[{"match":"extensions/karyll/hid/","class":"seed","seed_gen":1}]}"#,
        );
        write_file(&out.join("extensions/karyll/bin/karyll"), b"armhf");
        write_file(&out.join("extensions/karyll/hid/config.ini"), b"[device]\n");
        write_file(&out.join("documents/Karyll.sh"), b"# Name: Karyll\n");
        crate::library::db::AppSourceRow {
            id: "karyll".into(),
            source_kind: crate::library::db::APP_SOURCE_LOCAL.into(),
            source: repo.display().to_string(),
            root: out.display().to_string(),
            added_at: 0,
        }
    }

    fn plan_with_karyll(repo: &Path, karyll: &Path) -> crate::library::apps::DevicePlan {
        let source = make_source(repo, true);
        source.stage_binary().unwrap();
        crate::library::apps::plan_from(&source.mount_dir, &[karyll_row(karyll)])
    }

    /// One cable push installs every app, not just the picker.
    #[test]
    fn a_registered_app_installs_over_the_same_cable() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let plan = plan_with_karyll(tmp.path(), karyll.path());

        install_all(
            &plan,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();

        for landed in [
            "extensions/sidle/bin/sidle",
            "extensions/sidle/etc/ca.pem",
            "extensions/karyll/bin/karyll",
            "extensions/karyll/hid/config.ini",
            "documents/Karyll.sh",
            "documents/Sidle.sh",
        ] {
            assert!(
                device.path().join(landed).exists(),
                "{landed} never arrived"
            );
        }
    }

    /// A `seed` path is planted once. The on-device copy is the live one — a
    /// user may have set `[device] name` in it — so a later push must leave it
    /// exactly as it found it, even though the source's bytes differ.
    #[test]
    fn a_seed_file_is_never_overwritten_once_it_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let plan = plan_with_karyll(tmp.path(), karyll.path());
        let conf = make_conf();
        let ca = ca_path(tmp.path());

        install_all(&plan, Some(&conf), &ca, &ms(device.path()), |_| {}).unwrap();

        let on_device = device.path().join("extensions/karyll/hid/config.ini");
        std::fs::write(&on_device, b"[device]\nname = My-Keyboard\n").unwrap();

        let report = install_all(&plan, Some(&conf), &ca, &ms(device.path()), |_| {}).unwrap();
        assert_eq!(
            std::fs::read(&on_device).unwrap(),
            b"[device]\nname = My-Keyboard\n",
            "an edited seed file must survive a re-push"
        );
        assert!(matches!(
            report
                .results
                .iter()
                .find(|r| matches!(r,
                    DeployFileInstallResult::Skipped { device_path }
                        | DeployFileInstallResult::Wrote { device_path }
                        if device_path == "extensions/karyll/hid/config.ini"))
                .unwrap(),
            DeployFileInstallResult::Skipped { .. }
        ));
    }

    /// A seed path the device does not have yet is a first install, and gets
    /// written like anything else.
    #[test]
    fn a_seed_file_is_written_when_the_device_lacks_it() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let plan = plan_with_karyll(tmp.path(), karyll.path());

        install_all(
            &plan,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();
        assert_eq!(
            std::fs::read(device.path().join("extensions/karyll/hid/config.ini")).unwrap(),
            b"[device]\n"
        );
    }

    /// The saving the class exists for: a seeded path is reported from its
    /// existence alone, and its bytes are never read off the device. Against
    /// karyll's real tree that is 100 files and 49 MB a status check skips.
    #[test]
    fn a_seeded_path_is_reported_without_reading_either_side() {
        let tmp = tempfile::tempdir().unwrap();
        let karyll = tempfile::tempdir().unwrap();
        let device = tempfile::tempdir().unwrap();
        let plan = plan_with_karyll(tmp.path(), karyll.path());
        let source = make_source(tmp.path(), true);
        let conf = make_conf();
        let ca = ca_path(tmp.path());

        install_all(&plan, Some(&conf), &ca, &ms(device.path()), |_| {}).unwrap();
        // Diverge the on-device copy. A `sync` path would read Stale; this one
        // is not compared at all.
        std::fs::write(
            device.path().join("extensions/karyll/hid/config.ini"),
            b"edited on device\n",
        )
        .unwrap();

        let status = compute_status(&plan, &source, Some(&conf), &ca, &ms(device.path())).unwrap();
        let hid = status
            .files
            .iter()
            .find(|f| f.device_path == "extensions/karyll/hid/config.ini")
            .unwrap();
        assert_eq!(hid.state, DeployFileState::Seeded);
        assert_eq!(
            status.overall,
            DeployOverall::InSync,
            "a seeded path that differs is not something to push"
        );
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

        install_all(
            &plan,
            Some(&make_conf()),
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();

        assert!(bin_dir.join("sidle").exists());
        assert!(bin_dir.join("sidle.sh").exists());
        assert!(!bin_dir.join("sidle.new").exists());
        assert!(
            !bin_dir.join("sidle.sh.new").exists(),
            "every staged path is cleared, not just the binary"
        );
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

    #[test]
    fn stage_dist_writes_binary_and_matching_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let source = make_source(tmp.path(), true);
        let dist = tempfile::tempdir().unwrap();

        assert_eq!(
            stage_dist(&source, dist.path()).unwrap(),
            StageOutcome::Staged
        );

        let staged = dist.path().join("bin/sidle");
        let src_bytes = std::fs::read(&source.binary_path).unwrap();
        assert_eq!(
            std::fs::read(&staged).unwrap(),
            src_bytes,
            "staged == source bytes"
        );

        let manifest: DistManifest =
            serde_json::from_slice(&std::fs::read(dist.path().join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest.files.len(), 1);
        let entry = &manifest.files[0];
        // The name is the device-relative slot path, and the hash matches what
        // the USB push would report for the very same bytes (the LAN==USB gate).
        assert_eq!(entry.name, "bin/sidle");
        assert_eq!(entry.sha256, sha256_bytes(&src_bytes));
        assert_eq!(entry.size, src_bytes.len() as u64);
    }

    #[test]
    fn stage_dist_source_missing_is_graceful() {
        let tmp = tempfile::tempdir().unwrap();
        let source = make_source(tmp.path(), false); // binary not built
        let dist = tempfile::tempdir().unwrap();

        assert_eq!(
            stage_dist(&source, dist.path()).unwrap(),
            StageOutcome::SourceMissing
        );
        assert!(!dist.path().join("manifest.json").exists());
        assert!(!dist.path().join("bin/sidle").exists());
    }

    #[test]
    fn stage_dist_is_mtime_gated_and_restages_when_manifest_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let source = make_source(tmp.path(), true);
        let dist = tempfile::tempdir().unwrap();

        assert_eq!(
            stage_dist(&source, dist.path()).unwrap(),
            StageOutcome::Staged
        );
        // Source unchanged + manifest present → near-instant no-op.
        assert_eq!(
            stage_dist(&source, dist.path()).unwrap(),
            StageOutcome::UpToDate
        );
        // A torn prior run (binary written, manifest lost) must re-stage.
        std::fs::remove_file(dist.path().join("manifest.json")).unwrap();
        assert_eq!(
            stage_dist(&source, dist.path()).unwrap(),
            StageOutcome::Staged
        );
    }
}
