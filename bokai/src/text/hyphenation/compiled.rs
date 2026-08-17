//! The precompiled dictionary form: a pattern set already laid out as the
//! matching automaton, which is what a Kindle carries in its reader resource
//! bundle under `dicts/bin/hyph_<name>.bin`.
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

use super::HyphenationError;
use super::automaton::{Level, State};

/// The only format version this reader implements.
const FORMAT_VERSION: u16 = 2;
/// Bytes of a level header, before its state, transition and pool arrays.
const LEVEL_HEADER_LEN: usize = 36;
/// Bytes per state record.
const STATE_LEN: usize = 8;
/// Bytes per transition record.
const TRANS_LEN: usize = 4;

/// Read a compiled dictionary into its levels.
pub(super) fn parse(bytes: &[u8]) -> Result<Vec<Level>, HyphenationError> {
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
    Ok(levels)
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
        let trans_len = i8::from_le_bytes([r[6]]).max(0) as u32;
        states.push(State {
            digits: (digits != 0).then_some(digits),
            fallback: (fallback >= 0).then_some(fallback as u32),
            trans_start,
            trans_len,
        });
        trans_start += trans_len;
    }
    if trans_start as usize != trans_count {
        return Err(HyphenationError::Truncated);
    }

    let transitions = (0..trans_count)
        .map(|i| {
            let r = &bytes[trans_at + i * TRANS_LEN..trans_at + (i + 1) * TRANS_LEN];
            (read_u16(r, 0).unwrap() as u32, r[2])
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
    use super::super::Hyphenator;
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
        let h = Hyphenator::from_compiled(&ab_dictionary([2, 3, 2, 3])).unwrap();
        assert_eq!(h.left_min(), 2);
        assert_eq!(h.right_min(), 3);
        assert_eq!(h.levels.len(), 2);
    }

    #[test]
    fn rejects_a_version_it_does_not_implement() {
        let mut bytes = ab_dictionary([1, 1, 1, 1]);
        bytes[0] = 9;
        assert!(matches!(
            Hyphenator::from_compiled(&bytes),
            Err(HyphenationError::UnsupportedVersion(9))
        ));
    }

    #[test]
    fn rejects_truncated_bytes() {
        let bytes = ab_dictionary([1, 1, 1, 1]);
        assert!(matches!(
            Hyphenator::from_compiled(&bytes[..bytes.len() - 4]),
            Err(HyphenationError::Truncated)
        ));
    }

    #[test]
    fn breaks_where_the_pattern_says() {
        let h = Hyphenator::from_compiled(&ab_dictionary([1, 1, 1, 1])).unwrap();
        // `a1b` puts the break between the two letters wherever "ab" occurs.
        assert_eq!(h.hyphenate("xaby"), vec![2]);
        assert_eq!(h.with_soft_hyphens("xaby"), "xa\u{ad}by");
        assert_eq!(h.hyphenate("xyz"), Vec::<usize>::new());
    }

    #[test]
    fn limits_keep_breaks_away_from_the_edges() {
        // With three characters required on the right, "xab" has nowhere legal
        // to break even though the pattern matches.
        let h = Hyphenator::from_compiled(&ab_dictionary([2, 3, 2, 3])).unwrap();
        assert_eq!(h.hyphenate("xab"), Vec::<usize>::new());
        assert_eq!(h.hyphenate("xabyz"), vec![2]);
    }

    #[test]
    fn an_existing_hyphen_splits_the_word_into_parts() {
        let h = Hyphenator::from_compiled(&ab_dictionary([1, 1, 1, 1])).unwrap();
        // The first level's `1-1` makes each side of the hyphen its own word
        // for the second level, so both halves break on their own pattern.
        assert_eq!(h.hyphenate("ab-ab"), vec![1, 4]);
        // No break is offered against the hyphen itself: the word already
        // breaks there, and a soft hyphen would print a second one.
        assert_eq!(h.with_soft_hyphens("ab-ab"), "a\u{ad}b-a\u{ad}b");
    }

    #[test]
    fn multibyte_text_breaks_only_on_character_boundaries() {
        let h = Hyphenator::from_compiled(&ab_dictionary([1, 1, 1, 1])).unwrap();
        let word = "é-abé";
        for at in h.hyphenate(word) {
            assert!(
                word.is_char_boundary(at),
                "break at {at} splits a character"
            );
        }
    }
}
