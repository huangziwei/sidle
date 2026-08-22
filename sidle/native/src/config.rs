//! Parse `etc/server.conf` — shell-style `KEY=VALUE` lines, `#`-prefix
//! comments. Single source of truth for "where is the Mac, and what's the
//! token". The file lives at `/mnt/us/extensions/sidle/etc/server.conf`.

use std::path::Path;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Mac's LAN IP or hostname. Named `HOST` in the config file — it holds an
    /// address, never a MAC.
    pub host: String,
    pub port: u16,
    pub token: String,
    /// This Kindle's USB iSerial, echoed back as `device_serial` in the
    /// `/sync/annotations` push. Empty when `server.conf` carries no `SERIAL=`;
    /// only the push path reads it.
    pub serial: String,
}

pub fn load(path: &Path) -> Result<ServerConfig> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    parse(&raw).with_context(|| format!("parse {}", path.display()))
}

/// [`load`] over the file's text, so the shape of the format is testable
/// without one on disk.
pub fn parse(raw: &str) -> Result<ServerConfig> {
    let mut host = String::new();
    let mut port_str = String::new();
    let mut token = String::new();
    let mut serial = String::new();

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
            "SERIAL" => serial = v.to_string(),
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

    Ok(ServerConfig {
        host,
        port,
        token,
        serial,
    })
}

/// `raw` with its `HOST=` line naming `host`, appending the line when the file
/// carries none.
///
/// Rewrites one line and copies the rest through: the file also holds the
/// bearer token and this Kindle's serial, neither of which the picker knows how
/// to regenerate.
pub fn set_host(raw: &str, host: &str) -> String {
    let mut out = String::with_capacity(raw.len() + host.len());
    let mut replaced = false;
    for line in raw.lines() {
        let key = line.split_once('=').map(|(k, _)| k.trim());
        if !line.trim_start().starts_with('#') && key == Some("HOST") {
            out.push_str(&format!("HOST={host}"));
            replaced = true;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !replaced {
        out.push_str(&format!("HOST={host}\n"));
    }
    out
}

/// Point `path`'s `HOST=` at `host`, so the next launch dials the server
/// directly instead of searching for it again.
///
/// Through a sibling temp file and a `rename(2)`: a half-written `server.conf`
/// is a picker that cannot reach the server at all, and the two are in the same
/// directory so the rename stays inside the one `/mnt/us` mount.
pub fn save_host(path: &Path, host: &str) -> Result<()> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, set_host(&raw, host))
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {}", tmp.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONF: &str = "# comment\n\nHOST=192.168.0.248\nPORT=8731\nSERIAL=G000X\nTOKEN=abc\n";

    #[test]
    fn rewriting_the_host_keeps_every_other_field() {
        let out = set_host(CONF, "192.168.0.174");
        assert_eq!(
            out,
            "# comment\n\nHOST=192.168.0.174\nPORT=8731\nSERIAL=G000X\nTOKEN=abc\n"
        );
        let cfg = parse(&out).unwrap();
        assert_eq!(cfg.host, "192.168.0.174");
        assert_eq!(cfg.port, 8731);
        assert_eq!(cfg.token, "abc");
        assert_eq!(cfg.serial, "G000X");
    }

    #[test]
    fn a_conf_without_a_host_line_gains_one() {
        let out = set_host("PORT=8731\nTOKEN=abc\n", "10.0.0.2");
        assert_eq!(out, "PORT=8731\nTOKEN=abc\nHOST=10.0.0.2\n");
    }

    /// A commented-out `HOST=` is prose, not the setting: rewriting it would
    /// leave the live value untouched and the picker still dialling the old
    /// address.
    #[test]
    fn a_commented_host_line_is_left_alone() {
        let out = set_host("# HOST=1.2.3.4\nHOST=5.6.7.8\nTOKEN=abc\n", "10.0.0.2");
        assert_eq!(out, "# HOST=1.2.3.4\nHOST=10.0.0.2\nTOKEN=abc\n");
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = std::env::temp_dir().join(format!("sidle-conf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.conf");
        std::fs::write(&path, CONF).unwrap();

        save_host(&path, "10.0.0.9").unwrap();

        assert_eq!(load(&path).unwrap().host, "10.0.0.9");
        assert!(!dir.join("server.conf.tmp").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
