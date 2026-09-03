//! `tokens`, `attributes`, `offset_of`, `body_span` and `content_properties`
//! read XHTML source text; `set_attr` and `rewrite_tags` edit it in place.

use crate::formats::epub::edit::{attr_value, escape_attr};

pub(crate) const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

const BLOCK: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "caption",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hgroup",
    "hr",
    "html",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "meta",
    "nav",
    "ol",
    "optgroup",
    "option",
    "p",
    "pre",
    "script",
    "section",
    "select",
    "style",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "ul",
];

pub(crate) const VERBATIM: &[&str] = &["pre", "textarea", "script", "style", "svg", "math"];

pub(crate) enum Tok {
    Text {
        start: usize,
        end: usize,
    },
    Tag {
        start: usize,
        end: usize,
        name: String,
        closing: bool,
        self_closing: bool,
    },
}

pub(crate) fn is_block(name: &str) -> bool {
    BLOCK.contains(&name)
}

pub(crate) fn is_void(name: &str) -> bool {
    VOID.contains(&name)
}

pub(crate) fn tokens(text: &str) -> Vec<Tok> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut text_start = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &text[i..];
        let skip = if rest.starts_with("<!--") {
            rest.find("-->").map(|e| e + 3)
        } else if rest.starts_with("<![CDATA[") {
            rest.find("]]>").map(|e| e + 3)
        } else if rest.starts_with("<?") {
            rest.find("?>").map(|e| e + 2)
        } else if rest.starts_with("<!") {
            rest.find('>').map(|e| e + 1)
        } else {
            None
        };
        if let Some(n) = skip {
            i += n;
            continue;
        }
        let closing = rest.starts_with("</");
        let name_start = if closing { 2 } else { 1 };
        let name_len = rest[name_start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == ':' || c == '-' || c == '_'))
            .unwrap_or(rest.len() - name_start);
        if name_len == 0 {
            i += 1;
            continue;
        }
        let mut j = name_start + name_len;
        let mut quote: Option<u8> = None;
        let rb = rest.as_bytes();
        while j < rb.len() {
            match (quote, rb[j]) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, b'"') | (None, b'\'') => quote = Some(rb[j]),
                (None, b'>') => break,
                _ => {}
            }
            j += 1;
        }
        if j >= rb.len() {
            break;
        }
        if text_start < i {
            out.push(Tok::Text {
                start: text_start,
                end: i,
            });
        }
        let end = i + j + 1;
        out.push(Tok::Tag {
            start: i,
            end,
            name: rest[name_start..name_start + name_len].to_ascii_lowercase(),
            closing,
            self_closing: rb[..j].ends_with(b"/"),
        });
        i = end;
        text_start = end;
    }
    if text_start < text.len() {
        out.push(Tok::Text {
            start: text_start,
            end: text.len(),
        });
    }
    out
}

pub(crate) fn set_attr(tag: &str, name: &str, value: Option<&str>) -> String {
    let needle = format!("{name}=");
    let mut from = 0;
    while let Some(rel) = tag[from..].find(&needle) {
        let pos = from + rel;
        let boundary = pos > 0 && tag.as_bytes()[pos - 1].is_ascii_whitespace();
        if boundary {
            let after = &tag[pos + needle.len()..];
            if let Some(q) = after.chars().next().filter(|q| *q == '"' || *q == '\'')
                && let Some(end) = after[1..].find(q)
            {
                let value_start = pos + needle.len() + 1;
                let value_end = value_start + end;
                return match value {
                    Some(v) => format!(
                        "{}{}{}",
                        &tag[..value_start],
                        escape_attr(v),
                        &tag[value_end..]
                    ),
                    None => {
                        let ws_start = tag[..pos].trim_end().len();
                        format!("{}{}", &tag[..ws_start], &tag[value_end + 1..])
                    }
                };
            }
        }
        from = pos + needle.len();
    }
    match value {
        None => tag.to_string(),
        Some(v) => {
            let trimmed = tag.trim_end_matches('>');
            let (head, tail) = match trimmed.strip_suffix('/') {
                Some(h) => (h.trim_end(), "/>"),
                None => (trimmed, ">"),
            };
            format!("{head} {name}=\"{}\"{tail}", escape_attr(v))
        }
    }
}

pub(crate) fn class_list(tag: &str) -> Vec<String> {
    attr_value(tag, "class")
        .map(|c| c.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

pub(crate) struct Attr {
    pub name: String,
    pub value: String,
}

pub(crate) fn attributes(tag: &str) -> Vec<Attr> {
    let inner = tag
        .trim_start_matches('<')
        .trim_start_matches('/')
        .trim_end_matches('>')
        .trim_end_matches('/');
    let name_len = inner
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(inner.len());
    let mut rest = &inner[name_len..];
    let mut out = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let name_end = rest
            .find(|c: char| c == '=' || c.is_ascii_whitespace())
            .unwrap_or(rest.len());
        let name = rest[..name_end].to_string();
        rest = rest[name_end..].trim_start();
        let Some(after_eq) = rest.strip_prefix('=') else {
            out.push(Attr {
                name,
                value: String::new(),
            });
            continue;
        };
        let after_eq = after_eq.trim_start();
        let (value, tail) = match after_eq.chars().next() {
            Some(q) if q == '"' || q == '\'' => match after_eq[1..].find(q) {
                Some(end) => (after_eq[1..1 + end].to_string(), &after_eq[end + 2..]),
                None => (after_eq[1..].to_string(), ""),
            },
            _ => {
                let end = after_eq
                    .find(|c: char| c.is_ascii_whitespace())
                    .unwrap_or(after_eq.len());
                (after_eq[..end].to_string(), &after_eq[end..])
            }
        };
        out.push(Attr {
            name,
            value: unescape(&value),
        });
        rest = tail;
    }
    out
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

pub(crate) fn rewrite_tags<F>(text: &str, mut edit: F) -> String
where
    F: FnMut(&str, &str) -> Option<String>,
{
    let mut out = String::with_capacity(text.len());
    for tok in tokens(text) {
        match tok {
            Tok::Text { start, end } => out.push_str(&text[start..end]),
            Tok::Tag {
                start,
                end,
                name,
                closing,
                ..
            } => {
                let raw = &text[start..end];
                match (!closing).then(|| edit(&name, raw)).flatten() {
                    Some(new) => out.push_str(&new),
                    None => out.push_str(raw),
                }
            }
        }
    }
    out
}

pub(crate) fn offset_of(text: &str, line: usize, col: usize) -> Option<usize> {
    let line_start = if line <= 1 {
        0
    } else {
        let mut seen = 1;
        let mut pos = None;
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                seen += 1;
                if seen == line {
                    pos = Some(i + 1);
                    break;
                }
            }
        }
        pos?
    };
    let rest = &text[line_start..];
    let line_end = rest.find('\n').unwrap_or(rest.len());
    let in_line = &rest[..line_end];
    let byte = in_line
        .char_indices()
        .nth(col.saturating_sub(1))
        .map_or(in_line.len(), |(i, _)| i);
    Some(line_start + byte)
}

pub(crate) fn body_span(text: &str) -> Option<(usize, usize, usize, usize)> {
    let mut open = None;
    for tok in tokens(text) {
        if let Tok::Tag {
            start,
            end,
            name,
            closing,
            ..
        } = tok
            && name == "body"
        {
            if !closing {
                open = Some((start, end));
            } else if let Some((os, oe)) = open {
                return Some((os, oe, start, end));
            }
        }
    }
    None
}

pub(crate) fn content_properties(text: &str) -> Vec<&'static str> {
    let mut svg = false;
    let mut mathml = false;
    let mut scripted = false;
    let mut remote = false;
    for tok in tokens(text) {
        let Tok::Tag {
            start,
            end,
            name,
            closing: false,
            ..
        } = tok
        else {
            continue;
        };
        let tag = &text[start..end];
        match name.as_str() {
            "svg" | "svg:svg" => svg = true,
            "math" | "m:math" => mathml = true,
            "form" => scripted = true,
            "script" => {
                let kind = attr_value(tag, "type").unwrap_or_default();
                let kind = kind.trim().to_ascii_lowercase();
                if kind.is_empty()
                    || kind.contains("javascript")
                    || kind.contains("ecmascript")
                    || kind == "module"
                {
                    scripted = true;
                }
            }
            _ => {}
        }
        if !remote
            && matches!(
                name.as_str(),
                "img"
                    | "image"
                    | "audio"
                    | "video"
                    | "source"
                    | "track"
                    | "iframe"
                    | "object"
                    | "embed"
                    | "script"
                    | "link"
            )
        {
            for a in attributes(tag) {
                let is_ref = matches!(
                    a.name.as_str(),
                    "src" | "href" | "xlink:href" | "data" | "poster"
                );
                if is_ref && is_remote(&a.value) {
                    remote = true;
                }
            }
        }
    }
    let mut out = Vec::new();
    if svg {
        out.push("svg");
    }
    if mathml {
        out.push("mathml");
    }
    if scripted {
        out.push("scripted");
    }
    if remote {
        out.push("remote-resources");
    }
    out
}

fn is_remote(url: &str) -> bool {
    let u = url.trim().to_ascii_lowercase();
    u.starts_with("http://") || u.starts_with("https://") || u.starts_with("//")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_attr_replaces_removes_and_adds() {
        assert_eq!(
            set_attr(r#"<p class="a b">"#, "class", Some("c")),
            r#"<p class="c">"#
        );
        assert_eq!(
            set_attr(r#"<p id="x" class="a">"#, "class", None),
            r#"<p id="x">"#
        );
        assert_eq!(set_attr(r#"<p>"#, "class", Some("c")), r#"<p class="c">"#);
        assert_eq!(
            set_attr(r#"<img src="a"/>"#, "class", Some("c")),
            r#"<img src="a" class="c"/>"#
        );
        assert_eq!(
            set_attr(r#"<link href="a" rel="stylesheet"/>"#, "href", Some("b")),
            r#"<link href="b" rel="stylesheet"/>"#
        );
    }

    #[test]
    fn tokens_skip_comments_and_track_closing_tags() {
        let t = tokens("<a><!-- <b> --><br/>x</a>");
        let names: Vec<String> = t
            .iter()
            .filter_map(|tok| match tok {
                Tok::Tag {
                    name,
                    closing,
                    self_closing,
                    ..
                } => Some(format!(
                    "{name}{}{}",
                    if *closing { "/" } else { "" },
                    if *self_closing { "!" } else { "" }
                )),
                Tok::Text { .. } => None,
            })
            .collect();
        assert_eq!(names, vec!["a", "br!", "a/"]);
    }

    #[test]
    fn attributes_read_quoted_bare_and_entity_values() {
        let a = attributes(r#"<a href="x&amp;y" title='t' data-x=bare hidden/>"#);
        let pairs: Vec<(String, String)> = a.into_iter().map(|a| (a.name, a.value)).collect();
        assert_eq!(
            pairs,
            vec![
                ("href".to_string(), "x&y".to_string()),
                ("title".to_string(), "t".to_string()),
                ("data-x".to_string(), "bare".to_string()),
                ("hidden".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn offsets_follow_lines_and_character_columns() {
        let text = "ab\nc漢d\ne";
        assert_eq!(offset_of(text, 1, 1), Some(0));
        assert_eq!(offset_of(text, 2, 1), Some(3));
        assert_eq!(offset_of(text, 2, 3), Some(7));
        assert_eq!(offset_of(text, 2, 9), Some(8));
        assert_eq!(offset_of(text, 3, 1), Some(9));
        assert_eq!(offset_of(text, 4, 1), None);
    }

    #[test]
    fn body_span_and_content_properties() {
        let text = r#"<html><body class="x"><p>t</p><svg/><script src="http://a/b.js"></script></body></html>"#;
        let (os, oe, cs, ce) = body_span(text).unwrap();
        assert_eq!(&text[os..oe], r#"<body class="x">"#);
        assert_eq!(&text[cs..ce], "</body>");
        assert_eq!(
            content_properties(text),
            vec!["svg", "scripted", "remote-resources"]
        );
        assert!(content_properties("<p><a href=\"http://x\">l</a></p>").is_empty());
    }
}
