//! The library this run works on, and how it reports.

use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use sidle_core::library::db;
use sidle_core::library::paths::LibraryPaths;

pub struct Ctx {
    pub paths: LibraryPaths,
    /// Behind a mutex because the conversion sweep hands it to worker threads,
    /// which take it only to record a finished book — see
    /// [`sidle_core::library::db::Access`].
    pub db: std::sync::Mutex<Connection>,
    /// Report as JSON rather than prose.
    pub json: bool,
}

impl Ctx {
    /// Open the library the desktop app would open, or the one under `root`.
    pub fn open(root: Option<PathBuf>, json: bool) -> Result<Self> {
        let paths = match root {
            Some(root) => LibraryPaths {
                root: absolute(root)?,
            },
            None => LibraryPaths::resolve().context("resolve the library root")?,
        };
        let db_path = paths.db();
        if !db_path.is_file() {
            anyhow::bail!("no library at {}", db_path.display());
        }
        let conn = db::open(&db_path).with_context(|| format!("open {}", db_path.display()))?;
        Ok(Self {
            paths,
            db: std::sync::Mutex::new(conn),
            json,
        })
    }

    /// Borrow the connection.
    ///
    /// Every command is single-threaded except the conversion sweep, whose
    /// workers borrow through [`db::Access`] — so a command that ends in a sweep
    /// must release this first. Nothing here is re-entrant.
    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Print `value` as JSON, or run `prose` — whichever this run asked for.
    pub fn report<T: Serialize>(&self, value: &T, prose: impl FnOnce()) -> Result<()> {
        if self.json {
            println!("{}", serde_json::to_string_pretty(value)?);
        } else {
            prose();
        }
        Ok(())
    }

    /// A line of running commentary, silent in JSON mode so stdout stays a
    /// single parsable document.
    pub fn say(&self, msg: impl std::fmt::Display) {
        if !self.json {
            println!("{msg}");
        }
    }
}

/// A library root is a location, so it is absolute.
///
/// The stored file paths are root-relative and resolved against the directory
/// SQLite reports for the open database — which is absolute. A relative `--root`
/// would have every path comparison in the library measuring one form against
/// the other, and a book's own file would read as a stranger sitting on its
/// name.
pub fn absolute(root: PathBuf) -> Result<PathBuf> {
    std::path::absolute(&root).with_context(|| format!("resolve {}", root.display()))
}
