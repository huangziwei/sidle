//! OPF metadata value vocabulary shared by import and export.
//!
//! Helpers that compute canonical OPF `<meta>` values from book facts.

/// Resolve the Kindle `primary-writing-mode` hint from a book's writing mode
pub fn primary_writing_mode(writing_mode: Option<&str>, ppd: Option<&str>) -> Option<String> {
    let wm = writing_mode.unwrap_or("horizontal-tb");
    let ppd = ppd.unwrap_or("ltr");
    let value = if wm == "horizontal-tb" || wm.is_empty() {
        if ppd == "rtl" {
            "horizontal-rl"
        } else {
            "horizontal-lr"
        }
    } else {
        wm
    };
    if value == "horizontal-lr" {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_writing_mode_rules() {
        assert_eq!(primary_writing_mode(None, None), None);
        assert_eq!(
            primary_writing_mode(Some("horizontal-tb"), Some("ltr")),
            None
        );
        assert_eq!(
            primary_writing_mode(Some("horizontal-tb"), Some("rtl")),
            Some("horizontal-rl".to_string())
        );
        assert_eq!(
            primary_writing_mode(Some("vertical-rl"), Some("ltr")),
            Some("vertical-rl".to_string())
        );
        assert_eq!(
            primary_writing_mode(Some("vertical-lr"), None),
            Some("vertical-lr".to_string())
        );
    }
}
