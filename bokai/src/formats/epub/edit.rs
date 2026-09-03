//! In-place EPUB (OCF zip) editing. `EpubPackage` holds every member in
//! memory; `tokens`, `attributes`, `offset_of` and `set_attr` read and edit
//! XHTML source text; `members`, `add_manifest_item` and `remove_manifest_item`
//! edit the OPF; `rename_class`, `remove_unused_css`, `beautify`,
//! `split_document`, `merge_with_next` and `upgrade_to_epub3` are the
//! whole-package operations, each returning `Changes`.

use std::collections::{BTreeSet, HashMap};
use std::io::{self, Cursor, Read, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::formats::epub::nav_doc::{render_landmarks_nav, render_nav_doc, render_toc_nav};
use crate::formats::epub::spine_repair::flatten_declared;
use crate::formats::epub::structure::{
    basename, dir_of, rebase_toc, relativize, resolve_href, spine_documents, split_fragment,
};
use crate::formats::epub::{
    OpfData, neutralize_spurious_zip64, parse_container_xml, parse_ncx, parse_opf, parse_opf_guide,
};
use crate::html::{ArenaDom, any_element_matches, parse_dom};
use crate::model::{Landmark, TocEntry};
use crate::style::Stylesheet;
use crate::style::source::{self, BlockItem, Item, Kind};
use crate::util::{decode_text, extract_xml_encoding, percent_decode, time_now_iso8601_utc};

/// The OCF-mandated first member: the media-type marker.
const MIMETYPE_NAME: &str = "mimetype";
/// The canonical `mimetype` body, synthesized when a source EPUB lacks one.
const MIMETYPE_BODY: &[u8] = b"application/epub+zip";
/// The OCF container descriptor that names the OPF package document.
const CONTAINER_PATH: &str = "META-INF/container.xml";

/// One EPUB zip member, decompressed, with the storage method it had on disk;
/// an untouched member re-serializes with that method.
#[derive(Clone)]
struct Entry {
    name: String,
    data: Vec<u8>,
    method: CompressionMethod,
}

/// An EPUB opened for surgical editing: every zip member decompressed in memory,
/// in original order. [`into_bytes`](Self::into_bytes) repackages, mimetype first.
pub struct EpubPackage {
    entries: Vec<Entry>,
}

impl EpubPackage {
    /// Parse an EPUB's zip directory, decompressing every member into memory.
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        match Self::parse_inner(bytes) {
            Ok(pkg) => Ok(pkg),
            Err(first) => match neutralize_spurious_zip64(bytes) {
                Some(repaired) => Self::parse_inner(&repaired),
                None => Err(first),
            },
        }
    }

    fn parse_inner(bytes: &[u8]) -> io::Result<Self> {
        let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(io::Error::other)?;
        let mut entries = Vec::with_capacity(archive.len());
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).map_err(io::Error::other)?;
            if file.name().ends_with('/') {
                continue; // directory entry — implied by member paths
            }
            let name = file.name().to_string();
            let method = file.compression();
            let mut data = Vec::with_capacity(usize::try_from(file.size()).unwrap_or(0));
            file.read_to_end(&mut data)?;
            entries.push(Entry { name, data, method });
        }
        Ok(Self { entries })
    }

    /// The decompressed bytes of the member at `name`, if present.
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.data.as_slice())
    }

    /// True if a member at exactly `path` exists.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }

    /// Every member path, in original zip order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    /// Replace an existing member's bytes in place, keeping its position and
    /// storage method. Returns `false` (changing nothing) if no member has that
    /// name — use [`set`](Self::set) to upsert.
    pub fn replace(&mut self, name: &str, data: Vec<u8>) -> bool {
        match self.entries.iter_mut().find(|e| e.name == name) {
            Some(e) => {
                e.data = data;
                true
            }
            None => false,
        }
    }

    /// Replace an existing member, or append a new deflated one if absent.
    pub fn set(&mut self, name: &str, data: Vec<u8>) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.name == name) {
            e.data = data;
        } else {
            self.entries.push(Entry {
                name: name.to_string(),
                data,
                method: CompressionMethod::Deflated,
            });
        }
    }

    /// A new package holding only the members `keep` accepts, each with its
    /// original bytes, order and storage method.
    pub fn subset(&self, keep: impl Fn(&str) -> bool) -> Self {
        Self {
            entries: self
                .entries
                .iter()
                .filter(|e| keep(&e.name))
                .cloned()
                .collect(),
        }
    }

    /// Remove the member at `name`. Returns `true` if one was removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.name != name);
        self.entries.len() != before
    }

    /// The OPF package document's zip path, read from `META-INF/container.xml`
    /// (percent-decoded to the literal member name, matching the importer).
    pub fn opf_path(&self) -> io::Result<String> {
        let container = self.get(CONTAINER_PATH).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "EPUB has no META-INF/container.xml",
            )
        })?;
        Ok(percent_decode(&parse_container_xml(container)?))
    }

    /// The OPF package document's bytes, located via `container.xml`.
    pub fn opf_bytes(&self) -> io::Result<&[u8]> {
        let path = self.opf_path()?;
        self.get(&path).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("OPF not found at {path}"))
        })
    }

    pub fn into_bytes(self) -> io::Result<Vec<u8>> {
        self.to_bytes()
    }

    pub fn to_bytes(&self) -> io::Result<Vec<u8>> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));

        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let mimetype = self
            .entries
            .iter()
            .find(|e| e.name == MIMETYPE_NAME)
            .map(|e| e.data.as_slice())
            .unwrap_or(MIMETYPE_BODY);
        zip.start_file(MIMETYPE_NAME, stored)
            .map_err(io::Error::other)?;
        zip.write_all(mimetype)?;

        for e in &self.entries {
            if e.name == MIMETYPE_NAME {
                continue; // already emitted first
            }
            let opts = SimpleFileOptions::default().compression_method(writable_method(e.method));
            zip.start_file(&e.name, opts).map_err(io::Error::other)?;
            zip.write_all(&e.data)?;
        }

        let cursor = zip.finish().map_err(io::Error::other)?;
        Ok(cursor.into_inner())
    }
}

#[derive(Debug, Default, Clone)]
pub struct Changes {
    pub changed: Vec<String>,
    pub added: Vec<(String, String)>,
    pub removed: Vec<String>,
    pub notes: Vec<String>,
}

impl Changes {
    pub(crate) fn touch(&mut self, path: &str) {
        if !self.changed.iter().any(|p| p == path) && !self.added.iter().any(|(p, _)| p == path) {
            self.changed.push(path.to_string());
        }
    }

    pub(crate) fn add(&mut self, path: &str, media_type: &str) {
        self.added.push((path.to_string(), media_type.to_string()));
    }

    pub(crate) fn drop(&mut self, path: &str) {
        self.changed.retain(|p| p != path);
        self.removed.push(path.to_string());
    }

    pub(crate) fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }
}

pub fn read_member(bytes: &[u8], name: &str) -> io::Result<Option<Vec<u8>>> {
    match read_member_inner(bytes, name) {
        Ok(found) => Ok(found),
        Err(first) => match neutralize_spurious_zip64(bytes) {
            Some(repaired) => read_member_inner(&repaired, name),
            None => Err(first),
        },
    }
}

fn read_member_inner(bytes: &[u8], name: &str) -> io::Result<Option<Vec<u8>>> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(io::Error::other)?;
    let mut file = match archive.by_name(name) {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(io::Error::other(e)),
    };
    let mut data = Vec::with_capacity(usize::try_from(file.size()).unwrap_or(0));
    file.read_to_end(&mut data)?;
    Ok(Some(data))
}

/// The compression method the writer emits for a parsed member: `Stored` stays
/// `Stored`; every other method becomes `Deflated`.
fn writable_method(m: CompressionMethod) -> CompressionMethod {
    if m == CompressionMethod::Stored {
        CompressionMethod::Stored
    } else {
        CompressionMethod::Deflated
    }
}

/// XML-escape text content (`&`, `<`, `>`) for emission into an OPF / nav / NCX.
/// Shared by the EPUB surgical-write primitives ([`super::toc_repair`],
/// [`super::metadata_edit`]).
pub(crate) fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// XML-escape an attribute value (text plus the quote char).
pub(crate) fn escape_attr(s: &str) -> String {
    escape_text(s).replace('"', "&quot;")
}

/// The value of `name="…"`/`name='…'` in a start tag, at an attribute
/// boundary: `type` does not match inside `epub:type`.
pub(crate) fn attr_value(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let mut from = 0;
    while let Some(rel) = tag[from..].find(&needle) {
        let pos = from + rel;
        let boundary = pos == 0 || tag.as_bytes()[pos - 1].is_ascii_whitespace();
        if boundary {
            let after = &tag[pos + needle.len()..];
            let q = after.chars().next()?;
            if q == '"' || q == '\'' {
                let end = after[1..].find(q)?;
                return Some(after[1..1 + end].to_string());
            }
        }
        from = pos + needle.len();
    }
    None
}

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

pub fn upgrade_to_epub3(pkg: &mut EpubPackage) -> io::Result<Changes> {
    let opf_path = pkg.opf_path()?;
    let opf_base = dir_of(&opf_path);
    let mut opf_text = member_text(pkg, &opf_path)?;
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
            let text = member_text(pkg, &m.path)?;
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
        if let Ok(ncx) = member_text(pkg, &ncx_path) {
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
        let text = member_text(pkg, &m.path)?;
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
        let opf_attrs: Vec<&Attr> = attrs
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
    let Ok(ncx) = member_text(pkg, &path) else {
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
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    const FIXTURE: &str = "tests/fixtures/[太宰 治] 人間失格.epub";

    pub(crate) fn package_from(members: &[(&str, &str)]) -> EpubPackage {
        let mut entries = vec![
            Entry {
                name: MIMETYPE_NAME.to_string(),
                data: MIMETYPE_BODY.to_vec(),
                method: CompressionMethod::Stored,
            },
            Entry {
                name: CONTAINER_PATH.to_string(),
                data: br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#.to_vec(),
                method: CompressionMethod::Deflated,
            },
        ];
        for (name, body) in members {
            entries.push(Entry {
                name: name.to_string(),
                data: body.as_bytes().to_vec(),
                method: CompressionMethod::Deflated,
            });
        }
        EpubPackage { entries }
    }

    fn read_fixture() -> Vec<u8> {
        std::fs::read(FIXTURE).expect("read EPUB fixture")
    }

    /// An untouched parse→repackage round-trip preserves every member's bytes
    /// and reopens as a valid `Book`.
    #[test]
    fn roundtrip_is_faithful_and_reopens() {
        let epub = read_fixture();
        let before = EpubPackage::parse(&epub).expect("parse fixture");
        let before_names: Vec<String> = before.names().map(str::to_string).collect();
        let before_cover = before
            .get("OEBPS/cover.jpeg")
            .expect("cover present")
            .to_vec();

        let out = EpubPackage::parse(&epub)
            .expect("parse fixture")
            .into_bytes()
            .expect("repackage");
        let after = EpubPackage::parse(&out).expect("re-parse repackaged");

        let after_names: Vec<String> = after.names().map(str::to_string).collect();
        assert_eq!(
            before_names, after_names,
            "every member preserved, in order"
        );
        assert_eq!(
            after.get("OEBPS/cover.jpeg"),
            Some(before_cover.as_slice()),
            "image bytes pass through unchanged"
        );

        // The repackaged bytes open and parse as an EPUB.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.epub");
        std::fs::write(&path, &out).expect("write");
        let book = crate::Book::open(&path).expect("repackaged EPUB opens");
        assert_eq!(book.metadata().title, "人間失格");
        assert!(!book.spine().is_empty(), "spine survives repackage");
    }

    /// `mimetype` must be the first member and uncompressed (OCF §3.3).
    #[test]
    fn mimetype_is_first_and_stored() {
        let epub = read_fixture();
        let out = EpubPackage::parse(&epub)
            .expect("parse")
            .into_bytes()
            .expect("repackage");

        let mut archive = ZipArchive::new(Cursor::new(&out)).expect("open output");
        let first = archive.by_index(0).expect("first member");
        assert_eq!(first.name(), MIMETYPE_NAME, "mimetype is first");
        assert_eq!(
            first.compression(),
            CompressionMethod::Stored,
            "mimetype is uncompressed"
        );
        drop(first);
        let mut body = String::new();
        archive
            .by_name(MIMETYPE_NAME)
            .expect("mimetype member")
            .read_to_string(&mut body)
            .expect("read mimetype");
        assert_eq!(body.as_bytes(), MIMETYPE_BODY);
    }

    /// A single `replace` changes exactly one member; the rest are byte-identical.
    #[test]
    fn replace_is_surgical() {
        let epub = read_fixture();
        let original_css = {
            let p = EpubPackage::parse(&epub).expect("parse");
            p.get("OEBPS/style.css").expect("css present").to_vec()
        };
        let new_css = b"/* edited */ body { color: red; }".to_vec();

        let mut pkg = EpubPackage::parse(&epub).expect("parse");
        assert!(pkg.replace("OEBPS/style.css", new_css.clone()), "replaced");
        assert!(
            !pkg.replace("OEBPS/does-not-exist.css", vec![]),
            "replace of a missing member is a no-op returning false"
        );
        let cover = pkg.get("OEBPS/cover.jpeg").expect("cover").to_vec();

        let after = EpubPackage::parse(&pkg.into_bytes().expect("repackage")).expect("re-parse");
        assert_eq!(after.get("OEBPS/style.css"), Some(new_css.as_slice()));
        assert_ne!(
            after.get("OEBPS/style.css").unwrap(),
            original_css.as_slice()
        );
        assert_eq!(
            after.get("OEBPS/cover.jpeg"),
            Some(cover.as_slice()),
            "an unrelated member is untouched"
        );
    }

    /// `set` upserts (replace-or-append) and `remove` deletes.
    #[test]
    fn set_and_remove() {
        let epub = read_fixture();
        let mut pkg = EpubPackage::parse(&epub).expect("parse");

        pkg.set("OEBPS/new.txt", b"hello".to_vec());
        assert_eq!(pkg.get("OEBPS/new.txt"), Some(b"hello".as_slice()));
        pkg.set("OEBPS/new.txt", b"world".to_vec()); // upsert existing
        assert_eq!(pkg.get("OEBPS/new.txt"), Some(b"world".as_slice()));

        assert!(pkg.remove("OEBPS/titlepage.xhtml"));
        assert!(!pkg.remove("OEBPS/titlepage.xhtml"), "already gone");
        assert!(!pkg.contains("OEBPS/titlepage.xhtml"));

        let after = EpubPackage::parse(&pkg.into_bytes().expect("repackage")).expect("re-parse");
        assert_eq!(after.get("OEBPS/new.txt"), Some(b"world".as_slice()));
        assert!(!after.contains("OEBPS/titlepage.xhtml"));
    }

    /// The OPF is located via `container.xml`, and its bytes come back.
    #[test]
    fn opf_path_and_bytes() {
        let epub = read_fixture();
        let pkg = EpubPackage::parse(&epub).expect("parse");
        assert_eq!(pkg.opf_path().expect("opf path"), "OEBPS/content.opf");
        let opf = pkg.opf_bytes().expect("opf bytes");
        assert!(
            opf.windows(9).any(|w| w == b"<dc:title"),
            "opf_bytes returns the package document"
        );
    }

    #[test]
    fn read_member_matches_the_full_parse() {
        let epub = read_fixture();
        let pkg = EpubPackage::parse(&epub).expect("parse");
        let css = read_member(&epub, "OEBPS/style.css")
            .expect("read")
            .expect("present");
        assert_eq!(pkg.get("OEBPS/style.css"), Some(css.as_slice()));
        assert!(
            read_member(&epub, "OEBPS/missing.css")
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn parse_rejects_non_zip() {
        assert!(EpubPackage::parse(b"not a zip at all").is_err());
    }

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

    fn rename_book() -> EpubPackage {
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
        let mut pkg = rename_book();
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
        let mut pkg = rename_book();
        assert!(rename_class(&mut pkg, "old", "1bad").is_err());
        assert!(rename_class(&mut pkg, "old", "old").is_err());
        assert!(rename_class(&mut pkg, "missing", "fine").is_err());
    }

    fn unused_book() -> EpubPackage {
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
        let pkg = unused_book();
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
        let mut pkg = unused_book();
        let changes = remove_unused_css(&mut pkg).unwrap();
        assert_eq!(changes.changed, vec!["OEBPS/s.css"]);
        let css = std::str::from_utf8(pkg.get("OEBPS/s.css").unwrap()).unwrap();
        assert_eq!(
            css,
            "@charset \"utf-8\";\n.used { a: b }\np.used:first-child::before, .gone { e: f }\na:hover { g: h }\n@media screen {\n  p { m: n }\n}\n@font-face { font-family: x }\n"
        );
    }

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

    fn split_book() -> EpubPackage {
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
        let mut pkg = split_book();
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
        let mut pkg = split_book();
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
