//! Handwritten notebooks: what has been backed up, and getting them out again.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use sidle_core::library::db;
use sidle_core::library::notebook;

use crate::ctx::Ctx;

#[derive(Subcommand)]
pub enum NotebookCmd {
    /// Every notebook in the library.
    List,
    /// Import a `.notebooks/` folder (or one notebook directory).
    Import { folder: PathBuf },
    /// Write notebooks out as PDFs.
    Export {
        /// Notebook row id. Repeatable; all of them when none is given.
        #[arg(long = "id", value_name = "N")]
        ids: Vec<i64>,
        #[arg(long, value_name = "DIR")]
        dest: PathBuf,
    },
    /// Rename a notebook.
    Rename { id: i64, title: String },
    /// Remove a notebook from the library. The device keeps its copy; a later
    /// sync will not resurrect it.
    Remove {
        #[arg(long = "id", value_name = "N", required = true)]
        ids: Vec<i64>,
        #[arg(long)]
        apply: bool,
    },
}

pub fn run(ctx: &Ctx, cmd: NotebookCmd) -> Result<()> {
    match cmd {
        NotebookCmd::List => list(ctx),
        NotebookCmd::Import { folder } => import(ctx, &folder),
        NotebookCmd::Export { ids, dest } => export(ctx, &ids, &dest),
        NotebookCmd::Rename { id, title } => rename(ctx, id, &title),
        NotebookCmd::Remove { ids, apply } => remove(ctx, &ids, apply),
    }
}

fn list(ctx: &Ctx) -> Result<()> {
    let conn = ctx.conn();
    let rows = db::list_notebooks(&conn)?;
    ctx.report(&rows, || {
        for n in &rows {
            println!(
                "{:>6}  {:>4} pages  {:<20}  {}",
                n.id,
                n.page_count,
                n.updated_at.as_deref().unwrap_or("—"),
                n.title
            );
        }
        println!("\n{} notebook(s)", rows.len());
    })
}

fn import(ctx: &Ctx, folder: &std::path::Path) -> Result<()> {
    if !folder.is_dir() {
        anyhow::bail!("{} is not a folder", folder.display());
    }
    let summary = notebook::import_folder(&ctx.conn(), &ctx.paths, folder);
    ctx.report(&summary, || {
        println!(
            "imported {}, unchanged {}, failed {}",
            summary.imported,
            summary.unchanged,
            summary.failed.len()
        );
        for f in &summary.failed {
            println!("  {f}");
        }
    })
}

fn export(ctx: &Ctx, ids: &[i64], dest: &std::path::Path) -> Result<()> {
    if !dest.is_dir() {
        anyhow::bail!("{} is not a folder", dest.display());
    }
    let conn = ctx.conn();
    let rows: Vec<_> = db::list_notebooks(&conn)?
        .into_iter()
        .filter(|n| ids.is_empty() || ids.contains(&n.id))
        .collect();
    if rows.is_empty() {
        anyhow::bail!("no notebook matches");
    }
    let mut written = Vec::new();
    for n in &rows {
        match notebook::export_notebook_pdf(&ctx.paths, &n.uuid, n.page_count as usize) {
            Ok(pdf) => {
                let name = sidle_core::library::paths::sanitize_segment(&n.title);
                let target = sidle_core::library::paths::dedup_path(dest.join(format!(
                    "{}.pdf",
                    if name.is_empty() { "Notebook" } else { &name }
                )));
                std::fs::write(&target, pdf)?;
                written.push(target.to_string_lossy().to_string());
            }
            Err(e) => eprintln!("failed {}: {e:#}", n.title),
        }
    }
    ctx.report(&written, || {
        println!("wrote {} PDF(s) to {}", written.len(), dest.display())
    })
}

fn rename(ctx: &Ctx, id: i64, title: &str) -> Result<()> {
    let title = title.trim();
    if title.is_empty() {
        anyhow::bail!("a notebook needs a name");
    }
    let conn = ctx.conn();
    db::rename_notebook(&conn, id, title)?;
    let row =
        db::get_notebook(&conn, id)?.ok_or_else(|| anyhow::anyhow!("no notebook with id {id}"))?;
    ctx.report(&row, || println!("[{}] {}", row.id, row.title))
}

fn remove(ctx: &Ctx, ids: &[i64], apply: bool) -> Result<()> {
    let conn = ctx.conn();
    let rows: Vec<_> = db::list_notebooks(&conn)?
        .into_iter()
        .filter(|n| ids.contains(&n.id))
        .collect();
    if rows.is_empty() {
        anyhow::bail!("no notebook matches");
    }
    if !apply {
        return ctx.report(&rows, || {
            println!("{} notebook(s) would be removed:", rows.len());
            for n in &rows {
                println!("  [{}] {}", n.id, n.title);
            }
            println!("\nRe-run with --apply.");
        });
    }
    for n in &rows {
        db::remove_notebook(&conn, n.id)?;
        ctx.paths.remove_notebook(&n.uuid)?;
    }
    ctx.report(&rows.len(), || {
        println!("removed {} notebook(s)", rows.len())
    })
}
