//! Romanization for the searchable `title_romaji`/`author_romaji` metadata and
//! the picker's derived search key.
//!
//! Japanese → rōmaji (kakasi, kanji-aware), Chinese → tone-less pīnyīn,
//! everything else → itself. Two surfaces:
//!
//! - [`romanize_field`] produces the **human-readable** romaji stored in the
//!   editable `*_romaji` columns (spaces kept, lowercased) — generated at import
//!   and correctable in the library modal.
//! - [`search_key`] assembles those (curated) romaji with the auto-romanized
//!   series/publisher/tags and the raw fields, then [`canon`]s the whole into
//!   the space/punctuation-free, ASCII-folded string the device substring-matches.
//!
//! Why prefer a book's own *kana* reading over the engine: a Japanese book's
//! `opf:file-as` / KFX `*_pronunciation` (surfaced by bokai as
//! `Metadata.title_sort` / `author_sorts`) is the authoritative yomigana when
//! it's actually phonetic. But a `file-as` is *sometimes* a kanji **sort form**
//! (e.g. `森橋 ビンゴ`), which is no better than the raw text — so we only trust a
//! reading that is [`is_kana_dominant`] and otherwise fall back to the engine.

use unicode_normalization::UnicodeNormalization;

/// Human-readable romaji for one metadata field (a title or an author line).
///
/// `reading` is the book's own yomi for this field when known (bokai's
/// `title_sort` / `author_sorts`); it wins when it's phonetic kana. Output is
/// lowercased with single-spaced words — the form stored in `title_romaji` /
/// `author_romaji` and shown in the editor.
pub fn romanize_field(text: &str, reading: Option<&str>, language: &str) -> String {
    collapse_ws(&romanize_raw(text, reading, language).to_lowercase())
}

/// The romaji before lowercase/whitespace normalization — split out so the
/// kana-reading and engine paths share the same post-processing.
fn romanize_raw(text: &str, reading: Option<&str>, language: &str) -> String {
    // A phonetic kana reading romanizes exactly — prefer it over any engine guess.
    if let Some(r) = reading {
        let r = r.trim();
        if !r.is_empty() && is_kana_dominant(r) {
            return kakasi::convert(r).romaji;
        }
    }
    match primary_subtag(language) {
        "ja" => kakasi::convert(text).romaji,
        "zh" => pinyin_of(text),
        // English and anything else is already Latin (or we have no engine for
        // it) — pass through; `canon` ASCII-folds any accents later.
        _ => text.to_string(),
    }
}

/// Tone-less pīnyīn for Han runs, leaving non-Han characters (Latin, digits,
/// spaces) in place so a mixed title keeps its ASCII parts. One space after each
/// syllable, collapsed by [`collapse_ws`] in the caller.
fn pinyin_of(text: &str) -> String {
    use pinyin::ToPinyin;
    let mut out = String::new();
    // `to_pinyin()` yields one `Option<Pinyin>` per char (None for non-Han),
    // aligned 1:1 with `chars()`.
    for (ch, py) in text.chars().zip(text.to_pinyin()) {
        match py {
            Some(p) => {
                out.push_str(p.plain());
                out.push(' ');
            }
            None => out.push(ch),
        }
    }
    out
}

/// The device match key: curated romaji (title/author) + auto-romanized
/// series/publisher/tags + the raw fields, all [`canon`]'d into one
/// space/punctuation-free lowercase ASCII string. Including the raw fields means
/// English titles and embedded Latin runs (`"ONE HAND EDEN"`) match with no
/// romanization at all.
///
/// `title_romaji` / `author_romaji` are the stored (possibly hand-corrected)
/// values; when empty (a pre-backfill row, or a book in a language we generated
/// nothing for) they fall back to a live [`romanize_field`] so search never goes
/// blind on a book.
#[allow(clippy::too_many_arguments)]
pub fn search_key(
    title: &str,
    author: &str,
    publisher: Option<&str>,
    series: Option<&str>,
    tags: &[String],
    language: &str,
    title_romaji: &str,
    author_romaji: &str,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Curated romaji (or a live fallback when the column is empty).
    parts.push(non_empty_or(title_romaji, || {
        romanize_field(title, None, language)
    }));
    parts.push(non_empty_or(author_romaji, || {
        romanize_field(author, None, language)
    }));

    // Auto-romanized secondary fields — rarely hand-corrected, so generated on
    // the fly rather than stored (a series is reachable via its members anyway).
    if let Some(s) = series {
        parts.push(romanize_field(s, None, language));
    }
    if let Some(p) = publisher {
        parts.push(romanize_field(p, None, language));
    }
    for t in tags {
        parts.push(romanize_field(t, None, language));
    }

    // Raw fields too — Latin substrings (and English titles) match directly.
    parts.push(title.to_string());
    parts.push(author.to_string());
    if let Some(s) = series {
        parts.push(s.to_string());
    }
    if let Some(p) = publisher {
        parts.push(p.to_string());
    }
    for t in tags {
        parts.push(t.clone());
    }

    // Primary key: NFKD-folded (ä→a, é→e, ß→dropped). Also index the
    // **digraph-expanded** Latin form (ä→ae, ö→oe, ü→ue, ß→ss, œ→oe, ø→oe) so a
    // German/Nordic spelling matches both ways — `muller` *and* `mueller` find
    // "Müller", and `strasse`/`oeuvre` aren't lost to dropped glyphs. Appended
    // only when it actually differs (i.e. the text had such a character), so
    // ASCII/CJK books carry no extra bytes. The on-screen keyboard types ASCII,
    // so the query side needs no expansion.
    let joined = parts.join(" ");
    let mut key = canon(&joined);
    let expanded = canon(&expand_latin(&joined));
    if expanded != key {
        key.push_str(&expanded);
    }
    key
}

/// Fold a string to the canonical match form: NFKD ASCII-fold (ō→o, é→e,
/// fullwidth→half), lowercase, then keep only `[a-z0-9]`. Space- and
/// punctuation-free so the on-screen keyboard needs neither: `murakami` and
/// `murakamiharuki` both substring-hit `"murakami haruki"`. Applied to both the
/// stored-key assembly and the typed query.
pub fn canon(s: &str) -> String {
    s.nfkd()
        .filter(|c| !is_combining(*c))
        .flat_map(|c| c.to_lowercase())
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// Expand the Latin letters whose conventional ASCII spelling is a **digraph**,
/// not a single base letter — so they survive `canon` (which would otherwise
/// fold `ä→a` and *drop* `ß`/`œ`/`ø` entirely, since those have no NFKD
/// decomposition). German umlauts + eszett, the œ/æ ligatures, and the Nordic ø.
/// Everything else passes through unchanged (its accent is handled by `canon`'s
/// NFKD fold). Case is irrelevant — the result is lowercased by `canon`.
fn expand_latin(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'ä' | 'Ä' => out.push_str("ae"),
            'ö' | 'Ö' | 'ø' | 'Ø' | 'œ' | 'Œ' => out.push_str("oe"),
            'ü' | 'Ü' => out.push_str("ue"),
            'ß' | 'ẞ' => out.push_str("ss"),
            'æ' | 'Æ' => out.push_str("ae"),
            other => out.push(other),
        }
    }
    out
}

/// True when `s` is phonetic kana — at least one hiragana/katakana and **no**
/// kanji. A `file-as` that contains kanji is a sort form, not a yomi, so it
/// fails this and the engine takes over.
pub fn is_kana_dominant(s: &str) -> bool {
    let mut kana = 0usize;
    for c in s.chars() {
        if is_kanji(c) {
            return false;
        }
        if is_kana(c) {
            kana += 1;
        }
    }
    kana > 0
}

fn is_kana(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{309F}'   // hiragana
        | '\u{30A0}'..='\u{30FF}' // katakana (incl. ー prolonged sound mark)
        | '\u{31F0}'..='\u{31FF}' // katakana phonetic extensions
        | '\u{FF66}'..='\u{FF9D}' // half-width katakana
    )
}

fn is_kanji(c: char) -> bool {
    matches!(c,
        '\u{3400}'..='\u{4DBF}'   // CJK ext A
        | '\u{4E00}'..='\u{9FFF}' // CJK unified
        | '\u{F900}'..='\u{FAFF}' // CJK compatibility ideographs
    )
}

/// Combining diacritical marks dropped after NFKD (the macron from ō, the acute
/// from é, …) so the base letter survives the `[a-z0-9]` filter.
fn is_combining(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{20D0}'..='\u{20FF}')
}

/// Primary BCP-47 subtag, lowercased (`zh-Hant` → `zh`, `en-US` → `en`). The
/// language is already harmonized by [`super::lang`] upstream, but split
/// defensively.
fn primary_subtag(language: &str) -> &str {
    language.split(['-', '_']).next().unwrap_or("").trim()
    // NB: returns a borrowed slice; case is handled by the canonical codes
    // (`ja`/`zh`/`en` are already lowercase out of `lang::normalize`).
}

fn non_empty_or(s: &str, f: impl FnOnce() -> String) -> String {
    if s.trim().is_empty() {
        f()
    } else {
        s.to_string()
    }
}

/// Collapse runs of whitespace to single spaces and trim — kakasi/pinyin emit a
/// space per word/syllable, which we tidy for the human-readable stored form.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kana_reading_romanizes_exactly() {
        // A phonetic kana yomi for a kanji title → exact romaji, beating the engine.
        assert_eq!(romanize_field("世界", Some("せかい"), "ja"), "sekai");
        // Katakana reading (the real `コチラアミコ` shape from the library).
        assert_eq!(
            romanize_field("此方アミ子", Some("コチラアミコ"), "ja"),
            "kochiraamiko"
        );
    }

    #[test]
    fn kanji_sort_form_reading_is_ignored() {
        // `森橋 ビンゴ` contains kanji → it's a sort form, not a yomi → not used.
        assert!(!is_kana_dominant("森橋 ビンゴ"));
        // Pure kana passes.
        assert!(is_kana_dominant("もりはし　びんご"));
        assert!(is_kana_dominant("コチラアミコ"));
        // No kana at all (pure Latin / digits) is not "kana dominant".
        assert!(!is_kana_dominant("Bingo"));
        assert!(!is_kana_dominant("001"));
    }

    #[test]
    fn engine_falls_back_by_language() {
        // Japanese kanji via kakasi's dictionary.
        assert!(romanize_field("世界", None, "ja").contains("sekai"));
        // Chinese via pinyin (tone-less), even with a script subtag.
        assert_eq!(romanize_field("北京", None, "zh-Hans"), "bei jing");
        assert_eq!(romanize_field("北京", None, "zh-Hant"), "bei jing");
        // English (and unknown) pass through, lowercased.
        assert_eq!(romanize_field("The Mystery", None, "en"), "the mystery");
        assert_eq!(romanize_field("Café Noir", None, ""), "café noir");
    }

    #[test]
    fn canon_folds_accents_and_strips_nonalnum() {
        assert_eq!(canon("Tōkyō"), "tokyo");
        assert_eq!(canon("Café Noir"), "cafenoir");
        assert_eq!(canon("bei jing"), "beijing");
        assert_eq!(canon("Murakami Haruki!"), "murakamiharuki");
        // Fullwidth digits/letters fold to ASCII; CJK drops out.
        assert_eq!(canon("ＡＢ１２ 世界"), "ab12");
    }

    #[test]
    fn expand_latin_maps_digraphs_only() {
        // Only the mapped glyph changes (to a lowercase digraph — case is moot,
        // canon lowercases); surrounding letters keep their case.
        assert_eq!(expand_latin("Müller"), "Mueller");
        assert_eq!(expand_latin("Köln"), "Koeln");
        assert_eq!(expand_latin("Straße"), "Strasse");
        assert_eq!(expand_latin("Œuvre"), "oeuvre"); // ligature maps to lowercase
        assert_eq!(expand_latin("Køln"), "Koeln"); // Nordic ø
        // French acute etc. are left to canon's NFKD fold (no digraph needed).
        assert_eq!(expand_latin("Café"), "Café");
        assert_eq!(expand_latin("Murakami"), "Murakami");
    }

    #[test]
    fn search_key_indexes_both_accent_and_digraph_forms() {
        // ä/ö/ü: BOTH the accent-stripped (a/o/u) and digraph (ae/oe/ue) spellings
        // index, so either guess on the ASCII keyboard hits.
        let key = search_key("Müller", "Köln", None, None, &[], "de", "", "");
        assert!(key.contains("muller") && key.contains("mueller"), "{key}");
        assert!(key.contains("koln") && key.contains("koeln"), "{key}");
        // ß and the œ/ø glyphs would otherwise be DROPPED by canon — now ss / oe.
        assert!(search_key("Straße", "", None, None, &[], "de", "", "").contains("strasse"));
        assert!(search_key("Œuvre", "", None, None, &[], "fr", "", "").contains("oeuvre"));
        assert!(search_key("Køln", "", None, None, &[], "da", "", "").contains("koeln"));
        // A book with no such characters carries no expanded suffix (idempotent).
        let plain = search_key("Murakami", "", None, None, &[], "en", "", "");
        assert_eq!(plain, canon("Murakami Murakami"));
    }

    #[test]
    fn search_key_uses_curated_romaji_and_includes_raw() {
        let key = search_key(
            "世界最高の暗殺者",
            "村上春樹",
            Some("新潮社"),
            Some("物語"),
            &["scifi".to_string()],
            "ja",
            "sekai saikou no ansatsusha", // curated title romaji
            "murakami haruki",            // curated author romaji
        );
        assert!(
            key.contains("sekaisaikou"),
            "curated title romaji folded in: {key}"
        );
        assert!(
            key.contains("murakamiharuki"),
            "curated author romaji: {key}"
        );
        assert!(key.contains("scifi"), "tags included: {key}");
        // Series romaji (auto) present so a series search surfaces members.
        assert!(key.contains("monogatari"), "series auto-romaji: {key}");
    }

    #[test]
    fn search_key_falls_back_when_romaji_empty() {
        // Empty stored romaji → live engine fallback, so search still works.
        let key = search_key("世界", "", None, None, &[], "ja", "", "");
        assert!(
            key.contains("sekai"),
            "live fallback romanized the title: {key}"
        );

        // English book with empty romaji → raw Latin matches directly.
        let key = search_key(
            "The Roman Hat Mystery",
            "Ellery Queen",
            None,
            None,
            &[],
            "en",
            "",
            "",
        );
        assert!(key.contains("romanhat"));
        assert!(key.contains("elleryqueen"));
    }
}
