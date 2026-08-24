//! Cache each book's position-axis extent in `books.max_position`.
//!
//! A Kindle's reading-session log redacts every title (`Title:<private>`) but
//! states the book's last valid position on each event. That integer is the only
//! identity on offer, so attributing reading time means comparing it against the
//! same axis computed from the book Sidle holds — and computing that axis means
//! parsing the whole KFX, which is far too slow to do per lookup. Hence a cached
//! column, filled here.
//!
//! Incremental, not a migration step: a library of a couple of thousand books
//! takes minutes to index. Each row fills once and is skipped after, and
//! steady state is only whatever was imported since the last pass.
//!
//! **A book is measured when its KFX is produced**, which is the only moment its
//! axis can change and the only one early enough to matter: a book read the day
//! it was added is attributable only if it was indexed before that day's reading
//! was synced, and the everyday case is a device that reports its reading within
//! hours. A background sweep at start covers the rows nothing has filled.
//!
//! [`backfill`] is that same work with progress reporting, for the one path that
//! wants it done *before* a bulk of history lands — the manual archive import.
//! That import is a warm-start and testing route that most libraries never take,
//! so **nothing may depend on it having run**: a column filled only there is a
//! column that is empty on a normal install.
//!
//! An extent that arrives late is not lost time. A session whose position
//! matched nothing is kept, unattributed, and re-examined on every attribution
//! pass: indexing a book later claims the reading recorded against it.

use bokai::model::{Book, Format};
use rusqlite::Connection;

use super::{db, job};

/// Outcome of one [`backfill`] pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Filled {
    /// Books that produced an extent.
    pub indexed: usize,
    /// Books whose file is missing, unreadable, or carries no position map.
    /// Recorded as 0 so they are not retried forever.
    pub skipped: usize,
    /// True when the pass stopped early at the caller's request. What it
    /// indexed is committed, and the next pass resumes from there.
    pub cancelled: bool,
}

/// The exclusive end of `bytes`' position axis, or `None` for a container
/// that does not parse or carries no position map. The position map alone is
/// read; a full index's source text is left.
pub fn of_kfx(bytes: &[u8]) -> Option<i64> {
    let mut book = Book::from_bytes(bytes, Format::Kfx).ok()?;
    let positions = book.position_map()?;
    Some(positions.max_position())
}

/// The extent of the KFX at `path`, or `None` for a file that cannot be read
/// or carries no position map. The half of indexing that touches no database,
/// and parses a whole container.
pub fn of_file(path: impl AsRef<std::path::Path>) -> Option<i64> {
    std::fs::read(path).ok().and_then(|bytes| of_kfx(&bytes))
}

/// Compute and store the extent for every book that lacks one. Each book
/// commits as it is computed, and one unreadable file leaves that book
/// unattributable. `watch` reports the pass — see [`job::Report`].
pub fn backfill(conn: &Connection, watch: job::Watch<'_>) -> rusqlite::Result<Filled> {
    let mut out = Filled::default();
    let pending = db::books_missing_max_position(conn)?;
    let total = pending.len();
    for (done, (book_id, kfx_path)) in pending.into_iter().enumerate() {
        let name = std::path::Path::new(&kfx_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if watch(job::Report {
            phase: "index",
            done,
            total,
            label: &name,
        })
        .is_break()
        {
            out.cancelled = true;
            break;
        }
        let extent = of_file(&kfx_path);
        if extent.is_some() {
            out.indexed += 1;
        } else {
            out.skipped += 1;
        }
        db::set_max_position(conn, book_id, extent)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        db::open(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn the_extent_column_starts_empty_and_fills_once() {
        let c = conn();
        // A book with no readable file takes a value, which the next pass
        // skips.
        let id = insert_stub(&c, "deadbeef", Some("/nonexistent/book.kfx"));
        assert_eq!(db::books_missing_max_position(&c).unwrap().len(), 1);
        let filled = backfill(&c, &mut job::ignore).unwrap();
        assert_eq!((filled.indexed, filled.skipped), (0, 1));
        assert!(db::books_missing_max_position(&c).unwrap().is_empty());
        // 0 is unreachable as a device's last position, so it never joins.
        assert!(db::books_with_last_position(&c, 0).unwrap().is_empty());
        assert!(db::books_with_last_position(&c, -1).unwrap().contains(&id));
    }

    #[test]
    fn a_device_position_is_one_less_than_the_extent() {
        let c = conn();
        let id = insert_stub(&c, "cafe", None);
        db::set_max_position(&c, id, Some(189_439)).unwrap();
        // The device reports the last valid position; bokai's extent is the
        // exclusive end, so the join must offset by exactly one.
        assert_eq!(db::books_with_last_position(&c, 189_438).unwrap(), vec![id]);
        assert!(
            db::books_with_last_position(&c, 189_439)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn two_books_of_one_length_are_both_returned() {
        let c = conn();
        let a = insert_stub(&c, "aaa", None);
        let b = insert_stub(&c, "bbb", None);
        db::set_max_position(&c, a, Some(174_897)).unwrap();
        db::set_max_position(&c, b, Some(174_897)).unwrap();
        // Unrelated books collide on length, and the caller sees both.
        assert_eq!(
            db::books_with_last_position(&c, 174_896).unwrap(),
            vec![a, b]
        );
    }

    fn insert_stub(c: &Connection, sha: &str, kfx: Option<&str>) -> i64 {
        db::insert_book(
            c,
            &db::NewBook {
                sha256: sha,
                title: "t",
                author: "",
                language: "en",
                ppd: None,
                epub_path: None,
                cover_path: None,
                kfx_path: kfx,
                kfx_sha256: kfx.map(|_| sha),
                pdf_path: None,
                file_size: 1,
                imported_at: "2026-08-09T00:00:00Z",
                asin: None,
                amazon_asin: None,
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
                title_romaji: "",
                author_romaji: "",
                source_format: None,
            },
        )
        .unwrap()
    }
}
