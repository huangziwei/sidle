//! Highlights, notes and bookmarks — read out, in bulk.

use anyhow::Result;
use clap::Args;
use serde::Serialize;
use sidle_core::library::anchor::BookIndex;
use sidle_core::library::{ingest, reanchor};

use crate::ctx::Ctx;
use crate::select::Select;

#[derive(Args)]
pub struct AnnotationsArgs {
    #[command(flatten)]
    select: Select,
    /// Write one file per book into this folder.
    #[arg(long, value_name = "DIR")]
    dest: Option<std::path::PathBuf>,
    /// `markdown` (the readable form) or `json` (every field).
    #[arg(long, default_value = "markdown", value_name = "FORMAT")]
    format: String,
    /// Count them.
    #[arg(long)]
    count: bool,
    /// Put every anchor back on the book's own scale, without converting
    /// anything: a rebuilt book renumbers the position map under handles that
    /// did not move. `--write` applies what it prints.
    #[arg(long)]
    reanchor: bool,
    /// With `--reanchor`, write the repairs.
    #[arg(long)]
    write: bool,
}

/// One book's re-anchor pass.
#[derive(Serialize)]
struct Repaired {
    book_id: i64,
    title: String,
    author: String,
    /// Anchors the pass relocated onto the book's own text.
    moved: usize,
    /// Anchors whose stored positions it put back on the current scale.
    refreshed: usize,
    /// Anchors whose text is gone, or sits in several places. Left untouched.
    stranded: usize,
}

impl Repaired {
    /// Rows the pass wrote.
    fn written(&self) -> usize {
        self.moved + self.refreshed
    }
}

/// Re-anchor every selected book against its own KFX. Without `--write` the
/// pass runs in full and is rolled back.
fn reanchor(ctx: &Ctx, args: &AnnotationsArgs) -> Result<()> {
    let conn = ctx.conn();
    let books = args.select.resolve_nonempty(&conn)?;
    let tx = conn.unchecked_transaction()?;
    let mut done: Vec<Repaired> = Vec::new();

    for b in &books {
        let annotated: i64 = tx.query_row(
            "SELECT COUNT(*) FROM annotations WHERE book_id = ?1",
            [b.id],
            |r| r.get(0),
        )?;
        if annotated == 0 {
            continue;
        }
        let Some(kfx) = b.kfx_path.as_ref() else {
            ctx.say(format!("{}: no KFX to anchor against", b.title));
            continue;
        };
        let path = ctx.paths.root.join(kfx);
        let Some(index) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| BookIndex::from_kfx(&bytes))
        else {
            ctx.say(format!("{}: cannot read {}", b.title, path.display()));
            continue;
        };
        let pass = reanchor::book(&tx, b.id, &index)?;
        if pass.moved == 0 && pass.refreshed == 0 && pass.stranded == 0 {
            continue;
        }
        done.push(Repaired {
            book_id: b.id,
            title: b.title.clone(),
            author: b.author.clone(),
            moved: pass.moved,
            refreshed: pass.refreshed,
            stranded: pass.stranded,
        });
    }

    if args.write {
        tx.commit()?;
    } else {
        tx.rollback()?;
    }

    let rows: usize = done.iter().map(Repaired::written).sum();
    let stranded: usize = done.iter().map(|d| d.stranded).sum();
    let (books_touched, wrote) = (done.len(), args.write);
    ctx.report(&done, || {
        for d in &done {
            println!(
                "{:>6}  moved {:>3}  refreshed {:>3}  stranded {:>3}  {} — {}",
                d.book_id, d.moved, d.refreshed, d.stranded, d.title, d.author
            );
        }
        if books_touched == 0 {
            println!("every anchor is on its book's current scale");
            return;
        }
        if wrote {
            println!("\nwrote {rows} row(s) across {books_touched} book(s)");
        } else {
            println!("\n{rows} row(s) across {books_touched} book(s) — pass --write to apply");
        }
        if stranded > 0 {
            println!("{stranded} left untouched: their text is gone, or sits in several places");
        }
    })
}

#[derive(Serialize)]
struct Count {
    book_id: i64,
    title: String,
    annotations: i64,
}

pub fn run(ctx: &Ctx, args: AnnotationsArgs) -> Result<()> {
    if args.reanchor {
        return reanchor(ctx, &args);
    }
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
                // A book with nothing marked in it has nothing to write.
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
