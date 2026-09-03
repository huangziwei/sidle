//! `rename_class` rewrites one class name in every stylesheet selector,
//! `<style>` block and `class` attribute of an EPUB.

use std::io;

use crate::formats::epub::edit::{Changes, EpubPackage};
use crate::formats::epub::manifest::{MemberRole, members};
use crate::formats::epub::markup::{Tok, class_list, set_attr, tokens};
use crate::style::source;
use crate::util::{decode_text, extract_xml_encoding};

pub fn rename_class(pkg: &mut EpubPackage, from: &str, to: &str) -> io::Result<Changes> {
    let from = from.trim().trim_start_matches('.');
    let to = to.trim().trim_start_matches('.');
    if from.is_empty() || to.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a class name is empty",
        ));
    }
    if !valid_identifier(to) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{to} is not a valid class name"),
        ));
    }
    if from == to {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the new name is the old name",
        ));
    }
    let mut changes = Changes::default();
    let mut elements = 0;
    let mut rules = 0;
    let mut merged = false;
    for m in members(pkg)? {
        let Some(bytes) = pkg.get(&m.path) else {
            continue;
        };
        match m.role {
            MemberRole::Style => {
                let css = decode_text(bytes, extract_xml_encoding(bytes)).into_owned();
                if !css.contains(from) {
                    continue;
                }
                merged |= css_defines(&css, to);
                let (out, n) = source::rename_class(&css, from, to);
                if n > 0 {
                    rules += n;
                    pkg.replace(&m.path, out.into_bytes());
                    changes.touch(&m.path);
                }
            }
            MemberRole::Text | MemberRole::Nav => {
                let text = decode_text(bytes, extract_xml_encoding(bytes)).into_owned();
                if !text.contains(from) {
                    continue;
                }
                let (out, e, r, defines) = rename_in_document(&text, from, to);
                merged |= defines;
                if e + r > 0 {
                    elements += e;
                    rules += r;
                    pkg.replace(&m.path, out.into_bytes());
                    changes.touch(&m.path);
                }
            }
            _ => {}
        }
    }
    if changes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no rule or element uses the class {from}"),
        ));
    }
    changes.note(format!(
        "{elements} element(s) and {rules} selector(s) now use {to}"
    ));
    if merged {
        changes.note(format!(
            "{to} was already defined; the two classes now share its rules"
        ));
    }
    Ok(changes)
}

fn valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' || c == '-' || !c.is_ascii() => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_' || c == '-' || !c.is_ascii())
}

fn css_defines(css: &str, class: &str) -> bool {
    source::rename_class(css, class, class).1 > 0
}

fn rename_in_document(text: &str, from: &str, to: &str) -> (String, usize, usize, bool) {
    let mut out = String::with_capacity(text.len());
    let mut elements = 0;
    let mut rules = 0;
    let mut defines = false;
    let mut in_style = false;
    for tok in tokens(text) {
        match tok {
            Tok::Text { start, end } => {
                let raw = &text[start..end];
                if in_style {
                    defines |= css_defines(raw, to);
                    let (renamed, n) = source::rename_class(raw, from, to);
                    rules += n;
                    out.push_str(&renamed);
                } else {
                    out.push_str(raw);
                }
            }
            Tok::Tag {
                start,
                end,
                name,
                closing,
                self_closing,
            } => {
                let raw = &text[start..end];
                if name == "style" {
                    in_style = !closing && !self_closing;
                }
                if closing {
                    out.push_str(raw);
                    continue;
                }
                let classes = class_list(raw);
                if !classes.iter().any(|c| c == from) {
                    out.push_str(raw);
                    continue;
                }
                let mut renamed: Vec<&str> = Vec::with_capacity(classes.len());
                for c in &classes {
                    let c = if c == from { to } else { c.as_str() };
                    if !renamed.contains(&c) {
                        renamed.push(c);
                    }
                }
                elements += 1;
                out.push_str(&set_attr(raw, "class", Some(&renamed.join(" "))));
            }
        }
    }
    (out, elements, rules, defines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::epub::edit::tests::package_from;

    fn book() -> EpubPackage {
        package_from(&[
            (
                "OEBPS/content.opf",
                r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="id">x</dc:identifier><dc:title>T</dc:title><dc:language>en</dc:language></metadata><manifest><item href="a.xhtml" id="a" media-type="application/xhtml+xml"/><item href="s.css" id="s" media-type="text/css"/></manifest><spine><itemref idref="a"/></spine></package>"#,
            ),
            (
                "OEBPS/a.xhtml",
                "<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><style>.old { x: y } .older {}</style></head><body><p class=\"old older new\">a</p><p class=\"x\">b</p></body></html>",
            ),
            ("OEBPS/s.css", ".old { a: b } p.old:hover, .older { c: d }"),
        ])
    }

    #[test]
    fn renames_selectors_style_blocks_and_class_attributes() {
        let mut pkg = book();
        let changes = rename_class(&mut pkg, ".old", "new").unwrap();
        assert_eq!(changes.changed, vec!["OEBPS/a.xhtml", "OEBPS/s.css"]);
        let css = std::str::from_utf8(pkg.get("OEBPS/s.css").unwrap()).unwrap();
        assert_eq!(css, ".new { a: b } p.new:hover, .older { c: d }");
        let doc = std::str::from_utf8(pkg.get("OEBPS/a.xhtml").unwrap()).unwrap();
        assert!(doc.contains("<style>.new { x: y } .older {}</style>"));
        assert!(doc.contains("<p class=\"new older\">a</p>"));
        assert!(doc.contains("<p class=\"x\">b</p>"));
        assert!(changes.notes[0].starts_with("1 element(s) and 3 selector(s)"));
    }

    #[test]
    fn rejects_bad_names_and_unknown_classes() {
        let mut pkg = book();
        assert!(rename_class(&mut pkg, "old", "1bad").is_err());
        assert!(rename_class(&mut pkg, "old", "old").is_err());
        assert!(rename_class(&mut pkg, "missing", "fine").is_err());
    }
}
