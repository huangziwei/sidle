//! What the validator knows about CSS.

/// A place a stylesheet stops making sense — epubcheck's `CSS-008`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    /// 1-based line of the construct that is never closed.
    pub line: u32,
    pub message: String,
}

/// A declaration an EPUB style sheet must not carry — epubcheck's `CSS-001`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForbiddenProperty {
    pub line: u32,
    pub name: String,
}

/// The properties `CSSHandler` rejects outright in EPUB 3. Both express writing
/// direction, which EPUB 3 reserves to the content document's `dir`/`xml:lang`
/// so a reading system can honour it — a stylesheet may not override it.
const FORBIDDEN_EPUB3_PROPERTIES: &[&str] = &["direction", "unicode-bidi"];

/// The unterminated constructs in a stylesheet.
pub fn syntax_errors(css: &str) -> Vec<SyntaxError> {
    let mut out = Vec::new();
    let mut open: Vec<(char, u32)> = Vec::new();
    let mut scan = Scanner::new(css);
    while let Some(token) = scan.next_token() {
        match token {
            Token::Open(c, line) => open.push((c, line)),
            Token::Close(c) => {
                // A close with nothing open, or closing the wrong bracket, is
                if let Some(i) = open.iter().rposition(|(o, _)| matching(*o) == c) {
                    open.truncate(i);
                }
            }
            Token::Unterminated(what, line) => out.push(SyntaxError {
                line,
                message: format!("unterminated {what}"),
            }),
        }
    }
    for (c, line) in open {
        out.push(SyntaxError {
            line,
            message: format!("unterminated block: {c:?} is never closed"),
        });
    }
    out.sort_by_key(|e| e.line);
    out
}

/// The `CSS-001` declarations in a stylesheet: a property name at the start of
/// a declaration, at block depth, that EPUB 3 forbids.
pub fn forbidden_properties(css: &str) -> Vec<ForbiddenProperty> {
    let mut out = Vec::new();
    for (name, line) in Scanner::new(css).declarations() {
        if let Some(found) = FORBIDDEN_EPUB3_PROPERTIES
            .iter()
            .find(|p| name.eq_ignore_ascii_case(p))
        {
            out.push(ForbiddenProperty {
                line,
                name: (*found).to_string(),
            });
        }
    }
    out
}

/// The `CSS-001` and `CSS-008` findings in a **declaration list** — the content
/// of a `style=""` attribute, which is not a stylesheet: it has no selectors and
/// no blocks, just `name: value` components separated by `;`.
pub fn declaration_list_errors(css: &str) -> (Vec<SyntaxError>, Vec<ForbiddenProperty>) {
    let mut syntax = Vec::new();
    for component in Scanner::new(css).declaration_list() {
        let text = component.trim();
        if !text.is_empty() && !text.contains(':') {
            syntax.push(SyntaxError {
                line: 1, // a style attribute is one place; the element names it
                message: format!("{text:?} is not a declaration (no property/value separator)"),
            });
        }
    }
    (syntax, forbidden_properties(&format!("*{{{css}}}")))
}

/// The encoding a CSS resource declares, lowercased — from a byte-order mark,
/// else from a leading `@charset "…";` rule. `None` means it declares none,
/// which is UTF-8 by definition and never reported.
pub fn declared_charset(bytes: &[u8]) -> Option<String> {
    for (bom, name) in [
        (b"\xEF\xBB\xBF".as_slice(), "utf-8"),
        (b"\xFF\xFE\x00\x00".as_slice(), "utf-32le"),
        (b"\x00\x00\xFE\xFF".as_slice(), "utf-32be"),
        (b"\xFF\xFE".as_slice(), "utf-16le"),
        (b"\xFE\xFF".as_slice(), "utf-16be"),
    ] {
        if bytes.starts_with(bom) {
            return Some(name.to_string());
        }
    }
    let rest = bytes.strip_prefix(b"@charset \"")?;
    let end = rest.iter().position(|&b| b == b'"')?;
    let name = std::str::from_utf8(&rest[..end]).ok()?;
    // The rule is only a charset declaration if it is actually terminated.
    match rest.get(end + 1) == Some(&b';') {
        true => Some(name.to_ascii_lowercase()),
        false => None,
    }
}

/// Remove CSS `/* … */` comments so a commented-out `url()`/`@import` never
/// counts as a reference (an unterminated comment swallows the rest, as a CSS
/// parser would). Comments do not nest in CSS.
pub fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => return out, // unterminated comment → rest is all comment
        }
    }
    out.push_str(rest);
    out
}

/// Case-insensitive byte search for the ASCII keyword `kw` in `hay`, returning
/// its byte offset. `kw` is ASCII, so the returned offset is a char boundary.
pub fn find_ascii_ci(hay: &str, kw: &[u8]) -> Option<usize> {
    let hb = hay.as_bytes();
    if kw.is_empty() || hb.len() < kw.len() {
        return None;
    }
    (0..=hb.len() - kw.len()).find(|&i| hb[i..i + kw.len()].eq_ignore_ascii_case(kw))
}

/// Every raw `url(...)` / `@import` target in a CSS resource, unclassified —
/// comments stripped, one layer of quotes removed, empties dropped.
pub fn url_tokens(css_raw: &str) -> Vec<String> {
    url_tokens_with_empties(css_raw).0
}

/// The `url()`/`@import` targets of a stylesheet plus the number of *empty*
/// ones — `url()` with nothing inside is CSS-002 ("empty or NULL reference"),
/// which the reference passes cannot see once the token is dropped.
pub fn url_tokens_with_empties(css_raw: &str) -> (Vec<String>, usize) {
    let css = strip_comments(css_raw);
    let mut out = Vec::new();
    let mut empties = 0;
    // url( … ) — also covers `@import url(…)`.
    let mut from = 0;
    while let Some(rel) = find_ascii_ci(&css[from..], b"url(") {
        let open = from + rel + 4;
        let Some(close_rel) = css[open..].find(')') else {
            break;
        };
        let tok = unquote(css[open..open + close_rel].trim()).trim();
        if tok.is_empty() {
            empties += 1;
        } else {
            out.push(tok.to_string());
        }
        from = open + close_rel + 1;
    }
    // @import "…" (the string form; the `@import url(…)` form is caught above).
    let mut from = 0;
    while let Some(rel) = find_ascii_ci(&css[from..], b"@import") {
        let after = &css[from + rel + 7..];
        let seg = after.trim_start();
        if let Some(quote @ ('"' | '\'')) = seg.chars().next()
            && let Some(end) = seg[1..].find(quote)
        {
            let tok = seg[1..1 + end].trim();
            if !tok.is_empty() {
                out.push(tok.to_string());
            }
        }
        from = from + rel + 7;
    }
    (out, empties)
}

/// The `url(...)` targets that appear inside `@font-face` rules — the CSS
pub fn font_face_url_tokens(css_raw: &str) -> std::collections::HashSet<String> {
    let css = strip_comments(css_raw);
    let mut out = std::collections::HashSet::new();
    let mut from = 0;
    while let Some(rel) = find_ascii_ci(&css[from..], b"@font-face") {
        let after = from + rel + "@font-face".len();
        let Some(open_rel) = css[after..].find('{') else {
            break;
        };
        let open = after + open_rel + 1;
        let close = css[open..].find('}').map_or(css.len(), |c| open + c);
        for tok in url_tokens(&css[open..close]) {
            out.insert(tok);
        }
        from = close;
    }
    out
}

/// Strip one layer of matching `"`/`'` quotes from a CSS token, if present.
pub fn unquote(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

// ------------------------------------------------------------------ scanning

/// What [`Scanner`] reports: the bracket structure and anything left open.
enum Token {
    Open(char, u32),
    Close(char),
    Unterminated(&'static str, u32),
}

fn matching(open: char) -> char {
    match open {
        '{' => '}',
        '[' => ']',
        _ => ')',
    }
}

/// A CSS lexical scanner: enough of the tokenizer to know where a construct
/// begins and ends, and nothing more. Comments, strings and `url()` bodies are
/// skipped as units so a brace inside one never counts as structure.
struct Scanner<'a> {
    css: &'a str,
    /// Byte offset of the next unread character.
    at: usize,
    /// 1-based line of `at`, maintained as the scan advances.
    line: u32,
}

impl<'a> Scanner<'a> {
    fn new(css: &'a str) -> Scanner<'a> {
        Scanner {
            css,
            at: 0,
            line: 1,
        }
    }

    /// Advance past `n` bytes, counting the newlines crossed.
    fn advance(&mut self, n: usize) {
        let end = (self.at + n).min(self.css.len());
        self.line += self.css[self.at..end].matches('\n').count() as u32;
        self.at = end;
    }

    /// The next structural token, skipping over everything that is not one.
    fn next_token(&mut self) -> Option<Token> {
        loop {
            let rest = &self.css[self.at..];
            let mut chars = rest.chars();
            let c = chars.next()?;
            match c {
                '/' if rest.starts_with("/*") => {
                    let line = self.line;
                    match rest[2..].find("*/") {
                        Some(end) => self.advance(2 + end + 2),
                        None => {
                            self.advance(rest.len());
                            return Some(Token::Unterminated("comment", line));
                        }
                    }
                }
                '"' | '\'' => {
                    let line = self.line;
                    match self.string_len(rest, c) {
                        Some(len) => self.advance(len),
                        None => {
                            self.advance(rest.len());
                            return Some(Token::Unterminated("string", line));
                        }
                    }
                }
                '\\' => self.advance(c.len_utf8() + chars.next().map_or(0, char::len_utf8)),
                '{' | '[' | '(' => {
                    let line = self.line;
                    self.advance(1);
                    return Some(Token::Open(c, line));
                }
                '}' | ']' | ')' => {
                    self.advance(1);
                    return Some(Token::Close(c));
                }
                _ => self.advance(c.len_utf8()),
            }
        }
    }

    /// The byte length of the string literal starting at `rest[0] == quote`,
    /// including both quotes, or `None` if it is never closed. A CSS string may
    /// not span a raw newline, which also ends it unterminated.
    fn string_len(&self, rest: &str, quote: char) -> Option<usize> {
        let mut escaped = false;
        for (i, c) in rest.char_indices().skip(1) {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '\n' => return None,
                _ if c == quote => return Some(i + c.len_utf8()),
                _ => {}
            }
        }
        None
    }

    /// The `;`-separated components of a declaration list, with separators
    /// inside strings, comments and brackets left alone.
    fn declaration_list(mut self) -> Vec<String> {
        let mut out = Vec::new();
        let mut start = 0usize;
        let mut depth = 0usize;
        loop {
            let rest = &self.css[self.at..];
            let Some(c) = rest.chars().next() else { break };
            match c {
                '/' if rest.starts_with("/*") => match rest[2..].find("*/") {
                    Some(end) => self.advance(2 + end + 2),
                    None => break,
                },
                '"' | '\'' => match self.string_len(rest, c) {
                    Some(len) => self.advance(len),
                    None => break,
                },
                '{' | '[' | '(' => {
                    depth += 1;
                    self.advance(1);
                }
                '}' | ']' | ')' => {
                    depth = depth.saturating_sub(1);
                    self.advance(1);
                }
                ';' if depth == 0 => {
                    out.push(self.css[start..self.at].to_string());
                    self.advance(1);
                    start = self.at;
                }
                _ => self.advance(c.len_utf8()),
            }
        }
        out.push(self.css[start..].to_string());
        out
    }

    /// Every `name` that begins a declaration — a name token followed by `:`,
    /// at a point where a declaration can start — with the line it is on.
    fn declarations(mut self) -> Vec<(String, u32)> {
        let mut out = Vec::new();
        // A declaration may only start at the beginning of a block or right
        // after a `;`, which is what keeps a selector like `a[dir]:hover` and a
        // value like `background: url(a:b)` out.
        let mut can_start = false;
        let mut depth = 0usize;
        loop {
            let rest = &self.css[self.at..];
            let Some(c) = rest.chars().next() else { break };
            match c {
                '/' if rest.starts_with("/*") => match rest[2..].find("*/") {
                    Some(end) => self.advance(2 + end + 2),
                    None => break,
                },
                '"' | '\'' => match self.string_len(rest, c) {
                    Some(len) => self.advance(len),
                    None => break,
                },
                '{' => {
                    depth += 1;
                    can_start = true;
                    self.advance(1);
                }
                '}' => {
                    depth = depth.saturating_sub(1);
                    can_start = true;
                    self.advance(1);
                }
                ';' => {
                    can_start = true;
                    self.advance(1);
                }
                c if c.is_whitespace() => self.advance(c.len_utf8()),
                _ if can_start && depth > 0 && is_name_start(c) => {
                    let line = self.line;
                    let len = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
                    let name = &rest[..len];
                    self.advance(len);
                    // Only a `:` immediately after the name (whitespace aside)
                    // makes this a declaration rather than a selector.
                    let after = self.css[self.at..].trim_start();
                    if after.starts_with(':') {
                        out.push((name.to_string(), line));
                    }
                    can_start = false;
                }
                _ => {
                    can_start = false;
                    self.advance(c.len_utf8());
                }
            }
        }
        out
    }
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '-' || c == '_' || !c.is_ascii()
}

fn is_name_char(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errors(css: &str) -> Vec<String> {
        syntax_errors(css)
            .into_iter()
            .map(|e| format!("{}:{}", e.line, e.message))
            .collect()
    }

    /// epubcheck's own `CSS-008` stylesheet fixture, verbatim: two rules whose
    /// closing brace is missing, reported twice.
    #[test]
    fn an_unclosed_block_is_a_syntax_error() {
        let css = "body {\n  margin-left: 6em;\n  color: black;\n/* missing closing brace */\n\np {\n  font-size: inherit;\n}\n\n\nh4 {\n  font-size: inherit;\n/* missing closing brace */\n";
        let found = errors(css);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].starts_with("1:"), "{found:?}");
        assert!(found[1].starts_with("11:"), "{found:?}");
    }

    #[test]
    fn a_well_formed_stylesheet_has_no_syntax_error() {
        let css = r#"
@charset "utf-8";
@media screen and (min-width: 30em) {
  body { color: black; content: "}"; background: url(a}b.png); }
}
@font-face { font-family: "X"; src: url('f.otf'); }
p::before { content: '\'' }
a[href$="{"] { color: red }
"#;
        assert!(errors(css).is_empty(), "{:?}", errors(css));
    }

    #[test]
    fn an_unterminated_string_or_comment_is_reported() {
        assert!(
            errors("p { content: \"open\n}")
                .iter()
                .any(|e| e.contains("string"))
        );
        assert!(
            errors("p { color: red }\n/* open")
                .iter()
                .any(|e| e.contains("comment"))
        );
        // A stray close brace is recovered from, not reported — CSS defines
        // that recovery and real stylesheets end with one often enough.
        assert!(errors("p { color: red }}").is_empty());
    }

    #[test]
    fn forbidden_properties_are_declarations_not_selectors_or_values() {
        let css = r#"
p { direction: rtl; unicode-bidi: embed }
.direction { color: red }
a[dir="direction"] { content: "unicode-bidi"; background: url(direction:x) }
q { quotes: none; /* direction: rtl */ }
"#;
        let found = forbidden_properties(css);
        assert_eq!(
            found.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            ["direction", "unicode-bidi"],
            "{found:?}"
        );
        assert_eq!(found[0].line, 2);
    }

    /// A `style=""` attribute is a declaration list, not a stylesheet: it has
    /// no braces to leave unclosed, and the error epubcheck's own fixture
    /// carries (`style="blue"`) is a component with no `name: value` split.
    #[test]
    fn a_declaration_list_is_judged_as_one() {
        let bad = |css: &str| declaration_list_errors(css).0.len();
        assert_eq!(bad("blue"), 1);
        assert_eq!(bad("color: red; blue"), 1);
        assert_eq!(bad("color: red"), 0);
        assert_eq!(bad("color: red;"), 0, "a trailing ; is legal");
        assert_eq!(bad(""), 0);
        assert_eq!(bad("  ;; "), 0, "empty components are legal");
        assert_eq!(
            bad(r#"background: url(a;b.png); font-family: "a;b""#),
            0,
            "a ; inside a url() or a string does not split the list"
        );
        // CSS-001 applies to a style attribute too.
        let (_, forbidden) = declaration_list_errors("color: red; direction: rtl");
        assert_eq!(forbidden.len(), 1);
        assert_eq!(forbidden[0].name, "direction");
    }

    #[test]
    fn a_charset_is_read_from_a_bom_or_a_leading_rule() {
        assert_eq!(declared_charset(b"p{}"), None);
        assert_eq!(
            declared_charset(b"@charset \"ISO-8859-1\";\np{}").as_deref(),
            Some("iso-8859-1")
        );
        assert_eq!(
            declared_charset(b"\xEF\xBB\xBFp{}").as_deref(),
            Some("utf-8")
        );
        assert_eq!(
            declared_charset(b"\xFE\xFF\x00p").as_deref(),
            Some("utf-16be")
        );
        // Only the very first bytes count — CSS Syntax §3.2.
        assert_eq!(declared_charset(b"\n@charset \"x\";"), None);
        assert_eq!(declared_charset(b"@charset \"x\" p{}"), None);
    }

    #[test]
    fn url_extraction_keeps_working_after_the_move() {
        let (urls, empties) =
            url_tokens_with_empties("a{background:url()}b{src:url( )}c{d:url(x.png)}");
        assert_eq!(urls, ["x.png"]);
        assert_eq!(empties, 2);
        assert_eq!(strip_comments("a/*b*/c"), "ac");
        assert_eq!(unquote("\"x\""), "x");
    }
}
