//! The datatype libraries a RELAX NG grammar can reference.
//!
//! Two are needed here. The **built-in** library (`string` and `token`) is
//! required of every implementation. The **XSD** library is what the vendored
//! EPUB grammars actually lean on — `ID`, `IDREF`, `NCName`, `anyURI`, `string`,
//! `token`, `NMTOKEN`, `language`, `positiveInteger` and a few others — and it
//! is where a violation like "an `id` that starts with a digit" is decided.
//!
//! Each type answers two questions the algorithm asks: does this string belong
//! to the type's lexical space (`allows`), and are these two strings equal in
//! its value space (`equal`) — the second being why `<value>1</value>` matches
//! the literal ` 1 ` for a whitespace-collapsing type and not for `string`.
//!
//! Unknown types are **accepted**. A grammar naming a type this module does not
//! model would otherwise reject every document that uses it, turning a gap in
//! coverage into a false positive; accepting is the direction that can only
//! under-report.

/// XSD's `whiteSpace` facet, which decides both what `allows` sees and how
/// `equal` compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhiteSpace {
    /// Keep the literal as written (`string`).
    Preserve,
    /// Tab/newline/CR become spaces (`normalizedString`).
    Replace,
    /// …and runs of spaces collapse, with the ends trimmed (`token` and every
    /// type derived from it, which is most of them).
    Collapse,
}

/// Apply a type's whitespace facet, yielding the value the lexical checks and
/// value-space comparison both work on.
fn normalize(ws: WhiteSpace, s: &str) -> String {
    match ws {
        WhiteSpace::Preserve => s.to_string(),
        WhiteSpace::Replace => s
            .chars()
            .map(|c| {
                if matches!(c, '\t' | '\n' | '\r') {
                    ' '
                } else {
                    c
                }
            })
            .collect(),
        WhiteSpace::Collapse => s.split_whitespace().collect::<Vec<_>>().join(" "),
    }
}

/// The whitespace facet of an XSD type. Everything derived from `token` — which
/// is every name-like and numeric type — collapses.
fn whitespace_of(name: &str) -> WhiteSpace {
    match name {
        "string" => WhiteSpace::Preserve,
        "normalizedString" => WhiteSpace::Replace,
        _ => WhiteSpace::Collapse,
    }
}

/// Is `library` the XSD datatype library?
fn is_xsd(library: &str) -> bool {
    library == "http://www.w3.org/2001/XMLSchema-datatypes"
        || library == "http://www.w3.org/2001/XMLSchema"
}

/// Does `value` belong to the lexical space of `name` in `library`, once its
/// parameters are applied?
pub fn allows(library: &str, name: &str, params: &[(String, String)], value: &str) -> bool {
    if !is_xsd(library) {
        // The built-in library has exactly `string` and `token`, and both accept
        // every string; any other library is one this module does not model, and
        // accepting is the direction that can only under-report.
        return true;
    }
    let ws = whitespace_of(name);
    let v = normalize(ws, value);
    if !lexically_valid(name, &v) {
        return false;
    }
    params
        .iter()
        .all(|(param, arg)| facet_holds(param, arg, &v))
}

/// Are `a` and `b` the same value of this type? RELAX NG's `<value>` compares in
/// the value space, so a collapsing type ignores surrounding whitespace.
pub fn equal(library: &str, name: &str, a: &str, b: &str) -> bool {
    if !is_xsd(library) {
        return match name {
            "token" => normalize(WhiteSpace::Collapse, a) == normalize(WhiteSpace::Collapse, b),
            _ => a == b,
        };
    }
    let ws = whitespace_of(name);
    let (a, b) = (normalize(ws, a), normalize(ws, b));
    if is_numeric(name) {
        // 01 and 1 are the same integer; a lexical comparison would say no.
        if let (Ok(x), Ok(y)) = (a.parse::<i128>(), b.parse::<i128>()) {
            return x == y;
        }
    }
    a == b
}

fn is_numeric(name: &str) -> bool {
    matches!(
        name,
        "integer"
            | "int"
            | "long"
            | "short"
            | "byte"
            | "nonNegativeInteger"
            | "positiveInteger"
            | "nonPositiveInteger"
            | "negativeInteger"
            | "unsignedInt"
            | "unsignedLong"
            | "unsignedShort"
            | "unsignedByte"
    )
}

/// The lexical rule of one XSD type. An unmodelled type accepts everything, so
/// coverage gaps under-report rather than misfire.
fn lexically_valid(name: &str, v: &str) -> bool {
    match name {
        "string" | "normalizedString" | "token" | "anyURI" => true,
        "NCName" | "ID" | "IDREF" | "ENTITY" => is_ncname(v),
        "Name" => is_name(v),
        "NMTOKEN" => !v.is_empty() && v.chars().all(is_name_char),
        "IDREFS" | "ENTITIES" => !v.is_empty() && v.split(' ').all(is_ncname),
        "NMTOKENS" => {
            !v.is_empty()
                && v.split(' ')
                    .all(|t| !t.is_empty() && t.chars().all(is_name_char))
        }
        "language" => is_language(v),
        "boolean" => matches!(v, "true" | "false" | "0" | "1"),
        "integer" | "int" | "long" | "short" | "byte" => is_integer(v),
        "nonNegativeInteger" | "unsignedInt" | "unsignedLong" | "unsignedShort"
        | "unsignedByte" => is_integer(v) && !v.starts_with('-'),
        "positiveInteger" => {
            is_integer(v) && !v.starts_with('-') && !v.trim_start_matches('0').is_empty()
        }
        "nonPositiveInteger" => {
            is_integer(v) && (v.starts_with('-') || v.trim_start_matches('0').is_empty())
        }
        "negativeInteger" => is_integer(v) && v.starts_with('-'),
        "decimal" | "double" | "float" => is_decimal(v),
        "QName" => v.split(':').count() <= 2 && v.split(':').all(is_ncname),
        _ => true,
    }
}

/// The `length`/`minLength`/`maxLength`/`pattern` facets a `<param>` can set.
/// An unmodelled facet holds, for the same reason an unmodelled type allows.
fn facet_holds(param: &str, arg: &str, v: &str) -> bool {
    let len = v.chars().count();
    match param {
        "length" => arg.parse::<usize>().is_ok_and(|n| len == n),
        "minLength" => arg.parse::<usize>().is_ok_and(|n| len >= n),
        "maxLength" => arg.parse::<usize>().is_ok_and(|n| len <= n),
        // `pattern` is an XSD regular expression. Applying it needs a regex
        // engine over that dialect; until then it holds, which under-reports.
        _ => true,
    }
}

/// XML's `NameStartChar`, restricted to the ranges the specification lists.
fn is_name_start(c: char) -> bool {
    c == '_'
        || c.is_ascii_alphabetic()
        || matches!(c as u32,
            0xC0..=0xD6 | 0xD8..=0xF6 | 0xF8..=0x2FF | 0x370..=0x37D | 0x37F..=0x1FFF
            | 0x200C..=0x200D | 0x2070..=0x218F | 0x2C00..=0x2FEF | 0x3001..=0xD7FF
            | 0xF900..=0xFDCF | 0xFDF0..=0xFFFD | 0x10000..=0xEFFFF)
}

/// XML's `NameChar`.
fn is_name_char(c: char) -> bool {
    is_name_start(c)
        || c == '-'
        || c == '.'
        || c == ':'
        || c.is_ascii_digit()
        || matches!(c as u32, 0xB7 | 0x0300..=0x036F | 0x203F..=0x2040)
}

/// An `NCName` is a `Name` with no colon — the lexical space of `ID`, `IDREF`
/// and `NCName` itself, which is what makes an `id` starting with a digit a
/// schema violation.
fn is_ncname(v: &str) -> bool {
    let mut chars = v.chars();
    match chars.next() {
        Some(c) if is_name_start(c) && c != ':' => {}
        _ => return false,
    }
    chars.all(|c| is_name_char(c) && c != ':')
}

fn is_name(v: &str) -> bool {
    let mut chars = v.chars();
    match chars.next() {
        Some(c) if is_name_start(c) || c == ':' => {}
        _ => return false,
    }
    chars.all(is_name_char)
}

fn is_integer(v: &str) -> bool {
    let digits = v.strip_prefix(['+', '-']).unwrap_or(v);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn is_decimal(v: &str) -> bool {
    let body = v.strip_prefix(['+', '-']).unwrap_or(v);
    let (int, frac) = match body.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    (!int.is_empty() || frac.is_some_and(|f| !f.is_empty()))
        && digits(int)
        && frac.is_none_or(digits)
}

/// RFC 3066 shape, which is all XSD's `language` requires: alphabetic primary
/// subtag of 1–8 characters, then alphanumeric subtags of 1–8.
fn is_language(v: &str) -> bool {
    let mut parts = v.split('-');
    let Some(first) = parts.next() else {
        return false;
    };
    if first.is_empty() || first.len() > 8 || !first.bytes().all(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    parts.all(|p| !p.is_empty() && p.len() <= 8 && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const XSD: &str = "http://www.w3.org/2001/XMLSchema-datatypes";

    #[test]
    fn id_and_ncname_reject_what_the_grammars_reject() {
        // The corpus class: calibre ids beginning with a digit.
        for bad in ["1first", "0-4358", "a:b", "a b", "", "-lead", ".lead"] {
            assert!(!allows(XSD, "ID", &[], bad), "{bad:?} is not an NCName");
            assert!(!allows(XSD, "NCName", &[], bad));
        }
        for ok in ["a", "_x", "chapter-1", "p.1_2", "日本語", "é1"] {
            assert!(allows(XSD, "ID", &[], ok), "{ok:?} is an NCName");
        }
        // ID derives from token, so it collapses: a padded value is still valid.
        assert!(allows(XSD, "ID", &[], "  nav  "));
        assert!(!allows(XSD, "ID", &[], "a b"), "collapsing leaves a space");
    }

    #[test]
    fn whitespace_facet_drives_both_lexical_and_value_space() {
        // `string` preserves, so the padded literal is a different value…
        assert!(!equal(XSD, "string", " x ", "x"));
        // …while a collapsing type calls them equal.
        assert!(equal(XSD, "token", " x ", "x"));
        assert!(equal(XSD, "NCName", "a\n", "a"));
        // Numeric types compare in the value space, not lexically.
        assert!(equal(XSD, "integer", "01", "1"));
        assert!(!equal(XSD, "string", "01", "1"));
    }

    #[test]
    fn the_numeric_and_name_types_hold_their_lexical_rules() {
        assert!(allows(XSD, "positiveInteger", &[], "1"));
        assert!(!allows(XSD, "positiveInteger", &[], "0"));
        assert!(!allows(XSD, "positiveInteger", &[], "-1"));
        assert!(allows(XSD, "nonNegativeInteger", &[], "0"));
        assert!(allows(XSD, "boolean", &[], "true"));
        assert!(!allows(XSD, "boolean", &[], "yes"));
        assert!(allows(XSD, "NMTOKEN", &[], "a:b-1"));
        assert!(!allows(XSD, "NMTOKEN", &[], "a b"));
        assert!(allows(XSD, "language", &[], "zh-Hant-TW"));
        assert!(!allows(XSD, "language", &[], "toolongsubtag"));
        assert!(allows(XSD, "decimal", &[], "-1.5"));
        assert!(!allows(XSD, "decimal", &[], "1.5e3"));
    }

    #[test]
    fn length_facets_apply_and_unknown_ones_hold() {
        let p = |k: &str, v: &str| vec![(k.to_string(), v.to_string())];
        assert!(allows(XSD, "string", &p("length", "3"), "abc"));
        assert!(!allows(XSD, "string", &p("length", "3"), "abcd"));
        assert!(allows(XSD, "string", &p("minLength", "2"), "abc"));
        assert!(!allows(XSD, "string", &p("maxLength", "2"), "abc"));
        // A facet this module does not model must not reject the document.
        assert!(allows(XSD, "string", &p("pattern", "[0-9]+"), "abc"));
    }

    #[test]
    fn unknown_types_and_libraries_accept() {
        assert!(allows(XSD, "someTypeWeDoNotModel", &[], "anything"));
        assert!(allows("urn:other:library", "whatever", &[], "anything"));
        // The built-in library has exactly two types.
        assert!(allows("", "string", &[], "x"));
        assert!(allows("", "token", &[], " x "));
    }
}
