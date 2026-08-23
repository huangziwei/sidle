//! Text-preservation validation — verify that every visible character in
//! the source EPUB is preserved in the converted KFX.
//!
//! The concatenated visible text of every spine XHTML file, `<rt>` furigana
//! included, against the concatenated text of every `content` and
//! `ruby_content` fragment. Whitespace drops out on both sides: HTML collapses
//! runs of it and KFX emits its own paragraph breaks. The comparison is per
//! character over multisets, reporting each character whose KFX count is the
//! lower one.

use crate::formats::epub::structure::resolve_href;
use std::collections::HashMap;
use std::io::Cursor;

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::formats::epub::{parse_container_xml, parse_opf};
use crate::formats::kfx::container::{
    SymbolTable, entity_media, parse_container_header, parse_container_info, parse_index_table,
    slice_at,
};
use crate::formats::kfx::ion::{IonParser, IonValue};
use crate::formats::kfx::symbols::KfxSymbol;

/// Comparison report: char counts on each side plus per-character defects.
/// Direction-neutral: callers interpret `only_in_epub` / `only_in_kfx` based
/// on which side is the conversion source (see [`super::Direction`]).
#[derive(Debug, Default)]
pub struct Report {
    /// Total non-whitespace characters in the EPUB side (post-normalisation).
    pub epub_chars: usize,
    /// Total non-whitespace characters in the KFX side (post-normalisation).
    pub kfx_chars: usize,
    /// Characters with epub_count > kfx_count, sorted by deficit desc.
    pub only_in_epub: Vec<(char, usize)>,
    /// Characters with kfx_count > epub_count, sorted by deficit desc.
    pub only_in_kfx: Vec<(char, usize)>,
}

impl Report {
    pub fn is_clean_for(&self, dir: super::Direction) -> bool {
        // Clean covers the source side alone. Fabrication in the target —
        // nav metadata reaching visible text — is reported in
        // `only_in_kfx` / `only_in_epub`.
        if dir.epub_is_source() {
            self.only_in_epub.is_empty()
        } else {
            self.only_in_kfx.is_empty()
        }
    }

    /// Percentage of source-side characters preserved in the target side.
    pub fn preservation_ratio(&self, dir: super::Direction) -> f64 {
        let (source_total, dropped) = if dir.epub_is_source() {
            (
                self.epub_chars,
                self.only_in_epub.iter().map(|(_, n)| n).sum::<usize>(),
            )
        } else {
            (
                self.kfx_chars,
                self.only_in_kfx.iter().map(|(_, n)| n).sum::<usize>(),
            )
        };
        if source_total == 0 {
            return 1.0;
        }
        source_total.saturating_sub(dropped) as f64 / source_total as f64
    }

    pub fn print_summary(&self, dir: super::Direction) {
        let (source_chars, target_chars, dropped, fabricated) = if dir.epub_is_source() {
            (
                self.epub_chars,
                self.kfx_chars,
                &self.only_in_epub,
                &self.only_in_kfx,
            )
        } else {
            (
                self.kfx_chars,
                self.epub_chars,
                &self.only_in_kfx,
                &self.only_in_epub,
            )
        };
        let dropped_total: usize = dropped.iter().map(|(_, n)| n).sum();
        let fabricated_total: usize = fabricated.iter().map(|(_, n)| n).sum();
        println!("{} chars (source):  {}", dir.source_label(), source_chars);
        println!("{} chars (target):  {}", dir.target_label(), target_chars);
        println!(
            "Preservation:  {:.4}%",
            self.preservation_ratio(dir) * 100.0
        );
        println!(
            "Dropped (missing in {}): {} ({} unique)",
            dir.target_label(),
            dropped_total,
            dropped.len()
        );
        println!(
            "Fabricated (extra in {}): {} ({} unique)",
            dir.target_label(),
            fabricated_total,
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
                "\n--- Dropped (in {}, missing from {}) [first {}] ---",
                dir.source_label(),
                dir.target_label(),
                limit
            );
            for (c, n) in dropped.iter().take(limit) {
                println!("  ({}×)  {:?}  (U+{:04X})", n, c, *c as u32);
            }
            if dropped.len() > limit {
                println!("  ... and {} more unique chars", dropped.len() - limit);
            }
        }
        if !fabricated.is_empty() {
            println!(
                "\n--- Fabricated (in {}, not in {}) [first {}] ---",
                dir.target_label(),
                dir.source_label(),
                limit
            );
            for (c, n) in fabricated.iter().take(limit) {
                println!("  ({}×)  {:?}  (U+{:04X})", n, c, *c as u32);
            }
            if fabricated.len() > limit {
                println!("  ... and {} more unique chars", fabricated.len() - limit);
            }
        }
    }
}

pub fn validate(epub_bytes: &[u8], kfx_bytes: &[u8]) -> Result<Report, String> {
    let epub = extract_text_from_epub(epub_bytes)?;
    let kfx = extract_text_from_kfx(kfx_bytes)?;

    let epub_counts = char_counts(&epub);
    let kfx_counts = char_counts(&kfx);

    let epub_total: usize = epub_counts.values().sum();
    let kfx_total: usize = kfx_counts.values().sum();

    let mut only_in_epub: Vec<(char, usize)> = Vec::new();
    let mut only_in_kfx: Vec<(char, usize)> = Vec::new();
    for (c, ec) in &epub_counts {
        let kc = kfx_counts.get(c).copied().unwrap_or(0);
        if ec > &kc {
            only_in_epub.push((*c, ec - kc));
        }
    }
    for (c, kc) in &kfx_counts {
        let ec = epub_counts.get(c).copied().unwrap_or(0);
        if kc > &ec {
            only_in_kfx.push((*c, kc - ec));
        }
    }
    only_in_epub.sort_by_key(|b| std::cmp::Reverse(b.1));
    only_in_kfx.sort_by_key(|b| std::cmp::Reverse(b.1));

    Ok(Report {
        epub_chars: epub_total,
        kfx_chars: kfx_total,
        only_in_epub,
        only_in_kfx,
    })
}

/// A histogram of the non-whitespace characters in `s`. HTML collapses runs
/// of whitespace and KFX emits its own paragraph breaks, and neither side's
/// whitespace counts carry content.
fn char_counts(s: &str) -> HashMap<char, usize> {
    let mut out = HashMap::new();
    for c in s.chars() {
        if c.is_whitespace() {
            continue;
        }
        *out.entry(c).or_insert(0) += 1;
    }
    out
}

// ============================================================================
// EPUB-side extraction
// ============================================================================

/// The visible text of a source EPUB's spine XHTML files. `<script>`,
/// `<style>` and `<rp>` content drops out; every other tag is a transparent
/// container around its text leaves.
pub fn extract_text_from_epub(epub_bytes: &[u8]) -> Result<String, String> {
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

    let mut out = String::new();
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
        extract_text_from_xhtml(&xhtml, &mut out);
    }
    Ok(out)
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

/// Walk a single XHTML document and append all visible text to `out`.
pub fn extract_text_from_xhtml(xhtml: &str, out: &mut String) {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);

    // Text inside script, style, head, <rp> and <nav> is suppressed. An EPUB 3
    // nav document holds landmark and TOC labels, which the Kindle reader
    // draws in its own UI and not as book text.
    let mut suppress_depth: usize = 0;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"script" | b"style" | b"head" | b"rp" | b"nav" => {
                    suppress_depth += 1;
                }
                _ => {}
            },
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"script" | b"style" | b"head" | b"rp" | b"nav" => {
                    suppress_depth = suppress_depth.saturating_sub(1);
                }
                _ => {}
            },
            Ok(Event::Text(e)) if suppress_depth == 0 => {
                out.push_str(&String::from_utf8_lossy(e.as_ref()));
            }
            // quick-xml emits an entity reference (`&amp;`, `&#160;`) as its
            // own `GeneralRef` event, outside `Text`. Each decodes and
            // appends here.
            Ok(Event::GeneralRef(e)) if suppress_depth == 0 => {
                let entity = String::from_utf8_lossy(e.as_ref());
                if let Some(resolved) = crate::formats::epub::parser::resolve_entity(&entity) {
                    out.push_str(&resolved);
                }
            }
            Ok(Event::CData(e)) if suppress_depth == 0 => {
                out.push_str(&String::from_utf8_lossy(&e));
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

/// Every `content` fragment's text, plus one copy of each ruby annotation per
/// style_event reference. `extract_pairs_from_kfx` yields one pair per
/// reference, undoing `ruby_content`'s deduplication of annotation strings.
pub fn extract_text_from_kfx(kfx_bytes: &[u8]) -> Result<String, String> {
    let header = parse_container_header(kfx_bytes).map_err(|e| format!("kfx header: {:?}", e))?;
    let info_data = slice_at(
        kfx_bytes,
        header.container_info_offset,
        header.container_info_length,
    )
    .ok_or("container info out of bounds")?;
    let info =
        parse_container_info(info_data).map_err(|e| format!("kfx container info: {:?}", e))?;

    // SymbolTable::from_fragment seats doc-local ids at the container's
    // declared import max_id.
    let symbols = SymbolTable::from_fragment(
        info.doc_symbols
            .and_then(|(off, len)| slice_at(kfx_bytes, off, len)),
    );

    let resolve_sym = |id: u64| -> String { symbols.resolve(id).to_string() };

    let Some((idx_off, idx_len)) = info.index else {
        return Err("kfx: no index table".into());
    };
    let index_data = slice_at(kfx_bytes, idx_off, idx_len).ok_or("kfx: index out of bounds")?;
    let entities = parse_index_table(index_data, header.header_len);

    // Both entity types that carry reading text: `$145 content`, and `$259
    // storyline`, whose own `content_list` holds the text of a storyline that
    // names no separate content fragment.
    let text_types: [u32; 2] = [KfxSymbol::Content as u32, KfxSymbol::Storyline as u32];

    let mut out = String::new();
    for ent in &entities {
        if !text_types.contains(&ent.type_id) {
            continue;
        }
        let Some(ion) = entity_media(kfx_bytes, ent) else {
            continue;
        };
        let Ok(value) = IonParser::new(ion).parse() else {
            continue;
        };
        collect_content_text(&value, &resolve_sym, &mut out);
    }

    // Annotations: one copy per style_event reference, not one per deduped
    // ruby_content entry. `extract_pairs_from_kfx` yields one entry per
    // reference.
    let pairs = super::ruby::extract_pairs_from_kfx(kfx_bytes)?;
    for pair in pairs {
        out.push_str(&pair.annotation);
        out.push(' ');
    }

    Ok(out)
}

fn collect_content_text<F>(value: &IonValue, resolve_sym: &F, out: &mut String)
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, inner) => inner.as_ref(),
        _ => value,
    };
    if let IonValue::Struct(fields) = inner {
        for (k, v) in fields {
            match resolve_sym(*k).as_str() {
                // `$145 content`: leaf text as a string, or a content_ref
                // struct (`$176 name` + `$169 index`) resolved when the
                // outer loop reaches its target Content entity.
                "content" => {
                    if let IonValue::String(s) = v.unwrap_annotated() {
                        out.push_str(s);
                        out.push(' ');
                    }
                }
                // `$146 content_list`: structural children. A string is leaf
                // text; a struct carries its own `content` /
                // `content_list` and is walked recursively.
                "content_list" => {
                    if let IonValue::List(items) = v {
                        for item in items {
                            match item.unwrap_annotated() {
                                IonValue::String(s) => {
                                    out.push_str(s);
                                    out.push(' ');
                                }
                                IonValue::Struct(_) => {
                                    collect_content_text(item, resolve_sym, out);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xhtml_strips_script_and_style() {
        let mut out = String::new();
        extract_text_from_xhtml(
            r#"<html><head><style>p{color:red}</style></head>
               <body><script>alert(1)</script><p>hello</p></body></html>"#,
            &mut out,
        );
        assert!(!out.contains("color:red"));
        assert!(!out.contains("alert"));
        assert!(out.contains("hello"));
    }

    #[test]
    fn xhtml_keeps_rt_drops_rp() {
        let mut out = String::new();
        extract_text_from_xhtml(
            "<p><ruby>漢<rp>(</rp><rt>かん</rt><rp>)</rp></ruby></p>",
            &mut out,
        );
        // base + annotation kept, parens dropped
        assert!(out.contains("漢"));
        assert!(out.contains("かん"));
        assert!(!out.contains("("));
        assert!(!out.contains(")"));
    }

    #[test]
    fn whitespace_normalised() {
        let a = char_counts("foo bar\nbaz\tqux");
        let b = char_counts("foo  bar     baz qux");
        // 'f','o','o','b','a','r','z','q','u','x' counts identical
        for c in ['f', 'o', 'b', 'a', 'r', 'z', 'q', 'u', 'x'] {
            assert_eq!(a.get(&c), b.get(&c), "mismatch for {:?}", c);
        }
    }
}
