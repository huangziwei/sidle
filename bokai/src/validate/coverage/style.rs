//! Style-coverage validation — count CSS declarations in the source and
//! report which property names bokai's parser silently drops.
//!
//! Every `(property_name, raw_value)` declaration in the source's linked
//! stylesheets, inline `<style>` blocks and element `style=` attributes goes
//! through `Declaration::parse(name, value)`. A supported property yields a
//! non-empty `Vec<Declaration>`; an unsupported one yields an empty `Vec`.
//! The report ranks the dropped property names by count.
//!
//! Scope: property names alone. Whether a parsed declaration reaches a KFX
//! style symbol belongs to the export-side pass, and a parsed property's
//! value is not checked.

use crate::formats::epub::structure::resolve_href;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use cssparser::{Parser, ParserInput};
use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::formats::epub::{parse_container_xml, parse_opf};
use crate::formats::kfx::container::{
    parse_container_header, parse_container_info, parse_index_table, slice_at,
};
use crate::formats::kfx::symbols::KfxSymbol;
use crate::style::Declaration;

/// Per-property statistics: how many times bokai parsed it vs dropped it.
#[derive(Debug, Default, Clone)]
pub struct PropertyStats {
    pub parsed: usize,
    pub dropped: usize,
    /// Up to a few example value strings that failed to parse, useful when
    /// the property *name* is supported but the *value* causes the drop.
    pub dropped_examples: Vec<String>,
}

impl PropertyStats {
    pub fn total(&self) -> usize {
        self.parsed + self.dropped
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// Total CSS declarations seen in the source.
    pub total: usize,
    /// Total declarations bokai's parser accepted.
    pub parsed: usize,
    /// Total declarations silently dropped by the parser.
    pub dropped: usize,
    /// Stats per property name, sorted by dropped-count descending.
    pub by_property: Vec<(String, PropertyStats)>,

    // --- EPUB class system richness ---
    /// Total `class="..."` attribute occurrences on spine elements (sums
    /// every occurrence — an element with `class="a b"` contributes 1).
    pub epub_class_attr_occurrences: usize,
    /// Distinct class-name tokens used across spine `class=` attrs.
    pub epub_distinct_class_names: usize,
    /// Number of class-selector rules in bundled stylesheets (i.e. selectors
    /// containing at least one `.name` component).
    pub epub_class_rule_count: usize,
    /// Number of `<p>` elements with non-empty visible text in spine docs.
    pub epub_leaf_p_text: usize,
    /// Number of `<div>` elements whose only children are inline runs of text:
    /// paragraph-shaped containers `consolidate_html` leaves as `<div>`.
    pub epub_leaf_div_text: usize,

    // --- KFX baseline ---
    /// Distinct `$style` ($157) entity count in KFX: the class universe the
    /// EPUB draws per-element classes from.
    pub kfx_distinct_style_count: usize,
}

impl Report {
    pub fn coverage_ratio(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        self.parsed as f64 / self.total as f64
    }

    pub fn is_clean(&self) -> bool {
        self.dropped == 0 && !self.classes_collapsed_to_zero() && !self.paragraphs_stuck_as_divs()
    }

    /// True when KFX carries styles and the EPUB has no class rules in any
    /// bundled stylesheet and no `class=` attributes on spine elements: every
    /// style choice in the source is dropped.
    pub fn classes_collapsed_to_zero(&self) -> bool {
        self.kfx_distinct_style_count > 0
            && self.epub_class_rule_count == 0
            && self.epub_class_attr_occurrences == 0
    }

    /// True when leaf-text containers are predominantly `<div>` and not `<p>`:
    /// at least 50 div-text containers against fewer than 10 `<p>`s with text.
    pub fn paragraphs_stuck_as_divs(&self) -> bool {
        self.epub_leaf_div_text >= 50 && self.epub_leaf_p_text < 10
    }

    pub fn print_summary(&self) {
        println!("CSS declarations:   {}", self.total);
        println!("Parsed:             {}", self.parsed);
        println!("Dropped:            {}", self.dropped);
        println!("Coverage:           {:.2}%", self.coverage_ratio() * 100.0);
        println!("Class system:");
        println!(
            "  KFX distinct styles ($style entities): {}",
            self.kfx_distinct_style_count
        );
        println!(
            "  EPUB class= occurrences:  {}",
            self.epub_class_attr_occurrences
        );
        println!(
            "  EPUB distinct class names: {}",
            self.epub_distinct_class_names
        );
        println!(
            "  EPUB stylesheet class rules: {}",
            self.epub_class_rule_count
        );
        println!("Leaf-text container shape:");
        println!(
            "  <p> with text:                    {}",
            self.epub_leaf_p_text
        );
        println!(
            "  <div> with text-only inline kids: {}",
            self.epub_leaf_div_text
        );
        if self.classes_collapsed_to_zero() {
            println!(
                "  DEFECT: KFX has {} style structs but EPUB has 0 class rules / 0 class= attrs",
                self.kfx_distinct_style_count
            );
        }
        if self.paragraphs_stuck_as_divs() {
            println!(
                "  DEFECT: leaf-text containers are predominantly <div> ({} divs vs {} p)",
                self.epub_leaf_div_text, self.epub_leaf_p_text
            );
        }
    }

    pub fn print_details(&self, limit: usize) {
        let dropped: Vec<&(String, PropertyStats)> = self
            .by_property
            .iter()
            .filter(|(_, s)| s.dropped > 0)
            .collect();
        if !dropped.is_empty() {
            println!(
                "\n--- Dropped properties [first {}] (sorted by frequency) ---",
                limit
            );
            for (name, stats) in dropped.iter().take(limit) {
                let examples = stats
                    .dropped_examples
                    .iter()
                    .take(3)
                    .map(|s| format!("\"{}\"", s))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("  {:>5}×  {:<32}  e.g. {}", stats.dropped, name, examples);
            }
            if dropped.len() > limit {
                println!("  ... and {} more unique properties", dropped.len() - limit);
            }
        }
    }
}

pub fn validate(epub_bytes: &[u8], kfx_bytes: &[u8]) -> Result<Report, String> {
    let declarations = collect_declarations(epub_bytes)?;

    let mut by_property: HashMap<String, PropertyStats> = HashMap::new();
    let mut total = 0;
    let mut parsed = 0;
    let mut dropped = 0;

    const MAX_EXAMPLES: usize = 5;
    for (name, value) in &declarations {
        total += 1;
        let stats = by_property.entry(name.clone()).or_default();
        let mut input = ParserInput::new(value);
        let mut parser = Parser::new(&mut input);
        let result = Declaration::parse(name, &mut parser);
        if result.is_empty() {
            dropped += 1;
            stats.dropped += 1;
            if stats.dropped_examples.len() < MAX_EXAMPLES
                && !stats.dropped_examples.iter().any(|e| e == value)
            {
                stats.dropped_examples.push(value.clone());
            }
        } else {
            parsed += 1;
            stats.parsed += 1;
        }
    }

    let mut by_property_vec: Vec<(String, PropertyStats)> = by_property.into_iter().collect();
    by_property_vec.sort_by(|a, b| b.1.dropped.cmp(&a.1.dropped).then(a.0.cmp(&b.0)));

    let richness = collect_class_richness(epub_bytes)?;
    let kfx_distinct_style_count = count_kfx_style_structs(kfx_bytes)?;

    Ok(Report {
        total,
        parsed,
        dropped,
        by_property: by_property_vec,
        epub_class_attr_occurrences: richness.class_attr_occurrences,
        epub_distinct_class_names: richness.distinct_class_names,
        epub_class_rule_count: richness.class_rule_count,
        epub_leaf_p_text: richness.leaf_p_text,
        epub_leaf_div_text: richness.leaf_div_text,
        kfx_distinct_style_count,
    })
}

#[derive(Debug, Default)]
struct ClassRichness {
    class_attr_occurrences: usize,
    distinct_class_names: usize,
    class_rule_count: usize,
    leaf_p_text: usize,
    leaf_div_text: usize,
}

/// EPUB-side: count class= attrs, distinct class names, class rules in
/// stylesheets, and `<p>` vs leaf `<div>` text containers across the spine.
fn collect_class_richness(epub_bytes: &[u8]) -> Result<ClassRichness, String> {
    let cursor = Cursor::new(epub_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|e| format!("not a valid zip: {}", e))?;

    let container_bytes = read_zip_entry(&mut archive, "META-INF/container.xml")
        .map_err(|e| format!("container.xml: {}", e))?;
    let opf_path = parse_container_xml(&container_bytes)
        .map_err(|e| format!("container.xml parse: {:?}", e))?;
    let opf_base = opf_path
        .rfind('/')
        .map(|i| &opf_path[..=i])
        .unwrap_or("")
        .to_string();
    let opf_bytes =
        read_zip_entry(&mut archive, &opf_path).map_err(|e| format!("opf {}: {}", opf_path, e))?;
    let enc = crate::util::extract_xml_encoding(&opf_bytes);
    let opf_str = crate::util::decode_text(&opf_bytes, enc);
    let opf = parse_opf(&opf_str).map_err(|e| format!("opf parse: {:?}", e))?;

    let mut richness = ClassRichness::default();
    let mut class_names: HashSet<String> = HashSet::new();

    // Stylesheet class rules: count selectors with at least one `.name`
    // component. Per-file, dedup is across the whole spine corpus.
    let mut seen_css: HashSet<String> = HashSet::new();
    for (href, media_type) in opf.manifest.values() {
        let is_css = media_type == "text/css" || href.to_lowercase().ends_with(".css");
        if !is_css {
            continue;
        }
        let full_path = resolve_href(&opf_base, href);
        if !seen_css.insert(full_path.clone()) {
            continue;
        }
        if let Ok(css_bytes) = read_zip_entry(&mut archive, &full_path) {
            let enc = crate::util::extract_xml_encoding(&css_bytes);
            let css = crate::util::decode_text(&css_bytes, enc);
            richness.class_rule_count += count_class_selectors(&css);
        }
    }

    // Per-spine-doc: walk every element, count class= attrs and
    // leaf-text container shape.
    for spine_id in &opf.spine_ids {
        let Some((href, _media_type)) = opf.manifest.get(spine_id) else {
            continue;
        };
        let full_path = resolve_href(&opf_base, href);
        let Ok(xhtml_bytes) = read_zip_entry(&mut archive, &full_path) else {
            continue;
        };
        let enc = crate::util::extract_xml_encoding(&xhtml_bytes);
        let xhtml = crate::util::decode_text(&xhtml_bytes, enc);
        scan_xhtml_richness(&xhtml, &mut richness, &mut class_names);
    }

    richness.distinct_class_names = class_names.len();
    Ok(richness)
}

/// Count of selectors in `css` carrying at least one `.name` component. The
/// scan splits on `{` for the selector list, splits each selector on `,`, and
/// checks for a `.<ident>` prefix.
fn count_class_selectors(css: &str) -> usize {
    let mut count = 0;
    for (i, segment) in css.split('{').enumerate() {
        if i == 0 {
            // First segment is the first selector list. Subsequent splits
            // are <body>}<selector list>; the selector portion is the part
            // after the last `}`.
            count += class_selector_count_in_list(segment);
        } else if let Some(end) = segment.rfind('}') {
            let after = &segment[end + 1..];
            count += class_selector_count_in_list(after);
        }
    }
    count
}

fn class_selector_count_in_list(selectors: &str) -> usize {
    selectors
        .split(',')
        .filter(|s| {
            let s = s.trim();
            // Has a `.<ident>` somewhere in the selector.
            let bytes = s.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'.'
                    && i + 1 < bytes.len()
                    && (bytes[i + 1].is_ascii_alphabetic()
                        || bytes[i + 1] == b'_'
                        || bytes[i + 1] == b'-')
                {
                    return true;
                }
                i += 1;
            }
            false
        })
        .count()
}

/// Walk an XHTML into `richness`: `class=` attributes with their distinct
/// names, `<p>` with text, and `<div>` whose children are all inline. A leaf
/// `<div>` holding only inline runs is paragraph-shaped.
fn scan_xhtml_richness(
    xhtml: &str,
    richness: &mut ClassRichness,
    class_names: &mut HashSet<String>,
) {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    // Pass 1: count class attrs + collect names (cheap, single pass).
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                for attr in e.attributes().flatten() {
                    if attr.key.local_name().as_ref() == b"class" {
                        richness.class_attr_occurrences += 1;
                        let v = String::from_utf8_lossy(&attr.value);
                        for tok in v.split_whitespace() {
                            class_names.insert(tok.to_string());
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }

    // Pass 2: leaf-text container shape. A `Frame` per Start event holds the
    // tag name and whether a block child appeared by its End, separating a
    // div with inline kids and text from a div with block kids.
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);
    struct Frame {
        tag: String,
        has_block_child: bool,
        has_text: bool,
    }
    let block_tags: &[&[u8]] = &[
        b"div",
        b"p",
        b"section",
        b"article",
        b"aside",
        b"header",
        b"footer",
        b"nav",
        b"main",
        b"h1",
        b"h2",
        b"h3",
        b"h4",
        b"h5",
        b"h6",
        b"ul",
        b"ol",
        b"li",
        b"dl",
        b"dt",
        b"dd",
        b"table",
        b"thead",
        b"tbody",
        b"tfoot",
        b"tr",
        b"td",
        b"th",
        b"figure",
        b"figcaption",
        b"blockquote",
        b"hr",
        b"pre",
    ];
    let mut stack: Vec<Frame> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let tag = std::str::from_utf8(e.local_name().as_ref())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let is_block = block_tags.iter().any(|t| t == &tag.as_bytes());
                if is_block && let Some(parent) = stack.last_mut() {
                    parent.has_block_child = true;
                }
                stack.push(Frame {
                    tag,
                    has_block_child: false,
                    has_text: false,
                });
            }
            Ok(Event::Empty(e)) => {
                let tag = std::str::from_utf8(e.local_name().as_ref())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let is_block = block_tags.iter().any(|t| t == &tag.as_bytes());
                if is_block && let Some(parent) = stack.last_mut() {
                    parent.has_block_child = true;
                }
            }
            Ok(Event::Text(t)) => {
                let s = String::from_utf8_lossy(t.as_ref());
                if s.chars().any(|c| !c.is_whitespace())
                    && let Some(top) = stack.last_mut()
                {
                    top.has_text = true;
                }
            }
            Ok(Event::End(_)) => {
                if let Some(frame) = stack.pop() {
                    if frame.has_text {
                        if frame.tag == "p" {
                            richness.leaf_p_text += 1;
                        } else if frame.tag == "div" && !frame.has_block_child {
                            richness.leaf_div_text += 1;
                        }
                    }
                    // `has_text` rises to the parent, which registers a
                    // `<div>` whose only kids are inline runs — the text of
                    // `<div><span>x</span></div>` — as leaf text.
                    if frame.has_text
                        && let Some(parent) = stack.last_mut()
                    {
                        parent.has_text = true;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

/// KFX-side: number of distinct `$style` ($157) entities in the container.
/// Calibre emits one `class_sN` rule per style struct.
fn count_kfx_style_structs(kfx_bytes: &[u8]) -> Result<usize, String> {
    let header = parse_container_header(kfx_bytes).map_err(|e| format!("kfx header: {:?}", e))?;
    let info_data = slice_at(
        kfx_bytes,
        header.container_info_offset,
        header.container_info_length,
    )
    .ok_or("container info out of bounds")?;
    let info =
        parse_container_info(info_data).map_err(|e| format!("kfx container info: {:?}", e))?;
    let Some((idx_off, idx_len)) = info.index else {
        return Ok(0);
    };
    let Some(index_data) = slice_at(kfx_bytes, idx_off, idx_len) else {
        return Ok(0);
    };
    let entities = parse_index_table(index_data, header.header_len);
    let style_type = KfxSymbol::Style as u32;
    Ok(entities
        .iter()
        .filter(|ent| ent.type_id == style_type)
        .count())
}

// ============================================================================
// CSS collection
// ============================================================================

/// Every `(property, value)` declaration in the EPUB: manifest `.css` files,
/// inline `<style>` blocks in spine XHTML, and element `style=` attributes.
fn collect_declarations(epub_bytes: &[u8]) -> Result<Vec<(String, String)>, String> {
    let cursor = Cursor::new(epub_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|e| format!("not a valid zip: {}", e))?;

    let container_bytes = read_zip_entry(&mut archive, "META-INF/container.xml")
        .map_err(|e| format!("container.xml: {}", e))?;
    let opf_path = parse_container_xml(&container_bytes)
        .map_err(|e| format!("container.xml parse: {:?}", e))?;
    let opf_base = opf_path
        .rfind('/')
        .map(|i| &opf_path[..=i])
        .unwrap_or("")
        .to_string();

    let opf_bytes =
        read_zip_entry(&mut archive, &opf_path).map_err(|e| format!("opf {}: {}", opf_path, e))?;
    let hint_encoding = crate::util::extract_xml_encoding(&opf_bytes);
    let opf_str = crate::util::decode_text(&opf_bytes, hint_encoding);
    let opf = parse_opf(&opf_str).map_err(|e| format!("opf parse: {:?}", e))?;

    let mut decls: Vec<(String, String)> = Vec::new();

    // 1. Linked stylesheets: manifest items with the text/css media type, plus
    //    any href ending in `.css`. `seen_css` parses each file once, however
    //    many chapters reference it.
    let mut seen_css: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (href, media_type) in opf.manifest.values() {
        let is_css = media_type == "text/css" || href.to_lowercase().ends_with(".css");
        if !is_css {
            continue;
        }
        let full_path = resolve_href(&opf_base, href);
        if !seen_css.insert(full_path.clone()) {
            continue;
        }
        let Ok(css_bytes) = read_zip_entry(&mut archive, &full_path) else {
            continue;
        };
        let enc = crate::util::extract_xml_encoding(&css_bytes);
        let css = crate::util::decode_text(&css_bytes, enc);
        parse_css_blob(&css, &mut decls);
    }

    // 2. Spine XHTML: inline <style> blocks + style="..." attributes.
    for spine_id in &opf.spine_ids {
        let Some((href, _media_type)) = opf.manifest.get(spine_id) else {
            continue;
        };
        let full_path = resolve_href(&opf_base, href);
        let Ok(xhtml_bytes) = read_zip_entry(&mut archive, &full_path) else {
            continue;
        };
        let enc = crate::util::extract_xml_encoding(&xhtml_bytes);
        let xhtml = crate::util::decode_text(&xhtml_bytes, enc);
        scan_xhtml_for_inline_styles(&xhtml, &mut decls);
    }

    Ok(decls)
}

fn read_zip_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    let mut file = archive.by_name(name)?;
    let mut buf = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Parse a CSS blob (whole stylesheet or `<style>` body) and append every
/// declaration found to `out`. Every rule body contributes its declarations,
/// whatever its selector matches: the measure is property coverage.
fn parse_css_blob(css: &str, out: &mut Vec<(String, String)>) {
    use cssparser::{
        AtRuleParser, CowRcStr, DeclarationParser, ParseError, QualifiedRuleParser,
        RuleBodyItemParser, RuleBodyParser, StyleSheetParser,
    };

    // Simple visitor: walk every rule body, collect each declaration's
    // (name, value-as-string) pair.
    struct Collect<'a> {
        out: &'a mut Vec<(String, String)>,
    }

    type Err<'i> = ParseError<'i, ()>;

    impl<'i> DeclarationParser<'i> for Collect<'_> {
        type Declaration = ();
        type Error = ();
        fn parse_value<'t>(
            &mut self,
            name: CowRcStr<'i>,
            input: &mut Parser<'i, 't>,
            _start: &cssparser::ParserState,
        ) -> Result<Self::Declaration, Err<'i>> {
            let start = input.position();
            while input.next().is_ok() {}
            let value = input.slice_from(start).trim().to_string();
            self.out.push((name.as_ref().to_string(), value));
            Ok(())
        }
    }

    impl<'i> AtRuleParser<'i> for Collect<'_> {
        type Prelude = ();
        type AtRule = ();
        type Error = ();
    }

    impl<'i> QualifiedRuleParser<'i> for Collect<'_> {
        type Prelude = ();
        type QualifiedRule = ();
        type Error = ();
        fn parse_prelude<'t>(
            &mut self,
            input: &mut Parser<'i, 't>,
        ) -> Result<Self::Prelude, Err<'i>> {
            while input.next().is_ok() {}
            Ok(())
        }
        fn parse_block<'t>(
            &mut self,
            _prelude: Self::Prelude,
            _start: &cssparser::ParserState,
            input: &mut Parser<'i, 't>,
        ) -> Result<Self::QualifiedRule, Err<'i>> {
            let body = RuleBodyParser::new(input, self);
            for result in body {
                let _ = result;
            }
            Ok(())
        }
    }

    impl<'i> RuleBodyItemParser<'i, (), ()> for Collect<'_> {
        fn parse_declarations(&self) -> bool {
            true
        }
        fn parse_qualified(&self) -> bool {
            false
        }
    }

    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    let mut visitor = Collect { out };
    let rules = StyleSheetParser::new(&mut parser, &mut visitor);
    for result in rules {
        let _ = result;
    }
}

/// Scan an XHTML file for inline `<style>` blocks and `style=` attributes.
fn scan_xhtml_for_inline_styles(xhtml: &str, out: &mut Vec<(String, String)>) {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);

    let mut in_style: bool = false;
    let mut style_buf = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if e.local_name().as_ref() == b"style" {
                    in_style = true;
                    style_buf.clear();
                }
                // style="..." attribute on any element
                for attr in e.attributes().flatten() {
                    if attr.key.local_name().as_ref() == b"style" {
                        let value = String::from_utf8_lossy(&attr.value).into_owned();
                        parse_inline_style_attr(&value, out);
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                for attr in e.attributes().flatten() {
                    if attr.key.local_name().as_ref() == b"style" {
                        let value = String::from_utf8_lossy(&attr.value).into_owned();
                        parse_inline_style_attr(&value, out);
                    }
                }
            }
            Ok(Event::End(e)) => {
                if e.local_name().as_ref() == b"style" && in_style {
                    parse_css_blob(&style_buf, out);
                    style_buf.clear();
                    in_style = false;
                }
            }
            Ok(Event::Text(e)) if in_style => {
                style_buf.push_str(&String::from_utf8_lossy(e.as_ref()));
            }
            Ok(Event::CData(e)) if in_style => {
                style_buf.push_str(&String::from_utf8_lossy(&e));
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

/// Parse a `style="prop: val; prop: val"` attribute body.
fn parse_inline_style_attr(body: &str, out: &mut Vec<(String, String)>) {
    for decl in body.split(';') {
        let Some((name, value)) = decl.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if !name.is_empty() && !value.is_empty() {
            out.push((name.to_string(), value.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_style_attr_parses() {
        let mut out = Vec::new();
        parse_inline_style_attr("color: red; font-size: 12pt", &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "color");
        assert_eq!(out[0].1, "red");
        assert_eq!(out[1].0, "font-size");
        assert_eq!(out[1].1, "12pt");
    }

    #[test]
    fn css_blob_collects_declarations() {
        let mut out = Vec::new();
        parse_css_blob(
            "p { color: red; font-weight: bold } h1 { color: blue }",
            &mut out,
        );
        // Order of properties within a rule preserved; rules in document order.
        assert!(out.iter().any(|(n, v)| n == "color" && v == "red"));
        assert!(out.iter().any(|(n, v)| n == "font-weight" && v == "bold"));
        assert!(out.iter().any(|(n, v)| n == "color" && v == "blue"));
    }
}
