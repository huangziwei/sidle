//! Break decisions taken by hand, for the words a stock pattern set breaks
//! badly.
//!
//! A pattern set is generated from a word list by machine and is judged on
//! aggregate: it may break a rare word in an odd place and still be a good set.
//! A reader meets the commonest words on every page, though, so a poor break
//! in one of those is seen constantly, and no adjustment of the pattern
//! machinery reaches it — the decision is per word.
//!
//! # Layout
//!
//! One entry per line, plus one keyword:
//!
//! | line | meaning |
//! |---|---|
//! | `MINWORDLENGTH n` | words shorter than this are never broken |
//! | `word` | never broken |
//! | `wo-rd` | broken only where the marks are |
//! | `%…` | a comment |
//!
//! An entry replaces the patterns for the word it names, and stands whatever
//! the word's length, being itself the exception to the line above. The
//! dictionary's own limits on how near an edge a break may fall still apply,
//! as they do to every break. Entries are matched without regard to case, and
//! a word that already carries a mark cannot be named by one, the marks being
//! the breaks; a word inside such a compound can.

use std::collections::HashMap;

/// The words a dictionary breaks by hand rather than by pattern.
#[derive(Debug, Clone, Default)]
pub(super) struct Curation {
    /// Words shorter than this are not broken at all.
    pub min_word_length: usize,
    /// Each word, lowercased, against the byte offsets it breaks at.
    words: HashMap<String, Vec<usize>>,
}

impl Curation {
    /// Read a curation file.
    pub fn parse(text: &str) -> Curation {
        let mut curation = Curation::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('%') {
                continue;
            }
            if let Some(value) = line.strip_prefix("MINWORDLENGTH") {
                curation.min_word_length = value.trim().parse().unwrap_or_default();
                continue;
            }
            let mut word = String::with_capacity(line.len());
            let mut breaks = Vec::new();
            for part in line.split('-') {
                if !word.is_empty() {
                    breaks.push(word.len());
                }
                word.push_str(part);
            }
            curation.words.insert(word.to_lowercase(), breaks);
        }
        curation
    }

    /// The breaks stated for a word, if it is one of them.
    pub fn breaks(&self, word: &str) -> Option<&[usize]> {
        if self.words.is_empty() {
            return None;
        }
        self.words
            .get(word)
            .or_else(|| self.words.get(&word.to_lowercase()))
            .map(Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unmarked_word_is_never_broken() {
        let c = Curation::parse("after\n");
        assert_eq!(c.breaks("after"), Some(&[][..]));
    }

    #[test]
    fn a_marked_word_breaks_where_the_marks_are() {
        let c = Curation::parse("under-stand-ing\n");
        assert_eq!(c.breaks("understanding"), Some(&[5, 10][..]));
    }

    #[test]
    fn case_is_not_part_of_the_match() {
        let c = Curation::parse("every-thing\n");
        assert_eq!(c.breaks("Everything"), Some(&[5][..]));
        assert_eq!(c.breaks("anything"), None);
    }

    #[test]
    fn comments_and_the_length_keyword_are_not_words() {
        let c = Curation::parse("% words we break by hand\nMINWORDLENGTH 6\nafter\n");
        assert_eq!(c.min_word_length, 6);
        assert_eq!(c.breaks("MINWORDLENGTH"), None);
        assert_eq!(c.breaks("words"), None);
    }
}
