//! On-device search. [`canon`] matches a query against [`Book::search_key`];
//! [`spelled`] matches it against [`Book::title`] and [`Book::author`] as
//! written.

use crate::api::Book;

/// Canonical match form: lowercase, keep only `[a-z0-9]`. A query holding none
/// answers empty, which [`matches`] tests for.
pub fn canon(s: &str) -> String {
    s.chars()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// `s` lowercased with its whitespace dropped.
pub fn spelled(s: &str) -> String {
    s.chars()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// Does `book` match the raw `query`? [`canon`] against [`Book::search_key`],
/// then [`spelled`] against title and author. An empty `query` matches every
/// book.
pub fn matches(book: &Book, query: &str) -> bool {
    let canon_q = canon(query);
    if !canon_q.is_empty() {
        let keyed = match book.search_key.is_empty() {
            false => book.search_key.contains(&canon_q),
            true => {
                let mut fallback = canon(&book.title);
                fallback.push_str(&canon(&book.author));
                fallback.contains(&canon_q)
            }
        };
        if keyed {
            return true;
        }
    }
    let spelled_q = spelled(query);
    if spelled_q.is_empty() {
        return canon_q.is_empty();
    }
    spelled(&book.title).contains(&spelled_q) || spelled(&book.author).contains(&spelled_q)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(search_key: &str, title: &str, author: &str) -> Book {
        Book {
            id: 1,
            title: title.into(),
            kfx_sha256: None,
            device_filename: None,
            author: author.into(),
            language: String::new(),
            publisher: None,
            series_name: None,
            series_index: None,
            kind: None,
            asin: None,
            file_size: 0,
            imported_at: String::new(),
            tags: Vec::new(),
            cover_rev: 0,
            kfx_rev: 0,
            search_key: search_key.into(),
        }
    }

    #[test]
    fn canon_strips_to_lower_alnum() {
        assert_eq!(canon("Murakami Haruki!"), "murakamiharuki");
        assert_eq!(canon("Vol. 2"), "vol2");
        assert_eq!(canon(""), "");
    }

    #[test]
    fn matches_against_search_key() {
        let b = book("sekainoowarimurakamiharuki", "世界の終り", "村上春樹");
        assert!(matches(&b, "murakami"));
        assert!(matches(&b, "sekai"));
        assert!(matches(&b, "murakamiharuki"));
        assert!(!matches(&b, "agatha"));
    }

    /// [`matches`] finds a book by its own script through [`spelled`].
    #[test]
    fn a_committed_run_matches_the_title_as_written() {
        let b = book("sekainoowarimurakamiharuki", "世界の終り", "村上春樹");
        assert!(matches(&b, "世界"));
        assert!(matches(&b, "終り"));
        assert!(matches(&b, "村上"));
        assert!(matches(&b, "村上春樹"));
        assert!(!matches(&b, "夏目"));
    }

    /// [`matches`] tries both forms over one book.
    #[test]
    fn a_book_is_found_by_romaji_or_by_script() {
        let b = book("sekainoowarimurakamiharuki", "世界の終り", "村上春樹");
        assert!(matches(&b, "murakami"), "romaji");
        assert!(matches(&b, "村上"), "as written");
    }

    /// [`matches`] reaches a book whose `search_key` is empty.
    #[test]
    fn a_book_without_a_key_matches_its_own_script() {
        let b = book("", "夢遊病者の手記", "安部公房");
        assert!(matches(&b, "夢遊"));
        assert!(matches(&b, "公房"));
    }

    /// [`spelled`] drops whitespace from both sides of the test.
    #[test]
    fn spacing_does_not_decide_a_match() {
        let b = book("", "The Roman Hat Mystery", "Ellery Queen");
        assert!(matches(&b, "roman hat"));
        assert_eq!(spelled("Roman  Hat"), "romanhat");
    }

    #[test]
    fn empty_query_matches_all() {
        let b = book("anything", "T", "A");
        assert!(matches(&b, ""));
    }

    #[test]
    fn falls_back_to_raw_when_key_empty() {
        // An empty `search_key` falls back to title and author.
        let b = book("", "The Roman Hat Mystery", "Ellery Queen");
        assert!(matches(&b, "romanhat"));
        assert!(matches(&b, "queen"));
        // A romaji query reaches no romaji key; the book answers to its own
        // script through [`spelled`].
        let jp = book("", "世界", "村上");
        assert!(!matches(&jp, "sekai"));
        assert!(matches(&jp, "世界"));
    }
}
