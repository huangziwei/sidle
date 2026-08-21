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
use sidle_core::library::db;

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
    /// Register every app under a path, so a device push carries it.
    ///
    /// One path can hold several — sidle's own tree holds the picker and
    /// bokai — and all of them are registered. Re-adding an id repoints it.
    Add {
        /// Repo checkout or unpacked bundle.
        path: PathBuf,
    },
    /// What is registered, and what each source currently holds.
    List,
    /// Forget an app. Unregisters the source only: nothing on disk and
    /// nothing already on a device is touched.
    Remove { id: String },
}

/// Every verb that needs an open library. `inspect` does not, and main
/// dispatches it before the library is opened.
pub fn run(ctx: &Ctx, cmd: AppsCmd) -> Result<()> {
    match cmd {
        AppsCmd::Inspect { .. } => unreachable!("dispatched before the library opens"),
        AppsCmd::Add { path } => add(ctx, &path),
        AppsCmd::List => list(ctx),
        AppsCmd::Remove { id } => remove(ctx, &id),
    }
}

fn add(ctx: &Ctx, path: &std::path::Path) -> Result<()> {
    let path = crate::ctx::absolute(path.to_path_buf())?;
    let trees = apps::discover(&path)?;
    if trees.is_empty() {
        anyhow::bail!(
            "no extensions/<id>/app.json under {} — an app declares itself with \
             one, so a tree without it is not one sidle can install",
            path.display()
        );
    }
    let conn = ctx.conn();
    let mut added = Vec::new();
    for tree in &trees {
        db::upsert_app_source(
            &conn,
            &tree.spec.id,
            db::APP_SOURCE_LOCAL,
            &path.display().to_string(),
            &tree.root.display().to_string(),
        )?;
        added.push(summarize(tree, false));
    }
    ctx.report(&added, || {
        for app in &added {
            println!(
                "registered {} {} — {} file(s), {}",
                app.id,
                app.version,
                app.file_count,
                human(app.total_bytes)
            );
        }
    })
}

/// What each registered row currently holds, read fresh off disk. A row that
/// no longer resolves is reported rather than hidden: a moved checkout should
/// say so, not quietly stop being part of the fleet.
#[derive(Serialize)]
struct Registered {
    id: String,
    source: String,
    #[serde(flatten)]
    state: RegisteredState,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum RegisteredState {
    Present(Box<AppSummary>),
    Unreadable { error: String },
}

fn list(ctx: &Ctx) -> Result<()> {
    let rows = {
        let conn = ctx.conn();
        db::list_app_sources(&conn)?
    };
    let listed: Vec<Registered> = rows
        .iter()
        .map(|row| Registered {
            id: row.id.clone(),
            source: row.source.clone(),
            state: match read_registered(row) {
                Ok(tree) => RegisteredState::Present(Box::new(summarize(&tree, false))),
                Err(e) => RegisteredState::Unreadable {
                    error: format!("{e:#}"),
                },
            },
        })
        .collect();

    // What a push would actually carry: the registered rows plus the tree that
    // ships with this binary, flattened to one path-keyed list. Composing it
    // here is also the only way to see a path two apps both claim.
    let composed = crate::cmd::device::workspace_root()
        .map(|repo| {
            let source =
                sidle_core::library::device::deploy::DeploySource::from_workspace_root(&repo);
            let _ = source.stage_binary();
            apps::plan_from(&source.mount_dir, &rows)
        })
        .ok();

    ctx.report(&listed, || {
        if listed.is_empty() {
            println!("no apps registered — `sidle-cli apps add <path>`");
            println!("(the picker and bokai ship with the app and need no row)");
        }
        for app in &listed {
            match &app.state {
                RegisteredState::Present(s) => println!(
                    "{:<14} {:<10} {} file(s), {:<9} {}",
                    app.id,
                    s.version,
                    s.file_count,
                    human(s.total_bytes),
                    app.source
                ),
                RegisteredState::Unreadable { error } => {
                    println!("{:<14} {:<10} {}", app.id, "—", app.source);
                    println!("               {error}");
                }
            }
        }
        let Some(plan) = &composed else { return };
        let hashed: u64 = plan.sync_files().map(|f| f.size).sum();
        println!();
        println!(
            "a push carries {} app(s), {} file(s), {} — of which {} is read to \
             decide what changed",
            plan.apps.len(),
            plan.files.len(),
            human(plan.total_size()),
            human(hashed)
        );
        for c in &plan.conflicts {
            println!("  {} also claims {} — kept {}'s", c.dropped, c.path, c.kept);
        }
    })
}

fn read_registered(row: &db::AppSourceRow) -> Result<apps::AppTree> {
    let root = PathBuf::from(&row.root);
    let spec = apps::AppSpec::load(
        &root
            .join("extensions")
            .join(&row.id)
            .join(apps::APP_SPEC_FILE),
    )?;
    apps::walk(&root, &spec)
}

fn remove(ctx: &Ctx, id: &str) -> Result<()> {
    let gone = {
        let conn = ctx.conn();
        db::remove_app_source(&conn, id)?
    };
    ctx.report(&gone, || {
        if gone {
            println!("unregistered {id} — its files on disk and on any device are untouched");
        } else {
            println!("{id} was not registered");
        }
    })
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
