//! Highlights, notes and bookmarks — read out, in bulk.

use anyhow::Result;
use clap::Args;
use serde::Serialize;
use sidle_core::library::ingest;

use crate::ctx::Ctx;
use crate::select::Select;

#[derive(Args)]
pub struct AnnotationsArgs {
    #[command(flatten)]
    select: Select,
    /// Write one file per book into this folder instead of printing.
    #[arg(long, value_name = "DIR")]
    dest: Option<std::path::PathBuf>,
    /// `markdown` (the readable form) or `json` (every field).
    #[arg(long, default_value = "markdown", value_name = "FORMAT")]
    format: String,
    /// Count them instead of reading them out.
    #[arg(long)]
    count: bool,
}

#[derive(Serialize)]
struct Count {
    book_id: i64,
    title: String,
    annotations: i64,
}

pub fn run(ctx: &Ctx, args: AnnotationsArgs) -> Result<()> {
    let conn = ctx.conn();
    let books = args.select.resolve_nonempty(&conn)?;

    if args.count {
        let mut counts = Vec::with_capacity(books.len());
        for b in &books {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM annotations WHERE book_id = ?1",
                [b.id],
                |r| r.get(0),
            )?;
            counts.push(Count {
                book_id: b.id,
                title: b.title.clone(),
                annotations: n,
            });
        }
        let total: i64 = counts.iter().map(|c| c.annotations).sum();
        return ctx.report(&counts, || {
            for c in counts.iter().filter(|c| c.annotations > 0) {
                println!("{:>6}  {:>5}  {}", c.book_id, c.annotations, c.title);
            }
            println!("\n{total} annotation(s) across {} book(s)", counts.len());
        });
    }

    let render = |book_id: i64| -> Result<String> {
        Ok(match args.format.as_str() {
            "markdown" => ingest::export_book_markdown(&conn, book_id)?,
            "json" => ingest::export_book_json(&conn, book_id)?,
            other => anyhow::bail!("unknown format {other:?} (markdown, json)"),
        })
    };

    match &args.dest {
        None => {
            for b in &books {
                println!("{}", render(b.id)?);
            }
            Ok(())
        }
        Some(dest) => {
            if !dest.is_dir() {
                anyhow::bail!("{} is not a folder", dest.display());
            }
            let ext = if args.format == "json" { "json" } else { "md" };
            let mut written = 0usize;
            for b in &books {
                let text = render(b.id)?;
                // A book with nothing marked in it has nothing to write; an
                // empty file per unread book would bury the ones that matter.
                if text.trim().is_empty() {
                    continue;
                }
                let name = sidle_core::library::paths::sanitize_segment(&b.title);
                let target = sidle_core::library::paths::dedup_path(dest.join(format!(
                    "{}.{ext}",
                    if name.is_empty() { "book" } else { &name }
                )));
                std::fs::write(&target, text)?;
                written += 1;
            }
            ctx.report(&written, || {
                println!("wrote {written} file(s) to {}", dest.display())
            })
        }
    }
}
