//! A character's script, its standing on a vertical line, and its advance.

/// Whether `ch` is Han, kana, Hangul, bopomofo or the punctuation with them.
pub fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x11FF   // Hangul Jamo
        | 0x2E80..=0x2EFF // CJK radicals
        | 0x3000..=0x303F // CJK symbols and punctuation
        | 0x3040..=0x30FF // Hiragana, katakana
        | 0x3100..=0x312F // Bopomofo
        | 0x3130..=0x318F // Hangul compatibility jamo
        | 0x31C0..=0x31FF // CJK strokes, katakana extensions
        | 0x3200..=0x33FF // Enclosed and compatibility forms
        | 0x3400..=0x4DBF // Unified extension A
        | 0x4E00..=0x9FFF // Unified
        | 0xA960..=0xA97F // Hangul Jamo extended A
        | 0xAC00..=0xD7AF // Hangul syllables
        | 0xF900..=0xFAFF // Compatibility ideographs
        | 0xFE10..=0xFE1F // Vertical forms
        | 0xFE30..=0xFE4F // CJK compatibility forms
        | 0xFF00..=0xFF60 // Fullwidth forms
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD)
}

/// Whether `ch` stands upright on a vertical line. Latin lies on its side.
pub fn is_upright_in_vertical(ch: char) -> bool {
    is_cjk(ch)
}

/// Whether `ch` occupies a full em: the Wide and Fullwidth classes of
/// UAX #11.
pub fn is_full_width(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x20000..=0x3FFFD)
}

/// Which half of its em a mark leaves blank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blank {
    /// Ink first, blank after: a comma, a full stop, a closing bracket.
    After,
    /// Blank first, ink after: an opening bracket.
    Before,
    /// No blank half: an ideograph, a kana, a mark centred in its own em.
    None,
}

/// Marks whose ink leads and whose blank follows.
const BLANK_AFTER: &[char] = &[
    '、', '。', '，', '．', '｡', '､', '」', '』', '）', '］', '｝', '〉', '》', '】', '〕', '〗',
    '〙', '〛', '｣',
];

/// Marks whose blank leads and whose ink follows.
const BLANK_BEFORE: &[char] = &[
    '「', '『', '（', '［', '｛', '〈', '《', '【', '〔', '〖', '〘', '〚', '｢',
];

/// Which half of `ch`'s em is blank.
pub fn blank_half(ch: char) -> Blank {
    if BLANK_AFTER.contains(&ch) {
        Blank::After
    } else if BLANK_BEFORE.contains(&ch) {
        Blank::Before
    } else {
        Blank::None
    }
}

/// Whether `junction_trim` can take a half-em off `ch`.
pub fn is_cjk_punctuation(ch: char) -> bool {
    blank_half(ch) != Blank::None
}

/// The em fractions `left` and `right` give up at the junction between them.
/// One half-em goes where a mark offers a blank, `right` first.
pub fn junction_trim(left: char, right: char) -> (f32, f32) {
    if !is_cjk_punctuation(left) || !is_cjk_punctuation(right) {
        return (0.0, 0.0);
    }
    match (blank_half(left), blank_half(right)) {
        (_, Blank::Before) => (0.0, 0.5),
        (Blank::After, _) => (0.5, 0.0),
        _ => (0.0, 0.0),
    }
}

/// `ch`'s advance as a multiple of the em, `None` where the face states it.
/// `：` and `；` take one and a half on a vertical line.
pub fn em_advance(ch: char, vertical: bool) -> Option<f32> {
    if vertical && matches!(ch, '：' | '；') {
        return Some(1.5);
    }
    is_full_width(ch).then_some(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kana_and_kanji_are_cjk_and_latin_is_not() {
        assert!(is_cjk('日'));
        assert!(is_cjk('ひ'));
        assert!(is_cjk('カ'));
        assert!(is_cjk('。'));
        assert!(!is_cjk('a'));
        assert!(!is_cjk('!'));
    }

    #[test]
    fn latin_lies_on_its_side_in_vertical_text() {
        assert!(is_upright_in_vertical('日'));
        assert!(!is_upright_in_vertical('A'));
    }

    #[test]
    fn fullwidth_punctuation_counts_as_full_width() {
        assert!(is_full_width('、'));
        assert!(is_full_width('！')); // U+FF01, the fullwidth form
        assert!(!is_full_width('!')); // U+0021, the ASCII one
    }

    #[test]
    fn a_mark_beside_an_ideograph_keeps_its_whole_em() {
        assert_eq!(junction_trim('亜', '、'), (0.0, 0.0));
        assert_eq!(junction_trim('、', '亜'), (0.0, 0.0));
        assert_eq!(junction_trim('。', 'あ'), (0.0, 0.0));
        assert_eq!(junction_trim('あ', '「'), (0.0, 0.0));
    }

    #[test]
    fn one_blank_at_a_junction_is_given_up_by_the_side_that_has_it() {
        assert_eq!(junction_trim('、', '。'), (0.5, 0.0));
        assert_eq!(junction_trim('、', '、'), (0.5, 0.0));
        assert_eq!(junction_trim('、', '」'), (0.5, 0.0));
        assert_eq!(junction_trim('「', '「'), (0.0, 0.5));
    }

    #[test]
    fn two_blanks_at_a_junction_are_charged_to_the_following_mark() {
        assert_eq!(junction_trim('。', '「'), (0.0, 0.5));
        assert_eq!(junction_trim('」', '「'), (0.0, 0.5));
        assert_eq!(junction_trim('）', '（'), (0.0, 0.5));
        assert_eq!(junction_trim('】', '【'), (0.0, 0.5));
    }

    #[test]
    fn ink_meeting_ink_gives_up_nothing() {
        assert_eq!(junction_trim('「', '」'), (0.0, 0.0));
        assert_eq!(junction_trim('（', '）'), (0.0, 0.0));
    }

    #[test]
    fn the_colon_and_semicolon_widen_on_a_vertical_line() {
        assert_eq!(em_advance('：', true), Some(1.5));
        assert_eq!(em_advance('：', false), Some(1.0));
        assert_eq!(em_advance('；', true), Some(1.5));
        assert_eq!(em_advance('亜', true), Some(1.0));
        assert_eq!(em_advance('n', false), None);
    }
}
