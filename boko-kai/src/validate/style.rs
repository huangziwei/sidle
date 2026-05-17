//! Style-coverage validation — count CSS declarations in the source and
//! report which property names boko-kai's parser silently drops.
//!
//! For each `(property_name, raw_value)` declaration found in the source's
//! linked stylesheets, inline `<style>` blocks, and element `style=` attrs,
//! we call `Declaration::parse(name, value)`. boko's parser returns a non-
//! empty `Vec<Declaration>` for supported properties and an empty `Vec` for
//! unsupported ones (silently swallowing the property). The report tells
//! the user, per book, which CSS properties they need to make boko-kai
//! handle next — the biggest deficits matter most.
//!
//! Limitations: we do **not** verify that a parsed declaration produced a
//! matching KFX style symbol on the export side — that's a separate
//! validation pass. We also don't validate value correctness within a
//! parsed property (e.g. an unsupported `font-size` keyword would parse
//! to nothing without us noticing).

use std::collections::HashMap;
use std::io::Cursor;

use cssparser::{Parser, ParserInput};
use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::epub::{parse_container_xml, parse_opf};
use crate::style::Declaration;

/// Per-property statistics: how many times boko parsed it vs dropped it.
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
    /// Total declarations boko's parser accepted.
    pub parsed: usize,
    /// Total declarations silently dropped by the parser.
    pub dropped: usize,
    /// Stats per property name, sorted by dropped-count descending.
    pub by_property: Vec<(String, PropertyStats)>,
}

impl Report {
    pub fn coverage_ratio(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        self.parsed as f64 / self.total as f64
    }

    pub fn is_clean(&self) -> bool {
        self.dropped == 0
    }

    pub fn print_summary(&self) {
        println!("CSS declarations:   {}", self.total);
        println!("Parsed:             {}", self.parsed);
        println!("Dropped:            {}", self.dropped);
        println!("Coverage:           {:.2}%", self.coverage_ratio() * 100.0);
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

pub fn validate(epub_bytes: &[u8]) -> Result<Report, String> {
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

    Ok(Report {
        total,
        parsed,
        dropped,
        by_property: by_property_vec,
    })
}

// ============================================================================
// CSS collection
// ============================================================================

/// Collect every `(property, value)` declaration from the EPUB:
/// - linked .css files in the manifest,
/// - inline `<style>` blocks in spine XHTML,
/// - element `style=` attributes in spine XHTML.
fn collect_declarations(epub_bytes: &[u8]) -> Result<Vec<(String, String)>, String> {
    let cursor = Cursor::new(epub_bytes);
    let mut archive =
        ZipArchive::new(cursor).map_err(|e| format!("not a valid zip: {}", e))?;

    let container_bytes = read_zip_entry(&mut archive, "META-INF/container.xml")
        .map_err(|e| format!("container.xml: {}", e))?;
    let opf_path = parse_container_xml(&container_bytes)
        .map_err(|e| format!("container.xml parse: {:?}", e))?;
    let opf_base = opf_path
        .rfind('/')
        .map(|i| &opf_path[..=i])
        .unwrap_or("")
        .to_string();

    let opf_bytes = read_zip_entry(&mut archive, &opf_path)
        .map_err(|e| format!("opf {}: {}", opf_path, e))?;
    let hint_encoding = crate::util::extract_xml_encoding(&opf_bytes);
    let opf_str = crate::util::decode_text(&opf_bytes, hint_encoding);
    let opf = parse_opf(&opf_str).map_err(|e| format!("opf parse: {:?}", e))?;

    let mut decls: Vec<(String, String)> = Vec::new();

    // 1. Linked stylesheets: walk manifest for items with css media-type
    //    (text/css), plus any href ending in .css. We don't dedup — a sheet
    //    referenced by N chapters still counts its declarations once because
    //    we only parse each file once.
    let mut seen_css: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (href, media_type) in opf.manifest.values() {
        let is_css = media_type == "text/css" || href.to_lowercase().ends_with(".css");
        if !is_css {
            continue;
        }
        let full_path = format!("{}{}", opf_base, href);
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
        let full_path = format!("{}{}", opf_base, href);
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
/// declaration found to `out`. We deliberately keep the parser tolerant:
/// each rule's declaration block contributes whether or not the selector
/// would match anything, since the *property* coverage is what we measure.
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
            let mut body = RuleBodyParser::new(input, self);
            while let Some(result) = body.next() {
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
    let mut rules = StyleSheetParser::new(&mut parser, &mut visitor);
    while let Some(result) = rules.next() {
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
        parse_css_blob("p { color: red; font-weight: bold } h1 { color: blue }", &mut out);
        // Order of properties within a rule preserved; rules in document order.
        assert!(out.iter().any(|(n, v)| n == "color" && v == "red"));
        assert!(out.iter().any(|(n, v)| n == "font-weight" && v == "bold"));
        assert!(out.iter().any(|(n, v)| n == "color" && v == "blue"));
    }
}
