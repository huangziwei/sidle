//! Supervises the standalone `sidle-server` as a **detached child process**, so
//! the LAN server outlives the desktop GUI — the Kindle can still reach the
//! library, and (P3) push annotations back, with the app closed. Replaces the
//! old in-process tokio task.
//!
//! start/stop/status mirror what sakabar does, so both can manage one shared
//! daemon and agree on a single instance:
//! - **start** health-probes `/` and ADOPTS an already-running instance (sakabar-,
//!   CLI-, or previously-app-started) instead of double-spawning; otherwise spawns
//!   the binary detached (new session, stdio → `<root>/server.log`).
//! - **stop** SIGTERMs the daemon via its `<root>/server.pid` file (the daemon
//!   writes it on start, removes it on graceful exit), then reaps the child if we
//!   were the one who spawned it.
//! - **status** is observation-based (`/` probe + PID file), so a daemon started
//!   anywhere shows as running here, and vice-versa.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::library::LibraryPaths;

/// Matches `sidle-server`'s clap default so the app, the standalone binary, and
/// sakabar all contend for / adopt one listener.
pub const DEFAULT_PORT: u16 = 8731;

#[derive(Clone)]
pub struct ServerHandle {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    /// `Some` only while WE hold a spawned child, so `stop` can reap it instead of
    /// leaking a zombie. `None` when nothing is spawned or the daemon was adopted
    /// (an adopted daemon isn't our child — there's nothing to reap).
    child: Option<Child>,
    /// The port we manage — `DEFAULT_PORT` until a `start` overrides it.
    /// `status`/`stop` probe this.
    port: u16,
}

impl Default for ServerHandle {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                child: None,
                port: DEFAULT_PORT,
            })),
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct ServerStatus {
    pub running: bool,
    pub port: Option<u16>,
    /// Bearer secret the KUAL bundle (or curl tests) must send as `X-Sidle-Token`.
    /// Loaded/generated lazily so the UI can show it even while the server is off
    /// (it lives in `.server-token`, persistent across runs).
    pub token: Option<String>,
    pub default_port: u16,
    /// PID of the running daemon (from `server.pid`), so the UI/CLI can show who
    /// is serving and a stop targets it precisely. `None` when down.
    pub pid: Option<i32>,
}

/// Shared HTTP client for liveness probes — built once (reqwest client
/// construction is non-trivial; the start loop probes repeatedly).
fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("build reqwest probe client")
    })
}

/// "Up" = the liveness page answers (any HTTP response — mirrors sakabar's
/// "healthy = any HTTPURLResponse"). A down port fails fast (connection refused),
/// so this is cheap to poll.
async fn probe(port: u16) -> bool {
    http()
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .is_ok()
}

impl ServerHandle {
    fn pid_path(paths: &LibraryPaths) -> PathBuf {
        paths.root.join("server.pid")
    }

    fn read_pid(paths: &LibraryPaths) -> Option<i32> {
        std::fs::read_to_string(Self::pid_path(paths))
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
    }

    pub async fn status(&self, paths: &LibraryPaths) -> ServerStatus {
        let port = self.inner.lock().await.port;
        let up = probe(port).await;
        let token = sidle_server::load_or_generate_token(&paths.root).ok();
        ServerStatus {
            running: up,
            port: up.then_some(port),
            token,
            default_port: DEFAULT_PORT,
            pid: if up { Self::read_pid(paths) } else { None },
        }
    }

    pub async fn start(&self, paths: LibraryPaths, port: u16) -> Result<ServerStatus> {
        self.inner.lock().await.port = port;

        // Adopt an already-running daemon — never double-spawn. Mirrors sakabar,
        // and avoids a port-bind race against a concurrent sakabar/CLI start.
        if probe(port).await {
            return Ok(self.status(&paths).await);
        }

        // Resolving the binary (and a dev build-on-demand) plus the spawn syscall
        // are blocking, so do them off the reactor.
        let p = paths.clone();
        let child = tokio::task::spawn_blocking(move || -> Result<Child> {
            let bin = server_binary()?;
            spawn_detached(&bin, &p, port)
        })
        .await
        .context("join server-spawn task")?
        .context("spawn sidle-server")?;

        self.inner.lock().await.child = Some(child);

        // The spawn returns before axum binds — wait until it's accepting. 10 s cap
        // (the dev build, if any, already happened in the blocking task above).
        for _ in 0..200 {
            if probe(port).await {
                return Ok(self.status(&paths).await);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(anyhow!(
            "sidle-server spawned but never became reachable on :{port} — see server.log"
        ))
    }

    pub async fn stop(&self, paths: &LibraryPaths) {
        let port = self.inner.lock().await.port;
        // Only signal a server that's actually up — guards against SIGTERM to a
        // reused PID from a stale `server.pid` (the daemon removes it on graceful
        // exit, but a SIGKILL/crash could leave one behind).
        if probe(port).await
            && let Some(pid) = Self::read_pid(paths)
        {
            // SAFETY: plain libc `kill(2)`; `pid` is from the daemon's fresh PID
            // file and we just confirmed it's serving.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        // Reap our child if we spawned one (else it lingers as a zombie until the
        // app exits). Adopted daemons aren't our children — nothing to reap.
        let child = self.inner.lock().await.child.take();
        if let Some(mut child) = child {
            let _ = tokio::task::spawn_blocking(move || child.wait()).await;
        }
    }
}

/// Resolve the `sidle-server` binary.
///
/// **Dev** (`debug_assertions`): `<workspace>/target/debug/sidle-server`,
/// **rebuilt unconditionally before every spawn**. `cargo run -p sidle` recompiles
/// the `sidle_server` *lib* but not this *binary*, and the supervisor adopts an
/// already-running server — so without an unconditional rebuild the app silently
/// spawns/adopts stale server code (a pre-route binary once 404'd
/// `/sync/annotations`). cargo is incremental, so this is a fast freshness check when
/// nothing changed. Note: a *running* instance is still adopted, not replaced —
/// toggle the server off→on to pick up an edit.
///
/// **Packaged** (release): the daemon rides along as a Tauri sidecar, copied into
/// `Sidle.app/Contents/MacOS/sidle-server` next to our own binary (build.sh stages
/// `binaries/sidle-server-<host-triple>`; `externalBin` strips the suffix on bundle).
/// Resolved relative to `current_exe` so the bundle is self-contained — no reach-back
/// into a dev checkout's `target/`.
fn server_binary() -> Result<PathBuf> {
    if cfg!(debug_assertions) {
        let root = crate::state::find_workspace_root()
            .context("locate workspace root for the sidle-server binary")?;
        let bin = root.join("target").join("debug").join("sidle-server");
        // Always rebuild before spawning (not just when missing) — otherwise a stale
        // on-disk binary from before a server-code edit gets spawned as-is.
        let status = Command::new("cargo")
            .args(["build", "-p", "sidle-server"])
            .current_dir(&root)
            .status()
            .context("cargo build -p sidle-server")?;
        if status.success() && bin.exists() {
            return Ok(bin);
        }
        return Err(anyhow!(
            "cargo build -p sidle-server did not produce {}",
            bin.display()
        ));
    }

    // Packaged (release): the sidecar sits next to the app's own executable.
    let exe = std::env::current_exe().context("locate the running executable")?;
    let bin = exe
        .parent()
        .context("running executable has no parent directory")?
        .join("sidle-server");
    if bin.exists() {
        return Ok(bin);
    }
    Err(anyhow!(
        "sidle-server sidecar missing at {} — packaged build is incomplete (build.sh stages it)",
        bin.display()
    ))
}

/// Spawn the daemon fully detached so it outlives the app: a new session
/// (`setsid`) drops the controlling terminal and puts it in its own process
/// group (so the app quitting — or a dev terminal closing — doesn't take it
/// down), and stdio goes to `<root>/server.log`. No `--data-dir`: the daemon
/// resolves the same root via `LibraryPaths::resolve()`, so it shares
/// `library.db` + `.server-token` with the app.
fn spawn_detached(bin: &Path, paths: &LibraryPaths, port: u16) -> Result<Child> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.root.join("server.log"))
        .context("open server.log")?;
    let log_err = log.try_clone().context("clone server.log handle")?;

    let mut cmd = Command::new(bin);
    cmd.arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe — the only call between fork and
        // exec, touching no shared state in the forked child.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    cmd.spawn()
        .with_context(|| format!("spawn {}", bin.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway HTTP server on a free port that 200s any request, run on a
    /// blocking std thread (so the test needs no extra tokio features). Leaks the
    /// thread — fine for a test process.
    fn dummy_http_server() -> u16 {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            }
        });
        port
    }

    #[test]
    fn read_pid_parses_and_tolerates_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        assert_eq!(ServerHandle::read_pid(&paths), None); // missing file
        std::fs::write(ServerHandle::pid_path(&paths), "  4321\n").unwrap();
        assert_eq!(ServerHandle::read_pid(&paths), Some(4321));
        std::fs::write(ServerHandle::pid_path(&paths), "not-a-pid").unwrap();
        assert_eq!(ServerHandle::read_pid(&paths), None);
    }

    /// `start` must ADOPT an already-healthy server (any HTTP response on `/`)
    /// rather than spawn a duplicate — so an app start finds a sakabar/CLI-started
    /// daemon. The real spawn path is covered by the live gate (it needs the
    /// actual binary).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_adopts_already_running_without_spawning() {
        let port = dummy_http_server();
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();

        let handle = ServerHandle::default();
        let s = handle.start(paths.clone(), port).await.unwrap();
        assert!(s.running);
        assert_eq!(s.port, Some(port));
        assert!(
            handle.inner.lock().await.child.is_none(),
            "an already-running server must be adopted, not re-spawned"
        );
    }
}
