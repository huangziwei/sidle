use std::io::{Read, Write};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use sidle_core::library::db::BookRow;
use sidle_core::library::editor::{self, EpubSession, Operation};

use crate::ctx::Ctx;
use crate::select::Select;

#[derive(Args)]
pub struct EditArgs {
    #[command(flatten)]
    select: Select,
    #[command(subcommand)]
    op: EditOp,
}

#[derive(Subcommand)]
pub enum EditOp {
    /// List members.
    Ls,
    /// Print a member.
    Cat { member: String },
    /// Write a member from FILE or stdin; a missing member is added.
    Put {
        member: String,
        #[arg(long, value_name = "FILE")]
        from: Option<std::path::PathBuf>,
        /// Media type of an added member.
        #[arg(long, value_name = "TYPE")]
        media_type: Option<String>,
        #[arg(long)]
        no_reconvert: bool,
    },
    /// Print validator findings.
    Validate,
    /// Restore the publisher's stylesheets and class names from a sibling book; without --from, list the siblings that qualify.
    RestoreStyles {
        #[arg(long, value_name = "ID")]
        from: Option<i64>,
        #[arg(long)]
        dry_run: bool,
        /// Write even when the restoration changes computed styles.
        #[arg(long)]
        force: bool,
        /// Also write the restored EPUB to FILE.
        #[arg(long, value_name = "FILE")]
        out: Option<std::path::PathBuf>,
        #[arg(long)]
        no_reconvert: bool,
    },
    /// Rename a CSS class in every stylesheet, style block and class attribute.
    RenameClass {
        from: String,
        to: String,
        #[command(flatten)]
        apply: Apply,
    },
    /// Remove stylesheet rules no document can match.
    RemoveUnusedCss {
        #[command(flatten)]
        apply: Apply,
    },
    /// Re-indent XHTML and CSS members (one member, or every text member).
    Beautify {
        member: Option<String>,
        #[command(flatten)]
        apply: Apply,
    },
    /// Split a document before the block at LINE, moving ids, links, manifest and spine entries.
    SplitDocument {
        member: String,
        #[arg(long)]
        line: usize,
        #[arg(long, default_value_t = 1)]
        col: usize,
        #[command(flatten)]
        apply: Apply,
    },
    /// Fold the next spine document into MEMBER.
    MergeDocument {
        member: String,
        #[command(flatten)]
        apply: Apply,
    },
    /// Upgrade an EPUB 2 package to EPUB 3.
    Upgrade {
        #[command(flatten)]
        apply: Apply,
    },
}

#[derive(Args)]
pub struct Apply {
    /// Report the changes without writing.
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    no_reconvert: bool,
}

pub fn run(ctx: &Ctx, args: EditArgs) -> Result<()> {
    let book = one_book(ctx, &args.select)?;
    match args.op {
        EditOp::Ls => ls(ctx, &book),
        EditOp::Cat { member } => cat(&book, &member),
        EditOp::Put {
            member,
            from,
            media_type,
            no_reconvert,
        } => put(ctx, &book, &member, from, media_type, no_reconvert),
        EditOp::Validate => validate(ctx, &book),
        EditOp::RestoreStyles {
            from,
            dry_run,
            force,
            out,
            no_reconvert,
        } => restore_styles(ctx, &book, from, dry_run, force, out, no_reconvert),
        EditOp::RenameClass { from, to, apply } => {
            operate(ctx, &book, Operation::RenameClass { from, to }, apply)
        }
        EditOp::RemoveUnusedCss { apply } => operate(ctx, &book, Operation::RemoveUnusedCss, apply),
        EditOp::Beautify { member, apply } => {
            operate(ctx, &book, Operation::Beautify { member }, apply)
        }
        EditOp::SplitDocument {
            member,
            line,
            col,
            apply,
        } => operate(
            ctx,
            &book,
            Operation::SplitDocument { member, line, col },
            apply,
        ),
        EditOp::MergeDocument { member, apply } => {
            operate(ctx, &book, Operation::MergeWithNext { member }, apply)
        }
        EditOp::Upgrade { apply } => operate(ctx, &book, Operation::UpgradeEpub3, apply),
    }
}

fn operate(ctx: &Ctx, book: &BookRow, op: Operation, apply: Apply) -> Result<()> {
    let mut session = EpubSession::open(book)?;
    let outcome = session.apply(&op)?;
    let written = if apply.dry_run {
        Vec::new()
    } else {
        session.save(&ctx.conn())?
    };
    ctx.report(&outcome, || {
        println!("{}", outcome.operation);
        for n in &outcome.notes {
            println!("  {n}");
        }
        for m in &outcome.changed {
            println!("  changed  {}", m.path);
        }
        for m in &outcome.added {
            println!("  added    {}", m.path);
        }
        for p in &outcome.removed {
            println!("  removed  {p}");
        }
        if apply.dry_run {
            println!("dry run: nothing written");
        } else {
            println!("wrote {} member(s) to {}", written.len(), session.path());
        }
    })?;
    if !written.is_empty() && !apply.no_reconvert {
        crate::cmd::convert::run(
            ctx,
            crate::cmd::convert::ConvertArgs::sweep(
                Select {
                    ids: vec![book.id],
                    ..Default::default()
                },
                true,
                None,
            ),
        )?;
    }
    Ok(())
}

fn one_book(ctx: &Ctx, select: &Select) -> Result<BookRow> {
    let books = select.resolve_nonempty(&ctx.conn())?;
    match books.len() {
        1 => Ok(books.into_iter().next().unwrap()),
        n => anyhow::bail!("edit works on one book; the selection matched {n}"),
    }
}

fn ls(ctx: &Ctx, book: &BookRow) -> Result<()> {
    let session = EpubSession::open(book)?;
    let members = session.members()?;
    ctx.report(&members, || {
        for m in &members {
            let spine = m.spine_index.map(|i| format!("#{i}")).unwrap_or_default();
            println!(
                "{:>9}  {:<9} {:<4} {}{}",
                m.size,
                m.role,
                spine,
                m.path,
                m.label
                    .as_deref()
                    .map(|l| format!("  ({l})"))
                    .unwrap_or_default()
            );
        }
    })
}

fn cat(book: &BookRow, member: &str) -> Result<()> {
    let session = EpubSession::open(book)?;
    let bytes = session.read(member)?;
    std::io::stdout().write_all(bytes)?;
    Ok(())
}

fn put(
    ctx: &Ctx,
    book: &BookRow,
    member: &str,
    from: Option<std::path::PathBuf>,
    media_type: Option<String>,
    no_reconvert: bool,
) -> Result<()> {
    let bytes = match from {
        Some(p) => std::fs::read(&p).with_context(|| format!("read {}", p.display()))?,
        None => {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf)?;
            buf
        }
    };
    let mut session = EpubSession::open(book)?;
    let exists = session.members()?.iter().any(|m| m.path == member);
    if exists {
        session.write_bytes(member, bytes)?;
    } else {
        let mt = media_type
            .or_else(|| editor::media_type_for(member).map(str::to_string))
            .context("a new member needs --media-type")?;
        let id = session.add(member, &mt, bytes)?;
        ctx.say(format!("added {member} as manifest item {id}"));
    }
    let written = session.save(&ctx.conn())?;
    ctx.report(&written, || {
        println!("wrote {} member(s) to {}", written.len(), session.path())
    })?;
    if !no_reconvert {
        crate::cmd::convert::run(
            ctx,
            crate::cmd::convert::ConvertArgs::sweep(
                Select {
                    ids: vec![book.id],
                    ..Default::default()
                },
                true,
                None,
            ),
        )?;
    }
    Ok(())
}

fn validate(ctx: &Ctx, book: &BookRow) -> Result<()> {
    let session = EpubSession::open(book)?;
    let findings = session.validate()?;
    ctx.report(&findings, || {
        for f in &findings {
            println!(
                "{:<7} {}/{} @ {}: {}",
                f.severity, f.check, f.rule, f.location, f.message
            );
        }
        println!("{} finding(s)", findings.len());
    })
}

fn restore_styles(
    ctx: &Ctx,
    book: &BookRow,
    from: Option<i64>,
    dry_run: bool,
    force: bool,
    out: Option<std::path::PathBuf>,
    no_reconvert: bool,
) -> Result<()> {
    let Some(from) = from else {
        let list = editor::candidates(&ctx.conn(), book)?;
        return ctx.report(&list, || {
            if list.is_empty() {
                println!("no sibling book keeps the publisher's stylesheets");
            }
            for c in &list {
                let series = match (&c.series_name, c.series_index) {
                    (Some(s), Some(i)) => format!("  [{s} {i}]"),
                    (Some(s), None) => format!("  [{s}]"),
                    _ => String::new(),
                };
                println!("{:>6}  {}{}", c.id, c.title, series);
            }
        });
    };
    let report = {
        let conn = ctx.conn();
        let reference = sidle_core::library::db::get_book(&conn, from)?
            .with_context(|| format!("no book with id {from}"))?;
        editor::restore(&conn, book, &reference, !dry_run, force, out.as_deref())?
    };
    ctx.report(&report, || {
        println!(
            "{} document(s) restyled from {}{}",
            report.documents.len(),
            report.reference,
            if dry_run {
                " (dry run, nothing written)"
            } else {
                ""
            }
        );
        for (from, to) in &report.classes {
            if from != to {
                println!(
                    "  {from:<24} -> {}",
                    if to.is_empty() { "(dropped)" } else { to }
                );
            }
        }
        if !report.residual.is_empty() {
            println!("  kept with a residual rule: {}", report.residual.join(" "));
            for line in report.residual_css.lines().filter(|l| !l.is_empty()) {
                println!("    {line}");
            }
        }
        for d in &report.diffs {
            println!(
                "  {} {}: {} -> {} (x{}){}",
                d.document,
                d.property,
                d.before,
                d.after,
                d.count,
                if d.text.is_empty() {
                    String::new()
                } else {
                    format!("  at \"{}\"", d.text)
                }
            );
        }
        println!(
            "  {} material change(s); {}",
            report.material,
            match &report.blocked {
                Some(why) => why.as_str(),
                None if report.written => "written",
                None => "not written",
            }
        );
    })?;
    if report.written && !no_reconvert {
        crate::cmd::convert::run(
            ctx,
            crate::cmd::convert::ConvertArgs::sweep(
                Select {
                    ids: vec![book.id],
                    ..Default::default()
                },
                true,
                None,
            ),
        )?;
    }
    Ok(())
}
