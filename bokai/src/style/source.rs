//! `scan` reads a stylesheet into rules, groups and blocks with byte spans;
//! `block_items` and `rename_class` read and edit them in place.

pub(crate) struct Item {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}

pub(crate) enum Kind {
    Comment,
    Statement,
    Group {
        name: String,
        prelude: (usize, usize),
        inner: (usize, usize),
        body: Vec<Item>,
    },
    Block {
        name: String,
        prelude: (usize, usize),
        inner: (usize, usize),
    },
    Rule {
        prelude: (usize, usize),
        inner: (usize, usize),
    },
}

const GROUPS: &[&str] = &[
    "media",
    "supports",
    "document",
    "container",
    "layer",
    "scope",
];

pub(crate) fn scan(css: &str) -> Vec<Item> {
    scan_range(css, 0, css.len())
}

pub(crate) fn scan_within(css: &str, from: usize, to: usize) -> Vec<Item> {
    scan_range(css, from, to)
}

fn scan_range(css: &str, from: usize, to: usize) -> Vec<Item> {
    let bytes = css.as_bytes();
    let mut items = Vec::new();
    let mut i = from;
    while i < to {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if css[i..].starts_with("/*") {
            let end = css[i + 2..to].find("*/").map_or(to, |e| i + 2 + e + 2);
            items.push(Item {
                start: i,
                end,
                kind: Kind::Comment,
            });
            i = end;
            continue;
        }
        if c == b'@' {
            let name_end = i
                + 1
                + css[i + 1..to]
                    .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
                    .unwrap_or(to - i - 1);
            let name = css[i + 1..name_end].to_ascii_lowercase();
            let Some(stop) = seek(css, name_end, to, true) else {
                items.push(Item {
                    start: i,
                    end: to,
                    kind: Kind::Statement,
                });
                break;
            };
            if bytes[stop] == b';' {
                items.push(Item {
                    start: i,
                    end: stop + 1,
                    kind: Kind::Statement,
                });
                i = stop + 1;
                continue;
            }
            let close = matching_brace(css, stop, to);
            let prelude = (name_end, trim_end(css, name_end, stop));
            let inner = (stop + 1, close);
            let end = (close + 1).min(to);
            let kind = if GROUPS.contains(&name.as_str()) {
                Kind::Group {
                    name,
                    prelude,
                    inner,
                    body: scan_range(css, stop + 1, close),
                }
            } else {
                Kind::Block {
                    name,
                    prelude,
                    inner,
                }
            };
            items.push(Item {
                start: i,
                end,
                kind,
            });
            i = end;
            continue;
        }
        let Some(stop) = seek(css, i, to, false) else {
            items.push(Item {
                start: i,
                end: to,
                kind: Kind::Statement,
            });
            break;
        };
        let close = matching_brace(css, stop, to);
        let end = (close + 1).min(to);
        items.push(Item {
            start: i,
            end,
            kind: Kind::Rule {
                prelude: (i, trim_end(css, i, stop)),
                inner: (stop + 1, close),
            },
        });
        i = end;
    }
    items
}

fn trim_end(css: &str, from: usize, to: usize) -> usize {
    from + css[from..to].trim_end().len()
}

fn seek(css: &str, from: usize, to: usize, semicolon_ends: bool) -> Option<usize> {
    let bytes = css.as_bytes();
    let mut i = from;
    let mut depth = 0usize;
    while i < to {
        match bytes[i] {
            b'"' | b'\'' => i = skip_string(css, i, to),
            b'/' if css[i..].starts_with("/*") => {
                i = css[i + 2..to].find("*/").map_or(to, |e| i + 2 + e + 2);
            }
            b'(' | b'[' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'{' if depth == 0 => return Some(i),
            b';' if depth == 0 && semicolon_ends => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn matching_brace(css: &str, open: usize, to: usize) -> usize {
    let bytes = css.as_bytes();
    let mut i = open + 1;
    let mut depth = 1usize;
    while i < to {
        match bytes[i] {
            b'"' | b'\'' => i = skip_string(css, i, to),
            b'/' if css[i..].starts_with("/*") => {
                i = css[i + 2..to].find("*/").map_or(to, |e| i + 2 + e + 2);
            }
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    to
}

fn skip_string(css: &str, at: usize, to: usize) -> usize {
    let bytes = css.as_bytes();
    let q = bytes[at];
    let mut i = at + 1;
    while i < to {
        match bytes[i] {
            b'\\' => i += 2,
            c if c == q => return i + 1,
            b'\n' => return i,
            _ => i += 1,
        }
    }
    to
}

pub(crate) enum BlockItem {
    Decl(String, String),
    Comment(String),
}

pub(crate) fn block_items(block: &str) -> Vec<BlockItem> {
    let mut out = Vec::new();
    let bytes = block.as_bytes();
    let mut start = 0;
    let mut i = 0;
    let mut depth = 0usize;
    let push = |out: &mut Vec<BlockItem>, s: &str| {
        let s = s.trim();
        if s.is_empty() {
            return;
        }
        match s.split_once(':') {
            Some((name, value)) => out.push(BlockItem::Decl(
                name.trim().to_string(),
                value.trim().to_string(),
            )),
            None => out.push(BlockItem::Decl(s.to_string(), String::new())),
        }
    };
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => i = skip_string(block, i, block.len()),
            b'/' if block[i..].starts_with("/*") => {
                let end = block[i + 2..]
                    .find("*/")
                    .map_or(block.len(), |e| i + 2 + e + 2);
                if block[start..i].trim().is_empty() {
                    out.push(BlockItem::Comment(block[i..end].to_string()));
                    start = end;
                }
                i = end;
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b';' if depth == 0 => {
                push(&mut out, &strip_comments(&block[start..i]));
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    push(&mut out, &strip_comments(&block[start..]));
    out
}

pub(crate) fn collapse_space(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut pending = false;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                if pending {
                    out.push(' ');
                    pending = false;
                }
                let end = skip_string(s, i, s.len());
                out.push_str(&s[i..end]);
                i = end;
            }
            c if c.is_ascii_whitespace() => {
                pending = !out.is_empty();
                i += 1;
            }
            _ => {
                if pending {
                    out.push(' ');
                    pending = false;
                }
                let ch = s[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

pub(crate) fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find("/*") {
        out.push_str(&rest[..p]);
        match rest[p + 2..].find("*/") {
            Some(e) => rest = &rest[p + 2 + e + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

pub(crate) fn split_top_level(s: &str, sep: u8) -> Vec<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => i = skip_string(s, i, s.len()),
            b'(' | b'[' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            c if c == sep && depth == 0 => {
                out.push(s[start..i].trim().to_string());
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    let last = s[start..].trim();
    if !last.is_empty() || out.is_empty() {
        out.push(last.to_string());
    }
    out
}

pub(crate) fn rename_class_in_prelude(prelude: &str, from: &str, to: &str) -> (String, usize) {
    let bytes = prelude.as_bytes();
    let mut out = String::with_capacity(prelude.len());
    let mut i = 0;
    let mut hits = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                let end = skip_string(prelude, i, prelude.len());
                out.push_str(&prelude[i..end]);
                i = end;
            }
            b'.' => {
                let name_end = i
                    + 1
                    + prelude[i + 1..]
                        .find(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
                        .unwrap_or(prelude.len() - i - 1);
                if &prelude[i + 1..name_end] == from {
                    out.push('.');
                    out.push_str(to);
                    hits += 1;
                } else {
                    out.push_str(&prelude[i..name_end]);
                }
                i = name_end;
            }
            _ => {
                let ch = prelude[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    (out, hits)
}

pub(crate) fn rename_class(css: &str, from: &str, to: &str) -> (String, usize) {
    let mut out = String::with_capacity(css.len());
    let mut hits = 0;
    let mut pos = 0;
    rename_items(css, &scan(css), from, to, &mut out, &mut pos, &mut hits);
    out.push_str(&css[pos..]);
    (out, hits)
}

fn rename_items(
    css: &str,
    items: &[Item],
    from: &str,
    to: &str,
    out: &mut String,
    pos: &mut usize,
    hits: &mut usize,
) {
    for item in items {
        match &item.kind {
            Kind::Rule { prelude, .. } => {
                out.push_str(&css[*pos..prelude.0]);
                let (renamed, n) = rename_class_in_prelude(&css[prelude.0..prelude.1], from, to);
                out.push_str(&renamed);
                *hits += n;
                *pos = prelude.1;
            }
            Kind::Group { body, .. } => rename_items(css, body, from, to, out, pos, hits),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_rules_groups_blocks_and_statements() {
        let css = "@charset \"utf-8\";\n/* c */\n@import url(a.css);\np, .x { color: red; }\n@media (min-width: 1px) { .y { a: b } }\n@font-face { src: url(\"f.ttf\") }";
        let items = scan(css);
        let kinds: Vec<&str> = items
            .iter()
            .map(|i| match &i.kind {
                Kind::Comment => "comment",
                Kind::Statement => "statement",
                Kind::Group { .. } => "group",
                Kind::Block { .. } => "block",
                Kind::Rule { .. } => "rule",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "statement",
                "comment",
                "statement",
                "rule",
                "group",
                "block"
            ]
        );
        let Kind::Rule { prelude, inner } = &items[3].kind else {
            panic!()
        };
        assert_eq!(&css[prelude.0..prelude.1], "p, .x");
        assert_eq!(&css[inner.0..inner.1], " color: red; ");
        let Kind::Group { body, .. } = &items[4].kind else {
            panic!()
        };
        assert_eq!(body.len(), 1);
    }

    #[test]
    fn declarations_split_outside_strings_and_parens() {
        let d: Vec<(String, String)> =
            block_items("a: 1; b: url(\"x;y\"); /* c: 2; */ d : rgb(1,2,3)")
                .into_iter()
                .filter_map(|i| match i {
                    BlockItem::Decl(n, v) => Some((n, v)),
                    BlockItem::Comment(_) => None,
                })
                .collect();
        assert_eq!(
            d,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "url(\"x;y\")".to_string()),
                ("d".to_string(), "rgb(1,2,3)".to_string()),
            ]
        );
    }

    #[test]
    fn renames_classes_in_selectors_only() {
        let css =
            ".a, .ab .a:hover { background: url(.a) } @media print { div.a { x: y } } .a-b {}";
        let (out, n) = rename_class(css, "a", "z");
        assert_eq!(
            out,
            ".z, .ab .z:hover { background: url(.a) } @media print { div.z { x: y } } .a-b {}"
        );
        assert_eq!(n, 3);
    }
}
