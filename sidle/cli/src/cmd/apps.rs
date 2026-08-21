//! `AppsCmd`: read an app tree off a path, register it, and list what is
//! registered. An app tree is mount-rooted — its paths install under `/mnt/us`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;
use sidle_core::library::apps::{self, AppTree, Apply};
use sidle_core::library::db;

use crate::ctx::Ctx;

#[derive(Subcommand)]
pub enum AppsCmd {
    /// Read every app tree under `path` and print what it installs. Takes a
    /// repo checkout, an unpacked release bundle, or the composed device tree.
    Inspect {
        /// Repo or bundle to read. Defaults to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// List every file, not just the totals.
        #[arg(long)]
        files: bool,
    },
    /// Register every app in `target`; a device push carries what is
    /// registered. One target can hold several. Re-adding an id repoints it.
    Add {
        /// A repo checkout or unpacked bundle on this machine, or the
        /// `owner/repo` of a GitHub repo publishing release bundles. A path
        /// that exists is read as a path.
        target: String,
        /// Take this release instead of the repo's latest.
        #[arg(long, value_name = "TAG")]
        tag: Option<String>,
    },
    /// What is registered, and what each source holds.
    List,
    /// Forget an app. Drops its row; nothing on disk and nothing on a device
    /// is touched.
    Remove { id: String },
}

/// Every verb that needs an open library. `inspect` does not, and main
/// dispatches it before the library is opened.
pub fn run(ctx: &Ctx, cmd: AppsCmd) -> Result<()> {
    match cmd {
        AppsCmd::Inspect { .. } => unreachable!("dispatched before the library opens"),
        AppsCmd::Add { target, tag } => add(ctx, &target, tag.as_deref()),
        AppsCmd::List => list(ctx),
        AppsCmd::Remove { id } => remove(ctx, &id),
    }
}

/// The message `path` gets when it holds no app tree.
fn no_apps_here(path: &std::path::Path) -> String {
    format!(
        "no extensions/<id>/ under {} — an app is a directory of files that \
         install to /mnt/us/extensions/, so a folder with no such tree is not \
         one sidle can install",
        path.display()
    )
}

/// Register a folder on this machine, or the latest release of a GitHub repo.
/// A `target` that names an existing path is a path, whatever else it looks
/// like.
fn add(ctx: &Ctx, target: &str, tag: Option<&str>) -> Result<()> {
    let path = PathBuf::from(target);
    if tag.is_none() && path.exists() {
        return add_local(ctx, &path);
    }
    if path.exists() {
        anyhow::bail!("--tag names a release; {target} is a folder on this machine");
    }
    add_release(ctx, target, tag)
}

fn add_local(ctx: &Ctx, path: &std::path::Path) -> Result<()> {
    let path = crate::ctx::absolute(path.to_path_buf())?;
    let trees = apps::discover(&path)?;
    if trees.is_empty() {
        anyhow::bail!("{}", no_apps_here(&path));
    }
    register(
        ctx,
        &trees,
        db::APP_SOURCE_LOCAL,
        &path.display().to_string(),
    )
}

fn add_release(ctx: &Ctx, source: &str, tag: Option<&str>) -> Result<()> {
    let fetched = apps::release::fetch(&ctx.paths, source, tag)?;
    ctx.say(match fetched.downloaded {
        true => format!("fetched {} {}", fetched.repo, fetched.tag),
        false => format!("{} {} was already unpacked", fetched.repo, fetched.tag),
    });
    register(
        ctx,
        &fetched.apps,
        db::APP_SOURCE_RELEASE,
        &fetched.repo.to_string(),
    )
}

/// One row per tree, all pointing at the same source.
fn register(ctx: &Ctx, trees: &[AppTree], kind: &str, source: &str) -> Result<()> {
    let conn = ctx.conn();
    let mut added = Vec::new();
    for tree in trees {
        db::upsert_app_source(
            &conn,
            &tree.app.id,
            kind,
            source,
            &tree.root.display().to_string(),
        )?;
        added.push(summarize(tree, false));
    }
    ctx.report(&added, || {
        for app in &added {
            println!(
                "registered {} {} — {} file(s), {}",
                app.id,
                app.version.as_deref().unwrap_or("(no version)"),
                app.file_count,
                human(app.total_bytes)
            );
        }
    })
}

/// What each registered row holds, read off disk. A row whose source fails to
/// resolve carries its error.
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
            state: match apps::walk(std::path::Path::new(&row.root), &row.id) {
                Ok(tree) => RegisteredState::Present(Box::new(summarize(&tree, false))),
                Err(e) => RegisteredState::Unreadable {
                    error: format!("{e:#}"),
                },
            },
        })
        .collect();

    // `plan_from` flattens the registered rows and the mount tree beside this
    // binary into one path-keyed list, naming any path two apps claim.
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
            println!("no apps registered — `sidle-cli apps add <path|owner/repo>`");
            println!("(the picker and bokai ship with the app and need no row)");
        }
        for app in &listed {
            match &app.state {
                RegisteredState::Present(s) => println!(
                    "{:<14} {:<10} {} file(s), {:<9} {}",
                    app.id,
                    s.version.as_deref().unwrap_or("—"),
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
        println!();
        println!(
            "a push carries {} app(s), {} file(s), {}",
            plan.apps.len(),
            plan.files.len(),
            human(plan.total_size())
        );
        for c in &plan.conflicts {
            println!("  {} also claims {} — kept {}'s", c.dropped, c.path, c.kept);
        }
    })
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
    version: Option<String>,
    root: String,
    tile: Option<String>,
    built_at: u64,
    file_count: usize,
    total_bytes: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files: Vec<FileSummary>,
}

#[derive(Serialize)]
struct FileSummary {
    path: String,
    size: u64,
    apply: Apply,
}

fn summarize(tree: &AppTree, with_files: bool) -> AppSummary {
    AppSummary {
        id: tree.app.id.clone(),
        name: tree.app.name.clone(),
        version: tree.app.version.clone(),
        root: tree.root.display().to_string(),
        tile: tree.app.tile.clone(),
        built_at: tree.built_at(),
        file_count: tree.files.len(),
        total_bytes: tree.total_size(),
        files: if with_files {
            tree.files
                .iter()
                .map(|f| FileSummary {
                    path: f.path.clone(),
                    size: f.size,
                    apply: f.apply,
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
    if summaries.is_empty() {
        println!("{}", no_apps_here(&path));
        return Ok(());
    }
    for app in &summaries {
        println!(
            "{} {} — {}",
            app.id,
            app.version.as_deref().unwrap_or("(no version)"),
            app.name
        );
        println!("  root      {}", app.root);
        println!("  tile      {}", app.tile.as_deref().unwrap_or("none"));
        println!(
            "  files     {} ({})",
            app.file_count,
            human(app.total_bytes)
        );
        for f in &app.files {
            let apply = match f.apply {
                Apply::Direct => "",
                Apply::Staged => "  staged",
            };
            println!("    {:>9}  {}{apply}", human(f.size), f.path);
        }
        println!();
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
