//! Time read, as the Kindle's own system logs recorded it.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;
use sidle_core::library::db;
use sidle_core::library::reading_log;

use crate::ctx::Ctx;

/// A range wide enough to mean "ever" against `YYYY-MM-DD` day keys.
const ALL_TIME: (&str, &str) = ("0000-00-00", "9999-99-99");

#[derive(Subcommand)]
pub enum ReadingLogCmd {
    /// Days read, hours, streaks.
    Overview,
    /// What was read, most recently read first.
    Books {
        /// `YYYY-MM-DD`; everything before it is left out.
        #[arg(long, value_name = "DAY")]
        from: Option<String>,
        #[arg(long, value_name = "DAY")]
        to: Option<String>,
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// Import a `logbackup/` folder copied off a Kindle.
    Import {
        #[arg(required = true, value_name = "DIR")]
        folders: Vec<PathBuf>,
        /// The Kindle these logs came from. Read out of the logs when absent —
        /// which is safer than naming one, because filing one Kindle's reading
        /// under another is invisible afterwards.
        #[arg(long, value_name = "SERIAL")]
        serial: Option<String>,
    },
    /// Sessions the log recorded against a position no book could be matched to.
    Unmatched,
    /// Attribute unmatched sessions at a position to a book.
    Attribute {
        #[arg(long, value_name = "POSITION")]
        position: i64,
        #[arg(long, value_name = "ID")]
        book_id: i64,
    },
    /// Forget every stored session. The device will not send them again.
    Clear {
        #[arg(long)]
        apply: bool,
    },
}

pub fn run(ctx: &Ctx, cmd: ReadingLogCmd) -> Result<()> {
    match cmd {
        ReadingLogCmd::Overview => overview(ctx),
        ReadingLogCmd::Books { from, to, limit } => books(ctx, from, to, limit),
        ReadingLogCmd::Import { folders, serial } => import(ctx, &folders, serial.as_deref()),
        ReadingLogCmd::Unmatched => unmatched(ctx),
        ReadingLogCmd::Attribute { position, book_id } => attribute(ctx, position, book_id),
        ReadingLogCmd::Clear { apply } => clear(ctx, apply),
    }
}

#[derive(Serialize)]
struct Overview {
    days_read: usize,
    total_seconds: i64,
    books: i64,
}

fn overview(ctx: &Ctx) -> Result<()> {
    let conn = ctx.conn();
    let days = db::reading_days(&conn, ALL_TIME.0, ALL_TIME.1)?;
    let overview = Overview {
        days_read: days.len(),
        total_seconds: days.iter().map(|(_, s)| s).sum(),
        books: db::reading_book_count(&conn)?,
    };
    ctx.report(&overview, || {
        println!(
            "{} book(s) across {} day(s), {}",
            overview.books,
            overview.days_read,
            hours(overview.total_seconds)
        );
        if let (Some(first), Some(last)) = (days.first(), days.last()) {
            println!("{} … {}", first.0, last.0);
        }
    })
}

fn books(ctx: &Ctx, from: Option<String>, to: Option<String>, limit: Option<usize>) -> Result<()> {
    let conn = ctx.conn();
    let mut entries = db::reading_books(
        &conn,
        from.as_deref().unwrap_or(ALL_TIME.0),
        to.as_deref().unwrap_or(ALL_TIME.1),
        db::ReadingSort::from_name(""),
        false,
        db::ReadingBucket::from_name(""),
    )?;
    if let Some(limit) = limit {
        entries.truncate(limit);
    }
    ctx.report(&entries, || {
        for e in &entries {
            println!(
                "{:>6}  {:>9}  {:<10}  {}",
                e.book_id,
                hours(e.seconds),
                e.last_at.get(..10).unwrap_or(&e.last_at),
                e.title
            );
        }
        println!("\n{} book(s)", entries.len());
    })
}

fn import(ctx: &Ctx, folders: &[PathBuf], serial: Option<&str>) -> Result<()> {
    for f in folders {
        if !f.exists() {
            anyhow::bail!("no such folder: {}", f.display());
        }
    }
    let conn = ctx.conn();
    // A person picking a serial from a list will eventually pick wrong, and the
    // mistake is invisible afterwards — so read it out of the archive unless the
    // caller insists.
    let serial = match serial {
        Some(s) => s.to_string(),
        None => match reading_log::identify(&conn, folders)? {
            reading_log::Origin::Recorded(serial) => serial,
            reading_log::Origin::Unrecognised => anyhow::bail!(
                "nothing here has been imported before, so the archive cannot say which \
                 Kindle wrote it — pass --serial with the one it came from"
            ),
            reading_log::Origin::Mixed(serials) => anyhow::bail!(
                "this folder holds logs from more than one Kindle ({}); importing it as \
                 either would misfile the other's reading",
                serials.join(", ")
            ),
        },
    };
    ctx.say(format!("importing as device {serial}"));

    let imported = reading_log::import(&conn, folders, &serial, &mut |r| {
        if r.total > 0 && r.done.is_multiple_of(50) {
            eprintln!("  {} {}/{} {}", r.phase, r.done, r.total, r.label);
        }
        std::ops::ControlFlow::Continue(())
    })?;
    ctx.report(&imported, || {
        if let Some(other) = &imported.conflict {
            println!(
                "nothing imported: these logs are device {other}'s, not {serial}'s — \
                 filing one Kindle's reading under another is worse than not importing"
            );
            return;
        }
        println!(
            "{} file(s) read, {} skipped, {} event(s) → {} session(s), {} attributed",
            imported.files,
            imported.skipped,
            imported.events,
            imported.sessions,
            imported.attributed
        );
        if imported.truncated > 0 {
            println!(
                "{} file(s) were cut short by the device and will be re-read if a \
                 complete copy arrives",
                imported.truncated
            );
        }
    })
}

fn unmatched(ctx: &Ctx) -> Result<()> {
    let conn = ctx.conn();
    let rows = db::unmatched_reading(&conn)?;
    ctx.report(&rows, || {
        for u in &rows {
            println!(
                "position {:>9}  {:>9}  {} session(s)  {} … {}",
                u.end_position,
                hours(u.seconds),
                u.sessions,
                u.first_at.get(..10).unwrap_or(&u.first_at),
                u.last_at.get(..10).unwrap_or(&u.last_at)
            );
        }
        println!(
            "\n{} position(s) no book could be matched to — a book with no position \
             axis can never be recognised, so `convert --all --force` measures them",
            rows.len()
        );
    })
}

fn attribute(ctx: &Ctx, position: i64, book_id: i64) -> Result<()> {
    let conn = ctx.conn();
    let n = db::attribute_reading_position(&conn, position, book_id)?;
    ctx.report(&n, || {
        println!("attributed {n} session(s) at position {position} to book {book_id}")
    })
}

fn clear(ctx: &Ctx, apply: bool) -> Result<()> {
    let conn = ctx.conn();
    if !apply {
        let days = db::reading_days(&conn, ALL_TIME.0, ALL_TIME.1)?;
        let seconds: i64 = days.iter().map(|(_, s)| s).sum();
        ctx.say(format!(
            "{} across {} day(s) would be forgotten. The Kindle does not send its \
             logs twice, so this cannot be undone by syncing again.\n\nRe-run with --apply.",
            hours(seconds),
            days.len()
        ));
        return Ok(());
    }
    let n = db::clear_reading_log(&conn)?;
    ctx.report(&n, || println!("forgot {n} row(s)"))
}

fn hours(seconds: i64) -> String {
    match seconds {
        s if s >= 3600 => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
        s => format!("{}m", s / 60),
    }
}
