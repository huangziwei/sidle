//! `beautify` re-indents XHTML and CSS members. Whitespace changes only at
//! block boundaries, where rendering collapses it.

use std::io;

use crate::formats::epub::edit::{Changes, EpubPackage};
use crate::formats::epub::manifest::{MemberRole, members};
use crate::formats::epub::markup::{Tok, VERBATIM, is_block, is_void, tokens};
use crate::style::source::{self, BlockItem, Item, Kind};
use crate::util::{decode_text, extract_xml_encoding};

const INDENT: &str = "  ";

pub fn beautify(pkg: &mut EpubPackage, only: Option<&str>) -> io::Result<Changes> {
    let mut changes = Changes::default();
    let mut seen = false;
    for m in members(pkg)? {
        if only.is_some_and(|p| p != m.path) {
            continue;
        }
        let Some(bytes) = pkg.get(&m.path) else {
            continue;
        };
        let text = decode_text(bytes, extract_xml_encoding(bytes)).into_owned();
        let out = match m.role {
            MemberRole::Text | MemberRole::Nav => pretty_xhtml(&text),
            MemberRole::Style => pretty_css(&text),
            _ => continue,
        };
        seen = true;
        if out != text {
            pkg.replace(&m.path, out.into_bytes());
            changes.touch(&m.path);
        }
    }
    if let Some(path) = only
        && !seen
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{path} is not an XHTML or CSS member"),
        ));
    }
    changes.note(format!("{} member(s) re-indented", changes.changed.len()));
    Ok(changes)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Last {
    BlockOpen,
    BlockClose,
    Inline,
}

fn ascii_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_ascii_whitespace())
}

fn ascii_trim_start(s: &str) -> &str {
    s.trim_start_matches(|c: char| c.is_ascii_whitespace())
}

fn ascii_trim_end(s: &str) -> &str {
    s.trim_end_matches(|c: char| c.is_ascii_whitespace())
}

pub fn pretty_xhtml(text: &str) -> String {
    let toks = tokens(text);
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let mut stack: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut verbatim: Option<(String, usize)> = None;
    let mut last = Last::BlockClose;
    for (i, tok) in toks.iter().enumerate() {
        match tok {
            Tok::Text { start, end } => {
                let raw = &text[*start..*end];
                if verbatim.is_some() {
                    out.push_str(raw);
                    continue;
                }
                let next_block =
                    matches!(toks.get(i + 1), Some(Tok::Tag { name, .. }) if is_block(name));
                if ascii_trim(raw).is_empty() {
                    if last == Last::Inline && !next_block {
                        out.push_str(raw);
                    }
                    continue;
                }
                if markup_only(raw) {
                    for piece in raw.split('\n').map(ascii_trim).filter(|p| !p.is_empty()) {
                        newline(&mut out, depth);
                        out.push_str(piece);
                    }
                    last = Last::BlockClose;
                    continue;
                }
                let piece = match (last != Last::Inline, next_block) {
                    (true, true) => ascii_trim(raw),
                    (true, false) => ascii_trim_start(raw),
                    (false, true) => ascii_trim_end(raw),
                    (false, false) => raw,
                };
                if last == Last::BlockClose {
                    newline(&mut out, depth);
                }
                out.push_str(piece);
                last = Last::Inline;
            }
            Tok::Tag {
                start,
                end,
                name,
                closing,
                self_closing,
            } => {
                let raw = &text[*start..*end];
                if let Some((v, n)) = &mut verbatim {
                    out.push_str(raw);
                    if name == v {
                        if *closing {
                            *n -= 1;
                            if *n == 0 {
                                verbatim = None;
                                stack.pop();
                                if is_block(name) {
                                    depth = depth.saturating_sub(1);
                                    last = Last::BlockClose;
                                } else {
                                    last = Last::Inline;
                                }
                            }
                        } else if !*self_closing && !is_void(name) {
                            *n += 1;
                        }
                    }
                    continue;
                }
                let block = is_block(name);
                if *closing {
                    if block {
                        depth = depth.saturating_sub(1);
                        if last != Last::Inline {
                            newline(&mut out, depth);
                        }
                    }
                    while let Some(top) = stack.pop() {
                        if top == *name {
                            break;
                        }
                    }
                    out.push_str(raw);
                    last = if block {
                        Last::BlockClose
                    } else {
                        Last::Inline
                    };
                    continue;
                }
                if block {
                    newline(&mut out, depth);
                }
                out.push_str(raw);
                let open = !*self_closing && !is_void(name);
                if open {
                    stack.push(name.clone());
                    if block {
                        depth += 1;
                    }
                    if VERBATIM.contains(&name.as_str()) {
                        verbatim = Some((name.clone(), 1));
                        last = Last::Inline;
                        continue;
                    }
                }
                last = if !open && block {
                    Last::BlockClose
                } else if block {
                    Last::BlockOpen
                } else {
                    Last::Inline
                };
            }
        }
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn markup_only(raw: &str) -> bool {
    let mut rest = raw.trim();
    while let Some(after) = rest.strip_prefix('<') {
        let end = if after.starts_with("!--") {
            after.find("-->").map(|e| e + 3)
        } else if after.starts_with('?') {
            after.find("?>").map(|e| e + 2)
        } else if after.starts_with('!') {
            after.find('>').map(|e| e + 1)
        } else {
            None
        };
        match end {
            Some(e) => rest = after[e..].trim_start(),
            None => return false,
        }
    }
    rest.is_empty()
}

fn newline(out: &mut String, depth: usize) {
    if !out.is_empty() {
        let trimmed = out.trim_end_matches([' ', '\t']).len();
        out.truncate(trimmed);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

pub fn pretty_css(css: &str) -> String {
    let mut out = String::with_capacity(css.len() + css.len() / 4);
    write_items(css, &source::scan(css), 0, &mut out);
    out
}

fn write_items(css: &str, items: &[Item], depth: usize, out: &mut String) {
    let pad = INDENT.repeat(depth);
    for (i, item) in items.iter().enumerate() {
        if i > 0 && depth == 0 {
            out.push('\n');
        }
        let raw = &css[item.start..item.end];
        match &item.kind {
            Kind::Comment | Kind::Statement => {
                for line in raw.trim().lines() {
                    out.push_str(&pad);
                    out.push_str(line.trim());
                    out.push('\n');
                }
            }
            Kind::Rule { prelude, inner } => {
                let selectors = source::split_top_level(&css[prelude.0..prelude.1], b',')
                    .iter()
                    .map(|s| source::collapse_space(s))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&pad);
                out.push_str(&selectors);
                out.push_str(" {\n");
                write_block(&css[inner.0..inner.1], depth + 1, out);
                out.push_str(&pad);
                out.push_str("}\n");
            }
            Kind::Group {
                name,
                prelude,
                body,
                ..
            } => {
                out.push_str(&pad);
                out.push('@');
                out.push_str(name);
                let prelude = source::collapse_space(&css[prelude.0..prelude.1]);
                if !prelude.is_empty() {
                    out.push(' ');
                    out.push_str(&prelude);
                }
                out.push_str(" {\n");
                write_items(css, body, depth + 1, out);
                out.push_str(&pad);
                out.push_str("}\n");
            }
            Kind::Block {
                name,
                prelude,
                inner,
            } => {
                out.push_str(&pad);
                out.push('@');
                out.push_str(name);
                let prelude = source::collapse_space(&css[prelude.0..prelude.1]);
                if !prelude.is_empty() {
                    out.push(' ');
                    out.push_str(&prelude);
                }
                out.push_str(" {\n");
                let body = &css[inner.0..inner.1];
                if body.contains('{') {
                    write_items(
                        css,
                        &source::scan_within(css, inner.0, inner.1),
                        depth + 1,
                        out,
                    );
                } else {
                    write_block(body, depth + 1, out);
                }
                out.push_str(&pad);
                out.push_str("}\n");
            }
        }
    }
}

fn write_block(body: &str, depth: usize, out: &mut String) {
    let pad = INDENT.repeat(depth);
    for item in source::block_items(body) {
        out.push_str(&pad);
        match item {
            BlockItem::Comment(c) => out.push_str(c.trim()),
            BlockItem::Decl(name, value) => {
                out.push_str(&name);
                if !value.is_empty() {
                    out.push_str(": ");
                    out.push_str(&source::collapse_space(&value));
                }
                out.push(';');
            }
        }
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "tests/fixtures/[太宰 治] 人間失格.epub";

    fn visible(s: &str) -> String {
        s.chars()
            .filter(|c| !c.is_ascii_whitespace() && *c != ';')
            .collect()
    }

    #[test]
    fn fixture_keeps_every_visible_character() {
        let bytes = std::fs::read(FIXTURE).unwrap();
        let before = EpubPackage::parse(&bytes).unwrap();
        let mut after = EpubPackage::parse(&bytes).unwrap();
        let changes = beautify(&mut after, None).unwrap();
        assert!(changes.changed.contains(&"OEBPS/c16.xhtml".to_string()));
        assert!(changes.changed.contains(&"OEBPS/style.css".to_string()));
        for path in &changes.changed {
            let a = String::from_utf8(before.get(path).unwrap().to_vec()).unwrap();
            let b = String::from_utf8(after.get(path).unwrap().to_vec()).unwrap();
            assert_eq!(visible(&a), visible(&b), "{path}");
        }
        #[cfg(feature = "validate")]
        {
            let out = after.to_bytes().unwrap();
            let added = crate::validate::source::added_errors(&bytes, &out);
            assert!(added.is_empty(), "{added:?}");
        }
    }

    #[test]
    fn xhtml_breaks_only_at_block_boundaries() {
        let src = "<?xml version=\"1.0\"?>\n<!DOCTYPE html>\n<html xmlns=\"x\"><head><title>t</title><link rel=\"stylesheet\" href=\"s.css\"/></head><body><div><p class=\"a\">　私は<ruby><rb>人</rb><rt>ひと</rt></ruby>、<em>x</em> y<br/></p>\n  <p>b</p></div><pre>\n  keep\n   this</pre><!-- c --><p>tail <span>s</span></p></body></html>";
        let out = pretty_xhtml(src);
        assert_eq!(
            out,
            "<?xml version=\"1.0\"?>\n<!DOCTYPE html>\n<html xmlns=\"x\">\n  <head>\n    <title>t</title>\n    <link rel=\"stylesheet\" href=\"s.css\"/>\n  </head>\n  <body>\n    <div>\n      <p class=\"a\">　私は<ruby><rb>人</rb><rt>ひと</rt></ruby>、<em>x</em> y<br/></p>\n      <p>b</p>\n    </div>\n    <pre>\n  keep\n   this</pre>\n    <!-- c -->\n    <p>tail <span>s</span></p>\n  </body>\n</html>\n"
        );
        assert_eq!(pretty_xhtml(&out), out);
    }

    #[test]
    fn css_gets_one_declaration_per_line() {
        let src = "@charset \"utf-8\";\n.a,.b   >  .c{color:red;margin:0 auto}\n@media   (min-width:1px){.d{x:y} /* k */ .e{z:w;}}\n@font-face{font-family:\"F  G\";src:url(a.ttf)}";
        let out = pretty_css(src);
        assert_eq!(
            out,
            "@charset \"utf-8\";\n\n.a, .b > .c {\n  color: red;\n  margin: 0 auto;\n}\n\n@media (min-width:1px) {\n  .d {\n    x: y;\n  }\n  /* k */\n  .e {\n    z: w;\n  }\n}\n\n@font-face {\n  font-family: \"F  G\";\n  src: url(a.ttf);\n}\n"
        );
        assert_eq!(pretty_css(&out), out);
    }
}
