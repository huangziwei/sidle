use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io;

use selectors::parser::{Combinator, Component, Selector};

use crate::formats::epub::edit::{
    EpubPackage, Tok, VOID, add_manifest_item, attr_value, class_list, remove_manifest_item,
    set_attr, tokens,
};
use crate::formats::epub::parse_opf;
use crate::formats::epub::structure::{
    basename, dir_of, relativize, resolve_href, spine_documents,
};
use crate::html::{
    BokoSelectors, compile_html, css_import_targets, extract_stylesheets, inline_css_imports,
};
use crate::model::{Chapter, NodeId, Role};
use crate::style::{
    BorderStyle, ComputedStyle, Declaration, Display, Length, Origin, Specificity, Stylesheet,
    ToCss, VerticalAlign, WritingMode,
};
use crate::util::{decode_text, extract_xml_encoding, percent_decode};

#[derive(Debug, Clone)]
pub struct FlattenedStyles {
    pub sheets: Vec<String>,
    pub generated_classes: usize,
    pub producer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleDiff {
    pub document: String,
    pub text: String,
    pub property: String,
    pub before: String,
    pub after: String,
    pub count: usize,
}

#[derive(Debug, Clone)]
pub struct Restored {
    pub bytes: Vec<u8>,
    pub documents: Vec<String>,
    pub classes: BTreeMap<String, String>,
    pub residual: Vec<String>,
    pub residual_css: String,
    pub diffs: Vec<StyleDiff>,
}

impl Restored {
    pub fn material_diffs(&self) -> usize {
        self.diffs
            .iter()
            .filter(|d| {
                matches!(d.property.as_str(), "blocks" | "text")
                    || (!d.text.is_empty()
                        && !matches!(d.property.as_str(), "writing-mode" | "font-family"))
            })
            .map(|d| d.count)
            .sum()
    }
}

pub fn flattened_styles(epub_bytes: &[u8]) -> io::Result<Option<FlattenedStyles>> {
    let pkg = EpubPackage::parse(epub_bytes)?;
    Ok(detect(&pkg))
}

pub fn restore_styles(epub_bytes: &[u8], reference_bytes: &[u8]) -> io::Result<Restored> {
    let mut pkg = EpubPackage::parse(epub_bytes)?;
    let flat = detect(&pkg)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no flattened stylesheet"))?;
    let reference_pkg = EpubPackage::parse(reference_bytes)?;
    if detect(&reference_pkg).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the reference book is flattened too",
        ));
    }
    let reference = Reference::load(&reference_pkg)?;
    if reference.entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the reference book links no stylesheet",
        ));
    }
    let flat_rules = flat_rules_of(&pkg, &flat.sheets);
    let flat_axes = flat_axes_of(&pkg, &flat.sheets);
    let style_dir = dir_of(&flat.sheets[0]);
    let sheet_paths: BTreeMap<String, String> = reference
        .sheet_members
        .keys()
        .map(|r| (r.clone(), format!("{style_dir}{}", basename(r))))
        .collect();

    let opf_path = pkg.opf_path()?;
    let opf_raw = pkg.opf_bytes()?;
    let opf_text = decode_text(opf_raw, extract_xml_encoding(opf_raw)).into_owned();
    let opf = parse_opf(&opf_text).map_err(io::Error::other)?;
    let opf_base = dir_of(&opf_path);

    let mut removed: BTreeSet<String> = flat.sheets.iter().cloned().collect();
    let mut docs: Vec<(String, String, Vec<String>)> = Vec::new();
    for (path, _) in spine_documents(&opf, &opf_base) {
        let Some(bytes) = pkg.get(&path) else {
            continue;
        };
        let text = decode_text(bytes, extract_xml_encoding(bytes)).into_owned();
        let (linked, _) = extract_stylesheets(&text);
        let doc_dir = dir_of(&path);
        let linked: Vec<String> = linked
            .iter()
            .map(|h| resolve_href(&doc_dir, &percent_decode(h)))
            .collect();
        if !linked.iter().any(|l| flat.sheets.contains(l)) {
            continue;
        }
        for l in &linked {
            if !flat.sheets.contains(l) && pkg.get(l).is_some_and(|b| page_rules_only(&text_of(b)))
            {
                removed.insert(l.clone());
            }
        }
        docs.push((path, text, linked));
    }

    let mut plans: Vec<DocPlan> = Vec::new();
    for (_, text, _) in &docs {
        plans.push(plan_document(text, &flat_rules, &flat_axes, &reference));
    }
    enforce_singletons(&mut plans, &reference);

    let mut residuals = Residuals::default();
    let mut classes: BTreeMap<String, String> = BTreeMap::new();
    let mut diffs: Vec<StyleDiff> = Vec::new();
    let mut documents = Vec::new();
    let mut restored_texts: Vec<(String, String)> = Vec::new();
    for ((path, text, _), plan) in docs.iter().zip(&plans) {
        let entry_ref = &plan.entry;
        let entry_here = &sheet_paths[entry_ref];
        let link = relativize(&dir_of(path), entry_here);
        let rules = &reference.entries[entry_ref];
        let mut mapper = Mapper::new(
            &flat_rules,
            rules,
            &reference,
            plan.kind.as_deref(),
            &mut residuals,
        );
        let out = rewrite_document(text, path, plan, &mut mapper, &removed, &link);
        for (key, choice) in &mapper.memo {
            let from = key.classes.join(" ");
            if !from.is_empty() {
                classes
                    .entry(from)
                    .or_insert_with(|| choice.classes.join(" "));
            }
        }
        restored_texts.push((path.clone(), out));
        documents.push(path.clone());
    }

    let residual_css = render_residual(&residuals);
    for (ref_path, here) in &sheet_paths {
        let mut bytes = reference.sheet_members[ref_path].clone();
        if reference.entries.contains_key(ref_path) && !residual_css.is_empty() {
            bytes.extend_from_slice(residual_css.as_bytes());
        }
        pkg.set(here, bytes);
        add_manifest_item(&mut pkg, here, "text/css")?;
    }
    for path in &removed {
        if sheet_paths.values().any(|p| p == path) {
            continue;
        }
        pkg.remove(path);
        remove_manifest_item(&mut pkg, path)?;
    }
    for (path, out) in &restored_texts {
        pkg.replace(path, out.clone().into_bytes());
    }

    let before_pkg = EpubPackage::parse(epub_bytes)?;
    for ((path, text, _), (_, out)) in docs.iter().zip(&restored_texts) {
        let before = compile_document(&before_pkg, path, text);
        let after = compile_document(&pkg, path, out);
        compare_chapters(path, &before, &after, &mut diffs);
    }

    Ok(Restored {
        bytes: pkg.into_bytes()?,
        documents,
        classes,
        residual: residuals.rules.keys().cloned().collect(),
        residual_css,
        diffs,
    })
}

type Decls = BTreeMap<String, String>;

fn detect(pkg: &EpubPackage) -> Option<FlattenedStyles> {
    let mut sheets = Vec::new();
    let mut generated = 0;
    for name in pkg.names() {
        if !name.to_ascii_lowercase().ends_with(".css") {
            continue;
        }
        let Some(bytes) = pkg.get(name) else { continue };
        let n = flat_rules(&text_of(bytes))
            .keys()
            .filter(|c| is_generated(c))
            .count();
        if n > 0 {
            sheets.push(name.to_string());
            generated += n;
        }
    }
    if sheets.is_empty() {
        return None;
    }
    sheets.sort();
    Some(FlattenedStyles {
        sheets,
        generated_classes: generated,
        producer: producer(pkg),
    })
}

fn is_generated(class: &str) -> bool {
    class
        .strip_prefix("calibre")
        .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
}

fn producer(pkg: &EpubPackage) -> Option<String> {
    let opf = pkg.opf_bytes().ok()?;
    let text = decode_text(opf, extract_xml_encoding(opf));
    let mut from = 0;
    while let Some(rel) = text[from..].find("<dc:contributor") {
        let start = from + rel;
        let open_end = start + text[start..].find('>')?;
        let close = open_end + text[open_end..].find("</dc:contributor>")?;
        let tag = &text[start..=open_end];
        let body = text[open_end + 1..close].trim();
        if tag.contains("bkp") && body.to_ascii_lowercase().contains("calibre") {
            return Some(body.to_string());
        }
        from = close + 1;
    }
    None
}

fn text_of(bytes: &[u8]) -> String {
    decode_text(bytes, extract_xml_encoding(bytes)).into_owned()
}

fn page_rules_only(css: &str) -> bool {
    let sheet = Stylesheet::parse(css);
    sheet.rules.is_empty() && sheet.font_faces.is_empty() && css.contains("@page")
}

#[derive(Debug, Clone, Default)]
struct Shape {
    tag: Option<String>,
    classes: Vec<String>,
    context_classes: Vec<String>,
    context_tags: Vec<String>,
}

fn decompose(selector: &Selector<BokoSelectors>) -> Option<Shape> {
    let mut iter = selector.iter();
    let mut shape = Shape::default();
    for c in &mut iter {
        match c {
            Component::Class(v) => shape.classes.push(v.0.clone()),
            Component::LocalName(n) => {
                shape.tag = Some(n.lower_name.as_ref().to_ascii_lowercase());
            }
            Component::ExplicitUniversalType => {}
            _ => return None,
        }
    }
    loop {
        match iter.next_sequence() {
            None => break,
            Some(Combinator::Descendant) | Some(Combinator::Child) => {}
            Some(_) => return None,
        }
        for c in &mut iter {
            match c {
                Component::Class(v) => shape.context_classes.push(v.0.clone()),
                Component::LocalName(n) => shape
                    .context_tags
                    .push(n.lower_name.as_ref().to_ascii_lowercase()),
                Component::ExplicitUniversalType => {}
                _ => return None,
            }
        }
    }
    Some(shape)
}

fn flat_rules(css: &str) -> BTreeMap<String, Vec<Declaration>> {
    let mut out: BTreeMap<String, Vec<Declaration>> = BTreeMap::new();
    for rule in Stylesheet::parse(css).rules {
        for selector in &rule.selectors {
            let Some(shape) = decompose(selector) else {
                continue;
            };
            if shape.tag.is_some()
                || shape.classes.len() != 1
                || !shape.context_classes.is_empty()
                || !shape.context_tags.is_empty()
            {
                continue;
            }
            out.entry(shape.classes[0].clone())
                .or_default()
                .extend(rule.declarations.iter().cloned());
        }
    }
    out
}

fn flat_rules_of(pkg: &EpubPackage, sheets: &[String]) -> BTreeMap<String, Decls> {
    let mut out = BTreeMap::new();
    for sheet in sheets {
        let Some(bytes) = pkg.get(sheet) else {
            continue;
        };
        for (class, decls) in flat_rules(&text_of(bytes)) {
            out.insert(class, essential(&decls, false));
        }
    }
    out
}

fn zero_debug(v: &str) -> bool {
    ["(Px(0.0))", "(Em(0.0))", "(Rem(0.0))", "(Percent(0.0))"]
        .iter()
        .any(|z| v.ends_with(z))
}

fn zero(l: &Length) -> bool {
    matches!(l, Length::Px(v) | Length::Em(v) | Length::Rem(v) | Length::Percent(v) if *v == 0.0)
}

fn drawn(style: &BorderStyle) -> bool {
    !matches!(style, BorderStyle::Unset | BorderStyle::None)
}

fn ignorable(d: &Declaration, border_drawn: bool, reference: bool) -> bool {
    use Declaration as D;
    match d {
        D::LineHeight(_) | D::UniversalKeyword { .. } | D::Hyphens(_) | D::WritingMode(_) => true,
        D::Display(v) => matches!(v, Display::Block | Display::Inline),
        D::Width(l)
        | D::Height(l)
        | D::MaxWidth(l)
        | D::MaxHeight(l)
        | D::MinWidth(l)
        | D::MinHeight(l) => matches!(l, Length::Auto),
        D::Margin(l)
        | D::MarginTop(l)
        | D::MarginRight(l)
        | D::MarginBottom(l)
        | D::MarginLeft(l)
        | D::Padding(l)
        | D::PaddingTop(l)
        | D::PaddingRight(l)
        | D::PaddingBottom(l)
        | D::PaddingLeft(l)
        | D::BorderRadius(l)
        | D::BorderTopLeftRadius(l)
        | D::BorderTopRightRadius(l)
        | D::BorderBottomLeftRadius(l)
        | D::BorderBottomRightRadius(l) => !reference && zero(l),
        D::TextIndent(l) | D::LetterSpacing(l) | D::WordSpacing(l) => {
            (!reference && zero(l)) || matches!(l, Length::Auto)
        }
        D::VerticalAlign(v) => *v == VerticalAlign::Baseline,
        D::BackgroundColor(c) => c.a == 0,
        D::WhiteSpace(v) => *v == Default::default(),
        D::LineBreak(v) => *v == Default::default(),
        D::WordBreak(v) => *v == Default::default(),
        D::TextTransform(v) => *v == Default::default(),
        D::BorderStyle(_)
        | D::BorderTopStyle(_)
        | D::BorderRightStyle(_)
        | D::BorderBottomStyle(_)
        | D::BorderLeftStyle(_)
        | D::BorderWidth(_)
        | D::BorderTopWidth(_)
        | D::BorderRightWidth(_)
        | D::BorderBottomWidth(_)
        | D::BorderLeftWidth(_)
        | D::BorderColor(_)
        | D::BorderTopColor(_)
        | D::BorderRightColor(_)
        | D::BorderBottomColor(_)
        | D::BorderLeftColor(_) => !border_drawn,
        _ => false,
    }
}

fn essential(decls: &[Declaration], reference: bool) -> Decls {
    use Declaration as D;
    let border_drawn = decls.iter().any(|d| match d {
        D::BorderStyle(s)
        | D::BorderTopStyle(s)
        | D::BorderRightStyle(s)
        | D::BorderBottomStyle(s)
        | D::BorderLeftStyle(s) => drawn(s),
        _ => false,
    });
    let mut out = BTreeMap::new();
    for d in decls {
        if ignorable(d, border_drawn, reference) {
            continue;
        }
        let dbg = match d {
            D::FontSize(Length::Percent(p)) => format!("{:?}", D::FontSize(Length::Em(p / 100.0))),
            _ => format!("{d:?}"),
        };
        let key = dbg
            .split(['(', ' ', '{'])
            .next()
            .unwrap_or(&dbg)
            .to_string();
        out.insert(key, dbg);
    }
    out
}

const INHERITED: &[&str] = &[
    "FontFamily",
    "FontWeight",
    "FontStyle",
    "FontVariant",
    "TextAlign",
    "TextAlignLast",
    "Color",
    "LetterSpacing",
    "WordSpacing",
    "WhiteSpace",
    "LineBreak",
    "WordBreak",
    "OverflowWrap",
    "TextIndent",
    "TextTransform",
    "TextOrientation",
    "TextEmphasisStyle",
    "TextEmphasisColor",
    "TextEmphasisPosition",
    "ListStyleType",
    "Visibility",
];

struct RefRule {
    shape: Shape,
    decls: Decls,
    writing_mode: Option<WritingMode>,
    specificity: Spec,
}

struct RefDoc {
    kind: Option<String>,
    axis: Option<String>,
    entry: Option<String>,
}

struct Reference {
    entries: BTreeMap<String, Vec<RefRule>>,
    sheet_members: BTreeMap<String, Vec<u8>>,
    docs: Vec<RefDoc>,
    kinds: Vec<String>,
    doc_tag_classes: HashSet<String>,
    kind_subjects: HashMap<String, HashSet<String>>,
}

impl Reference {
    fn load(pkg: &EpubPackage) -> io::Result<Self> {
        let opf_path = pkg.opf_path()?;
        let opf_raw = pkg.opf_bytes()?;
        let opf_text = decode_text(opf_raw, extract_xml_encoding(opf_raw));
        let opf = parse_opf(&opf_text).map_err(io::Error::other)?;
        let opf_base = dir_of(&opf_path);
        let mut docs = Vec::new();
        let mut doc_classes: HashSet<String> = HashSet::new();
        let mut doc_tag_classes: HashSet<String> = HashSet::new();
        let mut entry_paths: BTreeSet<String> = BTreeSet::new();
        for (path, _) in spine_documents(&opf, &opf_base) {
            let Some(bytes) = pkg.get(&path) else {
                continue;
            };
            let text = text_of(bytes);
            let (linked, _) = extract_stylesheets(&text);
            let doc_dir = dir_of(&path);
            let entry = linked
                .first()
                .map(|h| resolve_href(&doc_dir, &percent_decode(h)))
                .filter(|p| pkg.get(p).is_some());
            if let Some(e) = &entry {
                entry_paths.insert(e.clone());
            }
            for (tag, class) in all_classes(&text) {
                doc_tag_classes.insert(format!("{tag}.{class}"));
                doc_classes.insert(class);
            }
            docs.push(RefDoc {
                kind: first_class(&text, "body"),
                axis: first_class(&text, "html"),
                entry,
            });
        }
        let mut sheet_members = BTreeMap::new();
        let mut queue: Vec<String> = entry_paths.iter().cloned().collect();
        while let Some(path) = queue.pop() {
            if sheet_members.contains_key(&path) {
                continue;
            }
            let Some(bytes) = pkg.get(&path) else {
                continue;
            };
            for child in css_import_targets(&text_of(bytes), &path) {
                queue.push(child);
            }
            sheet_members.insert(path, bytes.to_vec());
        }
        let mut entries = BTreeMap::new();
        let mut kinds: BTreeSet<String> = docs.iter().filter_map(|d| d.kind.clone()).collect();
        for entry in &entry_paths {
            let css = inline_sheet(pkg, entry);
            let mut rules = Vec::new();
            for rule in Stylesheet::parse(&css).rules {
                for selector in &rule.selectors {
                    let Some(shape) = decompose(selector) else {
                        continue;
                    };
                    if shape.tag.as_deref() == Some("body") && shape.classes.len() == 1 {
                        kinds.insert(shape.classes[0].clone());
                    }
                    let writing_mode = rule.declarations.iter().find_map(|d| match d {
                        Declaration::WritingMode(m) => Some(*m),
                        _ => None,
                    });
                    rules.push(RefRule {
                        shape,
                        decls: essential(&rule.declarations, true),
                        writing_mode,
                        specificity: {
                            let s = Specificity::from_selector(selector);
                            (s.ids, s.classes, s.elements)
                        },
                    });
                }
            }
            entries.insert(entry.clone(), rules);
        }
        let mut context_classes: BTreeSet<String> = BTreeSet::new();
        let mut styled_subjects: BTreeSet<String> = BTreeSet::new();
        for r in entries.values().flatten() {
            context_classes.extend(r.shape.context_classes.iter().cloned());
            if !r.decls.is_empty() || r.writing_mode.is_some() {
                styled_subjects.extend(r.shape.classes.iter().cloned());
            }
        }
        for c in context_classes {
            if !styled_subjects.contains(&c) && !doc_classes.contains(&c) {
                kinds.insert(c);
            }
        }
        let mut kind_subjects: HashMap<String, HashSet<String>> = HashMap::new();
        for r in entries.values().flatten() {
            for c in &r.shape.context_classes {
                if kinds.contains(c) {
                    kind_subjects
                        .entry(c.clone())
                        .or_default()
                        .extend(r.shape.classes.iter().cloned());
                }
            }
        }
        Ok(Self {
            entries,
            sheet_members,
            docs,
            kinds: kinds.into_iter().collect(),
            doc_tag_classes,
            kind_subjects,
        })
    }

    fn kind_count(&self, kind: &str) -> usize {
        self.docs
            .iter()
            .filter(|d| d.kind.as_deref() == Some(kind))
            .count()
    }

    fn default_kind(&self) -> Option<String> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for d in &self.docs {
            if let Some(k) = &d.kind {
                *counts.entry(k).or_default() += 1;
            }
        }
        counts
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
            .map(|(k, _)| k.to_string())
    }

    fn axis_for_kind(&self, kind: Option<&str>) -> Option<String> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for d in &self.docs {
            if d.kind.as_deref() == kind
                && let Some(a) = &d.axis
            {
                *counts.entry(a).or_default() += 1;
            }
        }
        counts
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
            .map(|(a, _)| a.to_string())
    }

    fn axis_class(&self, entry: &str, mode: WritingMode) -> Option<String> {
        self.entries.get(entry)?.iter().find_map(|r| {
            (r.writing_mode == Some(mode)
                && r.shape.tag.is_none()
                && r.shape.classes.len() == 1
                && r.shape.context_classes.is_empty())
            .then(|| r.shape.classes[0].clone())
        })
    }

    fn entries_for_kind(&self, kind: Option<&str>) -> Vec<String> {
        let set: BTreeSet<String> = self
            .docs
            .iter()
            .filter(|d| d.kind.as_deref() == kind)
            .filter_map(|d| d.entry.clone())
            .collect();
        set.into_iter().collect()
    }

    fn entry_for_kind(&self, kind: Option<&str>) -> Option<String> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for d in &self.docs {
            if d.kind.as_deref() == kind
                && let Some(e) = &d.entry
            {
                *counts.entry(e).or_default() += 1;
            }
        }
        counts
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
            .map(|(e, _)| e.to_string())
            .or_else(|| self.entries.keys().next().cloned())
    }
}

fn inline_sheet(pkg: &EpubPackage, path: &str) -> String {
    fn go(pkg: &EpubPackage, path: &str, visited: &mut HashSet<String>) -> Option<String> {
        if !visited.insert(path.to_string()) {
            return Some(String::new());
        }
        let bytes = pkg.get(path)?;
        Some(inline_css_imports(&text_of(bytes), path, |child| {
            go(pkg, child, visited)
        }))
    }
    go(pkg, path, &mut HashSet::new()).unwrap_or_default()
}

fn all_classes(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for tok in tokens(text) {
        if let Tok::Tag {
            closing: false,
            start,
            end,
            name,
            ..
        } = tok
            && name != "html"
            && name != "body"
        {
            out.extend(
                class_list(&text[start..end])
                    .into_iter()
                    .map(|c| (name.clone(), c)),
            );
        }
    }
    out
}

fn first_class(text: &str, tag: &str) -> Option<String> {
    for tok in tokens(text) {
        if let Tok::Tag {
            name,
            closing: false,
            start,
            end,
            ..
        } = &tok
            && name == tag
        {
            return attr_value(&text[*start..*end], "class")
                .and_then(|c| c.split_whitespace().next().map(str::to_string));
        }
    }
    None
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    tag: String,
    classes: Vec<String>,
    ancestors: Vec<String>,
    ancestor_tags: Vec<String>,
    inherited: Vec<(String, String)>,
}

#[derive(Clone, Default)]
struct Choice {
    classes: Vec<String>,
    residual: Option<(String, Decls)>,
    context_hits: usize,
    own: Decls,
}

type Spec = (u16, u16, u16);
type Candidate = (BTreeMap<String, (String, Spec)>, usize);

#[derive(Default)]
struct Residuals {
    rules: BTreeMap<String, (String, Decls)>,
}

impl Residuals {
    fn name(
        &mut self,
        base: &str,
        tag: &str,
        kind: Option<&str>,
        decls: &Decls,
        taken: &HashSet<String>,
    ) -> String {
        let base = if taken.contains(base) {
            format!("{base}-flat")
        } else {
            base.to_string()
        };
        let selector = |name: &str| match (tag, kind) {
            ("html", _) => format!("html.{name}"),
            ("body", Some(k)) => format!("body.{k}.{name}"),
            ("body", None) => format!("body.{name}"),
            (_, Some(k)) => format!("body.{k} {tag}.{name}"),
            (_, None) => format!("{tag}.{name}"),
        };
        let mut name = base.clone();
        let mut n = 1;
        loop {
            match self.rules.get(&name) {
                None => {
                    self.rules
                        .insert(name.clone(), (selector(&name), decls.clone()));
                    return name;
                }
                Some((sel, d)) if *sel == selector(&name) && d == decls => return name,
                Some(_) => {
                    n += 1;
                    name = format!("{base}-{n}");
                }
            }
        }
    }
}

struct Mapper<'a> {
    flat: &'a BTreeMap<String, Decls>,
    rules: &'a [RefRule],
    vocab: HashSet<String>,
    taken: HashSet<String>,
    kinds: HashSet<String>,
    kind: Option<String>,
    residuals: &'a mut Residuals,
    order: HashMap<String, usize>,
    no_context: Vec<usize>,
    by_context: HashMap<String, Vec<usize>>,
    memo: HashMap<Key, Choice>,
}

impl<'a> Mapper<'a> {
    fn new(
        flat: &'a BTreeMap<String, Decls>,
        rules: &'a [RefRule],
        reference: &Reference,
        kind: Option<&str>,
        residuals: &'a mut Residuals,
    ) -> Self {
        let vocab = reference.doc_tag_classes.clone();
        let mut taken: HashSet<String> = vocab
            .iter()
            .filter_map(|tc| tc.split_once('.').map(|(_, c)| c.to_string()))
            .collect();
        taken.extend(rules.iter().flat_map(|r| r.shape.classes.iter().cloned()));
        taken.extend(reference.kinds.iter().cloned());
        let mut order: HashMap<String, usize> = HashMap::new();
        let mut no_context = Vec::new();
        let mut by_context: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, r) in rules.iter().enumerate() {
            if r.shape.classes.len() == 1 {
                order.entry(r.shape.classes[0].clone()).or_insert(i);
            }
            match r.shape.context_classes.first() {
                None => no_context.push(i),
                Some(c) => by_context.entry(c.clone()).or_default().push(i),
            }
        }
        Self {
            flat,
            rules,
            vocab,
            taken,
            kinds: reference.kinds.iter().cloned().collect(),
            kind: kind.map(str::to_string),
            residuals,
            order,
            no_context,
            by_context,
            memo: HashMap::new(),
        }
    }

    fn applies(shape: &Shape, tag: &str, ancestors: &[String], ancestor_tags: &[String]) -> bool {
        shape.tag.as_deref().is_none_or(|t| t == tag)
            && shape.context_classes.iter().all(|c| ancestors.contains(c))
            && shape.context_tags.iter().all(|t| ancestor_tags.contains(t))
    }

    fn choose(&mut self, key: Key) -> Choice {
        if let Some(c) = self.memo.get(&key) {
            return c.clone();
        }
        let mut choice = self.compute(&key);
        if let Some((base, decls)) = choice.residual.take() {
            let name =
                self.residuals
                    .name(&base, &key.tag, self.kind.as_deref(), &decls, &self.taken);
            if !choice.classes.contains(&name) {
                choice.classes.push(name.clone());
            }
            choice.residual = Some((name, decls));
        }
        self.memo.insert(key, choice.clone());
        choice
    }

    fn compute(&self, key: &Key) -> Choice {
        let inherited: Decls = key.inherited.iter().cloned().collect();
        let mut needed: Decls = BTreeMap::new();
        for x in &key.classes {
            if let Some(d) = self.flat.get(x) {
                needed.extend(d.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        }
        if key.tag == "body" {
            needed.retain(|k, _| {
                matches!(
                    k.as_str(),
                    "FontSize"
                        | "FontWeight"
                        | "FontStyle"
                        | "TextAlign"
                        | "Color"
                        | "BackgroundColor"
                )
            });
        } else if key.tag == "html" {
            needed.clear();
        }
        let mut idx: Vec<usize> = self.no_context.clone();
        for a in &key.ancestors {
            if let Some(v) = self.by_context.get(a) {
                idx.extend(v.iter().copied());
            }
        }
        idx.sort_unstable();
        idx.dedup();
        let applicable: Vec<&RefRule> = idx
            .iter()
            .map(|i| &self.rules[*i])
            .filter(|r| Self::applies(&r.shape, &key.tag, &key.ancestors, &key.ancestor_tags))
            .collect();
        let mut free: BTreeMap<String, (String, Spec)> = BTreeMap::new();
        for r in applicable.iter().filter(|r| r.shape.classes.is_empty()) {
            for (k, v) in &r.decls {
                if free.get(k).is_none_or(|(_, s)| *s <= r.specificity) {
                    free.insert(k.clone(), (v.clone(), r.specificity));
                }
            }
        }
        let given = |k: &str, v: &str| {
            free.get(k).is_some_and(|(f, _)| f == v) || inherited.get(k).is_some_and(|f| f == v)
        };
        let mut target: Decls = needed
            .iter()
            .filter(|(k, v)| !given(k, v))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut candidates: BTreeMap<String, Candidate> = BTreeMap::new();
        let order = &self.order;
        for r in applicable.iter().filter(|r| {
            r.shape.classes.len() == 1
                && (key.tag == "body" || !self.kinds.contains(&r.shape.classes[0]))
        }) {
            let e = candidates.entry(r.shape.classes[0].clone()).or_default();
            for (k, v) in &r.decls {
                if e.0.get(k).is_none_or(|(_, s)| *s <= r.specificity) {
                    e.0.insert(k.clone(), (v.clone(), r.specificity));
                }
            }
            e.1 += usize::from(
                self.kind
                    .as_deref()
                    .is_some_and(|k| r.shape.context_classes.iter().any(|c| c == k)),
            );
        }
        let wins = |k: &str, spec: Spec| free.get(k).is_none_or(|(_, fs)| spec > *fs);
        let admissible = |decls: &BTreeMap<String, (String, Spec)>, target: &Decls| {
            decls.iter().all(|(k, (v, spec))| {
                !wins(k, *spec)
                    || target.get(k).is_some_and(|t| t == v)
                    || (!target.contains_key(k) && needed.get(k).is_some_and(|n| n == v))
                    || (!needed.contains_key(k) && zero_debug(v))
            })
        };
        let affinity_of = |name: &str| {
            key.classes
                .iter()
                .map(|x| {
                    if x == name {
                        3
                    } else if name.starts_with(x.as_str()) || x.starts_with(name) {
                        2
                    } else {
                        0
                    }
                })
                .max()
                .unwrap_or(0)
        };
        let mut chosen: Vec<String> = Vec::new();
        let mut hits = 0;
        loop {
            let mut best: Option<(usize, i32, usize, &String)> = None;
            for (name, (decls, _)) in &candidates {
                if chosen.contains(name) || !admissible(decls, &target) {
                    continue;
                }
                let covers = decls
                    .iter()
                    .filter(|(k, (v, spec))| wins(k, *spec) && target.get(*k) == Some(v))
                    .count();
                if covers == 0 {
                    continue;
                }
                let rank = (covers, affinity_of(name), usize::MAX - name.len(), name);
                if best.is_none_or(|b| rank > b) {
                    best = Some(rank);
                }
            }
            let Some((_, _, _, name)) = best else { break };
            let name = name.clone();
            let (decls, ctx) = &candidates[&name];
            target.retain(|k, v| {
                !decls
                    .get(k)
                    .is_some_and(|(c, spec)| c == v && wins(k, *spec))
            });
            hits += *ctx;
            chosen.push(name);
        }
        for x in &key.classes {
            if key.tag == "body" || key.tag == "html" || is_generated(x) {
                continue;
            }
            let base = x.trim_end_matches(|c: char| c.is_ascii_digit());
            let used = |c: &str| self.vocab.contains(&format!("{}.{c}", key.tag));
            let name = if used(x) {
                x.as_str()
            } else if !base.is_empty() && used(base) {
                base
            } else {
                continue;
            };
            if chosen.iter().any(|c| c == name) || self.kinds.contains(name) {
                continue;
            }
            let harmless = candidates
                .get(name)
                .is_none_or(|(d, _)| admissible(d, &BTreeMap::new()));
            if harmless {
                chosen.push(name.to_string());
            }
        }
        chosen.sort_by_key(|c| {
            (
                std::cmp::Reverse(affinity_of(c)),
                order.get(c).copied().unwrap_or(usize::MAX),
            )
        });
        let residual = (!target.is_empty()).then(|| {
            let base = key
                .classes
                .iter()
                .find(|x| !is_generated(x))
                .cloned()
                .unwrap_or_else(|| format!("{}-flat", key.tag));
            (base, target.clone())
        });
        let mut own: Decls = free
            .iter()
            .map(|(k, (v, _))| (k.clone(), v.clone()))
            .collect();
        for c in &chosen {
            if let Some((d, _)) = candidates.get(c) {
                for (k, (v, spec)) in d {
                    if wins(k, *spec) {
                        own.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        own.extend(target.iter().map(|(k, v)| (k.clone(), v.clone())));
        Choice {
            classes: chosen,
            residual,
            context_hits: hits,
            own,
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Score {
    body: u8,
    hits: usize,
    fit: std::cmp::Reverse<usize>,
}

struct DocPlan {
    kind: Option<String>,
    flat_kind: Option<String>,
    flat_axis: WritingMode,
    axis: Option<String>,
    entry: String,
    scores: BTreeMap<Option<String>, (Score, String)>,
}

fn only_class_attr(tag: &str, name: &str) -> bool {
    let inner = tag
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_end_matches('/')
        .trim();
    let rest = inner[name.len()..].trim();
    rest.starts_with("class=")
        && attr_value(tag, "class").is_some_and(|c| {
            let attr = format!("class=\"{c}\"");
            let attr2 = format!("class='{c}'");
            rest == attr || rest == attr2
        })
}

struct Frame {
    tag: String,
    classes: Vec<String>,
    inherited: Vec<(String, String)>,
    unwrap: bool,
}

fn walk<F>(text: &str, plan: &DocPlan, mapper: &mut Mapper<'_>, mut visit: F)
where
    F: FnMut(&Tok, &str, Option<(Vec<String>, Vec<String>, Choice)>, bool),
{
    let mut stack: Vec<Frame> = Vec::new();
    let toks = tokens(text);
    for tok in &toks {
        match tok {
            Tok::Text { .. } => visit(tok, "", None, false),
            Tok::Tag {
                start,
                end,
                name,
                closing,
                self_closing,
            } => {
                let raw = &text[*start..*end];
                if *closing {
                    let unwrap = stack
                        .iter()
                        .rposition(|f| &f.tag == name)
                        .map(|i| stack.drain(i..).next().is_some_and(|f| f.unwrap))
                        .unwrap_or(false);
                    visit(tok, raw, None, unwrap);
                    continue;
                }
                let is_void = *self_closing || VOID.contains(&name.as_str());
                let mut ancestors: Vec<String> =
                    stack.iter().flat_map(|f| f.classes.clone()).collect();
                let mut ancestor_tags: Vec<String> = stack.iter().map(|f| f.tag.clone()).collect();
                if let Some(k) = &plan.kind {
                    ancestors.push(k.clone());
                }
                if let Some(a) = &plan.axis {
                    ancestors.push(a.clone());
                }
                ancestors.sort();
                ancestors.dedup();
                ancestor_tags.sort();
                ancestor_tags.dedup();
                let inherited = stack
                    .last()
                    .map(|f| f.inherited.clone())
                    .unwrap_or_default();
                let flat_classes = class_list(raw);
                let key = Key {
                    tag: name.clone(),
                    classes: flat_classes.clone(),
                    ancestors,
                    ancestor_tags,
                    inherited,
                };
                let mut choice = mapper.choose(key);
                if name == "body" {
                    if let Some(k) = &plan.kind
                        && !choice.classes.contains(k)
                    {
                        choice.classes.insert(0, k.clone());
                    }
                } else if name == "html" {
                    choice.classes = plan.axis.iter().cloned().collect();
                }
                let unwrap = name == "span"
                    && choice.classes.is_empty()
                    && !flat_classes.is_empty()
                    && only_class_attr(raw, name);
                let mut inherited: BTreeMap<String, String> = stack
                    .last()
                    .map(|f| f.inherited.iter().cloned().collect())
                    .unwrap_or_default();
                for (k, v) in &choice.own {
                    if INHERITED.contains(&k.as_str()) {
                        inherited.insert(k.clone(), v.clone());
                    }
                }
                visit(
                    tok,
                    raw,
                    Some((flat_classes, choice.classes.clone(), choice.clone())),
                    unwrap,
                );
                if !is_void {
                    stack.push(Frame {
                        tag: name.clone(),
                        classes: choice.classes,
                        inherited: inherited.into_iter().collect(),
                        unwrap,
                    });
                }
            }
        }
    }
}

fn plan_document(
    text: &str,
    flat: &BTreeMap<String, Decls>,
    axes: &BTreeMap<String, WritingMode>,
    reference: &Reference,
) -> DocPlan {
    let flat_kind = first_class(text, "body").filter(|k| !is_generated(k));
    let flat_axis_mode = first_class(text, "body")
        .and_then(|k| axes.get(&k).copied())
        .unwrap_or_default();
    let mut scores: BTreeMap<Option<String>, (Score, String)> = BTreeMap::new();
    let mut present: HashSet<String> = HashSet::new();
    for (_, class) in all_classes(text) {
        present.insert(
            class
                .trim_end_matches(|c: char| c.is_ascii_digit())
                .to_string(),
        );
        present.insert(class);
    }
    let reachable = |kind: &String| {
        flat_kind.as_ref() == Some(kind)
            || reference.kind_count(kind) > 0
            || reference
                .kind_subjects
                .get(kind)
                .is_some_and(|s| s.iter().any(|c| present.contains(c)))
    };
    let mut candidates: Vec<Option<String>> = vec![None];
    candidates.extend(
        reference
            .kinds
            .iter()
            .filter(|k| reachable(k))
            .cloned()
            .map(Some),
    );
    for kind in &candidates {
        let mut entries = reference.entries_for_kind(kind.as_deref());
        if entries.is_empty() {
            entries = reference.entries.keys().cloned().collect();
        }
        for entry in entries {
            let Some(rules) = reference.entries.get(&entry) else {
                continue;
            };
            let axis = reference
                .axis_for_kind(kind.as_deref())
                .or_else(|| reference.axis_class(&entry, flat_axis_mode));
            let plan = DocPlan {
                kind: kind.clone(),
                flat_kind: flat_kind.clone(),
                flat_axis: flat_axis_mode,
                axis,
                entry: entry.clone(),
                scores: BTreeMap::new(),
            };
            let mut scratch = Residuals::default();
            let mut mapper = Mapper::new(flat, rules, reference, kind.as_deref(), &mut scratch);
            let mut hits = 0;
            let mut residual = 0;
            walk(text, &plan, &mut mapper, |_, _, choice, _| {
                if let Some((_, _, c)) = choice {
                    hits += c.context_hits;
                    residual += c.residual.as_ref().map_or(0, |(_, d)| d.len());
                }
            });
            let score = Score {
                body: u8::from(kind.is_some() && *kind == flat_kind),
                hits,
                fit: std::cmp::Reverse(residual),
            };
            if scores.get(kind).is_none_or(|(s, _)| score > *s) {
                scores.insert(kind.clone(), (score, entry));
            }
        }
    }
    let (kind, entry) = choose_kind(&scores, &flat_kind, reference);
    let axis = reference
        .axis_for_kind(kind.as_deref())
        .or_else(|| reference.axis_class(&entry, flat_axis_mode));
    DocPlan {
        kind,
        flat_kind,
        flat_axis: flat_axis_mode,
        axis,
        entry,
        scores,
    }
}

fn choose_kind(
    scores: &BTreeMap<Option<String>, (Score, String)>,
    flat_kind: &Option<String>,
    reference: &Reference,
) -> (Option<String>, String) {
    let Some(best) = scores.values().map(|(s, _)| *s).max() else {
        return (
            None,
            reference.entries.keys().next().cloned().unwrap_or_default(),
        );
    };
    let tied: Vec<&Option<String>> = scores
        .iter()
        .filter(|(_, (s, _))| *s == best)
        .map(|(k, _)| k)
        .collect();
    let kind = if flat_kind.is_some() && tied.contains(&flat_kind) {
        flat_kind.clone()
    } else if best.hits > 0 {
        tied.iter()
            .find(|k| k.is_some())
            .cloned()
            .cloned()
            .flatten()
    } else if flat_kind.is_some() {
        reference.default_kind()
    } else {
        tied.first().cloned().cloned().flatten()
    };
    let entry = scores
        .get(&kind)
        .map(|(_, e)| e.clone())
        .or_else(|| reference.entry_for_kind(kind.as_deref()))
        .unwrap_or_default();
    (kind, entry)
}

fn flat_axes_of(pkg: &EpubPackage, sheets: &[String]) -> BTreeMap<String, WritingMode> {
    let mut out = BTreeMap::new();
    for sheet in sheets {
        let Some(bytes) = pkg.get(sheet) else {
            continue;
        };
        for (class, decls) in flat_rules(&text_of(bytes)) {
            if let Some(mode) = decls.iter().rev().find_map(|d| match d {
                Declaration::WritingMode(m) => Some(*m),
                _ => None,
            }) {
                out.insert(class, mode);
            }
        }
    }
    out
}

fn enforce_singletons(plans: &mut [DocPlan], reference: &Reference) {
    for kind in &reference.kinds {
        if reference.kind_count(kind) != 1 {
            continue;
        }
        let key = Some(kind.clone());
        let holders: Vec<usize> = plans
            .iter()
            .enumerate()
            .filter(|(_, p)| p.kind == key)
            .map(|(i, _)| i)
            .collect();
        if holders.len() <= 1 {
            continue;
        }
        let keep = holders
            .iter()
            .copied()
            .max_by(|a, b| {
                let sa = plans[*a]
                    .scores
                    .get(&key)
                    .map(|(s, _)| *s)
                    .unwrap_or_default();
                let sb = plans[*b]
                    .scores
                    .get(&key)
                    .map(|(s, _)| *s)
                    .unwrap_or_default();
                sa.cmp(&sb).then(b.cmp(a))
            })
            .unwrap();
        for i in holders {
            if i == keep {
                continue;
            }
            let p = &mut plans[i];
            let mut remaining = p.scores.clone();
            remaining.remove(&key);
            let (kind, entry) = choose_kind(&remaining, &p.flat_kind, reference);
            p.axis = reference
                .axis_for_kind(kind.as_deref())
                .or_else(|| reference.axis_class(&entry, p.flat_axis));
            p.kind = kind;
            p.entry = entry;
        }
    }
}

fn rewrite_document(
    text: &str,
    path: &str,
    plan: &DocPlan,
    mapper: &mut Mapper<'_>,
    removed: &BTreeSet<String>,
    link: &str,
) -> String {
    let doc_dir = dir_of(path);
    let mut out = String::with_capacity(text.len());
    let mut linked = false;
    walk(text, plan, mapper, |tok, raw, choice, unwrap| match tok {
        Tok::Text { start, end } => out.push_str(&text[*start..*end]),
        Tok::Tag { name, closing, .. } => {
            if *closing {
                if !unwrap {
                    out.push_str(raw);
                }
                return;
            }
            if name == "link" {
                let is_sheet =
                    attr_value(raw, "rel").is_some_and(|r| r.eq_ignore_ascii_case("stylesheet"));
                let target =
                    attr_value(raw, "href").map(|h| resolve_href(&doc_dir, &percent_decode(&h)));
                if is_sheet && target.is_some_and(|t| removed.contains(&t)) {
                    if !linked {
                        linked = true;
                        out.push_str(&set_attr(raw, "href", Some(link)));
                    } else if let Some(nl) = out.rfind('\n')
                        && out[nl..].trim().is_empty()
                    {
                        out.truncate(nl);
                    }
                    return;
                }
            }
            if unwrap {
                return;
            }
            let Some((flat_classes, classes, _)) = choice else {
                out.push_str(raw);
                return;
            };
            if flat_classes == classes {
                out.push_str(raw);
                return;
            }
            let value = if classes.is_empty() {
                None
            } else {
                Some(classes.join(" "))
            };
            out.push_str(&set_attr(raw, "class", value.as_deref()));
        }
    });
    out
}

fn render_residual(residuals: &Residuals) -> String {
    let mut css = String::new();
    for (selector, decls) in residuals.rules.values() {
        if decls.is_empty() {
            continue;
        }
        css.push_str(&format!("\n{selector} {{\n"));
        for value in decls.values() {
            if let Some(line) = css_line(value) {
                css.push_str("  ");
                css.push_str(&line);
                css.push_str(";\n");
            }
        }
        css.push_str("}\n");
    }
    css
}

fn css_line(debug: &str) -> Option<String> {
    let (name, rest) = debug.split_once('(')?;
    let inner = rest.strip_suffix(')')?;
    let prop = kebab(name);
    let value = match name {
        n if n.ends_with("Width") && !n.starts_with("Border") || n == "Height" => length(inner)?,
        "FontSize" | "TextIndent" | "LetterSpacing" | "WordSpacing" | "LineHeight" => {
            length(inner)?
        }
        n if n.starts_with("Margin") || n.starts_with("Padding") || n.ends_with("Width") => {
            length(inner)?
        }
        "FontFamily" => inner.trim_matches('"').to_string(),
        "FontWeight" => inner
            .trim_start_matches("FontWeight(")
            .trim_end_matches(')')
            .to_string(),
        n if n.ends_with("Color") => color(inner)?,
        _ => kebab(inner.split(['(', ' ']).next()?),
    };
    Some(format!("{prop}: {value}"))
}

fn kebab(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn length(inner: &str) -> Option<String> {
    let (unit, v) = inner.split_once('(')?;
    let v = v.trim_end_matches(')');
    let n: f32 = v.parse().ok()?;
    let unit = match unit {
        "Px" => "px",
        "Em" => "em",
        "Rem" => "rem",
        "Percent" => "%",
        _ => return None,
    };
    Some(format!("{}{unit}", trim_float(n)))
}

fn trim_float(n: f32) -> String {
    let s = format!("{n:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

fn color(inner: &str) -> Option<String> {
    let mut parts = [0u8; 4];
    for (i, field) in inner.trim_matches(['{', '}', ' ']).split(',').enumerate() {
        let (_, v) = field.trim().split_once(": ")?;
        *parts.get_mut(i)? = v.trim().parse().ok()?;
    }
    Some(format!("#{:02x}{:02x}{:02x}", parts[0], parts[1], parts[2]))
}

fn compile_document(pkg: &EpubPackage, path: &str, text: &str) -> Chapter {
    let (linked, inline) = extract_stylesheets(text);
    let doc_dir = dir_of(path);
    let mut sheets = Vec::new();
    for href in linked {
        let abs = resolve_href(&doc_dir, &percent_decode(&href));
        if pkg.get(&abs).is_some() {
            sheets.push((Stylesheet::parse(&inline_sheet(pkg, &abs)), Origin::Author));
        }
    }
    for css in inline {
        sheets.push((Stylesheet::parse(&css), Origin::Author));
    }
    compile_html(text, &sheets)
}

fn block_props(s: &ComputedStyle) -> Vec<(&'static str, String)> {
    let mut v = vec![
        ("writing-mode", s.writing_mode.to_css_string()),
        ("font-size", s.font_size.to_css_string()),
        ("font-family", s.font_family.clone().unwrap_or_default()),
        ("font-weight", s.font_weight.to_css_string()),
        ("font-style", s.font_style.to_css_string()),
        ("text-align", s.text_align.to_css_string()),
        ("margin-top", s.margin_top.to_css_string()),
        ("margin-bottom", s.margin_bottom.to_css_string()),
        ("margin-left", s.margin_left.to_css_string()),
        ("margin-right", s.margin_right.to_css_string()),
        ("padding-top", s.padding_top.to_css_string()),
        ("padding-bottom", s.padding_bottom.to_css_string()),
        ("padding-left", s.padding_left.to_css_string()),
        ("padding-right", s.padding_right.to_css_string()),
        ("width", s.width.to_css_string()),
        ("height", s.height.to_css_string()),
        (
            "text-combine-upright",
            s.text_combine_upright.to_css_string(),
        ),
    ];
    for (name, style, width) in [
        ("border-top", s.border_style_top, s.border_width_top),
        (
            "border-bottom",
            s.border_style_bottom,
            s.border_width_bottom,
        ),
        ("border-left", s.border_style_left, s.border_width_left),
        ("border-right", s.border_style_right, s.border_width_right),
    ] {
        let value = if drawn(&style) {
            format!("{} {}", style.to_css_string(), width.to_css_string())
        } else {
            "none".to_string()
        };
        v.push((name, value));
    }
    v
}

fn text_props(s: &ComputedStyle) -> Vec<(&'static str, String)> {
    vec![
        ("writing-mode", s.writing_mode.to_css_string()),
        ("font-size", s.font_size.to_css_string()),
        ("font-family", s.font_family.clone().unwrap_or_default()),
        ("font-weight", s.font_weight.to_css_string()),
        ("font-style", s.font_style.to_css_string()),
        (
            "text-combine-upright",
            s.text_combine_upright.to_css_string(),
        ),
        (
            "color",
            s.color.map(|c| c.to_css_string()).unwrap_or_default(),
        ),
    ]
}

fn is_block(role: Role) -> bool {
    matches!(
        role,
        Role::Paragraph | Role::Heading(_) | Role::Container | Role::Image | Role::ListItem
    )
}

fn node_text(chapter: &Chapter, id: NodeId) -> String {
    let mut out = String::new();
    let mut stack = vec![id];
    while let Some(n) = stack.pop() {
        let Some(node) = chapter.node(n) else {
            continue;
        };
        if node.role == Role::Text {
            out.push_str(chapter.text(node.text));
            if out.chars().count() > 24 {
                break;
            }
        }
        let children: Vec<NodeId> = chapter.children(n).collect();
        stack.extend(children.into_iter().rev());
    }
    out.chars().take(24).collect::<String>().trim().to_string()
}

fn compare_chapters(document: &str, before: &Chapter, after: &Chapter, diffs: &mut Vec<StyleDiff>) {
    let mut push = |text: String, property: &str, b: String, a: String| {
        if let Some(d) = diffs.iter_mut().find(|d| {
            d.document == document && d.property == property && d.before == b && d.after == a
        }) {
            d.count += 1;
        } else {
            diffs.push(StyleDiff {
                document: document.to_string(),
                text,
                property: property.to_string(),
                before: b,
                after: a,
                count: 1,
            });
        }
    };
    let blocks = |c: &Chapter| -> Vec<NodeId> {
        c.iter_dfs()
            .filter(|id| c.node(*id).is_some_and(|n| is_block(n.role)))
            .collect()
    };
    let (bb, ab) = (blocks(before), blocks(after));
    if bb.len() != ab.len() {
        push(
            String::new(),
            "blocks",
            bb.len().to_string(),
            ab.len().to_string(),
        );
    } else {
        for (b, a) in bb.iter().zip(&ab) {
            let (Some(sb), Some(sa)) = (
                before.node(*b).and_then(|n| before.styles.get(n.style)),
                after.node(*a).and_then(|n| after.styles.get(n.style)),
            ) else {
                continue;
            };
            for ((name, vb), (_, va)) in block_props(sb).into_iter().zip(block_props(sa)) {
                if vb != va {
                    push(node_text(before, *b), name, vb, va);
                }
            }
        }
    }
    let runs = |c: &Chapter| -> Vec<(String, Vec<(&'static str, String)>)> {
        c.iter_dfs()
            .filter_map(|id| {
                let n = c.node(id)?;
                (n.role == Role::Text).then(|| {
                    (
                        c.text(n.text).to_string(),
                        c.styles.get(n.style).map(text_props).unwrap_or_default(),
                    )
                })
            })
            .collect()
    };
    let (rb, ra) = (runs(before), runs(after));
    let tb: String = rb.iter().map(|r| r.0.as_str()).collect();
    let ta: String = ra.iter().map(|r| r.0.as_str()).collect();
    if tb != ta {
        push(
            String::new(),
            "text",
            tb.chars().count().to_string(),
            ta.chars().count().to_string(),
        );
        return;
    }
    let mut ia = 0;
    let mut consumed_a = 0;
    let mut pos_b = 0;
    for (text, props) in &rb {
        let start = pos_b;
        let end = pos_b + text.len();
        pos_b = end;
        let mut p = start;
        while p < end && ia < ra.len() {
            let a_end = consumed_a + ra[ia].0.len();
            for ((name, vb), (_, va)) in props.iter().zip(&ra[ia].1) {
                if vb != va {
                    push(
                        text.chars().take(24).collect(),
                        name,
                        vb.clone(),
                        va.clone(),
                    );
                }
            }
            if a_end <= end {
                consumed_a = a_end;
                ia += 1;
                p = a_end;
            } else {
                p = end;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn epub(files: &[(&str, &str)]) -> Vec<u8> {
        use zip::write::SimpleFileOptions;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("mimetype", stored).unwrap();
        zip.write_all(b"application/epub+zip").unwrap();
        zip.start_file("META-INF/container.xml", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#).unwrap();
        for (name, body) in files {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn opf(items: &[(&str, &str, &str)], spine: &[&str], contributor: &str) -> String {
        let mut s = String::from(
            r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf"><dc:identifier id="id">x</dc:identifier><dc:title>T</dc:title><dc:language>ja</dc:language>"#,
        );
        s.push_str(contributor);
        s.push_str("</metadata>\n  <manifest>\n");
        for (id, href, mt) in items {
            s.push_str(&format!(
                "    <item href=\"{href}\" id=\"{id}\" media-type=\"{mt}\"/>\n"
            ));
        }
        s.push_str("  </manifest>\n  <spine>");
        for id in spine {
            s.push_str(&format!("<itemref idref=\"{id}\"/>"));
        }
        s.push_str("</spine>\n</package>");
        s
    }

    fn reference() -> Vec<u8> {
        let title = r#"<?xml version="1.0" encoding="UTF-8"?><html xmlns="http://www.w3.org/1999/xhtml" class="hltr"><head><link rel="stylesheet" type="text/css" href="styles/book.css"/></head><body class="p-titlepage"><div class="main"><div class="author gfont"><p>A</p></div></div></body></html>"#;
        let text = r#"<?xml version="1.0" encoding="UTF-8"?><html xmlns="http://www.w3.org/1999/xhtml" class="vrtl"><head><link rel="stylesheet" type="text/css" href="styles/book.css"/></head><body class="p-text"><div class="main"><p class="start-1em gfont bold">B</p><p>C<span class="tcy">12</span></p></div></body></html>"#;
        epub(&[
            (
                "OEBPS/content.opf",
                &opf(
                    &[
                        ("t", "title.xhtml", "application/xhtml+xml"),
                        ("c1", "ch1.xhtml", "application/xhtml+xml"),
                        ("c2", "ch2.xhtml", "application/xhtml+xml"),
                        ("css", "styles/book.css", "text/css"),
                        ("base", "styles/base.css", "text/css"),
                    ],
                    &["t", "c1", "c2"],
                    "",
                ),
            ),
            ("OEBPS/title.xhtml", title),
            ("OEBPS/ch1.xhtml", text),
            ("OEBPS/ch2.xhtml", text),
            (
                "OEBPS/styles/book.css",
                "@charset \"UTF-8\";\n@import url(base.css);\n.p-titlepage .main { margin: 0 auto 0 auto; text-align: center; padding: 4em 1em 1.5em 1em; }\n.p-titlepage .author { margin: 1.5em 0 3em 0; padding: 1.5em 0 0 0; font-size: 0.85em; border-top: 1px solid black; }\n.p-titlepage .author p { margin: 0.5em 0 0 0; }\n",
            ),
            (
                "OEBPS/styles/base.css",
                "html, .hltr { -webkit-writing-mode: horizontal-tb; }\n.vrtl { -webkit-writing-mode: vertical-rl; }\nbody { margin: 0; padding: 0; font-size: 100%; text-align: justify; }\np { margin: 0; text-indent: 0; }\n.gfont { font-family: sans-serif-ja, sans-serif; }\n.bold { font-weight: bold; }\n.start-1em { margin-top: 1em; }\n.tcy { -webkit-text-combine: horizontal; text-combine-upright: all; }\n",
            ),
        ])
    }

    fn flattened() -> Vec<u8> {
        let title = r#"<?xml version='1.0' encoding='utf-8'?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head>
  <link href="styles/stylesheet.css" rel="stylesheet" type="text/css"/>
  <link href="styles/page_styles.css" rel="stylesheet" type="text/css"/>
</head>
<body class="p-titlepage">
  <div class="main1"><div class="author"><p class="calibre2"><span>A</span></p></div></div>
</body>
</html>"#;
        let text = r#"<?xml version='1.0' encoding='utf-8'?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head>
  <link href="styles/stylesheet.css" rel="stylesheet" type="text/css"/>
  <link href="styles/page_styles.css" rel="stylesheet" type="text/css"/>
</head>
<body class="p-titlepage">
  <div class="main"><p class="start-1em"><span class="calibre5">B</span></p><p class="calibre1"><span class="calibre5">C</span><span class="tcy">12</span></p></div>
</body>
</html>"#;
        let css = ".author {\n  display: block;\n  font-size: 0.85em;\n  line-height: 150%;\n  padding: 1.5em 0 0;\n  margin: 1.5em 0 3em;\n  border-top: black solid 1px;\n}\n.calibre {\n  display: block;\n  line-height: 150%;\n}\n.calibre1 {\n  display: block;\n  line-height: 150%;\n  text-indent: inherit;\n  margin: 0;\n}\n.calibre2 {\n  display: block;\n  line-height: 150%;\n  margin: 0.5em 0 0;\n}\n.calibre5 {\n  line-height: 150%;\n}\n.main {\n  display: block;\n  line-height: 150%;\n}\n.main1 {\n  display: block;\n  line-height: 1.6;\n  text-align: center;\n  padding: 4em 1em 1.5em;\n  margin: 0 auto;\n}\n.p-titlepage {\n  -webkit-writing-mode: vertical-rl;\n  display: block;\n  font-size: 100%;\n  line-height: 1.75;\n  text-align: justify;\n  writing-mode: vertical-rl;\n  margin: 0 5pt;\n}\n.start-1em {\n  display: block;\n  font-family: sans-serif-ja, sans-serif;\n  font-weight: bold;\n  line-height: 150%;\n  margin: 1em 0 0;\n}\n.tcy {\n  -webkit-text-combine: horizontal;\n  line-height: 150%;\n  text-combine-upright: all;\n}\n.pcalibre:hover {\n  color: #696969;\n}\n";
        epub(&[
            (
                "OEBPS/content.opf",
                &opf(
                    &[
                        ("t", "title.xhtml", "application/xhtml+xml"),
                        ("c1", "ch1.xhtml", "application/xhtml+xml"),
                        ("c2", "ch2.xhtml", "application/xhtml+xml"),
                        ("css", "styles/stylesheet.css", "text/css"),
                        ("page", "styles/page_styles.css", "text/css"),
                    ],
                    &["t", "c1", "c2"],
                    r#"<dc:contributor opf:role="bkp">calibre (2.56.0) [http://calibre-ebook.com]</dc:contributor>"#,
                ),
            ),
            ("OEBPS/title.xhtml", title),
            ("OEBPS/ch1.xhtml", text),
            ("OEBPS/ch2.xhtml", text),
            ("OEBPS/styles/stylesheet.css", css),
            (
                "OEBPS/styles/page_styles.css",
                "@page {\n  margin-bottom: 5pt;\n  margin-top: 5pt;\n  }\n",
            ),
        ])
    }

    #[test]
    fn detects_the_flattener_by_its_generated_classes() {
        let f = flattened_styles(&flattened()).unwrap().unwrap();
        assert_eq!(f.sheets, vec!["OEBPS/styles/stylesheet.css"]);
        assert_eq!(f.generated_classes, 4);
        assert!(f.producer.unwrap().starts_with("calibre (2.56.0)"));
        assert!(flattened_styles(&reference()).unwrap().is_none());
    }

    #[test]
    fn restores_class_names_page_kinds_and_axis_from_the_reference() {
        let r = restore_styles(&flattened(), &reference()).unwrap();
        let pkg = EpubPackage::parse(&r.bytes).unwrap();
        let title = text_of(pkg.get("OEBPS/title.xhtml").unwrap());
        assert!(
            title.contains(r#"<html xmlns="http://www.w3.org/1999/xhtml" class="hltr">"#),
            "{title}"
        );
        assert!(title.contains(r#"<body class="p-titlepage">"#), "{title}");
        assert!(
            title.contains(r#"<div class="main"><div class="author">"#),
            "{title}"
        );
        assert!(title.contains("<p><span>A</span></p>"), "{title}");
        assert!(title.contains(r#"href="styles/book.css""#), "{title}");
        assert!(!title.contains("page_styles"), "{title}");
        let ch = text_of(pkg.get("OEBPS/ch1.xhtml").unwrap());
        assert!(ch.contains(r#"class="vrtl""#), "{ch}");
        assert!(ch.contains(r#"<body class="p-text">"#), "{ch}");
        assert!(
            ch.contains(r#"<p class="start-1em gfont bold">B</p>"#),
            "{ch}"
        );
        assert!(
            ch.contains(r#"<p>C<span class="tcy">12</span></p>"#),
            "{ch}"
        );
        assert!(pkg.get("OEBPS/styles/stylesheet.css").is_none());
        assert!(pkg.get("OEBPS/styles/page_styles.css").is_none());
        assert!(pkg.get("OEBPS/styles/book.css").is_some());
        assert!(pkg.get("OEBPS/styles/base.css").is_some());
        let opf = text_of(pkg.opf_bytes().unwrap());
        assert!(!opf.contains("stylesheet.css"), "{opf}");
        assert!(
            opf.contains("styles/book.css") && opf.contains("styles/base.css"),
            "{opf}"
        );
        assert!(r.residual.is_empty(), "{:?}", r.residual);
        assert_eq!(r.documents.len(), 3);
        let axis_only = r
            .diffs
            .iter()
            .all(|d| d.property == "writing-mode" && d.document == "OEBPS/title.xhtml");
        assert!(axis_only, "{:?}", r.diffs);
        assert!(!r.diffs.is_empty());
    }

    #[test]
    fn refuses_a_flattened_reference() {
        assert!(restore_styles(&flattened(), &flattened()).is_err());
        assert!(restore_styles(&reference(), &reference()).is_err());
    }
}
