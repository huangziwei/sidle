//! Finding a string in a chapter's text.
//!
//! [`Chapter::find`] runs over the one text buffer a chapter keeps and reports
//! byte ranges into it; [`Chapter::node_at`] turns a range back into a node.

use super::{Chapter, NodeId, Role};

/// Where a needle was found: a byte range into the chapter's text buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// Byte offset of the match in the chapter's text buffer.
    pub at: usize,
    /// How many bytes of the buffer the match covers.
    pub len: usize,
}

impl Match {
    /// One past the last byte matched.
    pub fn end(&self) -> usize {
        self.at + self.len
    }
}

/// `text` in lower case, with the source byte offset of each of its bytes
/// and the source length as a last entry.
fn folded(text: &str) -> (String, Vec<usize>) {
    let mut lower = String::with_capacity(text.len());
    let mut from = Vec::with_capacity(text.len() + 1);
    for (at, character) in text.char_indices() {
        for part in character.to_lowercase() {
            lower.push(part);
            from.resize(lower.len(), at);
        }
    }
    from.push(text.len());
    (lower, from)
}

impl Chapter {
    /// Every place `needle` occurs in the chapter's text, ignoring case.
    /// Matches never overlap: each search resumes past the one before.
    pub fn find(&self, needle: &str) -> Vec<Match> {
        if needle.is_empty() {
            return Vec::new();
        }
        let (haystack, from) = folded(self.text_buffer());
        let (needle, _) = folded(needle);
        haystack
            .match_indices(&needle)
            .map(|(at, hit)| Match {
                at: from[at],
                len: from[at + hit.len()] - from[at],
            })
            .collect()
    }

    /// The text node whose own range covers `at`.
    pub fn node_at(&self, at: usize) -> Option<NodeId> {
        self.iter_dfs().find(|id| {
            self.node(*id).is_some_and(|node| {
                node.role == Role::Text
                    && (node.text.start as usize..node.text.end() as usize).contains(&at)
            })
        })
    }

    /// The match with up to `before` characters of the text ahead of it and
    /// `after` behind, cut at character boundaries and at the line it sits on.
    pub fn around(&self, found: Match, before: usize, after: usize) -> (&str, &str, &str) {
        let text = self.text_buffer();
        let opens = text[..found.at].rfind('\n').map_or(0, |at| at + 1);
        let closes = text[found.end()..]
            .find('\n')
            .map_or(text.len(), |at| found.end() + at);
        let start = text[opens..found.at]
            .char_indices()
            .rev()
            .take(before)
            .last()
            .map_or(found.at, |(at, _)| opens + at);
        let end = text[found.end()..closes]
            .char_indices()
            .take(after)
            .last()
            .map_or(found.end(), |(at, character)| {
                found.end() + at + character.len_utf8()
            });
        (
            &text[start..found.at],
            &text[found.at..found.end()],
            &text[found.end()..end],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, TextRange};

    fn chapter_of(text: &str) -> Chapter {
        let mut chapter = Chapter::new();
        let range = chapter.append_text(text);
        let node = chapter.alloc_node(Node::text(range));
        let root = chapter.root();
        chapter.append_child(root, node);
        chapter
    }

    #[test]
    fn a_search_ignores_case_and_reports_source_offsets() {
        let chapter = chapter_of("The Cat sat. the cat ran.");
        let found = chapter.find("CAT");
        assert_eq!(found.len(), 2);
        assert_eq!(&chapter.text_buffer()[found[0].at..found[0].end()], "Cat");
        assert_eq!(&chapter.text_buffer()[found[1].at..found[1].end()], "cat");
    }

    #[test]
    fn a_match_carries_the_text_either_side_of_it() {
        let chapter = chapter_of("one two three four five");
        let found = chapter.find("three")[0];
        let (before, hit, after) = chapter.around(found, 4, 4);
        assert_eq!(before, "two ");
        assert_eq!(hit, "three");
        assert_eq!(after, " fou");
    }

    #[test]
    fn a_snippet_stops_at_the_line_the_match_sits_on() {
        let chapter = chapter_of("first line\nsecond line\nthird line");
        let found = chapter.find("second")[0];
        let (before, _, after) = chapter.around(found, 40, 40);
        assert_eq!(before, "");
        assert_eq!(after, " line");
    }

    #[test]
    fn a_match_names_the_text_node_it_falls_in() {
        let chapter = chapter_of("hello world");
        let found = chapter.find("world")[0];
        let node = chapter.node_at(found.at).expect("a node holds the match");
        assert_eq!(
            chapter.node(node).map(|node| node.text),
            Some(TextRange::new(0, 11))
        );
    }

    #[test]
    fn an_empty_needle_finds_nothing() {
        assert!(chapter_of("anything").find("").is_empty());
    }
}
