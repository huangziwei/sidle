//! Headings + TOC validation — verify that `<h1>`–`<h6>` from spine XHTML
//! and the OPF/NCX/nav table-of-contents survive into KFX's `book_navigation`
//! ($389) entity.
//!
//! KFX structure (built in `export::kfx::build_book_navigation_fragment_*`):
//!
//! ```text
//! book_navigation:
//!   [{reading_order_name: $default, nav_containers: [
//!     NavContainer{ nav_type: $headings,  entries: [NavUnit{...}, ...] },
//!     NavContainer{ nav_type: $toc,       entries: [NavUnit{...}, ...] },
//!     NavContainer{ nav_type: $landmarks, entries: [NavUnit{...}, ...] },
//!   ]}]
//! ```
//!
//! Each `NavUnit` has a `target_position: {id, offset}` whose `id` references
//! an element ID emitted inside a storyline. If the id doesn't resolve, the
//! nav entry tap-jumps to nowhere on Kindle. Checks:
//!
//! 1. **Headings count balance** — for each level h2–h6, the source spine's
//!    heading count should match the count of leaf nav_units under the
//!    KFX headings container. (h1 is intentionally not navigated by Kindle:
//!    `build_headings_entries` skips it; see `export/kfx.rs::level_to_symbol`.)
//! 2. **TOC count balance** — flat-traversal entry count of source NCX
//!    should match KFX TOC nav_unit count (leaves + inner; we count all
//!    nodes that carry a target_position).
//! 3. **Reachability** — every `target_position.id` (headings and TOC)
//!    must resolve to an element ID present in some storyline.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Cursor;

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::epub::{parse_container_xml, parse_ncx, parse_opf};
use crate::kfx::container::{
    extract_doc_symbols, parse_container_header, parse_container_info, parse_index_table,
    skip_enty_header,
};
use crate::kfx::ion::{IonParser, IonValue};
use crate::kfx::symbols::{KFX_SYMBOL_TABLE, KfxSymbol};
use crate::model::TocEntry;

/// A nav_unit's resolved target inside KFX.
#[derive(Debug, Clone)]
pub struct NavTarget {
    /// Element ID the nav_unit jumps to.
    pub element_id: u64,
    /// Optional offset within the fragment.
    pub offset: u64,
}

/// A dangling nav target: target_position.id is not present in any storyline.
#[derive(Debug, Clone)]
pub struct DanglingNav {
    /// "headings" or "toc" — which container the dangling entry came from.
    pub container: String,
    pub target: NavTarget,
}

#[derive(Debug, Default)]
pub struct Report {
    // --- EPUB side ---
    /// Count of `<h1>`–`<h6>` per level in the EPUB spine (level → count).
    pub epub_headings_by_level: HashMap<u8, usize>,
    /// Flat count of TOC entries from EPUB NCX (recursive total).
    pub epub_toc_entry_count: usize,
    /// Count of TOC entries pointing at manifest items not in the spine.
    /// These can't be addressed by KFX position-based navigation and are
    /// excluded from the count diff.
    pub epub_non_spine_toc_entries: usize,
    /// Whether the EPUB has an NCX. If not, TOC checks are skipped.
    pub epub_has_ncx: bool,
    /// Distinct `<content src>` paths (everything before `#`) across all NCX
    /// `<navPoint>` entries. A well-formed NCX has roughly one distinct path
    /// per top-level chapter; when this collapses to 1 while
    /// `epub_toc_entry_count` is many, every TOC entry is silently pointing at
    /// the same placeholder — the most common port defect (boko's
    /// `nav_unit_to_navpoint` placeholder before `process_position` lands).
    pub epub_distinct_ncx_hrefs: usize,
    /// NCX `<content src>` paths (before `#`) that don't resolve to any
    /// manifest entry. Each one is a broken TOC link.
    pub epub_unresolved_ncx_hrefs: Vec<String>,

    // --- KFX side ---
    /// Count of nav_units under the KFX headings container, keyed by level.
    /// Only counts leaf entries (the inner ones with offsets), not the level
    /// group entry which `build_headings_entries` emits as a wrapper.
    pub kfx_headings_by_level: HashMap<u8, usize>,
    /// All NavUnit target_positions inside the headings container.
    pub kfx_heading_targets: Vec<NavTarget>,
    /// Flat count of TOC nav_units (counts every node with a target_position).
    pub kfx_toc_entry_count: usize,
    pub kfx_toc_targets: Vec<NavTarget>,

    // --- Defects ---
    /// Nav targets whose element_id isn't present in any storyline.
    pub dangling_nav: Vec<DanglingNav>,
    /// Per-level heading count discrepancies (epub - kfx), only listed
    /// when non-zero. Positive = boko's converter dropped headings (in
    /// EPUB→KFX) or fabricated headings (in KFX→EPUB), depending on direction.
    pub heading_count_diffs: Vec<(u8, i64)>,
    /// TOC count discrepancy (epub - kfx). Only set when EPUB has NCX.
    pub toc_count_diff: Option<i64>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.dangling_nav.is_empty()
            && self.heading_count_diffs.is_empty()
            && self.toc_count_diff.unwrap_or(0) == 0
            && self.epub_unresolved_ncx_hrefs.is_empty()
            && !self.ncx_collapsed_to_placeholder()
    }

    /// Heuristic: if the NCX has multiple TOC entries but every entry points
    /// at the same file (distinct hrefs == 1 while total > 1), the port has
    /// stamped a placeholder href on every entry. Real NCXs have at least one
    /// distinct path per chapter.
    pub fn ncx_collapsed_to_placeholder(&self) -> bool {
        self.epub_has_ncx
            && self.epub_toc_entry_count > 1
            && self.epub_distinct_ncx_hrefs <= 1
    }

    pub fn print_summary(&self, dir: super::Direction) {
        let epub_total: usize = self.epub_headings_by_level.values().sum();
        let kfx_total: usize = self.kfx_headings_by_level.values().sum();
        println!("EPUB headings:");
        for level in 1..=6u8 {
            let n = self.epub_headings_by_level.get(&level).copied().unwrap_or(0);
            if n > 0 {
                println!("  h{}:  {}", level, n);
            }
        }
        println!("  total: {}", epub_total);
        println!("KFX headings nav (h1 intentionally not navigated):");
        for level in 2..=6u8 {
            let n = self.kfx_headings_by_level.get(&level).copied().unwrap_or(0);
            if n > 0 {
                println!("  h{}:  {}", level, n);
            }
        }
        println!("  total: {}", kfx_total);
        println!("TOC entries:");
        if self.epub_has_ncx {
            if self.epub_non_spine_toc_entries > 0 {
                println!(
                    "  EPUB NCX:    {} ({} non-spine, can't be addressed in KFX)",
                    self.epub_toc_entry_count, self.epub_non_spine_toc_entries
                );
            } else {
                println!("  EPUB NCX:    {}", self.epub_toc_entry_count);
            }
        } else {
            println!("  EPUB NCX:    (none — TOC check skipped)");
        }
        println!("  KFX TOC nav: {}", self.kfx_toc_entry_count);
        if self.epub_has_ncx {
            println!(
                "  EPUB NCX distinct hrefs: {} (of {} entries)",
                self.epub_distinct_ncx_hrefs, self.epub_toc_entry_count
            );
        }
        println!(
            "Defects (source = {}, target = {}):",
            dir.source_label(),
            dir.target_label()
        );
        println!("  dangling nav targets:       {}", self.dangling_nav.len());
        println!("  heading level diffs:        {}", self.heading_count_diffs.len());
        println!(
            "  NCX hrefs not in manifest:  {}",
            self.epub_unresolved_ncx_hrefs.len()
        );
        if self.ncx_collapsed_to_placeholder() {
            println!(
                "  NCX collapsed to placeholder: {} of {} TOC entries share 1 href",
                self.epub_toc_entry_count, self.epub_toc_entry_count
            );
        }
        if let Some(d) = self.toc_count_diff {
            println!("  TOC count diff (epub-kfx):  {}", d);
        }
    }

    pub fn print_details(&self, limit: usize, _dir: super::Direction) {
        if !self.heading_count_diffs.is_empty() {
            println!("\n--- Heading level discrepancies (epub − kfx) ---");
            for (level, diff) in &self.heading_count_diffs {
                println!("  h{}:  {:+}", level, diff);
            }
        }
        if !self.epub_unresolved_ncx_hrefs.is_empty() {
            println!(
                "\n--- NCX <content src> not in manifest [first {}] ---",
                limit
            );
            for h in self.epub_unresolved_ncx_hrefs.iter().take(limit) {
                println!("  {}", h);
            }
            if self.epub_unresolved_ncx_hrefs.len() > limit {
                println!(
                    "  ... and {} more",
                    self.epub_unresolved_ncx_hrefs.len() - limit
                );
            }
        }
        if !self.dangling_nav.is_empty() {
            println!(
                "\n--- Nav targets pointing at missing elements [first {}] ---",
                limit
            );
            for d in self.dangling_nav.iter().take(limit) {
                println!(
                    "  [{}]  element_id {}  +{}",
                    d.container, d.target.element_id, d.target.offset
                );
            }
            if self.dangling_nav.len() > limit {
                println!("  ... and {} more", self.dangling_nav.len() - limit);
            }
        }
    }
}

pub fn validate(epub_bytes: &[u8], kfx_bytes: &[u8]) -> Result<Report, String> {
    let epub_side = extract_epub_nav(epub_bytes)?;
    let kfx = extract_kfx_nav(kfx_bytes)?;

    // h1 is intentionally NOT navigated by Kindle (h1 → None in
    // export::kfx::level_to_symbol). When comparing levels we skip h1.
    let mut heading_count_diffs: Vec<(u8, i64)> = Vec::new();
    for level in 2..=6u8 {
        let ep = epub_side.headings_by_level.get(&level).copied().unwrap_or(0) as i64;
        let kfx_c = kfx.headings_by_level.get(&level).copied().unwrap_or(0) as i64;
        if ep != kfx_c {
            heading_count_diffs.push((level, ep - kfx_c));
        }
    }

    // Exclude non-spine entries from the expected count — they can't survive
    // into KFX (position-based nav requires spine content) — and discount the
    // synthesized leading cover (表紙) entry boko prepends to match Amazon, which
    // the source NCX never carries, so the comparison stays source-vs-source.
    let toc_count_diff = epub_side.has_ncx.then(|| {
        let expected = epub_side
            .toc_entry_count
            .saturating_sub(epub_side.non_spine_toc_entries);
        let cover_synth = kfx.cover_target.is_some() && kfx.toc_targets.len() > expected;
        let kfx_count = kfx.toc_targets.len() - cover_synth as usize;
        expected as i64 - kfx_count as i64
    });

    // A nav target is dangling unless it resolves to a storyline element id — or
    // it's the cover, a section-root / page-template position that's valid and
    // navigable (via `section_position_id_map`) but never lives in a storyline.
    // A real Amazon KFX is identical here: its cover TOC/landmark id is flagged
    // by this same check without the exemption.
    let reachable = |id: u64| Some(id) == kfx.cover_target || kfx.element_ids.contains(&id);
    let mut dangling_nav: Vec<DanglingNav> = Vec::new();
    for t in &kfx.heading_targets {
        if !reachable(t.element_id) {
            dangling_nav.push(DanglingNav {
                container: "headings".into(),
                target: t.clone(),
            });
        }
    }
    for t in &kfx.toc_targets {
        if !reachable(t.element_id) {
            dangling_nav.push(DanglingNav {
                container: "toc".into(),
                target: t.clone(),
            });
        }
    }

    Ok(Report {
        epub_headings_by_level: epub_side.headings_by_level,
        epub_toc_entry_count: epub_side.toc_entry_count,
        epub_non_spine_toc_entries: epub_side.non_spine_toc_entries,
        epub_has_ncx: epub_side.has_ncx,
        epub_distinct_ncx_hrefs: epub_side.distinct_ncx_hrefs,
        epub_unresolved_ncx_hrefs: epub_side.unresolved_ncx_hrefs,
        kfx_headings_by_level: kfx.headings_by_level,
        kfx_heading_targets: kfx.heading_targets,
        kfx_toc_entry_count: kfx.toc_targets.len(),
        kfx_toc_targets: kfx.toc_targets,
        dangling_nav,
        heading_count_diffs,
        toc_count_diff,
    })
}

// ============================================================================
// Source-side extraction
// ============================================================================

#[derive(Debug, Default)]
struct EpubNav {
    headings_by_level: HashMap<u8, usize>,
    /// Flat-recursive count of every TocEntry node.
    toc_entry_count: usize,
    /// Count of TOC entries whose href path is in the manifest but not in the
    /// spine. KFX position-based navigation can only address spine content,
    /// so these entries are genuinely unreachable — boko silently drops them
    /// and the validator excludes them from the count diff.
    non_spine_toc_entries: usize,
    has_ncx: bool,
    distinct_ncx_hrefs: usize,
    unresolved_ncx_hrefs: Vec<String>,
}

fn extract_epub_nav(epub_bytes: &[u8]) -> Result<EpubNav, String> {
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

    // 1. Headings — walk every spine xhtml.
    let mut headings_by_level: HashMap<u8, usize> = HashMap::new();
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
        count_headings(&xhtml, &mut headings_by_level);
    }

    // Build the set of spine paths so we can identify TOC entries that
    // point at non-spine manifest items (calibre puts a "Beginning" landmark
    // file in manifest+guide but not spine; the NCX still references it).
    let spine_paths: HashSet<String> = opf
        .spine_ids
        .iter()
        .filter_map(|id| opf.manifest.get(id).map(|(href, _)| {
            // NCX hrefs are relative to OPF; spine items live in manifest with the same form.
            // Normalise to opf_base-prefixed for comparison with NCX (which is also opf_base-relative).
            href.clone()
        }))
        .collect();

    // Set of all manifest paths (relative to opf_base, like NCX hrefs).
    // Used to flag NCX `<content src>` entries that don't resolve to any
    // file in the manifest.
    let manifest_paths: HashSet<String> = opf
        .manifest
        .values()
        .map(|(href, _)| href.clone())
        .collect();

    // 2. TOC — parse NCX if present.
    let (toc_entry_count, non_spine_toc_entries, has_ncx, distinct_ncx_hrefs, unresolved_ncx_hrefs) =
        if let Some(ncx_href) = &opf.ncx_href {
            let ncx_path = format!("{}{}", opf_base, ncx_href);
            match read_zip_entry(&mut archive, &ncx_path) {
                Ok(ncx_bytes) => {
                    let enc = crate::util::extract_xml_encoding(&ncx_bytes);
                    let ncx_str = crate::util::decode_text(&ncx_bytes, enc);
                    match parse_ncx(&ncx_str) {
                        Ok(entries) => {
                            let total = count_toc_entries(&entries);
                            let non_spine = count_non_spine_entries(&entries, &spine_paths);
                            let mut href_paths: Vec<String> = Vec::new();
                            collect_toc_hrefs(&entries, &mut href_paths);
                            let distinct: HashSet<&String> = href_paths.iter().collect();
                            let unresolved: Vec<String> = href_paths
                                .iter()
                                .filter(|p| !manifest_paths.contains(*p))
                                .cloned()
                                .collect::<HashSet<_>>()
                                .into_iter()
                                .collect();
                            (total, non_spine, true, distinct.len(), unresolved)
                        }
                        Err(_) => (0, 0, false, 0, Vec::new()),
                    }
                }
                Err(_) => (0, 0, false, 0, Vec::new()),
            }
        } else {
            (0, 0, false, 0, Vec::new())
        };

    Ok(EpubNav {
        headings_by_level,
        toc_entry_count,
        non_spine_toc_entries,
        has_ncx,
        distinct_ncx_hrefs,
        unresolved_ncx_hrefs,
    })
}

/// Recursively collect TOC entry hrefs (path component before `#`).
fn collect_toc_hrefs(entries: &[TocEntry], out: &mut Vec<String>) {
    for e in entries {
        let path = e.href.split('#').next().unwrap_or(&e.href).to_string();
        if !path.is_empty() {
            out.push(path);
        }
        collect_toc_hrefs(&e.children, out);
    }
}

/// Count TOC entries whose href path (everything before `#`) isn't in the
/// spine. These can't survive into KFX because KFX position-based navigation
/// requires the target to be inside spine content.
fn count_non_spine_entries(entries: &[TocEntry], spine_paths: &HashSet<String>) -> usize {
    let mut n = 0;
    for e in entries {
        let path = e.href.split('#').next().unwrap_or(&e.href);
        if !spine_paths.contains(path) {
            n += 1;
        }
        n += count_non_spine_entries(&e.children, spine_paths);
    }
    n
}

fn count_toc_entries(entries: &[TocEntry]) -> usize {
    let mut n = 0;
    for e in entries {
        n += 1;
        n += count_toc_entries(&e.children);
    }
    n
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

/// Count `<h1>`–`<h6>` start tags by level in one XHTML.
fn count_headings(xhtml: &str, out: &mut HashMap<u8, usize>) {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = e.local_name();
                let bytes = local.as_ref();
                if bytes.len() == 2
                    && bytes[0] == b'h'
                    && let Some(level) = match bytes[1] {
                        b'1' => Some(1),
                        b'2' => Some(2),
                        b'3' => Some(3),
                        b'4' => Some(4),
                        b'5' => Some(5),
                        b'6' => Some(6),
                        _ => None,
                    }
                {
                    *out.entry(level).or_insert(0) += 1;
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

#[derive(Debug, Default)]
struct KfxNav {
    /// Count of leaf nav_units per heading level. Computed at the end of
    /// `extract_kfx_nav` from `heading_targets`/`heading_target_levels`,
    /// EXCLUDING targets that point at an image element (`type: image`): calibre
    /// and boko both promote only `<div>`→`<hN>`, never `<img>`, so an
    /// image-typed heading nav entry is a valid Kindle heading-jump target but
    /// is never an `<hN>` in the EPUB. Counting it would over-state the expected
    /// heading count and false-fail every book that decorates chapter titles
    /// with a heading-level image.
    headings_by_level: HashMap<u8, usize>,
    /// Every nav_unit target_position inside the headings container.
    heading_targets: Vec<NavTarget>,
    /// Heading level (2..=6) parallel to `heading_targets`, in the same order.
    heading_target_levels: Vec<u8>,
    /// Element IDs of `type: image` content structs — excluded from the heading
    /// count (see `headings_by_level`).
    image_element_ids: HashSet<u64>,
    /// Every nav_unit target_position inside the toc container.
    toc_targets: Vec<NavTarget>,
    /// Element IDs present in storylines (for reachability check).
    element_ids: HashSet<u64>,
    /// The `cover_page` landmark's target id, if any — a signal that the book
    /// has a cover, so boko's synthesized leading 表紙 TOC entry is expected and
    /// should be discounted from the source-count comparison. (The TOC entry
    /// itself targets the cover's first *content* id, a real reachable storyline
    /// element, so it needs no reachability exemption — unlike the landmark's
    /// page-template target.)
    cover_target: Option<u64>,
}

fn extract_kfx_nav(kfx_bytes: &[u8]) -> Result<KfxNav, String> {
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

    let book_nav_type = KfxSymbol::BookNavigation as u32;
    let storyline_type = KfxSymbol::Storyline as u32;
    let nav_container_type = KfxSymbol::NavContainer as u32;

    let mut nav = KfxNav::default();

    // Index every nav_container ($391) entity by its name first, so
    // book_navigation's *referenced* (symbol) form — required by fixed-layout /
    // PDOC books — resolves to the real container instead of counting zero.
    let mut nav_container_by_name: HashMap<String, IonValue> = HashMap::new();
    for ent in &entities {
        if ent.type_id == nav_container_type
            && let Some(value) = parse_entity(kfx_bytes, ent)
        {
            nav_container_by_name.insert(resolve_sym(ent.id as u64), value);
        }
    }

    for ent in &entities {
        if ent.type_id == book_nav_type {
            if let Some(value) = parse_entity(kfx_bytes, ent) {
                extract_from_book_nav(&value, &resolve_sym, &nav_container_by_name, &mut nav);
            }
        } else if ent.type_id == storyline_type
            && let Some(value) = parse_entity(kfx_bytes, ent) {
                collect_element_ids(
                    &value,
                    &resolve_sym,
                    &mut nav.element_ids,
                    &mut nav.image_element_ids,
                );
            }
    }

    // Heading count per level, now that both the nav targets and the image
    // element ids are known: count each heading target EXCEPT those pointing at
    // an image element (never an `<hN>` in the EPUB; see `headings_by_level`).
    let mut headings_by_level: HashMap<u8, usize> = HashMap::new();
    for (target, level) in nav.heading_targets.iter().zip(&nav.heading_target_levels) {
        if !nav.image_element_ids.contains(&target.element_id) {
            *headings_by_level.entry(*level).or_insert(0) += 1;
        }
    }
    nav.headings_by_level = headings_by_level;

    Ok(nav)
}

fn parse_entity(data: &[u8], ent: &crate::kfx::container::EntityLoc) -> Option<IonValue> {
    if ent.offset + ent.length > data.len() {
        return None;
    }
    let entity = &data[ent.offset..ent.offset + ent.length];
    let ion = skip_enty_header(entity);
    IonParser::new(ion).parse().ok()
}

/// Walk the book_navigation value: a list of reading_orders, each with
/// `nav_containers` (list of {nav_type, entries}). Pick the headings and toc
/// containers; for each, descend the `entries` tree counting every nav_unit
/// that carries a `target_position`.
fn extract_from_book_nav<F>(
    value: &IonValue,
    resolve_sym: &F,
    containers_by_name: &HashMap<String, IonValue>,
    out: &mut KfxNav,
) where
    F: Fn(u64) -> String,
{
    // book_navigation is List[ Struct{ reading_order_name, nav_containers } ]
    let inner = match value {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    let IonValue::List(readings) = inner else {
        return;
    };
    for r in readings {
        let r_inner = match r {
            IonValue::Annotated(_, b) => b.as_ref(),
            v => v,
        };
        let IonValue::Struct(fields) = r_inner else {
            continue;
        };
        for (k, v) in fields {
            if resolve_sym(*k) == "nav_containers"
                && let IonValue::List(containers) = v
            {
                for c in containers {
                    extract_from_nav_container(c, resolve_sym, containers_by_name, out);
                }
            }
        }
    }
}

fn extract_from_nav_container<F>(
    value: &IonValue,
    resolve_sym: &F,
    containers_by_name: &HashMap<String, IonValue>,
    out: &mut KfxNav,
) where
    F: Fn(u64) -> String,
{
    let unwrapped = match value {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    // Referenced form: a symbol naming a separate nav_container entity (the
    // fixed-layout / PDOC shape). Resolve it to that entity; inline structs pass
    // through unchanged.
    let container = match unwrapped {
        IonValue::Symbol(s) => match containers_by_name.get(&resolve_sym(*s)) {
            Some(v) => v,
            None => return,
        },
        other => other,
    };
    let inner = match container {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    let IonValue::Struct(fields) = inner else {
        return;
    };

    let mut nav_type = String::new();
    let mut entries: Option<&Vec<IonValue>> = None;
    for (k, v) in fields {
        match resolve_sym(*k).as_str() {
            "nav_type" => {
                if let IonValue::Symbol(s) = v {
                    nav_type = resolve_sym(*s);
                }
            }
            "entries" => {
                if let IonValue::List(items) = v {
                    entries = Some(items);
                }
            }
            _ => {}
        }
    }

    let Some(entries) = entries else { return };

    match nav_type.as_str() {
        "$headings" | "headings" => {
            // Per-level entries: each top-level nav_unit has a `landmark_type`
            // ($h2..$h6) and nested `entries` for individual headings of that
            // level. We count the nested entries (the actual headings).
            for level_unit in entries {
                walk_heading_level_unit(level_unit, resolve_sym, out);
            }
        }
        "$toc" | "toc" => {
            // TOC: flat-traverse all nav_units; count every node with a
            // target_position. Hierarchy is preserved in nested `entries`.
            for u in entries {
                walk_toc_unit(u, resolve_sym, out);
            }
        }
        "$landmarks" | "landmarks" => {
            // Only the cover_page target is needed (to know the book has a cover,
            // so the synthesized 表紙 TOC entry is expected); the rest are out of
            // scope here.
            for u in entries {
                record_cover_landmark(u, resolve_sym, out);
            }
        }
        _ => {}
    }
}

/// Record the `cover_page` landmark's target id — a signal that boko prepended a
/// synthesized cover (表紙) TOC entry, so the TOC-count comparison can discount it.
fn record_cover_landmark<F>(value: &IonValue, resolve_sym: &F, out: &mut KfxNav)
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    let IonValue::Struct(fields) = inner else {
        return;
    };
    let is_cover = fields.iter().any(|(k, v)| {
        resolve_sym(*k) == "landmark_type"
            && matches!(v, IonValue::Symbol(s) if {
                let n = resolve_sym(*s);
                n == "cover_page" || n == "$cover_page"
            })
    });
    if is_cover
        && let Some(target) = extract_target_position(value, resolve_sym)
    {
        out.cover_target = Some(target.element_id);
    }
}

fn walk_heading_level_unit<F>(value: &IonValue, resolve_sym: &F, out: &mut KfxNav)
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    let IonValue::Struct(fields) = inner else {
        return;
    };

    let mut level: Option<u8> = None;
    let mut nested: Option<&Vec<IonValue>> = None;
    for (k, v) in fields {
        match resolve_sym(*k).as_str() {
            "landmark_type" => {
                if let IonValue::Symbol(s) = v {
                    let name = resolve_sym(*s);
                    level = match name.as_str() {
                        "$h2" | "h2" => Some(2),
                        "$h3" | "h3" => Some(3),
                        "$h4" | "h4" => Some(4),
                        "$h5" | "h5" => Some(5),
                        "$h6" | "h6" => Some(6),
                        _ => None,
                    };
                }
            }
            "entries" => {
                if let IonValue::List(items) = v {
                    nested = Some(items);
                }
            }
            _ => {}
        }
    }

    let Some(level) = level else { return };
    let Some(nested) = nested else { return };

    for unit in nested {
        if let Some(target) = extract_target_position(unit, resolve_sym) {
            out.heading_targets.push(target);
            out.heading_target_levels.push(level);
        }
    }
}

fn walk_toc_unit<F>(value: &IonValue, resolve_sym: &F, out: &mut KfxNav)
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    let IonValue::Struct(fields) = inner else {
        return;
    };
    if let Some(target) = extract_target_position(value, resolve_sym) {
        out.toc_targets.push(target);
    }
    for (k, v) in fields {
        if resolve_sym(*k) == "entries"
            && let IonValue::List(children) = v
        {
            for c in children {
                walk_toc_unit(c, resolve_sym, out);
            }
        }
    }
}

fn extract_target_position<F>(value: &IonValue, resolve_sym: &F) -> Option<NavTarget>
where
    F: Fn(u64) -> String,
{
    let inner = match value {
        IonValue::Annotated(_, b) => b.as_ref(),
        v => v,
    };
    let IonValue::Struct(fields) = inner else {
        return None;
    };
    for (k, v) in fields {
        if resolve_sym(*k) == "target_position"
            && let IonValue::Struct(pos_fields) = v
        {
            let mut id: Option<u64> = None;
            let mut offset: u64 = 0;
            for (pk, pv) in pos_fields {
                match resolve_sym(*pk).as_str() {
                    "id" => {
                        if let IonValue::Int(n) = pv
                            && *n >= 0
                        {
                            id = Some(*n as u64);
                        }
                    }
                    "offset" => {
                        if let IonValue::Int(n) = pv
                            && *n >= 0
                        {
                            offset = *n as u64;
                        }
                    }
                    _ => {}
                }
            }
            return id.map(|id| NavTarget { element_id: id, offset });
        }
    }
    None
}

fn collect_element_ids<F>(
    value: &IonValue,
    resolve_sym: &F,
    out: &mut HashSet<u64>,
    image_ids: &mut HashSet<u64>,
) where
    F: Fn(u64) -> String,
{
    match value {
        IonValue::Struct(fields) => {
            // Capture this struct's own id + whether it is an image content
            // element (`type: image`), so an image-typed heading nav target can
            // be excluded from the heading count.
            let mut this_id: Option<u64> = None;
            let mut is_image = false;
            for (k, v) in fields {
                match resolve_sym(*k).as_str() {
                    "id" => {
                        if let IonValue::Int(n) = v
                            && *n >= 0
                        {
                            this_id = Some(*n as u64);
                            out.insert(*n as u64);
                        }
                    }
                    "type" => {
                        if let IonValue::Symbol(s) = v
                            && resolve_sym(*s) == "image"
                        {
                            is_image = true;
                        }
                    }
                    _ => {}
                }
                collect_element_ids(v, resolve_sym, out, image_ids);
            }
            if is_image
                && let Some(id) = this_id
            {
                image_ids.insert(id);
            }
        }
        IonValue::List(items) => {
            for item in items {
                collect_element_ids(item, resolve_sym, out, image_ids);
            }
        }
        IonValue::Annotated(_, inner) => {
            collect_element_ids(inner, resolve_sym, out, image_ids);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_headings_by_level() {
        let xhtml = "<html><body><h1>A</h1><h2>B</h2><h2>C</h2><h3>D</h3></body></html>";
        let mut out = HashMap::new();
        count_headings(xhtml, &mut out);
        assert_eq!(out.get(&1), Some(&1));
        assert_eq!(out.get(&2), Some(&2));
        assert_eq!(out.get(&3), Some(&1));
    }

    #[test]
    fn counts_toc_entries_recursively() {
        let entries = vec![
            TocEntry {
                title: "A".into(),
                href: "a".into(),
                target: None,
                play_order: None,
                children: vec![
                    TocEntry {
                        title: "A.1".into(),
                        href: "a1".into(),
                        target: None,
                        play_order: None,
                        children: vec![],
                    },
                    TocEntry {
                        title: "A.2".into(),
                        href: "a2".into(),
                        target: None,
                        play_order: None,
                        children: vec![],
                    },
                ],
            },
            TocEntry {
                title: "B".into(),
                href: "b".into(),
                target: None,
                play_order: None,
                children: vec![],
            },
        ];
        assert_eq!(count_toc_entries(&entries), 4);
    }
}
