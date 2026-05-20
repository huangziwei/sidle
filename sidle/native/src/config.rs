//! Parse `etc/server.conf` — shell-style `KEY=VALUE` lines, `#`-prefix
//! comments. Single source of truth for "where is the Mac, and what's the
//! token". File lives at `/mnt/us/extensions/sidle/etc/server.conf` on the
//! device; the in-repo copy at `kual/sidle/etc/server.conf` is the deploy
//! template.

use std::path::Path;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Mac's LAN IP or hostname. Named `HOST` in the file (the old Phase 5
    /// bundle called it `MAC`; we renamed because it's an IP, not a MAC
    /// address — the legacy name was a bug magnet).
    pub host: String,
    pub port: u16,
    pub token: String,
}

pub fn load(path: &Path) -> Result<ServerConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;

    let mut host = String::new();
    let mut port_str = String::new();
    let mut token = String::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim().trim_matches('"').trim_matches('\'');
        match k.trim() {
            "HOST" => host = v.to_string(),
            "PORT" => port_str = v.to_string(),
            "TOKEN" => token = v.to_string(),
            _ => {} // ignore unknown keys for forward compatibility
        }
    }

    if host.is_empty() {
        bail!("server.conf missing HOST=");
    }
    if token.is_empty() {
        bail!("server.conf missing TOKEN=");
    }
    let port: u16 = port_str.parse().context("server.conf PORT= invalid")?;

    Ok(ServerConfig { host, port, token })
}
