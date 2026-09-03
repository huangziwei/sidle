//! `unused_css` lists stylesheet rules no content document matches;
//! `remove_unused_css` cuts them from the sheets in place.

use std::io;

use crate::formats::epub::edit::{Changes, EpubPackage};
use crate::formats::epub::manifest::{MemberRole, members};
use crate::html::{ArenaDom, any_element_matches, parse_dom};
use crate::style::Stylesheet;
use crate::style::source::{self, Item, Kind};
use crate::util::{decode_text, extract_xml_encoding};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusedRule {
    pub sheet: String,
    pub selector: String,
    pub line: usize,
}

pub fn unused_css(pkg: &EpubPackage) -> io::Result<Vec<UnusedRule>> {
    let doms = documents(pkg)?;
    let mut out = Vec::new();
    for m in members(pkg)? {
        if m.role != MemberRole::Style {
            continue;
        }
        let Some(bytes) = pkg.get(&m.path) else {
            continue;
        };
        let css = decode_text(bytes, extract_xml_encoding(bytes)).into_owned();
        for (start, end) in unused_spans(&css, &source::scan(&css), &doms) {
            let prelude = css[start..end].split('{').next().unwrap_or("").trim();
            out.push(UnusedRule {
                sheet: m.path.clone(),
                selector: source::collapse_space(prelude),
                line: css[..start].matches('\n').count() + 1,
            });
        }
    }
    Ok(out)
}

pub fn remove_unused_css(pkg: &mut EpubPackage) -> io::Result<Changes> {
    let doms = documents(pkg)?;
    let mut changes = Changes::default();
    let mut removed = 0;
    for m in members(pkg)? {
        if m.role != MemberRole::Style {
            continue;
        }
        let Some(bytes) = pkg.get(&m.path) else {
            continue;
        };
        let css = decode_text(bytes, extract_xml_encoding(bytes)).into_owned();
        let spans = unused_spans(&css, &source::scan(&css), &doms);
        if spans.is_empty() {
            continue;
        }
        removed += spans.len();
        pkg.replace(&m.path, cut(&css, &spans).into_bytes());
        changes.touch(&m.path);
    }
    if changes.is_empty() {
        changes.note("every rule matches something");
    } else {
        changes.note(format!("{removed} unused rule(s) removed"));
    }
    Ok(changes)
}

fn documents(pkg: &EpubPackage) -> io::Result<Vec<ArenaDom>> {
    let mut doms = Vec::new();
    for m in members(pkg)? {
        if !matches!(m.role, MemberRole::Text | MemberRole::Nav) {
            continue;
        }
        if let Some(bytes) = pkg.get(&m.path) {
            let text = decode_text(bytes, extract_xml_encoding(bytes));
            doms.push(parse_dom(&text));
        }
    }
    Ok(doms)
}

fn unused_spans(css: &str, items: &[Item], doms: &[ArenaDom]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for item in items {
        match &item.kind {
            Kind::Rule { prelude, .. } => {
                if !rule_used(&css[prelude.0..prelude.1], doms) {
                    spans.push((item.start, item.end));
                }
            }
            Kind::Group { body, inner, .. } => {
                let inside = unused_spans(css, body, doms);
                let rules = body
                    .iter()
                    .filter(|i| !matches!(i.kind, Kind::Comment))
                    .count();
                if rules > 0 && inside.len() == rules && !css[inner.0..inner.1].trim().is_empty() {
                    spans.push((item.start, item.end));
                } else {
                    spans.extend(inside);
                }
            }
            _ => {}
        }
    }
    spans
}

fn rule_used(prelude: &str, doms: &[ArenaDom]) -> bool {
    source::split_top_level(prelude, b',').iter().any(|sel| {
        let base = strip_pseudo(sel);
        if base.trim().is_empty() {
            return true;
        }
        let sheet = Stylesheet::parse(&format!("{base}{{}}"));
        let Some(rule) = sheet.rules.first() else {
            return true;
        };
        if rule.selectors.is_empty() {
            return true;
        }
        doms.iter()
            .any(|dom| any_element_matches(dom, &rule.selectors))
    })
}

fn strip_pseudo(selector: &str) -> String {
    let bytes = selector.as_bytes();
    let mut out = String::with_capacity(selector.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b':' => {
                let mut j = i + 1;
                while j < bytes.len()
                    && (bytes[j] == b':' || bytes[j] == b'-' || bytes[j].is_ascii_alphanumeric())
                {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'(' {
                    let mut depth = 0;
                    while j < bytes.len() {
                        match bytes[j] {
                            b'(' => depth += 1,
                            b')' => {
                                depth -= 1;
                                if depth == 0 {
                                    j += 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                }
                i = j;
            }
            b'"' | b'\'' => {
                let q = bytes[i];
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] != q {
                    j += 1;
                }
                out.push_str(&selector[i..(j + 1).min(bytes.len())]);
                i = j + 1;
            }
            _ => {
                let ch = selector[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

fn cut(css: &str, spans: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(css.len());
    let mut pos = 0;
    for &(start, end) in spans {
        if start < pos {
            continue;
        }
        let line_start = css[..start].rfind('\n').map_or(0, |i| i + 1);
        let alone = css[line_start..start].trim().is_empty();
        let mut cut_end = end;
        if alone {
            let rest = &css[end..];
            let trail = rest.len() - rest.trim_start_matches([' ', '\t']).len();
            if rest[trail..].starts_with("\r\n") {
                cut_end = end + trail + 2;
            } else if rest[trail..].starts_with('\n') {
                cut_end = end + trail + 1;
            }
        }
        let cut_start = if alone && cut_end > end {
            line_start.max(pos)
        } else {
            start
        };
        out.push_str(&css[pos..cut_start]);
        pos = cut_end;
    }
    out.push_str(&css[pos..]);
    out
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
                "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>t</title></head><body><p class=\"used\">a</p><a href=\"#\">l</a></body></html>",
            ),
            (
                "OEBPS/s.css",
                "@charset \"utf-8\";\n.used { a: b }\n.gone { c: d }\np.used:first-child::before, .gone { e: f }\na:hover { g: h }\n@media print {\n  .gone { i: j }\n}\n@media screen {\n  .gone { k: l }\n  p { m: n }\n}\n@font-face { font-family: x }\n",
            ),
        ])
    }

    #[test]
    fn reports_and_removes_rules_nothing_matches() {
        let pkg = book();
        let unused = unused_css(&pkg).unwrap();
        let sel: Vec<(String, usize)> = unused
            .iter()
            .map(|u| (u.selector.clone(), u.line))
            .collect();
        assert_eq!(
            sel,
            vec![
                (".gone".to_string(), 3),
                ("@media print".to_string(), 6),
                (".gone".to_string(), 10),
            ]
        );
        let mut pkg = book();
        let changes = remove_unused_css(&mut pkg).unwrap();
        assert_eq!(changes.changed, vec!["OEBPS/s.css"]);
        let css = std::str::from_utf8(pkg.get("OEBPS/s.css").unwrap()).unwrap();
        assert_eq!(
            css,
            "@charset \"utf-8\";\n.used { a: b }\np.used:first-child::before, .gone { e: f }\na:hover { g: h }\n@media screen {\n  p { m: n }\n}\n@font-face { font-family: x }\n"
        );
    }
}
