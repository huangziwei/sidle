//! Sort model for the picker.
//!
//! Mirrors sidle-tauri's sort (`web/library.js` `SORT_KEYS` :50-59, `sortedBooks`
//! / `sortValue` :449-487) minus the `on_kindle` key — the picker hides
//! already-downloaded books (`main.rs`), so that key would be constant here.
//!
//! One comparator over `&Book`, applied to the view (the post-filter book list)
//! before paging. The default is Date-added-descending, matching both the
//! desktop default (`library.js:9`) and the server's `ORDER BY imported_at DESC`
//! (`core/src/library/db.rs:493`) — only now it's labelled in the UI instead of
//! reading as a random order.
//!
//! Collation is [`crate::collate::natural_compare`] (port of the desktop's
//! `naturalCompare`): digit runs compare numerically — "Vol 2" before "Vol 10" —
//! while the non-digit segments stay code-point order (correct for ASCII,
//! code-point order for CJK). Revisit only if kana/kanji ordering looks wrong on
//! device.

use std::borrow::Cow;
use std::cmp::Ordering;

use crate::api::Book;
use crate::collate::natural_compare;

/// The seven sort keys, in the order shown in the Sort overlay (matches the
/// desktop popover order). `on_kindle` is intentionally absent (see module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Title,
    Author,
    Series,
    Publisher,
    Language,
    DateAdded,
    Size,
}

impl SortKey {
    /// Display order for the overlay list.
    pub const ALL: [SortKey; 7] = [
        SortKey::Title,
        SortKey::Author,
        SortKey::Series,
        SortKey::Publisher,
        SortKey::Language,
        SortKey::DateAdded,
        SortKey::Size,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SortKey::Title => "Title",
            SortKey::Author => "Author",
            SortKey::Series => "Series",
            SortKey::Publisher => "Publisher",
            SortKey::Language => "Language",
            SortKey::DateAdded => "Date added",
            SortKey::Size => "Size",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    pub fn toggled(self) -> Self {
        match self {
            SortDir::Asc => SortDir::Desc,
            SortDir::Desc => SortDir::Asc,
        }
    }

    /// Compact arrow for the strip header / overlay (same Unicode arrow block as
    /// the `← →` the pager strip already renders).
    pub fn arrow(self) -> &'static str {
        match self {
            SortDir::Asc => "↑",
            SortDir::Desc => "↓",
        }
    }

    /// Spelled-out direction for the overlay's Direction row — carries the
    /// meaning even if the arrow glyph isn't in the font.
    pub fn word(self) -> &'static str {
        match self {
            SortDir::Asc => "Ascending",
            SortDir::Desc => "Descending",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortState {
    pub key: SortKey,
    pub dir: SortDir,
}

impl Default for SortState {
    fn default() -> Self {
        SortState {
            key: SortKey::DateAdded,
            dir: SortDir::Desc,
        }
    }
}

impl SortState {
    /// Sort `books` in place by this key/direction. Stable so books that tie on
    /// the key keep their prior (server) relative order.
    pub fn apply(self, books: &mut [Book]) {
        books.sort_by(|a, b| compare(a, b, self));
    }

    /// One-line label for the grid header: `"Date added ↓"`.
    pub fn header(self) -> String {
        format!("{} {}", self.key.label(), self.dir.arrow())
    }
}

/// A book's comparable value for one key. `Missing` always sorts *after* any
/// present value regardless of direction — the null-handling from
/// `library.js` `sortedBooks` (:456-458): a book with no value for the active
/// key sinks to the bottom whether ascending or descending.
enum SortVal<'a> {
    Text(Cow<'a, str>),
    Num(i64),
    Missing,
}

fn value<'a>(book: &'a Book, key: SortKey) -> SortVal<'a> {
    match key {
        // Non-Option columns are always present (empty string sorts as empty,
        // matching the desktop, which only sinks JSON `null`/`undefined`).
        SortKey::Title => SortVal::Text(Cow::Borrowed(&book.title)),
        SortKey::Author => SortVal::Text(Cow::Borrowed(&book.author)),
        SortKey::Language => SortVal::Text(Cow::Borrowed(&book.language)),
        SortKey::DateAdded => SortVal::Text(Cow::Borrowed(&book.imported_at)),
        SortKey::Size => SortVal::Num(book.file_size),
        // Option column: absent → sinks last.
        SortKey::Publisher => match &book.publisher {
            Some(p) => SortVal::Text(Cow::Borrowed(p)),
            None => SortVal::Missing,
        },
        SortKey::Series => series_key(book),
    }
}

/// Composite series key, port of `seriesSortKey` (`library.js:477-487`):
/// `series_name` directly followed by the index `round(index*10)` zero-padded
/// to 8 digits, so a single `str` compare orders by name then by index, and
/// half-numbered entries (1.5, 2.5) sort correctly. No series name → `Missing`
/// (sinks last); a name with no index → a max sentinel index (sorts after the
/// numbered volumes within that series).
fn series_key(book: &Book) -> SortVal<'static> {
    let name = book
        .series_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(name) = name else {
        return SortVal::Missing;
    };
    let idx = match book.series_index {
        Some(i) if i.is_finite() => (i * 10.0).round() as i64,
        _ => 99_999_999,
    };
    SortVal::Text(Cow::Owned(format!("{name}{idx:08}")))
}

fn compare(a: &Book, b: &Book, state: SortState) -> Ordering {
    let av = value(a, state.key);
    let bv = value(b, state.key);
    match (&av, &bv) {
        (SortVal::Missing, SortVal::Missing) => Ordering::Equal,
        (SortVal::Missing, _) => Ordering::Greater, // a sinks
        (_, SortVal::Missing) => Ordering::Less,    // b sinks
        _ => {
            let ord = cmp_present(&av, &bv);
            match state.dir {
                SortDir::Asc => ord,
                SortDir::Desc => ord.reverse(),
            }
        }
    }
}

/// Compare two present values. For a given key both are the same variant; the
/// cross-variant arms can't occur in practice but resolve deterministically.
fn cmp_present(a: &SortVal<'_>, b: &SortVal<'_>) -> Ordering {
    match (a, b) {
        (SortVal::Text(x), SortVal::Text(y)) => natural_compare(x, y),
        (SortVal::Num(x), SortVal::Num(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal book with only the fields a given test sets; the rest default.
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
            search_key: String::new(),
        }
    }

    fn ids(books: &[Book]) -> Vec<i64> {
        books.iter().map(|b| b.id).collect()
    }

    #[test]
    fn title_asc_then_desc() {
        let mut v = vec![
            Book { title: "Banana".into(), ..book(1) },
            Book { title: "apple".into(), ..book(2) },
            Book { title: "Cherry".into(), ..book(3) },
        ];
        SortState { key: SortKey::Title, dir: SortDir::Asc }.apply(&mut v);
        // Code-point order: uppercase 'B','C' (0x42,0x43) precede lowercase
        // 'a' (0x61). This is the documented stdlib-collation behavior.
        assert_eq!(ids(&v), vec![1, 3, 2]);
        SortState { key: SortKey::Title, dir: SortDir::Desc }.apply(&mut v);
        assert_eq!(ids(&v), vec![2, 3, 1]);
    }

    #[test]
    fn size_is_numeric_not_lexical() {
        let mut v = vec![
            Book { file_size: 9, ..book(1) },
            Book { file_size: 100, ..book(2) },
            Book { file_size: 20, ..book(3) },
        ];
        SortState { key: SortKey::Size, dir: SortDir::Asc }.apply(&mut v);
        assert_eq!(ids(&v), vec![1, 3, 2]); // 9 < 20 < 100, not "100" < "20" < "9"
    }

    #[test]
    fn missing_publisher_sinks_in_both_directions() {
        let mut v = vec![
            Book { publisher: None, ..book(1) },
            Book { publisher: Some("Aperture".into()), ..book(2) },
            Book { publisher: Some("Black Lake".into()), ..book(3) },
        ];
        SortState { key: SortKey::Publisher, dir: SortDir::Asc }.apply(&mut v);
        assert_eq!(ids(&v), vec![2, 3, 1]); // missing last
        SortState { key: SortKey::Publisher, dir: SortDir::Desc }.apply(&mut v);
        assert_eq!(ids(&v), vec![3, 2, 1]); // still last, not flipped to front
    }

    #[test]
    fn series_orders_by_name_then_index_halfsteps() {
        let mut v = vec![
            Book { series_name: Some("Saga".into()), series_index: Some(2.0), ..book(1) },
            Book { series_name: Some("Saga".into()), series_index: Some(1.5), ..book(2) },
            Book { series_name: Some("Saga".into()), series_index: Some(1.0), ..book(3) },
            Book { series_name: Some("Abyss".into()), series_index: Some(1.0), ..book(4) },
            Book { series_name: None, series_index: None, ..book(5) },
        ];
        SortState { key: SortKey::Series, dir: SortDir::Asc }.apply(&mut v);
        // Abyss#1, then Saga 1 < 1.5 < 2, then the seriesless book last.
        assert_eq!(ids(&v), vec![4, 3, 2, 1, 5]);
    }

    #[test]
    fn cjk_author_split_is_a_facet_concern_sort_uses_raw_field() {
        // Sort compares the raw author string as-is (the CJK comma split lives
        // in the facet extractor, not here). Just assert determinism + that a
        // CJK string sorts by code point without panicking on byte boundaries.
        let mut v = vec![
            Book { author: "村上春樹".into(), ..book(1) },
            Book { author: "夏目漱石".into(), ..book(2) },
        ];
        SortState { key: SortKey::Author, dir: SortDir::Asc }.apply(&mut v);
        // 村 (U+6751) < 夏 (U+590F)? No: 0x590F < 0x6751, so 夏目漱石 (id 2) first.
        assert_eq!(ids(&v), vec![2, 1]);
    }
}
