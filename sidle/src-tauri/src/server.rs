//! Supervises the standalone `sidle-server` as a **detached child process**, so
//! the LAN server outlives the desktop GUI — the Kindle can still reach the
//! library, and push annotations back, with the app closed.
//!
//! The syscalls and observations are `sidle_core::library::daemon`, shared with
//! the CLI. What lives here is the desktop's policy on top of them:
//! - **start** REPLACES an already-running instance, then spawns a fresh one.
//!   Because the daemon outlives the GUI, the one a launching app finds is
//!   usually its own predecessor, still running whatever code was on disk when
//!   it started; replacing it is what keeps "the app and the server were built
//!   from the same tree" true without anyone having to check. A daemon with no
//!   PID file is still adopted — there is nothing to signal, and it belongs to
//!   someone else.
//! - **stop** SIGTERMs the daemon via its PID file, then reaps the child if we
//!   were the one who spawned it.
//! - **status** is observation-based, so a daemon started anywhere — by the CLI,
//!   by an earlier app run — shows as running here, and vice-versa.

use std::process::Child;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::Serialize;
use sidle_core::library::daemon;
use tokio::sync::Mutex;

use crate::library::LibraryPaths;

pub use sidle_core::library::daemon::DEFAULT_PORT;

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

/// How long to let a replaced daemon drain before giving up and adopting it.
/// Generous: it finishes in-flight requests first, and a Kindle sync mid-flight
/// is exactly the request worth waiting for.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

impl ServerHandle {
    pub async fn status(&self, paths: &LibraryPaths) -> ServerStatus {
        let port = self.inner.lock().await.port;
        let p = paths.clone();
        let up = tokio::task::spawn_blocking(move || daemon::probe(&p, port))
            .await
            .unwrap_or(false);
        let token = sidle_server::load_or_generate_token(&paths.root).ok();
        ServerStatus {
            running: up,
            port: up.then_some(port),
            token,
            default_port: DEFAULT_PORT,
            pid: if up { daemon::read_pid(paths) } else { None },
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
        // fail.
        if blocking(move || daemon::port_open(port)).await {
            if daemon::read_pid(&paths).is_none() {
                return Ok(self.status(&paths).await);
            }
            self.stop(&paths).await;
            // The daemon drains in-flight requests before releasing the port, so
            // binding before it lets go would fail the spawn below. If it never
            // lets go, adopt what is there instead of erroring — a working server
            // beats a failed launch.
            if !blocking(move || daemon::wait_for_port_free(port, DRAIN_TIMEOUT)).await {
                tracing::warn!("sidle-server did not release :{port}; adopting it");
                return Ok(self.status(&paths).await);
            }
        }

        // Resolving the binary (and a dev build-on-demand), the spawn syscall,
        // and the wait for the daemon to bind are all blocking.
        let p = paths.clone();
        let child = tokio::task::spawn_blocking(move || daemon::start(&p, port))
            .await
            .map_err(|e| anyhow!("join server-spawn task: {e}"))??;
        self.inner.lock().await.child = Some(child);
        Ok(self.status(&paths).await)
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
        if !blocking(move || daemon::port_open(port)).await {
            return Ok(());
        }
        self.start(paths, port).await.map(|_| ())
    }

    pub async fn stop(&self, paths: &LibraryPaths) {
        let port = self.inner.lock().await.port;
        let p = paths.clone();
        let _ = tokio::task::spawn_blocking(move || daemon::signal_stop(&p, port)).await;
        // Reap our child if we spawned one (else it lingers as a zombie until the
        // app exits). Adopted daemons aren't our children — nothing to reap.
        let child = self.inner.lock().await.child.take();
        if let Some(mut child) = child {
            let _ = tokio::task::spawn_blocking(move || child.wait()).await;
        }
    }
}

/// Run a short blocking probe off the reactor.
async fn blocking<T: Send + Default + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plaintext listener holding a port — a *squatter*, not a sidle server.
    /// It answers HTTP and cannot complete a TLS handshake, which is what makes
    /// it the right stand-in for both cases the adoption path guards against: a
    /// daemon from a pre-TLS build, and an unrelated process on the port.
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
        std::fs::write(daemon::pid_path(&paths), i32::MAX.to_string()).unwrap();

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
}
