//! Ruby validation — extract `(base, annotation)` pairs independently from
//! the source EPUB and from the converted KFX, then compare as multisets.
//!
//! The EPUB extractor uses quick-xml directly on each spine XHTML, with a
//! small state machine that walks `<ruby>`/`<rb>`/`<rt>`/`<rp>` events.
//! It deliberately does **not** go through boko's DOM-to-IR transform —
//! the goal is to catch bugs in that pipeline, not silently mirror them.
//!
//! The KFX extractor walks every storyline's `style_events` looking for
//! entries that carry `ruby_name` + `ruby_id`, then slices the base text
//! out of the referenced content fragment and looks the annotation up in
//! the matching `ruby_content` fragment's `content_list`.
//!
//! `validate(...)` returns a `Report` with both sides plus the
//! per-pair multiset diff (`missing` = in EPUB only, `extra` = in KFX only).

use std::collections::HashMap;
use std::io::Cursor;

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::epub::{parse_container_xml, parse_opf};
use crate::kfx::container::{
    extract_doc_symbols, parse_container_header, parse_container_info, parse_index_table,
    skip_enty_header,
};
use crate::kfx::ion::{IonParser, IonValue};
use crate::kfx::symbols::{KFX_SYMBOL_TABLE, KfxSymbol};

/// A single ruby pair: base text plus its annotation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RubyPair {
    pub base: String,
    pub annotation: String,
}

impl RubyPair {
    fn from_trimmed(base: &str, annotation: &str) -> Self {
        Self {
            base: base.trim().to_string(),
            annotation: annotation.trim().to_string(),
        }
    }
}

/// Result of comparing EPUB-side and KFX-side pair sets.
#[derive(Debug, Default)]
pub struct Report {
    pub epub_pairs: Vec<RubyPair>,
    pub kfx_pairs: Vec<RubyPair>,
    /// Pairs present in EPUB but not in KFX (boko dropped them or paired wrong).
    pub missing: Vec<(RubyPair, usize)>,
    /// Pairs present in KFX but not in EPUB (boko fabricated or mispaired).
    pub extra: Vec<(RubyPair, usize)>,
    /// Pairs present in both, with min(epub_count, kfx_count) per unique pair.
    pub matched: usize,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty()
    }

    pub fn print_summary(&self) {
        println!("EPUB pairs:    {}", self.epub_pairs.len());
        println!("KFX pairs:     {}", self.kfx_pairs.len());
        println!("Matched:       {}", self.matched);
        let missing_total: usize = self.missing.iter().map(|(_, n)| n).sum();
        let extra_total: usize = self.extra.iter().map(|(_, n)| n).sum();
        println!(
            "Missing in KFX: {} ({} unique)",
            missing_total,
            self.missing.len()
        );
        println!(
            "Extra in KFX:   {} ({} unique)",
            extra_total,
            self.extra.len()
        );
    }

    pub fn print_details(&self, limit: usize) {
        if !self.missing.is_empty() {
            println!("\n--- Missing (in EPUB, not in KFX) [first {}] ---", limit);
            for (pair, n) in self.missing.iter().take(limit) {
                println!("  ({}×)  {}  →  {}", n, pair.base, pair.annotation);
            }
            if self.missing.len() > limit {
                println!("  ... and {} more unique pairs", self.missing.len() - limit);
            }
        }
        if !self.extra.is_empty() {
            println!("\n--- Extra (in KFX, not in EPUB) [first {}] ---", limit);
            for (pair, n) in self.extra.iter().take(limit) {
                println!("  ({}×)  {}  →  {}", n, pair.base, pair.annotation);
            }
            if self.extra.len() > limit {
                println!("  ... and {} more unique pairs", self.extra.len() - limit);
            }
        }
    }
}

/// Validate that the KFX preserves every ruby pair from the source EPUB.
pub fn validate(epub_bytes: &[u8], kfx_bytes: &[u8]) -> Result<Report, String> {
    let epub_pairs = extract_pairs_from_epub(epub_bytes)?;
    let kfx_pairs = extract_pairs_from_kfx(kfx_bytes)?;

    // Multiset diff: count occurrences of each pair on both sides.
    let mut epub_counts: HashMap<RubyPair, usize> = HashMap::new();
    for p in &epub_pairs {
        *epub_counts.entry(p.clone()).or_insert(0) += 1;
    }
    let mut kfx_counts: HashMap<RubyPair, usize> = HashMap::new();
    for p in &kfx_pairs {
        *kfx_counts.entry(p.clone()).or_insert(0) += 1;
    }

    let mut missing: Vec<(RubyPair, usize)> = Vec::new();
    let mut extra: Vec<(RubyPair, usize)> = Vec::new();
    let mut matched: usize = 0;

    for (pair, ecount) in &epub_counts {
        let kcount = kfx_counts.get(pair).copied().unwrap_or(0);
        matched += ecount.min(&kcount).to_owned();
        if ecount > &kcount {
            missing.push((pair.clone(), ecount - kcount));
        }
    }
    for (pair, kcount) in &kfx_counts {
        let ecount = epub_counts.get(pair).copied().unwrap_or(0);
        if kcount > &ecount {
            extra.push((pair.clone(), kcount - ecount));
        }
    }

    // Sort by frequency descending so the worst offenders surface first.
    missing.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.base.cmp(&b.0.base)));
    extra.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.base.cmp(&b.0.base)));

    Ok(Report {
        epub_pairs,
        kfx_pairs,
        missing,
        extra,
        matched,
    })
}

// ============================================================================
// EPUB-side extraction
// ============================================================================

/// Extract ruby pairs from a source EPUB. Spine order is preserved.
pub fn extract_pairs_from_epub(epub_bytes: &[u8]) -> Result<Vec<RubyPair>, String> {
    let cursor = Cursor::new(epub_bytes);
    let mut archive =
        ZipArchive::new(cursor).map_err(|e| format!("not a valid zip: {}", e))?;

    // Read container.xml to find OPF path.
    let container_bytes = read_zip_entry(&mut archive, "META-INF/container.xml")
        .map_err(|e| format!("container.xml: {}", e))?;
    let opf_path = parse_container_xml(&container_bytes)
        .map_err(|e| format!("container.xml parse: {:?}", e))?;
    let opf_base = opf_path
        .rfind('/')
        .map(|i| &opf_path[..=i])
        .unwrap_or("")
        .to_string();

    // Parse OPF to get spine order.
    let opf_bytes = read_zip_entry(&mut archive, &opf_path)
        .map_err(|e| format!("opf {}: {}", opf_path, e))?;
    let hint_encoding = crate::util::extract_xml_encoding(&opf_bytes);
    let opf_str = crate::util::decode_text(&opf_bytes, hint_encoding);
    let opf = parse_opf(&opf_str).map_err(|e| format!("opf parse: {:?}", e))?;

    let mut pairs = Vec::new();
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
        extract_pairs_from_xhtml(&xhtml, &mut pairs);
    }
    Ok(pairs)
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

/// State for the ruby walker: which element we're currently inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RubyState {
    /// Not inside any ruby-related element.
    Outside,
    /// Inside `<ruby>` but not yet inside an rb/rt/rp child — text here
    /// becomes implicit base (e.g. `<ruby>漢<rt>かん</rt></ruby>`).
    InRuby,
    /// Inside `<rb>` — text accumulates as base for the current pair.
    InRb,
    /// Inside `<rt>` — text accumulates as the annotation.
    InRt,
    /// Inside `<rp>` — fallback parens, skipped entirely.
    InRp,
}

/// Walk a single XHTML document and append all ruby pairs to `out`.
pub fn extract_pairs_from_xhtml(xhtml: &str, out: &mut Vec<RubyPair>) {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);

    let mut state = RubyState::Outside;
    let mut pending_base = String::new();
    let mut pending_annotation = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"ruby" => {
                    state = RubyState::InRuby;
                    pending_base.clear();
                    pending_annotation.clear();
                }
                b"rb" if state != RubyState::Outside => {
                    state = RubyState::InRb;
                }
                b"rt" if state != RubyState::Outside => {
                    state = RubyState::InRt;
                }
                b"rp" if state != RubyState::Outside => {
                    state = RubyState::InRp;
                }
                _ => {}
            },
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"ruby" => {
                    // Drop any orphan base without a paired rt.
                    state = RubyState::Outside;
                    pending_base.clear();
                    pending_annotation.clear();
                }
                b"rb" => {
                    state = RubyState::InRuby;
                }
                b"rt" => {
                    if !pending_base.is_empty() {
                        out.push(RubyPair::from_trimmed(&pending_base, &pending_annotation));
                    }
                    pending_base.clear();
                    pending_annotation.clear();
                    state = RubyState::InRuby;
                }
                b"rp" => {
                    state = RubyState::InRuby;
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                b"rt" => {
                    // Self-closing rt — emit pair with empty annotation if base exists.
                    if !pending_base.is_empty() {
                        out.push(RubyPair::from_trimmed(&pending_base, ""));
                    }
                    pending_base.clear();
                    pending_annotation.clear();
                }
                _ => {}
            },
            Ok(Event::Text(e)) => {
                if state == RubyState::Outside || state == RubyState::InRp {
                    continue;
                }
                let txt = String::from_utf8_lossy(e.as_ref()).into_owned();
                match state {
                    RubyState::InRuby | RubyState::InRb => pending_base.push_str(&txt),
                    RubyState::InRt => pending_annotation.push_str(&txt),
                    _ => {}
                }
            }
            Ok(Event::CData(e)) => {
                if state == RubyState::Outside || state == RubyState::InRp {
                    continue;
                }
                let txt = String::from_utf8_lossy(&e).into_owned();
                match state {
                    RubyState::InRuby | RubyState::InRb => pending_base.push_str(&txt),
                    RubyState::InRt => pending_annotation.push_str(&txt),
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

// ============================================================================
// KFX-side extraction
// ============================================================================

/// Extract ruby pairs from a converted KFX file. Storyline order is preserved.
pub fn extract_pairs_from_kfx(kfx_bytes: &[u8]) -> Result<Vec<RubyPair>, String> {
    let header =
        parse_container_header(kfx_bytes).map_err(|e| format!("kfx header: {:?}", e))?;
    if header.container_info_offset + header.container_info_length > kfx_bytes.len() {
        return Err("container info out of bounds".into());
    }
    let info_data = &kfx_bytes[header.container_info_offset
        ..header.container_info_offset + header.container_info_length];
    let info = parse_container_info(info_data)
        .map_err(|e| format!("kfx container info: {:?}", e))?;

    let extended_symbols = match info.doc_symbols {
        Some((off, len)) if off + len <= kfx_bytes.len() => {
            extract_doc_symbols(&kfx_bytes[off..off + len])
        }
        _ => Vec::new(),
    };
    let base_symbol_count = KFX_SYMBOL_TABLE.len() as u64;

    let resolve_sym = |id: u64| -> String {
        if id < base_symbol_count {
            KFX_SYMBOL_TABLE
                .get(id as usize)
                .copied()
                .unwrap_or("?")
                .to_string()
        } else {
            let idx = (id - base_symbol_count) as usize;
            extended_symbols
                .get(idx)
                .cloned()
                .unwrap_or_else(|| "?".to_string())
        }
    };

    let Some((idx_off, idx_len)) = info.index else {
        return Err("kfx: no index table".into());
    };
    let entities =
        parse_index_table(&kfx_bytes[idx_off..idx_off + idx_len], header.header_len);

    // Pass 1: build content_map (name → Vec<String>) and ruby_lookup
    // (ruby_name → Vec<annotation> indexed by ruby_id-1).
    let mut content_map: HashMap<String, Vec<String>> = HashMap::new();
    let mut ruby_lookup: HashMap<String, Vec<String>> = HashMap::new();
    let mut storyline_locs: Vec<&crate::kfx::container::EntityLoc> = Vec::new();

    let content_type = KfxSymbol::Content as u32;
    let ruby_content_type = KfxSymbol::RubyContent as u32;
    let storyline_type = KfxSymbol::Storyline as u32;

    for ent in &entities {
        if ent.type_id == storyline_type {
            storyline_locs.push(ent);
            continue;
        }
        if ent.type_id != content_type && ent.type_id != ruby_content_type {
            continue;
        }
        let Some(value) = parse_entity(kfx_bytes, ent) else {
            continue;
        };

        if ent.type_id == content_type {
            if let Some((name, texts)) = extract_content_texts(&value, &resolve_sym) {
                content_map.insert(name, texts);
            }
        } else if ent.type_id == ruby_content_type {
            if let Some((ruby_name, annotations)) = extract_ruby_content(&value, &resolve_sym) {
                ruby_lookup.insert(ruby_name, annotations);
            }
        }
    }

    // Pass 2: walk every storyline, collect pairs in document order.
    let mut pairs = Vec::new();
    for ent in &storyline_locs {
        let Some(value) = parse_entity(kfx_bytes, ent) else {
            continue;
        };
        collect_pairs_from_ion(&value, &content_map, &ruby_lookup, &resolve_sym, &mut pairs);
    }

    Ok(pairs)
}

fn parse_entity(data: &[u8], ent: &crate::kfx::container::EntityLoc) -> Option<IonValue> {
    if ent.offset + ent.length > data.len() {
        return None;
    }
    let entity = &data[ent.offset..ent.offset + ent.length];
    let ion = skip_enty_header(entity);
    IonParser::new(ion).parse().ok()
}

fn extract_content_texts<F>(value: &IonValue, resolve_sym: &F) -> Option<(String, Vec<String>)>
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, inner) => inner.as_ref(),
        _ => value,
    };
    let IonValue::Struct(fields) = inner else {
        return None;
    };
    let mut name = String::new();
    let mut texts: Vec<String> = Vec::new();
    for (k, v) in fields {
        match resolve_sym(*k).as_str() {
            "name" => {
                if let IonValue::Symbol(s) = v {
                    name = resolve_sym(*s);
                } else if let IonValue::String(s) = v {
                    name = s.clone();
                }
            }
            "content_list" => {
                if let IonValue::List(items) = v {
                    for item in items {
                        if let IonValue::String(s) = item {
                            texts.push(s.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if name.is_empty() {
        None
    } else {
        Some((name, texts))
    }
}

fn extract_ruby_content<F>(value: &IonValue, resolve_sym: &F) -> Option<(String, Vec<String>)>
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, inner) => inner.as_ref(),
        _ => value,
    };
    let IonValue::Struct(fields) = inner else {
        return None;
    };

    let mut ruby_name = String::new();
    let mut annotations: Vec<String> = Vec::new();
    for (k, v) in fields {
        match resolve_sym(*k).as_str() {
            "ruby_name" => {
                if let IonValue::Symbol(s) = v {
                    ruby_name = resolve_sym(*s);
                }
            }
            "content_list" => {
                if let IonValue::List(items) = v {
                    for item in items {
                        let IonValue::Struct(item_fields) = item else {
                            continue;
                        };
                        let mut content = String::new();
                        let mut ruby_id: i64 = 0;
                        for (ik, iv) in item_fields {
                            match resolve_sym(*ik).as_str() {
                                "content" => {
                                    if let IonValue::String(s) = iv {
                                        content = s.clone();
                                    }
                                }
                                "ruby_id" => {
                                    if let IonValue::Int(n) = iv {
                                        ruby_id = *n;
                                    }
                                }
                                _ => {}
                            }
                        }
                        if ruby_id > 0 {
                            let idx = (ruby_id - 1) as usize;
                            while annotations.len() <= idx {
                                annotations.push(String::new());
                            }
                            annotations[idx] = content;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if ruby_name.is_empty() {
        None
    } else {
        Some((ruby_name, annotations))
    }
}

fn collect_pairs_from_ion<F>(
    value: &IonValue,
    content_map: &HashMap<String, Vec<String>>,
    ruby_lookup: &HashMap<String, Vec<String>>,
    resolve_sym: &F,
    pairs: &mut Vec<RubyPair>,
) where
    F: Fn(u64) -> String,
{
    match value {
        IonValue::Struct(fields) => {
            // If this struct is a text element with style_events + content,
            // pull base text out and emit one pair per ruby style_event.
            let mut content_name = String::new();
            let mut content_index: i64 = -1;
            let mut style_events: Option<&Vec<IonValue>> = None;
            for (k, v) in fields {
                match resolve_sym(*k).as_str() {
                    "content" => {
                        if let IonValue::Struct(cfields) = v {
                            for (ck, cv) in cfields {
                                match resolve_sym(*ck).as_str() {
                                    "name" => {
                                        if let IonValue::Symbol(s) = cv {
                                            content_name = resolve_sym(*s);
                                        }
                                    }
                                    "index" => {
                                        if let IonValue::Int(n) = cv {
                                            content_index = *n;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    "style_events" => {
                        if let IonValue::List(items) = v {
                            style_events = Some(items);
                        }
                    }
                    _ => {}
                }
            }

            if let (Some(events), false) = (style_events, content_name.is_empty())
                && content_index >= 0
                && let Some(text_vec) = content_map.get(&content_name)
                && let Some(text) = text_vec.get(content_index as usize)
            {
                let chars: Vec<char> = text.chars().collect();
                for evt in events {
                    let IonValue::Struct(efields) = evt else {
                        continue;
                    };
                    let mut offset: i64 = -1;
                    let mut length: i64 = -1;
                    let mut ruby_name = String::new();
                    let mut ruby_id: i64 = 0;
                    for (k, v) in efields {
                        match resolve_sym(*k).as_str() {
                            "offset" => {
                                if let IonValue::Int(n) = v {
                                    offset = *n;
                                }
                            }
                            "length" => {
                                if let IonValue::Int(n) = v {
                                    length = *n;
                                }
                            }
                            "ruby_name" => {
                                if let IonValue::Symbol(s) = v {
                                    ruby_name = resolve_sym(*s);
                                }
                            }
                            "ruby_id" => {
                                if let IonValue::Int(n) = v {
                                    ruby_id = *n;
                                }
                            }
                            _ => {}
                        }
                    }
                    if offset >= 0 && length > 0 && !ruby_name.is_empty() && ruby_id > 0 {
                        let start = offset as usize;
                        let end = (start + length as usize).min(chars.len());
                        if start < end {
                            let base: String = chars[start..end].iter().collect();
                            let annotation = ruby_lookup
                                .get(&ruby_name)
                                .and_then(|v| v.get((ruby_id - 1) as usize))
                                .cloned()
                                .unwrap_or_default();
                            pairs.push(RubyPair::from_trimmed(&base, &annotation));
                        }
                    }
                }
            }

            // Recurse — storylines have nested content_list etc.
            for (_, v) in fields {
                collect_pairs_from_ion(v, content_map, ruby_lookup, resolve_sym, pairs);
            }
        }
        IonValue::List(items) => {
            for item in items {
                collect_pairs_from_ion(item, content_map, ruby_lookup, resolve_sym, pairs);
            }
        }
        IonValue::Annotated(_, inner) => {
            collect_pairs_from_ion(inner, content_map, ruby_lookup, resolve_sym, pairs);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xhtml_simple_ruby() {
        let mut pairs = Vec::new();
        extract_pairs_from_xhtml("<p><ruby>漢字<rt>かんじ</rt></ruby></p>", &mut pairs);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].base, "漢字");
        assert_eq!(pairs[0].annotation, "かんじ");
    }

    #[test]
    fn xhtml_explicit_rb() {
        let mut pairs = Vec::new();
        extract_pairs_from_xhtml(
            "<p><ruby><rb>漢字</rb><rt>かんじ</rt></ruby></p>",
            &mut pairs,
        );
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].base, "漢字");
        assert_eq!(pairs[0].annotation, "かんじ");
    }

    #[test]
    fn xhtml_compound_implicit() {
        let mut pairs = Vec::new();
        extract_pairs_from_xhtml(
            "<p><ruby>漢<rt>かん</rt>字<rt>じ</rt></ruby></p>",
            &mut pairs,
        );
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], RubyPair::from_trimmed("漢", "かん"));
        assert_eq!(pairs[1], RubyPair::from_trimmed("字", "じ"));
    }

    #[test]
    fn xhtml_compound_explicit_rb() {
        let mut pairs = Vec::new();
        extract_pairs_from_xhtml(
            "<p><ruby><rb>漢</rb><rt>かん</rt><rb>字</rb><rt>じ</rt></ruby></p>",
            &mut pairs,
        );
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], RubyPair::from_trimmed("漢", "かん"));
        assert_eq!(pairs[1], RubyPair::from_trimmed("字", "じ"));
    }

    #[test]
    fn xhtml_rp_is_skipped() {
        let mut pairs = Vec::new();
        extract_pairs_from_xhtml(
            "<p><ruby>漢<rp>(</rp><rt>かん</rt><rp>)</rp></ruby></p>",
            &mut pairs,
        );
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], RubyPair::from_trimmed("漢", "かん"));
    }

    #[test]
    fn xhtml_orphan_ruby_dropped() {
        let mut pairs = Vec::new();
        extract_pairs_from_xhtml("<p><ruby>orphan</ruby></p>", &mut pairs);
        assert!(pairs.is_empty());
    }

    #[test]
    fn xhtml_text_outside_ruby_ignored() {
        let mut pairs = Vec::new();
        extract_pairs_from_xhtml(
            "<p>before <ruby>A<rt>a</rt></ruby> middle <ruby>B<rt>b</rt></ruby> after</p>",
            &mut pairs,
        );
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], RubyPair::from_trimmed("A", "a"));
        assert_eq!(pairs[1], RubyPair::from_trimmed("B", "b"));
    }
}
