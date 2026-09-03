//! `upgrade_to_epub3` rewrites an EPUB 2 package's version, metadata,
//! navigation document, manifest properties and DOCTYPEs in place. Bodies,
//! stylesheets, images and fonts pass through byte for byte.

use std::collections::BTreeSet;
use std::io;

use crate::formats::epub::edit::{Changes, EpubPackage, attr_value, escape_attr, escape_text};
use crate::formats::epub::manifest::{MemberRole, members, set_item_properties};
use crate::formats::epub::markup::{
    Tok, attributes, content_properties, rewrite_tags, set_attr, tokens,
};
use crate::formats::epub::nav_doc::{render_landmarks_nav, render_nav_doc, render_toc_nav};
use crate::formats::epub::structure::{dir_of, rebase_toc, relativize, resolve_href};
use crate::formats::epub::{parse_ncx, parse_opf, parse_opf_guide};
use crate::model::{Landmark, TocEntry};
use crate::util::{decode_text, extract_xml_encoding, percent_decode, time_now_iso8601_utc};

pub fn upgrade_to_epub3(pkg: &mut EpubPackage) -> io::Result<Changes> {
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let mut opf_text = text_of(pkg, &opf_path)?;
    let opf = parse_opf(&opf_text).map_err(io::Error::other)?;
    if opf.version.starts_with('3') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the package is already EPUB 3",
        ));
    }
    let mut changes = Changes::default();

    let uid_value = opf.unique_identifier.clone().filter(|s| !s.is_empty());
    let (metadata, uid_value, cover_id, notes) = upgrade_metadata(&opf_text, uid_value);
    opf_text = metadata;
    for n in notes {
        changes.note(n);
    }

    opf_text = rewrite_tags(&opf_text, |name, tag| {
        if name != "package" {
            return None;
        }
        let mut out = set_attr(tag, "version", Some("3.0"));
        if attr_value(&out, "unique-identifier").is_none() {
            out = set_attr(&out, "unique-identifier", Some("pub-id"));
        }
        Some(out)
    });

    let mut declared = 0;
    for m in members(pkg)? {
        let Some(id) = m.id.as_deref() else {
            continue;
        };
        let mut props: Vec<String> = Vec::new();
        if matches!(m.role, MemberRole::Text | MemberRole::Nav) {
            let text = text_of(pkg, &m.path)?;
            props.extend(content_properties(&text).iter().map(|s| s.to_string()));
        }
        if cover_id.as_deref() == Some(id) && m.role == MemberRole::Image {
            props.push("cover-image".to_string());
        }
        let existing = item_properties(&opf_text, id);
        let merged: Vec<String> = existing
            .iter()
            .cloned()
            .chain(props.into_iter().filter(|p| !existing.contains(p)))
            .collect();
        if merged.len() > existing.len() {
            declared += merged.len() - existing.len();
            opf_text = set_item_properties(&opf_text, id, &merged.join(" "));
        }
    }
    if declared > 0 {
        changes.note(format!(
            "{declared} manifest propert{} declared",
            if declared == 1 { "y" } else { "ies" }
        ));
    }
    opf_text = fix_font_types(&opf_text, &mut changes);

    if opf.nav_href.is_none() {
        let entries = ncx_entries(pkg, &opf, &opf_base);
        let landmarks: Vec<Landmark> = parse_opf_guide(&opf_text)
            .unwrap_or_default()
            .into_iter()
            .map(|mut l| {
                l.href = resolve_href(&opf_base, &l.href);
                l
            })
            .collect();
        let entries = if entries.is_empty() {
            first_document_entry(&opf, &opf_base)
        } else {
            entries
        };
        if !entries.is_empty() {
            let nav_path = free_path(pkg, &format!("{opf_base}nav.xhtml"));
            let nav_dir = dir_of(&nav_path);
            let mut body = render_toc_nav(&entries, &nav_dir);
            if !landmarks.is_empty() {
                body.push('\n');
                body.push_str(&render_landmarks_nav(&landmarks, &nav_dir));
            }
            let title = if opf.metadata.title.is_empty() {
                "Contents"
            } else {
                &opf.metadata.title
            };
            let lang = if opf.metadata.language.is_empty() {
                "en"
            } else {
                &opf.metadata.language
            };
            pkg.set(&nav_path, render_nav_doc(&body, lang, title).into_bytes());
            changes.add(&nav_path, "application/xhtml+xml");
            let id = free_id(&opf_text, "nav");
            opf_text = insert_manifest_item(
                &opf_text,
                &id,
                &relativize(&opf_base, &nav_path).replace(' ', "%20"),
                "application/xhtml+xml",
                "nav",
            );
            changes.note(format!(
                "navigation document written with {} entr{} and {} landmark(s)",
                count(&entries),
                if count(&entries) == 1 { "y" } else { "ies" },
                landmarks.len()
            ));
        }
    }

    pkg.replace(&opf_path, opf_text.into_bytes());
    changes.touch(&opf_path);

    if let Some(ncx_href) = &opf.ncx_href
        && let Some(uid) = &uid_value
    {
        let ncx_path = format!("{opf_base}{}", percent_decode(ncx_href));
        if let Ok(ncx) = text_of(pkg, &ncx_path) {
            let fixed = set_ncx_uid(&ncx, uid);
            if fixed != ncx {
                pkg.replace(&ncx_path, fixed.into_bytes());
                changes.touch(&ncx_path);
            }
        }
    }

    let mut doctypes = 0;
    for m in members(pkg)? {
        if !matches!(m.role, MemberRole::Text | MemberRole::Nav)
            || changes.added.iter().any(|(p, _)| *p == m.path)
        {
            continue;
        }
        let text = text_of(pkg, &m.path)?;
        let fixed = html5_doctype(&text);
        if fixed != text {
            doctypes += 1;
            pkg.replace(&m.path, fixed.into_bytes());
            changes.touch(&m.path);
        }
    }
    if doctypes > 0 {
        changes.note(format!("{doctypes} document DOCTYPE(s) set to HTML"));
    }
    Ok(changes)
}

fn text_of(pkg: &EpubPackage, path: &str) -> io::Result<String> {
    let bytes = pkg
        .get(path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no member {path}")))?;
    Ok(decode_text(bytes, extract_xml_encoding(bytes)).into_owned())
}

fn count(entries: &[TocEntry]) -> usize {
    entries.iter().map(|e| 1 + count(&e.children)).sum()
}

fn upgrade_metadata(
    opf: &str,
    uid_value: Option<String>,
) -> (String, Option<String>, Option<String>, Vec<String>) {
    let mut notes = Vec::new();
    let Some(meta_start) = opf.find("<metadata") else {
        return (opf.to_string(), uid_value, None, notes);
    };
    let Some(meta_end) = opf[meta_start..]
        .find("</metadata>")
        .map(|i| meta_start + i)
    else {
        return (opf.to_string(), uid_value, None, notes);
    };
    let region = &opf[meta_start..meta_end];
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for tok in tokens(opf) {
        if let Tok::Tag {
            start,
            end,
            closing: false,
            ..
        } = tok
            && let Some(id) = attr_value(&opf[start..end], "id")
        {
            ids.insert(id);
        }
    }
    let has_modified = region.contains("dcterms:modified");
    let mut out = String::with_capacity(opf.len() + 256);
    let mut refines: Vec<String> = Vec::new();
    let mut modified: Option<String> = None;
    let mut cover_id: Option<String> = None;
    let mut uid_value = uid_value;
    let mut first_identifier_id: Option<String> = None;
    let mut dates_seen = 0;
    let mut skip_until: Option<usize> = None;
    let mut dropped_attrs = 0;
    let toks = tokens(region);
    let mut pos = 0;
    for (i, tok) in toks.iter().enumerate() {
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
        if let Some(until) = skip_until {
            if *start < until {
                continue;
            }
            skip_until = None;
        }
        if *closing {
            continue;
        }
        let tag = &region[*start..*end];
        let local = name.rsplit(':').next().unwrap_or(name).to_string();
        let attrs = attributes(tag);
        let opf_attrs: Vec<&crate::formats::epub::markup::Attr> = attrs
            .iter()
            .filter(|a| a.name.starts_with("opf:"))
            .collect();
        let element_text = || -> String {
            match toks.get(i + 1) {
                Some(Tok::Text { start: ts, end: te }) => region[*ts..*te].trim().to_string(),
                _ => String::new(),
            }
        };
        let element_end = |i: usize| -> usize {
            let mut depth = 0usize;
            for t in &toks[i..] {
                if let Tok::Tag {
                    end,
                    closing,
                    self_closing,
                    ..
                } = t
                {
                    if *closing {
                        if depth <= 1 {
                            return *end;
                        }
                        depth -= 1;
                    } else if !*self_closing {
                        depth += 1;
                    } else if depth == 0 {
                        return *end;
                    }
                }
            }
            region.len()
        };
        match local.as_str() {
            "meta" if attr_value(tag, "name").as_deref() == Some("cover") => {
                cover_id = attr_value(tag, "content");
                continue;
            }
            "date" => {
                let event = attr_value(tag, "opf:event").unwrap_or_default();
                if event == "modification" {
                    if !has_modified && modified.is_none() {
                        modified = Some(normalize_datetime(&element_text()));
                    }
                    let end = element_end(i);
                    out.push_str(&region[pos..*start]);
                    pos = end;
                    skip_until = Some(end);
                    continue;
                }
                dates_seen += 1;
                if dates_seen > 1 {
                    notes.push(format!("a second dc:date ({}) dropped", element_text()));
                    let end = element_end(i);
                    out.push_str(&region[pos..*start]);
                    pos = end;
                    skip_until = Some(end);
                    continue;
                }
            }
            _ => {}
        }
        if opf_attrs.is_empty() {
            if local == "identifier" && first_identifier_id.is_none() {
                first_identifier_id = attr_value(tag, "id");
                if uid_value.is_none() {
                    uid_value = Some(element_text());
                }
            }
            continue;
        }
        let mut new_tag = tag.to_string();
        let mut id = attr_value(tag, "id");
        let needs_id = matches!(
            local.as_str(),
            "creator" | "contributor" | "title" | "identifier"
        ) && opf_attrs.iter().any(|a| a.name != "opf:event");
        if needs_id && id.is_none() {
            let fresh = fresh_id(&ids, &local);
            ids.insert(fresh.clone());
            new_tag = set_attr(&new_tag, "id", Some(&fresh));
            id = Some(fresh);
        }
        for a in &opf_attrs {
            dropped_attrs += 1;
            new_tag = set_attr(&new_tag, &a.name, None);
            let Some(id) = &id else {
                continue;
            };
            match (local.as_str(), a.name.as_str()) {
                ("creator" | "contributor", "opf:role") => refines.push(format!(
                    "<meta refines=\"#{}\" property=\"role\" scheme=\"marc:relators\">{}</meta>",
                    escape_attr(id),
                    escape_text(&a.value)
                )),
                (_, "opf:file-as") => refines.push(format!(
                    "<meta refines=\"#{}\" property=\"file-as\">{}</meta>",
                    escape_attr(id),
                    escape_text(&a.value)
                )),
                ("identifier", "opf:scheme") => refines.push(format!(
                    "<meta refines=\"#{}\" property=\"identifier-type\">{}</meta>",
                    escape_attr(id),
                    escape_text(&a.value)
                )),
                _ => {}
            }
        }
        if local == "identifier" && first_identifier_id.is_none() {
            first_identifier_id = id.clone();
            if uid_value.is_none() {
                uid_value = Some(element_text());
            }
        }
        if *self_closing || new_tag != tag {
            out.push_str(&region[pos..*start]);
            out.push_str(&new_tag);
            pos = *end;
        }
    }
    out.push_str(&region[pos..]);
    if dropped_attrs > 0 {
        notes.push(format!(
            "{dropped_attrs} opf: attribute(s) rewritten as refining meta elements"
        ));
    }
    if !has_modified {
        refines.push(format!(
            "<meta property=\"dcterms:modified\">{}</meta>",
            modified.unwrap_or_else(time_now_iso8601_utc)
        ));
    }
    let indent = region
        .rfind('\n')
        .map(|i| {
            region[i + 1..]
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "    ".to_string());
    let tail = out.trim_end_matches([' ', '\t']).len();
    out.truncate(tail);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    for r in &refines {
        out.push_str(&indent);
        out.push_str(r);
        out.push('\n');
    }
    let close_indent: String = indent.chars().skip(2).collect();
    out.push_str(&close_indent);
    let mut result = format!("{}{out}{}", &opf[..meta_start], &opf[meta_end..]);
    if attr_value(&package_tag(&result), "unique-identifier").is_none() {
        if let Some(first) = first_identifier_id {
            result = rewrite_tags(&result, |name, tag| {
                (name == "package").then(|| set_attr(tag, "unique-identifier", Some(&first)))
            });
        } else if result.contains("<dc:identifier") {
            result = result.replacen("<dc:identifier", "<dc:identifier id=\"pub-id\"", 1);
            result = rewrite_tags(&result, |name, tag| {
                (name == "package").then(|| set_attr(tag, "unique-identifier", Some("pub-id")))
            });
        }
    }
    (result, uid_value, cover_id, notes)
}

fn package_tag(opf: &str) -> String {
    for tok in tokens(opf) {
        if let Tok::Tag {
            start,
            end,
            name,
            closing: false,
            ..
        } = tok
            && name == "package"
        {
            return opf[start..end].to_string();
        }
    }
    String::new()
}

fn fresh_id(ids: &BTreeSet<String>, base: &str) -> String {
    (1..)
        .map(|n| format!("{base}{n}"))
        .find(|c| !ids.contains(c))
        .unwrap_or_else(|| format!("{base}-x"))
}

fn free_id(opf: &str, preferred: &str) -> String {
    let taken: BTreeSet<String> = tokens(opf)
        .into_iter()
        .filter_map(|t| match t {
            Tok::Tag {
                start,
                end,
                closing: false,
                ..
            } => attr_value(&opf[start..end], "id"),
            _ => None,
        })
        .collect();
    if !taken.contains(preferred) {
        return preferred.to_string();
    }
    (2..)
        .map(|n| format!("{preferred}-{n}"))
        .find(|c| !taken.contains(c))
        .unwrap_or_else(|| format!("{preferred}-x"))
}

fn free_path(pkg: &EpubPackage, preferred: &str) -> String {
    if !pkg.contains(preferred) {
        return preferred.to_string();
    }
    let (stem, ext) = preferred.rsplit_once('.').unwrap_or((preferred, "xhtml"));
    (2..)
        .map(|n| format!("{stem}-{n}.{ext}"))
        .find(|p| !pkg.contains(p))
        .unwrap_or_else(|| format!("{stem}-x.{ext}"))
}

fn insert_manifest_item(
    opf: &str,
    id: &str,
    href: &str,
    media_type: &str,
    properties: &str,
) -> String {
    let Some(close) = opf.find("</manifest>") else {
        return opf.to_string();
    };
    let line_start = opf[..close].rfind('\n').map_or(0, |i| i + 1);
    let indent = opf[..close]
        .rfind("<item")
        .map(|i| {
            let ls = opf[..i].rfind('\n').map_or(0, |j| j + 1);
            opf[ls..i].to_string()
        })
        .filter(|s| s.chars().all(char::is_whitespace))
        .unwrap_or_else(|| format!("{}  ", &opf[line_start..close]));
    let item = format!(
        "{indent}<item href=\"{}\" id=\"{}\" media-type=\"{}\" properties=\"{}\"/>\n",
        escape_attr(href),
        escape_attr(id),
        escape_attr(media_type),
        escape_attr(properties)
    );
    format!("{}{item}{}", &opf[..line_start], &opf[line_start..])
}

fn item_properties(opf: &str, id: &str) -> Vec<String> {
    for tok in tokens(opf) {
        if let Tok::Tag {
            start,
            end,
            name,
            closing: false,
            ..
        } = tok
            && name == "item"
            && attr_value(&opf[start..end], "id").as_deref() == Some(id)
        {
            return attr_value(&opf[start..end], "properties")
                .map(|p| p.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default();
        }
    }
    Vec::new()
}

fn fix_font_types(opf: &str, changes: &mut Changes) -> String {
    let mut fixed = 0;
    let out = rewrite_tags(opf, |name, tag| {
        if name != "item" {
            return None;
        }
        let mt = attr_value(tag, "media-type")?;
        let new = match mt.to_ascii_lowercase().as_str() {
            "application/x-font-ttf" | "application/x-font-truetype" | "application/truetype" => {
                "font/ttf"
            }
            "application/x-font-opentype"
            | "application/vnd.ms-opentype"
            | "application/x-font-otf" => "font/otf",
            "application/font-woff" | "application/x-font-woff" => "font/woff",
            "application/font-woff2" => "font/woff2",
            _ => return None,
        };
        fixed += 1;
        Some(set_attr(tag, "media-type", Some(new)))
    });
    if fixed > 0 {
        changes.note(format!("{fixed} font media type(s) modernized"));
    }
    out
}

fn ncx_entries(
    pkg: &EpubPackage,
    opf: &crate::formats::epub::OpfData,
    opf_base: &str,
) -> Vec<TocEntry> {
    let Some(href) = &opf.ncx_href else {
        return Vec::new();
    };
    let path = format!("{opf_base}{}", percent_decode(href));
    let Ok(ncx) = text_of(pkg, &path) else {
        return Vec::new();
    };
    match parse_ncx(&ncx) {
        Ok(entries) => rebase_toc(&entries, &dir_of(&path)),
        Err(_) => Vec::new(),
    }
}

fn first_document_entry(opf: &crate::formats::epub::OpfData, opf_base: &str) -> Vec<TocEntry> {
    opf.spine_ids
        .iter()
        .filter_map(|id| opf.manifest.get(id))
        .find(|(_, mt)| mt.eq_ignore_ascii_case("application/xhtml+xml"))
        .map(|(href, _)| {
            vec![TocEntry {
                title: if opf.metadata.title.is_empty() {
                    "Start".to_string()
                } else {
                    opf.metadata.title.clone()
                },
                href: format!("{opf_base}{}", percent_decode(href)),
                children: Vec::new(),
                play_order: None,
                target: None,
            }]
        })
        .unwrap_or_default()
}

fn set_ncx_uid(ncx: &str, uid: &str) -> String {
    rewrite_tags(ncx, |name, tag| {
        (name == "meta" && attr_value(tag, "name").as_deref() == Some("dtb:uid"))
            .then(|| set_attr(tag, "content", Some(uid)))
    })
}

fn normalize_datetime(s: &str) -> String {
    let s = s.trim();
    if s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
        return format!("{s}T00:00:00Z");
    }
    if s.len() == 19 && s.as_bytes()[10] == b'T' {
        return format!("{s}Z");
    }
    if let Some(stripped) = s.strip_suffix("+00:00") {
        return format!("{stripped}Z");
    }
    if s.len() == 4 && s.chars().all(|c| c.is_ascii_digit()) {
        return format!("{s}-01-01T00:00:00Z");
    }
    s.to_string()
}

pub(crate) fn html5_doctype(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let Some(start) = lower.find("<!doctype") else {
        return text.to_string();
    };
    let Some(end_rel) = text[start..].find('>') else {
        return text.to_string();
    };
    let end = start + end_rel + 1;
    let current = &text[start..end];
    if current.eq_ignore_ascii_case("<!DOCTYPE html>") {
        return text.to_string();
    }
    format!("{}<!DOCTYPE html>{}", &text[..start], &text[end..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::epub::edit::tests::package_from;

    fn epub2() -> EpubPackage {
        package_from(&[
            (
                "OEBPS/content.opf",
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"2.0\" unique-identifier=\"uid\">\n  <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:opf=\"http://www.idpf.org/2007/opf\">\n    <dc:identifier id=\"uid\" opf:scheme=\"ISBN\">9780000000000</dc:identifier>\n    <dc:title>Old Book</dc:title>\n    <dc:creator opf:role=\"aut\" opf:file-as=\"Doe, J\">Jane Doe</dc:creator>\n    <dc:contributor opf:role=\"ill\">Ann</dc:contributor>\n    <dc:language>en</dc:language>\n    <dc:date opf:event=\"publication\">2001-02-03</dc:date>\n    <dc:date opf:event=\"modification\">2002-03-04</dc:date>\n    <meta name=\"cover\" content=\"cov\"/>\n  </metadata>\n  <manifest>\n    <item href=\"toc.ncx\" id=\"ncx\" media-type=\"application/x-dtbncx+xml\"/>\n    <item href=\"cover.jpg\" id=\"cov\" media-type=\"image/jpeg\"/>\n    <item href=\"a.xhtml\" id=\"a\" media-type=\"application/xhtml+xml\"/>\n    <item href=\"b.xhtml\" id=\"b\" media-type=\"application/xhtml+xml\"/>\n    <item href=\"f.ttf\" id=\"f\" media-type=\"application/x-font-ttf\"/>\n  </manifest>\n  <spine toc=\"ncx\">\n    <itemref idref=\"a\"/>\n    <itemref idref=\"b\"/>\n  </spine>\n  <guide>\n    <reference type=\"cover\" title=\"Cover\" href=\"a.xhtml\"/>\n    <reference type=\"text\" title=\"Start\" href=\"b.xhtml\"/>\n  </guide>\n</package>\n",
            ),
            (
                "OEBPS/toc.ncx",
                "<?xml version=\"1.0\"?>\n<ncx xmlns=\"http://www.daisy.org/z3986/2005/ncx/\" version=\"2005-1\"><head><meta name=\"dtb:uid\" content=\"wrong\"/></head><docTitle><text>Old Book</text></docTitle><navMap><navPoint id=\"n1\" playOrder=\"1\"><navLabel><text>A</text></navLabel><content src=\"a.xhtml\"/></navPoint><navPoint id=\"n2\" playOrder=\"2\"><navLabel><text>B</text></navLabel><content src=\"b.xhtml#x\"/></navPoint></navMap></ncx>",
            ),
            (
                "OEBPS/a.xhtml",
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.1//EN\"\n  \"http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd\">\n<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>a</title></head><body><p><img src=\"cover.jpg\" alt=\"\"/></p></body></html>",
            ),
            (
                "OEBPS/b.xhtml",
                "<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>b</title></head><body><p id=\"x\">b</p><svg xmlns=\"http://www.w3.org/2000/svg\"/></body></html>",
            ),
            ("OEBPS/cover.jpg", "notreallyjpeg"),
            ("OEBPS/f.ttf", "notreallyfont"),
        ])
    }

    fn text(pkg: &EpubPackage, p: &str) -> String {
        String::from_utf8(pkg.get(p).unwrap().to_vec()).unwrap()
    }

    #[test]
    fn upgrades_package_metadata_nav_and_documents() {
        let mut pkg = epub2();
        let changes = upgrade_to_epub3(&mut pkg).unwrap();
        let opf = text(&pkg, "OEBPS/content.opf");
        assert!(opf.contains("version=\"3.0\""));
        assert!(opf.contains("<dc:identifier id=\"uid\">9780000000000</dc:identifier>"));
        assert!(opf.contains("<dc:creator id=\"creator1\">Jane Doe</dc:creator>"));
        assert!(opf.contains(
            "<meta refines=\"#creator1\" property=\"role\" scheme=\"marc:relators\">aut</meta>"
        ));
        assert!(opf.contains("<meta refines=\"#creator1\" property=\"file-as\">Doe, J</meta>"));
        assert!(opf.contains(
            "<meta refines=\"#contributor1\" property=\"role\" scheme=\"marc:relators\">ill</meta>"
        ));
        assert!(opf.contains("<meta refines=\"#uid\" property=\"identifier-type\">ISBN</meta>"));
        assert!(opf.contains("<dc:date>2001-02-03</dc:date>"));
        assert!(!opf.contains("opf:event"));
        assert!(opf.contains("<meta property=\"dcterms:modified\">2002-03-04T00:00:00Z</meta>"));
        assert!(opf.contains("<meta name=\"cover\" content=\"cov\"/>"));
        assert!(opf.contains("id=\"cov\" media-type=\"image/jpeg\" properties=\"cover-image\"/>"));
        assert!(opf.contains("id=\"b\" media-type=\"application/xhtml+xml\" properties=\"svg\"/>"));
        assert!(opf.contains("media-type=\"font/ttf\""));
        assert!(opf.contains("<item href=\"nav.xhtml\" id=\"nav\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>"));
        assert!(opf.contains("<guide>"));
        let nav = text(&pkg, "OEBPS/nav.xhtml");
        assert!(nav.contains("<a href=\"b.xhtml#x\">B</a>"));
        assert!(nav.contains("<a epub:type=\"bodymatter\" href=\"b.xhtml\">Start</a>"));
        assert!(nav.contains("<a epub:type=\"cover\" href=\"a.xhtml\">Cover</a>"));
        let ncx = text(&pkg, "OEBPS/toc.ncx");
        assert!(ncx.contains("<meta name=\"dtb:uid\" content=\"9780000000000\"/>"));
        let a = text(&pkg, "OEBPS/a.xhtml");
        assert!(
            a.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE html>\n<html")
        );
        assert_eq!(
            changes.added,
            vec![(
                "OEBPS/nav.xhtml".to_string(),
                "application/xhtml+xml".to_string()
            )]
        );
        assert!(upgrade_to_epub3(&mut pkg).is_err());
    }

    #[cfg(feature = "validate")]
    #[test]
    fn upgrade_clears_epub2_findings_without_adding_errors() {
        let before = epub2().to_bytes().unwrap();
        let mut pkg = epub2();
        upgrade_to_epub3(&mut pkg).unwrap();
        let after = pkg.to_bytes().unwrap();
        let added = crate::validate::source::added_errors(&before, &after);
        assert!(added.is_empty(), "{added:?}");
        let errors = |bytes: &[u8]| {
            crate::validate::source::validate(bytes)
                .findings
                .into_iter()
                .filter(|f| f.severity == crate::validate::Severity::Error)
                .count()
        };
        assert!(errors(&after) < errors(&before));
    }
}
