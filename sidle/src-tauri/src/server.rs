//! Embedded LAN server lifecycle.
//!
//! Wraps `sidle_server::serve` as a process-wide tokio task. The Tauri app
//! spawns it on user request (toggle in the device popover); aborting the
//! task drops axum's listener so the OS releases the port — the standalone
//! `sidle-server` CLI can then take over if the user prefers that mode.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::library::LibraryPaths;

/// Matches `sidle-server`'s clap default so the embedded toggle and the
/// standalone binary contend for the same port — exactly the single-instance
/// contract the plan calls for (Phase 2/3).
pub const DEFAULT_PORT: u16 = 8731;

#[derive(Clone, Default)]
pub struct ServerHandle {
    inner: Arc<Mutex<Option<Running>>>,
}

struct Running {
    task: JoinHandle<()>,
    port: u16,
}

#[derive(Serialize, Clone, Debug)]
pub struct ServerStatus {
    pub running: bool,
    pub port: Option<u16>,
    /// Bearer secret the KUAL bundle (or curl tests) must send as
    /// `X-Sidle-Token`. Loaded/generated lazily so the UI can always show it
    /// even while the server is off (it lives in `.server-token`, persistent
    /// across runs).
    pub token: Option<String>,
    pub default_port: u16,
}

impl ServerHandle {
    pub async fn status(&self, paths: &LibraryPaths) -> ServerStatus {
        let running_port = {
            let guard = self.inner.lock().await;
            guard.as_ref().and_then(|r| {
                if r.task.is_finished() {
                    None
                } else {
                    Some(r.port)
                }
            })
        };
        let token = sidle_server::load_or_generate_token(&paths.root).ok();
        ServerStatus {
            running: running_port.is_some(),
            port: running_port,
            token,
            default_port: DEFAULT_PORT,
        }
    }

    pub async fn start(&self, paths: LibraryPaths, port: u16) -> Result<ServerStatus> {
        let mut guard = self.inner.lock().await;
        if let Some(r) = guard.as_ref()
            && !r.task.is_finished()
        {
            return Err(anyhow!("server already running on port {}", r.port));
        }

        let token = sidle_server::load_or_generate_token(&paths.root)
            .context("load or generate server token")?;
        let bind = format!("0.0.0.0:{port}");
        let config = sidle_server::Config {
            paths: paths.clone(),
            bind,
            token: token.clone(),
        };
        // Plain `tokio::spawn`: every caller is already in a tokio context —
        // Tauri commands run on Tauri's tokio runtime, tests on the test
        // runtime. Avoids initialising a second runtime via
        // `tauri::async_runtime::spawn`.
        let task = tokio::spawn(async move {
            if let Err(err) = sidle_server::serve(config).await {
                tracing::error!(?err, "embedded sidle-server exited with error");
            }
        });
        *guard = Some(Running { task, port });
        Ok(ServerStatus {
            running: true,
            port: Some(port),
            token: Some(token),
            default_port: DEFAULT_PORT,
        })
    }

    pub async fn stop(&self) {
        let mut guard = self.inner.lock().await;
        if let Some(r) = guard.take() {
            r.task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Picks a currently-free local TCP port by asking the OS for one and
    /// immediately releasing it. Inherently racy with another process but
    /// fine for a single-threaded test (which the tokio current-thread
    /// runtime gives us by default).
    fn pick_free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_stop_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths { root: tmp.path().to_path_buf() };
        paths.ensure().unwrap();

        let handle = ServerHandle::default();
        let port = pick_free_port();

        // Off before start.
        let s = handle.status(&paths).await;
        assert!(!s.running);
        assert_eq!(s.port, None);

        // Start: status flips, port is bound, double-start errors.
        let s = handle.start(paths.clone(), port).await.unwrap();
        assert!(s.running);
        assert_eq!(s.port, Some(port));
        // Wait for the spawned axum task to actually bind by polling a TCP
        // connect (bind-collision didn't seem reliable on macOS — possibly
        // 0.0.0.0 vs 127.0.0.1 + SO_REUSEADDR weirdness).
        let mut connected = false;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                connected = true;
                break;
            }
        }
        assert!(connected, "embedded server never accepted a connection on {port}");
        assert!(handle.start(paths.clone(), port).await.is_err());

        // Stop: status flips, port frees (allow the OS a moment).
        handle.stop().await;
        let s = handle.status(&paths).await;
        assert!(!s.running);
        let mut refused = false;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused, "embedded server never stopped accepting on {port}");
    }
}
