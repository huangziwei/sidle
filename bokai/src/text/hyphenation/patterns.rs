//! The text dictionary form: a Liang pattern set as written, which is what
//! `libhyphen` distributes as `hyph_<language>.dic` and what TeX distributes as
//! a bare pattern list.

use std::collections::VecDeque;

use super::HyphenationError;
use super::automaton::{Level, State};

/// The level that finds the breaks a word already carries, for a dictionary that
/// declares patterns alone. `NOHYPHEN` keeps a soft hyphen off a visible mark.
const COMPOUND_LEVEL: &str = "\
NOHYPHEN ',–,’,-
1-1
1'1
1–1
1’1
";

/// Characters a break must leave behind it where a file names no limit, which
/// is the convention a bare TeX pattern list is written to.
const DEFAULT_MIN: usize = 2;

/// Characters a break must leave against a boundary inside a compound where a
/// file names neither that limit nor the plain one.
const DEFAULT_COMPOUND_MIN: usize = 3;

/// Read a text dictionary into its levels.
pub(super) fn parse(text: &str) -> Result<Vec<Level>, HyphenationError> {
    let mut lines = text.lines();
    let charset = lines.next().unwrap_or_default().trim();
    if !charset.eq_ignore_ascii_case("UTF-8") {
        return Err(HyphenationError::UnsupportedCharset(charset.to_string()));
    }

    let mut sections = vec![Section::default()];
    for line in lines {
        let line = line.trim();
        if line.is_empty() || line.starts_with('%') {
            continue;
        }
        if line == "NEXTLEVEL" {
            sections.push(Section::default());
            continue;
        }
        sections.last_mut().unwrap().read(line)?;
    }

    // Patterns alone are the language's own level, under a generated one.
    if sections.len() == 1 {
        let mut compound = Section::default();
        for line in COMPOUND_LEVEL.lines() {
            compound.read(line)?;
        }
        let patterns = sections.pop().unwrap();
        compound.left_min = patterns.left_min;
        compound.right_min = patterns.right_min;
        compound.compound_left_min = patterns.compound_left_min;
        compound.compound_right_min = patterns.compound_right_min;
        sections = vec![compound, patterns];
    }

    let levels: Vec<Level> = sections.into_iter().map(Section::build).collect();
    if levels.iter().any(|l| l.states.is_empty()) {
        return Err(HyphenationError::EmptyLevel);
    }
    Ok(levels)
}

/// One level of a dictionary as the file states it.
#[derive(Default)]
struct Section {
    left_min: Option<usize>,
    right_min: Option<usize>,
    compound_left_min: Option<usize>,
    compound_right_min: Option<usize>,
    no_hyphen: Vec<Vec<u8>>,
    /// Each pattern's letters and its digit string, the digits already
    /// stripped of leading zeros.
    patterns: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Section {
    /// Take one line of the file.
    fn read(&mut self, line: &str) -> Result<(), HyphenationError> {
        for (keyword, field) in [
            ("LEFTHYPHENMIN", 0),
            ("RIGHTHYPHENMIN", 1),
            ("COMPOUNDLEFTHYPHENMIN", 2),
            ("COMPOUNDRIGHTHYPHENMIN", 3),
        ] {
            if let Some(value) = line.strip_prefix(keyword) {
                let n = value.trim().parse().ok();
                match field {
                    0 => self.left_min = n,
                    1 => self.right_min = n,
                    2 => self.compound_left_min = n,
                    _ => self.compound_right_min = n,
                }
                return Ok(());
            }
        }
        if let Some(value) = line.strip_prefix("NOHYPHEN") {
            self.no_hyphen = value
                .trim()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.as_bytes().to_vec())
                .collect();
            return Ok(());
        }
        // A pattern that spells a word differently on either side of the break
        // is beyond what this reader applies, and dropping it would silently
        // hyphenate the word wrongly.
        if line.contains('/') {
            return Err(HyphenationError::UnsupportedReplacement(line.to_string()));
        }
        let mut letters: Vec<u8> = Vec::new();
        let mut digits: Vec<u8> = vec![b'0'];
        for &b in line.as_bytes() {
            if b.is_ascii_digit() {
                *digits.last_mut().unwrap() = b;
            } else {
                letters.push(b);
                digits.push(b'0');
            }
        }
        let start = digits.iter().take_while(|&&d| d == b'0').count();
        if start < digits.len() {
            self.patterns.push((letters, digits[start..].to_vec()));
        }
        Ok(())
    }

    /// Compile the section's patterns into a level.
    fn build(self) -> Level {
        let mut trie = Trie::default();
        for (letters, digits) in &self.patterns {
            trie.insert(letters, digits);
        }
        trie.link_and_merge();

        let left_min = self.left_min.unwrap_or(DEFAULT_MIN);
        let right_min = self.right_min.unwrap_or(DEFAULT_MIN);
        let mut level = Level {
            left_min,
            right_min,
            compound_left_min: self
                .compound_left_min
                .or(self.left_min)
                .unwrap_or(DEFAULT_COMPOUND_MIN),
            compound_right_min: self
                .compound_right_min
                .or(self.right_min)
                .unwrap_or(DEFAULT_COMPOUND_MIN),
            states: Vec::with_capacity(trie.nodes.len()),
            transitions: Vec::new(),
            pool: vec![0],
            no_hyphen: self.no_hyphen,
        };
        for node in &trie.nodes {
            let digits = (!node.digits.is_empty()).then(|| {
                let at = level.pool.len() as u32;
                level.pool.extend_from_slice(&node.digits);
                level.pool.push(0);
                at
            });
            let trans_start = level.transitions.len() as u32;
            level
                .transitions
                .extend(node.transitions.iter().map(|&(byte, to)| (to, byte)));
            level.states.push(State {
                digits,
                fallback: node.fallback,
                trans_start,
                trans_len: node.transitions.len() as u32,
            });
        }
        level
    }
}

/// A trie of pattern letters under construction.
#[derive(Default)]
struct Trie {
    nodes: Vec<Node>,
}

#[derive(Default)]
struct Node {
    /// Matched byte and the state it leads to.
    transitions: Vec<(u8, u32)>,
    /// ASCII digits, leading zeros already stripped.
    digits: Vec<u8>,
    fallback: Option<u32>,
}

impl Trie {
    /// Place one pattern, replacing any digits already held for its letters.
    fn insert(&mut self, letters: &[u8], digits: &[u8]) {
        if self.nodes.is_empty() {
            self.nodes.push(Node::default());
        }
        let mut at = 0u32;
        for &byte in letters {
            at = match self.nodes[at as usize]
                .transitions
                .iter()
                .find(|(b, _)| *b == byte)
            {
                Some(&(_, next)) => next,
                None => {
                    let next = self.nodes.len() as u32;
                    self.nodes.push(Node::default());
                    self.nodes[at as usize].transitions.push((byte, next));
                    next
                }
            };
        }
        self.nodes[at as usize].digits = digits.to_vec();
    }

    /// Point each state at its longest proper suffix and absorb that suffix's
    /// digits, so a state states every pattern that ends where it does.
    fn link_and_merge(&mut self) {
        if self.nodes.is_empty() {
            self.nodes.push(Node::default());
            return;
        }
        for node in &mut self.nodes {
            node.transitions.sort_unstable();
        }
        let mut queue: VecDeque<u32> = VecDeque::new();
        for i in 0..self.nodes[0].transitions.len() {
            let (_, child) = self.nodes[0].transitions[i];
            self.nodes[child as usize].fallback = Some(0);
            queue.push_back(child);
        }
        while let Some(at) = queue.pop_front() {
            let fallback = self.nodes[at as usize].fallback.unwrap_or(0);
            let digits = merge(
                &self.nodes[at as usize].digits,
                &self.nodes[fallback as usize].digits,
            );
            self.nodes[at as usize].digits = digits;
            for i in 0..self.nodes[at as usize].transitions.len() {
                let (byte, child) = self.nodes[at as usize].transitions[i];
                self.nodes[child as usize].fallback = Some(self.step(fallback, byte));
                queue.push_back(child);
            }
        }
    }

    /// The state reached from `at` on `byte`, following fallbacks. Every
    /// fallback it consults belongs to a shorter pattern, so it is already set.
    fn step(&self, at: u32, byte: u8) -> u32 {
        let mut at = at;
        loop {
            if let Some(&(_, next)) = self.nodes[at as usize]
                .transitions
                .iter()
                .find(|(b, _)| *b == byte)
            {
                return next;
            }
            match self.nodes[at as usize].fallback {
                Some(f) => at = f,
                None => return 0,
            }
        }
    }
}

/// Two digit strings that end in the same place, merged digit by digit from
/// the right, keeping the larger of each pair.
fn merge(a: &[u8], b: &[u8]) -> Vec<u8> {
    if a.is_empty() {
        return b.to_vec();
    }
    if b.is_empty() {
        return a.to_vec();
    }
    let len = a.len().max(b.len());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let from = |s: &[u8]| match (i + s.len()).checked_sub(len) {
            Some(k) => s[k],
            None => b'0',
        };
        out.push(from(a).max(from(b)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::Hyphenator;
    use super::*;
    use std::collections::HashMap;

    /// A dictionary of the given lines, with the charset line already on it.
    fn dictionary(body: &str) -> Hyphenator {
        Hyphenator::from_patterns(&format!("UTF-8\n{body}")).unwrap()
    }

    #[test]
    fn breaks_where_a_pattern_says() {
        let h = dictionary("LEFTHYPHENMIN 1\nRIGHTHYPHENMIN 1\na1b\n");
        assert_eq!(h.hyphenate("xaby"), vec![2]);
        assert_eq!(h.with_soft_hyphens("xaby"), "xa\u{ad}by");
        assert_eq!(h.hyphenate("xyz"), Vec::<usize>::new());
    }

    #[test]
    fn an_even_digit_outranks_an_odd_one() {
        // Both patterns land on the same place; the higher value decides, and
        // an even value is a refusal.
        let h = dictionary("LEFTHYPHENMIN 1\nRIGHTHYPHENMIN 1\na1b\nxa2by\n");
        assert_eq!(h.hyphenate("xaby"), Vec::<usize>::new());
    }

    #[test]
    fn a_shorter_pattern_still_matches_under_a_longer_one() {
        // Walking "abc" ends in a state built for `a2bc`; the break comes from
        // `b1c`, which ends in the same place and is only reachable through
        // the fallback chain.
        let h = dictionary("LEFTHYPHENMIN 1\nRIGHTHYPHENMIN 1\na2bc\nb1c\n");
        assert_eq!(h.hyphenate("abcd"), vec![2]);
    }

    #[test]
    fn limits_keep_breaks_away_from_the_edges() {
        let h = dictionary("LEFTHYPHENMIN 2\nRIGHTHYPHENMIN 3\na1b\n");
        assert_eq!(h.hyphenate("xab"), Vec::<usize>::new());
        assert_eq!(h.hyphenate("xabyz"), vec![2]);
    }

    #[test]
    fn a_generated_compound_level_breaks_a_marked_word_into_parts() {
        let h = dictionary("LEFTHYPHENMIN 1\nRIGHTHYPHENMIN 1\na1b\n");
        assert_eq!(h.hyphenate("ab-ab"), vec![1, 4]);
        // The mark is a break already; no soft hyphen is offered against it.
        assert_eq!(h.with_soft_hyphens("ab-ab"), "a\u{ad}b-a\u{ad}b");
        assert_eq!(h.with_soft_hyphens("ab’ab"), "a\u{ad}b’a\u{ad}b");
    }

    #[test]
    fn a_declared_compound_level_is_taken_as_written() {
        let h = dictionary("LEFTHYPHENMIN 1\nRIGHTHYPHENMIN 1\n1=1\nNEXTLEVEL\na1b\n");
        // `=` is the only mark this file calls a compound boundary, and it
        // declares no `NOHYPHEN`, so a break is offered on both sides of it.
        assert_eq!(h.hyphenate("ab=ab"), vec![1, 2, 3, 4]);
        // A hyphen divides nothing here, so no break is offered against it and
        // the word is matched whole.
        assert_eq!(h.hyphenate("ab-ab"), vec![1, 4]);
    }

    #[test]
    fn multibyte_letters_break_only_on_character_boundaries() {
        let h = dictionary("LEFTHYPHENMIN 1\nRIGHTHYPHENMIN 1\né1è\n");
        let word = "xéèy";
        assert_eq!(h.hyphenate(word), vec![3]);
        assert_eq!(h.with_soft_hyphens(word), "xé\u{ad}èy");
    }

    #[test]
    fn rejects_a_charset_it_cannot_match_against_text() {
        assert!(matches!(
            Hyphenator::from_patterns("ISO8859-1\na1b\n"),
            Err(HyphenationError::UnsupportedCharset(c)) if c == "ISO8859-1"
        ));
    }

    #[test]
    fn rejects_a_pattern_that_respells_the_word() {
        assert!(matches!(
            Hyphenator::from_patterns("UTF-8\nf1f/ff=f,1,2\n"),
            Err(HyphenationError::UnsupportedReplacement(_))
        ));
    }

    /// Every substring of the framed word looked up in a plain map of the
    /// patterns — the definition of Liang matching, sharing nothing with the
    /// automaton but the file it reads.
    struct BruteForce {
        map: HashMap<Vec<u8>, Vec<u8>>,
        longest: usize,
        left_min: usize,
        right_min: usize,
    }

    impl BruteForce {
        fn parse(text: &str) -> BruteForce {
            let mut map = HashMap::new();
            let (mut left_min, mut right_min, mut longest) = (2usize, 2usize, 0usize);
            for line in text.lines().skip(1) {
                let line = line.trim();
                if line.is_empty() || line.starts_with('%') {
                    continue;
                }
                if let Some(v) = line.strip_prefix("LEFTHYPHENMIN") {
                    left_min = v.trim().parse().unwrap();
                    continue;
                }
                if let Some(v) = line.strip_prefix("RIGHTHYPHENMIN") {
                    right_min = v.trim().parse().unwrap();
                    continue;
                }
                if line.starts_with("COMPOUND") || line.starts_with("NOHYPHEN") {
                    continue;
                }
                let mut letters: Vec<u8> = Vec::new();
                let mut digits: Vec<u8> = vec![0];
                for &b in line.as_bytes() {
                    if b.is_ascii_digit() {
                        *digits.last_mut().unwrap() = b - b'0';
                    } else {
                        letters.push(b);
                        digits.push(0);
                    }
                }
                longest = longest.max(letters.len());
                map.insert(letters, digits);
            }
            BruteForce {
                map,
                longest,
                left_min,
                right_min,
            }
        }

        fn hyphenate(&self, word: &str) -> Vec<usize> {
            let mut framed = vec![b'.'];
            framed.extend_from_slice(word.as_bytes());
            framed.push(b'.');
            let mut values = vec![0u8; framed.len() + 2];
            for i in 0..framed.len() {
                for len in 1..=self.longest.min(framed.len() - i) {
                    if let Some(digits) = self.map.get(&framed[i..i + len]) {
                        for (j, &v) in digits.iter().enumerate() {
                            values[i + j] = values[i + j].max(v);
                        }
                    }
                }
            }
            let chars = word.chars().count();
            (1..word.len())
                .filter(|&at| word.is_char_boundary(at))
                .filter(|&at| values[at + 1] % 2 == 1)
                .filter(|&at| {
                    let before = word[..at].chars().count();
                    before >= self.left_min && chars - before >= self.right_min
                })
                .collect()
        }
    }

    /// Words chosen for the shapes that stress the walk: prefixes and suffixes
    /// of every length, doubled consonants, ligatures the pattern file spells
    /// as one character, and the marks the compound level acts on.
    const WORDS: &[&str] = &[
        "hyphenation",
        "algorithm",
        "typography",
        "understand",
        "everything",
        "difficult",
        "immediately",
        "philosophy",
        "recognize",
        "manuscript",
        "photograph",
        "bookkeeper",
        "unhappiness",
        "reorganize",
        "coordinate",
        "walking",
        "singing",
        "chapter",
        "brother",
        "children",
        "example",
        "problem",
        "present",
        "project",
        "record",
        "object",
        "subject",
        "another",
        "against",
        "because",
        "between",
        "through",
        "thought",
        "although",
        "straight",
        "strength",
        "abbreviation",
        "acknowledgement",
        "extraordinary",
        "responsibility",
        "characteristic",
        "international",
        "administration",
        "unbelievable",
        "grandmother",
        "afternoon",
        "somewhere",
        "yesterday",
        "beautiful",
        "dangerous",
        "wilderness",
        "government",
        "department",
        "university",
        "difference",
        "experience",
        "well-known",
        "self-evident",
        "mother-in-law",
        "o'clock",
        "don't",
        "shouldn't",
        "affinity",
        "filigree",
        "flyleaf",
        "viewfinder",
    ];

    #[test]
    fn the_automaton_agrees_with_plain_liang_matching() {
        for (language, text) in super::super::bundled_patterns() {
            let automaton = Hyphenator::from_patterns(text).unwrap();
            let brute = BruteForce::parse(text);
            for word in WORDS {
                // Only the automaton knows the compound level, so compare on
                // each part of a word that already breaks.
                for part in word.split(['-', '\'']) {
                    if part.len() < 2 {
                        continue;
                    }
                    assert_eq!(
                        automaton.hyphenate(part),
                        brute.hyphenate(part),
                        "{language}: {part}"
                    );
                }
            }
        }
    }
}
