//! Converting many books at once.
//!
//! The desktop's queue is a dispatcher over a tokio `JoinSet`, sized to the
//! machine, feeding a window that wants progress bars. Here there is no window,
//! so the sweep is what a sweep should be: N worker threads pulling from one
//! list, each running the same [`convert`] pipeline the desktop runs, printing a
//! line per finished book.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use clap::Args;
use serde::Serialize;
use sidle_core::library::convert::{self, Mode};
use sidle_core::library::db::{self, Access, BookRow};

use crate::ctx::Ctx;
use crate::select::Select;

#[derive(Args)]
pub struct ConvertArgs {
    #[command(flatten)]
    select: Select,

    /// Convert books that already have their output, too — the sweep to run
    /// after bokai changes.
    ///
    /// Source→target only: the import-time cover enrichment is skipped, so the
    /// source KFX (and the `kfx_sha256` the device names its file after) is left
    /// alone and a book already on a Kindle keeps its highlights.
    #[arg(long)]
    force: bool,

    /// How many books to convert at once. Defaults to the machine's cores.
    #[arg(long, short = 'j', value_name = "N")]
    jobs: Option<usize>,

    /// List what would be converted and stop.
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
    // The workers take the connection themselves, a book at a time. A caller
    // still holding it would leave them waiting on a lock nothing will release,
    // so say so now instead of hanging.
    if ctx.db.try_lock().is_err() {
        anyhow::bail!("the library connection is still held by the caller of this sweep");
    }
    let books = {
        let conn = ctx.conn();
        args.select.resolve(&conn)?
    };

    // A book with no job kind was never enqueued for anything — a row that
    // predates its own conversion, or one whose source vanished. There is
    // nothing to dispatch on, so it is reported as skipped rather than failed.
    let (mut work, no_kind): (Vec<BookRow>, Vec<BookRow>) =
        books.into_iter().partition(|b| b.kind.is_some());
    // Without `--force` this is "finish what's outstanding": books already
    // converted are left alone.
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
    let queue = Mutex::new(work);
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Done>> = Mutex::new(Vec::new());
    let stop = std::sync::atomic::AtomicBool::new(false);

    std::thread::scope(|scope| {
        for _ in 0..jobs.min(total) {
            scope.spawn(|| {
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let Some(book) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop() else {
                        return;
                    };
                    let n = next.fetch_add(1, Ordering::Relaxed) + 1;
                    let kind = book.kind.clone().unwrap_or_default();
                    let started = std::time::Instant::now();
                    let outcome = convert_one(ctx, &book, &kind, mode);
                    let seconds = started.elapsed().as_secs_f32();

                    let done = match outcome {
                        Ok(converted) => {
                            ctx.say(format!(
                                "[{n}/{total}] {} — {kind} in {seconds:.1}s{}",
                                book.title,
                                match converted.reanchored {
                                    0 => String::new(),
                                    moved => format!(", {moved} annotation(s) re-anchored"),
                                }
                            ));
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
                            eprintln!("[{n}/{total}] {} FAILED: {error}", book.title);
                            if args.stop_on_error {
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
fn convert_one(ctx: &Ctx, book: &BookRow, kind: &str, mode: Mode) -> Result<convert::Converted> {
    ctx.db
        .with(|conn| db::set_job_status(conn, book.id, "converting", None))?;
    let outcome = convert::run(&ctx.paths, book, kind, mode, &|_, _, _, _| {})
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
