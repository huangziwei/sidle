//! The apps that install to a Kindle's `/mnt/us` — the picker, bokai, steb,
//! karyll, kfxdedrm-fe.
//!
//! Each is built by its own repo and publishes a mount-rooted tree carrying an
//! `app.json`. These verbs read that: point at a repo (or an unpacked bundle)
//! and see what sidle would install, which files an update would compare by
//! hash, and which are seeded once and left alone.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;
use sidle_core::library::apps::{self, AppTree, FileClass};

use crate::ctx::Ctx;

#[derive(Subcommand)]
pub enum AppsCmd {
    /// Read every app tree under a path and print what it would install.
    ///
    /// Works on a repo checkout, an unpacked release bundle, or the composed
    /// device tree — the shape is the same, only the depth differs.
    Inspect {
        /// Repo or bundle to read. Defaults to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// List every file with its class, not just the totals.
        #[arg(long)]
        files: bool,
    },
}

/// Every verb that needs an open library. `inspect` does not, and main
/// dispatches it before the library is opened.
pub fn run(_ctx: &Ctx, cmd: AppsCmd) -> Result<()> {
    match cmd {
        AppsCmd::Inspect { .. } => unreachable!("dispatched before the library opens"),
    }
}

/// What one app tree holds, flattened for `--json`.
#[derive(Serialize)]
struct AppSummary {
    id: String,
    name: String,
    version: String,
    root: String,
    tile: Option<String>,
    pidof: Option<String>,
    built_at: u64,
    file_count: usize,
    total_bytes: u64,
    /// Files an update decides by hash — the ones a status check has to read.
    sync_count: usize,
    sync_bytes: u64,
    /// Files written only when absent, and never read to decide.
    seed_count: usize,
    seed_bytes: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files: Vec<FileSummary>,
}

#[derive(Serialize)]
struct FileSummary {
    path: String,
    size: u64,
    class: FileClass,
    #[serde(skip_serializing_if = "is_zero")]
    seed_gen: u32,
    apply: apps::Apply,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn summarize(tree: &AppTree, with_files: bool) -> AppSummary {
    let (mut sync_count, mut sync_bytes, mut seed_count, mut seed_bytes) = (0, 0u64, 0, 0u64);
    for f in &tree.files {
        match f.policy.class {
            FileClass::Sync => {
                sync_count += 1;
                sync_bytes += f.size;
            }
            FileClass::Seed => {
                seed_count += 1;
                seed_bytes += f.size;
            }
            // Filtered out during the walk; it never reaches a file list.
            FileClass::Ignore => {}
        }
    }
    AppSummary {
        id: tree.spec.id.clone(),
        name: tree.spec.name.clone(),
        version: tree.spec.version.clone(),
        root: tree.root.display().to_string(),
        tile: tree.spec.tile.clone(),
        pidof: tree.spec.pidof.clone(),
        built_at: tree.built_at(),
        file_count: tree.files.len(),
        total_bytes: tree.total_size(),
        sync_count,
        sync_bytes,
        seed_count,
        seed_bytes,
        files: if with_files {
            tree.files
                .iter()
                .map(|f| FileSummary {
                    path: f.path.clone(),
                    size: f.size,
                    class: f.policy.class,
                    seed_gen: f.policy.seed_gen,
                    apply: f.policy.apply,
                })
                .collect()
        } else {
            Vec::new()
        },
    }
}

pub fn inspect(json: bool, path: &std::path::Path, with_files: bool) -> Result<()> {
    let path = crate::ctx::absolute(path.to_path_buf())?;
    let trees = apps::discover(&path)?;
    let summaries: Vec<AppSummary> = trees.iter().map(|t| summarize(t, with_files)).collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&summaries)?);
        return Ok(());
    }
    {
        if summaries.is_empty() {
            println!("no app.json under {}", path.display());
            return Ok(());
        }
        for app in &summaries {
            println!("{} {} — {}", app.id, app.version, app.name);
            println!("  root      {}", app.root);
            println!("  tile      {}", app.tile.as_deref().unwrap_or("none"));
            println!(
                "  files     {} ({})",
                app.file_count,
                human(app.total_bytes)
            );
            // The split that decides what a status check costs: `sync` is read
            // and hashed every time, `seed` is only asked whether it exists.
            println!(
                "  hashed    {} ({})   seeded {} ({})",
                app.sync_count,
                human(app.sync_bytes),
                app.seed_count,
                human(app.seed_bytes)
            );
            for f in &app.files {
                let generation = if f.seed_gen > 0 {
                    format!(" gen {}", f.seed_gen)
                } else {
                    String::new()
                };
                let apply = match f.apply {
                    apps::Apply::Direct => "",
                    apps::Apply::Staged => " staged",
                };
                println!(
                    "    {:<10} {:>9}  {}{generation}{apply}",
                    format!("{:?}", f.class).to_lowercase(),
                    human(f.size),
                    f.path
                );
            }
            println!();
        }
    }
    Ok(())
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "K", "M", "G"];
    let mut n = bytes as f64;
    let mut unit = 0;
    while n >= 1024.0 && unit < UNITS.len() - 1 {
        n /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{n:.1} {}", UNITS[unit])
    }
}
