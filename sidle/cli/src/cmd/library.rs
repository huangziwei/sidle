//! The library itself: what is in it, what it says, and what comes out of it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use rusqlite::Connection;
use serde::Serialize;
use sidle_core::library::db::{self, BookRow, BulkMetadataPatch};
use sidle_core::library::import::{self, ImportOutcome};
use sidle_core::library::{cover_fetch, export as exporter, metadata, omnibus, progress};

use crate::ctx::Ctx;
use crate::select::Select;

/// What the library holds, and what needs doing to it.
#[derive(Serialize)]
struct Status {
    root: String,
    books: usize,
    converted: usize,
    pending: usize,
    errored: usize,
    with_catalogue_asin: usize,
    sharing_catalogue_id: usize,
    unmeasured: usize,
    notebooks: i64,
    annotations: i64,
}

pub fn status(ctx: &Ctx) -> Result<()> {
    let conn = ctx.conn();
    let books = db::list_books(&conn).context("read the library")?;
    let status = Status {
        root: ctx.paths.root.to_string_lossy().to_string(),
        books: books.len(),
        converted: books.iter().filter(|b| b.status == "done").count(),
        pending: books
            .iter()
            .filter(|b| b.status == "pending" || b.status == "converting")
            .count(),
        errored: books.iter().filter(|b| b.status == "error").count(),
        with_catalogue_asin: books.iter().filter(|b| b.amazon_asin.is_some()).count(),
        sharing_catalogue_id: books.iter().filter(|b| shares_a_catalogue_id(b)).count(),
        unmeasured: db::books_missing_max_position(&conn)?.len(),
        notebooks: count(&conn, "notebooks")?,
        annotations: count(&conn, "annotations")?,
    };
    ctx.report(&status, || {
        println!("{}", status.root);
        println!(
            "{} books — {} converted, {} pending, {} in error",
            status.books, status.converted, status.pending, status.errored
        );
        println!(
            "{} carry a catalogue ASIN for cover fetching",
            status.with_catalogue_asin
        );
        if status.sharing_catalogue_id > 0 {
            println!(
                "{} still carry one *inside the file*, where a Kindle reads it as the \
                 catalogue item itself — `sidle-cli rekey` fixes that",
                status.sharing_catalogue_id
            );
        }
        if status.unmeasured > 0 {
            println!(
                "{} have no position axis, so reading time on them can't be attributed \
                 — `sidle-cli convert --all --force` measures them",
                status.unmeasured
            );
        }
        println!(
            "{} annotations, {} notebooks",
            status.annotations, status.notebooks
        );
    })
}

fn count(conn: &Connection, table: &str) -> Result<i64> {
    Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)
}

pub fn list(ctx: &Ctx, select: &Select, field: Option<&str>) -> Result<()> {
    let conn = ctx.conn();
    let books = select.resolve(&conn)?;
    if let Some(field) = field {
        for b in &books {
            println!("{}", one_field(b, field)?);
        }
        return Ok(());
    }
    ctx.report(&books, || {
        for b in &books {
            println!(
                "{:>6}  {:<9} {:<7} {}  —  {}",
                b.id,
                b.status,
                formats(b),
                b.title,
                if b.author.is_empty() {
                    "—"
                } else {
                    &b.author
                }
            );
        }
        println!("\n{} books", books.len());
    })
}

/// One value per book, for feeding the next command in a pipeline.
fn one_field(book: &BookRow, field: &str) -> Result<String> {
    let path = |p: &Option<String>| p.clone().unwrap_or_default();
    Ok(match field {
        "id" => book.id.to_string(),
        "sha" => book.sha256.clone(),
        "title" => book.title.clone(),
        "author" => book.author.clone(),
        "asin" => book.asin.clone().unwrap_or_default(),
        "amazon-asin" => book.amazon_asin.clone().unwrap_or_default(),
        "source-format" => book.source_format.clone().unwrap_or_default(),
        "kfx" => path(&book.kfx_path),
        "epub" => path(&book.epub_path),
        "pdf" => path(&book.pdf_path),
        "cover" => path(&book.cover_path),
        "path" => book
            .kfx_path
            .clone()
            .or_else(|| book.epub_path.clone())
            .or_else(|| book.pdf_path.clone())
            .unwrap_or_default(),
        other => anyhow::bail!(
            "unknown field {other:?} \
             (id, sha, title, author, asin, amazon-asin, source-format, kfx, epub, pdf, cover, path)"
        ),
    })
}

/// Which artefacts a book actually has on disk, as a fixed-width badge.
fn formats(book: &BookRow) -> String {
    let mark = |p: &Option<String>, ch: char| match p.as_deref() {
        Some(p) if Path::new(p).exists() => ch,
        _ => '·',
    };
    format!(
        "{}{}{}",
        mark(&book.kfx_path, 'K'),
        mark(&book.epub_path, 'E'),
        mark(&book.pdf_path, 'P')
    )
}

pub fn show(ctx: &Ctx, select: &Select) -> Result<()> {
    let conn = ctx.conn();
    let books = select.resolve_nonempty(&conn)?;
    ctx.report(&books, || {
        for b in &books {
            println!("[{}] {}", b.id, b.title);
            let line = |k: &str, v: &str| {
                if !v.is_empty() {
                    println!("  {k:<14} {v}");
                }
            };
            line("author", &b.author);
            line("language", &b.language);
            line("series", &series(b));
            line("publisher", b.publisher.as_deref().unwrap_or(""));
            line("published", b.published_at.as_deref().unwrap_or(""));
            line("tags", &b.tags.join(", "));
            line("layout", b.writing_mode.as_deref().unwrap_or(""));
            line("direction", b.ppd.as_deref().unwrap_or(""));
            line("asin", b.asin.as_deref().unwrap_or(""));
            line("catalogue asin", b.amazon_asin.as_deref().unwrap_or(""));
            line("sha256", &b.sha256);
            line("kfx sha256", b.kfx_sha256.as_deref().unwrap_or(""));
            line("kfx", b.kfx_path.as_deref().unwrap_or(""));
            line("epub", b.epub_path.as_deref().unwrap_or(""));
            line("pdf", b.pdf_path.as_deref().unwrap_or(""));
            line("cover", b.cover_path.as_deref().unwrap_or(""));
            line("imported", &b.imported_at);
            line("updated", &b.updated_at);
            line("source format", b.source_format.as_deref().unwrap_or(""));
            line("conversion", &format!("{} ({})", b.status, kind(b)));
            if let Some(e) = &b.error {
                line("error", e);
            }
            println!();
        }
    })
}

fn series(book: &BookRow) -> String {
    match (&book.series_name, book.series_index) {
        (Some(name), Some(i)) => format!("{name} #{i}"),
        (Some(name), None) => name.clone(),
        _ => String::new(),
    }
}

fn kind(book: &BookRow) -> &str {
    book.kind.as_deref().unwrap_or("no conversion")
}

/// True when the file's own key is a catalogue ASIN, which means the device
/// cannot tell this copy from the store's.
fn shares_a_catalogue_id(book: &BookRow) -> bool {
    book.kfx_path.is_some()
        && book
            .asin
            .as_deref()
            .is_some_and(cover_fetch::looks_like_real_amazon_asin)
}

#[derive(Args)]
pub struct ImportArgs {
    /// Files to add: `.epub`, `.kfx`, `.kfx-zip`, `.azw3`, `.mobi`, `.pobi`,
    /// `.pdf`, or
    /// an Aozora `.zip`. A directory is walked one level deep.
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Land the files but leave the companion format to a later `convert`.
    #[arg(long)]
    no_convert: bool,
    /// How many books to convert at once once they have landed.
    #[arg(long, short = 'j', value_name = "N")]
    jobs: Option<usize>,
}

#[derive(Serialize)]
struct Imported {
    path: String,
    outcome: &'static str,
    book_id: Option<i64>,
    title: Option<String>,
    error: Option<String>,
}

pub fn import(ctx: &Ctx, args: ImportArgs) -> Result<()> {
    let files = expand(&args.paths)?;
    let mut results = Vec::with_capacity(files.len());
    let mut queued: Vec<i64> = Vec::new();

    for (i, path) in files.iter().enumerate() {
        ctx.say(format!("[{}/{}] {}", i + 1, files.len(), path.display()));
        let outcome = stage_and_record(ctx, path);
        results.push(match outcome {
            Ok(ImportOutcome::Imported {
                book,
                needs_enqueue,
            }) => {
                if needs_enqueue {
                    queued.push(book.id);
                }
                Imported {
                    path: path.to_string_lossy().to_string(),
                    outcome: "imported",
                    book_id: Some(book.id),
                    title: Some(book.title),
                    error: None,
                }
            }
            Ok(ImportOutcome::Duplicate(book)) => Imported {
                path: path.to_string_lossy().to_string(),
                outcome: "duplicate",
                book_id: Some(book.id),
                title: Some(book.title),
                error: None,
            },
            Err(e) => Imported {
                path: path.to_string_lossy().to_string(),
                outcome: "failed",
                book_id: None,
                title: None,
                error: Some(format!("{e:#}")),
            },
        });
    }

    let imported = results.iter().filter(|r| r.outcome == "imported").count();
    let duplicate = results.iter().filter(|r| r.outcome == "duplicate").count();
    let failed = results.iter().filter(|r| r.outcome == "failed").count();
    ctx.report(&results, || {
        println!("\nimported {imported}, already present {duplicate}, failed {failed}");
    })?;

    if !queued.is_empty() && !args.no_convert {
        ctx.say(format!("\nconverting {} new book(s)", queued.len()));
        let select = Select {
            ids: queued,
            ..Default::default()
        };
        crate::cmd::convert::run(
            ctx,
            crate::cmd::convert::ConvertArgs::sweep(select, false, args.jobs),
        )?;
    }
    Ok(())
}

/// Import one file: the slow stage (parse, transcode) with no database, then the
/// row.
fn stage_and_record(ctx: &Ctx, path: &Path) -> Result<ImportOutcome> {
    let kind = import::detect_kind(path)?;
    let identity = import::identify_file(path)?;
    if let Some(existing) = db::find_by_sha(&ctx.conn(), &identity.0)? {
        return Ok(ImportOutcome::Duplicate(existing));
    }
    let pipeline = progress::import_pipeline(kind);
    let throttle = progress::Throttle::new();
    let on_progress = |phase: &str, cur: usize, total: usize, label: &str| {
        let Some(pipeline) = pipeline else { return };
        let fraction = progress::fraction(pipeline, phase, cur, total);
        if throttle.worth_emitting(fraction) && !label.is_empty() {
            ctx.say(format!("      {:>3}%  {label}", (fraction * 100.0) as u32));
        }
    };
    let staged = import::stage_file(&ctx.paths, path, identity, &on_progress)?;
    import::record(&ctx.conn(), staged)
}

/// The files an argument list names: each path itself, plus the ebooks one level
/// inside any directory given.
fn expand(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(p)
                .with_context(|| format!("read {}", p.display()))?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && import::detect_kind(p).is_ok())
                .collect();
            found.sort();
            out.extend(found);
        } else if p.is_file() {
            out.push(p.clone());
        } else {
            anyhow::bail!("no such file: {}", p.display());
        }
    }
    if out.is_empty() {
        anyhow::bail!("nothing to import");
    }
    Ok(out)
}

#[derive(Args)]
pub struct SetArgs {
    #[command(flatten)]
    select: Select,

    /// Title. Refused for a multi-book selection — a title is per book.
    #[arg(long)]
    title: Option<String>,
    /// Author(s). Split on `&` or `、`; Western names are flipped to natural
    /// order and re-joined.
    #[arg(long)]
    author: Option<String>,
    /// Language code, harmonized: `en-US` → `en`, `zh-TW` → `zh-Hant`.
    #[arg(long)]
    language: Option<String>,
    /// Page direction: `rtl` or `ltr`.
    #[arg(long)]
    ppd: Option<String>,
    /// Reading layout: `horizontal-lr`, `horizontal-rl`, `vertical-rl`,
    /// `vertical-lr`. Sets the page direction with it.
    #[arg(long)]
    writing_mode: Option<String>,
    #[arg(long)]
    publisher: Option<String>,
    /// Publication date, as the source states it.
    #[arg(long)]
    published: Option<String>,
    /// The series this book belongs to.
    #[arg(long = "series-name")]
    series_name: Option<String>,
    #[arg(long)]
    series_index: Option<f64>,
    /// Add a tag. Repeatable.
    #[arg(long = "add-tag")]
    add_tags: Vec<String>,
    /// Remove a tag. Repeatable.
    #[arg(long = "remove-tag")]
    remove_tags: Vec<String>,
    /// Re-derive the KFX afterwards, so the edit reaches the file the Kindle
    /// reads and not just the library row.
    #[arg(long)]
    reconvert: bool,
    /// Print the selection and change nothing.
    #[arg(long)]
    dry_run: bool,
}

pub fn set(ctx: &Ctx, args: SetArgs) -> Result<()> {
    let books = {
        let conn = ctx.conn();
        args.select.resolve_nonempty(&conn)?
    };
    if args.title.is_some() && books.len() > 1 {
        anyhow::bail!(
            "--title names one book, but {} are selected; narrow the selection",
            books.len()
        );
    }
    if args.dry_run {
        return ctx.report(&books, || {
            println!("{} book(s) would be edited:", books.len());
            for b in &books {
                println!("  [{}] {}", b.id, b.title);
            }
        });
    }

    let updated = {
        let conn = ctx.conn();
        // A single book with a title edit is a full-replacement patch (the only
        // shape that can rewrite one); everything else is the sparse bulk patch,
        // which leaves untouched fields alone.
        if let Some(title) = args.title.clone() {
            let mut patch = metadata::patch_from(&books[0]);
            patch.title = title;
            apply_scalars(&mut patch, &args);
            for t in &args.add_tags {
                patch.tags.push(t.clone());
            }
            patch
                .tags
                .retain(|t| !args.remove_tags.iter().any(|r| r.eq_ignore_ascii_case(t)));
            vec![metadata::apply(&conn, &ctx.paths, books[0].id, patch)?]
        } else {
            let patch = BulkMetadataPatch {
                author: args.author.clone(),
                language: args.language.clone(),
                ppd: args.ppd.clone(),
                writing_mode: args.writing_mode.clone(),
                publisher: args.publisher.clone(),
                published_at: args.published.clone(),
                series_name: args.series_name.clone(),
                series_index: args.series_index,
                add_tags: args.add_tags.clone(),
                remove_tags: args.remove_tags.clone(),
            };
            let ids: Vec<i64> = books.iter().map(|b| b.id).collect();
            let rows = metadata::apply_bulk(&conn, &ids, patch)?;
            // A bulk patch writes the row; each file keeps the old
            // `[Author] Title (Year)` name until they are renamed to match.
            for id in &ids {
                sidle_core::library::rename::rename_book_files(&conn, &ctx.paths, *id)?;
            }
            rows
        }
    };

    ctx.report(&updated, || {
        println!("edited {} book(s)", updated.len());
    })?;

    if args.reconvert {
        let select = Select {
            ids: updated.iter().map(|b| b.id).collect(),
            ..Default::default()
        };
        crate::cmd::convert::run(
            ctx,
            crate::cmd::convert::ConvertArgs::sweep(select, true, None),
        )?;
    }
    Ok(())
}

fn apply_scalars(patch: &mut db::MetadataPatch, args: &SetArgs) {
    if let Some(v) = &args.author {
        patch.author = v.clone();
    }
    if let Some(v) = &args.language {
        patch.language = v.clone();
    }
    if args.ppd.is_some() {
        patch.ppd = args.ppd.clone();
    }
    if args.writing_mode.is_some() {
        patch.writing_mode = args.writing_mode.clone();
    }
    if args.publisher.is_some() {
        patch.publisher = args.publisher.clone();
    }
    if args.published.is_some() {
        patch.published_at = args.published.clone();
    }
    if args.series_name.is_some() {
        patch.series_name = args.series_name.clone();
    }
    if args.series_index.is_some() {
        patch.series_index = args.series_index;
    }
}

pub fn asin(ctx: &Ctx, select: &Select, asin: &str) -> Result<()> {
    let conn = ctx.conn();
    let books = select.resolve_nonempty(&conn)?;
    if books.len() > 1 {
        anyhow::bail!(
            "an ASIN names one book, but {} are selected — it is the per-book key \
             covers are fetched with",
            books.len()
        );
    }
    let updated = metadata::set_amazon_asin(&conn, books[0].id, Some(asin))?;
    ctx.report(&updated, || {
        println!(
            "{} → {}",
            updated.title,
            updated.amazon_asin.as_deref().unwrap_or("")
        );
    })
}

/// The formats an import can read, as `books.source_format` records them.
const SOURCE_FORMATS: &[&str] = &["azw3", "mobi", "epub", "kfx", "kfx-zip", "pdf", "aozora"];

/// Record what the selected books arrived as. Provenance, not metadata: a
/// `.azw3` import stores the EPUB it exported, and `conversion_jobs.kind`
/// names the direction a reconvert takes from there.
pub fn source_format(ctx: &Ctx, select: &Select, format: &str) -> Result<()> {
    if !SOURCE_FORMATS.contains(&format) {
        anyhow::bail!(
            "unknown source format {format:?} ({})",
            SOURCE_FORMATS.join(", ")
        );
    }
    let conn = ctx.conn();
    let books = select.resolve_nonempty(&conn)?;
    for b in &books {
        db::set_source_format(&conn, b.id, Some(format))?;
    }
    ctx.report(&books, || {
        for b in &books {
            println!("{} → {format}", b.title);
        }
        println!("\n{} book(s)", books.len());
    })
}

/// Re-key every KFX whose baked identity is a catalogue ASIN. `kfx_sha256`
/// stays put — it is the `<sha8>` in the on-device filename each `.sdr` binds
/// to — and the rewrite moves the mtime the picker's update pass watches.
pub fn rekey(ctx: &Ctx, apply: bool) -> Result<()> {
    let conn = ctx.conn();
    let books = db::list_books(&conn).context("read the library")?;
    let affected: Vec<&BookRow> = books.iter().filter(|b| shares_a_catalogue_id(b)).collect();

    if affected.is_empty() {
        ctx.say("nothing to re-key: no file carries a catalogue ASIN as its own key");
        return Ok(());
    }
    if !apply {
        return ctx.report(&affected, || {
            println!("{} books would be re-keyed:", affected.len());
            for b in affected.iter().take(20) {
                println!("  {} — {}", b.asin.as_deref().unwrap_or("?"), b.title);
            }
            if affected.len() > 20 {
                println!("  … and {} more", affected.len() - 20);
            }
            println!("\nRe-run with --apply.");
        });
    }

    #[derive(Serialize)]
    struct Rekeyed {
        book_id: i64,
        title: String,
        from: String,
        to: Option<String>,
        error: Option<String>,
    }
    let mut done = Vec::new();
    for (i, book) in affected.iter().enumerate() {
        let from = book.asin.clone().unwrap_or_default();
        match rekey_one(&conn, book) {
            Ok(to) => {
                ctx.say(format!(
                    "[{}/{}] {} -> {to}",
                    i + 1,
                    affected.len(),
                    book.title
                ));
                done.push(Rekeyed {
                    book_id: book.id,
                    title: book.title.clone(),
                    from,
                    to: Some(to),
                    error: None,
                });
            }
            Err(e) => {
                eprintln!("failed {}: {e:#}", book.title);
                done.push(Rekeyed {
                    book_id: book.id,
                    title: book.title.clone(),
                    from,
                    to: None,
                    error: Some(format!("{e:#}")),
                });
            }
        }
    }
    let failed = done.iter().filter(|d| d.error.is_some()).count();
    let ok = done.len() - failed;
    ctx.report(&done, || {
        println!("\nre-keyed {ok}, failed {failed}");
        if ok > 0 {
            println!(
                "Books already on a Kindle keep their filename and pick the change up on \
                 the next Update in the picker."
            );
        }
    })
}

/// Re-key one book, returning the new key.
fn rekey_one(conn: &Connection, book: &BookRow) -> Result<String> {
    use bokai::formats::kfx::metadata::{generate_content_id, resolve_export_asin};
    use bokai::formats::kfx::metadata_edit::{self, MetadataPatch};

    let path = PathBuf::from(
        book.kfx_path
            .as_deref()
            .expect("filtered to books with a KFX"),
    );
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let kfx = bokai::Book::from_bytes(&bytes, bokai::Format::Kfx)
        .with_context(|| format!("parse {}", path.display()))?;

    // A KFX Amazon produced names no publication identifier for the export
    // rule to derive from. `sha256` stands in: per-book, the name of the
    // directory the file lives in, and fixed for the life of the bytes.
    let new_key =
        resolve_export_asin(kfx.metadata()).unwrap_or_else(|| generate_content_id(&book.sha256));
    let old_key = book.asin.as_deref().unwrap_or_default().to_string();
    if new_key == old_key {
        return Ok(new_key);
    }

    // Both fields, to one value: they are written equal at export, and a device
    // that keys on either sees one identity.
    let patched = metadata_edit::edit_metadata(
        &bytes,
        &MetadataPatch {
            asin: Some(new_key.clone()),
            content_id: Some(new_key.clone()),
            ..Default::default()
        },
    )
    .with_context(|| format!("re-key {}", path.display()))?;
    import::write_bytes_atomic(&path, &patched)
        .with_context(|| format!("write {}", path.display()))?;

    // `old_key` is this book's only colour-cover key. A curated
    // `amazon_asin` is left as it stands.
    if book.amazon_asin.is_none() && cover_fetch::looks_like_real_amazon_asin(&old_key) {
        db::set_amazon_asin(conn, book.id, Some(&old_key))?;
    }
    db::set_asin(conn, book.id, &new_key)?;
    db::relink_ink(conn, &old_key, &new_key)?;
    Ok(new_key)
}

#[derive(Args)]
pub struct CoverArgs {
    #[command(flatten)]
    select: Select,
    /// Pull the colour cover from the catalogue, by the book's ASIN.
    #[arg(long, conflicts_with = "set")]
    refetch: bool,
    /// Use this image file (JPG, PNG or WebP). One book at a time.
    #[arg(long, value_name = "FILE")]
    set: Option<PathBuf>,
}

pub fn cover(ctx: &Ctx, args: CoverArgs) -> Result<()> {
    use sidle_core::library::cover;

    let conn = ctx.conn();
    let books = args.select.resolve_nonempty(&conn)?;

    #[derive(Serialize)]
    struct Applied {
        book_id: i64,
        title: String,
        outcome: String,
        detail: Option<String>,
    }
    let mut done = Vec::new();

    if let Some(src) = &args.set {
        if books.len() > 1 {
            anyhow::bail!("--set takes one book, but {} are selected", books.len());
        }
        let outcome = cover::set_from_file(&conn, &ctx.paths, &books[0], src);
        done.push(record_cover(&books[0], outcome));
    } else if args.refetch {
        for (i, book) in books.iter().enumerate() {
            ctx.say(format!("[{}/{}] {}", i + 1, books.len(), book.title));
            done.push(record_cover(book, cover::refetch(&conn, &ctx.paths, book)));
        }
    } else {
        anyhow::bail!("say what to do: --refetch, or --set <FILE>");
    }

    fn record_cover(book: &BookRow, outcome: cover::Outcome) -> Applied {
        let (o, detail) = match outcome {
            cover::Outcome::Updated { cover_path } => ("updated", Some(cover_path)),
            cover::Outcome::NoAsin => ("no_asin", None),
            cover::Outcome::Failed { error } => ("failed", Some(error)),
        };
        Applied {
            book_id: book.id,
            title: book.title.clone(),
            outcome: o.to_string(),
            detail,
        }
    }

    let updated = done.iter().filter(|d| d.outcome == "updated").count();
    let no_asin = done.iter().filter(|d| d.outcome == "no_asin").count();
    let failed = done.iter().filter(|d| d.outcome == "failed").count();
    ctx.report(&done, || {
        println!("\nupdated {updated}, no catalogue ASIN {no_asin}, failed {failed}");
    })
}

#[derive(Args)]
pub struct ExportArgs {
    #[command(flatten)]
    select: Select,
    /// `epub`, `kfx`, `pdf` (the stored file) or `txt` (generated on demand).
    #[arg(long, value_name = "FORMAT")]
    format: String,
    /// Folder to write into.
    #[arg(long, value_name = "DIR")]
    dest: PathBuf,
}

pub fn export(ctx: &Ctx, args: ExportArgs) -> Result<()> {
    let format = exporter::Format::parse(&args.format)?;
    let conn = ctx.conn();
    let ids: Vec<i64> = args
        .select
        .resolve_nonempty(&conn)?
        .iter()
        .map(|b| b.id)
        .collect();
    let summary = exporter::export_books(&conn, &ids, format, &args.dest)?;
    ctx.report(&summary, || {
        println!(
            "wrote {} file(s) to {}, skipped {}",
            summary.exported, summary.dest, summary.skipped
        );
        for e in &summary.errors {
            println!("  {e}");
        }
    })
}

pub fn remove(ctx: &Ctx, select: &Select, apply: bool) -> Result<()> {
    let conn = ctx.conn();
    let books = select.resolve_nonempty(&conn)?;
    if !apply {
        return ctx.report(&books, || {
            println!("{} book(s) would be removed, files and all:", books.len());
            for b in &books {
                println!("  [{}] {}", b.id, b.title);
            }
            println!("\nRe-run with --apply.");
        });
    }

    #[derive(Serialize)]
    struct Removed {
        book_id: i64,
        title: String,
        error: Option<String>,
    }
    let mut done = Vec::new();
    for book in &books {
        // Files first: a failure there leaves the row in place, so the book is
        // visible, not an orphan folder nothing lists.
        let error = ctx
            .paths
            .remove_sha(&book.sha256)
            .err()
            .map(|e| format!("could not remove files: {e}"));
        if error.is_none() {
            db::remove_book(&conn, book.id)?;
        }
        done.push(Removed {
            book_id: book.id,
            title: book.title.clone(),
            error,
        });
    }
    db::vacuum(&conn)?;
    let failed = done.iter().filter(|d| d.error.is_some()).count();
    let ok = done.len() - failed;
    ctx.report(&done, || println!("removed {ok}, failed {failed}"))
}

pub fn compact(ctx: &Ctx) -> Result<()> {
    let before = std::fs::metadata(ctx.paths.db()).map(|m| m.len()).ok();
    db::vacuum(&ctx.conn())?;
    let after = std::fs::metadata(ctx.paths.db()).map(|m| m.len()).ok();
    ctx.report(&(before, after), || match (before, after) {
        (Some(b), Some(a)) => println!("library.db {} KB → {} KB", b / 1024, a / 1024),
        _ => println!("compacted"),
    })
}

#[derive(Args)]
pub struct SplitArgs {
    #[command(flatten)]
    select: Select,
    /// Write the volumes into the library. Without it, the proposed cut is
    /// printed and nothing is created.
    #[arg(long)]
    apply: bool,
}

/// Split an omnibus into its volumes — the same proposal the desktop shows,
/// applied without the dialog.
pub fn split(ctx: &Ctx, args: SplitArgs) -> Result<()> {
    let books = {
        let conn = ctx.conn();
        args.select.resolve_nonempty(&conn)?
    };
    if books.len() > 1 {
        anyhow::bail!("split takes one omnibus, but {} are selected", books.len());
    }
    let book = &books[0];
    let epub = book
        .epub_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("{} has no EPUB to split", book.title))?;
    let bytes = std::fs::read(epub).with_context(|| format!("read {epub}"))?;
    let plan = omnibus::propose(&bytes, book)?;

    if !args.apply {
        return ctx.report(&plan, || {
            println!(
                "{} would become {} volumes:",
                book.title,
                plan.volumes.len()
            );
            for (i, cut) in plan.volumes.iter().enumerate() {
                println!("  {}. {}", i + 1, cut.title);
            }
            println!("\nRe-run with --apply.");
        });
    }

    let volumes = omnibus::carve_volumes(book, &plan)?;
    // The connection is taken for the writes and released before the sweep at
    // the end: its workers borrow the same one.
    let outcomes = {
        let conn = ctx.conn();
        omnibus::add_volumes(&conn, &ctx.paths, book, &plan, volumes, |n, title| {
            ctx.say(format!("  [{}/{}] {title}", n + 1, plan.volumes.len()));
        })?
    };
    // Each fresh volume lands as an EPUB with a pending job; its KFX is this
    // sweep's to produce, the same shape the desktop's queue takes. A
    // volume whose place in the series is taken is left alone.
    let ids: Vec<i64> = outcomes
        .iter()
        .filter(|o| o.needs_enqueue)
        .filter_map(|o| o.book_id)
        .collect();
    ctx.report(&outcomes, || {
        let duplicates = outcomes.iter().filter(|o| o.duplicate).count();
        println!(
            "created {} volume(s){}",
            outcomes
                .iter()
                .filter(|o| !o.duplicate && o.error.is_none())
                .count(),
            match duplicates {
                0 => String::new(),
                n => format!("; {n} already sat in the series and were left alone"),
            }
        );
        for o in outcomes.iter().filter(|o| o.error.is_some()) {
            println!("  {}: {}", o.title, o.error.as_deref().unwrap_or(""));
        }
    })?;
    if !ids.is_empty() {
        crate::cmd::convert::run(
            ctx,
            crate::cmd::convert::ConvertArgs::sweep(
                Select {
                    ids,
                    ..Default::default()
                },
                false,
                None,
            ),
        )?;
    }
    Ok(())
}
