//! On-device search: canonicalize the typed query the same way the server
//! canonicalized each book's [`Book::search_key`], then substring-match.

use crate::api::Book;

/// Canonical match form: lowercase, keep only `[a-z0-9]`, drop everything else
pub fn canon(s: &str) -> String {
    s.chars()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// Does `book` match the already-[`canon`]'d `query`? An empty query matches
/// everything (no search active).
pub fn matches(book: &Book, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if !book.search_key.is_empty() {
        return book.search_key.contains(query);
    }
    let mut fallback = canon(&book.title);
    fallback.push_str(&canon(&book.author));
    fallback.contains(query)
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
        assert!(matches(&b, &canon("murakami")));
        assert!(matches(&b, &canon("sekai")));
        assert!(matches(&b, &canon("murakamiharuki")));
        assert!(!matches(&b, &canon("agatha")));
    }

    #[test]
    fn empty_query_matches_all() {
        let b = book("anything", "T", "A");
        assert!(matches(&b, ""));
    }

    #[test]
    fn falls_back_to_raw_when_key_empty() {
        // Old server: no search_key → Latin substring of title/author still works.
        let b = book("", "The Roman Hat Mystery", "Ellery Queen");
        assert!(matches(&b, &canon("romanhat")));
        assert!(matches(&b, &canon("queen")));
        // CJK can't match in the fallback (no romaji on-device) — documented limit.
        let jp = book("", "世界", "村上");
        assert!(!matches(&jp, &canon("sekai")));
    }
}
