//! Facet filter model.
//!
//! Mirrors sidle-tauri's facets (`web/library.js` `FACETS` :61,
//! `extractFacetValues` :514-537, `matchesFacets` :549-555, `facetOptions`
//! :578-600) minus the `on_kindle` facet — the picker hides already-downloaded
//! books, so it'd be a no-op (every visible book is off-device).
//!
//! Semantics: **AND across facets, OR within a facet.** A book passes if, for
//! every active facet, at least one of its values for that facet is selected.
//! Facet option lists **cascade** (leave-one-out): the options offered for facet
//! X are computed against the books matching all *other* active facets, so
//! picking language=jp narrows the Author options but not the Language options.
//!
//! Collation is stdlib `str::cmp` (matches `ui::sort`); the "—" sentinel (a
//! book with no value for a facet) sorts last and is itself selectable.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use crate::api::Book;

/// Sentinel value for "this book has no value for this facet" — itself a
/// selectable option (so "books with no author" is a filter you can pick).
/// Mirrors the desktop's `"—"`.
pub const NONE: &str = "—";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Facet {
    Language,
    Author,
    Publisher,
    Series,
    Tags,
}

impl Facet {
    /// Display order in the Filter menu.
    pub const ALL: [Facet; 5] = [
        Facet::Language,
        Facet::Author,
        Facet::Publisher,
        Facet::Series,
        Facet::Tags,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Facet::Language => "Language",
            Facet::Author => "Author",
            Facet::Publisher => "Publisher",
            Facet::Series => "Series",
            Facet::Tags => "Tags",
        }
    }
}

/// Per-facet selected-value sets. An absent or empty set = facet inactive.
/// `BTreeSet` keeps the persisted form deterministic and membership cheap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    sel: HashMap<Facet, BTreeSet<String>>,
}

impl Filters {
    /// The selected values for a facet (empty set when inactive).
    pub fn selected(&self, facet: Facet) -> Option<&BTreeSet<String>> {
        self.sel.get(&facet).filter(|s| !s.is_empty())
    }

    /// How many values are selected in a facet — shown on its menu row.
    pub fn count(&self, facet: Facet) -> usize {
        self.sel.get(&facet).map(BTreeSet::len).unwrap_or(0)
    }

    /// Number of facets with at least one value selected — drives the strip's
    /// active-filter badge.
    pub fn active_facets(&self) -> usize {
        Facet::ALL.iter().filter(|f| self.count(**f) > 0).count()
    }

    /// Toggle a value in a facet (add if absent, remove if present).
    pub fn toggle(&mut self, facet: Facet, value: &str) {
        let set = self.sel.entry(facet).or_default();
        if !set.remove(value) {
            set.insert(value.to_string());
        }
        if set.is_empty() {
            self.sel.remove(&facet);
        }
    }

    pub fn is_selected(&self, facet: Facet, value: &str) -> bool {
        self.sel.get(&facet).is_some_and(|s| s.contains(value))
    }

    pub fn clear_facet(&mut self, facet: Facet) {
        self.sel.remove(&facet);
    }

    pub fn clear_all(&mut self) {
        self.sel.clear();
    }
}

/// The values a book contributes to a facet. Author splits on ASCII *and* CJK
/// comma (`、`, U+3001) — Japanese OPFs pack multiple creators into one
/// `<dc:creator>` as `村上春樹、夏目漱石`. A missing value yields the single
/// [`NONE`] sentinel. Port of `extractFacetValues` (`library.js:514-537`).
pub fn extract_facet_values(book: &Book, facet: Facet) -> Vec<String> {
    match facet {
        Facet::Language => vec![non_empty_or_sentinel(&book.language)],
        Facet::Publisher => vec![non_empty_or_sentinel(book.publisher.as_deref().unwrap_or(""))],
        Facet::Series => vec![non_empty_or_sentinel(book.series_name.as_deref().unwrap_or(""))],
        Facet::Tags => {
            if book.tags.is_empty() {
                vec![NONE.to_string()]
            } else {
                book.tags.clone()
            }
        }
        Facet::Author => {
            let trimmed = book.author.trim();
            if trimmed.is_empty() {
                return vec![NONE.to_string()];
            }
            // Split on ASCII or ideographic comma; trim each part (the desktop
            // regex `\s*[,、]\s*` swallows surrounding whitespace), drop empties,
            // dedupe in-order.
            let mut seen = BTreeSet::new();
            let mut out = Vec::new();
            for part in trimmed.split([',', '、']) {
                let p = part.trim();
                if !p.is_empty() && seen.insert(p.to_string()) {
                    out.push(p.to_string());
                }
            }
            if out.is_empty() {
                vec![NONE.to_string()]
            } else {
                out
            }
        }
    }
}

fn non_empty_or_sentinel(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        NONE.to_string()
    } else {
        t.to_string()
    }
}

/// AND across active facets, OR within (`matchesFacets` + `activeFacetsExcept`,
/// `library.js:539-555`). `skip` excludes one facet from the test — used by
/// [`facet_options`] for the leave-one-out cascade; pass `None` for the real
/// visibility test.
pub fn matches(book: &Book, filters: &Filters, skip: Option<Facet>) -> bool {
    for facet in Facet::ALL {
        if Some(facet) == skip {
            continue;
        }
        let Some(sel) = filters.selected(facet) else {
            continue;
        };
        let vals = extract_facet_values(book, facet);
        if !vals.iter().any(|v| sel.contains(v)) {
            return false;
        }
    }
    true
}

/// Distinct values for a facet among the books matching **all other** active
/// facets (leave-one-out cascade), each with its count, sorted with [`NONE`]
/// last. Currently-selected values are always included even if the cross-facet
/// filter would exclude them, so they stay un-selectable-back. Port of
/// `facetOptions` (`library.js:578-600`).
pub fn facet_options(books: &[Book], filters: &Filters, facet: Facet) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for b in books {
        if !matches(b, filters, Some(facet)) {
            continue;
        }
        for v in extract_facet_values(b, facet) {
            *counts.entry(v).or_insert(0) += 1;
        }
    }
    if let Some(sel) = filters.sel.get(&facet) {
        for v in sel {
            counts.entry(v.clone()).or_insert(0);
        }
    }
    let mut out: Vec<(String, usize)> = counts.into_iter().collect();
    out.sort_by(|a, b| match (a.0.as_str(), b.0.as_str()) {
        (NONE, NONE) => Ordering::Equal,
        (NONE, _) => Ordering::Greater,
        (_, NONE) => Ordering::Less,
        _ => a.0.cmp(&b.0),
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn author_splits_on_ascii_and_cjk_comma() {
        let b = Book { author: "村上春樹、夏目漱石".into(), ..book(1) };
        assert_eq!(extract_facet_values(&b, Facet::Author), vec!["村上春樹", "夏目漱石"]);
        let b2 = Book { author: "Strunk, White".into(), ..book(2) };
        assert_eq!(extract_facet_values(&b2, Facet::Author), vec!["Strunk", "White"]);
    }

    #[test]
    fn missing_values_become_sentinel() {
        let b = book(1); // empty author/language, None publisher/series, no tags
        assert_eq!(extract_facet_values(&b, Facet::Author), vec![NONE]);
        assert_eq!(extract_facet_values(&b, Facet::Language), vec![NONE]);
        assert_eq!(extract_facet_values(&b, Facet::Publisher), vec![NONE]);
        assert_eq!(extract_facet_values(&b, Facet::Series), vec![NONE]);
        assert_eq!(extract_facet_values(&b, Facet::Tags), vec![NONE]);
    }

    #[test]
    fn and_across_or_within() {
        let books = vec![
            Book { language: "jp".into(), author: "A".into(), ..book(1) },
            Book { language: "jp".into(), author: "B".into(), ..book(2) },
            Book { language: "en".into(), author: "A".into(), ..book(3) },
        ];
        let mut f = Filters::default();
        f.toggle(Facet::Language, "jp");
        // OR within author: A or B; AND with language=jp.
        f.toggle(Facet::Author, "A");
        f.toggle(Facet::Author, "B");
        let pass: Vec<i64> = books.iter().filter(|b| matches(b, &f, None)).map(|b| b.id).collect();
        assert_eq!(pass, vec![1, 2]); // both jp; id 3 is en, excluded
    }

    #[test]
    fn options_cascade_leave_one_out() {
        let books = vec![
            Book { language: "jp".into(), author: "A".into(), ..book(1) },
            Book { language: "jp".into(), author: "B".into(), ..book(2) },
            Book { language: "en".into(), author: "C".into(), ..book(3) },
        ];
        let mut f = Filters::default();
        f.toggle(Facet::Language, "jp");
        // Author options are computed against language=jp → only A, B (not C).
        let authors: Vec<String> =
            facet_options(&books, &f, Facet::Author).into_iter().map(|(v, _)| v).collect();
        assert_eq!(authors, vec!["A", "B"]);
        // But Language options ignore the language selection (leave-one-out) →
        // still both languages, with counts.
        let langs = facet_options(&books, &f, Facet::Language);
        assert_eq!(langs, vec![("en".to_string(), 1), ("jp".to_string(), 2)]);
    }

    #[test]
    fn selected_value_survives_cross_filter_and_sentinel_sorts_last() {
        let books = vec![
            Book { language: "jp".into(), publisher: Some("Z".into()), ..book(1) },
            Book { language: "en".into(), publisher: None, ..book(2) },
        ];
        let mut f = Filters::default();
        // Select publisher=Z, then language=en (which excludes Z's only book).
        f.toggle(Facet::Publisher, "Z");
        f.toggle(Facet::Language, "en");
        let opts = facet_options(&books, &f, Facet::Publisher);
        // Z must still appear (count 0) so it can be unselected; "—" (id 2's
        // missing publisher, matching language=en) sorts after it.
        assert_eq!(opts, vec![("Z".to_string(), 0), (NONE.to_string(), 1)]);
    }

    #[test]
    fn toggle_round_trips_and_tracks_active() {
        let mut f = Filters::default();
        assert_eq!(f.active_facets(), 0);
        f.toggle(Facet::Tags, "scifi");
        assert!(f.is_selected(Facet::Tags, "scifi"));
        assert_eq!(f.active_facets(), 1);
        f.toggle(Facet::Tags, "scifi"); // remove → facet empties → inactive
        assert!(!f.is_selected(Facet::Tags, "scifi"));
        assert_eq!(f.active_facets(), 0);
        assert!(f.selected(Facet::Tags).is_none());
    }
}
