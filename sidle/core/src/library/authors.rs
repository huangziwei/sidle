//! Author-name normalization.
//!
//! Each book's author(s) live in the single `books.author` display column. Two
//! facts make a naive `Vec<String>` ↔ string round-trip lossy, so this module
//! owns the canonical form:
//!
//! 1. **Western catalogue names arrive surname-first.** KFX's lone `author`
//!    field — and many EPUB `opf:file-as` forms — carry `"Kafka, Franz"`, where
//!    the comma separates surname from given name. boko passes these through
//!    verbatim, so the comma is part of a *single* author's name.
//! 2. **CJK OPFs pack multiple authors into one `<dc:creator>`** with the
//!    ideographic comma `「、」` (`"村上春樹、夏目漱石"`).
//!
//! Joining authors with a plain ASCII comma (the old behaviour) is therefore
//! ambiguous: once `"Kafka, Franz"` is joined and re-split it's indistinguishable
//! from two authors. We resolve it by (a) flipping a Western `Surname, Given` to
//! natural `Given Surname` so a single author never carries a comma, and (b)
//! joining multiple authors with `" & "` (calibre/KFX's own convention), or with
//! `「、」` for all-CJK lists. The ASCII comma is then never a separator, so every
//! reader splits on `[&、]` only — never `,`.
//!
//! boko's EPUB parser emits one entry per `<dc:creator>`, and its KFX importer
//! splits the `author` field on `&` (calibre's join), so the author *count* is
//! already structurally encoded before we get here; the only in-field separator
//! left to unpack is `「、」`.

/// Generational / honorific suffixes that follow a comma in `"Name, Suffix"`
/// (e.g. `"Davis, Jr."`) — that's NOT a `Surname, Given` to flip. Compared
/// case-insensitively with any trailing `.` stripped.
const NAME_SUFFIXES: &[&str] = &[
    "jr", "sr", "ii", "iii", "iv", "v", "phd", "md", "do", "esq", "dds", "jd",
];

/// True for characters in a CJK / kana / Hangul block. Keeps the
/// `Surname, Given` flip away from East-Asian names, which never use an ASCII
/// comma to separate the two halves of a single name.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF |   // Hiragana + Katakana
        0x3400..=0x4DBF |   // CJK Extension A
        0x4E00..=0x9FFF |   // CJK Unified Ideographs
        0xF900..=0xFAFF |   // CJK Compatibility Ideographs
        0xFF00..=0xFFEF |   // Halfwidth/Fullwidth forms
        0xAC00..=0xD7AF |   // Hangul syllables
        0x20000..=0x2FA1F   // CJK Extension B+ / supplement
    )
}

/// Flip a Western `"Surname, Given"` display name to natural `"Given Surname"`.
/// Left unchanged when the name has anything other than exactly one ASCII comma,
/// contains a CJK character, or the post-comma part is a generational suffix.
/// Interior/edge whitespace is collapsed in every case.
pub fn normalize_display(name: &str) -> String {
    let collapsed = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.matches(',').count() != 1 || collapsed.chars().any(is_cjk) {
        return collapsed;
    }
    let (surname, given) = collapsed.split_once(',').unwrap();
    let (surname, given) = (surname.trim(), given.trim());
    if surname.is_empty() || given.is_empty() {
        return collapsed;
    }
    let tail = given.trim_end_matches('.').to_ascii_lowercase();
    if NAME_SUFFIXES.contains(&tail.as_str()) {
        return collapsed;
    }
    format!("{given} {surname}")
}

/// Trim, flip Western names, drop empties — the shared tail of every entry path.
fn canonicalize<'a>(parts: impl Iterator<Item = &'a str>) -> Vec<String> {
    parts
        .map(normalize_display)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Canonical author list from boko's parsed metadata. Each `<dc:creator>` (EPUB)
/// or `&`-split entry (KFX) is already one element; we additionally unpack the
/// CJK `「、」` multiple-authors-in-one-creator case and flip Western names.
pub fn from_metadata(authors: &[String]) -> Vec<String> {
    canonicalize(authors.iter().flat_map(|a| a.split('、')))
}

/// Canonical author list from a user-typed editor string. Authors are separated
/// by `&` or `「、」` — never a plain comma, which is the intra-name
/// `Surname, Given` separator we flip.
pub fn parse_input(s: &str) -> Vec<String> {
    canonicalize(s.split(['&', '、']))
}

/// Split a *stored* display string back into its author list. Splits on `[&、]`
/// only (never comma) and does NOT re-flip — stored names are already canonical.
pub fn split_display(author: &str) -> Vec<String> {
    author
        .split(['&', '、'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Join a canonical author list into the stored display string. All-CJK lists
/// read better with `「、」`; everything else uses `" & "`.
pub fn join_display(authors: &[String]) -> String {
    if authors.len() > 1 && authors.iter().all(|a| a.chars().any(is_cjk)) {
        authors.join("、")
    } else {
        authors.join(" & ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flips_western_surname_first() {
        assert_eq!(normalize_display("Kafka, Franz"), "Franz Kafka");
        assert_eq!(normalize_display("Johnson, Allan"), "Allan Johnson");
        assert_eq!(normalize_display("Alighieri, Dante"), "Dante Alighieri");
        // Multi-word given / initials and multi-word surnames.
        assert_eq!(normalize_display("Tolkien, J.R.R."), "J.R.R. Tolkien");
        assert_eq!(normalize_display("Le Guin, Ursula K."), "Ursula K. Le Guin");
    }

    #[test]
    fn leaves_natural_order_untouched() {
        assert_eq!(normalize_display("Franz Kafka"), "Franz Kafka");
        assert_eq!(normalize_display("J. R. R. Tolkien"), "J. R. R. Tolkien");
    }

    #[test]
    fn never_flips_cjk_or_suffixes() {
        // CJK single name — no flip even though faceting will split on 「、」.
        assert_eq!(normalize_display("村上春樹"), "村上春樹");
        // Romaji is Latin, so a surname-first romaji name does flip — intended.
        assert_eq!(normalize_display("Murakami, Haruki"), "Haruki Murakami");
        // Generational suffix is "Name, Suffix", not "Surname, Given".
        assert_eq!(normalize_display("Davis, Jr."), "Davis, Jr.");
        assert_eq!(normalize_display("King, III"), "King, III");
        // Two commas: ambiguous, leave alone.
        assert_eq!(normalize_display("Davis, Sammy, Jr."), "Davis, Sammy, Jr.");
    }

    #[test]
    fn from_metadata_flips_and_unpacks() {
        // KFX lone surname-first author.
        assert_eq!(from_metadata(&["Kafka, Franz".into()]), vec!["Franz Kafka"]);
        // EPUB: one entry per <dc:creator>, already split by boko.
        assert_eq!(
            from_metadata(&["Doe, John".into(), "Roe, Jane".into()]),
            vec!["John Doe", "Jane Roe"]
        );
        // CJK packed in one creator → unpacked on 「、」, not flipped.
        assert_eq!(
            from_metadata(&["村上春樹、夏目漱石".into()]),
            vec!["村上春樹", "夏目漱石"]
        );
    }

    #[test]
    fn parse_input_uses_ampersand_not_comma() {
        // A lone comma is intra-name → one (flipped) author, never two.
        assert_eq!(parse_input("Tolkien, J.R.R."), vec!["J.R.R. Tolkien"]);
        // Ampersand and ideographic comma separate authors.
        assert_eq!(
            parse_input("Franz Kafka & Jane Doe"),
            vec!["Franz Kafka", "Jane Doe"]
        );
        assert_eq!(parse_input("村上春樹、夏目漱石"), vec!["村上春樹", "夏目漱石"]);
        assert!(parse_input("   ").is_empty());
    }

    #[test]
    fn join_display_picks_separator_by_script() {
        assert_eq!(join_display(&["Franz Kafka".into()]), "Franz Kafka");
        assert_eq!(
            join_display(&["Franz Kafka".into(), "Jane Doe".into()]),
            "Franz Kafka & Jane Doe"
        );
        assert_eq!(
            join_display(&["村上春樹".into(), "夏目漱石".into()]),
            "村上春樹、夏目漱石"
        );
        // Round-trips through split_display.
        let stored = join_display(&["John Doe".into(), "Jane Roe".into()]);
        assert_eq!(split_display(&stored), vec!["John Doe", "Jane Roe"]);
    }
}
