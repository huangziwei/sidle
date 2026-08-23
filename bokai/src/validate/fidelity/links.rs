//! Link-preservation validation — verify that every `<a href>` in the source
//! EPUB has a corresponding link target in the converted KFX.
//!
//! Source-side: walk each spine XHTML, collect `<a href>` values plus every
//! `id` attribute, which tells an internal `#frag` with a source-side target
//! from one without. Source-side dangling hrefs are reported separately from
//! KFX-side defects.
//!
//! KFX-side: enumerate Anchor entities (type 266). Each has an `anchor_name`
//! plus either a `uri` (external link, e.g. `https://…`) or a `position`
//! (internal link, with `id` = content fragment ID + optional `offset`).
//! Then walk every Storyline, collect `link_to` references from `style_events`.
//!
//! Checks performed:
//!
//! 1. **External round-trip** — every distinct `<a href>` URL with an http/
//!    https/mailto/… scheme should appear as the `uri` of some KFX Anchor.
//!    This is an exact-string match: source `href="https://example.com"`
//!    must produce KFX `uri: "https://example.com"`.
//! 2. **Dangling KFX anchors** — an Anchor whose `position.id` points to a
//!    content fragment that doesn't exist in the entity index. The Kindle
//!    reader taps to nowhere on these.
//! 3. **Orphan KFX link_to** — a `style_event.link_to` symbol with no
//!    matching `anchor_name` on any Anchor entity. The link is dead in KFX.
//! 4. **Source-side dangling refs** — a `<a href="#frag">` (or
//!    `path#frag`) whose fragment has no `id="frag"` anywhere in the source.
//!    Reported for visibility; not counted as a bokai bug.

use crate::formats::epub::structure::resolve_href;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::formats::epub::{parse_container_xml, parse_opf};
use crate::formats::kfx::container::{
    SymbolTable, parse_container_header, parse_container_info, parse_entity, parse_index_table,
    slice_at,
};
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::symbols::KfxSymbol;

/// Classification of an `<a href>` from the source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HrefKind {
    /// Absolute URL with non-relative scheme: http/https/mailto/tel/ftp/…
    External(String),
    /// Relative href with a fragment: `chapter.xhtml#sec` or just `#sec`.
    InternalFragment {
        /// Path component (empty if href starts with `#`).
        path: String,
        /// Fragment after `#`.
        fragment: String,
    },
    /// Relative href without fragment: `chapter.xhtml` (chapter-start jump).
    InternalChapter(String),
    /// Couldn't classify (empty href, malformed, etc.).
    Unclassifiable(String),
}

/// A single `<a href>` occurrence from the EPUB side. Spine order preserved.
#[derive(Debug, Clone)]
pub struct EpubHref {
    /// Spine path the link came from (for diagnostics).
    pub spine_path: String,
    /// Raw href value as written.
    pub raw: String,
    pub kind: HrefKind,
}

/// What a KFX Anchor entity resolves to.
#[derive(Debug, Clone)]
pub enum AnchorResolution {
    /// External URL — `uri` field is set.
    External(String),
    /// Internal target — `position` field is set.
    Internal {
        /// Content fragment ID.
        fragment_id: u64,
        /// Byte offset within the fragment (0 for fragment-start).
        offset: u64,
    },
    /// Neither uri nor position — malformed.
    Empty,
}

/// One KFX Anchor entity.
#[derive(Debug, Clone)]
pub struct KfxAnchor {
    pub anchor_name: String,
    pub resolution: AnchorResolution,
}

/// A KFX Anchor with `position.id` pointing at a fragment not in the index.
#[derive(Debug, Clone)]
pub struct DanglingAnchor {
    pub anchor_name: String,
    pub fragment_id: u64,
}

#[derive(Debug, Default)]
pub struct Report {
    pub epub_hrefs: Vec<EpubHref>,
    pub kfx_anchors: Vec<KfxAnchor>,

    // --- EPUB-side counts ---
    pub epub_external_count: usize,
    pub epub_internal_fragment_count: usize,
    pub epub_internal_chapter_count: usize,
    /// EPUB-side dangling refs: `<a href="#x">` with no element having `id="x"`.
    /// In EPUB→KFX, this is source data quality (not bokai's fault). In
    /// KFX→EPUB, this is a defect in bokai's EPUB output.
    pub epub_dangling_refs: Vec<EpubHref>,

    // --- KFX-side counts ---
    /// Number of Anchor entities with `uri` (external).
    pub kfx_external_anchor_count: usize,
    /// Number of Anchor entities with `position` (internal).
    pub kfx_internal_anchor_count: usize,
    /// Distinct `link_to` symbols seen across all style_events.
    pub kfx_link_to_distinct: usize,
    /// Total `link_to` references (one per style_event with link_to).
    pub kfx_link_to_total: usize,

    // --- Defects (multiset diff) ---
    /// External URLs present in EPUB but not in KFX.
    pub external_only_in_epub: Vec<(String, usize)>,
    /// External URLs present in KFX but not in EPUB.
    pub external_only_in_kfx: Vec<(String, usize)>,

    // --- Defects (KFX-side internal) ---
    /// KFX Anchor entities whose `position.id` doesn't resolve to a real
    /// content fragment — link goes nowhere on Kindle.
    pub dangling_anchors: Vec<DanglingAnchor>,
    /// `style_event.link_to` symbols with no matching `anchor_name`.
    pub orphan_link_tos: Vec<String>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        // Direction-agnostic core. `dangling_anchors` (a KFX anchor pointing at a
        // missing KFX element) is deliberately NOT here — it's side-specific and
        // gated by `is_clean_for`: it's a defect only when bokai produced the KFX.
        self.external_only_in_epub.is_empty()
            && self.external_only_in_kfx.is_empty()
            && self.orphan_link_tos.is_empty()
    }

    /// Direction-aware gate on the two dangling classes, each counted only in
    /// the direction that produced the side carrying it: an EPUB-side dangling
    /// ref in KFX→EPUB, a KFX-side dangling anchor in EPUB→KFX.
    pub fn is_clean_for(&self, dir: super::Direction) -> bool {
        self.is_clean()
            && (dir.epub_is_source() || self.epub_dangling_refs.is_empty())
            && (!dir.epub_is_source() || self.dangling_anchors.is_empty())
    }

    pub fn print_summary(&self, dir: super::Direction) {
        let epub_is_src = dir.epub_is_source();
        println!("EPUB <a href>:");
        println!("  external:   {}", self.epub_external_count);
        println!("  internal #: {}", self.epub_internal_fragment_count);
        println!("  chapter:    {}", self.epub_internal_chapter_count);
        if epub_is_src {
            println!(
                "  dangling:   {} (source-side, not bokai's fault)",
                self.epub_dangling_refs.len()
            );
        } else {
            println!(
                "  dangling:   {} (bokai's EPUB output)",
                self.epub_dangling_refs.len()
            );
        }
        println!("KFX anchors:");
        println!("  external (uri):      {}", self.kfx_external_anchor_count);
        println!("  internal (position): {}", self.kfx_internal_anchor_count);
        println!(
            "  link_to refs:        {} ({} distinct)",
            self.kfx_link_to_total, self.kfx_link_to_distinct
        );
        let (dropped, dropped_count, fabricated, fabricated_count) = if epub_is_src {
            // EPUB→KFX: EPUB-only = dropped in KFX; KFX-only = fabricated.
            (
                &self.external_only_in_epub,
                self.external_only_in_epub
                    .iter()
                    .map(|(_, n)| n)
                    .sum::<usize>(),
                &self.external_only_in_kfx,
                self.external_only_in_kfx
                    .iter()
                    .map(|(_, n)| n)
                    .sum::<usize>(),
            )
        } else {
            (
                &self.external_only_in_kfx,
                self.external_only_in_kfx
                    .iter()
                    .map(|(_, n)| n)
                    .sum::<usize>(),
                &self.external_only_in_epub,
                self.external_only_in_epub
                    .iter()
                    .map(|(_, n)| n)
                    .sum::<usize>(),
            )
        };
        println!("Defects:");
        println!(
            "  external URLs dropped (missing in {}):  {} ({} unique)",
            dir.target_label(),
            dropped_count,
            dropped.len()
        );
        println!(
            "  external URLs fabricated (extra in {}): {} ({} unique)",
            dir.target_label(),
            fabricated_count,
            fabricated.len()
        );
        println!(
            "  dangling KFX anchors:         {}",
            self.dangling_anchors.len()
        );
        println!(
            "  orphan link_to symbols:       {}",
            self.orphan_link_tos.len()
        );
    }

    pub fn print_details(&self, limit: usize, dir: super::Direction) {
        let (dropped, fabricated) = if dir.epub_is_source() {
            (&self.external_only_in_epub, &self.external_only_in_kfx)
        } else {
            (&self.external_only_in_kfx, &self.external_only_in_epub)
        };
        if !dropped.is_empty() {
            println!(
                "\n--- External URLs in {} not in {} [first {}] ---",
                dir.source_label(),
                dir.target_label(),
                limit
            );
            for (url, n) in dropped.iter().take(limit) {
                println!("  ({}×)  {}", n, url);
            }
            if dropped.len() > limit {
                println!("  ... and {} more", dropped.len() - limit);
            }
        }
        if !fabricated.is_empty() {
            println!(
                "\n--- External URLs in {} not in {} [first {}] ---",
                dir.target_label(),
                dir.source_label(),
                limit
            );
            for (url, n) in fabricated.iter().take(limit) {
                println!("  ({}×)  {}", n, url);
            }
            if fabricated.len() > limit {
                println!("  ... and {} more", fabricated.len() - limit);
            }
        }
        if !self.dangling_anchors.is_empty() {
            println!(
                "\n--- KFX anchors pointing at missing fragments [first {}] ---",
                limit
            );
            for d in self.dangling_anchors.iter().take(limit) {
                println!("  {}  →  fragment_id {}", d.anchor_name, d.fragment_id);
            }
            if self.dangling_anchors.len() > limit {
                println!("  ... and {} more", self.dangling_anchors.len() - limit);
            }
        }
        if !self.orphan_link_tos.is_empty() {
            println!(
                "\n--- KFX link_to refs with no defining Anchor [first {}] ---",
                limit
            );
            for sym in self.orphan_link_tos.iter().take(limit) {
                println!("  {}", sym);
            }
            if self.orphan_link_tos.len() > limit {
                println!("  ... and {} more", self.orphan_link_tos.len() - limit);
            }
        }
        if !self.epub_dangling_refs.is_empty() {
            let header = if dir.epub_is_source() {
                "Source-side dangling EPUB hrefs (not bokai's fault)"
            } else {
                "Dangling hrefs in bokai's EPUB output"
            };
            println!("\n--- {} [first {}] ---", header, limit);
            for r in self.epub_dangling_refs.iter().take(limit) {
                println!("  {}  →  {}", r.spine_path, r.raw);
            }
            if self.epub_dangling_refs.len() > limit {
                println!("  ... and {} more", self.epub_dangling_refs.len() - limit);
            }
        }
    }
}

/// Validate links across both sides. Direction-neutral — caller interprets
/// the resulting fields per conversion direction.
pub fn validate(epub_bytes: &[u8], kfx_bytes: &[u8]) -> Result<Report, String> {
    let (epub_hrefs, epub_ids) = extract_hrefs_and_ids_from_epub(epub_bytes)?;
    let kfx = extract_anchors_and_link_tos_from_kfx(kfx_bytes)?;

    // --- EPUB-side counts ---
    let mut epub_external: HashMap<String, usize> = HashMap::new();
    let mut epub_external_count = 0;
    let mut epub_internal_fragment_count = 0;
    let mut epub_internal_chapter_count = 0;
    let mut epub_dangling_refs: Vec<EpubHref> = Vec::new();

    for href in &epub_hrefs {
        match &href.kind {
            HrefKind::External(url) => {
                epub_external_count += 1;
                *epub_external.entry(url.clone()).or_insert(0) += 1;
            }
            HrefKind::InternalFragment { path, fragment } => {
                epub_internal_fragment_count += 1;
                // Resolve the target spine: if path is empty, same file; else
                // join against the link's own spine_path.
                let target_path = if path.is_empty() {
                    href.spine_path.clone()
                } else {
                    resolve_relative(&href.spine_path, path)
                };
                let target_key = format!("{}#{}", target_path, fragment);
                let target_key_filename_only =
                    filename_only(&target_path).map(|f| format!("{}#{}", f, fragment));
                let has_target = epub_ids.contains(&target_key)
                    || target_key_filename_only
                        .as_ref()
                        .is_some_and(|k| epub_ids.contains(k));
                if !has_target {
                    epub_dangling_refs.push(href.clone());
                }
            }
            HrefKind::InternalChapter(_) => {
                epub_internal_chapter_count += 1;
            }
            HrefKind::Unclassifiable(_) => {}
        }
    }

    // --- KFX-side counts and indexes ---
    let mut kfx_external_anchor_count = 0;
    let mut kfx_internal_anchor_count = 0;
    let mut kfx_external_uris: HashMap<String, usize> = HashMap::new();
    let mut anchor_names: HashSet<String> = HashSet::new();
    let mut dangling_anchors: Vec<DanglingAnchor> = Vec::new();

    for a in &kfx.anchors {
        anchor_names.insert(a.anchor_name.clone());
        match &a.resolution {
            AnchorResolution::External(uri) => {
                kfx_external_anchor_count += 1;
                *kfx_external_uris.entry(uri.clone()).or_insert(0) += 1;
            }
            AnchorResolution::Internal { fragment_id, .. } => {
                kfx_internal_anchor_count += 1;
                if !kfx.element_ids.contains(fragment_id) {
                    dangling_anchors.push(DanglingAnchor {
                        anchor_name: a.anchor_name.clone(),
                        fragment_id: *fragment_id,
                    });
                }
            }
            AnchorResolution::Empty => {}
        }
    }

    // --- Orphan link_to refs ---
    let mut orphan_link_tos: Vec<String> = kfx
        .link_to_distinct
        .iter()
        .filter(|name| !anchor_names.contains(*name))
        .cloned()
        .collect();
    orphan_link_tos.sort();

    // --- External URL multiset diff ---
    let mut external_only_in_epub: Vec<(String, usize)> = Vec::new();
    let mut external_only_in_kfx: Vec<(String, usize)> = Vec::new();
    for (url, ec) in &epub_external {
        let kc = kfx_external_uris.get(url).copied().unwrap_or(0);
        if ec > &kc {
            external_only_in_epub.push((url.clone(), ec - kc));
        }
    }
    for (url, kc) in &kfx_external_uris {
        let ec = epub_external.get(url).copied().unwrap_or(0);
        if kc > &ec {
            external_only_in_kfx.push((url.clone(), kc - ec));
        }
    }
    external_only_in_epub.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    external_only_in_kfx.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    Ok(Report {
        epub_hrefs,
        kfx_anchors: kfx.anchors,
        epub_external_count,
        epub_internal_fragment_count,
        epub_internal_chapter_count,
        epub_dangling_refs,
        kfx_external_anchor_count,
        kfx_internal_anchor_count,
        kfx_link_to_distinct: kfx.link_to_distinct.len(),
        kfx_link_to_total: kfx.link_to_total,
        external_only_in_epub,
        external_only_in_kfx,
        dangling_anchors,
        orphan_link_tos,
    })
}

// ============================================================================
// Source-side extraction
// ============================================================================

/// Walk every spine XHTML and return all `<a href>` occurrences plus the set
/// of link targets defined anywhere (`id` attributes keyed as
/// `path#id` and also `filename#id` to handle relative path variants).
pub fn extract_hrefs_and_ids_from_epub(
    epub_bytes: &[u8],
) -> Result<(Vec<EpubHref>, HashSet<String>), String> {
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

    let mut hrefs = Vec::new();
    let mut ids: HashSet<String> = HashSet::new();
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
        extract_from_xhtml(&xhtml, &full_path, &mut hrefs, &mut ids);
    }
    Ok((hrefs, ids))
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

/// Walk one XHTML and collect `<a href>` occurrences and `id` attributes from
/// every element. `id` is recorded as `spine_path#id` AND `filename#id` so
/// the validator can resolve hrefs written with either form.
pub fn extract_from_xhtml(
    xhtml: &str,
    spine_path: &str,
    hrefs_out: &mut Vec<EpubHref>,
    ids_out: &mut HashSet<String>,
) {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);

    let file = filename_only(spine_path);

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let local = e.local_name();
                let is_anchor = local.as_ref() == b"a";
                let mut href_val: Option<String> = None;
                let mut id_val: Option<String> = None;

                for attr in e.attributes().flatten() {
                    match attr.key.local_name().as_ref() {
                        b"href" if is_anchor => {
                            href_val = Some(String::from_utf8_lossy(&attr.value).into_owned());
                        }
                        b"id" => {
                            id_val = Some(String::from_utf8_lossy(&attr.value).into_owned());
                        }
                        _ => {}
                    }
                }

                if let Some(raw) = href_val {
                    let kind = classify_href(&raw);
                    hrefs_out.push(EpubHref {
                        spine_path: spine_path.to_string(),
                        raw,
                        kind,
                    });
                }
                if let Some(id) = id_val {
                    ids_out.insert(format!("{}#{}", spine_path, id));
                    if let Some(f) = file.as_deref() {
                        ids_out.insert(format!("{}#{}", f, id));
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

/// Classify a raw href value.
pub fn classify_href(raw: &str) -> HrefKind {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return HrefKind::Unclassifiable(raw.to_string());
    }

    // Fragment-only: same-file link.
    if let Some(frag) = trimmed.strip_prefix('#') {
        return HrefKind::InternalFragment {
            path: String::new(),
            fragment: frag.to_string(),
        };
    }

    // Detect scheme: `scheme:rest` where scheme starts with a letter and
    // contains only [A-Za-z0-9+.-]. This catches http, https, mailto, tel,
    // ftp, data, javascript, etc. — all "external" from KFX's perspective.
    if has_scheme(trimmed) {
        // EPUB hrefs are XML-escaped (`&amp;`), KFX uris raw (`&`). Unescaping
        // compares logical URLs, and is idempotent on a URL with no entities.
        let url = quick_xml::escape::unescape(trimmed)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| trimmed.to_string());
        return HrefKind::External(url);
    }

    // Relative ref with optional fragment.
    if let Some((path, fragment)) = trimmed.split_once('#') {
        return HrefKind::InternalFragment {
            path: path.to_string(),
            fragment: fragment.to_string(),
        };
    }

    HrefKind::InternalChapter(trimmed.to_string())
}

fn has_scheme(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        if b == b':' {
            // At least one char precedes the colon.
            return i >= 1;
        }
        let ok = b.is_ascii_alphanumeric() || b == b'+' || b == b'.' || b == b'-';
        if !ok {
            return false;
        }
    }
    false
}

/// Resolve a relative href path against the current spine path.
/// Example: `("OEBPS/text/ch1.xhtml", "ch2.xhtml")` → `"OEBPS/text/ch2.xhtml"`.
fn resolve_relative(spine_path: &str, href_path: &str) -> String {
    let base = match spine_path.rfind('/') {
        Some(i) => &spine_path[..=i],
        None => "",
    };
    let combined = format!("{}{}", base, href_path);
    normalize_path(&combined)
}

fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let mut out = parts.join("/");
    if path.starts_with('/') {
        out.insert(0, '/');
    }
    out
}

fn filename_only(path: &str) -> Option<String> {
    path.rsplit('/').next().map(|s| s.to_string())
}

// ============================================================================
// KFX-side extraction
// ============================================================================

#[derive(Debug, Default)]
pub struct KfxLinkData {
    pub anchors: Vec<KfxAnchor>,
    /// Distinct link_to symbol names seen across all style_events.
    pub link_to_distinct: HashSet<String>,
    /// Total link_to references (one per style_event with link_to).
    pub link_to_total: usize,
    /// Element-level `id` values anywhere inside storyline entities. An anchor
    /// `position.id` resolves to one of these, never to a top-level container
    /// entity id.
    pub element_ids: HashSet<u64>,
}

pub fn extract_anchors_and_link_tos_from_kfx(kfx_bytes: &[u8]) -> Result<KfxLinkData, String> {
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

    let anchor_type = KfxSymbol::Anchor as u32;
    let storyline_type = KfxSymbol::Storyline as u32;

    let mut anchors: Vec<KfxAnchor> = Vec::new();
    let mut element_ids: HashSet<u64> = HashSet::new();
    let mut link_to_distinct: HashSet<String> = HashSet::new();
    let mut link_to_total: usize = 0;

    for ent in &entities {
        if ent.type_id == anchor_type {
            if let Some(value) = parse_entity(kfx_bytes, ent)
                && let Some(a) = extract_anchor(&value, &resolve_sym)
            {
                anchors.push(a);
            }
        } else if ent.type_id == storyline_type
            && let Some(value) = parse_entity(kfx_bytes, ent)
        {
            walk_storyline(
                &value,
                &resolve_sym,
                &mut link_to_distinct,
                &mut link_to_total,
                &mut element_ids,
            );
        }
    }

    Ok(KfxLinkData {
        anchors,
        link_to_distinct,
        link_to_total,
        element_ids,
    })
}

fn extract_anchor<F>(value: &IonValue, resolve_sym: &F) -> Option<KfxAnchor>
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

    let mut anchor_name = String::new();
    let mut uri: Option<String> = None;
    let mut position_fragment_id: Option<u64> = None;
    let mut position_offset: u64 = 0;

    for (k, v) in fields {
        match resolve_sym(*k).as_str() {
            "anchor_name" => {
                if let IonValue::Symbol(s) = v {
                    anchor_name = resolve_sym(*s);
                }
            }
            "uri" => {
                if let IonValue::String(s) = v {
                    uri = Some(s.clone());
                }
            }
            "position" => {
                if let IonValue::Struct(pfields) = v {
                    for (pk, pv) in pfields {
                        match resolve_sym(*pk).as_str() {
                            "id" => {
                                if let IonValue::Int(n) = pv {
                                    position_fragment_id = Some(*n as u64);
                                }
                            }
                            "offset" => {
                                if let IonValue::Int(n) = pv {
                                    position_offset = *n as u64;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if anchor_name.is_empty() {
        return None;
    }

    let resolution = if let Some(u) = uri {
        AnchorResolution::External(u)
    } else if let Some(id) = position_fragment_id {
        AnchorResolution::Internal {
            fragment_id: id,
            offset: position_offset,
        }
    } else {
        AnchorResolution::Empty
    };

    Some(KfxAnchor {
        anchor_name,
        resolution,
    })
}

/// Walk a storyline's Ion tree once, collecting both `link_to` references (in
/// style_events) and element-level `id` integers (every element struct that
/// has an `id` field — these are the targets anchor `position.id` resolves to).
fn walk_storyline<F>(
    value: &IonValue,
    resolve_sym: &F,
    link_to_distinct: &mut HashSet<String>,
    link_to_total: &mut usize,
    element_ids: &mut HashSet<u64>,
) where
    F: Fn(u64) -> String,
{
    match value {
        IonValue::Struct(fields) => {
            for (k, v) in fields {
                match resolve_sym(*k).as_str() {
                    "link_to" => {
                        if let IonValue::Symbol(s) = v {
                            let name = resolve_sym(*s);
                            if !name.is_empty() {
                                *link_to_total += 1;
                                link_to_distinct.insert(name);
                            }
                        }
                    }
                    "id" => {
                        if let IonValue::Int(n) = v
                            && *n >= 0
                        {
                            element_ids.insert(*n as u64);
                        }
                    }
                    _ => {}
                }
                walk_storyline(v, resolve_sym, link_to_distinct, link_to_total, element_ids);
            }
        }
        IonValue::List(items) => {
            for item in items {
                walk_storyline(
                    item,
                    resolve_sym,
                    link_to_distinct,
                    link_to_total,
                    element_ids,
                );
            }
        }
        IonValue::Annotated(_, inner) => {
            walk_storyline(
                inner,
                resolve_sym,
                link_to_distinct,
                link_to_total,
                element_ids,
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_external() {
        assert_eq!(
            classify_href("https://example.com"),
            HrefKind::External("https://example.com".to_string())
        );
        assert_eq!(
            classify_href("mailto:a@b.c"),
            HrefKind::External("mailto:a@b.c".to_string())
        );
        assert_eq!(
            classify_href("tel:+1-555"),
            HrefKind::External("tel:+1-555".to_string())
        );
    }

    #[test]
    fn classify_internal_fragment_only() {
        assert_eq!(
            classify_href("#foo"),
            HrefKind::InternalFragment {
                path: String::new(),
                fragment: "foo".to_string()
            }
        );
    }

    #[test]
    fn classify_internal_path_fragment() {
        assert_eq!(
            classify_href("ch2.xhtml#sec"),
            HrefKind::InternalFragment {
                path: "ch2.xhtml".to_string(),
                fragment: "sec".to_string()
            }
        );
    }

    #[test]
    fn classify_internal_chapter() {
        assert_eq!(
            classify_href("ch2.xhtml"),
            HrefKind::InternalChapter("ch2.xhtml".to_string())
        );
    }

    #[test]
    fn classify_empty_unclassifiable() {
        match classify_href("") {
            HrefKind::Unclassifiable(_) => {}
            _ => panic!("expected Unclassifiable"),
        }
    }

    #[test]
    fn relative_resolution() {
        assert_eq!(
            resolve_relative("OEBPS/text/ch1.xhtml", "ch2.xhtml"),
            "OEBPS/text/ch2.xhtml"
        );
        assert_eq!(
            resolve_relative("OEBPS/text/ch1.xhtml", "../images/c.png"),
            "OEBPS/images/c.png"
        );
        assert_eq!(resolve_relative("ch1.xhtml", "ch2.xhtml"), "ch2.xhtml");
    }

    #[test]
    fn xhtml_collects_hrefs_and_ids() {
        let xhtml = r##"<html><body>
            <p id="p1">First</p>
            <p>See <a href="#p1">above</a> and <a href="ch2.xhtml#start">next</a>.</p>
            <a href="https://example.com">ext</a>
            <a href="ch3.xhtml">whole chapter</a>
        </body></html>"##;
        let mut hrefs = Vec::new();
        let mut ids = HashSet::new();
        extract_from_xhtml(xhtml, "OEBPS/ch1.xhtml", &mut hrefs, &mut ids);

        assert_eq!(hrefs.len(), 4);
        assert!(
            matches!(hrefs[0].kind, HrefKind::InternalFragment { ref fragment, .. } if fragment == "p1")
        );
        assert!(
            matches!(hrefs[1].kind, HrefKind::InternalFragment { ref path, ref fragment } if path == "ch2.xhtml" && fragment == "start")
        );
        assert!(matches!(hrefs[2].kind, HrefKind::External(ref u) if u == "https://example.com"));
        assert!(matches!(hrefs[3].kind, HrefKind::InternalChapter(ref p) if p == "ch3.xhtml"));

        assert!(ids.contains("OEBPS/ch1.xhtml#p1"));
        assert!(ids.contains("ch1.xhtml#p1"));
    }
}
