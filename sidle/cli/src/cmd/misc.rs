//! What a sync brings across besides books: the configured collections'
//! folders, kept under `device-backup/<serial>/<collection>/`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;
use sidle_core::library::device_backup::SyncCollections;

use crate::ctx::Ctx;

#[derive(Subcommand)]
pub enum MiscCmd {
    /// Everything backed up off every device, newest first.
    List {
        /// Only this collection's, by id (`screenshots`, `logs`, …).
        #[arg(long, value_name = "ID")]
        collection: Option<String>,
        /// Only this device's.
        #[arg(long, value_name = "SERIAL")]
        device: Option<String>,
    },
    /// Which device folders a sync scans.
    Collections,
    /// Print a text file.
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
    /// The collection whose folder it came off the device in.
    collection: String,
    /// Path relative to that collection's dir.
    name: String,
    path: String,
    size: u64,
    modified: Option<String>,
    device: String,
}

/// How deep the scan descends — the device-side walk's cap, so anything that
/// could be backed up can be listed.
const MAX_DEPTH: usize = 5;

pub fn run(ctx: &Ctx, cmd: MiscCmd) -> Result<()> {
    match cmd {
        MiscCmd::List { collection, device } => list(ctx, collection.as_deref(), device.as_deref()),
        MiscCmd::Collections => collections(ctx),
        MiscCmd::Read { path } => read(ctx, &path),
        MiscCmd::Delete { paths, apply } => delete(ctx, &paths, apply),
    }
}

fn collections(ctx: &Ctx) -> Result<()> {
    let config = SyncCollections::load(&ctx.paths)?;
    ctx.report(&config, || {
        for c in &config.collections {
            println!(
                "{:<14} {:<16} {}  [{}]{}",
                c.id,
                c.label,
                c.dirs.join(", "),
                c.include.join(" "),
                if c.clear_device {
                    " cleared after sync"
                } else {
                    ""
                }
            );
        }
        println!("\n{} collection(s)", config.collections.len());
    })
}

fn list(ctx: &Ctx, collection: Option<&str>, device: Option<&str>) -> Result<()> {
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
        let Ok(dirs) = std::fs::read_dir(serial_dir.path()) else {
            continue;
        };
        for dir in dirs.flatten() {
            let id = dir.file_name().to_string_lossy().to_string();
            if id.starts_with('.') || collection.is_some_and(|c| c != id) {
                continue;
            }
            collect(&dir.path(), "", &id, &serial, &mut out, 0);
        }
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    ctx.report(&out, || {
        for f in &out {
            println!(
                "{:<14} {:>8} KB  {}  {}",
                f.collection,
                f.size / 1024,
                f.modified.as_deref().unwrap_or("—"),
                f.path
            );
        }
        println!("\n{} file(s)", out.len());
    })
}

/// Walk one collection dir, keeping each file's path relative to it.
fn collect(
    dir: &Path,
    rel: &str,
    collection: &str,
    device: &str,
    out: &mut Vec<MiscFile>,
    depth: usize,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let child_rel = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if depth + 1 < MAX_DEPTH {
                collect(
                    &entry.path(),
                    &child_rel,
                    collection,
                    device,
                    out,
                    depth + 1,
                );
            }
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        out.push(MiscFile {
            collection: collection.to_string(),
            name: child_rel,
            path: entry.path().to_string_lossy().to_string(),
            size: meta.len(),
            modified: mtime_iso(&meta),
            device: device.to_string(),
        });
    }
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
