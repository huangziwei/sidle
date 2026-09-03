use std::collections::HashMap;
use std::io;

use crate::formats::epub::edit::{EpubPackage, attr_value, escape_attr};
use crate::formats::epub::markup::{rewrite_tags, set_attr};
use crate::formats::epub::spine_repair::flatten_declared;
use crate::formats::epub::structure::{basename, dir_of, relativize};
use crate::formats::epub::{OpfData, parse_opf};
use crate::util::{decode_text, extract_xml_encoding, percent_decode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRole {
    Text,
    Style,
    Image,
    Font,
    Audio,
    Video,
    Nav,
    Ncx,
    Opf,
    Container,
    Other,
}

impl MemberRole {
    pub fn as_str(self) -> &'static str {
        match self {
            MemberRole::Text => "text",
            MemberRole::Style => "style",
            MemberRole::Image => "image",
            MemberRole::Font => "font",
            MemberRole::Audio => "audio",
            MemberRole::Video => "video",
            MemberRole::Nav => "nav",
            MemberRole::Ncx => "ncx",
            MemberRole::Opf => "opf",
            MemberRole::Container => "container",
            MemberRole::Other => "other",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Member {
    pub path: String,
    pub id: Option<String>,
    pub media_type: Option<String>,
    pub role: MemberRole,
    pub spine_index: Option<usize>,
    pub label: Option<String>,
    pub size: usize,
    pub text: bool,
}

pub fn members(pkg: &EpubPackage) -> io::Result<Vec<Member>> {
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_raw = pkg.opf_bytes()?;
    let opf_text = decode_text(opf_raw, extract_xml_encoding(opf_raw));
    let opf = parse_opf(&opf_text).map_err(io::Error::other)?;

    let mut declared: HashMap<String, (&str, &str)> = HashMap::new();
    for (id, (href, media_type)) in &opf.manifest {
        let abs = format!("{opf_base}{}", percent_decode(href));
        declared.insert(abs, (id.as_str(), media_type.as_str()));
    }
    let mut spine_index: HashMap<String, usize> = HashMap::new();
    for (i, id) in opf.spine_ids.iter().enumerate() {
        if let Some((href, _)) = opf.manifest.get(id) {
            let abs = format!("{opf_base}{}", percent_decode(href));
            spine_index.entry(abs).or_insert(i);
        }
    }
    let mut labels: HashMap<String, String> = HashMap::new();
    for (label, href) in flatten_declared(pkg, &opf, &opf_base) {
        labels.entry(basename(&href)).or_insert(label);
    }
    let nav_path = opf
        .nav_href
        .as_deref()
        .map(|h| format!("{opf_base}{}", percent_decode(h)));
    let ncx_path = opf
        .ncx_href
        .as_deref()
        .map(|h| format!("{opf_base}{}", percent_decode(h)));

    Ok(pkg
        .names()
        .map(|path| {
            let (id, media_type) = match declared.get(path) {
                Some((id, mt)) => (Some(id.to_string()), Some(mt.to_string())),
                None => (None, None),
            };
            let role = role_of(
                path,
                media_type.as_deref(),
                &opf_path,
                nav_path.as_deref(),
                ncx_path.as_deref(),
            );
            let text = is_text(role, media_type.as_deref(), path);
            Member {
                path: path.to_string(),
                id,
                media_type,
                role,
                spine_index: spine_index.get(path).copied(),
                label: labels.get(&basename(path)).cloned(),
                size: pkg.get(path).map_or(0, <[u8]>::len),
                text,
            }
        })
        .collect())
}

fn role_of(
    path: &str,
    media_type: Option<&str>,
    opf_path: &str,
    nav_path: Option<&str>,
    ncx_path: Option<&str>,
) -> MemberRole {
    if path == opf_path {
        return MemberRole::Opf;
    }
    if path == "mimetype" || path.starts_with("META-INF/") {
        return MemberRole::Container;
    }
    if nav_path == Some(path) {
        return MemberRole::Nav;
    }
    if ncx_path == Some(path) {
        return MemberRole::Ncx;
    }
    let mt = media_type.map(|m| m.trim().to_ascii_lowercase());
    let ext = path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match mt.as_deref() {
        Some("application/xhtml+xml" | "text/html") => MemberRole::Text,
        Some("text/css") => MemberRole::Style,
        Some("application/x-dtbncx+xml") => MemberRole::Ncx,
        Some(m) if m.starts_with("image/") => MemberRole::Image,
        Some(m) if m.starts_with("audio/") => MemberRole::Audio,
        Some(m) if m.starts_with("video/") => MemberRole::Video,
        Some(m)
            if m.starts_with("font/")
                || m.starts_with("application/font")
                || m.starts_with("application/x-font")
                || m == "application/vnd.ms-opentype" =>
        {
            MemberRole::Font
        }
        Some(_) => MemberRole::Other,
        None => match ext.as_str() {
            "xhtml" | "html" | "htm" => MemberRole::Text,
            "css" => MemberRole::Style,
            "ncx" => MemberRole::Ncx,
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "bmp" => MemberRole::Image,
            "ttf" | "otf" | "woff" | "woff2" => MemberRole::Font,
            "mp3" | "m4a" | "ogg" | "aac" => MemberRole::Audio,
            "mp4" | "webm" | "m4v" => MemberRole::Video,
            _ => MemberRole::Other,
        },
    }
}

fn is_text(role: MemberRole, media_type: Option<&str>, path: &str) -> bool {
    match role {
        MemberRole::Text
        | MemberRole::Style
        | MemberRole::Nav
        | MemberRole::Ncx
        | MemberRole::Opf
        | MemberRole::Container => true,
        MemberRole::Font | MemberRole::Audio | MemberRole::Video => false,
        MemberRole::Image => {
            media_type.is_some_and(|m| m.contains("svg")) || path.ends_with(".svg")
        }
        MemberRole::Other => {
            let mt = media_type.unwrap_or("").to_ascii_lowercase();
            mt.starts_with("text/")
                || mt.contains("xml")
                || mt.contains("json")
                || mt.contains("javascript")
                || matches!(
                    path.rsplit('.').next().unwrap_or(""),
                    "xml" | "txt" | "js" | "json" | "smil" | "pls"
                )
        }
    }
}

pub fn add_manifest_item(
    pkg: &mut EpubPackage,
    path: &str,
    media_type: &str,
) -> io::Result<String> {
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_raw = pkg.opf_bytes()?;
    let opf_text = decode_text(opf_raw, extract_xml_encoding(opf_raw)).into_owned();
    let opf = parse_opf(&opf_text).map_err(io::Error::other)?;

    for (id, (href, _)) in &opf.manifest {
        if format!("{opf_base}{}", percent_decode(href)) == path {
            return Ok(id.clone());
        }
    }

    let id = unique_id(&opf, path);
    let href = relativize(&opf_base, path).replace(' ', "%20");
    let rewritten = insert_item(&opf_text, &href, &id, media_type)?;
    pkg.replace(&opf_path, rewritten.into_bytes());
    Ok(id)
}

fn unique_id(opf: &OpfData, path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    let mut base: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if !base.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        base.insert(0, '_');
    }
    let mut id = base.clone();
    let mut n = 1;
    while opf.manifest.contains_key(&id) {
        n += 1;
        id = format!("{base}{n}");
    }
    id
}

fn insert_item(opf: &str, href: &str, id: &str, media_type: &str) -> io::Result<String> {
    let close = opf.find("</manifest>").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "package document has no </manifest>",
        )
    })?;
    let line_start = opf[..close].rfind('\n').map_or(0, |i| i + 1);
    let close_indent = &opf[line_start..close];
    let item_indent = opf[..close]
        .rfind("<item")
        .map(|i| {
            let ls = opf[..i].rfind('\n').map_or(0, |j| j + 1);
            opf[ls..i].to_string()
        })
        .filter(|s| s.chars().all(char::is_whitespace))
        .unwrap_or_else(|| format!("{close_indent}  "));
    let item = format!(
        "{item_indent}<item href=\"{}\" id=\"{}\" media-type=\"{}\"/>\n",
        escape_attr(href),
        escape_attr(id),
        escape_attr(media_type)
    );
    let mut out = String::with_capacity(opf.len() + item.len());
    out.push_str(&opf[..line_start]);
    out.push_str(&item);
    out.push_str(&opf[line_start..]);
    Ok(out)
}

pub fn remove_manifest_item(pkg: &mut EpubPackage, path: &str) -> io::Result<bool> {
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let opf_raw = pkg.opf_bytes()?;
    let opf_text = decode_text(opf_raw, extract_xml_encoding(opf_raw)).into_owned();
    let mut from = 0;
    while let Some(rel) = opf_text[from..].find("<item") {
        let start = from + rel;
        let Some(end_rel) = opf_text[start..].find('>') else {
            break;
        };
        let end = start + end_rel + 1;
        let tag = &opf_text[start..end];
        from = end;
        if !tag.starts_with("<item ") && !tag.starts_with("<item\n") && !tag.starts_with("<item\t")
        {
            continue;
        }
        let Some(href) = attr_value(tag, "href") else {
            continue;
        };
        if format!("{opf_base}{}", percent_decode(&href)) != path {
            continue;
        }
        let mut cut_start = start;
        let line_start = opf_text[..start].rfind('\n').map_or(0, |i| i + 1);
        if opf_text[line_start..start].chars().all(char::is_whitespace) {
            cut_start = line_start;
        }
        let mut cut_end = end;
        if tag.ends_with("/>") {
        } else if let Some(close) = opf_text[end..].find("</item>") {
            cut_end = end + close + "</item>".len();
        }
        if cut_start == line_start && opf_text[cut_end..].starts_with('\n') {
            cut_end += 1;
        }
        let mut out = String::with_capacity(opf_text.len());
        out.push_str(&opf_text[..cut_start]);
        out.push_str(&opf_text[cut_end..]);
        pkg.replace(&opf_path, out.into_bytes());
        return Ok(true);
    }
    Ok(false)
}

pub(crate) fn set_item_properties(opf: &str, id: &str, properties: &str) -> String {
    rewrite_tags(opf, |name, tag| {
        (name == "item" && attr_value(tag, "id").as_deref() == Some(id))
            .then(|| set_attr(tag, "properties", Some(properties)))
    })
}

pub(crate) fn itemref_span(opf: &str, id: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(rel) = opf[from..].find("<itemref") {
        let start = from + rel;
        let end = start + opf[start..].find('>')? + 1;
        if attr_value(&opf[start..end], "idref").as_deref() == Some(id) {
            return Some((start, end));
        }
        from = end;
    }
    None
}

pub(crate) fn insert_itemref_after(opf: &str, after_id: &str, new_id: &str) -> io::Result<String> {
    let (start, end) = itemref_span(opf, after_id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the spine has no itemref for {after_id}"),
        )
    })?;
    let tag = &opf[start..end];
    let linear = attr_value(tag, "linear")
        .map(|l| format!(" linear=\"{}\"", escape_attr(&l)))
        .unwrap_or_default();
    let line_start = opf[..start].rfind('\n').map_or(0, |i| i + 1);
    let indent = &opf[line_start..start];
    let sep = if indent.chars().all(char::is_whitespace) {
        format!("\n{indent}")
    } else {
        String::new()
    };
    Ok(format!(
        "{}{sep}<itemref idref=\"{}\"{linear}/>{}",
        &opf[..end],
        escape_attr(new_id),
        &opf[end..]
    ))
}

pub(crate) fn remove_itemref(opf: &str, id: &str) -> String {
    let Some((start, end)) = itemref_span(opf, id) else {
        return opf.to_string();
    };
    let line_start = opf[..start].rfind('\n').map_or(0, |i| i + 1);
    let alone = opf[line_start..start].chars().all(char::is_whitespace);
    let rest = &opf[end..];
    let after_nl = rest.find('\n').filter(|&i| rest[..i].trim().is_empty());
    match (alone, after_nl) {
        (true, Some(i)) => format!("{}{}", &opf[..line_start], &rest[i + 1..]),
        _ => format!("{}{}", &opf[..start], rest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tiny_epub() -> Vec<u8> {
        use zip::write::SimpleFileOptions;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let deflated = SimpleFileOptions::default();
        let mut put = |name: &str, body: &[u8], opts: SimpleFileOptions| {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body).unwrap();
        };
        put("mimetype", b"application/epub+zip", stored);
        put(
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            deflated,
        );
        put(
            "OEBPS/content.opf",
            br#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="id">x</dc:identifier><dc:title>T</dc:title><dc:language>en</dc:language></metadata>
  <manifest>
    <item href="nav.xhtml" id="nav" media-type="application/xhtml+xml" properties="nav"/>
    <item href="ch1.xhtml" id="ch1" media-type="application/xhtml+xml"/>
    <item href="ch2.xhtml" id="ch2" media-type="application/xhtml+xml"/>
    <item href="style.css" id="css" media-type="text/css"/>
    <item href="cover.jpg" id="cover" media-type="image/jpeg"/>
  </manifest>
  <spine><itemref idref="ch1"/><itemref idref="ch2"/></spine>
</package>"#,
            deflated,
        );
        put(
            "OEBPS/nav.xhtml",
            br#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>nav</title></head><body><nav epub:type="toc"><ol><li><a href="ch1.xhtml">One</a></li></ol></nav></body></html>"#,
            deflated,
        );
        put("OEBPS/ch1.xhtml", b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>1</title></head><body><p>a</p></body></html>", deflated);
        put("OEBPS/ch2.xhtml", b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>2</title></head><body><p>b</p></body></html>", deflated);
        put("OEBPS/style.css", b"p { margin: 0 }", deflated);
        put("OEBPS/cover.jpg", &[0xFF, 0xD8, 0xFF, 0xD9], deflated);
        put("OEBPS/stray.bin", b"?", deflated);
        zip.finish().unwrap().into_inner()
    }

    fn fixture() -> EpubPackage {
        EpubPackage::parse(&tiny_epub()).expect("parse")
    }

    #[test]
    fn every_member_is_listed_with_its_role() {
        let pkg = fixture();
        let list = members(&pkg).expect("members");
        assert_eq!(list.len(), pkg.names().count());
        let by_path = |p: &str| list.iter().find(|m| m.path == p).expect(p);
        assert_eq!(by_path("OEBPS/content.opf").role, MemberRole::Opf);
        assert_eq!(by_path("mimetype").role, MemberRole::Container);
        assert_eq!(
            by_path("META-INF/container.xml").role,
            MemberRole::Container
        );
        assert_eq!(by_path("OEBPS/nav.xhtml").role, MemberRole::Nav);
        assert_eq!(by_path("OEBPS/style.css").role, MemberRole::Style);
        assert_eq!(by_path("OEBPS/cover.jpg").role, MemberRole::Image);
        assert_eq!(by_path("OEBPS/stray.bin").role, MemberRole::Other);
        assert!(by_path("OEBPS/stray.bin").id.is_none());
        assert!(!by_path("OEBPS/cover.jpg").text);
        assert!(by_path("OEBPS/style.css").text);
        assert_eq!(by_path("OEBPS/style.css").size, 15);
        assert_eq!(by_path("OEBPS/style.css").id.as_deref(), Some("css"));
    }

    #[test]
    fn spine_documents_are_numbered_in_reading_order_and_labelled() {
        let pkg = fixture();
        let list = members(&pkg).expect("members");
        let mut spine: Vec<&Member> = list.iter().filter(|m| m.spine_index.is_some()).collect();
        spine.sort_by_key(|m| m.spine_index);
        assert_eq!(
            spine.iter().map(|m| m.path.as_str()).collect::<Vec<_>>(),
            ["OEBPS/ch1.xhtml", "OEBPS/ch2.xhtml"]
        );
        assert_eq!(spine[0].spine_index, Some(0));
        assert_eq!(spine[1].spine_index, Some(1));
        assert_eq!(spine[0].label.as_deref(), Some("One"));
        assert_eq!(spine[1].label, None);
        assert!(spine.iter().all(|m| m.role == MemberRole::Text && m.text));
    }

    #[test]
    fn a_new_file_is_registered_once_and_parses_back() {
        let mut pkg = fixture();
        pkg.set("OEBPS/styles/extra.css", b"p { margin: 0 }".to_vec());
        let id = add_manifest_item(&mut pkg, "OEBPS/styles/extra.css", "text/css").expect("add");
        let again = add_manifest_item(&mut pkg, "OEBPS/styles/extra.css", "text/css").expect("add");
        assert_eq!(id, again);

        let opf_text = decode_text(pkg.opf_bytes().expect("opf"), None);
        assert_eq!(opf_text.matches("styles/extra.css").count(), 1);
        let opf = parse_opf(&opf_text).expect("opf parses");
        assert_eq!(
            opf.manifest.get(&id).map(|(h, m)| (h.as_str(), m.as_str())),
            Some(("styles/extra.css", "text/css"))
        );
        let listed = members(&pkg).expect("members");
        let m = listed
            .iter()
            .find(|m| m.path == "OEBPS/styles/extra.css")
            .expect("listed");
        assert_eq!(m.id.as_deref(), Some(id.as_str()));
        assert_eq!(m.role, MemberRole::Style);
    }

    #[test]
    fn ids_are_ncname_safe_and_unique() {
        let pkg = fixture();
        let opf = parse_opf(&decode_text(pkg.opf_bytes().expect("opf"), None)).expect("opf");
        let id = unique_id(&opf, "OEBPS/表紙 2.jpg");
        assert!(id.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_'));
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
        assert!(!opf.manifest.contains_key(&id));
        assert_eq!(unique_id(&opf, "OEBPS/css.css"), "css2");
    }

    #[test]
    fn insertion_keeps_the_manifest_indentation() {
        let opf = "<package>\n  <manifest>\n    <item href=\"a.xhtml\" id=\"a\" media-type=\"application/xhtml+xml\"/>\n  </manifest>\n</package>\n";
        let out = insert_item(opf, "b.css", "b", "text/css").expect("insert");
        assert!(out.contains(
            "    <item href=\"a.xhtml\" id=\"a\" media-type=\"application/xhtml+xml\"/>\n    <item href=\"b.css\" id=\"b\" media-type=\"text/css\"/>\n  </manifest>"
        ));
    }
}
