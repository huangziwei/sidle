//! The Kindle on the other end of the cable.
//!
//! Every one of these opens its own transport and closes it when the command
//! ends: there is no monitor thread here and no shared session to keep alive.
//! An MTP Kindle allows one session at a time, so a command will fail while the
//! desktop app holds the device — quit it, or unplug and replug.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;
use sidle_core::library::db::{self, BookRow};
use sidle_core::library::device::push::{DeleteResult, PushResult};
use sidle_core::library::device::{
    DeviceInfo, Transport, annotations, dedrm, deploy, detect, inventory, notebooks, push,
};
use sidle_core::library::paths::kfx_device_filename;

use crate::ctx::Ctx;
use crate::select::Select;

#[derive(Subcommand)]
pub enum DeviceCmd {
    /// Is a Kindle connected, and what is it?
    Status,
    /// What sidle has put on the device.
    List,
    /// Send books to the device.
    Send {
        #[command(flatten)]
        select: Select,
        /// Print what would be sent and send nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove books from the device — the file and the sidecars it spawned.
    /// The library keeps its copy.
    Delete {
        #[command(flatten)]
        select: Select,
        /// Remove a file by its on-device name instead of by library selection.
        /// Repeatable — this is how an orphan goes.
        #[arg(long = "filename", value_name = "NAME")]
        filenames: Vec<String>,
        #[arg(long)]
        apply: bool,
    },
    /// Bring across what the device recorded: highlights, notes, bookmarks,
    /// reading positions, handwriting, screenshots — and write sidle's own
    /// annotations back into its sidecars.
    Sync {
        /// Take everything again from scratch: undo every deletion made on this
        /// side and forget what was already pulled, so anything still on the
        /// device comes back.
        #[arg(long)]
        restore: bool,
    },
    /// Import notebooks from the device.
    Notebooks,
    /// Import books the device holds that this library doesn't.
    ImportOrphans {
        #[arg(long)]
        apply: bool,
    },
    /// Import from the jailbreak's `/dedrm` folder (mass-storage Kindles).
    Pull,
    /// The on-device picker app: what is installed, and what is stale.
    App(AppArgs),
    /// Unmount a mass-storage Kindle so it can be unplugged safely.
    Eject,
}

pub fn run(ctx: &Ctx, cmd: DeviceCmd) -> Result<()> {
    match cmd {
        DeviceCmd::Status => status(ctx),
        DeviceCmd::List => list(ctx),
        DeviceCmd::Send { select, dry_run } => send(ctx, &select, dry_run),
        DeviceCmd::Delete {
            select,
            filenames,
            apply,
        } => delete(ctx, &select, &filenames, apply),
        DeviceCmd::Sync { restore } => sync(ctx, restore),
        DeviceCmd::Notebooks => device_notebooks(ctx),
        DeviceCmd::ImportOrphans { apply } => import_orphans(ctx, apply),
        DeviceCmd::Pull => pull(ctx),
        DeviceCmd::App(args) => app(ctx, args),
        DeviceCmd::Eject => eject(ctx),
    }
}

/// The connected Kindle, or a refusal naming what to do about it.
fn require_device() -> Result<DeviceInfo> {
    detect::detect().ok_or_else(|| {
        anyhow::anyhow!(
            "no Kindle connected — plug one in, and quit the sidle app if it is running \
             (an MTP Kindle allows one USB session at a time)"
        )
    })
}

fn open(device: &DeviceInfo) -> Result<Box<dyn Transport>> {
    device.open_transport().context("open the device transport")
}

fn status(ctx: &Ctx) -> Result<()> {
    match detect::detect() {
        None => ctx.report(&Option::<DeviceInfo>::None, || {
            println!("no Kindle connected")
        }),
        Some(device) => ctx.report(&Some(&device), || {
            println!("{}", device.model.as_deref().unwrap_or("Kindle"));
            println!("  serial     {}", device.serial);
            if let Some(fw) = &device.firmware {
                println!("  firmware   {fw}");
            }
            match (device.free_bytes, device.total_bytes) {
                (Some(free), Some(total)) => println!(
                    "  space      {} GB free of {} GB",
                    free / 1_000_000_000,
                    total / 1_000_000_000
                ),
                (Some(free), None) => println!("  space      {} GB free", free / 1_000_000_000),
                _ => {}
            }
            match device.mass_storage_mount() {
                Some(mount) => println!("  transport  mass storage at {}", mount.display()),
                None => println!("  transport  MTP"),
            }
        }),
    }
}

fn list(ctx: &Ctx) -> Result<()> {
    let device = require_device()?;
    let transport = open(&device)?;
    let entries = inventory::list_ours(&ctx.conn(), transport.as_ref())?;
    ctx.report(&entries, || {
        for e in &entries {
            match e {
                inventory::Entry::Sent {
                    book_id,
                    title,
                    filename,
                    ..
                } => println!("{book_id:>6}  {title}\n        {filename}"),
                inventory::Entry::Orphan { filename, .. } => {
                    println!("     ·  (not in this library)\n        {filename}")
                }
            }
        }
        let orphans = entries
            .iter()
            .filter(|e| matches!(e, inventory::Entry::Orphan { .. }))
            .count();
        println!(
            "\n{} file(s) on the device, {orphans} not in this library",
            entries.len()
        );
    })
}

#[derive(Serialize)]
struct Sent {
    book_id: i64,
    title: String,
    outcome: String,
    detail: Option<String>,
}

fn send(ctx: &Ctx, select: &Select, dry_run: bool) -> Result<()> {
    let device = require_device()?;
    let books = {
        let conn = ctx.conn();
        select.resolve_nonempty(&conn)?
    };
    if dry_run {
        return ctx.report(&books, || {
            println!("{} book(s) would be sent:", books.len());
            for b in &books {
                println!("  [{}] {}", b.id, b.title);
            }
        });
    }

    let transport = open(&device)?;
    let conn = ctx.conn();
    let mut done = Vec::with_capacity(books.len());
    for (i, book) in books.iter().enumerate() {
        ctx.say(format!("[{}/{}] {}", i + 1, books.len(), book.title));
        let result = push::push_one(&device, transport.as_ref(), &conn, book, &|_, _| {})?;
        done.push(match result {
            PushResult::Pushed { book_id, filename } => Sent {
                book_id,
                title: book.title.clone(),
                outcome: "sent".into(),
                detail: Some(filename),
            },
            PushResult::AlreadyPresent { book_id, filename } => Sent {
                book_id,
                title: book.title.clone(),
                outcome: "already_there".into(),
                detail: Some(filename),
            },
            PushResult::Skipped { book_id, reason } => Sent {
                book_id,
                title: book.title.clone(),
                outcome: "skipped".into(),
                detail: Some(reason),
            },
            PushResult::Failed { book_id, error } => Sent {
                book_id,
                title: book.title.clone(),
                outcome: "failed".into(),
                detail: Some(error),
            },
        });
    }
    let tally = |what: &str| done.iter().filter(|d| d.outcome == what).count();
    let (sent, already, skipped, failed) = (
        tally("sent"),
        tally("already_there"),
        tally("skipped"),
        tally("failed"),
    );
    ctx.report(&done, || {
        println!("\nsent {sent}, already there {already}, skipped {skipped}, failed {failed}");
        for d in done.iter().filter(|d| d.outcome != "sent") {
            println!("  {}: {}", d.title, d.detail.as_deref().unwrap_or(""));
        }
    })
}

fn delete(ctx: &Ctx, select: &Select, filenames: &[String], apply: bool) -> Result<()> {
    let device = require_device()?;
    // Two ways to name a file: by the library row that produced it (the usual
    // case), or by its on-device name — which is the only handle an orphan has.
    let mut targets: Vec<(String, Option<BookRow>)> =
        filenames.iter().map(|f| (f.clone(), None)).collect();
    if !select.is_unset() {
        let conn = ctx.conn();
        for book in select.resolve_nonempty(&conn)? {
            match (&book.kfx_path, &book.kfx_sha256) {
                (Some(path), Some(sha)) => {
                    targets.push((kfx_device_filename(path, sha), Some(book)));
                }
                _ => ctx.say(format!(
                    "skipped {} — no converted KFX, so nothing of it is on the device",
                    book.title
                )),
            }
        }
    }
    if targets.is_empty() {
        anyhow::bail!("nothing named to delete — select books, or pass --filename");
    }
    if !apply {
        return ctx.report(&targets.iter().map(|(f, _)| f).collect::<Vec<_>>(), || {
            println!(
                "{} file(s) would be removed from the device:",
                targets.len()
            );
            for (f, _) in &targets {
                println!("  {f}");
            }
            println!("\nThe library keeps its copy. Re-run with --apply.");
        });
    }

    let transport = open(&device)?;
    let mut done = Vec::with_capacity(targets.len());
    for (filename, book) in &targets {
        let asin = book.as_ref().and_then(|b| b.asin.as_deref());
        done.push(push::delete_one(
            &device,
            transport.as_ref(),
            filename,
            asin,
        )?);
    }
    let removed = done
        .iter()
        .filter(|d| matches!(d, DeleteResult::Removed { .. }))
        .count();
    ctx.report(&done, || {
        println!("removed {removed} of {}", done.len());
        for d in &done {
            match d {
                DeleteResult::NotOurs { filename } => {
                    println!("  refused {filename} — not a file sidle put there")
                }
                DeleteResult::Failed { filename, error } => {
                    println!("  failed {filename}: {error}")
                }
                DeleteResult::Removed { .. } => {}
            }
        }
    })
}

fn sync(ctx: &Ctx, restore: bool) -> Result<()> {
    let device = require_device()?;
    let transport = open(&device)?;
    if restore {
        let conn = ctx.conn();
        let undone = db::clear_all_deletions(&conn)?;
        db::clear_device_sync_checkpoints(&conn, &device.serial)?;
        ctx.say(format!(
            "restoring: {undone} deletion(s) undone, and every sidecar will be read again"
        ));
    }
    let report = annotations::import_device_annotations(
        &device,
        transport.as_ref(),
        &ctx.db,
        &ctx.paths,
        &|stage, cur, total, label| {
            if cur == total || cur % 25 == 0 {
                eprintln!("  {stage} {cur}/{total} {label}");
            }
        },
    )?;
    ctx.report(&report, || {
        println!(
            "{} book(s) matched: {} annotation(s) new, {} unchanged, {} position(s)",
            report.matched, report.annotations.inserted, report.unchanged, report.positions
        );
        if report.ink_books > 0 {
            println!(
                "{} handwritten page(s) across {} book(s)",
                report.ink_pages, report.ink_books
            );
        }
        if report.pushed_books > 0 {
            println!(
                "wrote {} of sidle's annotation(s) back into {} device sidecar(s)",
                report.pushed_annotations, report.pushed_books
            );
        }
        if report.misc_new > 0 || report.misc_refreshed > 0 {
            println!(
                "backed up {} new file(s) and refreshed {} more",
                report.misc_new, report.misc_refreshed
            );
        }
        if !report.unmatched.is_empty() {
            println!(
                "{} annotated file(s) on the device match no book here",
                report.unmatched.len()
            );
        }
    })
}

fn device_notebooks(ctx: &Ctx) -> Result<()> {
    let device = require_device()?;
    let transport = open(&device)?;
    let summary = notebooks::import_device_notebooks(
        transport.as_ref(),
        &ctx.paths,
        &ctx.db,
        &|done, total| {
            if total > 0 && done % 5 == 0 {
                eprintln!("  {done}/{total}");
            }
        },
    )?;
    ctx.report(&summary, || {
        println!(
            "imported {}, unchanged {}, failed {}",
            summary.imported,
            summary.unchanged,
            summary.failed.len()
        );
        for f in &summary.failed {
            println!("  {f}");
        }
    })
}

fn import_orphans(ctx: &Ctx, apply: bool) -> Result<()> {
    let device = require_device()?;
    let transport = open(&device)?;
    let orphans: Vec<String> = inventory::list_ours(&ctx.conn(), transport.as_ref())?
        .into_iter()
        .filter_map(|e| match e {
            inventory::Entry::Orphan { filename, .. } => Some(filename),
            inventory::Entry::Sent { .. } => None,
        })
        .collect();
    if orphans.is_empty() {
        return ctx.report(&orphans, || {
            println!("every file on the device is already in this library")
        });
    }
    if !apply {
        return ctx.report(&orphans, || {
            println!("{} file(s) would be imported:", orphans.len());
            for f in &orphans {
                println!("  {f}");
            }
            println!("\nRe-run with --apply.");
        });
    }

    // The importer dispatches on extension, so an MTP pull has to become a real
    // file first; mass-storage could hand over a path, but staging both the same
    // way keeps one import path.
    let docs = sidle_core::library::device::TPath::parse("documents/Sidle");
    let staging = tempfile::tempdir().context("stage pulled files")?;
    let mut done = Vec::new();
    for (i, filename) in orphans.iter().enumerate() {
        ctx.say(format!("[{}/{}] {filename}", i + 1, orphans.len()));
        let bytes = transport.read(&docs.join(filename))?;
        let staged = staging.path().join(filename);
        std::fs::write(&staged, &bytes)?;
        let outcome = sidle_core::library::import::import_file(&ctx.conn(), &ctx.paths, &staged);
        done.push(match outcome {
            Ok(sidle_core::library::import::ImportOutcome::Imported { book, .. }) => {
                format!("imported [{}] {}", book.id, book.title)
            }
            Ok(sidle_core::library::import::ImportOutcome::Duplicate(book)) => {
                format!("already here [{}] {}", book.id, book.title)
            }
            Err(e) => format!("failed {filename}: {e:#}"),
        });
        ctx.say(format!("      {}", done.last().expect("just pushed")));
    }
    ctx.report(&done, || println!("\n{} file(s) handled", done.len()))
}

fn pull(ctx: &Ctx) -> Result<()> {
    let device = require_device()?;
    if device.mass_storage_mount().is_none() {
        anyhow::bail!("/dedrm exists only on a jailbroken mass-storage Kindle");
    }
    let candidates = dedrm::hash_dedrm_candidates(&device);
    let conn = ctx.conn();
    let fresh = dedrm::filter_new_candidates(&conn, candidates);
    if fresh.is_empty() {
        return ctx.report(&fresh, || println!("nothing new in /dedrm"));
    }
    let mut done = Vec::new();
    for (i, path) in fresh.iter().enumerate() {
        ctx.say(format!("[{}/{}] {}", i + 1, fresh.len(), path.display()));
        let (result, _needs_convert) = dedrm::pull_one(&conn, &ctx.paths, &device, path);
        done.push(result);
    }
    ctx.report(&done, || {
        println!("\npulled {} file(s)", done.len());
        println!("Run `sidle-cli convert --all` to produce their companion formats.");
    })
}

#[derive(Args)]
pub struct AppArgs {
    /// Write the stale files to the device.
    #[arg(long)]
    install: bool,
    /// Stage the self-update bundle instead, so a Kindle already carrying the
    /// picker can update itself over the LAN with no cable.
    #[arg(long, conflicts_with = "install")]
    stage: bool,
    /// LAN address the picker should reach the server at. Detected when absent.
    #[arg(long, value_name = "IP")]
    host: Option<String>,
    /// Port the picker should use.
    #[arg(long, default_value_t = 8080)]
    port: u16,
}

fn app(ctx: &Ctx, args: AppArgs) -> Result<()> {
    // Staging is host-side only — it copies the current picker build into the
    // directory the LAN server serves `/device/...` from, and needs no Kindle.
    if args.stage {
        let source = deploy::DeploySource::from_workspace_root(&workspace_root()?);
        let outcome = deploy::stage_dist(&source, &ctx.paths.device_dist())?;
        return ctx.report(&format!("{outcome:?}"), || println!("{outcome:?}"));
    }

    let device = require_device()?;
    let transport = open(&device)?;
    let source = deploy::DeploySource::from_workspace_root(&workspace_root()?);
    // The CA has to exist before anything can be said about `etc/ca.pem`, and
    // making one needs no server and no network.
    let _ = sidle_core::library::tls::ensure_ca(&ctx.paths);
    // `None` when no address was given and none can be detected: the
    // `etc/server.conf` slot then reports `SourceMissing` and the other six
    // install, which is what a push from a machine with no routable interface
    // is for. Rendering `HOST=` instead would write a conf the picker cannot
    // use and call it installed.
    let host = match args.host {
        Some(h) => Some(h),
        None => deploy::detect_lan_ipv4().map(|ip| ip.to_string()),
    };
    let conf = match host {
        Some(host) => Some(deploy::ServerConfRender {
            host,
            port: args.port,
            serial: device.serial.clone(),
            token: sidle_server::load_or_generate_token(&ctx.paths.root)?,
        }),
        None => None,
    };
    let ca_cert = ctx.paths.ca_cert();

    if !args.install {
        let status = deploy::compute_status(&source, conf.as_ref(), &ca_cert, transport.as_ref())?;
        return ctx.report(&status, || {
            println!("{:?}", status.overall);
            for f in &status.files {
                println!("  {:<40} {:?}", f.device_path, f.state);
            }
        });
    }

    let report = deploy::install_all(&source, conf.as_ref(), &ca_cert, transport.as_ref(), |r| {
        eprintln!("  {r:?}");
    })?;
    ctx.report(&report, || {
        println!("installed {} file(s)", report.results.len());
    })
}

/// The checkout this binary was built from, which is where the picker's own
/// binary and `device/` mirror live.
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
            .ok_or_else(|| anyhow::anyhow!("workspace root not found above {}", start.display()))?;
    }
}

fn eject(ctx: &Ctx) -> Result<()> {
    let device = require_device()?;
    let mount = device
        .mass_storage_mount()
        .ok_or_else(|| anyhow::anyhow!("an MTP Kindle has nothing to eject — just unplug it"))?;
    let out = std::process::Command::new("diskutil")
        .arg("eject")
        .arg(&mount)
        .output()
        .context("run diskutil eject")?;
    if !out.status.success() {
        anyhow::bail!(
            "diskutil eject failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    ctx.say(format!("ejected {}", mount.display()));
    Ok(())
}
