use std::io::{Read, Write};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use sidle_core::library::db::BookRow;
use sidle_core::library::editor::{self, EpubSession};

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
    }
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
