//! Character classification: what script a character is written in, and how
//! it sits on a vertical line.

/// Whether a character belongs to a CJK script — Han, kana, Hangul, bopomofo
/// and the punctuation set with them.
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

/// Whether a character stands upright on a vertical line under
/// `text-orientation: mixed`. CJK does; Latin lies on its side.
pub fn is_upright_in_vertical(ch: char) -> bool {
    is_cjk(ch)
}

/// Whether a character occupies a full em — the East Asian Wide and
/// Fullwidth classes of UAX #11. A vertical line's inline size is one em, so
/// this is what fits it exactly.
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
}
