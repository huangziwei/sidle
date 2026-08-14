//! What a sync brings across besides books: screenshots and the picker's logs,
//! kept under `device-backup/<serial>/`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;

use crate::ctx::Ctx;

#[derive(Subcommand)]
pub enum MiscCmd {
    /// Everything backed up off every device, newest first.
    List {
        /// `screenshot` or `log`.
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
        /// Only this device's.
        #[arg(long, value_name = "SERIAL")]
        device: Option<String>,
    },
    /// Print a log file.
    Read { path: PathBuf },
    /// Delete backed-up files. The device keeps its own copies.
    Delete {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(long)]
        apply: bool,
    },
}

/// One backed-up file.
#[derive(Serialize)]
pub struct MiscFile {
    /// `screenshot` or `log`.
    kind: String,
    name: String,
    path: String,
    size: u64,
    modified: Option<String>,
    device: String,
}

pub fn run(ctx: &Ctx, cmd: MiscCmd) -> Result<()> {
    match cmd {
        MiscCmd::List { kind, device } => list(ctx, kind.as_deref(), device.as_deref()),
        MiscCmd::Read { path } => read(ctx, &path),
        MiscCmd::Delete { paths, apply } => delete(ctx, &paths, apply),
    }
}

fn list(ctx: &Ctx, kind: Option<&str>, device: Option<&str>) -> Result<()> {
    let root = ctx.paths.device_backup_dir();
    let mut out = Vec::new();
    let Ok(serials) = std::fs::read_dir(&root) else {
        // Nothing synced yet is an empty list, not a failure.
        return ctx.report(&out, || println!("nothing backed up yet"));
    };
    for serial_dir in serials.flatten() {
        let serial = serial_dir.file_name().to_string_lossy().to_string();
        if device.is_some_and(|d| d != serial) {
            continue;
        }
        for (sub, what) in [("screenshots", "screenshot"), ("logs", "log")] {
            if kind.is_some_and(|k| k != what) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(serial_dir.path().join(sub)) else {
                continue;
            };
            for f in files.flatten() {
                let Ok(meta) = f.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                out.push(MiscFile {
                    kind: what.to_string(),
                    name: f.file_name().to_string_lossy().to_string(),
                    path: f.path().to_string_lossy().to_string(),
                    size: meta.len(),
                    modified: mtime_iso(&meta),
                    device: serial.clone(),
                });
            }
        }
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    ctx.report(&out, || {
        for f in &out {
            println!(
                "{:<11} {:>8} KB  {}  {}",
                f.kind,
                f.size / 1024,
                f.modified.as_deref().unwrap_or("—"),
                f.path
            );
        }
        println!("\n{} file(s)", out.len());
    })
}

/// Filesystem mtime → naive local-wall-clock ISO, matching the shape the device
/// transports produce for `TEntry::modified`.
fn mtime_iso(meta: &std::fs::Metadata) -> Option<String> {
    let t = meta.modified().ok()?;
    Some(
        chrono::DateTime::<chrono::Utc>::from(t)
            .with_timezone(&chrono::Local)
            .naive_local()
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string(),
    )
}

fn read(ctx: &Ctx, path: &Path) -> Result<()> {
    inside_backup(ctx, path)?;
    print!("{}", std::fs::read_to_string(path)?);
    Ok(())
}

fn delete(ctx: &Ctx, paths: &[PathBuf], apply: bool) -> Result<()> {
    for p in paths {
        inside_backup(ctx, p)?;
    }
    if !apply {
        return ctx.report(&paths, || {
            println!("{} file(s) would be deleted:", paths.len());
            for p in paths {
                println!("  {}", p.display());
            }
            println!("\nRe-run with --apply.");
        });
    }
    let mut gone = 0usize;
    for p in paths {
        std::fs::remove_file(p)?;
        gone += 1;
    }
    ctx.report(&gone, || println!("deleted {gone} file(s)"))
}

/// Refuse a path outside the backup tree: these commands read and delete by
/// path, and a typo must not reach the rest of the disk.
fn inside_backup(ctx: &Ctx, path: &Path) -> Result<()> {
    let root = ctx.paths.device_backup_dir();
    let canonical = path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    if !canonical.starts_with(&root) {
        anyhow::bail!(
            "{} is not under {} — this command only touches files synced off a device",
            path.display(),
            root.display()
        );
    }
    Ok(())
}
