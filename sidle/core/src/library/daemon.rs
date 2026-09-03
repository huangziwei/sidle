//! The LAN server as a process: is one serving, which one, and how to start or
//! stop it.

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
pub fn read_pid(paths: &LibraryPaths) -> Option<i32> {
    std::fs::read_to_string(pid_path(paths))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0)
}

/// Is *anything* listening on `port`? A bare TCP connect, so it is deliberately
/// blind to what protocol the listener speaks.
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
pub fn ensure_tls_material(paths: &LibraryPaths) -> Result<()> {
    let mut sans = vec!["127.0.0.1".to_string(), "localhost".to_string()];
    if let Some(ip) = crate::library::device::deploy::detect_lan_ipv4() {
        sans.push(ip.to_string());
    }
    tls::issue_server_cert(paths, &sans).context("issue TLS material for the LAN server")
}

/// Ask the running daemon to stop, if one is both serving and nameable.
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

        // `kill(2)` treats these as broadcasts — 0 the process group, -1 every process
        // the user owns — so a corrupt PID file must never reach it.
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
