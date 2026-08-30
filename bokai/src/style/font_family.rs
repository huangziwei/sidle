//! What a `font-family` stack asks for.

/// Is this token a generic font *category* rather than a typeface name?
pub fn is_generic_font_keyword(name: &str) -> bool {
    generic_category(name).is_some()
}

/// The category `name` denotes, with any locale cut folded away: `serif-ja-v`
/// and `serif-ja` both answer `serif`. `None` for a typeface name.
fn generic_category(name: &str) -> Option<&'static str> {
    const CATEGORIES: &[&str] = &[
        "sans-serif",
        "serif",
        "monospace",
        "cursive",
        "fantasy",
        "system-ui",
    ];
    let n = name.trim().trim_matches('"').to_ascii_lowercase();
    // Longest first, so `sans-serif-ja` matches `sans-serif` and not `serif`.
    CATEGORIES
        .iter()
        .find(|c| n == **c || n.strip_prefix(*c).is_some_and(|rest| rest.starts_with('-')))
        .copied()
}

/// The category a whole stack asks for: the first generic in it, wherever the
pub fn font_stack_category(stack: &str) -> Option<&'static str> {
    stack.split(',').find_map(generic_category)
}

/// The face a stack prefers: its first entry, unquoted.
pub fn preferred_font_face(stack: &str) -> &str {
    stack
        .split(',')
        .next()
        .unwrap_or(stack)
        .trim()
        .trim_matches('"')
}

/// A stack with no padding around its separators — `a, b` becomes `a,b`.
pub fn compact_font_stack(stack: &str) -> String {
    stack
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_folds_locale_cuts() {
        assert_eq!(font_stack_category("serif-ja, serif"), Some("serif"));
        assert_eq!(font_stack_category("serif-ja-v"), Some("serif"));
        assert_eq!(font_stack_category("sans-serif-ja"), Some("sans-serif"));
        assert_eq!(font_stack_category("sans-serif-ja-v"), Some("sans-serif"));
    }

    #[test]
    fn category_found_in_any_position() {
        assert_eq!(font_stack_category("booksming, serif"), Some("serif"));
        assert_eq!(
            font_stack_category("serif, Apple LiSung Light, PMingLiU"),
            Some("serif")
        );
        assert_eq!(
            font_stack_category("bookskai, booksheiti, sans-serif"),
            Some("sans-serif")
        );
    }

    #[test]
    fn typeface_only_stacks_have_no_category() {
        assert_eq!(font_stack_category("標楷體"), None);
        assert_eq!(font_stack_category("MS Mincho, Hiragino Mincho Pron"), None);
    }

    #[test]
    fn preferred_face_is_the_first_entry() {
        assert_eq!(preferred_font_face("booksming, serif"), "booksming");
        assert_eq!(preferred_font_face("\"標楷體\", serif"), "標楷體");
        assert_eq!(preferred_font_face("serif"), "serif");
    }

    #[test]
    fn compacting_keeps_spaces_inside_names() {
        assert_eq!(
            compact_font_stack("default, times new roman, serif"),
            "default,times new roman,serif"
        );
        assert_eq!(compact_font_stack("serif"), "serif");
        assert_eq!(compact_font_stack("a,  ,b"), "a,b");
    }
}
