//! Series grouping — the picker's view model.
//!
//! The on-Kindle picker is **grouped by series, always** (no flat toggle):
//! same-series books fold into one
//! navigable collection tile, the opposite default from the desktop gallery
//! (which defaults flat and offers a toggle). Small screen, slow e-ink + LAN,
//! and one-cover-per-collection fetches are the reasons grouping is the only
//! mode here.
//!
//! This module is the **pure** half — no framebuffer, no network — so it lives
//! at the crate root (re-exported by `lib.rs`) and its tests run on the host
//! via `cargo test --lib`, the same split as [`crate::wrap`]. Rendering the
//! resulting [`Cell`]s is `ui::grid` (device-only); the drill-in state machine
//! is `main.rs`.
//!
//! Port of the desktop's `groupBySeries` / `bySeriesIndex` / `seriesNameOf`
//! (`web/library.js`). One deliberate divergence: members are sorted **eagerly**
//! here (the desktop sorts lazily at render). Collation matches the desktop —
//! [`crate::collate::natural_compare`], the port of `naturalCompare`, shared
//! with [`crate::ui::sort`].

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::api::Book;
use crate::collate::natural_compare;

/// A top-level tile: either a standalone book or a series collection. Folded
/// from the filtered+sorted view by [`group_by_series`]; a collection appears
/// at the position of its **first-seen** member so the active sort drives tile
/// order for free.
// `Standalone(Book)` dwarfs the `Series` variant, tripping `large_enum_variant`
// on a 64-bit host build. The picker ships on 32-bit armv7, where the variants
// stay under the lint's threshold (clean on-target); boxing the common
// standalone variant would add a heap allocation per book to this transient,
// bounded view model for no on-device gain.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Entry {
    /// A book with no series — rendered and downloaded as itself.
    Standalone(Book),
    /// A series collection. `books` are the available-to-download members,
    /// sorted into canonical within-series order ([`by_series_index`]) so
    /// `books[0]` is the lead (used for the tile cover) and a drill-in renders
    /// them in reading order.
    Series { name: String, books: Vec<Book> },
}

/// One grid cell in the current view, after a mode is resolved. The renderer
/// and the tap handler both work off this, so top-level and drilled-in views
/// share one code path (a drilled-in member is just a [`CellKind::Book`] cell).
#[derive(Debug)]
pub struct Cell {
    /// The book whose cover art fills the tile: the book itself for a
    /// standalone, or the series' lead member for a collection.
    pub cover_book: Book,
    pub kind: CellKind,
}

/// What a [`Cell`] is and what tapping it does.
#[derive(Debug)]
pub enum CellKind {
    /// A plain book — long-press downloads it.
    Book,
    /// A series collection — a tap drills into its members. `count` is the
    /// number of available-to-download members (the queue already hides
    /// downloaded ones, so this is *not* the full-series total).
    Series { name: String, count: usize },
}

/// A book's series identity, or `None` when it has none (→ stays standalone).
/// Port of `seriesNameOf`: trim, and treat an all-whitespace name as absent.
pub fn series_name_of(b: &Book) -> Option<&str> {
    b.series_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Canonical within-series order: by `series_index` ascending (half-numbers
/// like 1.5 sort correctly), books with no index after those with one, then by
/// title in natural order ([`natural_compare`], so "Vol 9" precedes "Vol 10"
/// without a hand-entered index). Port of `bySeriesIndex`.
pub fn by_series_index(a: &Book, b: &Book) -> Ordering {
    let an = a.series_index.filter(|x| x.is_finite());
    let bn = b.series_index.filter(|x| x.is_finite());
    match (an, bn) {
        (Some(x), Some(y)) => x
            .partial_cmp(&y)
            .unwrap_or(Ordering::Equal)
            .then_with(|| natural_compare(&a.title, &b.title)),
        (Some(_), None) => Ordering::Less, // indexed before un-indexed
        (None, Some(_)) => Ordering::Greater,
        (None, None) => natural_compare(&a.title, &b.title),
    }
}

/// Fold the already-filtered+sorted `view` into entries. A series collection
/// appears at the position of its first-seen member (so the active sort drives
/// order); books with no series stay standalone. Consumes `view` — each book is
/// *moved* into its entry, not cloned.
///
/// Members are sorted eagerly here (the desktop sorts lazily in `seriesCard`):
/// equivalent result, and it lets `books[0]` be the lead everywhere downstream.
pub fn group_by_series(view: Vec<Book>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for b in view {
        match series_name_of(&b).map(str::to_string) {
            None => out.push(Entry::Standalone(b)),
            Some(name) => match seen.get(&name) {
                Some(&i) => {
                    if let Entry::Series { books, .. } = &mut out[i] {
                        books.push(b);
                    }
                }
                None => {
                    seen.insert(name.clone(), out.len());
                    out.push(Entry::Series {
                        name,
                        books: vec![b],
                    });
                }
            },
        }
    }
    for entry in &mut out {
        if let Entry::Series { books, .. } = entry {
            books.sort_by(by_series_index);
        }
    }
    out
}

/// Build the top-level tiles from the grouped entries: a standalone book → a
/// [`CellKind::Book`] cell; a series → a [`CellKind::Series`] cell whose cover
/// is the lead member (`books[0]`, lowest `series_index`) and whose count is the
/// member total.
pub fn cells_for_top(entries: &[Entry]) -> Vec<Cell> {
    entries
        .iter()
        .map(|e| match e {
            Entry::Standalone(b) => Cell {
                cover_book: b.clone(),
                kind: CellKind::Book,
            },
            // A Series entry always holds ≥1 book (created with the first
            // member), so `books[0]` — the lead after the eager index sort — is
            // always present.
            Entry::Series { name, books } => Cell {
                cover_book: books[0].clone(),
                kind: CellKind::Series {
                    name: name.clone(),
                    count: books.len(),
                },
            },
        })
        .collect()
}

/// Build the drilled-in tiles for one series: every member is a plain,
/// downloadable [`CellKind::Book`] cell, in the order `members` arrives (which
/// [`group_by_series`] already sorted by [`by_series_index`]).
pub fn cells_for_series(members: &[Book]) -> Vec<Cell> {
    members
        .iter()
        .map(|b| Cell {
            cover_book: b.clone(),
            kind: CellKind::Book,
        })
        .collect()
}

/// The members of the series named `name` among `entries`, or `None` if no such
/// collection exists (e.g. a drilled-in series filtered away → caller exits to
/// the top level).
pub fn members_of<'a>(entries: &'a [Entry], name: &str) -> Option<&'a [Book]> {
    entries.iter().find_map(|e| match e {
        Entry::Series { name: n, books } if n == name => Some(books.as_slice()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal book with only the fields a given test sets; the rest default.
    /// Mirrors the helper in `ui::sort` / `ui::filter` tests.
    fn book(id: i64) -> Book {
        Book {
            id,
            title: String::new(),
            kfx_sha256: None,
            device_filename: None,
            author: String::new(),
            language: String::new(),
            publisher: None,
            series_name: None,
            series_index: None,
            file_size: 0,
            imported_at: String::new(),
            tags: Vec::new(),
            cover_rev: 0,
            kfx_rev: 0,
            search_key: String::new(),
        }
    }

    fn with_series(id: i64, name: &str, index: Option<f64>) -> Book {
        Book {
            series_name: Some(name.to_string()),
            series_index: index,
            ..book(id)
        }
    }

    fn ids(books: &[Book]) -> Vec<i64> {
        books.iter().map(|b| b.id).collect()
    }

    /// What kind each entry is, in order — `None` = standalone(id), `Some` =
    /// series(name, member ids). Lets a test assert entry order + folding.
    fn shape(entries: &[Entry]) -> Vec<(Option<&str>, Vec<i64>)> {
        entries
            .iter()
            .map(|e| match e {
                Entry::Standalone(b) => (None, vec![b.id]),
                Entry::Series { name, books } => (Some(name.as_str()), ids(books)),
            })
            .collect()
    }

    #[test]
    fn series_name_trims_and_treats_blank_as_none() {
        assert_eq!(
            series_name_of(&with_series(1, "  Saga  ", None)),
            Some("Saga")
        );
        assert_eq!(series_name_of(&with_series(2, "   ", None)), None);
        assert_eq!(series_name_of(&book(3)), None);
    }

    #[test]
    fn folds_at_first_seen_position_preserving_view_order() {
        // View order (already sorted upstream): SagaA, standalone, SagaB,
        // SagaA-again, standalone2. The Saga collection sits where its FIRST
        // member appeared (index 0); the later Saga book folds into it.
        let view = vec![
            with_series(1, "Saga", Some(2.0)),
            book(2),
            with_series(3, "Abyss", Some(1.0)),
            with_series(4, "Saga", Some(1.0)),
            book(5),
        ];
        let entries = group_by_series(view);
        assert_eq!(
            shape(&entries),
            vec![
                // Saga at its first-seen slot; members re-sorted by index → 4,1.
                (Some("Saga"), vec![4, 1]),
                (None, vec![2]),
                (Some("Abyss"), vec![3]),
                (None, vec![5]),
            ]
        );
    }

    #[test]
    fn members_sort_by_index_nulls_last_then_title() {
        let view = vec![
            Book {
                title: "Zeta".into(),
                ..with_series(1, "S", None)
            },
            with_series(2, "S", Some(2.5)),
            with_series(3, "S", Some(1.0)),
            Book {
                title: "Alpha".into(),
                ..with_series(4, "S", None)
            },
        ];
        let entries = group_by_series(view);
        // 1.0, 2.5, then the two index-less ones by title (Alpha < Zeta).
        assert_eq!(shape(&entries), vec![(Some("S"), vec![3, 2, 4, 1])]);
    }

    #[test]
    fn singleton_series_stays_a_collection() {
        // Locked decision 2: a one-member series is still a series (the rest are
        // just already downloaded / not present), never flattened to a book.
        let entries = group_by_series(vec![with_series(1, "Lonely", Some(1.0))]);
        assert_eq!(shape(&entries), vec![(Some("Lonely"), vec![1])]);
        let cells = cells_for_top(&entries);
        assert_eq!(cells.len(), 1);
        match &cells[0].kind {
            CellKind::Series { name, count } => {
                assert_eq!(name, "Lonely");
                assert_eq!(*count, 1);
            }
            CellKind::Book => panic!("singleton series flattened to a book"),
        }
    }

    #[test]
    fn cells_for_top_picks_lead_cover_and_member_count() {
        let view = vec![
            with_series(1, "S", Some(3.0)),
            with_series(2, "S", Some(1.0)), // lead (lowest index)
            with_series(3, "S", Some(2.0)),
            book(9),
        ];
        let entries = group_by_series(view);
        let cells = cells_for_top(&entries);
        assert_eq!(cells.len(), 2);
        match &cells[0].kind {
            CellKind::Series { count, .. } => assert_eq!(*count, 3),
            _ => panic!("expected series cell"),
        }
        // Cover source is the lead (id 2), not the first-seen-in-view (id 1).
        assert_eq!(cells[0].cover_book.id, 2);
        // Standalone passes through as a Book cell with itself as the cover.
        assert!(matches!(cells[1].kind, CellKind::Book));
        assert_eq!(cells[1].cover_book.id, 9);
    }

    #[test]
    fn members_of_finds_or_misses() {
        let entries = group_by_series(vec![
            with_series(1, "S", Some(1.0)),
            with_series(2, "S", Some(2.0)),
            book(3),
        ]);
        assert_eq!(ids(members_of(&entries, "S").unwrap()), vec![1, 2]);
        assert!(members_of(&entries, "Nope").is_none());
        // cells_for_series turns members into plain downloadable Book cells.
        let cells = cells_for_series(members_of(&entries, "S").unwrap());
        assert_eq!(cells.len(), 2);
        assert!(cells.iter().all(|c| matches!(c.kind, CellKind::Book)));
    }

    #[test]
    fn empty_view_is_no_entries() {
        assert!(group_by_series(Vec::new()).is_empty());
        assert!(cells_for_top(&[]).is_empty());
    }
}
