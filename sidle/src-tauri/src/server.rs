//! Supervises the standalone `sidle-server` as a **detached child process**, so
//! the LAN server outlives the desktop GUI — the Kindle can still reach the
//! library, and (P3) push annotations back, with the app closed. Replaces the
//! old in-process tokio task.
//!
//! start/stop/status mirror what sakabar does, so both can manage one shared
//! daemon and agree on a single instance:
//! - **start** issues the TLS material the daemon will not start without, checks
//!   the port for *any* listener, REPLACES an already-running instance, then
//!   spawns the binary detached (new session, stdio → `<root>/server.log`).
//!   Because the daemon outlives the GUI, the one a launching app finds is
//!   usually its own predecessor, still running whatever code was on disk when it
//!   started; replacing it is what keeps "the app and the server were built from
//!   the same tree" true without anyone having to check. A daemon with no PID
//!   file is still adopted — there is nothing to signal, and it belongs to
//!   someone else.
//! - **stop** SIGTERMs the daemon via its `<root>/server.pid` file (the daemon
//!   writes it on start, removes it on graceful exit), then reaps the child if we
//!   were the one who spawned it.
//! - **status** is observation-based (`/` probe + PID file), so a daemon started
//!   anywhere shows as running here, and vice-versa. Since the switch to TLS the
//!   probe verifies the leaf against our own CA, so "running" means a daemon a
//!   Kindle could actually use — a listener we cannot verify reads as down.

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
    /// Bearer secret the on-device app (or curl tests) must send as `X-Sidle-Token`.
    /// Loaded/generated lazily so the UI can show it even while the server is off
    /// (it lives in `.server-token`, persistent across runs).
    pub token: Option<String>,
    pub default_port: u16,
    /// PID of the running daemon (from `server.pid`), so the UI/CLI can show who
    /// is serving and a stop targets it precisely. `None` when down.
    pub pid: Option<i32>,
}

/// Shared HTTPS client for liveness probes, trusting our own CA and nothing
/// else added on top of the system roots.
///
/// Built once (reqwest client construction is non-trivial; the start loop probes
/// repeatedly) but **cached only on success**: on a first run the CA does not
/// exist until [`ensure_tls_material`] has run, and caching a client without it
/// would leave every later probe unable to verify the daemon it just started.
///
/// A CA regenerated after this point needs an app restart to take effect. That
/// costs nothing in practice — regenerating orphans every device carrying the
/// old root, so it is already a redeploy-everything event.
fn probe_client(paths: &LibraryPaths) -> Option<&'static reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Some(client);
    }
    let pem = crate::library::tls::ca_cert_pem(paths).ok()?;
    let ca = reqwest::Certificate::from_pem(pem.as_bytes()).ok()?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .add_root_certificate(ca)
        .build()
        .ok()?;
    let _ = CLIENT.set(client);
    CLIENT.get()
}

/// Issue the TLS material the daemon refuses to start without, covering the
/// addresses clients actually dial.
///
/// Loopback is for this app's own probe; the LAN address is what the Kindle
/// uses, and it is re-issued on every start because DHCP can move it. The CA
/// underneath is created once and never regenerated (see `library::tls`), so
/// this is cheap and idempotent in the way that matters — devices keep trusting
/// the same root across every re-issue.
///
/// Note the leaf covers the LAN address **as of server start**. If the Mac's
/// address changes while the daemon is running, the running leaf goes stale and
/// the device fails to verify it until the next start. That is the same trip
/// that already has to rewrite `HOST=` in the device's `server.conf`, so it is
/// not a new failure mode — but it is why this lives on the start path rather
/// than only at deploy.
fn ensure_tls_material(paths: &LibraryPaths) -> Result<()> {
    let mut sans = vec!["127.0.0.1".to_string(), "localhost".to_string()];
    if let Some(ip) = crate::device::deploy::detect_lan_ipv4() {
        sans.push(ip.to_string());
    }
    crate::library::tls::issue_server_cert(paths, &sans)
        .context("issue TLS material for the LAN server")
}

/// How long to let a replaced daemon drain before giving up and adopting it.
/// Generous: it finishes in-flight requests first, and a Kindle sync mid-flight
/// is exactly the request worth waiting for.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll until nothing holds `port`. `true` if it went quiet in time.
async fn wait_for_port_free(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !port_open(port).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !port_open(port).await
}

/// Is *anything* listening on `port`? A bare TCP connect, so it is deliberately
/// blind to what protocol the listener speaks.
///
/// That blindness is the point. Port contention is a question about the socket,
/// not about the daemon, and answering it over HTTPS would make a listener we
/// cannot speak to look like a free port — after which the spawn below fails to
/// bind for reasons the logs would not explain. The case is real rather than
/// theoretical: a daemon left over from a pre-TLS build serves plaintext and
/// answers no HTTPS probe at all, and it is exactly the one a launching app
/// most needs to displace.
async fn port_open(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// "Up" = the liveness page answers over TLS, with the leaf verified against our
/// own CA. Any HTTP status counts (mirrors sakabar's "healthy = any
/// HTTPURLResponse"); what must succeed is the handshake.
///
/// Stricter than the old plaintext probe on purpose: it now answers "our daemon
/// is serving, and its certificate is one our devices will accept", which is the
/// property a launch actually depends on. A listener we cannot verify reads as
/// down here — see [`port_open`] for the places that need the weaker question.
async fn probe(paths: &LibraryPaths, port: u16) -> bool {
    let Some(client) = probe_client(paths) else {
        return false;
    };
    client
        .get(format!("https://127.0.0.1:{port}/"))
        .send()
        .await
        .is_ok()
}

impl ServerHandle {
    fn pid_path(paths: &LibraryPaths) -> PathBuf {
        paths.root.join("server.pid")
    }

    /// The daemon's PID, if the file names a plausible one.
    ///
    /// **Only strictly positive values.** `kill(2)` reads non-positive PIDs as
    /// broadcasts: `0` signals every process in the caller's own process group
    /// and `-1` every process the user may signal. A truncated or corrupt PID
    /// file must never turn a routine `stop` into that — so anything that isn't
    /// a real process id is treated as no PID at all.
    fn read_pid(paths: &LibraryPaths) -> Option<i32> {
        std::fs::read_to_string(Self::pid_path(paths))
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .filter(|pid| *pid > 0)
    }

    pub async fn status(&self, paths: &LibraryPaths) -> ServerStatus {
        let port = self.inner.lock().await.port;
        let up = probe(paths, port).await;
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

        // Replace a running daemon rather than adopt it. The server deliberately
        // outlives the GUI, so the daemon found here is typically one an earlier
        // app run left behind — and it is running whatever code was on disk when
        // it started, which after a rebuild is not what this app expects. A
        // launching app always gets a server built from the same tree it was.
        //
        // Unconditional, with no freshness comparison: the version string does
        // not change between builds of the same release, so there is nothing
        // cheap to compare that would actually be right. Restarting always is
        // both simpler and stricter than any test for staleness.
        //
        // The exception is a daemon we cannot name — no PID file, so nothing to
        // signal. That is a server someone else is running; adopt it rather than
        // fail, exactly as before.
        if port_open(port).await {
            if Self::read_pid(&paths).is_none() {
                return Ok(self.status(&paths).await);
            }
            self.stop(&paths).await;
            // The daemon drains in-flight requests before releasing the port, so
            // binding before it lets go would fail the spawn below. If it never
            // lets go, adopt what is there instead of erroring — a working server
            // beats a failed launch.
            if !wait_for_port_free(port, DRAIN_TIMEOUT).await {
                tracing::warn!("sidle-server did not release :{port}; adopting it");
                return Ok(self.status(&paths).await);
            }
        }

        // The daemon hard-errors rather than falling back to plaintext when TLS
        // material is missing, so issue it before the spawn rather than letting
        // the failure surface as an unexplained "never became reachable".
        ensure_tls_material(&paths)?;

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
            if probe(&paths, port).await {
                return Ok(self.status(&paths).await);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(anyhow!(
            "sidle-server spawned but never became reachable on :{port} — see server.log"
        ))
    }

    /// Bring a daemon left running by an earlier app run in line with this one.
    ///
    /// The server deliberately survives the GUI closing, so the next launch can
    /// find one serving code built before the app that just started. Restarting
    /// it here is what makes "the running server was built from the same tree as
    /// the app" true by construction, instead of something anyone has to notice
    /// and fix by toggling the server off and on.
    ///
    /// **Only ever restarts; never starts.** The LAN server is opt-in, and a
    /// user who left it off must not find it switched on because they opened the
    /// app. Nothing running means nothing to realign.
    pub async fn realign_on_launch(&self, paths: LibraryPaths, port: u16) -> Result<()> {
        // `port_open` rather than the TLS probe: a daemon from a pre-TLS build is
        // precisely one that needs realigning, and it cannot answer HTTPS.
        if !port_open(port).await {
            return Ok(());
        }
        self.start(paths, port).await.map(|_| ())
    }

    pub async fn stop(&self, paths: &LibraryPaths) {
        let port = self.inner.lock().await.port;
        // Only signal a server that's actually up — guards against SIGTERM to a
        // reused PID from a stale `server.pid` (the daemon removes it on graceful
        // exit, but a SIGKILL/crash could leave one behind).
        if port_open(port).await
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
/// the `sidle_server` *lib* but not this *binary*, so without an unconditional
/// rebuild the app spawns stale server code (a pre-route binary once 404'd
/// `/sync/annotations`). cargo is incremental, so this is a fast freshness check
/// when nothing changed. Together with a launch replacing whatever daemon it
/// finds, this is what keeps the running server and the app the same vintage.
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
    /// A plaintext listener holding a port — a *squatter*, not a sidle server.
    /// It answers HTTP and cannot complete a TLS handshake, which is what makes
    /// it the right stand-in for both cases the adoption path guards against: a
    /// daemon from a pre-TLS build, and an unrelated process on 8731.
    fn dummy_plaintext_squatter() -> u16 {
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

        // `kill(2)` treats these as broadcasts — 0 hits our own process group,
        // -1 every process the user owns. A corrupt PID file must not be able to
        // turn `stop` into that, so they are not PIDs as far as this is concerned.
        for broadcast in ["0", "-1", "-4321"] {
            std::fs::write(ServerHandle::pid_path(&paths), broadcast).unwrap();
            assert_eq!(
                ServerHandle::read_pid(&paths),
                None,
                "{broadcast} is a kill(2) broadcast, not a process id",
            );
        }
    }

    /// A listener with no PID file is someone else's — there is nothing to
    /// signal, so it is adopted rather than replaced or duplicated. (The replace
    /// path needs the real binary and is covered by the live gate.)
    ///
    /// It is also reported as **not running**, and that is the point of the
    /// assertion rather than an accident of the fixture: since the switch to
    /// TLS, "running" means a daemon whose certificate our own CA vouches for —
    /// exactly the daemon a Kindle could use. A squatter that cannot present one
    /// is no more usable to the app than to the device, so calling it "running"
    /// would promise a sync that cannot happen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unverifiable_squatter_is_adopted_but_not_called_running() {
        let port = dummy_plaintext_squatter();
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();

        let handle = ServerHandle::default();
        let s = handle.start(paths.clone(), port).await.unwrap();
        assert!(
            handle.inner.lock().await.child.is_none(),
            "a server we cannot signal must be adopted, not re-spawned"
        );
        assert!(
            !s.running,
            "a listener that cannot complete our TLS handshake is not a server \
             the device could use either — reporting it as running would promise \
             a sync that cannot happen"
        );
        assert_eq!(
            s.port, None,
            "no port is being served that we can vouch for"
        );
    }

    /// A daemon that ignores the stop — here, a PID file naming a process that
    /// isn't the one serving — must not fail the launch or leave the app trying
    /// to bind an occupied port. Falling back to adoption keeps a working server.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_falls_back_to_adopting_a_daemon_that_will_not_release_the_port() {
        let port = dummy_plaintext_squatter();
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        // A PID far above the system maximum: nameable, so a replace is
        // attempted, but no process to actually receive it — the dummy server
        // keeps the port and the drain wait runs out.
        std::fs::write(ServerHandle::pid_path(&paths), i32::MAX.to_string()).unwrap();

        let handle = ServerHandle::default();
        let s = tokio::time::timeout(
            DRAIN_TIMEOUT + Duration::from_secs(5),
            handle.start(paths.clone(), port),
        )
        .await
        .expect("start must give up on the drain, not hang")
        .unwrap();
        assert!(
            handle.inner.lock().await.child.is_none(),
            "never spawn a second server onto an occupied port"
        );
        // The port is still held, so binding a second daemon would have failed —
        // that is what this test is really protecting. Whether the survivor is
        // *usable* is a separate question, answered by the TLS probe, and for a
        // plaintext squatter the answer is no.
        assert!(!s.running);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_port_free_returns_promptly_on_a_dead_port() {
        // Bind and drop, so the port is known-free without racing a live server.
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        assert!(wait_for_port_free(port, Duration::from_secs(2)).await);
    }
}
