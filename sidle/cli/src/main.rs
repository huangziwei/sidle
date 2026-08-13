//! `sidle-cli` — library maintenance across the whole library at once.
//!
//! The desktop app edits one book at a time, which is the right shape for
//! curation and the wrong one for a sweep over two thousand rows. This binary
//! opens the same library — same `config.json`, same `library.db`, same files —
//! and does the sweeps.
//!
//! It is not a second implementation of anything: every operation here is
//! `sidle-core` plus `bokai`, the same calls the desktop makes.
//!
//! ```text
//! sidle-cli status            what the library looks like, and what needs doing
//! sidle-cli rekey [--apply]   give every file its own identity (dry by default)
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result};
use bokai::formats::kfx::metadata::{looks_like_real_amazon_asin, resolve_export_asin};
use bokai::formats::kfx::metadata_edit::{self, MetadataPatch};
use rusqlite::Connection;
use sidle_core::library::db::{self, BookRow};
use sidle_core::library::import::write_bytes_atomic;
use sidle_core::library::paths::LibraryPaths;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sidle-cli: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from)
    };
    let root = value("--root");

    match args.first().map(String::as_str) {
        Some("status") => status(&open_library(root)?.1),
        Some("rekey") => rekey(root, flag("--apply")),
        Some("--help") | Some("-h") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => {
            anyhow::bail!("unknown command {other:?}\n\n{USAGE}");
        }
    }
}

const USAGE: &str = "\
sidle-cli — library maintenance

  status          Count the library and report what needs doing.
  rekey [--apply] Give every KFX an identity of its own, so a copy of a
                  store-bought book stops sharing the catalogue item's ASIN.
                  Prints the plan and changes nothing without --apply.

  --root <dir>    Work on the library under <dir> instead of the configured
                  one. A copy of a library is a library, so this is how a
                  sweep gets tried before it is run for real.";

/// Open the library the desktop app would open, or the one under `root`.
fn open_library(root: Option<PathBuf>) -> Result<(LibraryPaths, Connection)> {
    let paths = match root {
        Some(root) => LibraryPaths { root },
        None => LibraryPaths::resolve().context("resolve the library root")?,
    };
    let db_path = paths.db();
    if !db_path.is_file() {
        anyhow::bail!("no library at {}", db_path.display());
    }
    let conn = db::open(&db_path).with_context(|| format!("open {}", db_path.display()))?;
    Ok((paths, conn))
}

fn status(conn: &Connection) -> Result<()> {
    let books = db::list_books(conn).context("read the library")?;
    let with_kfx = books.iter().filter(|b| b.kfx_path.is_some()).count();
    let sharing = books.iter().filter(|b| shares_a_catalogue_id(b)).count();
    let coverable = books.iter().filter(|b| b.amazon_asin.is_some()).count();

    println!("{} books, {with_kfx} converted", books.len());
    println!("{coverable} carry a catalogue ASIN for cover fetching");
    if sharing > 0 {
        println!(
            "{sharing} still carry one *inside the file*, where a Kindle reads it \
             as the catalogue item itself — `sidle-cli rekey` fixes that"
        );
    }
    Ok(())
}

/// True when the file's own key is a catalogue ASIN, which means the device
/// cannot tell this copy from the store's.
fn shares_a_catalogue_id(book: &BookRow) -> bool {
    book.kfx_path.is_some()
        && book
            .asin
            .as_deref()
            .is_some_and(looks_like_real_amazon_asin)
}

/// Re-key every KFX whose baked identity is a catalogue ASIN.
///
/// `kfx_sha256` is deliberately left alone: it is the book's frozen identity,
/// the `<sha8>` in its on-device filename, and the Kindle binds each `.sdr` to
/// that exact name. Rewriting the file moves its mtime instead, which is the
/// revision the picker's update pass watches — so a re-keyed book arrives on
/// the next Update under the name it already had, keeping the reader's
/// highlights and position.
fn rekey(root: Option<PathBuf>, apply: bool) -> Result<()> {
    let (_paths, conn) = open_library(root)?;
    let books = db::list_books(&conn).context("read the library")?;
    let affected: Vec<&BookRow> = books.iter().filter(|b| shares_a_catalogue_id(b)).collect();

    if affected.is_empty() {
        println!("nothing to re-key: no file carries a catalogue ASIN as its own key");
        return Ok(());
    }
    if !apply {
        println!(
            "{} books would be re-keyed. Re-run with --apply.\n",
            affected.len()
        );
        for b in affected.iter().take(10) {
            println!("  {} — {}", b.asin.as_deref().unwrap_or("?"), b.title);
        }
        if affected.len() > 10 {
            println!("  … and {} more", affected.len() - 10);
        }
        return Ok(());
    }

    let (mut done, mut failed, mut skipped) = (0usize, 0usize, 0usize);
    for book in &affected {
        match rekey_one(&conn, book) {
            Ok(Some(new_key)) => {
                done += 1;
                println!("[{done}/{}] {} -> {new_key}", affected.len(), book.title);
            }
            Ok(None) => {
                skipped += 1;
                eprintln!(
                    "skipped {}: the KFX names no identifier to derive from",
                    book.title
                );
            }
            Err(e) => {
                failed += 1;
                eprintln!("failed {}: {e:#}", book.title);
            }
        }
    }

    println!("\nre-keyed {done}, skipped {skipped}, failed {failed}");
    if done > 0 {
        println!(
            "Books already on a Kindle keep their filename and pick the change up \
             on the next Update in the picker."
        );
    }
    Ok(())
}

/// Re-key one book, returning the new key — or `None` when the KFX carries no
/// identifier to derive one from.
fn rekey_one(conn: &Connection, book: &BookRow) -> Result<Option<String>> {
    let path = PathBuf::from(
        book.kfx_path
            .as_deref()
            .expect("filtered to books with a KFX"),
    );
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let kfx = bokai::Book::from_bytes(&bytes, bokai::Format::Kfx)
        .with_context(|| format!("parse {}", path.display()))?;

    let Some(new_key) = resolve_export_asin(kfx.metadata()) else {
        return Ok(None);
    };
    let old_key = book.asin.as_deref().unwrap_or_default().to_string();
    if new_key == old_key {
        return Ok(Some(new_key));
    }

    // Both fields, to one value: they are written equal at export, and a device
    // that keys on either then sees one identity rather than two.
    let patched = metadata_edit::edit_metadata(
        &bytes,
        &MetadataPatch {
            asin: Some(new_key.clone()),
            content_id: Some(new_key.clone()),
            ..Default::default()
        },
    )
    .with_context(|| format!("re-key {}", path.display()))?;
    write_bytes_atomic(&path, &patched).with_context(|| format!("write {}", path.display()))?;

    // The catalogue value the file used to carry is worth keeping — it is the
    // only colour-cover key this book has. Never overwrite one already there:
    // that one was curated.
    if book.amazon_asin.is_none() && looks_like_real_amazon_asin(&old_key) {
        db::set_amazon_asin(conn, book.id, Some(&old_key))?;
    }
    db::set_asin(conn, book.id, &new_key)?;
    db::relink_ink(conn, &old_key, &new_key)?;
    Ok(Some(new_key))
}
