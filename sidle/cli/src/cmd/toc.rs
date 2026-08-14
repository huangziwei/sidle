//! Tables of contents, across a whole shelf.
//!
//! The app judges and repairs one book's TOC in the editor's TOC panel. The same
//! two operations over a selection are what turns "this book's chapter list is
//! broken" into a thing you can find and fix everywhere at once.

use anyhow::Result;
use clap::Args;
use serde::Serialize;
use sidle_core::library::toc;

use crate::ctx::Ctx;
use crate::select::Select;

#[derive(Args)]
pub struct TocArgs {
    #[command(flatten)]
    select: Select,
    /// Rebuild the table of contents of every selected book whose verdict isn't
    /// OK, from what the book itself declares.
    #[arg(long)]
    repair: bool,
    /// Repair without re-deriving the KFX afterwards. An EPUB- or PDF-sourced
    /// book keeps its old KFX until the next `convert`, so the Kindle would read
    /// the unrepaired copy.
    #[arg(long)]
    no_reconvert: bool,
    /// Print what would be repaired and change nothing.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Serialize)]
struct Judged {
    book_id: i64,
    title: String,
    #[serde(flatten)]
    verdict: Option<toc::Verdict>,
    /// The verdict after a repair, when one ran.
    repaired_to: Option<String>,
    error: Option<String>,
}

pub fn run(ctx: &Ctx, args: TocArgs) -> Result<()> {
    let books = {
        let conn = ctx.conn();
        args.select.resolve_nonempty(&conn)?
    };

    let mut judged: Vec<Judged> = books
        .iter()
        .map(|b| match toc::audit(b) {
            Ok(verdict) => Judged {
                book_id: b.id,
                title: b.title.clone(),
                verdict: Some(verdict),
                repaired_to: None,
                error: None,
            },
            Err(e) => Judged {
                book_id: b.id,
                title: b.title.clone(),
                verdict: None,
                repaired_to: None,
                error: Some(format!("{e:#}")),
            },
        })
        .collect();

    if !args.repair {
        return ctx.report(&judged, || {
            for j in &judged {
                match (&j.verdict, &j.error) {
                    (Some(v), _) => println!(
                        "{:>6}  {:<10} {:>4} entries, {:>4} chapters  {}",
                        j.book_id, v.verdict, v.entries, v.chapters, j.title
                    ),
                    (None, Some(e)) => println!("{:>6}  {:<10} {}  ({e})", j.book_id, "—", j.title),
                    (None, None) => {}
                }
            }
            summarize(&judged);
        });
    }

    // A book whose TOC is already in good order is left alone: a repair rewrites
    // the source file, and rewriting a file to change nothing is not free.
    let broken: Vec<usize> = judged
        .iter()
        .enumerate()
        .filter(|(_, j)| j.verdict.as_ref().is_some_and(|v| !v.is_ok()))
        .map(|(i, _)| i)
        .collect();

    if args.dry_run {
        return ctx.report(&judged, || {
            println!("{} book(s) would be repaired:", broken.len());
            for &i in &broken {
                println!("  [{}] {}", judged[i].book_id, judged[i].title);
            }
        });
    }
    if broken.is_empty() {
        return ctx.report(&judged, || {
            println!("every selected book's TOC is in order")
        });
    }

    let mut reconvert: Vec<i64> = Vec::new();
    for (n, &i) in broken.iter().enumerate() {
        let book = &books[i];
        ctx.say(format!("[{}/{}] {}", n + 1, broken.len(), book.title));
        let outcome = {
            let conn = ctx.conn();
            toc::repair(&conn, book)
        };
        match outcome {
            Ok(after) => {
                ctx.say(format!(
                    "      {} → {}",
                    verdict_of(&judged[i]),
                    after.verdict
                ));
                judged[i].repaired_to = Some(after.verdict);
                reconvert.push(book.id);
            }
            Err(e) => {
                eprintln!("      failed: {e:#}");
                judged[i].error = Some(format!("{e:#}"));
            }
        }
    }

    let repaired = judged.iter().filter(|j| j.repaired_to.is_some()).count();
    let still_bad = judged
        .iter()
        .filter(|j| j.repaired_to.as_deref().is_some_and(|v| v != "OK"))
        .count();
    ctx.report(&judged, || {
        println!("\nrepaired {repaired}; {still_bad} still not OK afterwards");
    })?;

    // An EPUB or PDF source derives the KFX the Kindle reads, so the repair only
    // reaches the device once that is rebuilt. A KFX source was edited in place
    // and needs nothing — but it costs one skipped book to let the sweep decide
    // that per row rather than guessing here.
    if !reconvert.is_empty() && !args.no_reconvert {
        crate::cmd::convert::run(
            ctx,
            crate::cmd::convert::ConvertArgs::sweep(
                Select {
                    ids: reconvert,
                    ..Default::default()
                },
                true,
                None,
            ),
        )?;
    }
    Ok(())
}

fn verdict_of(j: &Judged) -> String {
    j.verdict
        .as_ref()
        .map(|v| v.verdict.clone())
        .unwrap_or_default()
}

fn summarize(judged: &[Judged]) {
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for j in judged {
        let key = j
            .verdict
            .as_ref()
            .map_or("not judged", |v| v.verdict.as_str());
        *counts.entry(key).or_default() += 1;
    }
    let line: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
    println!("\n{} book(s): {}", judged.len(), line.join(", "));
}
