//! On-device app deploy — pushes everything the native picker needs onto a
//! jailbroken Kindle: the armv7 binary, the launcher wrapper, a freshly
//! rendered `etc/server.conf`, the CA at `etc/ca.pem`, the KUAL menu entry
//! (`config.xml`, `menu.json`), and the one-tap launcher tile at
//! `documents/Sidle.sh`.
//!
//! The in-repo `device/` directory is a literal mirror of the mount, so a
//! slot's source path and its device path are the same relative string —
//! `device/documents/Sidle.sh` → `<mount>/documents/Sidle.sh`. Adding a file
//! to the deploy means dropping it in `device/` at the path it should land on
//! and adding one [`Slot`]; there is no source-vs-device path mapping to keep
//! in sync.
//!
//! Two slots are **not** mirrored, because their bytes are per-install rather
//! than per-repo: `etc/server.conf` is rendered live, and `etc/ca.pem` is
//! copied from the library root. The CA is the root the picker pins — its only
//! trust anchor, since the device client compiles in no public root set — so a
//! bundle that arrives without it leaves a device that cannot complete a single
//! handshake while looking perfectly installed.
//!
//! The picker is not a KUAL app: `documents/Sidle.sh` is a jailbreak-hotfix
//! scriptlet the library indexes as a tile, and tapping it runs
//! `extensions/sidle/bin/sidle.sh` directly. That tile is the only front door;
//! KUAL does not run on this firmware, so `config.xml` and `menu.json` are
//! inert and dropping them would cost nothing.
//!
//! Why this exists: the manual workflow (`cargo build ... && cp ...`) was
//! easy to forget after every native change, and `etc/server.conf` would
//! silently fall out of sync whenever sidle-server rotated its
//! `.server-token`, leaving the picker getting `403` from `/list.json`
//! with no UI breadcrumb. The button this module backs
//! (`device_app_install` in `commands/device.rs`) re-syncs every file in one
//! click and is idempotent — content-hash equal means skip.
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

/// Where on the device the extension bundle lives, relative to the mount.
/// Only [`bundle_tpath`] needs it — the slots carry mount-relative paths.
const DEVICE_BUNDLE_REL: &str = "extensions/sidle";

/// Cross-compile target the native binary is built for. Hard-coded
/// because the picker only runs on armv7l Kindles; if a different target ever
/// matters, that's a per-device problem, not a per-build one.
const NATIVE_TARGET_TRIPLE: &str = "armv7-unknown-linux-musleabihf";

/// Where on the desktop side each piece of the deploy comes from. The
/// `mount_dir` is the in-repo `device/` tree — a mirror of the Kindle's mount
/// root, so every static slot reads from `mount_dir.join(<its device path>)`.
/// The `binary_path` is the developer's freshly cross-compiled native binary,
/// which lives in `target/` rather than the mirror. Both are resolved at app
/// startup (see `state.rs`) and re-evaluated per status query so a fresh
/// `cargo build` shows up without restarting the desktop app.
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
    /// `from_workspace_root` points at — plus the cross-compiled armv7 picker at
    /// `native/sidle` — so `slots`, `compute_status`, and `stage_dist` behave
    /// identically dev vs packaged. `res_dir` is `app.path().resource_dir()`.
    /// The "binary older than source" mtime hint silently no-ops here (no
    /// `sidle/native/src` tree alongside).
    pub fn from_resource_root(res_dir: &Path) -> Self {
        let staged = res_dir.join("resources").join("device");
        Self {
            binary_path: staged.join("native").join("sidle"),
            mount_dir: staged,
        }
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
    /// File is missing on the device (clean install case).
    Missing { source_hash: String },
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
    /// Write failed; the rest of the install continues so a partial
    /// failure leaves a recoverable state.
    Failed { device_path: String, error: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct DeployInstallReport {
    pub results: Vec<DeployFileInstallResult>,
}

/// File-slot specs: where each piece comes from on the source side,
/// where it lands on the device. The `server.conf` slot has a special
/// `Source::Rendered` variant because its bytes are computed live, not
/// read from a file.
enum Source<'a> {
    File(PathBuf),
    Rendered(String),
    /// For the binary: same as File, but a missing source path
    /// surfaces as `SourceMissing` instead of an error.
    BinaryFile(&'a Path),
}

/// One file in the deploy. `device_rel` is **mount-relative** and doubles as
/// the path under the in-repo `device/` mirror, which is why a static slot
/// needs nothing but its own path — see [`mirrored`].
struct Slot<'a> {
    device_rel: &'static str,
    source: Source<'a>,
}

impl Slot<'_> {
    fn tpath(&self) -> TPath {
        TPath::parse(self.device_rel)
    }
}

/// A slot whose bytes come from the `device/` mirror verbatim: source path and
/// device path are the same relative string.
fn mirrored(source: &DeploySource, device_rel: &'static str) -> Slot<'static> {
    Slot {
        device_rel,
        source: Source::File(source.mount_dir.join(device_rel)),
    }
}

fn slots<'a>(source: &'a DeploySource, conf: &ServerConfRender, ca_cert: &Path) -> Vec<Slot<'a>> {
    vec![
        // Not mirrored: the picker is cross-compiled into `target/`, and ships
        // on-device under its short name.
        Slot {
            device_rel: "extensions/sidle/bin/sidle",
            source: Source::BinaryFile(source.binary_path.as_path()),
        },
        // Not mirrored: the CA lives in the library root, not the repo, and is
        // per-install material like `server.conf`. This is the root the picker
        // pins — its *only* trust anchor, since the device client is built with
        // no public root set compiled in — so a device without it cannot
        // complete a handshake at all. Pushed as a file rather than rendered
        // bytes because that is what it is; the caller guarantees it exists.
        Slot {
            device_rel: "extensions/sidle/etc/ca.pem",
            source: Source::File(ca_cert.to_path_buf()),
        },
        mirrored(source, "extensions/sidle/bin/sidle.sh"),
        // KUAL menu metadata, inert on this firmware. Pushed so a device that
        // ever gains a menu finds a current entry rather than a stale one.
        mirrored(source, "extensions/sidle/config.xml"),
        mirrored(source, "extensions/sidle/menu.json"),
        // Not mirrored: per-install secret, rendered live. The mirror holds
        // only `etc/server.conf.example`, which is never pushed.
        Slot {
            device_rel: "extensions/sidle/etc/server.conf",
            source: Source::Rendered(conf.render()),
        },
        // The one-tap launcher tile, and the app's primary front door. The
        // hotfix only indexes tiles from `documents/`, so this one sits at the
        // mount root rather than in the bundle. Its `# Icon:` header is
        // generated — see `device/make-tile.sh`.
        mirrored(source, "documents/Sidle.sh"),
    ]
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

/// Mount-relative [`TPath`] for something inside the extension bundle
/// (`bin/sidle.new` → `extensions/sidle/bin/sidle.new`). Slots don't need this
/// — they already carry mount-relative paths — but the staged-update cleanup
/// in [`install_all`] refers to a file that is not a slot.
fn bundle_tpath(bundle_rel: &str) -> TPath {
    TPath::parse(&format!("{DEVICE_BUNDLE_REL}/{bundle_rel}"))
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
/// Does no writes, no network. Reads the source side off the host filesystem
/// and the device side through `transport`.
pub fn compute_status(
    source: &DeploySource,
    conf: &ServerConfRender,
    ca_cert: &Path,
    transport: &dyn Transport,
) -> Result<DeployStatus> {
    let mut files = Vec::with_capacity(8);
    let mut binary_missing = false;

    for slot in slots(source, conf, ca_cert) {
        let device_hash = device_sha_opt(transport, &slot.tpath())?;

        let state = match slot.source {
            Source::Rendered(text) => {
                let source_hash = sha256_bytes(text.as_bytes());
                classify(source_hash, device_hash)
            }
            Source::File(path) => {
                // Two kinds of file land here: the `device/` mirror, where a
                // miss means repo layout drift, and the CA in the library root,
                // where it means nobody issued one. Name the path and let it say
                // which rather than asserting the mirror.
                let source_hash = sha256_file_opt(&path)?.ok_or_else(|| {
                    anyhow!(
                        "deploy source missing at {} — repo layout drift, or TLS \
                         material never issued",
                        path.display()
                    )
                })?;
                classify(source_hash, device_hash)
            }
            Source::BinaryFile(path) => match sha256_file_opt(path)? {
                Some(source_hash) => classify(source_hash, device_hash),
                None => {
                    binary_missing = true;
                    DeployFileState::SourceMissing
                }
            },
        };

        files.push(DeployFileStatus {
            device_path: slot.device_rel.to_string(),
            state,
        });
    }

    let overall = if binary_missing {
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
        None => DeployFileState::Missing { source_hash },
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
            DeployFileState::Synced => synced += 1,
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
pub fn install_all(
    source: &DeploySource,
    conf: &ServerConfRender,
    ca_cert: &Path,
    transport: &dyn Transport,
    mut on_progress: impl FnMut(&DeployFileInstallResult),
) -> Result<DeployInstallReport> {
    // No explicit mkdir: `Transport::write_atomic` creates the bin/ and etc/
    // parents on both transports (mass-storage `create_dir_all`, MTP
    // `ensure_folder`), so a fresh install needs no pre-step.
    let mut results = Vec::with_capacity(8);
    for slot in slots(source, conf, ca_cert) {
        let result = install_one(transport, &slot);
        on_progress(&result);
        results.push(result);
    }

    // A direct push is authoritative: the `bin/sidle` we just wrote supersedes any
    // pending LAN self-update staged as `bin/sidle.new` by an earlier "Update
    // over Wi-Fi". Leaving the stale `.new` would let the launcher's
    // unconditional swap (`sidle.sh`) clobber this freshly-pushed binary on the
    // next launch — a silent regression. Best-effort; absent is the norm.
    let _ = transport.delete(&bundle_tpath("bin/sidle.new"));

    Ok(DeployInstallReport { results })
}

fn install_one(transport: &dyn Transport, slot: &Slot<'_>) -> DeployFileInstallResult {
    let bytes_result: Result<Vec<u8>> = match &slot.source {
        Source::Rendered(text) => Ok(text.as_bytes().to_vec()),
        Source::File(path) => {
            std::fs::read(path).with_context(|| format!("read source {}", path.display()))
        }
        Source::BinaryFile(path) => {
            std::fs::read(path).with_context(|| format!("read binary {}", path.display()))
        }
    };

    let bytes = match bytes_result {
        Ok(b) => b,
        Err(e) => {
            return DeployFileInstallResult::Failed {
                device_path: slot.device_rel.to_string(),
                error: format!("{e:#}"),
            };
        }
    };

    let tpath = slot.tpath();
    let source_hash = sha256_bytes(&bytes);
    if let Ok(Some(device_hash)) = device_sha_opt(transport, &tpath)
        && device_hash == source_hash
    {
        return DeployFileInstallResult::Skipped {
            device_path: slot.device_rel.to_string(),
        };
    }

    if let Err(e) = transport.write_atomic(&tpath, &bytes) {
        return DeployFileInstallResult::Failed {
            device_path: slot.device_rel.to_string(),
            error: format!("{e:#}"),
        };
    }
    DeployFileInstallResult::Wrote {
        device_path: slot.device_rel.to_string(),
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

    /// Build a minimal `device/` mirror under `repo_root` — every static slot
    /// at the same relative path it lands on the device — and optionally
    /// `<repo>/target/.../release/sidle-native`.
    fn make_source(repo: &Path, include_binary: bool) -> DeploySource {
        let mirror = repo.join("device");
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

    #[test]
    fn status_not_installed_when_device_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let source = make_source(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();

        let status = compute_status(
            &source,
            &make_conf(),
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
        let source = make_source(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();
        let conf = make_conf();

        install_all(
            &source,
            &conf,
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();
        let status =
            compute_status(&source, &conf, &ca_path(tmp.path()), &ms(device.path())).unwrap();
        assert_eq!(status.overall, DeployOverall::InSync);
    }

    #[test]
    fn status_stale_when_token_rotated() {
        let tmp = tempfile::tempdir().unwrap();
        let source = make_source(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();

        // First install with one token.
        let conf1 = make_conf();
        install_all(
            &source,
            &conf1,
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();

        // Token rotates; everything else identical.
        let mut conf2 = conf1.clone();
        conf2.token = "rotated".into();
        let status =
            compute_status(&source, &conf2, &ca_path(tmp.path()), &ms(device.path())).unwrap();

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
        let source = make_source(tmp.path(), false); // no binary
        let device = tempfile::tempdir().unwrap();

        let status = compute_status(
            &source,
            &make_conf(),
            &ca_path(tmp.path()),
            &ms(device.path()),
        )
        .unwrap();
        assert_eq!(status.overall, DeployOverall::BinaryNotBuilt);
        let bin = status
            .files
            .iter()
            .find(|f| f.device_path == "extensions/sidle/bin/sidle")
            .unwrap();
        assert_eq!(bin.state, DeployFileState::SourceMissing);
    }

    #[test]
    fn install_skips_synced_files() {
        let tmp = tempfile::tempdir().unwrap();
        let source = make_source(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();
        let conf = make_conf();

        install_all(
            &source,
            &conf,
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();
        let report = install_all(
            &source,
            &conf,
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
        let source = make_source(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();

        let conf1 = make_conf();
        install_all(
            &source,
            &conf1,
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();

        let mut conf2 = conf1.clone();
        conf2.token = "rotated".into();
        let report = install_all(
            &source,
            &conf2,
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
        let source = make_source(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();

        install_all(
            &source,
            &make_conf(),
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
        let source = make_source(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();
        let ca = ca_path(tmp.path());

        install_all(&source, &make_conf(), &ca, &ms(device.path()), |_| {}).unwrap();

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
        let source = make_source(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();

        let mut events = 0;
        install_all(
            &source,
            &make_conf(),
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| events += 1,
        )
        .unwrap();
        // extensions/sidle/{bin/sidle, bin/sidle.sh, config.xml, menu.json,
        // etc/server.conf, etc/ca.pem} + documents/Sidle.sh
        assert_eq!(events, 7);
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
    fn install_all_clears_stale_sidle_new() {
        let tmp = tempfile::tempdir().unwrap();
        let source = make_source(tmp.path(), true);
        let device = tempfile::tempdir().unwrap();

        // A pending LAN self-update staged on the device before this USB push.
        let bin_dir = device.path().join("extensions/sidle/bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let stale = bin_dir.join("sidle.new");
        std::fs::write(&stale, b"stale-lan-stage").unwrap();

        install_all(
            &source,
            &make_conf(),
            &ca_path(tmp.path()),
            &ms(device.path()),
            |_| {},
        )
        .unwrap();

        assert!(
            !stale.exists(),
            "USB install must clear a pending bin/sidle.new"
        );
        assert!(
            bin_dir.join("sidle").exists(),
            "the authoritative bin/sidle is written"
        );
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
