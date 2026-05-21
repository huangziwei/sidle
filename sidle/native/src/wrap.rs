//! Pure text-wrap utility.
//!
//! Lives at the crate root (not under `ui/`) because the consumer
//! `ui::text::TextRenderer` pulls in `Framebuffer` which is Linux-only.
//! Keeping wrap separate means `cargo test --lib` can exercise the
//! wrap logic on the host with synthetic widths, without dragging the
//! whole render stack into the test build.

/// Word-wrap `text` to fit `max_width` per line.
///
/// Latin titles wrap at whitespace; CJK titles (no spaces) fall
/// through to char-level wrap so the line packs as densely as
/// possible without overflowing. A single Latin word wider than
/// `max_width` is also char-broken so it doesn't escape the box.
///
/// `measure` is the per-substring width function. The renderer
/// supplies a font-backed implementation; tests supply a fixed-width
/// closure so we can reason about the wrap arithmetic without a real
/// font.
pub fn wrap_to_width<F>(text: &str, max_width: u32, mut measure: F) -> Vec<String>
where
    F: FnMut(&str) -> u32,
{
    let space_w = measure(" ");
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0u32;
    let mut buf = [0u8; 4];

    for token in text.split_whitespace() {
        let token_w = measure(token);
        if token_w > max_width {
            // Token alone overflows a line — break at char boundaries.
            // CJK path (whole string is one token) and Latin edge
            // cases (URLs etc).
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_w = 0;
            }
            for ch in token.chars() {
                let ch_str = ch.encode_utf8(&mut buf);
                let ch_w = measure(ch_str);
                if current_w + ch_w > max_width && !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                    current_w = 0;
                }
                current.push(ch);
                current_w += ch_w;
            }
            continue;
        }

        // Token fits in a line by itself. Append (with leading space)
        // or start a new line.
        let prefix_w = if current.is_empty() { 0 } else { space_w };
        if current_w + prefix_w + token_w > max_width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_w = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_w += space_w;
        }
        current.push_str(token);
        current_w += token_w;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed-width "font" — every char is 10px, space is 10px. Lets us
    /// reason about wrap purely arithmetically.
    fn fixed(s: &str) -> u32 {
        s.chars().count() as u32 * 10
    }

    #[test]
    fn wraps_latin_at_word_boundaries() {
        // 100px max = 10 chars. "hello world" (11 chars) wraps after
        // "hello" because the full string is 110px > 100.
        let lines = wrap_to_width("hello world", 100, fixed);
        assert_eq!(lines, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn fits_single_line_when_under_max() {
        let lines = wrap_to_width("short", 100, fixed);
        assert_eq!(lines, vec!["short".to_string()]);
    }

    #[test]
    fn wraps_cjk_at_char_boundaries() {
        // 50px = 5 chars. "あいうえおかきくけこ" (10 chars) wraps to two
        // lines of 5. No whitespace → whole thing is one "token" →
        // char-level path.
        let lines = wrap_to_width("あいうえおかきくけこ", 50, fixed);
        assert_eq!(
            lines,
            vec!["あいうえお".to_string(), "かきくけこ".to_string()],
        );
    }

    #[test]
    fn empty_text_returns_no_lines() {
        let lines = wrap_to_width("", 100, fixed);
        assert!(lines.is_empty());
    }

    #[test]
    fn long_word_breaks_at_char_when_too_wide() {
        // 30px = 3 chars. "supercalifragilistic" (20 chars) wraps as
        // 3-char chunks. 20/3 = 6 full + 1 partial = 7 lines.
        let lines = wrap_to_width("supercalifragilistic", 30, fixed);
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[0], "sup");
        assert_eq!(lines.last().unwrap(), "ic");
    }

    #[test]
    fn no_line_exceeds_max_in_mixed_text() {
        // Property check: every line ≤ max chars regardless of input
        // mix.
        let lines = wrap_to_width("a bb cccc ddddd", 30, fixed);
        assert!(lines.iter().all(|l| l.chars().count() <= 3));
    }
}
