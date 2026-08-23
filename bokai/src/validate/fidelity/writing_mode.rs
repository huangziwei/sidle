//! Writing-mode validation — verify the book-level writing mode survives
//! conversion.
//!
//! In KFX, every style struct may carry a `writing_mode` field (symbol 560)
//! whose value is one of `$horizontal_tb` / `$vertical_lr` / `$vertical_rl`
//! (symbols 557/558/559). The book-level writing mode is the value most
//! commonly applied to the storyline root / body.
//!
//! In EPUB, the writing mode is a CSS property — `writing-mode: vertical-rl`,
//! optionally vendor-prefixed (`-webkit-writing-mode`, `-epub-writing-mode`).
//! It typically appears on the `body` selector or a class applied to the
//! storyline root.
//!
//! Every declared writing-mode value on each side enters a multiset. The book
//! writing mode is the most-cited non-default value, `horizontal-tb` where
//! none is declared.

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

/// One of `horizontal-tb` / `vertical-rl` / `vertical-lr`, or `Other` for any
/// value outside that set. `HorizontalTb` is the CSS initial value.
#[derive(Debug, Clone, Default, Hash, PartialEq, Eq)]
pub enum Mode {
    #[default]
    HorizontalTb,
    VerticalRl,
    VerticalLr,
    Other(String),
}

impl Mode {
    fn from_css_value(v: &str) -> Self {
        match v.trim().to_lowercase().as_str() {
            "horizontal-tb" => Self::HorizontalTb,
            "vertical-rl" => Self::VerticalRl,
            "vertical-lr" => Self::VerticalLr,
            other => Self::Other(other.to_string()),
        }
    }

    fn from_kfx_symbol(name: &str) -> Self {
        // KFX uses snake_case (`horizontal_tb`) sometimes prefixed with `$`.
        let trimmed = name.trim_start_matches('$');
        match trimmed {
            "horizontal_tb" | "horizontal-tb" => Self::HorizontalTb,
            "vertical_rl" | "vertical-rl" => Self::VerticalRl,
            "vertical_lr" | "vertical-lr" => Self::VerticalLr,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn as_css(&self) -> &str {
        match self {
            Self::HorizontalTb => "horizontal-tb",
            Self::VerticalRl => "vertical-rl",
            Self::VerticalLr => "vertical-lr",
            Self::Other(s) => s.as_str(),
        }
    }

    pub fn is_vertical(&self) -> bool {
        matches!(self, Self::VerticalRl | Self::VerticalLr)
    }

    /// Tie-break priority for [`dominant_mode`], lower winning. `vertical-rl`
    /// is the common Japanese vertical axis; source CSS defines `vertical-lr`
    /// as an unused utility class.
    fn rank(&self) -> u8 {
        match self {
            Self::VerticalRl => 0,
            Self::VerticalLr => 1,
            Self::Other(_) => 2,
            Self::HorizontalTb => 3,
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// Histogram of writing-mode values found in the EPUB CSS (across spine
    /// XHTML inline `<style>` blocks and external stylesheets in the manifest).
    pub epub_modes: HashMap<Mode, usize>,
    /// Histogram of writing-mode values found in KFX style structs.
    pub kfx_modes: HashMap<Mode, usize>,
    /// The dominant book-level writing mode on the EPUB side: the most-cited
    /// non-default value, or `horizontal-tb` where none is declared.
    pub epub_book_mode: Mode,
    /// Same for the KFX side.
    pub kfx_book_mode: Mode,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.epub_book_mode == self.kfx_book_mode
    }

    pub fn print_summary(&self, dir: super::Direction) {
        let (source, target) = if dir.epub_is_source() {
            (&self.epub_book_mode, &self.kfx_book_mode)
        } else {
            (&self.kfx_book_mode, &self.epub_book_mode)
        };
        println!("Writing mode:");
        println!("  EPUB book mode: {}", self.epub_book_mode.as_css());
        println!("  KFX book mode:  {}", self.kfx_book_mode.as_css());
        if source == target {
            println!(
                "  {} preserved on {} side",
                source.as_css(),
                dir.target_label()
            );
        } else {
            println!(
                "  MISMATCH: {} ({}) vs {} ({})",
                source.as_css(),
                dir.source_label(),
                target.as_css(),
                dir.target_label()
            );
        }
        if self.epub_modes.len() > 1 {
            println!("  EPUB mode histogram:");
            let mut items: Vec<(&Mode, &usize)> = self.epub_modes.iter().collect();
            items.sort_by(|a, b| b.1.cmp(a.1));
            for (m, n) in items {
                println!("    {}×  {}", n, m.as_css());
            }
        }
        if self.kfx_modes.len() > 1 {
            println!("  KFX mode histogram:");
            let mut items: Vec<(&Mode, &usize)> = self.kfx_modes.iter().collect();
            items.sort_by(|a, b| b.1.cmp(a.1));
            for (m, n) in items {
                println!("    {}×  {}", n, m.as_css());
            }
        }
    }

    pub fn print_details(&self, _limit: usize, _dir: super::Direction) {
        // The histograms belong to print_summary.
    }
}

pub fn validate(epub_bytes: &[u8], kfx_bytes: &[u8]) -> Result<Report, String> {
    let epub_modes = extract_modes_from_epub(epub_bytes)?;
    let kfx_modes = extract_modes_from_kfx(kfx_bytes)?;
    let epub_book_mode = dominant_mode(&epub_modes);
    let kfx_book_mode = dominant_mode(&kfx_modes);
    Ok(Report {
        epub_modes,
        kfx_modes,
        epub_book_mode,
        kfx_book_mode,
    })
}

/// Most-cited non-default mode, or `horizontal-tb` where no
/// non-default mode is declared.
fn dominant_mode(modes: &HashMap<Mode, usize>) -> Mode {
    let mut items: Vec<(&Mode, &usize)> = modes.iter().collect();
    // Count descending, then `Mode::rank`, which fixes ties that `HashMap`'s
    // randomised order leaves open.
    items.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.rank().cmp(&b.0.rank())));
    for (m, _) in &items {
        if **m != Mode::HorizontalTb {
            return (*m).clone();
        }
    }
    Mode::HorizontalTb
}

// ============================================================================
// EPUB-side extraction
// ============================================================================

fn extract_modes_from_epub(epub_bytes: &[u8]) -> Result<HashMap<Mode, usize>, String> {
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
    let hint = crate::util::extract_xml_encoding(&opf_bytes);
    let opf_str = crate::util::decode_text(&opf_bytes, hint);
    let opf = parse_opf(&opf_str).map_err(|e| format!("opf parse: {:?}", e))?;

    let mut modes: HashMap<Mode, usize> = HashMap::new();

    // 1. External stylesheets — every manifest item with media-type text/css.
    for (href, media_type) in opf.manifest.values() {
        if !media_type.eq_ignore_ascii_case("text/css") {
            continue;
        }
        let full = resolve_href(&opf_base, href);
        if let Ok(css_bytes) = read_zip_entry(&mut archive, &full) {
            let enc = crate::util::extract_xml_encoding(&css_bytes);
            let css = crate::util::decode_text(&css_bytes, enc);
            scan_css_for_modes(&css, &mut modes);
        }
    }

    // 2. Inline `<style>` blocks + element `style="..."` in each spine XHTML.
    for spine_id in &opf.spine_ids {
        let Some((href, _)) = opf.manifest.get(spine_id) else {
            continue;
        };
        let full_path = resolve_href(&opf_base, href);
        let Ok(xhtml_bytes) = read_zip_entry(&mut archive, &full_path) else {
            continue;
        };
        let enc = crate::util::extract_xml_encoding(&xhtml_bytes);
        let xhtml = crate::util::decode_text(&xhtml_bytes, enc);
        scan_xhtml_for_modes(&xhtml, &mut modes);
    }

    Ok(modes)
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

/// Tally every `writing-mode: VALUE` (with optional `-webkit-` / `-epub-`
/// prefix) declaration in a CSS source.
fn scan_css_for_modes(css: &str, out: &mut HashMap<Mode, usize>) {
    let needle_full = "writing-mode";
    let lower = css.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(needle_full) {
        let pos = from + rel;
        let after = pos + needle_full.len();
        // Vendor prefixes -webkit-writing-mode / -epub-writing-mode are part
        // of the same identifier, and `find` includes them.
        // Expect `:` next (skipping whitespace).
        let mut i = after;
        while i < lower.len() && matches!(lower.as_bytes()[i], b' ' | b'\t') {
            i += 1;
        }
        if i >= lower.len() || lower.as_bytes()[i] != b':' {
            from = after;
            continue;
        }
        i += 1;
        while i < lower.len() && matches!(lower.as_bytes()[i], b' ' | b'\t') {
            i += 1;
        }
        let value_start = i;
        while i < lower.len() && !matches!(lower.as_bytes()[i], b';' | b'}' | b'\n' | b'\r') {
            i += 1;
        }
        let value = lower[value_start..i]
            .trim()
            .trim_matches(|c| c == '!' || c == '\'' || c == '"');
        let value = value.split_whitespace().next().unwrap_or("");
        if !value.is_empty() {
            *out.entry(Mode::from_css_value(value)).or_insert(0) += 1;
        }
        from = i;
    }
}

/// Walk one XHTML for inline `<style>` blocks and element `style="..."`
/// attributes; pass their contents to `scan_css_for_modes`.
fn scan_xhtml_for_modes(xhtml: &str, out: &mut HashMap<Mode, usize>) {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);

    let mut in_style = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if e.local_name().as_ref() == b"style" {
                    in_style = true;
                }
                for attr in e.attributes().flatten() {
                    if attr.key.local_name().as_ref() == b"style" {
                        let v = String::from_utf8_lossy(&attr.value);
                        scan_css_for_modes(&v, out);
                    }
                }
            }
            Ok(Event::End(e)) => {
                if e.local_name().as_ref() == b"style" {
                    in_style = false;
                }
            }
            Ok(Event::Text(e)) if in_style => {
                let s = String::from_utf8_lossy(e.as_ref());
                scan_css_for_modes(&s, out);
            }
            Ok(Event::CData(e)) if in_style => {
                let s = String::from_utf8_lossy(&e);
                scan_css_for_modes(&s, out);
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

fn extract_modes_from_kfx(kfx_bytes: &[u8]) -> Result<HashMap<Mode, usize>, String> {
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

    let mut modes: HashMap<Mode, usize> = HashMap::new();
    for ent in &entities {
        let Some(ion) = entity_media(kfx_bytes, ent) else {
            continue;
        };
        let Ok(value) = IonParser::new(ion).parse() else {
            continue;
        };
        collect_writing_mode(&value, &resolve_sym, &mut modes);
    }
    Ok(modes)
}

/// Every struct field named `writing_mode` anywhere in `value`, keyed through
/// the symbol table. A value arrives as an Ion symbol (`$horizontal_tb`) or as
/// a string.
fn collect_writing_mode<F>(value: &IonValue, resolve_sym: &F, out: &mut HashMap<Mode, usize>)
where
    F: Fn(u64) -> String,
{
    match value {
        IonValue::Struct(fields) => {
            for (k, v) in fields {
                if resolve_sym(*k) == "writing_mode" {
                    match v {
                        IonValue::Symbol(s) => {
                            let name = resolve_sym(*s);
                            *out.entry(Mode::from_kfx_symbol(&name)).or_insert(0) += 1;
                        }
                        IonValue::String(s) => {
                            *out.entry(Mode::from_css_value(s)).or_insert(0) += 1;
                        }
                        _ => {}
                    }
                }
                collect_writing_mode(v, resolve_sym, out);
            }
        }
        IonValue::List(items) => {
            for item in items {
                collect_writing_mode(item, resolve_sym, out);
            }
        }
        IonValue::Annotated(_, inner) => {
            collect_writing_mode(inner, resolve_sym, out);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_scans_simple_decl() {
        let mut out = HashMap::new();
        scan_css_for_modes("body { writing-mode: vertical-rl; }", &mut out);
        assert_eq!(out.get(&Mode::VerticalRl), Some(&1));
    }

    #[test]
    fn css_scans_vendor_prefix() {
        let mut out = HashMap::new();
        scan_css_for_modes(
            "html { -webkit-writing-mode: vertical-rl; writing-mode: vertical-rl; }",
            &mut out,
        );
        assert_eq!(out.get(&Mode::VerticalRl), Some(&2));
    }

    #[test]
    fn css_handles_horizontal_default() {
        let mut out = HashMap::new();
        scan_css_for_modes("p { color: red; }", &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn dominant_picks_vertical_over_horizontal() {
        let mut modes = HashMap::new();
        modes.insert(Mode::HorizontalTb, 100);
        modes.insert(Mode::VerticalRl, 1);
        assert_eq!(dominant_mode(&modes), Mode::VerticalRl);
    }

    #[test]
    fn dominant_falls_back_to_horizontal_when_empty() {
        let modes = HashMap::new();
        assert_eq!(dominant_mode(&modes), Mode::HorizontalTb);
    }

    #[test]
    fn dominant_breaks_vrl_vlr_tie_deterministically_to_vrl() {
        // A vertical-rl / vertical-lr tie (source CSS commonly defines an
        // unused vertical-lr utility class alongside the vertical-rl body
        // rule) must resolve to vertical-rl every time, not by HashMap order.
        for _ in 0..64 {
            let mut modes = HashMap::new();
            modes.insert(Mode::VerticalLr, 2);
            modes.insert(Mode::VerticalRl, 2);
            modes.insert(Mode::HorizontalTb, 2);
            assert_eq!(dominant_mode(&modes), Mode::VerticalRl);
        }
    }

    #[test]
    fn xhtml_inline_style_attr() {
        let mut out = HashMap::new();
        scan_xhtml_for_modes(
            r#"<html><body style="writing-mode: vertical-rl;">x</body></html>"#,
            &mut out,
        );
        assert_eq!(out.get(&Mode::VerticalRl), Some(&1));
    }

    #[test]
    fn xhtml_style_element() {
        let mut out = HashMap::new();
        scan_xhtml_for_modes(
            r#"<html><head><style>html { writing-mode: vertical-rl; }</style></head></html>"#,
            &mut out,
        );
        assert_eq!(out.get(&Mode::VerticalRl), Some(&1));
    }
}
