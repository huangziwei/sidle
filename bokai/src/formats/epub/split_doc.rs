//! `split_document` cuts a content document in two at a block boundary;
//! `merge_with_next` folds the next spine document in. Both retarget ids and
//! links and update the manifest and spine.

use std::collections::BTreeSet;
use std::io;

use crate::formats::epub::edit::{Changes, EpubPackage, attr_value, escape_attr};
use crate::formats::epub::manifest::{
    MemberRole, add_manifest_item, insert_itemref_after, members, remove_itemref,
    remove_manifest_item, set_item_properties,
};
use crate::formats::epub::markup::{
    Tok, attributes, body_span, content_properties, is_block, is_void, offset_of, rewrite_tags,
    set_attr, tokens,
};
use crate::formats::epub::parse_opf;
use crate::formats::epub::structure::{
    dir_of, relativize, resolve_href, spine_documents, split_fragment,
};
use crate::util::{decode_text, extract_xml_encoding, percent_decode};

const XHTML: &str = "application/xhtml+xml";

pub fn split_document(
    pkg: &mut EpubPackage,
    path: &str,
    line: usize,
    col: usize,
) -> io::Result<Changes> {
    let text = member_text(pkg, path)?;
    let offset = offset_of(&text, line, col).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{path} has no line {line}"),
        )
    })?;
    let (_, body_open_end, body_close, _) = body_span(&text).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("{path} has no <body>"))
    })?;
    let cut = split_point(&text, body_open_end, body_close, offset).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "put the cursor inside the block that should start the new document",
        )
    })?;
    let before = &text[body_open_end..cut.at];
    if before
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .is_empty()
        || only_open_tags(before, cut.ancestors.len())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the cursor is in the first block; nothing would stay in this document",
        ));
    }
    let split_at = cut.at;
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_text = member_text(pkg, &opf_path)?;
    let opf = parse_opf(&opf_text).map_err(io::Error::other)?;
    let own_id = manifest_id_of(&opf, &opf_base, path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{path} is not in the manifest"),
        )
    })?;
    let new_path = free_sibling_name(pkg, path);
    let moved: BTreeSet<String> = ids_in(&text[split_at..body_close]);
    let head = &text[..body_open_end];
    let closers: String = cut
        .ancestors
        .iter()
        .rev()
        .map(|(name, _)| format!("</{name}>\n"))
        .collect();
    let openers: String = cut
        .ancestors
        .iter()
        .map(|(_, tag)| format!("{}\n", set_attr(tag, "id", None)))
        .collect();
    let first = format!(
        "{}\n{closers}{}",
        text[..split_at].trim_end_matches(|c: char| c.is_ascii_whitespace()),
        &text[body_close..]
    );
    let second = format!(
        "{head}\n{openers}{}",
        text[split_at..].trim_start_matches(|c: char| c.is_ascii_whitespace())
    );

    let mut changes = Changes::default();
    let first = retarget(&first, path, &mut |target, frag| {
        (target == path && !frag.is_empty() && moved.contains(frag))
            .then(|| (new_path.clone(), frag.to_string()))
    });
    let second = retarget(&second, &new_path, &mut |target, frag| {
        (target == new_path && !frag.is_empty() && !moved.contains(frag))
            .then(|| (path.to_string(), frag.to_string()))
    });
    pkg.replace(path, first.into_bytes());
    changes.touch(path);
    pkg.set(&new_path, second.into_bytes());
    changes.add(&new_path, XHTML);

    for m in members(pkg)? {
        if m.path == path || m.path == new_path {
            continue;
        }
        let rewritable = matches!(m.role, MemberRole::Text | MemberRole::Nav | MemberRole::Ncx);
        if !rewritable {
            continue;
        }
        let doc = member_text(pkg, &m.path)?;
        let out = retarget(&doc, &m.path, &mut |target, frag| {
            (target == path && !frag.is_empty() && moved.contains(frag))
                .then(|| (new_path.clone(), frag.to_string()))
        });
        if out != doc {
            pkg.replace(&m.path, out.into_bytes());
            changes.touch(&m.path);
        }
    }

    let new_id = add_manifest_item(pkg, &new_path, XHTML)?;
    let mut opf_text = member_text(pkg, &opf_path)?;
    let props = content_properties(&text_of(pkg, &new_path));
    if !props.is_empty() {
        opf_text = set_item_properties(&opf_text, &new_id, &props.join(" "));
    }
    opf_text = insert_itemref_after(&opf_text, &own_id, &new_id)?;
    pkg.replace(&opf_path, opf_text.into_bytes());
    changes.touch(&opf_path);
    changes.note(format!(
        "{} now starts at line {line}; {} id(s) moved with it",
        new_path.rsplit('/').next().unwrap_or(&new_path),
        moved.len()
    ));
    Ok(changes)
}

pub fn merge_with_next(pkg: &mut EpubPackage, path: &str) -> io::Result<Changes> {
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_text = member_text(pkg, &opf_path)?;
    let opf = parse_opf(&opf_text).map_err(io::Error::other)?;
    let spine = spine_documents(&opf, &opf_base);
    let idx = spine.iter().position(|(p, _)| p == path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{path} is not in the spine"),
        )
    })?;
    let Some((next, _)) = spine.get(idx + 1).cloned() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{path} is the last document in the spine"),
        ));
    };
    let next_id = manifest_id_of(&opf, &opf_base, &next).unwrap_or_default();
    let next_type = opf
        .manifest
        .get(&next_id)
        .map(|(_, t)| t.as_str())
        .unwrap_or_default();
    if !next_type.eq_ignore_ascii_case(XHTML) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{next} is {next_type}, not an XHTML document"),
        ));
    }
    let a = member_text(pkg, path)?;
    let b = member_text(pkg, &next)?;
    let (_, _, a_close, _) = body_span(&a).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("{path} has no <body>"))
    })?;
    let (_, b_open_end, b_close, _) = body_span(&b).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("{next} has no <body>"))
    })?;
    let a_ids = ids_in(&a);
    let clash: Vec<String> = ids_in(&b)
        .into_iter()
        .filter(|i| a_ids.contains(i))
        .collect();
    if !clash.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "both documents define id {}; rename it first",
                clash
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let a_dir = dir_of(path);
    let b_dir = dir_of(&next);
    let mut changes = Changes::default();

    let mut b_body = b[b_open_end..b_close]
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_string();
    if a_dir != b_dir {
        b_body = rebase_to(&b_body, &b_dir, &a_dir);
    }
    let mut merged = format!(
        "{}\n{b_body}\n{}",
        a[..a_close].trim_end_matches(|c: char| c.is_ascii_whitespace()),
        &a[a_close..]
    );
    let a_sheets = stylesheet_links(&a, &a_dir);
    let mut added_links = Vec::new();
    for sheet in stylesheet_links(&b, &b_dir) {
        if !a_sheets.contains(&sheet) {
            added_links.push(sheet);
        }
    }
    if !added_links.is_empty()
        && let Some(head_end) = merged.find("</head>")
    {
        let links: String = added_links
            .iter()
            .map(|s| {
                format!(
                    "<link rel=\"stylesheet\" type=\"text/css\" href=\"{}\"/>\n",
                    escape_attr(&relativize(&a_dir, s))
                )
            })
            .collect();
        merged.insert_str(head_end, &links);
        changes.note(format!(
            "{} stylesheet link(s) carried over from {}",
            added_links.len(),
            next.rsplit('/').next().unwrap_or(&next)
        ));
    }
    let merged = retarget(&merged, path, &mut |target, frag| {
        (target == next || target == path).then(|| (path.to_string(), frag.to_string()))
    });
    pkg.replace(path, merged.into_bytes());
    changes.touch(path);

    for m in members(pkg)? {
        if m.path == path || m.path == next {
            continue;
        }
        let rewritable = matches!(m.role, MemberRole::Text | MemberRole::Nav | MemberRole::Ncx);
        if !rewritable {
            continue;
        }
        let doc = member_text(pkg, &m.path)?;
        let out = retarget(&doc, &m.path, &mut |target, frag| {
            (target == next).then(|| (path.to_string(), frag.to_string()))
        });
        if out != doc {
            pkg.replace(&m.path, out.into_bytes());
            changes.touch(&m.path);
        }
    }

    remove_manifest_item(pkg, &next)?;
    let mut opf_text = member_text(pkg, &opf_path)?;
    opf_text = remove_itemref(&opf_text, &next_id);
    opf_text = retarget(&opf_text, &opf_path, &mut |target, frag| {
        (target == next).then(|| (path.to_string(), frag.to_string()))
    });
    pkg.replace(&opf_path, opf_text.into_bytes());
    changes.touch(&opf_path);
    pkg.remove(&next);
    changes.drop(&next);
    changes.note(format!(
        "{} folded into {}",
        next.rsplit('/').next().unwrap_or(&next),
        path.rsplit('/').next().unwrap_or(path)
    ));
    Ok(changes)
}

fn member_text(pkg: &EpubPackage, path: &str) -> io::Result<String> {
    let bytes = pkg
        .get(path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no member {path}")))?;
    Ok(decode_text(bytes, extract_xml_encoding(bytes)).into_owned())
}

fn text_of(pkg: &EpubPackage, path: &str) -> String {
    member_text(pkg, path).unwrap_or_default()
}

struct Cut {
    at: usize,
    ancestors: Vec<(String, String)>,
}

fn split_point(text: &str, from: usize, to: usize, offset: usize) -> Option<Cut> {
    let mut stack: Vec<(String, String, usize)> = Vec::new();
    let mut best: Option<Cut> = None;
    for tok in tokens(&text[from..to]) {
        let Tok::Tag {
            start,
            end,
            name,
            closing,
            self_closing,
        } = tok
        else {
            continue;
        };
        let abs_start = from + start;
        if abs_start > offset {
            break;
        }
        if closing {
            while let Some((top, _, _)) = stack.pop() {
                if top == name {
                    break;
                }
            }
            continue;
        }
        let raw = text[abs_start..from + end].to_string();
        if is_block(&name) {
            best = Some(Cut {
                at: abs_start,
                ancestors: stack
                    .iter()
                    .map(|(n, t, _)| (n.clone(), t.clone()))
                    .collect(),
            });
        }
        if !self_closing && !is_void(&name) {
            stack.push((name, raw, abs_start));
        }
    }
    best
}

fn only_open_tags(before: &str, ancestors: usize) -> bool {
    let mut opens = 0;
    for tok in tokens(before) {
        match tok {
            Tok::Text { start, end } => {
                if !before[start..end]
                    .trim_matches(|c: char| c.is_ascii_whitespace())
                    .is_empty()
                {
                    return false;
                }
            }
            Tok::Tag { closing, .. } => {
                if closing {
                    return false;
                }
                opens += 1;
            }
        }
    }
    opens <= ancestors
}

fn ids_in(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for tok in tokens(text) {
        if let Tok::Tag {
            start,
            end,
            closing: false,
            ..
        } = tok
            && let Some(id) = attr_value(&text[start..end], "id")
        {
            out.insert(id);
        }
    }
    out
}

fn manifest_id_of(
    opf: &crate::formats::epub::OpfData,
    opf_base: &str,
    path: &str,
) -> Option<String> {
    opf.manifest
        .iter()
        .find(|(_, (href, _))| format!("{opf_base}{}", percent_decode(href)) == path)
        .map(|(id, _)| id.clone())
}

fn free_sibling_name(pkg: &EpubPackage, path: &str) -> String {
    let (dir, file) = path.rsplit_once('/').map_or(("", path), |(d, f)| (d, f));
    let (stem, ext) = file.rsplit_once('.').unwrap_or((file, "xhtml"));
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    (2..)
        .map(|n| format!("{prefix}{stem}-{n}.{ext}"))
        .find(|p| !pkg.contains(p))
        .unwrap_or_else(|| format!("{prefix}{stem}-split.{ext}"))
}

const LINK_ATTRS: &[&str] = &["href", "src", "xlink:href", "poster", "data"];

fn retarget<F>(text: &str, doc_path: &str, map: &mut F) -> String
where
    F: FnMut(&str, &str) -> Option<(String, String)>,
{
    let doc_dir = dir_of(doc_path);
    rewrite_tags(text, |_, tag| {
        let mut out: Option<String> = None;
        for a in attributes(tag) {
            if !LINK_ATTRS.contains(&a.name.as_str()) || a.value.is_empty() {
                continue;
            }
            if a.value.contains("://") || a.value.starts_with("mailto:") {
                continue;
            }
            let (p, frag) = split_fragment(&a.value);
            let frag = frag.strip_prefix('#').unwrap_or(frag);
            let abs = if p.is_empty() {
                doc_path.to_string()
            } else {
                resolve_href(&doc_dir, p)
            };
            let Some((new_abs, new_frag)) = map(&abs, frag) else {
                continue;
            };
            let mut href = if new_abs == doc_path {
                String::new()
            } else {
                relativize(&doc_dir, &new_abs).replace(' ', "%20")
            };
            if !new_frag.is_empty() {
                href.push('#');
                href.push_str(&new_frag);
            }
            if href.is_empty() || href == a.value {
                continue;
            }
            let current = out.as_deref().unwrap_or(tag);
            out = Some(set_attr(current, &a.name, Some(&href)));
        }
        out
    })
}

fn rebase_to(text: &str, from_dir: &str, to_dir: &str) -> String {
    rewrite_tags(text, |_, tag| {
        let mut out: Option<String> = None;
        for a in attributes(tag) {
            if !LINK_ATTRS.contains(&a.name.as_str()) || a.value.is_empty() {
                continue;
            }
            if a.value.contains("://") || a.value.starts_with('#') || a.value.starts_with("mailto:")
            {
                continue;
            }
            let (p, frag) = split_fragment(&a.value);
            let abs = resolve_href(from_dir, p);
            let href = format!("{}{frag}", relativize(to_dir, &abs).replace(' ', "%20"));
            if href != a.value {
                let current = out.as_deref().unwrap_or(tag);
                out = Some(set_attr(current, &a.name, Some(&href)));
            }
        }
        out
    })
}

fn stylesheet_links(text: &str, doc_dir: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tok in tokens(text) {
        if let Tok::Tag {
            start,
            end,
            name,
            closing: false,
            ..
        } = tok
            && name == "link"
        {
            let tag = &text[start..end];
            let rel = attr_value(tag, "rel")
                .unwrap_or_default()
                .to_ascii_lowercase();
            if rel.split_whitespace().any(|r| r == "stylesheet")
                && let Some(href) = attr_value(tag, "href")
            {
                out.push(resolve_href(doc_dir, &href));
            }
        }
    }
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
                "<?xml version=\"1.0\"?>\n<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"id\">\n  <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:identifier id=\"id\">x</dc:identifier><dc:title>T</dc:title><dc:language>en</dc:language></metadata>\n  <manifest>\n    <item href=\"nav.xhtml\" id=\"nav\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n    <item href=\"text/a.xhtml\" id=\"a\" media-type=\"application/xhtml+xml\"/>\n    <item href=\"text/b.xhtml\" id=\"b\" media-type=\"application/xhtml+xml\"/>\n    <item href=\"s.css\" id=\"s\" media-type=\"text/css\"/>\n  </manifest>\n  <spine>\n    <itemref idref=\"nav\"/>\n    <itemref idref=\"a\"/>\n    <itemref idref=\"b\" linear=\"no\"/>\n  </spine>\n</package>\n",
            ),
            (
                "OEBPS/nav.xhtml",
                "<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\"><head><title>n</title></head><body><nav epub:type=\"toc\"><ol><li><a href=\"text/a.xhtml\">A</a></li><li><a href=\"text/a.xhtml#two\">Two</a></li><li><a href=\"text/b.xhtml#bb\">B</a></li></ol></nav></body></html>",
            ),
            (
                "OEBPS/text/a.xhtml",
                "<?xml version=\"1.0\"?>\n<html xmlns=\"http://www.w3.org/1999/xhtml\">\n<head><title>a</title><link rel=\"stylesheet\" href=\"../s.css\"/></head>\n<body class=\"p-text\">\n<h1 id=\"one\">One</h1>\n<p>see <a href=\"#two\">two</a></p>\n<h1 id=\"two\">Two</h1>\n<p>back to <a href=\"#one\">one</a> and <a href=\"b.xhtml#bb\">b</a></p>\n</body>\n</html>\n",
            ),
            (
                "OEBPS/text/b.xhtml",
                "<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>b</title><link rel=\"stylesheet\" href=\"../s.css\"/><link rel=\"stylesheet\" href=\"../t.css\"/></head><body><p id=\"bb\">bee <img src=\"../img/x.png\"/> <a href=\"a.xhtml#one\">one</a></p></body></html>",
            ),
            ("OEBPS/s.css", "p {}"),
        ])
    }

    fn text(pkg: &EpubPackage, p: &str) -> String {
        String::from_utf8(pkg.get(p).unwrap().to_vec()).unwrap()
    }

    #[test]
    fn splits_before_the_block_at_the_cursor_and_moves_links() {
        let mut pkg = book();
        let changes = split_document(&mut pkg, "OEBPS/text/a.xhtml", 7, 3).unwrap();
        assert_eq!(
            changes.added,
            vec![("OEBPS/text/a-2.xhtml".to_string(), XHTML.to_string())]
        );
        let a = text(&pkg, "OEBPS/text/a.xhtml");
        assert_eq!(
            a,
            "<?xml version=\"1.0\"?>\n<html xmlns=\"http://www.w3.org/1999/xhtml\">\n<head><title>a</title><link rel=\"stylesheet\" href=\"../s.css\"/></head>\n<body class=\"p-text\">\n<h1 id=\"one\">One</h1>\n<p>see <a href=\"a-2.xhtml#two\">two</a></p>\n</body>\n</html>\n"
        );
        let a2 = text(&pkg, "OEBPS/text/a-2.xhtml");
        assert_eq!(
            a2,
            "<?xml version=\"1.0\"?>\n<html xmlns=\"http://www.w3.org/1999/xhtml\">\n<head><title>a</title><link rel=\"stylesheet\" href=\"../s.css\"/></head>\n<body class=\"p-text\">\n<h1 id=\"two\">Two</h1>\n<p>back to <a href=\"a.xhtml#one\">one</a> and <a href=\"b.xhtml#bb\">b</a></p>\n</body>\n</html>\n"
        );
        let nav = text(&pkg, "OEBPS/nav.xhtml");
        assert!(nav.contains("href=\"text/a-2.xhtml#two\""));
        assert!(nav.contains("href=\"text/a.xhtml\">A"));
        let opf = text(&pkg, "OEBPS/content.opf");
        assert!(opf.contains(
            "<item href=\"text/a-2.xhtml\" id=\"a-2\" media-type=\"application/xhtml+xml\"/>"
        ));
        assert!(opf.contains("<itemref idref=\"a\"/>\n    <itemref idref=\"a-2\"/>\n    <itemref idref=\"b\" linear=\"no\"/>"));
        assert!(split_document(&mut pkg, "OEBPS/text/a.xhtml", 5, 1).is_err());
    }

    #[test]
    fn splits_inside_a_wrapper_by_closing_and_reopening_it() {
        let mut pkg = package_from(&[
            (
                "OEBPS/content.opf",
                "<?xml version=\"1.0\"?>\n<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"id\">\n<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:identifier id=\"id\">x</dc:identifier><dc:title>T</dc:title><dc:language>en</dc:language></metadata>\n<manifest>\n<item href=\"a.xhtml\" id=\"a\" media-type=\"application/xhtml+xml\"/>\n</manifest>\n<spine>\n<itemref idref=\"a\"/>\n</spine>\n</package>\n",
            ),
            (
                "OEBPS/a.xhtml",
                "<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>a</title></head>\n<body>\n<div class=\"main\" id=\"m\">\n<p>one</p>\n<p>two <span>x</span></p>\n</div>\n</body>\n</html>\n",
            ),
        ]);
        assert!(split_document(&mut pkg, "OEBPS/a.xhtml", 4, 2).is_err());
        split_document(&mut pkg, "OEBPS/a.xhtml", 5, 8).unwrap();
        assert_eq!(
            text(&pkg, "OEBPS/a.xhtml"),
            "<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>a</title></head>\n<body>\n<div class=\"main\" id=\"m\">\n<p>one</p>\n</div>\n</body>\n</html>\n"
        );
        assert_eq!(
            text(&pkg, "OEBPS/a-2.xhtml"),
            "<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>a</title></head>\n<body>\n<div class=\"main\">\n<p>two <span>x</span></p>\n</div>\n</body>\n</html>\n"
        );
    }

    #[test]
    fn merges_the_next_document_and_retargets_everything() {
        let mut pkg = book();
        let changes = merge_with_next(&mut pkg, "OEBPS/text/a.xhtml").unwrap();
        assert_eq!(changes.removed, vec!["OEBPS/text/b.xhtml"]);
        assert!(!pkg.contains("OEBPS/text/b.xhtml"));
        let a = text(&pkg, "OEBPS/text/a.xhtml");
        assert!(
            a.contains("<link rel=\"stylesheet\" type=\"text/css\" href=\"../t.css\"/>\n</head>")
        );
        assert!(a.ends_with("<p>back to <a href=\"#one\">one</a> and <a href=\"#bb\">b</a></p>\n<p id=\"bb\">bee <img src=\"../img/x.png\"/> <a href=\"#one\">one</a></p>\n</body>\n</html>\n"));
        let nav = text(&pkg, "OEBPS/nav.xhtml");
        assert!(nav.contains("href=\"text/a.xhtml#bb\""));
        let opf = text(&pkg, "OEBPS/content.opf");
        assert!(!opf.contains("b.xhtml"));
        assert!(!opf.contains("idref=\"b\""));
        assert!(opf.contains("<itemref idref=\"a\"/>\n  </spine>"));
        assert!(merge_with_next(&mut pkg, "OEBPS/text/a.xhtml").is_err());
    }
}
