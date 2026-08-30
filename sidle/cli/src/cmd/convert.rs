//! Converting many books at once.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use clap::Args;
use serde::Serialize;
use sidle_core::library::convert::{self, Mode};
use sidle_core::library::db::{self, Access, BookRow};
use sidle_core::library::progress::{Throttle, fraction};

use crate::ctx::Ctx;
use crate::progress::{Bar, stdout_is_terminal};
use crate::select::Select;

#[derive(Args)]
pub struct ConvertArgs {
    #[command(flatten)]
    select: Select,

    /// Convert every selected book, output or no output — the sweep to run
    /// after a bokai change. Source→target only: the import-time cover
    /// enrichment is skipped, leaving the source KFX and its `kfx_sha256`.
    #[arg(long)]
    force: bool,

    /// How many books to convert at once. Defaults to the machine's cores.
    #[arg(long, short = 'j', value_name = "N")]
    jobs: Option<usize>,

    /// List the selection and stop.
    #[arg(long)]
    dry_run: bool,

    /// Keep going after a book fails (the default). With `--stop-on-error` the
    /// sweep ends at the first failure, leaving the rest untouched.
    #[arg(long)]
    stop_on_error: bool,
}

impl ConvertArgs {
    /// The sweep another command ends with: convert exactly these books, now.
    pub fn sweep(select: Select, force: bool, jobs: Option<usize>) -> Self {
        Self {
            select,
            force,
            jobs,
            dry_run: false,
            stop_on_error: false,
        }
    }
}

/// What one book's conversion did.
#[derive(Serialize)]
struct Done {
    book_id: i64,
    title: String,
    kind: String,
    seconds: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    reanchored: usize,
    stranded: usize,
}

#[derive(Serialize)]
struct Report {
    converted: usize,
    failed: usize,
    skipped: usize,
    books: Vec<Done>,
}

pub fn run(ctx: &Ctx, args: ConvertArgs) -> Result<()> {
    // The workers take `ctx.db` themselves, a book at a time. A caller holding
    // the guard leaves them waiting on a lock nothing releases.
    if ctx.db.try_lock().is_err() {
        anyhow::bail!("the library connection is still held by the caller of this sweep");
    }
    let books = {
        let conn = ctx.conn();
        args.select.resolve(&conn)?
    };

    // A `kind` of `None` names no direction to dispatch on, which counts as
    // skipped.
    let (mut work, no_kind): (Vec<BookRow>, Vec<BookRow>) =
        books.into_iter().partition(|b| b.kind.is_some());
    // Without `--force` the sweep finishes what is outstanding: a `done` row
    // is left alone.
    let mut skipped = no_kind.len();
    if !args.force {
        let before = work.len();
        work.retain(|b| b.status != "done");
        skipped += before - work.len();
    }
    for b in &no_kind {
        ctx.say(format!("skipped {} — no conversion recorded", b.title));
    }

    if args.dry_run {
        let report = Report {
            converted: 0,
            failed: 0,
            skipped: skipped + work.len(),
            books: work
                .iter()
                .map(|b| Done {
                    book_id: b.id,
                    title: b.title.clone(),
                    kind: b.kind.clone().unwrap_or_default(),
                    seconds: 0.0,
                    error: None,
                    reanchored: 0,
                    stranded: 0,
                })
                .collect(),
        };
        return ctx.report(&report, || {
            println!("{} books would be converted:", work.len());
            for b in &work {
                println!(
                    "  [{}] {} ({})",
                    b.id,
                    b.title,
                    b.kind.as_deref().unwrap_or("?")
                );
            }
        });
    }

    if work.is_empty() {
        return ctx.report(
            &Report {
                converted: 0,
                failed: 0,
                skipped,
                books: Vec::new(),
            },
            || println!("nothing to convert (pass --force to reconvert books already done)"),
        );
    }

    let jobs = args.jobs.unwrap_or_else(default_jobs).max(1);
    let mode = if args.force {
        Mode::Reconvert
    } else {
        Mode::Import
    };
    ctx.say(format!(
        "converting {} book(s) on {jobs} worker(s){}",
        work.len(),
        if args.force { ", forced" } else { "" }
    ));

    let total = work.len();
    let slots = jobs.min(total);
    let bar = Bar::new(
        "converting",
        total,
        slots,
        !ctx.json && stdout_is_terminal(),
    );
    let queue = Mutex::new(work);
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Done>> = Mutex::new(Vec::new());
    let stop = std::sync::atomic::AtomicBool::new(false);

    let stop_on_error = args.stop_on_error;
    std::thread::scope(|scope| {
        for slot in 0..slots {
            // `slot` is this worker's own, so the closure takes it by value and
            // every shared operand by reference.
            let (bar, queue, next, results, stop) = (&bar, &queue, &next, &results, &stop);
            scope.spawn(move || {
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let Some(book) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop() else {
                        return;
                    };
                    let n = next.fetch_add(1, Ordering::Relaxed) + 1;
                    let kind = book.kind.clone().unwrap_or_default();
                    bar.start(slot, &book.title);
                    let started = std::time::Instant::now();
                    // The phase report becomes this slot's share of the bar.
                    // `Throttle` drops the movement a redraw cannot show.
                    let throttle = Throttle::new();
                    let tick = |phase: &str, cur: usize, steps: usize, _label: &str| {
                        let f = fraction(&kind, phase, cur, steps);
                        if throttle.worth_emitting(f) {
                            bar.tick(slot, f);
                        }
                    };
                    let outcome = convert_one(ctx, &book, &kind, mode, &tick);
                    let seconds = started.elapsed().as_secs_f32();

                    let done = match outcome {
                        Ok(converted) => {
                            bar.finish_item(slot, true);
                            if !bar.enabled() {
                                ctx.say(format!(
                                    "[{n}/{total}] {} — {kind} in {seconds:.1}s{}",
                                    book.title,
                                    match converted.reanchored {
                                        0 => String::new(),
                                        moved => format!(", {moved} annotation(s) re-anchored"),
                                    }
                                ));
                            }
                            Done {
                                book_id: book.id,
                                title: book.title,
                                kind,
                                seconds,
                                error: None,
                                reanchored: converted.reanchored,
                                stranded: converted.stranded,
                            }
                        }
                        Err(e) => {
                            let error = format!("{e:#}");
                            bar.finish_item(slot, false);
                            bar.note(&format!("[{n}/{total}] {} FAILED: {error}", book.title));
                            if stop_on_error {
                                stop.store(true, Ordering::Relaxed);
                            }
                            Done {
                                book_id: book.id,
                                title: book.title,
                                kind,
                                seconds,
                                error: Some(error),
                                reanchored: 0,
                                stranded: 0,
                            }
                        }
                    };
                    results.lock().unwrap_or_else(|e| e.into_inner()).push(done);
                }
            });
        }
    });
    bar.finish();

    let mut books = results.into_inner().unwrap_or_else(|e| e.into_inner());
    books.sort_by_key(|d| d.book_id);
    let failed = books.iter().filter(|d| d.error.is_some()).count();
    let report = Report {
        converted: books.len() - failed,
        failed,
        skipped: skipped + (total - books.len()),
        books,
    };
    let (converted, failed, stranded) = (
        report.converted,
        report.failed,
        report.books.iter().map(|d| d.stranded).sum::<usize>(),
    );
    ctx.report(&report, || {
        println!("\nconverted {converted}, failed {failed}");
        if stranded > 0 {
            println!(
                "{stranded} annotation(s) could not be re-found in the rebuilt text and were \
                 left where they were"
            );
        }
    })
}

/// Convert one book: the slow half with no database at all, then the connection
/// for as long as it takes to write the result down.
fn convert_one(
    ctx: &Ctx,
    book: &BookRow,
    kind: &str,
    mode: Mode,
    on_progress: convert::OnProgress<'_>,
) -> Result<convert::Converted> {
    ctx.db
        .with(|conn| db::set_job_status(conn, book.id, "converting", None))?;
    let outcome = convert::run(&ctx.paths, book, kind, mode, on_progress)
        .and_then(|produced| ctx.db.with(|conn| convert::record(conn, book, produced)));
    match &outcome {
        Ok(_) => ctx
            .db
            .with(|conn| db::set_job_status(conn, book.id, "done", None))?,
        Err(e) => ctx
            .db
            .with(|conn| db::set_job_status(conn, book.id, "error", Some(&format!("{e:#}"))))?,
    }
    outcome
}

/// Conversion is CPU-bound, so the default is every core: the OS scheduler
/// handles contention with whatever else is running better than a guessed cap.
fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Per-book conversion state, for watching a sweep or finding what failed.
#[derive(Serialize)]
struct Job {
    book_id: i64,
    title: String,
    status: String,
    kind: Option<String>,
    error: Option<String>,
}

pub fn jobs(ctx: &Ctx, select: &Select) -> Result<()> {
    let conn = ctx.conn();
    let books = select.resolve(&conn)?;
    let jobs: Vec<Job> = books
        .into_iter()
        .map(|b| Job {
            book_id: b.id,
            title: b.title,
            status: b.status,
            kind: b.kind,
            error: b.error,
        })
        .collect();
    ctx.report(&jobs, || {
        for j in &jobs {
            println!(
                "{:>6}  {:<11} {}{}",
                j.book_id,
                j.status,
                j.title,
                match &j.error {
                    Some(e) => format!("\n        {e}"),
                    None => String::new(),
                }
            );
        }
        let errors = jobs.iter().filter(|j| j.status == "error").count();
        let pending = jobs.iter().filter(|j| j.status == "pending").count();
        println!(
            "\n{} books, {errors} in error, {pending} pending",
            jobs.len()
        );
    })
}
