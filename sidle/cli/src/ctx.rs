//! The library this run works on, and how it reports.

use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use sidle_core::library::db;
use sidle_core::library::paths::LibraryPaths;

pub struct Ctx {
    pub paths: LibraryPaths,
    /// The conversion sweep hands this to worker threads, which take it
    /// through [`sidle_core::library::db::Access`] to record a finished book.
    pub db: std::sync::Mutex<Connection>,
    /// Report as JSON.
    pub json: bool,
}

impl Ctx {
    /// Open the configured library, or the one under `root`.
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

    /// Borrow the connection. The guard is not re-entrant, and a command
    /// ending in a sweep drops it first: the sweep's workers take the same
    /// mutex through [`db::Access`].
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

/// `root` as an absolute path. A stored file path is root-relative and
/// resolves against the absolute directory SQLite reports for the open
/// database, so both sides of a path comparison take the one form.
pub fn absolute(root: PathBuf) -> Result<PathBuf> {
    std::path::absolute(&root).with_context(|| format!("resolve {}", root.display()))
}
