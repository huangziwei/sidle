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

/// Result of comparing EPUB-side and KFX-side pair sets. Direction-neutral:
/// callers interpret `only_in_epub` and `only_in_kfx` based on which side is
/// the ground truth for the conversion direction under test (see
/// [`crate::validate::Direction`]).
#[derive(Debug, Default)]
pub struct Report {
    pub epub_pairs: Vec<RubyPair>,
    pub kfx_pairs: Vec<RubyPair>,
    /// Pairs present in EPUB but not in KFX. In EPUB→KFX, these are pairs
    /// boko dropped or mispaired. In KFX→EPUB, these are pairs boko fabricated.
    pub only_in_epub: Vec<(RubyPair, usize)>,
    /// Pairs present in KFX but not in EPUB. In EPUB→KFX, these are pairs
    /// boko fabricated. In KFX→EPUB, these are pairs boko dropped or mispaired.
    pub only_in_kfx: Vec<(RubyPair, usize)>,
    /// Pairs present in both, with min(epub_count, kfx_count) per unique pair.
    pub matched: usize,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.only_in_epub.is_empty() && self.only_in_kfx.is_empty()
    }

    pub fn print_summary(&self, dir: super::Direction) {
        println!("EPUB pairs:    {}", self.epub_pairs.len());
        println!("KFX pairs:     {}", self.kfx_pairs.len());
        println!("Matched:       {}", self.matched);
        let epub_only_total: usize = self.only_in_epub.iter().map(|(_, n)| n).sum();
        let kfx_only_total: usize = self.only_in_kfx.iter().map(|(_, n)| n).sum();
        let (dropped, dropped_n, fabricated, fabricated_n) = if dir.epub_is_source() {
            // EPUB→KFX: EPUB-only = dropped by boko; KFX-only = fabricated.
            (
                &self.only_in_epub,
                epub_only_total,
                &self.only_in_kfx,
                kfx_only_total,
            )
        } else {
            // KFX→EPUB: KFX-only = dropped by boko; EPUB-only = fabricated.
            (
                &self.only_in_kfx,
                kfx_only_total,
                &self.only_in_epub,
                epub_only_total,
            )
        };
        println!(
            "Dropped (missing in {}): {} ({} unique)",
            dir.target_label(),
            dropped_n,
            dropped.len()
        );
        println!(
            "Fabricated (extra in {}): {} ({} unique)",
            dir.target_label(),
            fabricated_n,
            fabricated.len()
        );
    }

    pub fn print_details(&self, limit: usize, dir: super::Direction) {
        let (dropped, fabricated) = if dir.epub_is_source() {
            (&self.only_in_epub, &self.only_in_kfx)
        } else {
            (&self.only_in_kfx, &self.only_in_epub)
        };
        if !dropped.is_empty() {
            println!(
                "\n--- Dropped (in source {}, missing from {}) [first {}] ---",
                dir.source_label(),
                dir.target_label(),
                limit
            );
            for (pair, n) in dropped.iter().take(limit) {
                println!("  ({}×)  {}  →  {}", n, pair.base, pair.annotation);
            }
            if dropped.len() > limit {
                println!("  ... and {} more unique pairs", dropped.len() - limit);
            }
        }
        if !fabricated.is_empty() {
            println!(
                "\n--- Fabricated (in {}, not in source {}) [first {}] ---",
                dir.target_label(),
                dir.source_label(),
                limit
            );
            for (pair, n) in fabricated.iter().take(limit) {
                println!("  ({}×)  {}  →  {}", n, pair.base, pair.annotation);
            }
            if fabricated.len() > limit {
                println!("  ... and {} more unique pairs", fabricated.len() - limit);
            }
        }
    }
}

/// Compare ruby pairs across both sides. Direction-neutral — caller interprets
/// the resulting `only_in_epub` / `only_in_kfx` per conversion direction.
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

    let mut only_in_epub: Vec<(RubyPair, usize)> = Vec::new();
    let mut only_in_kfx: Vec<(RubyPair, usize)> = Vec::new();
    let mut matched: usize = 0;

    for (pair, ecount) in &epub_counts {
        let kcount = kfx_counts.get(pair).copied().unwrap_or(0);
        matched += ecount.min(&kcount).to_owned();
        if ecount > &kcount {
            only_in_epub.push((pair.clone(), ecount - kcount));
        }
    }
    for (pair, kcount) in &kfx_counts {
        let ecount = epub_counts.get(pair).copied().unwrap_or(0);
        if kcount > &ecount {
            only_in_kfx.push((pair.clone(), kcount - ecount));
        }
    }

    // Sort by frequency descending so the worst offenders surface first.
    only_in_epub.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.base.cmp(&b.0.base)));
    only_in_kfx.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.base.cmp(&b.0.base)));

    Ok(Report {
        epub_pairs,
        kfx_pairs,
        only_in_epub,
        only_in_kfx,
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
            Ok(Event::Empty(e)) => if e.local_name().as_ref() == b"rt" {
                // Self-closing rt — emit pair with empty annotation if base exists.
                if !pending_base.is_empty() {
                    out.push(RubyPair::from_trimmed(&pending_base, ""));
                }
                pending_base.clear();
                pending_annotation.clear();
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
        } else if ent.type_id == ruby_content_type
            && let Some((ruby_name, annotations)) = extract_ruby_content(&value, &resolve_sym) {
                ruby_lookup.insert(ruby_name, annotations);
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
                    // Sub-ranges from `ruby_id_list`: one style_event can annotate
                    // several base spans, each with its own ruby_id. boko's emitter
                    // (content.rs try_emit_ruby_text) expands these into multiple
                    // pairs, so the KFX side must too — otherwise every list-event
                    // pair shows up as "fabricated".
                    let mut id_list: Vec<(i64, i64, i64)> = Vec::new();
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
                            "ruby_id_list" => {
                                if let Some(entries) = v.as_list() {
                                    for entry in entries {
                                        let Some(ef) = entry.unwrap_annotated().as_struct() else {
                                            continue;
                                        };
                                        let (mut o, mut l, mut rid) = (0i64, 0i64, 0i64);
                                        for (ek, ev2) in ef {
                                            match resolve_sym(*ek).as_str() {
                                                "offset" => {
                                                    if let IonValue::Int(n) = ev2 {
                                                        o = *n;
                                                    }
                                                }
                                                "length" => {
                                                    if let IonValue::Int(n) = ev2 {
                                                        l = *n;
                                                    }
                                                }
                                                "ruby_id" => {
                                                    if let IonValue::Int(n) = ev2 {
                                                        rid = *n;
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                        if l > 0 && rid > 0 {
                                            id_list.push((o, l, rid));
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    if offset < 0 || ruby_name.is_empty() {
                        continue;
                    }
                    // A single `ruby_id` covers the whole [0, length) span; otherwise
                    // use the `ruby_id_list` sub-ranges. Mirrors boko's emitter.
                    let ranges: Vec<(i64, i64, i64)> = if ruby_id > 0 {
                        vec![(0, length, ruby_id)]
                    } else {
                        id_list
                    };
                    for (sub_off, sub_len, rid) in ranges {
                        if sub_len <= 0 || rid <= 0 {
                            continue;
                        }
                        let start = (offset + sub_off).max(0) as usize;
                        let end = (start + sub_len as usize).min(chars.len());
                        if start < end {
                            let base: String = chars[start..end].iter().collect();
                            let annotation = ruby_lookup
                                .get(&ruby_name)
                                .and_then(|v| v.get((rid - 1) as usize))
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
