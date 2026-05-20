//! Sidle native — Milestone 0 hello-world.
//!
//! The picker described in `.claude/plans/native-kindle-app.md` starts here:
//! prove the toolchain (cargo-zigbuild → armv7-musleabihf static binary) by
//! getting a stamp onto the device.
//!
//! When launched from KUAL there's no tty, so stdout vanishes. We append to
//! `/mnt/us/sidle-native.log` — accessible over USB after the device is
//! replugged, which is how we'll verify M0 done.

use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_PATH: &str = "/mnt/us/sidle-native.log";

fn main() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let pid = std::process::id();

    let line = format!(
        "sidle-native hello: ts={stamp} arch={arch} os={os} pid={pid}\n",
    );

    // File first — under KUAL, stdout/stderr point at a pipe nobody reads,
    // so a `print!` that panics on EPIPE would shadow the log write. Once
    // the file is on disk, anything later is bonus.
    let log_path = if std::path::Path::new("/mnt/us").is_dir() {
        LOG_PATH
    } else {
        "./sidle-native.log"
    };

    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = f.write_all(line.as_bytes());
    }

    // `let _ =` swallows EPIPE — running from a real shell still echoes,
    // running from KUAL silently no-ops instead of panicking.
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
}
