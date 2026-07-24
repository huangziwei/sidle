//! Translate a RELAX NG grammar written in the **compact syntax** (`.rnc`) into
//! the XML syntax that [`super::rng`] compiles.
//!
//! Every grammar epubcheck ships for EPUB 3 is compact — the package document,
//! the content documents, the navigation document, media overlays, the OCF
//! files, and the whole HTML5 module set behind them — so without reading it
//! there is no EPUB 3 validation at all.
//!
//! The two syntaxes describe exactly the same schema language; the compact one
//! adds only notation (infix `,`/`&`/`|` for group/interleave/choice, postfix
//! `?`/`*`/`+`, prefix declarations in place of `xmlns`). Translating instead of
//! compiling directly keeps **one** compiler and therefore one set of semantics:
//! a rule about `include` overriding, `combine`, or datatype inheritance cannot
//! drift between the two syntaxes, because only [`super::rng`] implements it.
//!
//! The output is deliberately *fully explicit* — every name carries its own `ns`
//! and every datatype its own `datatypeLibrary` — so no prefix binding survives
//! the translation and nothing downstream has to know the compact syntax exists.
//! The one thing left implicit is a namespace the file does not declare, which
//! must stay inherited from whatever `include`s it (`inherit`, below).

use std::collections::HashMap;

use super::rng::{CompileError, RNG_NS};

/// The library the `xsd` datatype prefix is bound to with no declaration.
const XSD: &str = "http://www.w3.org/2001/XMLSchema-datatypes";
/// The namespace the `xml` prefix is bound to with no declaration.
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// Translate compact syntax to the equivalent XML syntax.
///
/// `path` is used only in error messages.
pub fn translate(path: &str, source: &str) -> Result<String, CompileError> {
    let mut parser = Parser::new(path, lex(path, source)?);
    parser.declarations()?;
    parser.document()
}

// ---------------------------------------------------------------- lexer

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// An NCName. Whether it is a keyword is left to the parser, because a
    /// keyword is a perfectly good *name*: `element text { … }` is legal.
    Name(String),
    /// `\text` — a name that is never read as a keyword.
    Escaped(String),
    /// `prefix:local`.
    CName(String, String),
    /// `prefix:*`.
    NsName(String),
    Literal(String),
    /// `=`, `|=`, `&=`.
    Assign,
    OrAssign,
    AndAssign,
    Comma,
    Amp,
    Pipe,
    Minus,
    Star,
    Plus,
    Question,
    Tilde,
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    /// `>>`, which introduces a following annotation.
    Follow,
    Eof,
}

fn is_name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | '\u{b7}')
}

fn lex(path: &str, source: &str) -> Result<Vec<(Tok, u32)>, CompileError> {
    let src = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut out: Vec<(Tok, u32)> = Vec::new();
    let mut i = 0usize;
    let mut line = 1u32;
    let err = |line: u32, msg: &str| CompileError(format!("{path}:{line}: {msg}"));

    while i < src.len() {
        let c = src[i..].chars().next().expect("in bounds");
        // Whitespace and comments. `##` documentation comments are comments too;
        // nothing downstream has any use for them.
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        if c == '#' {
            while i < src.len() && !src[i..].starts_with('\n') {
                i += src[i..].chars().next().expect("in bounds").len_utf8();
            }
            continue;
        }

        // String literals, which may be triple-quoted and may span lines.
        if c == '"' || c == '\'' {
            let (value, next, lines) =
                lex_literal(src, i, c).ok_or_else(|| err(line, "unterminated literal"))?;
            out.push((Tok::Literal(value), line));
            line += lines;
            i = next;
            continue;
        }

        // A name, possibly a CName or an nsName.
        if is_name_start(c) {
            let start = i;
            while i < src.len() {
                let ch = src[i..].chars().next().expect("in bounds");
                if !is_name_char(ch) {
                    break;
                }
                i += ch.len_utf8();
            }
            let name = &src[start..i];
            if src[i..].starts_with(":*") {
                i += 2;
                out.push((Tok::NsName(name.to_string()), line));
            } else if src[i..].starts_with(':')
                && src[i + 1..].chars().next().is_some_and(is_name_start)
            {
                i += 1;
                let local_start = i;
                while i < src.len() {
                    let ch = src[i..].chars().next().expect("in bounds");
                    if !is_name_char(ch) {
                        break;
                    }
                    i += ch.len_utf8();
                }
                out.push((
                    Tok::CName(name.to_string(), src[local_start..i].to_string()),
                    line,
                ));
            } else {
                out.push((Tok::Name(name.to_string()), line));
            }
            continue;
        }

        // `\name` — an identifier that shadows a keyword.
        if c == '\\' {
            i += 1;
            let start = i;
            while i < src.len() {
                let ch = src[i..].chars().next().expect("in bounds");
                if !is_name_char(ch) {
                    break;
                }
                i += ch.len_utf8();
            }
            if start == i {
                return Err(err(line, "`\\` must be followed by a name"));
            }
            out.push((Tok::Escaped(src[start..i].to_string()), line));
            continue;
        }

        let (tok, width) = match c {
            '=' => (Tok::Assign, 1),
            '|' if src[i..].starts_with("|=") => (Tok::OrAssign, 2),
            '|' => (Tok::Pipe, 1),
            '&' if src[i..].starts_with("&=") => (Tok::AndAssign, 2),
            '&' => (Tok::Amp, 1),
            '>' if src[i..].starts_with(">>") => (Tok::Follow, 2),
            ',' => (Tok::Comma, 1),
            '-' => (Tok::Minus, 1),
            '*' => (Tok::Star, 1),
            '+' => (Tok::Plus, 1),
            '?' => (Tok::Question, 1),
            '~' => (Tok::Tilde, 1),
            '{' => (Tok::LBrace, 1),
            '}' => (Tok::RBrace, 1),
            '(' => (Tok::LParen, 1),
            ')' => (Tok::RParen, 1),
            '[' => (Tok::LBracket, 1),
            ']' => (Tok::RBracket, 1),
            other => return Err(err(line, &format!("unexpected character {other:?}"))),
        };
        out.push((tok, line));
        i += width;
    }
    out.push((Tok::Eof, line));
    Ok(out)
}

/// Scan one literal starting at `i` (which holds the opening quote). Returns the
/// value, the index just past the closing quote, and how many newlines it spans.
fn lex_literal(src: &str, i: usize, quote: char) -> Option<(String, usize, u32)> {
    let triple: String = std::iter::repeat_n(quote, 3).collect();
    let (open, close) = if src[i..].starts_with(&triple) {
        (triple.clone(), triple)
    } else {
        (quote.to_string(), quote.to_string())
    };
    let mut pos = i + open.len();
    let mut value = String::new();
    let mut lines = 0;
    loop {
        if pos >= src.len() {
            return None;
        }
        if src[pos..].starts_with(&close) {
            return Some((value, pos + close.len(), lines));
        }
        // `\x{hhhh}` is the compact syntax's only escape.
        if src[pos..].starts_with("\\x{") {
            let end = src[pos..].find('}')? + pos;
            let code = u32::from_str_radix(&src[pos + 3..end], 16).ok()?;
            value.push(char::from_u32(code)?);
            pos = end + 1;
            continue;
        }
        let ch = src[pos..].chars().next()?;
        if ch == '\n' {
            lines += 1;
        }
        value.push(ch);
        pos += ch.len_utf8();
    }
}

// ---------------------------------------------------------------- parser

struct Parser<'a> {
    path: &'a str,
    toks: Vec<(Tok, u32)>,
    pos: usize,
    /// The namespace unprefixed *element* names take, when the file declares
    /// one. `None` means it is inherited from whatever includes this file, which
    /// the translation preserves by leaving `ns` off the names it emits.
    default_ns: Option<String>,
    prefixes: HashMap<String, String>,
    datatypes: HashMap<String, String>,
}

impl<'a> Parser<'a> {
    fn new(path: &'a str, toks: Vec<(Tok, u32)>) -> Self {
        Parser {
            path,
            toks,
            pos: 0,
            default_ns: None,
            prefixes: HashMap::from([("xml".to_string(), XML_NS.to_string())]),
            datatypes: HashMap::from([("xsd".to_string(), XSD.to_string())]),
        }
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)].0
    }

    fn peek_at(&self, ahead: usize) -> &Tok {
        &self.toks[(self.pos + ahead).min(self.toks.len() - 1)].0
    }

    fn line(&self) -> u32 {
        self.toks[self.pos.min(self.toks.len() - 1)].1
    }

    fn bump(&mut self) -> Tok {
        let tok = self.toks[self.pos.min(self.toks.len() - 1)].0.clone();
        self.pos = (self.pos + 1).min(self.toks.len() - 1);
        tok
    }

    fn err<T>(&self, msg: impl AsRef<str>) -> Result<T, CompileError> {
        Err(CompileError(format!(
            "{}:{}: {}",
            self.path,
            self.line(),
            msg.as_ref()
        )))
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.peek() == tok {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: &Tok) -> Result<(), CompileError> {
        if self.eat(tok) {
            Ok(())
        } else {
            self.err(format!("expected {tok:?}, found {:?}", self.peek()))
        }
    }

    /// Is the next token this keyword? A keyword is only a keyword unescaped.
    fn at_keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Tok::Name(n) if n == word)
    }

    /// `[ … ]` annotations carry no schema meaning; skip them wherever the
    /// syntax allows one. Brackets nest, and literals are already tokens, so
    /// counting brackets is exact.
    fn skip_annotations(&mut self) {
        while matches!(self.peek(), Tok::LBracket) {
            let mut depth = 0usize;
            loop {
                match self.bump() {
                    Tok::LBracket => depth += 1,
                    Tok::RBracket => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    Tok::Eof => return,
                    _ => {}
                }
            }
        }
    }

    /// `>> name [ … ]` — an annotation that follows what it annotates.
    fn skip_follow_annotation(&mut self) {
        match self.peek() {
            Tok::Name(_) | Tok::CName(..) => {
                self.bump();
            }
            _ => return,
        }
        self.skip_annotations();
    }

    // -------------------------------------------------------- declarations

    /// `namespace` / `default namespace` / `datatypes`, which must all precede
    /// the grammar.
    fn declarations(&mut self) -> Result<(), CompileError> {
        loop {
            self.skip_annotations();
            if self.at_keyword("default")
                && matches!(self.peek_at(1), Tok::Name(n) if n == "namespace")
            {
                self.bump();
                self.bump();
                let prefix = match self.peek() {
                    Tok::Name(n) => {
                        let n = n.clone();
                        self.bump();
                        Some(n)
                    }
                    _ => None,
                };
                self.expect(&Tok::Assign)?;
                let uri = self.namespace_uri()?;
                if let Some(p) = prefix {
                    self.prefixes.insert(p, uri.clone());
                }
                self.default_ns = Some(uri);
            } else if self.at_keyword("namespace") {
                self.bump();
                let Tok::Name(prefix) = self.bump() else {
                    return self.err("expected a prefix after `namespace`");
                };
                self.expect(&Tok::Assign)?;
                let uri = self.namespace_uri()?;
                self.prefixes.insert(prefix, uri);
            } else if self.at_keyword("datatypes") {
                self.bump();
                let Tok::Name(prefix) = self.bump() else {
                    return self.err("expected a prefix after `datatypes`");
                };
                self.expect(&Tok::Assign)?;
                let uri = self.literal()?;
                self.datatypes.insert(prefix, uri);
            } else {
                return Ok(());
            }
        }
    }

    /// The right-hand side of a namespace declaration: a literal, or `inherit`.
    fn namespace_uri(&mut self) -> Result<String, CompileError> {
        if self.at_keyword("inherit") {
            // Legal syntax, but it binds a prefix to a namespace this file does
            // not know, which the fully-explicit output cannot express. No
            // grammar epubcheck ships uses it.
            return self.err("`= inherit` in a namespace declaration is not supported");
        }
        self.literal()
    }

    /// A literal, including the `~` concatenation of adjacent segments.
    fn literal(&mut self) -> Result<String, CompileError> {
        let Tok::Literal(mut value) = self.bump() else {
            return self.err("expected a literal");
        };
        while self.eat(&Tok::Tilde) {
            let Tok::Literal(next) = self.bump() else {
                return self.err("expected a literal after `~`");
            };
            value.push_str(&next);
        }
        Ok(value)
    }

    fn namespace_of(&self, prefix: &str) -> Result<String, CompileError> {
        match self.prefixes.get(prefix) {
            Some(uri) => Ok(uri.clone()),
            None => self.err(format!("undeclared namespace prefix {prefix:?}")),
        }
    }

    fn datatype_library_of(&self, prefix: &str) -> Result<String, CompileError> {
        match self.datatypes.get(prefix) {
            Some(uri) => Ok(uri.clone()),
            None => self.err(format!("undeclared datatype prefix {prefix:?}")),
        }
    }

    // -------------------------------------------------------- top level

    /// A compact-syntax file is either a sequence of grammar content or a single
    /// pattern. Both become a `<grammar>`: wrapping a bare pattern in a
    /// `<start>` says the same thing and spares the caller a special case.
    fn document(&mut self) -> Result<String, CompileError> {
        self.skip_annotations();
        let body = if self.starts_grammar_content() {
            self.grammar_content(&Tok::Eof)?
        } else {
            let pattern = self.pattern()?;
            elem("start", &pattern)
        };
        if !matches!(self.peek(), Tok::Eof) {
            return self.err(format!("unexpected {:?} after the grammar", self.peek()));
        }
        let ns = match &self.default_ns {
            Some(uri) => format!(" ns=\"{}\"", esc(uri)),
            None => String::new(),
        };
        Ok(format!("<grammar xmlns=\"{RNG_NS}\"{ns}>{body}</grammar>"))
    }

    fn starts_grammar_content(&self) -> bool {
        if self.at_keyword("start") || self.at_keyword("div") || self.at_keyword("include") {
            return true;
        }
        // `name = pattern` — a definition. Anything else beginning with a name
        // is a pattern (a `ref`, a datatype, `element`, …).
        matches!(self.peek(), Tok::Name(_) | Tok::Escaped(_))
            && matches!(
                self.peek_at(1),
                Tok::Assign | Tok::OrAssign | Tok::AndAssign
            )
    }

    /// `start`/`define`/`div`/`include`, up to `terminator`.
    fn grammar_content(&mut self, terminator: &Tok) -> Result<String, CompileError> {
        let mut out = String::new();
        loop {
            self.skip_annotations();
            if self.peek() == terminator || matches!(self.peek(), Tok::Eof) {
                return Ok(out);
            }
            if self.at_keyword("div") {
                self.bump();
                self.expect(&Tok::LBrace)?;
                let inner = self.grammar_content(&Tok::RBrace)?;
                self.expect(&Tok::RBrace)?;
                out.push_str(&elem("div", &inner));
                continue;
            }
            if self.at_keyword("include") {
                self.bump();
                out.push_str(&self.include()?);
                continue;
            }
            let name = if self.at_keyword("start") {
                self.bump();
                None
            } else {
                match self.bump() {
                    Tok::Name(n) | Tok::Escaped(n) => Some(n),
                    other => return self.err(format!("expected a definition, found {other:?}")),
                }
            };
            let combine = match self.bump() {
                Tok::Assign => "",
                Tok::OrAssign => " combine=\"choice\"",
                Tok::AndAssign => " combine=\"interleave\"",
                other => return self.err(format!("expected `=`, `|=` or `&=`, found {other:?}")),
            };
            let body = self.pattern()?;
            out.push_str(&match name {
                None => elem_attr("start", combine, &body),
                Some(n) => elem_attr("define", &format!(" name=\"{}\"{combine}", esc(&n)), &body),
            });
        }
    }

    /// `include "href" [inherit = prefix] [{ overrides }]`.
    fn include(&mut self) -> Result<String, CompileError> {
        let href = self.literal()?;
        let ns = self.inherit_clause()?;
        let body = if self.eat(&Tok::LBrace) {
            let inner = self.grammar_content(&Tok::RBrace)?;
            self.expect(&Tok::RBrace)?;
            inner
        } else {
            String::new()
        };
        Ok(elem_attr(
            "include",
            &format!(" href=\"{}\"{ns}", esc(&href)),
            &body,
        ))
    }

    /// The `inherit = prefix` clause of an `include`/`external`, as an `ns`
    /// attribute. Without it the namespace in scope is inherited, which is what
    /// omitting `ns` already means.
    fn inherit_clause(&mut self) -> Result<String, CompileError> {
        if !self.at_keyword("inherit") {
            return Ok(String::new());
        }
        self.bump();
        self.expect(&Tok::Assign)?;
        let Tok::Name(prefix) = self.bump() else {
            return self.err("expected a prefix after `inherit =`");
        };
        let uri = self.namespace_of(&prefix)?;
        Ok(format!(" ns=\"{}\"", esc(&uri)))
    }

    // -------------------------------------------------------- patterns

    /// A pattern, including the infix operators. They share one precedence level
    /// and the compact syntax forbids mixing them without parentheses, so a
    /// mixture is a grammar error rather than a silent reinterpretation.
    fn pattern(&mut self) -> Result<String, CompileError> {
        let first = self.unary()?;
        let (wrapper, separator) = match self.peek() {
            Tok::Comma => ("group", Tok::Comma),
            Tok::Amp => ("interleave", Tok::Amp),
            Tok::Pipe => ("choice", Tok::Pipe),
            _ => return Ok(first),
        };
        let mut body = first;
        while self.eat(&separator) {
            body.push_str(&self.unary()?);
        }
        if matches!(self.peek(), Tok::Comma | Tok::Amp | Tok::Pipe) {
            return self.err("`,`, `&` and `|` cannot be mixed without parentheses");
        }
        Ok(elem(wrapper, &body))
    }

    /// A primary with its postfix `?`, `*` and `+`.
    fn unary(&mut self) -> Result<String, CompileError> {
        self.skip_annotations();
        let mut pattern = self.primary()?;
        loop {
            match self.peek() {
                Tok::Question => {
                    self.bump();
                    pattern = elem("optional", &pattern);
                }
                Tok::Star => {
                    self.bump();
                    pattern = elem("zeroOrMore", &pattern);
                }
                Tok::Plus => {
                    self.bump();
                    pattern = elem("oneOrMore", &pattern);
                }
                Tok::Follow => {
                    self.bump();
                    self.skip_follow_annotation();
                }
                _ => return Ok(pattern),
            }
        }
    }

    fn primary(&mut self) -> Result<String, CompileError> {
        match self.peek().clone() {
            Tok::Name(word) if word == "element" || word == "attribute" => {
                self.bump();
                let is_attribute = word == "attribute";
                let name_class = self.name_class(is_attribute)?;
                self.expect(&Tok::LBrace)?;
                let content = self.pattern()?;
                self.expect(&Tok::RBrace)?;
                Ok(elem(&word, &format!("{name_class}{content}")))
            }
            Tok::Name(word) if word == "list" || word == "mixed" => {
                self.bump();
                self.expect(&Tok::LBrace)?;
                let content = self.pattern()?;
                self.expect(&Tok::RBrace)?;
                Ok(elem(&word, &content))
            }
            Tok::Name(word) if word == "grammar" => {
                self.bump();
                self.expect(&Tok::LBrace)?;
                let content = self.grammar_content(&Tok::RBrace)?;
                self.expect(&Tok::RBrace)?;
                Ok(elem("grammar", &content))
            }
            Tok::Name(word) if word == "external" => {
                self.bump();
                let href = self.literal()?;
                let ns = self.inherit_clause()?;
                Ok(empty_elem(
                    "externalRef",
                    &format!(" href=\"{}\"{ns}", esc(&href)),
                ))
            }
            Tok::Name(word) if word == "parent" => {
                self.bump();
                match self.bump() {
                    Tok::Name(n) | Tok::Escaped(n) => {
                        Ok(empty_elem("parentRef", &format!(" name=\"{}\"", esc(&n))))
                    }
                    other => self.err(format!("expected a name after `parent`, found {other:?}")),
                }
            }
            Tok::Name(word) if word == "empty" || word == "text" || word == "notAllowed" => {
                self.bump();
                Ok(empty_elem(&word, ""))
            }
            // A datatype: `string`/`token` from the built-in library, or a
            // prefixed name from a declared one. Either may carry a value, a
            // parameter list, or an `except`.
            Tok::Name(word) if word == "string" || word == "token" => {
                self.bump();
                self.datatype(String::new(), word)
            }
            Tok::CName(prefix, local) => {
                self.bump();
                let library = self.datatype_library_of(&prefix)?;
                self.datatype(library, local)
            }
            // A bare literal is a value in the built-in `token` datatype.
            Tok::Literal(_) => {
                let value = self.literal()?;
                Ok(elem_attr(
                    "value",
                    " datatypeLibrary=\"\" type=\"token\"",
                    &esc(&value),
                ))
            }
            Tok::LParen => {
                self.bump();
                let inner = self.pattern()?;
                self.expect(&Tok::RParen)?;
                Ok(inner)
            }
            Tok::Name(name) | Tok::Escaped(name) => {
                self.bump();
                Ok(empty_elem("ref", &format!(" name=\"{}\"", esc(&name))))
            }
            other => self.err(format!("expected a pattern, found {other:?}")),
        }
    }

    /// The tail of a datatype reference: a value literal, or optional parameters
    /// and an optional `except`.
    fn datatype(&mut self, library: String, name: String) -> Result<String, CompileError> {
        let attrs = format!(
            " datatypeLibrary=\"{}\" type=\"{}\"",
            esc(&library),
            esc(&name)
        );
        if matches!(self.peek(), Tok::Literal(_)) {
            let value = self.literal()?;
            return Ok(elem_attr("value", &attrs, &esc(&value)));
        }
        let mut body = String::new();
        if self.eat(&Tok::LBrace) {
            while !self.eat(&Tok::RBrace) {
                let param = match self.bump() {
                    Tok::Name(n) | Tok::Escaped(n) => n,
                    other => return self.err(format!("expected a parameter, found {other:?}")),
                };
                self.expect(&Tok::Assign)?;
                let value = self.literal()?;
                body.push_str(&elem_attr(
                    "param",
                    &format!(" name=\"{}\"", esc(&param)),
                    &esc(&value),
                ));
            }
        }
        if self.eat(&Tok::Minus) {
            let except = self.primary()?;
            body.push_str(&elem("except", &except));
        }
        Ok(elem_attr("data", &attrs, &body))
    }

    // -------------------------------------------------------- name classes

    fn name_class(&mut self, is_attribute: bool) -> Result<String, CompileError> {
        let mut body = self.name_class_primary(is_attribute)?;
        if !matches!(self.peek(), Tok::Pipe) {
            return Ok(body);
        }
        while self.eat(&Tok::Pipe) {
            body.push_str(&self.name_class_primary(is_attribute)?);
        }
        Ok(elem("choice", &body))
    }

    fn name_class_primary(&mut self, is_attribute: bool) -> Result<String, CompileError> {
        self.skip_annotations();
        match self.bump() {
            Tok::Star => {
                let except = self.name_class_except(is_attribute)?;
                Ok(elem("anyName", &except))
            }
            Tok::NsName(prefix) => {
                let uri = self.namespace_of(&prefix)?;
                let except = self.name_class_except(is_attribute)?;
                Ok(elem_attr(
                    "nsName",
                    &format!(" ns=\"{}\"", esc(&uri)),
                    &except,
                ))
            }
            Tok::CName(prefix, local) => {
                let uri = self.namespace_of(&prefix)?;
                Ok(elem_attr(
                    "name",
                    &format!(" ns=\"{}\"", esc(&uri)),
                    &esc(&local),
                ))
            }
            // An unprefixed name. On an element it takes the default namespace —
            // left implicit when the file does not declare one, so that whatever
            // includes it can supply it. On an attribute it is always in no
            // namespace, which `ns=""` states outright.
            Tok::Name(local) | Tok::Escaped(local) => {
                let ns = if is_attribute {
                    " ns=\"\"".to_string()
                } else {
                    match &self.default_ns {
                        Some(uri) => format!(" ns=\"{}\"", esc(uri)),
                        None => String::new(),
                    }
                };
                Ok(elem_attr("name", &ns, &esc(&local)))
            }
            Tok::LParen => {
                let inner = self.name_class(is_attribute)?;
                self.expect(&Tok::RParen)?;
                Ok(inner)
            }
            other => self.err(format!("expected a name class, found {other:?}")),
        }
    }

    fn name_class_except(&mut self, is_attribute: bool) -> Result<String, CompileError> {
        if !self.eat(&Tok::Minus) {
            return Ok(String::new());
        }
        let inner = self.name_class_primary(is_attribute)?;
        Ok(elem("except", &inner))
    }
}

// ---------------------------------------------------------------- emitting

/// Escape for both text and attribute values; one function cannot be wrong in
/// the other position.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn elem(name: &str, body: &str) -> String {
    format!("<{name}>{body}</{name}>")
}

fn elem_attr(name: &str, attrs: &str, body: &str) -> String {
    format!("<{name}{attrs}>{body}</{name}>")
}

fn empty_elem(name: &str, attrs: &str) -> String {
    format!("<{name}{attrs}/>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::source::epub::xml::relaxng::derive::Validator;
    use crate::validate::source::epub::xml::relaxng::pattern::{Arena, PatternId};
    use crate::validate::source::epub::xml::relaxng::rng::{Compiler, MapResolver};
    use crate::validate::source::epub::xml::tree::Document;

    /// Compile a compact grammar (plus any files it includes) and return its
    /// start pattern.
    fn compile(files: &[(&str, &str)]) -> (Arena, PatternId) {
        let map: HashMap<String, String> = files
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect();
        let mut arena = Arena::new();
        let start = {
            let resolver = MapResolver(&map);
            let mut compiler = Compiler::new(&mut arena, &resolver);
            compiler
                .compile(files[0].0, files[0].1)
                .unwrap_or_else(|e| panic!("{} failed: {e}", files[0].0))
        };
        (arena, start)
    }

    fn valid(arena: &mut Arena, start: PatternId, xml: &str) -> bool {
        let doc = Document::parse(xml).expect("well-formed test document");
        Validator::new(arena).validate(&doc, start).is_empty()
    }

    #[test]
    fn translates_the_infix_operators_and_postfix_sugar() {
        let (mut arena, start) = compile(&[(
            "g.rnc",
            r#"
            default namespace = "urn:x"
            start = element doc { attribute id { text }?, (a & b), c* }
            a = element a { text }
            b = element b { empty }
            c = element c { empty } | element d { empty }
            "#,
        )]);
        assert!(valid(
            &mut arena,
            start,
            r#"<doc xmlns="urn:x" id="1"><b/><a>x</a><d/><c/></doc>"#
        ));
        assert!(
            valid(
                &mut arena,
                start,
                r#"<doc xmlns="urn:x"><a>x</a><b/></doc>"#
            ),
            "the optional attribute and the zeroOrMore may be absent"
        );
        assert!(
            !valid(&mut arena, start, r#"<doc xmlns="urn:x"><a>x</a></doc>"#),
            "interleave still requires both sides"
        );
        assert!(
            !valid(&mut arena, start, r#"<doc><a>x</a><b/></doc>"#),
            "the default namespace applies to element names"
        );
    }

    #[test]
    fn unprefixed_attribute_names_are_in_no_namespace() {
        let (mut arena, start) = compile(&[(
            "g.rnc",
            r#"
            default namespace = "urn:x"
            namespace other = "urn:y"
            start = element doc { attribute id { text }, attribute other:tag { text } }
            "#,
        )]);
        assert!(valid(
            &mut arena,
            start,
            r#"<doc xmlns="urn:x" xmlns:o="urn:y" id="1" o:tag="t"/>"#
        ));
        assert!(
            !valid(
                &mut arena,
                start,
                r#"<doc xmlns="urn:x" xmlns:p="urn:x" xmlns:o="urn:y" p:id="1" o:tag="t"/>"#
            ),
            "`id` is in no namespace, not in the default one"
        );
    }

    #[test]
    fn name_class_wildcards_and_exceptions() {
        let (mut arena, start) = compile(&[(
            "g.rnc",
            r#"
            namespace m = "urn:m"
            start = element doc { anything* }
            anything = element * - (m:* | skipped) { (attribute * { text } | anything | text)* }
            "#,
        )]);
        assert!(valid(&mut arena, start, r#"<doc><x a="1">t<y/></x></doc>"#));
        assert!(
            !valid(&mut arena, start, r#"<doc><skipped/></doc>"#),
            "the except removes that name"
        );
        assert!(
            !valid(
                &mut arena,
                start,
                r#"<doc xmlns:m="urn:m"><m:anything/></doc>"#
            ),
            "the except removes that whole namespace"
        );
    }

    #[test]
    fn datatypes_values_and_parameters() {
        let (mut arena, start) = compile(&[(
            "g.rnc",
            r#"
            start = element doc {
              attribute version { "1.0" | "1.1" },
              attribute id { xsd:NCName },
              attribute n { xsd:string { minLength = "2" } },
              attribute kind { string "keep" }
            }
            "#,
        )]);
        assert!(valid(
            &mut arena,
            start,
            r#"<doc version="1.1" id="a" n="xy" kind="keep"/>"#
        ));
        assert!(!valid(
            &mut arena,
            start,
            r#"<doc version="2.0" id="a" n="xy" kind="keep"/>"#
        ));
        assert!(
            !valid(
                &mut arena,
                start,
                r#"<doc version="1.0" id="1a" n="xy" kind="keep"/>"#
            ),
            "an NCName cannot start with a digit"
        );
        assert!(
            !valid(
                &mut arena,
                start,
                r#"<doc version="1.0" id="a" n="x" kind="keep"/>"#
            ),
            "minLength is enforced"
        );
    }

    #[test]
    fn include_inherits_and_overrides() {
        let (mut arena, start) = compile(&[
            (
                "main.rnc",
                r#"
                namespace svg = "urn:svg"
                default namespace = "urn:main"
                include "mod.rnc" inherit = svg {
                  shape = element shape { attribute kind { "square" } }
                }
                start = element doc { graphic }
                "#,
            ),
            (
                "mod.rnc",
                r#"
                graphic = element graphic { shape }
                shape = element shape { empty }
                "#,
            ),
        ]);
        // `mod.rnc` declares no namespace of its own, so `inherit = svg` puts
        // its `graphic` in the SVG namespace. The *override* is written in
        // `main.rnc`, so its unprefixed name takes `main.rnc`'s default
        // namespace — `inherit` governs the included file, not the text that
        // overrides it. (The XML syntax reads the other way round, because `ns`
        // on `<include>` is inherited by the overriding `<define>`s too; the
        // translation is what keeps the two apart, by making every name in a
        // file that declares a namespace carry it outright.)
        assert!(valid(
            &mut arena,
            start,
            r#"<doc xmlns="urn:main"><graphic xmlns="urn:svg"><shape xmlns="urn:main" kind="square"/></graphic></doc>"#
        ));
        assert!(
            !valid(
                &mut arena,
                start,
                r#"<doc xmlns="urn:main"><graphic xmlns="urn:svg"><shape xmlns="urn:main"/></graphic></doc>"#
            ),
            "the override replaced the included definition, so `kind` is required"
        );
        assert!(
            !valid(
                &mut arena,
                start,
                r#"<doc xmlns="urn:main"><graphic><shape xmlns="urn:main" kind="square"/></graphic></doc>"#
            ),
            "the inherited namespace applies to the included file"
        );
    }

    #[test]
    fn combine_grammar_div_and_annotations() {
        let (mut arena, start) = compile(&[(
            "g.rnc",
            r#"
            ## a documentation comment
            div {
              [ a:defaultValue = "1" ]
              start = element doc { inline* }
              inline = element b { empty }
              inline |= element i { empty }
            }
            "#,
        )]);
        assert!(valid(&mut arena, start, "<doc><b/><i/></doc>"));
        assert!(!valid(&mut arena, start, "<doc><x/></doc>"));
    }

    #[test]
    fn literals_concatenate_and_escape() {
        let toks = lex("t.rnc", r#"x = "a" ~ "\x{41}b" ~ '''c'''"#).expect("lexes");
        assert!(
            toks.iter()
                .any(|(t, _)| matches!(t, Tok::Literal(v) if v == "Ab")),
            "\\x{{}} decodes"
        );
        let (mut arena, start) = compile(&[(
            "g.rnc",
            r#"start = element doc { attribute v { "a" ~ "b" } }"#,
        )]);
        assert!(valid(&mut arena, start, r#"<doc v="ab"/>"#));
        assert!(!valid(&mut arena, start, r#"<doc v="a"/>"#));
    }

    #[test]
    fn markup_in_a_literal_survives_the_round_trip() {
        // The translation goes through XML text, so a value containing `<` or
        // `&` has to come back out byte for byte.
        let (mut arena, start) = compile(&[(
            "g.rnc",
            r#"start = element doc { attribute v { 'a<b&c"d' } }"#,
        )]);
        assert!(valid(
            &mut arena,
            start,
            r#"<doc v="a&lt;b&amp;c&quot;d"/>"#
        ));
        assert!(!valid(&mut arena, start, r#"<doc v="ab"/>"#));
    }

    #[test]
    fn a_keyword_can_still_be_an_element_name() {
        let (mut arena, start) = compile(&[(
            "g.rnc",
            r#"
            start = element text { \element }
            \element = element list { empty }
            "#,
        )]);
        assert!(valid(&mut arena, start, "<text><list/></text>"));
    }
}
