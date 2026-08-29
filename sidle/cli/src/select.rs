//! Which books a command acts on.
//!
//! One flag group, shared by every command that takes books, so `list` is how
//! you check what a `convert` or a `remove` is about to touch. Filters are
//! ANDed; a run that names none is refused, and `--all` says the whole library
//! out loud.

use anyhow::Result;
use clap::Args;
use rusqlite::Connection;
use sidle_core::library::db::{self, BookRow};

#[derive(Args, Debug, Clone, Default)]
pub struct Select {
    /// Every book in the library.
    #[arg(long)]
    pub all: bool,
    /// Library row id. Repeatable.
    #[arg(long = "id", value_name = "N")]
    pub ids: Vec<i64>,
    /// Content hash, or any unambiguous prefix of one. Repeatable.
    #[arg(long = "sha", value_name = "HASH")]
    pub shas: Vec<String>,
    /// The id baked into the file, or the catalogue ASIN. Repeatable.
    #[arg(long = "asin", value_name = "ASIN")]
    pub asins: Vec<String>,
    /// Substring of the title (case-insensitive).
    #[arg(long = "title-like", value_name = "TEXT")]
    pub title_like: Option<String>,
    /// Substring of the author (case-insensitive).
    #[arg(long = "author-like", value_name = "TEXT")]
    pub author_like: Option<String>,
    /// Series name (exact).
    #[arg(long, value_name = "NAME")]
    pub series: Option<String>,
    /// Carries this tag. Repeatable; a book must carry all of them.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,
    /// Language code (exact, canonical form — `ja`, `en`, `zh-Hant`).
    #[arg(long, value_name = "CODE")]
    pub lang: Option<String>,
    /// Conversion status: `done`, `pending`, `converting`, `error`.
    #[arg(long, value_name = "STATUS")]
    pub status: Option<String>,
    /// Conversion direction: `epub_to_kfx`, `kfx_to_epub`, `pdf_to_kfx`,
    /// `kfx_to_pdf`.
    #[arg(long, value_name = "KIND")]
    pub kind: Option<String>,

    /// The format that arrived at import: `azw3`, `mobi`, `epub`, `kfx`,
    /// `kfx-zip`, `pdf`, `aozora`.
    #[arg(long = "source-format", value_name = "FMT")]
    pub source_format: Option<String>,
    /// Only books that have this on disk: `kfx`, `epub`, `pdf`, `cover`.
    /// Repeatable.
    #[arg(long = "has", value_name = "WHAT")]
    pub has: Vec<String>,
    /// Only books MISSING this: `kfx`, `epub`, `pdf`, `cover`, `asin`,
    /// `extent`. Repeatable.
    #[arg(long = "missing", value_name = "WHAT")]
    pub missing: Vec<String>,
    /// Stop after this many (after every other filter, in library order).
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
}

impl Select {
    /// True when no filter was given at all.
    pub fn is_unset(&self) -> bool {
        !self.all
            && self.ids.is_empty()
            && self.shas.is_empty()
            && self.asins.is_empty()
            && self.title_like.is_none()
            && self.author_like.is_none()
            && self.series.is_none()
            && self.tags.is_empty()
            && self.lang.is_none()
            && self.status.is_none()
            && self.kind.is_none()
            && self.source_format.is_none()
            && self.has.is_empty()
            && self.missing.is_empty()
    }

    /// The books this selection names, in library order.
    pub fn resolve(&self, conn: &Connection) -> Result<Vec<BookRow>> {
        if self.is_unset() {
            anyhow::bail!(
                "no books selected — pass --all for the whole library, or narrow with \
                 --id/--sha/--asin/--title-like/--author-like/--series/--tag/--lang/\
                 --status/--kind/--has/--missing"
            );
        }
        for what in self.has.iter().chain(&self.missing) {
            if !matches!(
                what.as_str(),
                "kfx" | "epub" | "pdf" | "cover" | "asin" | "extent"
            ) {
                anyhow::bail!("unknown file kind {what:?} (kfx, epub, pdf, cover, asin, extent)");
            }
        }
        // The position axis is a column, not a file, and the query for "which
        // books lack one" exists — ask it once, not per row.
        let unmeasured: std::collections::HashSet<i64> =
            if self.has.iter().chain(&self.missing).any(|w| w == "extent") {
                db::books_missing_max_position(conn)?
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect()
            } else {
                std::collections::HashSet::new()
            };

        let lower = |s: &Option<String>| s.as_ref().map(|v| v.to_lowercase());
        let title = lower(&self.title_like);
        let author = lower(&self.author_like);

        let mut out: Vec<BookRow> = db::list_books(conn)?
            .into_iter()
            .filter(|b| self.ids.is_empty() || self.ids.contains(&b.id))
            .filter(|b| {
                self.shas.is_empty() || self.shas.iter().any(|s| b.sha256.starts_with(s.as_str()))
            })
            .filter(|b| {
                self.asins.is_empty()
                    || self.asins.iter().any(|a| {
                        b.asin.as_deref() == Some(a.as_str())
                            || b.amazon_asin.as_deref() == Some(a.as_str())
                    })
            })
            .filter(|b| match &title {
                Some(t) => b.title.to_lowercase().contains(t),
                None => true,
            })
            .filter(|b| match &author {
                Some(a) => b.author.to_lowercase().contains(a),
                None => true,
            })
            .filter(|b| match &self.series {
                Some(s) => b.series_name.as_deref() == Some(s.as_str()),
                None => true,
            })
            .filter(|b| self.tags.iter().all(|t| b.tags.contains(t)))
            .filter(|b| match &self.lang {
                Some(l) => &b.language == l,
                None => true,
            })
            .filter(|b| match &self.status {
                Some(s) => &b.status == s,
                None => true,
            })
            .filter(|b| match &self.kind {
                Some(k) => b.kind.as_deref() == Some(k.as_str()),
                None => true,
            })
            .filter(|b| match &self.source_format {
                Some(f) => b.source_format.as_deref() == Some(f.as_str()),
                None => true,
            })
            .filter(|b| self.has.iter().all(|w| has(b, w, &unmeasured)))
            .filter(|b| self.missing.iter().all(|w| !has(b, w, &unmeasured)))
            .collect();

        // Every id the caller named must exist: a script that asks for book 412
        // and silently gets nothing has been lied to.
        for id in &self.ids {
            if !out.iter().any(|b| b.id == *id) {
                anyhow::bail!("no book with id {id}");
            }
        }
        if let Some(limit) = self.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    /// Resolve, and refuse an empty result — for a command whose empty run
    /// reads as a successful no-op.
    pub fn resolve_nonempty(&self, conn: &Connection) -> Result<Vec<BookRow>> {
        let books = self.resolve(conn)?;
        if books.is_empty() {
            anyhow::bail!("no book matches that selection");
        }
        Ok(books)
    }
}

/// Whether a book has `what` — a file on disk, or a stored value. A path
/// recorded in the row is not a file present, and the file kinds check the
/// disk: a library restored from a partial copy holds rows naming neither.
fn has(book: &BookRow, what: &str, unmeasured: &std::collections::HashSet<i64>) -> bool {
    let on_disk = |p: &Option<String>| {
        p.as_deref()
            .is_some_and(|p| std::path::Path::new(p).exists())
    };
    match what {
        "kfx" => on_disk(&book.kfx_path),
        "epub" => on_disk(&book.epub_path),
        "pdf" => on_disk(&book.pdf_path),
        "cover" => on_disk(&book.cover_path),
        "asin" => book.amazon_asin.is_some(),
        // The position axis is what lets the Reading Log recognise the book at
        // all; without it, time read on this book is counted nowhere.
        "extent" => !unmeasured.contains(&book.id),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidle_core::library::db::NewBook;

    /// A library holding three books that differ in every axis the flags can
    /// filter on. The KFX path is a real file so the on-disk checks have
    /// something to find.
    fn library(dir: &std::path::Path) -> Connection {
        let conn = db::open(&dir.join("library.db")).unwrap();
        let kfx = dir.join("present.kfx");
        std::fs::write(&kfx, b"KFX").unwrap();
        let kfx = kfx.to_string_lossy().to_string();

        let insert = |sha: &str, title: &str, author: &str, lang: &str, with_file: bool| {
            let tags = vec!["manga".to_string()];
            db::insert_book(
                &conn,
                &NewBook {
                    sha256: sha,
                    title,
                    author,
                    language: lang,
                    ppd: None,
                    epub_path: None,
                    cover_path: None,
                    kfx_path: with_file.then_some(kfx.as_str()),
                    kfx_sha256: with_file.then_some("deadbeef"),
                    pdf_path: None,
                    file_size: 3,
                    imported_at: "2026-01-01",
                    asin: None,
                    amazon_asin: (sha == "aaa").then_some("B00CATALOG"),
                    publisher: None,
                    published_at: None,
                    series_name: (sha != "ccc").then_some("Saga"),
                    series_index: None,
                    tags: if sha == "ccc" { &[] } else { &tags },
                    title_romaji: "",
                    author_romaji: "",
                    source_format: Some(if sha == "bbb" { "azw3" } else { "epub" }),
                },
            )
            .unwrap()
        };
        insert("aaa", "Wool", "Hugh Howey", "en", true);
        insert("bbb", "人間失格", "太宰 治", "ja", true);
        insert("ccc", "Shift", "Hugh Howey", "en", false);
        conn
    }

    #[test]
    fn source_format_selects_by_the_file_that_arrived() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = library(tmp.path());
        let mut select = Select {
            source_format: Some("azw3".into()),
            ..Default::default()
        };
        assert_eq!(titles(&select, &conn).len(), 1);
        select.source_format = Some("epub".into());
        assert_eq!(titles(&select, &conn).len(), 2);
        // `source_format` names the file that arrived, which
        // `conversion_jobs.kind` does not: a `.azw3` import stores its EPUB and
        // reconverts from that.
        select.source_format = Some("mobi".into());
        assert!(titles(&select, &conn).is_empty());
    }

    fn titles(select: &Select, conn: &Connection) -> Vec<String> {
        let mut t: Vec<String> = select
            .resolve(conn)
            .unwrap()
            .into_iter()
            .map(|b| b.title)
            .collect();
        t.sort();
        t
    }

    #[test]
    fn a_selection_with_no_flags_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = library(tmp.path());
        // Silence is the one answer a sweep must never get: `convert` with a
        // typo'd flag rebuilds the whole library.
        assert!(Select::default().resolve(&conn).is_err());
    }

    #[test]
    fn filters_are_anded() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = library(tmp.path());

        let all = Select {
            all: true,
            ..Default::default()
        };
        assert_eq!(titles(&all, &conn).len(), 3);

        let english_in_series = Select {
            all: true,
            lang: Some("en".into()),
            series: Some("Saga".into()),
            ..Default::default()
        };
        assert_eq!(titles(&english_in_series, &conn), vec!["Wool".to_string()]);

        let by_author = Select {
            author_like: Some("howey".into()),
            ..Default::default()
        };
        assert_eq!(
            titles(&by_author, &conn),
            vec!["Shift".to_string(), "Wool".to_string()],
            "the author match is case-insensitive and needs no other flag"
        );
    }

    #[test]
    fn presence_is_read_from_the_disk_not_the_row() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = library(tmp.path());
        let with_kfx = Select {
            all: true,
            has: vec!["kfx".into()],
            ..Default::default()
        };
        assert_eq!(titles(&with_kfx, &conn).len(), 2);

        // Delete the file behind one of them: the row names a KFX that is gone.
        std::fs::remove_file(tmp.path().join("present.kfx")).unwrap();
        assert!(titles(&with_kfx, &conn).is_empty());
    }

    #[test]
    fn a_named_id_that_does_not_exist_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = library(tmp.path());
        // A script that asks for book 999 and silently gets nothing has been
        // lied to about what it just did.
        let missing = Select {
            ids: vec![999],
            ..Default::default()
        };
        assert!(missing.resolve(&conn).is_err());
    }

    #[test]
    fn an_unknown_file_kind_is_refused_rather_than_matching_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = library(tmp.path());
        let typo = Select {
            all: true,
            missing: vec!["kfz".into()],
            ..Default::default()
        };
        assert!(typo.resolve(&conn).is_err());
    }

    #[test]
    fn a_sha_prefix_is_enough() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = library(tmp.path());
        let by_prefix = Select {
            shas: vec!["bb".into()],
            ..Default::default()
        };
        assert_eq!(titles(&by_prefix, &conn), vec!["人間失格".to_string()]);
    }

    #[test]
    fn limit_applies_after_the_filters() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = library(tmp.path());
        let capped = Select {
            all: true,
            limit: Some(2),
            ..Default::default()
        };
        assert_eq!(capped.resolve(&conn).unwrap().len(), 2);
    }
}
