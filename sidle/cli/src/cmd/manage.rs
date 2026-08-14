//! The library as a whole: where it lives, and how it is copied.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;
use sidle_core::library::paths::LibraryPaths;
use sidle_core::library::{backup, db, merge, relocate};

use crate::ctx::Ctx;

#[derive(Subcommand)]
pub enum LibraryCmd {
    /// Create an empty library at the `--root` this run was given, so a sweep
    /// can be tried somewhere that isn't the real one.
    Init,
    /// Where the library lives.
    Location,
    /// Write a full backup archive.
    Backup {
        /// The `.sidlebak` to write.
        dest: PathBuf,
    },
    /// Replace this library with the contents of an archive.
    Restore {
        src: PathBuf,
        /// Leave the current library beside the new one as an undo, instead of
        /// deleting it once the restore is in place.
        #[arg(long)]
        keep_previous: bool,
        #[arg(long)]
        apply: bool,
    },
    /// Take in the books an archive holds that this library doesn't.
    Merge {
        src: PathBuf,
        #[arg(long)]
        apply: bool,
    },
    /// Move the library to another folder, or adopt one already there.
    Relocate {
        /// Move this library's files to DIR (must be empty).
        #[arg(long, value_name = "DIR", conflicts_with = "use_existing")]
        move_to: Option<PathBuf>,
        /// Point sidle at the library already at DIR, copying nothing.
        #[arg(long = "use", value_name = "DIR")]
        use_existing: Option<PathBuf>,
    },
}

/// Create the library `root` names. Runs before anything is opened — there is
/// nothing there yet to open.
pub fn init(root: Option<PathBuf>) -> Result<()> {
    let root = root.ok_or_else(|| {
        anyhow::anyhow!(
            "say where: `sidle-cli --root <DIR> library init`. \
             The configured library is created by the app itself."
        )
    })?;
    let paths = LibraryPaths {
        root: crate::ctx::absolute(root)?,
    };
    if paths.db().exists() {
        anyhow::bail!("{} already holds a library", paths.root.display());
    }
    paths.ensure()?;
    db::open(&paths.db())?;
    println!("created an empty library at {}", paths.root.display());
    Ok(())
}

pub fn run(ctx: &Ctx, cmd: LibraryCmd) -> Result<()> {
    match cmd {
        LibraryCmd::Init => unreachable!("handled before the library is opened"),
        LibraryCmd::Location => location(ctx),
        LibraryCmd::Backup { dest } => run_backup(ctx, &dest),
        LibraryCmd::Restore {
            src,
            keep_previous,
            apply,
        } => restore(ctx, &src, keep_previous, apply),
        LibraryCmd::Merge { src, apply } => run_merge(ctx, &src, apply),
        LibraryCmd::Relocate {
            move_to,
            use_existing,
        } => relocate_cmd(ctx, move_to, use_existing),
    }
}

#[derive(Serialize)]
struct Location {
    root: String,
    is_default: bool,
    db: String,
}

fn location(ctx: &Ctx) -> Result<()> {
    let default = LibraryPaths::default_root()?.root;
    let location = Location {
        is_default: ctx.paths.root == default,
        root: ctx.paths.root.to_string_lossy().to_string(),
        db: ctx.paths.db().to_string_lossy().to_string(),
    };
    ctx.report(&location, || {
        println!("{}", location.root);
        if !location.is_default {
            println!("(the default location is {})", default.display());
        }
    })
}

fn run_backup(ctx: &Ctx, dest: &std::path::Path) -> Result<()> {
    let conn = ctx.conn();
    let manifest = backup::create(
        &conn,
        &ctx.paths.root.join("books"),
        &ctx.paths.root,
        env!("CARGO_PKG_VERSION"),
        dest,
    )?;
    ctx.report(&manifest.counts, || {
        println!(
            "wrote {} — {} books, {} annotations",
            dest.display(),
            manifest.counts.books,
            manifest.counts.annotations
        );
    })
}

fn restore(ctx: &Ctx, src: &std::path::Path, keep_previous: bool, apply: bool) -> Result<()> {
    if !src.is_file() {
        anyhow::bail!("no archive at {}", src.display());
    }
    if !apply {
        ctx.say(format!(
            "This replaces the library at {} with the contents of {}.\n{}\n\nRe-run with --apply.",
            ctx.paths.root.display(),
            src.display(),
            if keep_previous {
                "The current library is kept beside the new one as an undo."
            } else {
                "The current library is deleted once the restore is in place; \
                 the archive is then the only other copy."
            }
        ));
        return Ok(());
    }
    // A conversion writing into a library being swapped out would land in the
    // set-aside copy and be lost.
    let pending = db::pending_or_error_book_ids(&ctx.conn())?;
    if !pending.is_empty() {
        anyhow::bail!(
            "{} book(s) are mid-conversion — finish or clear them first \
             (`sidle-cli jobs --all`)",
            pending.len()
        );
    }
    let previous = if keep_previous {
        backup::PreviousLibrary::Keep
    } else {
        backup::PreviousLibrary::Discard
    };
    let outcome = backup::restore(src, &ctx.paths.root, db::SCHEMA_VERSION, previous)?;
    ctx.say(format!("restored {} book(s)", outcome.books));
    if let Some(p) = &outcome.safety_copy {
        ctx.say(format!("the previous library is at {}", p.display()));
    }
    Ok(())
}

fn run_merge(ctx: &Ctx, src: &std::path::Path, apply: bool) -> Result<()> {
    if !src.is_file() {
        anyhow::bail!("no archive at {}", src.display());
    }
    // The archive is staged either way: what it holds that this library does not
    // can only be known by reading it, and nothing is written until `commit`.
    let prepared = merge::prepare(src, &ctx.paths.root, db::SCHEMA_VERSION)?;
    if prepared.is_empty() {
        return ctx.report(&false, || {
            println!(
                "{} holds nothing this library does not already have",
                src.display()
            )
        });
    }
    if !apply {
        return ctx.report(&true, || {
            println!(
                "{} holds books or notebooks this library does not.\n\nRe-run with --apply.",
                src.display()
            );
        });
    }
    let outcome = merge::commit(&ctx.conn(), &prepared)?;
    ctx.report(&outcome, || {
        println!(
            "took in {} book(s) and {} notebook(s); {} book(s) already here were \
             updated from the newer copy",
            outcome.books_added, outcome.notebooks_added, outcome.books_updated
        );
        if outcome.annotations_added > 0 || outcome.ink_added > 0 {
            println!(
                "{} annotation(s) and {} ink page(s) new to this library",
                outcome.annotations_added, outcome.ink_added
            );
        }
    })
}

fn relocate_cmd(ctx: &Ctx, move_to: Option<PathBuf>, use_existing: Option<PathBuf>) -> Result<()> {
    match (move_to, use_existing) {
        (Some(dest), None) => {
            if dest == ctx.paths.root {
                anyhow::bail!("that is already the library's location");
            }
            let pending = db::pending_or_error_book_ids(&ctx.conn())?;
            if !pending.is_empty() {
                anyhow::bail!(
                    "{} book(s) are mid-conversion — their output would be stranded in the \
                     old folder; finish or clear them first",
                    pending.len()
                );
            }
            let copied = relocate::move_library(&ctx.conn(), &ctx.paths.root, &dest)?;
            // Repoint first, then clear the old remnants: the destructive half
            // runs only once the new root is the live one.
            LibraryPaths::set_root(&dest)?;
            let state_dir = LibraryPaths::state_dir()?;
            relocate::finish_move(&ctx.paths.root, &state_dir, &copied);
            ctx.say(format!("the library is now at {}", dest.display()));
            Ok(())
        }
        (None, Some(dir)) => {
            if dir == ctx.paths.root {
                anyhow::bail!("that is already the library's location");
            }
            let books = relocate::validate_existing(&dir)?;
            LibraryPaths::set_root(&dir)?;
            ctx.say(format!(
                "now using the library at {} ({books} books)",
                dir.display()
            ));
            Ok(())
        }
        _ => anyhow::bail!("say which: --move-to <DIR>, or --use <DIR>"),
    }
}
