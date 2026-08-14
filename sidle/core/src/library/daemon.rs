//! The LAN server as a process: is one serving, which one, and how to start or
//! stop it.
//!
//! `sidle-server` is a detached daemon, not a task inside anything: it outlives
//! the desktop app so a Kindle can still reach the library with the window
//! closed, and it is equally the CLI's to start. What both need is the same
//! handful of observations and syscalls, which live here; the policy on top —
//! when to replace a running daemon, how long to let it drain — belongs to the
//! caller that has a reason for it.
//!
//! Every function here is blocking and sub-second.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use crate::library::paths::LibraryPaths;
use crate::library::tls;

/// Matches `sidle-server`'s clap default, so the app, the CLI, and the daemon
/// itself all contend for — or adopt — one listener.
pub const DEFAULT_PORT: u16 = 8731;

/// Where the daemon records its process id. Written on start, removed on
/// graceful exit.
pub fn pid_path(paths: &LibraryPaths) -> PathBuf {
    paths.root.join("server.pid")
}

/// The daemon's PID, if the file names a plausible one.
///
/// **Only strictly positive values.** `kill(2)` reads non-positive PIDs as
/// broadcasts: `0` signals every process in the caller's own process group and
/// `-1` every process the user may signal. A truncated or corrupt PID file must
/// never turn a routine stop into that — so anything that isn't a real process
/// id is treated as no PID at all.
pub fn read_pid(paths: &LibraryPaths) -> Option<i32> {
    std::fs::read_to_string(pid_path(paths))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0)
}

/// Is *anything* listening on `port`? A bare TCP connect, so it is deliberately
/// blind to what protocol the listener speaks.
///
/// That blindness is the point. Port contention is a question about the socket,
/// not about the daemon, and answering it over HTTPS would make a listener we
/// cannot speak to look like a free port — after which a spawn fails to bind for
/// reasons the logs would not explain. The case is real rather than theoretical:
/// a daemon left over from a pre-TLS build serves plaintext and answers no HTTPS
/// probe at all, and it is exactly the one a launch most needs to displace.
pub fn port_open(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// Poll until nothing holds `port`. `true` if it went quiet in time.
pub fn wait_for_port_free(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !port_open(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !port_open(port)
}

/// "Up" = the liveness page answers over TLS, with the leaf verified against our
/// own CA. Any HTTP status counts; what must succeed is the handshake.
///
/// Stricter than a plaintext probe on purpose: it answers "our daemon is
/// serving, and its certificate is one our devices will accept", which is the
/// property a sync actually depends on. A listener we cannot verify reads as
/// down here — see [`port_open`] for the places that need the weaker question.
pub fn probe(paths: &LibraryPaths, port: u16) -> bool {
    let Some(client) = probe_client(paths) else {
        return false;
    };
    client
        .get(format!("https://127.0.0.1:{port}/"))
        .send()
        .is_ok()
}

/// Shared HTTPS client for liveness probes, trusting our own CA and nothing
/// else added on top of the system roots.
///
/// Built once (client construction is non-trivial; a start loop probes
/// repeatedly) but **cached only on success**: on a first run the CA does not
/// exist until [`ensure_tls_material`] has run, and caching a client without it
/// would leave every later probe unable to verify the daemon it just started.
///
/// A CA regenerated after this point needs a restart to take effect. That costs
/// nothing in practice — regenerating orphans every device carrying the old
/// root, so it is already a redeploy-everything event.
fn probe_client(paths: &LibraryPaths) -> Option<&'static reqwest::blocking::Client> {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Some(client);
    }
    let pem = tls::ca_cert_pem(paths).ok()?;
    let ca = reqwest::Certificate::from_pem(pem.as_bytes()).ok()?;
    let client = reqwest::blocking::Client::builder()
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
/// Loopback is for a local probe; the LAN address is what the Kindle uses, and
/// it is re-issued on every start because DHCP can move it. The CA underneath is
/// created once and never regenerated (see [`crate::library::tls`]), so this is
/// cheap and idempotent in the way that matters — devices keep trusting the same
/// root across every re-issue.
///
/// The leaf covers the LAN address **as of server start**. If the Mac's address
/// changes while the daemon runs, the running leaf goes stale and the device
/// fails to verify it until the next start — the same trip that already has to
/// rewrite `HOST=` in the device's `server.conf`.
pub fn ensure_tls_material(paths: &LibraryPaths) -> Result<()> {
    let mut sans = vec!["127.0.0.1".to_string(), "localhost".to_string()];
    if let Some(ip) = crate::library::device::deploy::detect_lan_ipv4() {
        sans.push(ip.to_string());
    }
    tls::issue_server_cert(paths, &sans).context("issue TLS material for the LAN server")
}

/// Ask the running daemon to stop, if one is both serving and nameable.
///
/// Signalling is gated on the port being open so a stale `server.pid` — the
/// daemon removes it on graceful exit, but a crash could leave one — cannot
/// deliver a SIGTERM to whatever process later reused that id.
pub fn signal_stop(paths: &LibraryPaths, port: u16) -> bool {
    if !port_open(port) {
        return false;
    }
    let Some(pid) = read_pid(paths) else {
        return false;
    };
    // SAFETY: plain libc `kill(2)`; `pid` came from the daemon's own PID file
    // and the port it named is answering.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    true
}

/// Resolve the `sidle-server` binary.
///
/// **Dev** (`debug_assertions`): `<workspace>/target/debug/sidle-server`,
/// **rebuilt unconditionally before every spawn**. A `cargo run` of a consumer
/// recompiles the `sidle_server` *lib* but not this *binary*, so without an
/// unconditional rebuild the spawn gets stale server code. cargo is incremental,
/// so this is a fast freshness check when nothing changed.
///
/// **Packaged** (release): the daemon rides along as a sidecar next to the
/// calling executable, so the bundle is self-contained — no reach-back into a
/// dev checkout's `target/`.
pub fn binary() -> Result<PathBuf> {
    if cfg!(debug_assertions) {
        let root = workspace_root().context("locate workspace root for the sidle-server binary")?;
        let bin = root.join("target").join("debug").join("sidle-server");
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

    let exe = std::env::current_exe().context("locate the running executable")?;
    let bin = exe
        .parent()
        .context("running executable has no parent directory")?
        .join("sidle-server");
    if bin.exists() {
        return Ok(bin);
    }
    Err(anyhow!(
        "sidle-server sidecar missing at {} — the packaged build is incomplete",
        bin.display()
    ))
}

/// The checkout this crate was built from, found by walking up to the manifest
/// that declares the workspace.
fn workspace_root() -> Result<PathBuf> {
    let start = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut p = start.as_path();
    loop {
        let candidate = p.join("Cargo.toml");
        if candidate.exists()
            && std::fs::read_to_string(&candidate)
                .ok()
                .is_some_and(|s| s.contains("[workspace]"))
        {
            return Ok(p.to_path_buf());
        }
        p = p
            .parent()
            .ok_or_else(|| anyhow!("workspace root not found above {}", start.display()))?;
    }
}

/// Spawn the daemon fully detached so it outlives its launcher: a new session
/// (`setsid`) drops the controlling terminal and puts it in its own process
/// group, and stdio goes to `<root>/server.log`. No `--data-dir`: the daemon
/// resolves the same root via `LibraryPaths::resolve()`, so it shares
/// `library.db` and `.server-token` with whoever started it.
pub fn spawn_detached(bin: &Path, paths: &LibraryPaths, port: u16) -> Result<Child> {
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

/// Start a daemon on `port` and wait until it is answering, returning the child
/// so the caller can reap it.
///
/// The caller decides what to do about anything already on the port; this only
/// starts one.
pub fn start(paths: &LibraryPaths, port: u16) -> Result<Child> {
    // The daemon hard-errors rather than falling back to plaintext when TLS
    // material is missing, so issue it before the spawn rather than letting the
    // failure surface as an unexplained "never became reachable".
    ensure_tls_material(paths)?;
    let child = spawn_detached(&binary()?, paths, port).context("spawn sidle-server")?;
    // The spawn returns before axum binds — wait until it is accepting.
    for _ in 0..200 {
        if probe(paths, port) {
            return Ok(child);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(anyhow!(
        "sidle-server spawned but never became reachable on :{port} — see server.log"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pid_parses_and_tolerates_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        assert_eq!(read_pid(&paths), None); // missing file
        std::fs::write(pid_path(&paths), "  4321\n").unwrap();
        assert_eq!(read_pid(&paths), Some(4321));
        std::fs::write(pid_path(&paths), "not-a-pid").unwrap();
        assert_eq!(read_pid(&paths), None);

        // `kill(2)` treats these as broadcasts — 0 hits our own process group,
        // -1 every process the user owns. A corrupt PID file must not be able to
        // turn a stop into that, so they are not PIDs as far as this is
        // concerned.
        for broadcast in ["0", "-1", "-4321"] {
            std::fs::write(pid_path(&paths), broadcast).unwrap();
            assert_eq!(
                read_pid(&paths),
                None,
                "{broadcast} is a kill(2) broadcast, not a process id",
            );
        }
    }

    #[test]
    fn wait_for_port_free_returns_promptly_on_a_dead_port() {
        // Bind and drop, so the port is known-free without racing a live server.
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        assert!(wait_for_port_free(port, Duration::from_secs(2)));
    }

    /// A listener that cannot complete our TLS handshake is no more usable to us
    /// than to a Kindle, so it does not count as a running server.
    #[test]
    fn a_plaintext_squatter_is_not_a_running_server() {
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
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();

        assert!(port_open(port), "the squatter holds the socket");
        assert!(
            !probe(&paths, port),
            "but it cannot present a certificate our devices would accept"
        );
    }
}
