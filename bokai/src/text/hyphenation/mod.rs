//! Where a word may be broken at the end of a line.

mod automaton;
mod compiled;
mod curation;
mod patterns;

use std::collections::BTreeSet;
use std::sync::OnceLock;

use automaton::Level;
use curation::Curation;

/// The character marking a permitted break inside a word.
pub const SOFT_HYPHEN: char = '\u{00ad}';

/// A pattern set bokai ships, and the hand decisions that go with it.
struct Bundled {
    /// BCP-47 primary subtags this set is for.
    languages: &'static [&'static str],
    patterns: &'static str,
    /// Empty for a set that ships exactly as its author wrote it.
    curation: &'static str,
    loaded: OnceLock<Hyphenator>,
}

/// The pattern sets bokai ships. English is the American set, which serves
/// every English tag; German is the reformed (1996) orthography.
static BUNDLED: [Bundled; 2] = [
    Bundled {
        languages: &["en"],
        patterns: include_str!("dic/hyph_en_US.dic"),
        curation: include_str!("dic/en.curation"),
        loaded: OnceLock::new(),
    },
    Bundled {
        languages: &["de"],
        patterns: include_str!("dic/hyph_de_1996.dic"),
        curation: "",
        loaded: OnceLock::new(),
    },
];

/// The hyphenator for a language, or `None` where nothing bundled covers it.
pub fn for_language(language: &str) -> Option<&'static Hyphenator> {
    let primary = language.split(['-', '_']).next()?.to_ascii_lowercase();
    let bundled = BUNDLED
        .iter()
        .find(|b| b.languages.contains(&primary.as_str()))?;
    Some(bundled.loaded.get_or_init(|| {
        // A bundled dictionary is part of the crate, so it parses or the build
        // is broken; `every_bundled_dictionary_loads` holds that.
        Hyphenator::from_patterns(bundled.patterns)
            .expect("bundled hyphenation dictionary")
            .curated(bundled.curation)
    }))
}

/// Why a dictionary is not usable.
#[derive(Debug)]
pub enum HyphenationError {
    /// The bytes end before a declared structure does.
    Truncated,
    /// The file declares a format version this reader does not implement.
    UnsupportedVersion(u16),
    /// The dictionary is not UTF-8, so its patterns cannot be matched against
    /// UTF-8 text.
    UnsupportedCharset(String),
    /// A level declares no states, so it can match nothing.
    EmptyLevel,
    /// A pattern respells the word around the break, which this reader does
    /// not apply and must not silently drop.
    UnsupportedReplacement(String),
}

impl std::fmt::Display for HyphenationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HyphenationError::Truncated => write!(f, "hyphenation dictionary is truncated"),
            HyphenationError::UnsupportedVersion(v) => {
                write!(f, "unsupported hyphenation dictionary version {v}")
            }
            HyphenationError::UnsupportedCharset(c) => {
                write!(f, "hyphenation dictionary charset {c} is not UTF-8")
            }
            HyphenationError::EmptyLevel => write!(f, "hyphenation dictionary level has no states"),
            HyphenationError::UnsupportedReplacement(p) => {
                write!(f, "hyphenation pattern {p} respells the word")
            }
        }
    }
}

impl std::error::Error for HyphenationError {}

/// A hyphenation dictionary for one language.
#[derive(Debug, Clone)]
pub struct Hyphenator {
    levels: Vec<Level>,
    curation: Curation,
}

impl Hyphenator {
    /// Read a pattern set in the text form.
    pub fn from_patterns(text: &str) -> Result<Self, HyphenationError> {
        Ok(Hyphenator {
            levels: patterns::parse(text)?,
            curation: Curation::default(),
        })
    }

    /// Read a pattern set in the precompiled form.
    pub fn from_compiled(bytes: &[u8]) -> Result<Self, HyphenationError> {
        Ok(Hyphenator {
            levels: compiled::parse(bytes)?,
            curation: Curation::default(),
        })
    }

    /// The same dictionary with per-word decisions over it.
    fn curated(mut self, text: &str) -> Self {
        self.curation = Curation::parse(text);
        self
    }

    /// Least number of characters that must precede a break.
    pub fn left_min(&self) -> usize {
        self.levels[0].left_min
    }

    /// Least number of characters that must follow a break.
    pub fn right_min(&self) -> usize {
        self.levels[0].right_min
    }

    /// Shortest word the dictionary breaks at all, zero where it sets no such
    /// limit. A KFX book states its own as `min_hyphen_word_length`.
    pub fn min_word_length(&self) -> usize {
        self.curation.min_word_length
    }

    /// Set the shortest word to break, overriding what the dictionary carries.
    pub fn set_min_word_length(&mut self, characters: usize) {
        self.curation.min_word_length = characters;
    }

    /// Byte offsets in `word` at which it may be broken, ascending. Each is the
    /// index of the first byte of the part that would move to the next line.
    pub fn hyphenate(&self, word: &str) -> Vec<usize> {
        if word.is_empty() {
            return Vec::new();
        }
        // Patterns are written in lower case, so a capital matches nothing and
        // would leave the word to be broken by whatever its tail matches.
        if !word.chars().any(char::is_uppercase) {
            return self.matched_breaks(word);
        }
        let (lowered, origin) = lowercase_with_origins(word);
        self.matched_breaks(&lowered)
            .into_iter()
            .filter_map(|at| origin[at])
            .collect()
    }

    /// Byte offsets this dictionary permits a break at, for a word already in
    /// the case the patterns are written in.
    fn matched_breaks(&self, word: &str) -> Vec<usize> {
        let bytes = word.as_bytes();
        let top = &self.levels[0];
        // `values[i]` governs a break before byte `i`; an odd value permits one.
        let mut values = vec![0u8; bytes.len() + 1];
        top.apply(bytes, &mut values);

        let mut breaks: BTreeSet<usize> = BTreeSet::new();
        // Compound boundaries the first level found, and the segments between
        // them, which the deeper levels each run over on their own.
        let mut bounds: Vec<usize> = (1..bytes.len())
            .filter(|&i| values[i] % 2 == 1 && word.is_char_boundary(i))
            .collect();
        for &b in &bounds {
            breaks.insert(b);
        }
        bounds.insert(0, 0);
        bounds.push(bytes.len());

        for pair in bounds.windows(2) {
            let (from, to) = (pair[0], pair[1]);
            let segment = &bytes[from..to];
            if segment.is_empty() {
                continue;
            }
            // A word decided by hand is not put to the patterns at all, and
            // stands whatever its length.
            if let Some(stated) = self.curation.breaks(&word[from..to]) {
                breaks.extend(stated.iter().map(|&at| from + at));
                continue;
            }
            if word[from..to].chars().count() < self.curation.min_word_length {
                continue;
            }
            for level in &self.levels[1..] {
                let mut inner = vec![0u8; segment.len() + 1];
                level.apply(segment, &mut inner);
                // A part that starts or ends inside the word keeps its distance
                // from the boundary by the compound limits rather than the
                // plain ones.
                let head = if from == 0 {
                    top.left_min
                } else {
                    top.compound_left_min
                };
                let tail = if to == bytes.len() {
                    top.right_min
                } else {
                    top.compound_right_min
                };
                for (i, &v) in inner.iter().enumerate() {
                    if v % 2 == 0 || i == 0 || i == segment.len() {
                        continue;
                    }
                    let at = from + i;
                    if !word.is_char_boundary(at) {
                        continue;
                    }
                    if chars_between(word, from, at) < head || chars_between(word, at, to) < tail {
                        continue;
                    }
                    breaks.insert(at);
                }
            }
        }

        breaks
            .into_iter()
            .filter(|&at| {
                chars_between(word, 0, at) >= top.left_min
                    && chars_between(word, at, bytes.len()) >= top.right_min
                    && !self.suppressed(word, at)
            })
            .collect()
    }

    /// `word` with a soft hyphen at each permitted break.
    pub fn with_soft_hyphens(&self, word: &str) -> String {
        let breaks = self.hyphenate(word);
        if breaks.is_empty() {
            return word.to_string();
        }
        let mut out = String::with_capacity(word.len() + breaks.len() * 2);
        let mut last = 0;
        for at in breaks {
            out.push_str(&word[last..at]);
            out.push(SOFT_HYPHEN);
            last = at;
        }
        out.push_str(&word[last..]);
        out
    }

    /// Whether a no-hyphen sequence sits against a break, which forbids it.
    fn suppressed(&self, word: &str, at: usize) -> bool {
        self.levels.iter().any(|level| {
            level.no_hyphen.iter().any(|seq| {
                word.as_bytes()[at..].starts_with(seq)
                    || word.as_bytes()[..at].ends_with(seq.as_slice())
            })
        })
    }
}

/// Characters between two byte offsets of `s`.
fn chars_between(s: &str, from: usize, to: usize) -> usize {
    s[from..to].chars().count()
}

/// `word` in lower case, and where each byte offset of it sits in the
fn lowercase_with_origins(word: &str) -> (String, Vec<Option<usize>>) {
    let mut lowered = String::with_capacity(word.len());
    let mut origin = Vec::with_capacity(word.len() + 1);
    for (at, ch) in word.char_indices() {
        lowered.extend(ch.to_lowercase());
        origin.push(Some(at));
        origin.resize(lowered.len(), None);
    }
    origin.push(Some(word.len()));
    (lowered, origin)
}

/// Every bundled pattern set, against the first language it covers.
#[cfg(test)]
fn bundled_patterns() -> impl Iterator<Item = (&'static str, &'static str)> {
    BUNDLED.iter().map(|b| (b.languages[0], b.patterns))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_dictionary_loads() {
        for (language, _) in bundled_patterns() {
            let h = for_language(language)
                .unwrap_or_else(|| panic!("{language} is bundled but does not resolve"));
            assert!(h.left_min() >= 1, "{language} left minimum");
            assert!(h.right_min() >= 1, "{language} right minimum");
        }
    }

    #[test]
    fn a_language_tag_finds_its_dictionary_by_the_primary_subtag() {
        assert!(for_language("en").is_some());
        assert!(for_language("EN-gb").is_some());
        assert!(for_language("de-AT").is_some());
        // Nothing bundled, rather than something close: Japanese and Chinese
        // do not hyphenate, and Dutch simply is not shipped.
        assert!(for_language("ja").is_none());
        assert!(for_language("zh-Hant").is_none());
        assert!(for_language("nl").is_none());
        assert!(for_language("").is_none());
    }

    #[test]
    fn english_breaks_words_where_the_stock_patterns_do() {
        let en = for_language("en").unwrap();
        assert_eq!(
            en.with_soft_hyphens("hyphenation"),
            "hy\u{ad}phen\u{ad}ation"
        );
        assert_eq!(en.with_soft_hyphens("algorithm"), "al\u{ad}go\u{ad}rithm");
        assert_eq!(
            en.with_soft_hyphens("typography"),
            "ty\u{ad}pog\u{ad}ra\u{ad}phy"
        );
    }

    #[test]
    fn german_breaks_words_where_the_stock_patterns_do() {
        let de = for_language("de").unwrap();
        assert_eq!(
            de.with_soft_hyphens("Silbentrennung"),
            "Sil\u{ad}ben\u{ad}tren\u{ad}nung"
        );
        assert_eq!(de.with_soft_hyphens("Bibliothek"), "Bi\u{ad}blio\u{ad}thek");
    }

    #[test]
    fn a_word_that_already_breaks_takes_no_mark_against_the_break() {
        let en = for_language("en").unwrap();
        // Each part hyphenates on its own, and nothing is offered against the
        // hyphen or the apostrophe, which are printed already.
        assert_eq!(en.with_soft_hyphens("well-thumbed"), "well-thumbed");
        assert_eq!(en.with_soft_hyphens("cold-blooded"), "cold-blooded");
        assert_eq!(en.with_soft_hyphens("o'clock"), "o'clock");
    }

    #[test]
    fn a_short_word_is_left_whole() {
        let en = for_language("en").unwrap();
        assert_eq!(en.min_word_length(), 6);
        // Five letters and fewer, whatever the patterns would say.
        for word in ["table", "under", "after", "going", "water"] {
            assert!(en.hyphenate(word).is_empty(), "{word}");
        }
        // The limit is on the part, so a compound divides by its own parts.
        assert_eq!(en.with_soft_hyphens("water-table"), "water-table");
    }

    #[test]
    fn a_word_decided_by_hand_divides_as_decided() {
        let en = for_language("en").unwrap();
        // Divided at the seam the patterns cut through, rather than inside it.
        assert_eq!(en.with_soft_hyphens("everything"), "every\u{ad}thing");
        assert_eq!(
            en.with_soft_hyphens("understanding"),
            "under\u{ad}stand\u{ad}ing"
        );
        // No division at all, the patterns offering only a meaningless one.
        assert_eq!(en.with_soft_hyphens("father"), "father");
        assert_eq!(en.with_soft_hyphens("really"), "really");
        // A decision reaches a word inside a compound, and case is not part of
        // the match.
        assert_eq!(
            en.with_soft_hyphens("self-understanding"),
            "self-under\u{ad}stand\u{ad}ing"
        );
        assert_eq!(en.with_soft_hyphens("October"), "Octo\u{ad}ber");
    }

    #[test]
    fn a_prefix_still_divides() {
        let en = for_language("en").unwrap();
        // The decisions take nothing from the divisions that read well.
        assert_eq!(en.with_soft_hyphens("return"), "re\u{ad}turn");
        assert_eq!(en.with_soft_hyphens("explain"), "ex\u{ad}plain");
        assert_eq!(en.with_soft_hyphens("myself"), "my\u{ad}self");
        assert_eq!(en.with_soft_hyphens("appear"), "ap\u{ad}pear");
    }

    #[test]
    fn a_book_may_state_its_own_minimum_length() {
        let mut en = for_language("en").unwrap().clone();
        en.set_min_word_length(0);
        assert_eq!(en.with_soft_hyphens("table"), "ta\u{ad}ble");
        en.set_min_word_length(20);
        assert_eq!(en.with_soft_hyphens("hyphenation"), "hyphenation");
    }
}
