//! Cache each book's position-axis extent in `books.max_position`.
//!
//! A Kindle's reading-session log redacts every title (`Title:<private>`) but
//! states the book's last valid position on each event. That integer is the only
//! identity on offer, so attributing reading time means comparing it against the
//! same axis computed from the book Sidle holds — and computing that axis means
//! parsing the whole KFX, which is far too slow to do per lookup. Hence a cached
//! column, filled here.
//!
//! Deliberately incremental rather than a migration step: a library of a couple
//! of thousand books takes minutes to index, which is fine in the background and
//! unacceptable while opening the app. Rows are filled once and then skipped, so
//! steady state is only whatever was imported since the last pass.

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
    /// True when the pass stopped early at the caller's request. What it had
    /// already indexed is committed; the next pass resumes from there.
    pub cancelled: bool,
}

/// The exclusive end of `bytes`' position axis, or `None` when the container
/// does not parse or has no position map.
///
/// Only the position map is taken; the source text a full index would also build
/// is pure waste here.
pub fn of_kfx(bytes: &[u8]) -> Option<i64> {
    let mut book = Book::from_bytes(bytes, Format::Kfx).ok()?;
    let positions = book.position_map()?;
    Some(positions.max_position())
}

/// Compute and store the extent for every book that lacks one.
///
/// Idempotent and resumable, which is what makes it safe to interrupt: each book
/// is committed as it is computed, so cancelling — or crashing — keeps
/// everything done so far and the next pass starts where this one stopped.
/// Individual failures never abort the pass either; an unreadable file is one
/// unattributable book, not a broken index.
///
/// Minutes across a large library on its first run, near-instant afterwards,
/// so `watch` is not optional in practice: see [`job::Report`].
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
        let extent = std::fs::read(&kfx_path).ok().and_then(|b| of_kfx(&b));
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
        // A book with no readable file still gets a value, so the pass that
        // follows does not keep retrying it.
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
        // Unrelated books do collide on length; the caller has to see both
        // rather than be handed an arbitrary one.
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
                publisher: None,
                published_at: None,
                series_name: None,
                series_index: None,
                tags: &[],
                title_romaji: "",
                author_romaji: "",
            },
        )
        .unwrap()
    }
}
