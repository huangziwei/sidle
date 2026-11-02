//! Surgical in-place metadata edit for an EPUB's OPF package document.

use std::collections::HashSet;

use crate::formats::epub::edit::{
    EpubPackage, attr_value, attributes, escape_attr, escape_text, set_attr,
};
use crate::formats::epub::parser::{publication_date_rank, resolve_entity};
use crate::model::{Contributor, roles_agree, sort_key_survives};

/// Which OPF metadata fields to set. `None` leaves a field untouched; an empty
/// value removes the element.
#[derive(Debug, Clone, Default)]
pub struct MetadataPatch {
    pub title: Option<String>,
    /// The creators in order; one stating no role keeps its element's roles.
    pub creators: Option<Vec<Contributor>>,
    pub language: Option<String>,
    pub publisher: Option<String>,
    /// Publication date (`<dc:date>`).
    pub date: Option<String>,
    pub asin: Option<String>,
}

impl MetadataPatch {
    /// True if the patch sets no field — [`edit_metadata`] then returns the input
    /// unchanged.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.creators.is_none()
            && self.language.is_none()
            && self.publisher.is_none()
            && self.date.is_none()
            && self.asin.is_none()
    }
}

/// Apply `patch` to the OPF of `epub_bytes`, passing every other member through.
/// Returns `epub_bytes` itself when the OPF states every patched value, and errors
/// on an unreadable EPUB or a non-UTF-8 OPF.
pub fn edit_metadata(epub_bytes: &[u8], patch: &MetadataPatch) -> std::io::Result<Vec<u8>> {
    if patch.is_empty() {
        return Ok(epub_bytes.to_vec());
    }
    let mut pkg = EpubPackage::parse(epub_bytes)?;
    let opf_path = pkg.opf_path()?;
    let opf = std::str::from_utf8(pkg.opf_bytes()?).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("OPF not UTF-8: {e}"),
        )
    })?;
    let new_opf = rewrite_metadata(opf, patch);
    if new_opf == opf {
        return Ok(epub_bytes.to_vec());
    }
    pkg.replace(&opf_path, new_opf.into_bytes());
    pkg.into_bytes()
}

/// Which of several same-named elements carries a field, as the parser reads it.
#[derive(Clone, Copy)]
enum Pick {
    /// The first with text (`dc:title`).
    First,
    /// The last (`dc:language`, `dc:publisher`).
    Last,
    /// The best-ranked by [`publication_date_rank`] (`dc:date`).
    Publication,
}

/// Apply every set field of `patch` to the OPF text.
fn rewrite_metadata(opf: &str, patch: &MetadataPatch) -> String {
    let mut s = opf.to_string();
    if let Some(t) = &patch.title {
        s = set_single(&s, "title", t, Pick::First);
    }
    if let Some(l) = &patch.language {
        s = set_single(&s, "language", l, Pick::Last);
    }
    if let Some(p) = &patch.publisher {
        s = set_single(&s, "publisher", p, Pick::Last);
    }
    if let Some(d) = &patch.date {
        s = set_single(&s, "date", d, Pick::Publication);
    }
    if let Some(a) = &patch.asin {
        s = set_asin(&s, a);
    }
    if let Some(creators) = &patch.creators {
        s = set_creators(&s, creators);
    }
    s
}

/// One `<dc:…>text</dc:…>` element: where it starts, where its text runs, and
/// where it ends.
struct Element {
    start: usize,
    text: (usize, usize),
    end: usize,
}

impl Element {
    fn start_tag<'a>(&self, opf: &'a str) -> &'a str {
        &opf[self.start..self.text.0]
    }

    /// The text as the parser reads it: references resolved, ends trimmed.
    fn text(&self, opf: &str) -> String {
        decode(&opf[self.text.0..self.text.1])
    }
}

/// Every `<dc:{tag}>` element that has a closing tag, in document order.
fn dc_elements(opf: &str, tag: &str) -> Vec<Element> {
    let open = format!("<dc:{tag}");
    let close = format!("</dc:{tag}>");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = opf[from..].find(&open) {
        let start = from + rel;
        from = start + open.len();
        let after = opf.as_bytes().get(from).copied().unwrap_or(b'>');
        if !(after == b'>' || after.is_ascii_whitespace()) {
            continue;
        }
        let Some(gt) = opf[start..].find('>').map(|p| start + p) else {
            break;
        };
        if opf.as_bytes()[gt - 1] == b'/' {
            from = gt + 1;
            continue;
        }
        let Some(text_end) = opf[gt + 1..].find(&close).map(|p| gt + 1 + p) else {
            break;
        };
        from = text_end + close.len();
        out.push(Element {
            start,
            text: (gt + 1, text_end),
            end: from,
        });
    }
    out
}

/// The element of `elements` that carries the field.
fn pick<'a>(opf: &str, elements: &'a [Element], which: Pick) -> Option<&'a Element> {
    match which {
        Pick::First => elements
            .iter()
            .find(|e| !e.text(opf).is_empty())
            .or(elements.first()),
        Pick::Last => elements.last(),
        Pick::Publication => {
            let mut best: Option<(u8, &Element)> = None;
            for e in elements {
                let event = local_attr(e.start_tag(opf), "event");
                if let Some(rank) = publication_date_rank(event.as_deref())
                    && !e.text(opf).is_empty()
                    && best.is_none_or(|(kept, _)| rank < kept)
                {
                    best = Some((rank, e));
                }
            }
            best.map(|(_, e)| e)
        }
    }
}

/// Set the field `<dc:{tag}>` carries, keeping its attributes: an empty `value`
/// removes the element, and a `tag` with no element gains one.
fn set_single(opf: &str, tag: &str, value: &str, which: Pick) -> String {
    let elements = dc_elements(opf, tag);
    match pick(opf, &elements, which) {
        Some(e) if value.is_empty() => remove_element(opf, e),
        Some(e) if e.text(opf) == value => opf.to_string(),
        Some(e) => format!(
            "{}{}{}",
            &opf[..e.text.0],
            escape_text(value),
            &opf[e.text.1..]
        ),
        None if value.is_empty() => opf.to_string(),
        None => inject_into_metadata(opf, &format!("<dc:{tag}>{}</dc:{tag}>", escape_text(value))),
    }
}

/// What a creator's edit takes from the refinements of its element.
enum Stale {
    /// Every refinement: the creator goes.
    Every,
    /// The ones spelling its name (`file-as`, `alternate-script`).
    NameDerived,
    /// Its `role` statements, which new ones replace.
    Roles,
}

impl Stale {
    fn covers(&self, property: &str) -> bool {
        match self {
            Stale::Every => true,
            Stale::NameDerived => name_derived(property),
            Stale::Roles => property.rsplit(':').next() == Some("role"),
        }
    }
}

/// How one kept creator changes.
struct CreatorEdit {
    renamed: bool,
    keeps_sort_key: bool,
    roles_change: bool,
    /// A different person stating no role, who takes none of the element's.
    drops_roles: bool,
}

/// Rewrite the `<dc:creator>` run to `creators` place by place: a renamed creator keeps its
/// element, and its `file-as` and roles while [`sort_key_survives`]; stated roles replace the
/// element's (EPUB 3 refinements, EPUB 2 `opf:role`); surplus ones go, extra ones follow the last.
fn set_creators(opf: &str, creators: &[Contributor]) -> String {
    let epub3 = package_version(opf).is_none_or(|v| v.starts_with('3'));

    // The refinements that go are stripped first, against the input's ids.
    let mut plan: Vec<Option<CreatorEdit>> = Vec::new();
    let mut stale: Vec<(String, Stale)> = Vec::new();
    for (i, c) in dc_elements(opf, "creator").iter().enumerate() {
        let start_tag = c.start_tag(opf);
        let id = attr_value(start_tag, "id");
        let Some(new) = creators.get(i) else {
            stale.extend(id.map(|id| (id, Stale::Every)));
            plan.push(None);
            continue;
        };
        let old = c.text(opf);
        let renamed = old != new.name;
        let keeps_sort_key = !renamed || sort_key_survives(&old, &new.name);
        let roles_change = !new.roles.is_empty()
            && !roles_agree(&stated_roles(opf, start_tag, id.as_deref()), &new.roles);
        let drops_roles = !keeps_sort_key && new.roles.is_empty();
        if let Some(id) = id {
            if !keeps_sort_key {
                stale.push((id.clone(), Stale::NameDerived));
            }
            if roles_change || drops_roles {
                stale.push((id, Stale::Roles));
            }
        }
        plan.push(Some(CreatorEdit {
            renamed,
            keeps_sort_key,
            roles_change,
            drops_roles,
        }));
    }
    let opf = strip_refines(opf, |id, property| {
        stale
            .iter()
            .any(|(s, what)| s == id && what.covers(property))
    });

    let elements = dc_elements(&opf, "creator");
    let mut taken = ids_in(&opf);
    let mut writes_opf_attr = false;
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let extra = creators.get(elements.len()..).unwrap_or_default();
    for (i, (c, edit)) in elements.iter().zip(&plan).enumerate() {
        let Some(edit) = edit else {
            let (s, e) = line_span(&opf, c.start, c.end);
            edits.push((s, e, String::new()));
            continue;
        };
        let new = &creators[i];
        let indent = indent_of(&opf, c.start);
        let old_tag = c.start_tag(&opf);
        let mut tag = if edit.keeps_sort_key {
            old_tag.to_string()
        } else {
            without_local(old_tag, "file-as")
        };
        if edit.drops_roles {
            tag = without_local(&tag, "role");
        }
        let mut after = String::new();
        if edit.roles_change {
            tag = without_local(&tag, "role");
            if epub3 {
                let id = match attr_value(&tag, "id") {
                    Some(id) => id,
                    None => {
                        let id = free_id(&mut taken);
                        tag = set_attr(&tag, "id", Some(&id));
                        id
                    }
                };
                after.push_str(&role_refinements(&id, &new.roles, indent));
            } else {
                tag = set_attr(&tag, "opf:role", Some(&new.roles[0]));
                writes_opf_attr = true;
            }
        }
        if i + 1 == elements.len() {
            after.push_str(&extra_creators(
                extra,
                indent,
                epub3,
                &mut taken,
                &mut writes_opf_attr,
            ));
        }
        if edit.renamed || tag != old_tag {
            let text = if edit.renamed {
                escape_text(&new.name)
            } else {
                opf[c.text.0..c.text.1].to_string()
            };
            edits.push((c.start, c.text.1, format!("{tag}{text}")));
        }
        if !after.is_empty() {
            edits.push((c.end, c.end, after));
        }
    }

    edits.sort_by_key(|&(start, _, _)| std::cmp::Reverse(start));
    let mut out = opf;
    for (start, end, text) in edits {
        out.replace_range(start..end, &text);
    }
    if elements.is_empty() {
        for line in extra_creators(extra, "", epub3, &mut taken, &mut writes_opf_attr).lines() {
            if !line.is_empty() {
                out = inject_into_metadata(&out, line);
            }
        }
    }
    if writes_opf_attr {
        out = declare_opf_prefix(&out);
    }
    out
}

/// The `<dc:creator>` elements for `extra`, each on its own line after `indent`
/// under a fresh id, with its roles (refinements in EPUB 3, else `opf:role`).
fn extra_creators(
    extra: &[Contributor],
    indent: &str,
    epub3: bool,
    taken: &mut HashSet<String>,
    writes_opf_attr: &mut bool,
) -> String {
    let mut out = String::new();
    for c in extra {
        let id = free_id(taken);
        let role_attr = match c.roles.first() {
            Some(role) if !epub3 => {
                *writes_opf_attr = true;
                format!(" opf:role=\"{}\"", escape_attr(role))
            }
            _ => String::new(),
        };
        out.push_str(&format!(
            "\n{indent}<dc:creator id=\"{id}\"{role_attr}>{}</dc:creator>",
            escape_text(&c.name)
        ));
        if epub3 {
            out.push_str(&role_refinements(&id, &c.roles, indent));
        }
    }
    out
}

/// One `role` refinement of `#id` per role, each on its own line after `indent`.
fn role_refinements(id: &str, roles: &[String], indent: &str) -> String {
    roles
        .iter()
        .map(|role| {
            format!(
                "\n{indent}<meta refines=\"#{id}\" property=\"role\" scheme=\"marc:relators\">{}</meta>",
                escape_text(role)
            )
        })
        .collect()
}

/// The roles a creator's element states: its `role` attribute (EPUB 2), then the
/// `role` refinements of its id, as the parser reads them.
fn stated_roles(opf: &str, start_tag: &str, id: Option<&str>) -> Vec<String> {
    let mut roles: Vec<String> = local_attr(start_tag, "role").into_iter().collect();
    if let Some(id) = id {
        for m in meta_elements(opf) {
            let tag = &opf[m.start..m.tag_end];
            let refines = attr_value(tag, "refines");
            let property = attr_value(tag, "property").unwrap_or_default();
            if refines.as_deref().and_then(|r| r.strip_prefix('#')) == Some(id)
                && property.rsplit(':').next() == Some("role")
            {
                roles.push(match m.text {
                    Some((s, e)) => decode(&opf[s..e]),
                    None => attr_value(tag, "content").unwrap_or_default(),
                });
            }
        }
    }
    roles
}

/// The `version` of the `<package>` element, if the text has one.
fn package_version(opf: &str) -> Option<String> {
    let start = opf.find("<package")?;
    let end = start + opf[start..].find('>')?;
    attr_value(&opf[start..=end], "version")
}

/// `opf` with the `opf:` prefix declared on `<metadata>` when nothing declares it.
fn declare_opf_prefix(opf: &str) -> String {
    if opf.contains("xmlns:opf=") {
        return opf.to_string();
    }
    let Some(start) = opf.find("<metadata") else {
        return opf.to_string();
    };
    let Some(end) = opf[start..].find('>').map(|p| start + p) else {
        return opf.to_string();
    };
    let tag = set_attr(
        &opf[start..=end],
        "xmlns:opf",
        Some("http://www.idpf.org/2007/opf"),
    );
    format!("{}{tag}{}", &opf[..start], &opf[end + 1..])
}

/// Whether `property` spells the name it refines (`file-as`, `alternate-script`).
fn name_derived(property: &str) -> bool {
    matches!(
        property.rsplit(':').next(),
        Some("file-as" | "alternate-script")
    )
}

/// `tag` without the attributes whose local name is `local`, whatever their prefix.
fn without_local(tag: &str, local: &str) -> String {
    attributes(tag)
        .iter()
        .filter(|a| a.name.rsplit(':').next() == Some(local))
        .fold(tag.to_string(), |t, a| set_attr(&t, &a.name, None))
}

/// The value of the attribute whose local name is `local`, any prefix.
fn local_attr(tag: &str, local: &str) -> Option<String> {
    attributes(tag)
        .into_iter()
        .find(|a| a.name.rsplit(':').next() == Some(local))
        .map(|a| a.value)
}

/// Every `id` attribute value in `opf`.
fn ids_in(opf: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut from = 0;
    while let Some(rel) = opf[from..].find("id=") {
        let pos = from + rel;
        from = pos + 3;
        if pos == 0 || !opf.as_bytes()[pos - 1].is_ascii_whitespace() {
            continue;
        }
        let rest = &opf[from..];
        if let Some(q) = rest.chars().next().filter(|q| *q == '"' || *q == '\'')
            && let Some(end) = rest[1..].find(q)
        {
            out.insert(rest[1..1 + end].to_string());
        }
    }
    out
}

/// The first `creatorN` not in `taken`, claimed.
fn free_id(taken: &mut HashSet<String>) -> String {
    let id = (1..)
        .map(|n| format!("creator{n}"))
        .find(|id| !taken.contains(id))
        .expect("an unbounded range finds a free id");
    taken.insert(id.clone());
    id
}

/// The indentation of the line `pos` starts on, when only whitespace precedes it.
fn indent_of(opf: &str, pos: usize) -> &str {
    let line_start = opf[..pos].rfind('\n').map_or(0, |p| p + 1);
    let lead = &opf[line_start..pos];
    if lead.trim().is_empty() { lead } else { "    " }
}

/// `[start, end)` widened to its whole line, newline included, when the rest of
/// that line is blank.
fn line_span(opf: &str, start: usize, end: usize) -> (usize, usize) {
    let line_start = opf[..start].rfind('\n').map_or(0, |p| p + 1);
    let line_end = opf[end..].find('\n').map_or(opf.len(), |p| end + p + 1);
    if opf[line_start..start].trim().is_empty() && opf[end..line_end].trim().is_empty() {
        (line_start, line_end)
    } else {
        (start, end)
    }
}

/// `opf` without `e`, and without the refinements that pointed at it.
fn remove_element(opf: &str, e: &Element) -> String {
    let id = attr_value(e.start_tag(opf), "id");
    let (start, end) = line_span(opf, e.start, e.end);
    let out = format!("{}{}", &opf[..start], &opf[end..]);
    match id {
        Some(id) => strip_refines(&out, |target, _| target == id),
        None => out,
    }
}

/// One `<meta>` element: where it starts, where its start tag ends, its text
/// (none when self-closing) and where it ends.
struct MetaElement {
    start: usize,
    tag_end: usize,
    text: Option<(usize, usize)>,
    end: usize,
}

/// Every `<meta>` element of `opf`, in document order.
fn meta_elements(opf: &str) -> Vec<MetaElement> {
    let mut out = Vec::new();
    let mut scan = 0;
    while let Some(rel) = opf[scan..].find("<meta") {
        let start = scan + rel;
        // Tag-boundary guard: skip `<metadata`, match only the `<meta` element.
        let after = opf.as_bytes().get(start + 5).copied().unwrap_or(b'>');
        if !(after == b'>' || after == b'/' || after.is_ascii_whitespace()) {
            scan = start + 5;
            continue;
        }
        let Some(gt) = opf[start..].find('>').map(|p| start + p) else {
            break;
        };
        let (text, end) = if opf.as_bytes()[gt - 1] == b'/' {
            (None, gt + 1)
        } else {
            match opf[gt + 1..].find("</meta>") {
                Some(c) => (Some((gt + 1, gt + 1 + c)), gt + 1 + c + "</meta>".len()),
                None => (None, gt + 1),
            }
        };
        out.push(MetaElement {
            start,
            tag_end: gt + 1,
            text,
            end,
        });
        scan = end;
    }
    out
}

/// Remove every `<meta refines="#id" property="…">` (and self-closing form) for
/// which `drop(id, property)` holds, each with its line when it stood alone.
fn strip_refines(opf: &str, drop: impl Fn(&str, &str) -> bool) -> String {
    let mut out = String::with_capacity(opf.len());
    let mut copied = 0;
    for m in meta_elements(opf) {
        if m.start < copied {
            continue;
        }
        let tag = &opf[m.start..m.tag_end];
        let property = attr_value(tag, "property").unwrap_or_default();
        let hit = attr_value(tag, "refines")
            .as_deref()
            .and_then(|r| r.strip_prefix('#'))
            .is_some_and(|id| drop(id, &property));
        if hit {
            let (start, end) = line_span(opf, m.start, m.end);
            out.push_str(&opf[copied..start]);
            copied = end;
        }
    }
    out.push_str(&opf[copied..]);
    out
}

/// XML text with its entity and character references resolved and its ends
/// trimmed, as the parser reads an element's text.
fn decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp + 1..];
        match tail
            .find(';')
            .and_then(|semi| resolve_entity(&tail[..semi]).map(|r| (semi, r)))
        {
            Some((semi, resolved)) => {
                out.push_str(&resolved);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    crate::util::trim_markup_space(&out).to_string()
}

/// Set the ASIN identifier's text: replace the `scheme="ASIN"` (or `id="asin"`)
/// `<dc:identifier>`, or append one.
fn set_asin(opf: &str, asin: &str) -> String {
    let mut from = 0;
    while let Some(rel) = opf[from..].find("<dc:identifier") {
        let start = from + rel;
        let Some(gtrel) = opf[start..].find('>') else {
            break;
        };
        let gt = start + gtrel;
        let tag = opf[start..gt].to_ascii_lowercase();
        let is_asin = tag.contains("scheme=\"asin\"")
            || tag.contains("scheme='asin'")
            || tag.contains("scheme=\"mobi-asin\"")
            || tag.contains("scheme='mobi-asin'")
            || tag.contains("id=\"asin\"")
            || tag.contains("id='asin'");
        if is_asin && opf.as_bytes()[gt.saturating_sub(1)] != b'/' {
            let content_start = gt + 1;
            if let Some(cerel) = opf[content_start..].find("</dc:identifier>") {
                let ce = content_start + cerel;
                return format!(
                    "{}{}{}",
                    &opf[..content_start],
                    escape_text(asin),
                    &opf[ce..]
                );
            }
        }
        from = gt + 1;
    }
    inject_into_metadata(
        opf,
        &format!(
            "<dc:identifier opf:scheme=\"ASIN\">{}</dc:identifier>",
            escape_text(asin)
        ),
    )
}

/// Insert `element` just before `</metadata>`: on a line of its own above a
/// closing tag that stands alone, one level deeper, else right before the tag. An
/// `opf` without `</metadata>` comes back unchanged.
fn inject_into_metadata(opf: &str, element: &str) -> String {
    let Some(pos) = opf.rfind("</metadata>") else {
        return opf.to_string();
    };
    let line_start = opf[..pos].rfind('\n').map_or(0, |p| p + 1);
    let lead = &opf[line_start..pos];
    if lead.trim().is_empty() {
        format!(
            "{}{lead}  {element}\n{}",
            &opf[..line_start],
            &opf[line_start..]
        )
    } else {
        format!("{}  {element}\n{}", &opf[..pos], &opf[pos..])
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::Book;
    use crate::model::Format;

    const EPUB2: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>Old Title</dc:title>
    <dc:creator opf:role="aut" opf:file-as="Doe, Jane">Doe, Jane</dc:creator>
    <dc:identifier id="uid" opf:scheme="ISBN">9780000000000</dc:identifier>
    <dc:date>2020-02-01</dc:date>
    <dc:date opf:event="modification">2020-01-07</dc:date>
    <dc:publisher>Old Press</dc:publisher>
    <dc:language>en</dc:language>
    <meta name="cover" content="cov"/>
  </metadata>
  <manifest>
    <item href="ch1.xhtml" id="ch1" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
  </spine>
</package>
"#;

    const EPUB3_CREATORS: &str = "<metadata>\n    \
        <dc:creator id=\"creator1\">Doe, Jane</dc:creator>\n    \
        <meta refines=\"#creator1\" property=\"role\" scheme=\"marc:relators\">aut</meta>\n    \
        <meta refines=\"#creator1\" property=\"file-as\">Doe, Jane</meta>\n    \
        <dc:creator id=\"creator2\">Roe, John</dc:creator>\n    \
        <meta refines=\"#creator2\" property=\"file-as\">Roe, John</meta>\n    \
        <dc:language>en</dc:language>\n</metadata>";

    fn epub_with(opf: &str) -> Vec<u8> {
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
        put("OEBPS/content.opf", opf.as_bytes(), deflated);
        put(
            "OEBPS/ch1.xhtml",
            br#"<?xml version="1.0" encoding="utf-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>c</title></head><body><p>Text.</p></body></html>"#,
            deflated,
        );
        zip.finish().unwrap().into_inner()
    }

    fn reopen(bytes: &[u8]) -> Book {
        Book::from_bytes(bytes, Format::Epub).expect("edited EPUB opens")
    }

    fn opf_of(bytes: &[u8]) -> String {
        let pkg = EpubPackage::parse(bytes).unwrap();
        String::from_utf8(pkg.opf_bytes().unwrap().to_vec()).unwrap()
    }

    #[test]
    fn a_flipped_author_changes_only_the_creator_text() {
        let epub = epub_with(EPUB2);
        let patch = MetadataPatch {
            creators: Some(named(&["Jane Doe"])),
            ..Default::default()
        };
        let out = edit_metadata(&epub, &patch).unwrap();
        assert_eq!(
            opf_of(&out),
            EPUB2.replace(">Doe, Jane</dc:creator>", ">Jane Doe</dc:creator>"),
            "the role and the sort key stay, every other line is untouched"
        );
        let after = reopen(&out);
        assert_eq!(after.metadata().creator_names(), ["Jane Doe"]);
        assert_eq!(after.metadata().creator_sort_keys(), ["Doe, Jane"]);
    }

    #[test]
    fn a_different_person_takes_neither_the_old_sort_key_nor_the_old_role() {
        let out = set_creators(EPUB2, &named(&["Ann Writer"]));
        assert!(out.contains("<dc:creator>Ann Writer</dc:creator>"), "{out}");
        assert!(!out.contains("Doe, Jane"));

        let stated = set_creators(EPUB2, &[Contributor::named("Ann Writer").with_role("ill")]);
        assert!(
            stated.contains("<dc:creator opf:role=\"ill\">Ann Writer</dc:creator>"),
            "{stated}"
        );

        let illustrated = EPUB3_CREATORS.replace(
            "<meta refines=\"#creator2\" property=\"file-as\">Roe, John</meta>",
            "<meta refines=\"#creator2\" property=\"role\" scheme=\"marc:relators\">ill</meta>",
        );
        let out = set_creators(&illustrated, &named(&["Roe, John"]));
        assert_eq!(
            roles_of(&out),
            [("Roe, John".to_string(), Vec::<String>::new())]
        );
        assert!(
            !out.contains(">ill<"),
            "the illustrator's role leaves with the illustrator: {out}"
        );
    }

    #[test]
    fn epub3_refinements_follow_the_same_rule() {
        let out = set_creators(EPUB3_CREATORS, &named(&["Jane Doe", "Ann Writer"]));
        assert!(out.contains("<dc:creator id=\"creator1\">Jane Doe</dc:creator>"));
        assert!(
            out.contains("<meta refines=\"#creator1\" property=\"file-as\">Doe, Jane</meta>"),
            "a reordered name keeps its sort key"
        );
        assert!(out.contains("<dc:creator id=\"creator2\">Ann Writer</dc:creator>"));
        assert!(
            !out.contains("Roe, John"),
            "a replaced author's sort key goes"
        );
        assert!(!out.contains("\n\n"), "no blank line left behind: {out:?}");
    }

    #[test]
    fn surplus_creators_go_with_every_refinement() {
        let out = set_creators(EPUB3_CREATORS, &named(&["Doe, Jane"]));
        assert_eq!(out.matches("<dc:creator").count(), 1);
        assert!(!out.contains("#creator2"), "{out}");
        assert!(
            out.contains("<meta refines=\"#creator1\" property=\"role\""),
            "the kept creator keeps its role"
        );
        assert!(!out.contains("\n\n"), "no blank line left behind: {out:?}");
    }

    #[test]
    fn extra_authors_follow_the_last_creator_under_fresh_ids() {
        let out = set_creators(
            EPUB3_CREATORS,
            &named(&["Doe, Jane", "Roe, John", "Ann Writer"]),
        );
        assert!(
            out.contains(
                "<dc:creator id=\"creator2\">Roe, John</dc:creator>\n    <dc:creator id=\"creator3\">Ann Writer</dc:creator>"
            ),
            "{out}"
        );
        assert_eq!(
            reopen_opf(&out).metadata.creator_names(),
            ["Doe, Jane", "Roe, John", "Ann Writer"]
        );
    }

    fn named(names: &[&str]) -> Vec<Contributor> {
        names.iter().map(|n| Contributor::named(*n)).collect()
    }

    fn roles_of(opf: &str) -> Vec<(String, Vec<String>)> {
        reopen_opf(opf)
            .metadata
            .creators
            .into_iter()
            .map(|c| (c.name, c.roles))
            .collect()
    }

    #[test]
    fn a_changed_role_replaces_the_element_s_role_refinements() {
        let creators = vec![
            Contributor::named("Doe, Jane").with_role("aut"),
            Contributor::named("Roe, John").with_role("ill"),
        ];
        let out = set_creators(EPUB3_CREATORS, &creators);
        assert_eq!(
            out,
            EPUB3_CREATORS.replace(
                "<dc:creator id=\"creator2\">Roe, John</dc:creator>\n",
                "<dc:creator id=\"creator2\">Roe, John</dc:creator>\n    \
                 <meta refines=\"#creator2\" property=\"role\" scheme=\"marc:relators\">ill</meta>\n"
            ),
            "the agreeing creator is left byte for byte"
        );
        assert_eq!(
            roles_of(&out),
            [
                ("Doe, Jane".to_string(), vec!["aut".to_string()]),
                ("Roe, John".to_string(), vec!["ill".to_string()]),
            ]
        );

        let back = set_creators(&out, &named(&["Doe, Jane", "Roe, John"]));
        assert_eq!(back, out, "a creator stating no role keeps the element's");
        let author_again = set_creators(
            &out,
            &[
                Contributor::named("Doe, Jane"),
                Contributor::named("Roe, John").with_role("aut"),
            ],
        );
        assert!(
            author_again.contains(
                "<dc:creator id=\"creator2\">Roe, John</dc:creator>\n    \
                 <meta refines=\"#creator2\" property=\"role\" scheme=\"marc:relators\">aut</meta>"
            ),
            "{author_again}"
        );
        assert!(!author_again.contains(">ill<"));
    }

    #[test]
    fn a_creator_stating_no_role_agrees_with_one_stated_as_author() {
        let opf = "<metadata>\n  <dc:creator>Ann Writer</dc:creator>\n</metadata>";
        let author = [Contributor::named("Ann Writer").with_role("AUT")];
        assert_eq!(set_creators(opf, &author), opf);
    }

    #[test]
    fn an_epub3_creator_without_an_id_gains_one_for_its_role() {
        let opf = "<package version=\"3.0\">\n<metadata>\n  <dc:creator>絵師 花子</dc:creator>\n</metadata>\n</package>";
        let out = set_creators(opf, &[Contributor::named("絵師 花子").with_role("ill")]);
        assert!(
            out.contains(
                "<dc:creator id=\"creator1\">絵師 花子</dc:creator>\n  \
                 <meta refines=\"#creator1\" property=\"role\" scheme=\"marc:relators\">ill</meta>"
            ),
            "{out}"
        );
    }

    #[test]
    fn an_epub2_role_is_the_opf_role_attribute() {
        let out = set_creators(EPUB2, &[Contributor::named("Doe, Jane").with_role("edt")]);
        assert!(
            out.contains(
                "<dc:creator opf:file-as=\"Doe, Jane\" opf:role=\"edt\">Doe, Jane</dc:creator>"
            ),
            "{out}"
        );
        assert!(!out.contains("refines"), "EPUB 2 has no refinements: {out}");

        let bare = EPUB2.replace(" xmlns:opf=\"http://www.idpf.org/2007/opf\"", "");
        let extra = set_creators(
            &bare,
            &[
                Contributor::named("Doe, Jane"),
                Contributor::named("Ann Writer").with_role("ill"),
            ],
        );
        assert!(
            extra.contains("<dc:creator id=\"creator1\" opf:role=\"ill\">Ann Writer</dc:creator>"),
            "{extra}"
        );
        assert!(
            extra.contains("xmlns:opf=\"http://www.idpf.org/2007/opf\""),
            "the prefix the attribute uses is declared: {extra}"
        );
    }

    #[test]
    fn injected_elements_line_up_above_the_closing_tag() {
        let opf = "<package version=\"3.0\">\n  <metadata>\n    <dc:title>T</dc:title>\n  </metadata>\n</package>";
        let out = set_creators(
            opf,
            &[
                Contributor::named("編者 一").with_role("edt"),
                Contributor::named("編者 二"),
            ],
        );
        assert_eq!(
            out,
            "<package version=\"3.0\">\n  <metadata>\n    <dc:title>T</dc:title>\n    \
             <dc:creator id=\"creator1\">編者 一</dc:creator>\n    \
             <meta refines=\"#creator1\" property=\"role\" scheme=\"marc:relators\">edt</meta>\n    \
             <dc:creator id=\"creator2\">編者 二</dc:creator>\n  </metadata>\n</package>"
        );
    }

    #[test]
    fn extra_creators_carry_their_roles() {
        let out = set_creators(
            "<metadata>\n  <dc:title>読本</dc:title>\n</metadata>",
            &[
                Contributor::named("編者 一").with_role("edt"),
                Contributor::named("編者 二").with_role("edt"),
            ],
        );
        assert_eq!(
            roles_of(&out),
            [
                ("編者 一".to_string(), vec!["edt".to_string()]),
                ("編者 二".to_string(), vec!["edt".to_string()]),
            ]
        );
    }

    #[test]
    fn a_book_without_creators_gains_them() {
        let opf = "<metadata>\n  <dc:title>T</dc:title>\n</metadata>";
        let out = set_creators(opf, &named(&["Ann Writer"]));
        assert!(out.contains("<dc:creator id=\"creator1\">Ann Writer</dc:creator>"));
    }

    #[test]
    fn clearing_the_authors_removes_every_creator() {
        let out = set_creators(EPUB3_CREATORS, &[]);
        assert!(!out.contains("<dc:creator"));
        assert!(!out.contains("refines"), "{out}");
        assert!(out.contains("<dc:language>en</dc:language>"));
    }

    #[test]
    fn the_publication_date_is_written_where_the_parser_reads_it() {
        let out = set_single(EPUB2, "date", "2019-05-05", Pick::Publication);
        assert!(out.contains("<dc:date>2019-05-05</dc:date>"));
        assert!(
            out.contains("<dc:date opf:event=\"modification\">2020-01-07</dc:date>"),
            "the modification stamp is not the publication date"
        );
        assert_eq!(
            reopen_opf(&out).metadata.date.as_deref(),
            Some("2019-05-05")
        );
    }

    #[test]
    fn an_empty_value_removes_the_element() {
        let out = set_single(EPUB2, "publisher", "", Pick::Last);
        assert!(!out.contains("dc:publisher"));
        assert!(!out.contains("\n\n"), "the line goes with it: {out:?}");
        let out = set_single(EPUB2, "date", "", Pick::Publication);
        assert!(!out.contains("2020-02-01"));
        assert!(out.contains("2020-01-07"));
        assert_eq!(reopen_opf(&out).metadata.date, None);
    }

    #[test]
    fn the_last_language_is_the_one_replaced() {
        let opf = "<metadata>\n<dc:language>ja</dc:language>\n<dc:language>zh-TW</dc:language>\n</metadata>";
        let out = set_single(opf, "language", "zh-Hant", Pick::Last);
        assert!(out.contains("<dc:language>ja</dc:language>"));
        assert!(out.contains("<dc:language>zh-Hant</dc:language>"));
    }

    #[test]
    fn a_patch_the_file_already_carries_returns_the_input() {
        let epub = epub_with(EPUB2);
        let patch = MetadataPatch {
            title: Some("Old Title".into()),
            creators: Some(named(&["Doe, Jane"])),
            language: Some("en".into()),
            publisher: Some("Old Press".into()),
            date: Some("2020-02-01".into()),
            asin: None,
        };
        assert_eq!(edit_metadata(&epub, &patch).unwrap(), epub);
    }

    #[test]
    fn an_edit_reopens_with_the_new_values() {
        let epub = epub_with(EPUB2);
        let patch = MetadataPatch {
            title: Some("New Title".into()),
            publisher: Some("New Press".into()),
            ..Default::default()
        };
        let after = reopen(&edit_metadata(&epub, &patch).unwrap());
        assert_eq!(after.metadata().title, "New Title");
        assert_eq!(after.metadata().publisher.as_deref(), Some("New Press"));
        assert_eq!(after.metadata().creator_names(), ["Doe, Jane"], "untouched");
        assert_eq!(after.metadata().date.as_deref(), Some("2020-02-01"));
    }

    #[test]
    fn edit_asin_updates_existing_identifier() {
        let opf = EPUB2.replace(
            "<dc:language>",
            "<dc:identifier opf:scheme=\"ASIN\">B000000000</dc:identifier>\n    <dc:language>",
        );
        let patch = MetadataPatch {
            asin: Some("B0TESTASIN".into()),
            ..Default::default()
        };
        let after = reopen(&edit_metadata(&epub_with(&opf), &patch).unwrap());
        assert_eq!(after.metadata().asin.as_deref(), Some("B0TESTASIN"));
    }

    #[test]
    fn empty_patch_returns_input_unchanged() {
        let epub = epub_with(EPUB2);
        let out = edit_metadata(&epub, &MetadataPatch::default()).unwrap();
        assert_eq!(out, epub, "an empty patch is a no-op");
    }

    #[test]
    fn non_epub_bytes_error() {
        let patch = MetadataPatch {
            title: Some("x".into()),
            ..Default::default()
        };
        assert!(edit_metadata(b"not an epub", &patch).is_err());
    }

    #[test]
    fn escapes_xml_metacharacters() {
        let patch = MetadataPatch {
            title: Some("Q&A:<Draft>".into()),
            creators: Some(named(&["Smith & Jones"])),
            ..Default::default()
        };
        let out = edit_metadata(&epub_with(EPUB2), &patch).unwrap();
        let opf = opf_of(&out);
        assert!(
            opf.contains("Q&amp;A:&lt;Draft&gt;"),
            "value escaped in the OPF"
        );
        assert!(!opf.contains("Q&A:<Draft>"), "no raw metacharacters");
        assert_eq!(reopen(&out).metadata().title, "Q&A:<Draft>");
        assert_eq!(reopen(&out).metadata().creator_names(), ["Smith & Jones"]);
    }

    #[test]
    fn an_escaped_name_reads_as_its_text() {
        let opf = "<metadata>\n<dc:creator>Smith &amp; Jones</dc:creator>\n</metadata>";
        assert_eq!(
            set_creators(opf, &named(&["Smith & Jones"])),
            opf,
            "an unchanged name is left byte for byte"
        );
    }

    fn reopen_opf(opf: &str) -> crate::formats::epub::OpfData {
        let wrapped = if opf.contains("<package") {
            opf.to_string()
        } else {
            format!(
                "<?xml version=\"1.0\"?>\n<package xmlns=\"http://www.idpf.org/2007/opf\" xmlns:opf=\"http://www.idpf.org/2007/opf\" version=\"3.0\">\n{}\n<manifest/><spine/></package>",
                opf.replace(
                    "<metadata>",
                    "<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">"
                )
            )
        };
        crate::formats::epub::parse_opf(&wrapped).unwrap()
    }
}
