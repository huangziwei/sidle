//! Hyphenation driven by a compiled Liang-pattern dictionary.
//!
//! The dictionary is a pattern set in the `libhyphen` tradition, compiled ahead
//! of time into a byte-driven matching automaton instead of the usual text
//! `hyph_*.dic`. It is what a Kindle carries in its reader resource bundle, and
//! it is the form this module reads.
//!
//! # Layout
//!
//! All integers are little-endian. A file is a two-byte version, a two-byte
//! level count, and then that many levels laid end to end. Each level is a
//! 36-byte header followed by three arrays:
//!
//! | field | type | meaning |
//! |---|---|---|
//! | `+0x00` | `u32` | transition count |
//! | `+0x04` | `u32` | string-pool length in bytes |
//! | `+0x08` | `i16` | number of no-hyphen strings |
//! | `+0x0a` | `i16` | state count |
//! | `+0x0c` | `[u8; 20]` | character set name, NUL-padded |
//! | `+0x20` | `u8` | left hyphen minimum |
//! | `+0x21` | `u8` | right hyphen minimum |
//! | `+0x22` | `u8` | compound left hyphen minimum |
//! | `+0x23` | `u8` | compound right hyphen minimum in bits 0-6, UTF-8 flag in bit 7 |
//!
//! States are eight bytes each — a `u32` byte offset into the string pool for
//! the state's digit string (zero meaning none), an `i16` fallback state (`-1`
//! for none), an `i8` transition count and a `u8` pattern length. Transitions
//! are four bytes each — a `u16` destination state, the matched byte, and one
//! byte of padding that carries no meaning. Every state's transitions occupy a
//! contiguous run of the transition array, and the runs follow state order, so
//! a state's run begins after the runs of all lower-numbered states.
//!
//! The pool holds NUL-terminated strings: first the level's no-hyphen strings,
//! then the digit strings the states point at.
//!
//! # Levels
//!
//! The first level finds compound boundaries — the places an existing hyphen or
//! apostrophe already lets a word break. The second holds the language's real
//! pattern set and runs over each compound part on its own, which is why the
//! compound minimums exist alongside the plain ones.

use std::collections::BTreeSet;

/// The character marking a permitted break inside a word.
pub const SOFT_HYPHEN: char = '\u{00ad}';

/// Why a byte string is not a usable hyphenation dictionary.
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
        }
    }
}

impl std::error::Error for HyphenationError {}

/// The only format version this reader implements.
const FORMAT_VERSION: u16 = 2;
/// Bytes of a level header, before its state, transition and pool arrays.
const LEVEL_HEADER_LEN: usize = 36;
/// Bytes per state record.
const STATE_LEN: usize = 8;
/// Bytes per transition record.
const TRANS_LEN: usize = 4;

/// One state of a level's matching automaton.
#[derive(Debug, Clone, Copy)]
struct State {
    /// Byte offset of this state's digit string in the pool, or `None`.
    digits: Option<u32>,
    /// State to retry the current byte in, or `None` to restart from the root.
    fallback: Option<u16>,
    /// Where this state's transitions begin in the level's transition array.
    trans_start: u32,
    /// How many transitions this state has.
    trans_len: u8,
}

/// One level of the dictionary: a pattern set plus the limits it applies.
#[derive(Debug, Clone)]
struct Level {
    left_min: usize,
    right_min: usize,
    compound_left_min: usize,
    compound_right_min: usize,
    states: Vec<State>,
    /// Destination state and matched byte, in state order.
    transitions: Vec<(u16, u8)>,
    pool: Vec<u8>,
    /// Character sequences that suppress hyphenation next to them.
    no_hyphen: Vec<Vec<u8>>,
}

impl Level {
    /// The digit string for a state, as ASCII digits.
    fn digits(&self, state: u16) -> &[u8] {
        let Some(at) = self.states[state as usize].digits else {
            return &[];
        };
        let at = at as usize;
        let end = self.pool[at..]
            .iter()
            .position(|&b| b == 0)
            .map_or(self.pool.len(), |n| at + n);
        &self.pool[at..end]
    }

    /// Run the automaton over `word` and raise `values` wherever a pattern
    /// applies. `values[i]` governs a break before byte `i` of `word`.
    fn apply(&self, word: &[u8], values: &mut [u8]) {
        // Patterns are written against a word framed by `.` on both sides, so
        // that a pattern can anchor to the start or the end.
        let mut state: u16 = 0;
        let framed_len = word.len() + 2;
        for i in 0..=framed_len {
            let ch = match i {
                0 => b'.',
                _ if i <= word.len() => word[i - 1],
                _ if i == word.len() + 1 => b'.',
                // One step past the frame flushes any pattern that ends on it.
                _ => 0,
            };
            state = self.step(state, ch);
            let digits = self.digits(state);
            if digits.is_empty() {
                continue;
            }
            // The last digit lands on the byte just consumed, so the string
            // reaches back over the bytes that matched it.
            let Some(start) = (i + 1).checked_sub(digits.len()) else {
                continue;
            };
            for (k, &d) in digits.iter().enumerate() {
                let at = start + k;
                if at < values.len() && values[at] < d - b'0' {
                    values[at] = d - b'0';
                }
            }
        }
    }

    /// The state reached from `state` on `ch`, following fallbacks.
    fn step(&self, state: u16, ch: u8) -> u16 {
        let mut state = state;
        loop {
            let s = self.states[state as usize];
            let from = s.trans_start as usize;
            let found = self.transitions[from..from + s.trans_len as usize]
                .iter()
                .find(|(_, c)| *c == ch);
            if let Some(&(next, _)) = found {
                return next;
            }
            match s.fallback {
                Some(f) => state = f,
                // Nothing in the automaton continues this byte; restart.
                None => return 0,
            }
        }
    }
}

/// A hyphenation dictionary for one language.
#[derive(Debug, Clone)]
pub struct Hyphenator {
    levels: Vec<Level>,
}

impl Hyphenator {
    /// Read a compiled dictionary.
    pub fn parse(bytes: &[u8]) -> Result<Self, HyphenationError> {
        let version = read_u16(bytes, 0).ok_or(HyphenationError::Truncated)?;
        if version != FORMAT_VERSION {
            return Err(HyphenationError::UnsupportedVersion(version));
        }
        let count = read_u16(bytes, 2).ok_or(HyphenationError::Truncated)? as usize;
        let mut at = 4;
        let mut levels = Vec::with_capacity(count);
        for _ in 0..count {
            let (level, next) = parse_level(bytes, at)?;
            levels.push(level);
            at = next;
        }
        if levels.is_empty() {
            return Err(HyphenationError::EmptyLevel);
        }
        Ok(Hyphenator { levels })
    }

    /// Least number of characters that must precede a break.
    pub fn left_min(&self) -> usize {
        self.levels[0].left_min
    }

    /// Least number of characters that must follow a break.
    pub fn right_min(&self) -> usize {
        self.levels[0].right_min
    }

    /// Byte offsets in `word` at which it may be broken, ascending. Each is the
    /// index of the first byte of the part that would move to the next line.
    pub fn hyphenate(&self, word: &str) -> Vec<usize> {
        let bytes = word.as_bytes();
        if bytes.is_empty() {
            return Vec::new();
        }
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

        for level in &self.levels[1..] {
            for pair in bounds.windows(2) {
                let (from, to) = (pair[0], pair[1]);
                let segment = &bytes[from..to];
                if segment.is_empty() {
                    continue;
                }
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

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let b = bytes.get(at..at + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Read one level starting at `at`, returning it and the offset of the next.
fn parse_level(bytes: &[u8], at: usize) -> Result<(Level, usize), HyphenationError> {
    let head = bytes
        .get(at..at + LEVEL_HEADER_LEN)
        .ok_or(HyphenationError::Truncated)?;
    let trans_count = read_u32(head, 0).unwrap() as usize;
    let pool_len = read_u32(head, 4).unwrap() as usize;
    let no_hyphen_count = read_u16(head, 8).unwrap() as i16;
    let state_count = read_u16(head, 10).unwrap() as i16;
    let charset_end = head[12..32].iter().position(|&b| b == 0).unwrap_or(20);
    let charset = String::from_utf8_lossy(&head[12..12 + charset_end]).into_owned();
    if !charset.eq_ignore_ascii_case("UTF-8") {
        return Err(HyphenationError::UnsupportedCharset(charset));
    }
    if state_count <= 0 {
        return Err(HyphenationError::EmptyLevel);
    }
    let state_count = state_count as usize;
    let left_min = head[32] as usize;
    let right_min = head[33] as usize;
    let compound_left_min = head[34] as usize;
    // The top bit of the last byte is the UTF-8 flag, which the charset name
    // already states; the rest is the value.
    let compound_right_min = (head[35] & 0x7f) as usize;

    let states_at = at + LEVEL_HEADER_LEN;
    let trans_at = states_at + state_count * STATE_LEN;
    let pool_at = trans_at + trans_count * TRANS_LEN;
    let end = pool_at + pool_len;
    if bytes.len() < end {
        return Err(HyphenationError::Truncated);
    }

    let mut states = Vec::with_capacity(state_count);
    let mut trans_start: u32 = 0;
    for i in 0..state_count {
        let r = &bytes[states_at + i * STATE_LEN..states_at + (i + 1) * STATE_LEN];
        let digits = read_u32(r, 0).unwrap();
        let fallback = i16::from_le_bytes([r[4], r[5]]);
        let trans_len = i8::from_le_bytes([r[6]]).max(0) as u8;
        states.push(State {
            digits: (digits != 0).then_some(digits),
            fallback: (fallback >= 0).then_some(fallback as u16),
            trans_start,
            trans_len,
        });
        trans_start += trans_len as u32;
    }
    if trans_start as usize != trans_count {
        return Err(HyphenationError::Truncated);
    }

    let transitions = (0..trans_count)
        .map(|i| {
            let r = &bytes[trans_at + i * TRANS_LEN..trans_at + (i + 1) * TRANS_LEN];
            (read_u16(r, 0).unwrap(), r[2])
        })
        .collect();

    let pool = bytes[pool_at..end].to_vec();
    let mut no_hyphen = Vec::new();
    let mut cursor = 0usize;
    for _ in 0..no_hyphen_count.max(0) {
        let Some(n) = pool[cursor..].iter().position(|&b| b == 0) else {
            break;
        };
        no_hyphen.push(pool[cursor..cursor + n].to_vec());
        cursor += n + 1;
    }

    Ok((
        Level {
            left_min,
            right_min,
            compound_left_min,
            compound_right_min,
            states,
            transitions,
            pool,
            no_hyphen,
        },
        end,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One state as the file stores it.
    struct RawState {
        digits: u32,
        fallback: i16,
        trans_len: i8,
        pattern_len: u8,
    }

    /// Build a level's string pool, returning it and the offset of each digit
    /// string. Offset zero means "no digit string", so a pool that opens with
    /// digit strings reserves the first word, as the format does.
    fn pool(no_hyphen: &[&str], digit_strings: &[&str]) -> (Vec<u8>, Vec<u32>) {
        let mut out = Vec::new();
        if no_hyphen.is_empty() {
            out.extend_from_slice(&[0, 0, 0, 0]);
        }
        for s in no_hyphen {
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        let mut offsets = Vec::new();
        for s in digit_strings {
            offsets.push(out.len() as u32);
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        (out, offsets)
    }

    /// Build a level image: header, states, transitions, pool.
    fn level(
        mins: [u8; 4],
        states: &[RawState],
        transitions: &[(u16, u8)],
        no_hyphen: &[&str],
        pool: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(transitions.len() as u32).to_le_bytes());
        out.extend_from_slice(&(pool.len() as u32).to_le_bytes());
        out.extend_from_slice(&(no_hyphen.len() as i16).to_le_bytes());
        out.extend_from_slice(&(states.len() as i16).to_le_bytes());
        let mut charset = [0u8; 20];
        charset[..5].copy_from_slice(b"UTF-8");
        out.extend_from_slice(&charset);
        out.extend_from_slice(&[mins[0], mins[1], mins[2], mins[3] | 0x80]);
        for s in states {
            out.extend_from_slice(&s.digits.to_le_bytes());
            out.extend_from_slice(&s.fallback.to_le_bytes());
            out.push(s.trans_len as u8);
            out.push(s.pattern_len);
        }
        for &(next, ch) in transitions {
            out.extend_from_slice(&next.to_le_bytes());
            // The fourth byte is padding the producer leaves uninitialised.
            out.extend_from_slice(&[ch, 0x5a]);
        }
        out.extend_from_slice(pool);
        out
    }

    /// A dictionary whose only pattern is `a1b`, so "ab" may break between the
    /// two letters, plus the usual first level that breaks around a hyphen.
    fn ab_dictionary(mins: [u8; 4]) -> Vec<u8> {
        // First level: `1-1`.
        let (l0_pool, l0_digits) = pool(&["-"], &["11"]);
        let l0 = level(
            mins,
            &[
                RawState {
                    digits: 0,
                    fallback: -1,
                    trans_len: 1,
                    pattern_len: 0,
                },
                RawState {
                    digits: l0_digits[0],
                    fallback: 0,
                    trans_len: 0,
                    pattern_len: 1,
                },
            ],
            &[(1, b'-')],
            &["-"],
            &l0_pool,
        );
        // Second level: `a1b`, whose digit string is "10" against the two
        // letters that matched it.
        let (l1_pool, l1_digits) = pool(&[], &["10"]);
        let l1 = level(
            mins,
            &[
                RawState {
                    digits: 0,
                    fallback: -1,
                    trans_len: 1,
                    pattern_len: 0,
                },
                RawState {
                    digits: 0,
                    fallback: 0,
                    trans_len: 1,
                    pattern_len: 1,
                },
                RawState {
                    digits: l1_digits[0],
                    fallback: 0,
                    trans_len: 0,
                    pattern_len: 2,
                },
            ],
            &[(1, b'a'), (2, b'b')],
            &[],
            &l1_pool,
        );
        let mut out = Vec::new();
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&l0);
        out.extend_from_slice(&l1);
        out
    }

    #[test]
    fn parses_header_and_limits() {
        let h = Hyphenator::parse(&ab_dictionary([2, 3, 2, 3])).unwrap();
        assert_eq!(h.left_min(), 2);
        assert_eq!(h.right_min(), 3);
        assert_eq!(h.levels.len(), 2);
    }

    #[test]
    fn rejects_a_version_it_does_not_implement() {
        let mut bytes = ab_dictionary([1, 1, 1, 1]);
        bytes[0] = 9;
        assert!(matches!(
            Hyphenator::parse(&bytes),
            Err(HyphenationError::UnsupportedVersion(9))
        ));
    }

    #[test]
    fn rejects_truncated_bytes() {
        let bytes = ab_dictionary([1, 1, 1, 1]);
        assert!(matches!(
            Hyphenator::parse(&bytes[..bytes.len() - 4]),
            Err(HyphenationError::Truncated)
        ));
    }

    #[test]
    fn breaks_where_the_pattern_says() {
        let h = Hyphenator::parse(&ab_dictionary([1, 1, 1, 1])).unwrap();
        // `a1b` puts the break between the two letters wherever "ab" occurs.
        assert_eq!(h.hyphenate("xaby"), vec![2]);
        assert_eq!(h.with_soft_hyphens("xaby"), "xa\u{ad}by");
        assert_eq!(h.hyphenate("xyz"), Vec::<usize>::new());
    }

    #[test]
    fn limits_keep_breaks_away_from_the_edges() {
        // With three characters required on the right, "xab" has nowhere legal
        // to break even though the pattern matches.
        let h = Hyphenator::parse(&ab_dictionary([2, 3, 2, 3])).unwrap();
        assert_eq!(h.hyphenate("xab"), Vec::<usize>::new());
        assert_eq!(h.hyphenate("xabyz"), vec![2]);
    }

    #[test]
    fn an_existing_hyphen_splits_the_word_into_parts() {
        let h = Hyphenator::parse(&ab_dictionary([1, 1, 1, 1])).unwrap();
        // The first level's `1-1` makes each side of the hyphen its own word
        // for the second level, so both halves break on their own pattern.
        assert_eq!(h.hyphenate("ab-ab"), vec![1, 4]);
        // No break is offered against the hyphen itself: the word already
        // breaks there, and a soft hyphen would print a second one.
        assert_eq!(h.with_soft_hyphens("ab-ab"), "a\u{ad}b-a\u{ad}b");
    }

    #[test]
    fn multibyte_text_breaks_only_on_character_boundaries() {
        let h = Hyphenator::parse(&ab_dictionary([1, 1, 1, 1])).unwrap();
        let word = "é-abé";
        for at in h.hyphenate(word) {
            assert!(
                word.is_char_boundary(at),
                "break at {at} splits a character"
            );
        }
    }
}
