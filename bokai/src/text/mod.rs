//! Text services below the style and document layers: rules about characters and
//! words.

pub mod hyphenation;

/// Whether `c` is in a CJK ideograph, kana or Hangul block, or is a full/halfwidth
/// form.
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF |
        0x3400..=0x4DBF |
        0x4E00..=0x9FFF |
        0xF900..=0xFAFF |
        0xFF00..=0xFFEF |
        0xAC00..=0xD7AF |
        0x20000..=0x2FA1F
    )
}
