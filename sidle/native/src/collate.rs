//! Natural-order string collation, shared by the sort, series, and facet
//! orderings.
//!
//! Port of the desktop's `naturalCompare` (`web/library.js`:1008 —
//! `Intl.Collator(undefined, { numeric: true })`, added in commit 774bd58): a
//! run of ASCII digits compares by its numeric value, so "Vol 2" sorts *before*
//! "Vol 10" instead of lexicographically after it. That lines a series' volumes
//! up by their in-title number even before a numeric series index is set.
//!
//! `numeric` is the *only* override. The non-digit segments stay code-point
//! order — the same `str::cmp` collation the picker already used (see
//! `ui::sort`) — so case / accent / CJK ordering is unchanged; this
//! purely adds the numeric-run handling.

use std::cmp::Ordering;
use std::iter::Peekable;
use std::str::Chars;

/// Compare two strings in natural order: maximal runs of ASCII digits compare by
/// numeric value, every other character by Unicode code point. Total and
/// deterministic, so it is safe as a `sort_by` comparator — when two digit runs
/// have equal value the one with more leading zeros sorts first (e.g. "2" < "02").
pub fn natural_compare(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        let ord = match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                compare_digit_runs(&mut ai, &mut bi)
            }
            (Some(ca), Some(cb)) => {
                // Single code-point step, matching the stdlib `str::cmp` this
                // replaces on the non-numeric segments.
                ai.next();
                bi.next();
                ca.cmp(&cb)
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
}

/// Both iterators sit at the first digit of an ASCII-digit run. Consume each
/// maximal run and compare them as numbers (leading zeros ignored), advancing
/// past both. Equal-valued runs break the tie by leading-zero count (fewer
/// zeros first), keeping the order total.
fn compare_digit_runs(a: &mut Peekable<Chars<'_>>, b: &mut Peekable<Chars<'_>>) -> Ordering {
    let zeros_a = take_zeros(a);
    let zeros_b = take_zeros(b);
    // Walk the significant digits in lockstep, remembering the first mismatch.
    let mut first_diff = Ordering::Equal;
    loop {
        let da = a.peek().copied().filter(char::is_ascii_digit);
        let db = b.peek().copied().filter(char::is_ascii_digit);
        match (da, db) {
            (Some(x), Some(y)) => {
                a.next();
                b.next();
                if first_diff == Ordering::Equal {
                    first_diff = x.cmp(&y);
                }
            }
            // One significant run outlasts the other → it's the larger number,
            // regardless of any earlier same-position digit difference.
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            // Equal digit count: the first differing digit decides; if none
            // differed the values are equal, so fall back to leading-zero count.
            (None, None) => return first_diff.then(zeros_a.cmp(&zeros_b)),
        }
    }
}

/// Consume and count a leading run of `'0'`s at the iterator's current position.
fn take_zeros(it: &mut Peekable<Chars<'_>>) -> usize {
    let mut n = 0;
    while it.peek() == Some(&'0') {
        it.next();
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::{Equal, Greater, Less};

    fn nc(a: &str, b: &str) -> Ordering {
        natural_compare(a, b)
    }

    #[test]
    fn digit_runs_compare_numerically() {
        // The headline fix: numeric, not lexical.
        assert_eq!(nc("Vol 2", "Vol 10"), Less);
        assert_eq!(nc("Vol 10", "Vol 2"), Greater);
        assert_eq!(nc("9", "10"), Less);
        assert_eq!(nc("100", "99"), Greater);
    }

    #[test]
    fn equal_strings_are_equal() {
        assert_eq!(nc("Chapter 12", "Chapter 12"), Equal);
        assert_eq!(nc("", ""), Equal);
    }

    #[test]
    fn prefix_sorts_before_longer() {
        assert_eq!(nc("Vol", "Vol 1"), Less);
        assert_eq!(nc("Vol 1", "Vol 1 Extra"), Less);
    }

    #[test]
    fn non_digit_segments_use_code_point_order() {
        // Matches the str::cmp this replaces: uppercase before lowercase, and
        // CJK by code point (the documented native collation choice).
        assert_eq!(nc("Banana", "apple"), Less); // 'B'(0x42) < 'a'(0x61)
        assert_eq!(nc("夏目漱石", "村上春樹"), Less); // 0x590F < 0x6751
    }

    #[test]
    fn digit_run_then_text_tiebreak() {
        // Equal leading number, then the trailing text decides.
        assert_eq!(nc("3a", "3b"), Less);
        // Larger leading number wins regardless of the trailing text.
        assert_eq!(nc("10x", "9y"), Greater);
    }

    #[test]
    fn leading_zeros_break_only_equal_values() {
        // Same numeric value: deterministic total order, fewer zeros first.
        assert_eq!(nc("2", "02"), Less);
        assert_eq!(nc("02", "2"), Greater);
        assert_eq!(nc("007", "7"), Greater);
        // A larger value still wins despite the zeros.
        assert_eq!(nc("010", "9"), Greater); // 10 > 9
    }

    #[test]
    fn interleaved_runs() {
        assert_eq!(nc("a1b2", "a1b10"), Less);
        assert_eq!(nc("x2y", "x2y"), Equal);
    }

    #[test]
    fn matches_packed_series_key_ordering() {
        // series_key packs name + 8-digit index; natural compare keeps the name
        // primary and the (zero-padded) index numeric, so it slots cleanly into
        // the existing single-string series sort.
        assert_eq!(nc("Saga00000010", "Saga00000020"), Less); // idx 1.0 < 2.0
        assert_eq!(nc("Abyss00000010", "Saga00000010"), Less); // name primary
    }

    #[test]
    fn is_a_total_order_usable_for_sort() {
        // A non-total comparator can make sort_by misbehave; assert a tricky set
        // sorts the way the fix intends.
        let mut v = vec!["v10", "v2", "v1", "v10", "v9", "v100"];
        v.sort_by(|a, b| natural_compare(a, b));
        assert_eq!(v, vec!["v1", "v2", "v9", "v10", "v10", "v100"]);
    }
}
