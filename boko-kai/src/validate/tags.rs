//! HTML tag coverage validation — report which element names used in the
//! source EPUB get an explicit semantic role in boko's `role_map` versus
//! falling through to the generic-container catch-all.
//!
//! Why this matters: `role_map::element_to_role` returns `Role::Container`
//! for any unrecognised element name. Content still flows through (text
//! leaves are preserved), but no semantics — so `<svg>`, `<math>`,
//! `<details>`-children, or custom-namespaced elements lose their meaning
//! on the Kindle. The validator lists every distinct tag the book uses
//! and buckets them by quality:
//!
//! - **Semantic**: maps to a meaningful role (Paragraph, Heading, Table,
//!   Figure, Link, Image, …) — full handling.
//! - **Generic**: handled explicitly but as a `Container`/`Inline` shell —
//!   no special structure but content flows.
//! - **Fallback**: not in `role_map` at all, silently downgraded to
//!   generic Container. These are the priorities for `role_map.rs`.

use std::collections::HashMap;
use std::io::Cursor;

use html5ever::LocalName;
use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::dom::role_map::element_to_role_known;
use crate::epub::{parse_container_xml, parse_opf};
use crate::model::Role;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bucket {
    /// Explicit mapping to a meaningful role.
    Semantic,
    /// Explicit mapping to a generic container/inline shell.
    Generic,
    /// No mapping — falls through to default Container.
    Fallback,
}

#[derive(Debug, Clone)]
pub struct TagStats {
    pub tag: String,
    pub count: usize,
    pub role: Option<Role>,
    pub bucket: Bucket,
}

#[derive(Debug, Default)]
pub struct Report {
    pub total_elements: usize,
    pub total_unique_tags: usize,
    pub by_bucket: HashMap<Bucket, usize>,
    /// All tags, sorted by count descending.
    pub tags: Vec<TagStats>,
}

impl Report {
    pub fn semantic_ratio(&self) -> f64 {
        if self.total_elements == 0 {
            return 1.0;
        }
        let semantic = self.by_bucket.get(&Bucket::Semantic).copied().unwrap_or(0);
        semantic as f64 / self.total_elements as f64
    }

    pub fn is_clean(&self) -> bool {
        self.by_bucket.get(&Bucket::Fallback).copied().unwrap_or(0) == 0
    }

    pub fn print_summary(&self) {
        let sem = self.by_bucket.get(&Bucket::Semantic).copied().unwrap_or(0);
        let generic = self.by_bucket.get(&Bucket::Generic).copied().unwrap_or(0);
        let fal = self.by_bucket.get(&Bucket::Fallback).copied().unwrap_or(0);
        println!("Total elements: {}", self.total_elements);
        println!("Unique tags:    {}", self.total_unique_tags);
        println!(
            "Semantic:       {} ({:.2}%)",
            sem,
            sem as f64 * 100.0 / self.total_elements.max(1) as f64
        );
        println!(
            "Generic:        {} ({:.2}%)",
            generic,
            generic as f64 * 100.0 / self.total_elements.max(1) as f64
        );
        println!(
            "Fallback:       {} ({:.2}%)",
            fal,
            fal as f64 * 100.0 / self.total_elements.max(1) as f64
        );
    }

    pub fn print_details(&self, limit: usize) {
        let fallback: Vec<&TagStats> =
            self.tags.iter().filter(|t| t.bucket == Bucket::Fallback).collect();
        if !fallback.is_empty() {
            println!(
                "\n--- Fallback tags (no role_map entry) [first {}] ---",
                limit
            );
            for t in fallback.iter().take(limit) {
                println!("  {:>6}×  <{}>", t.count, t.tag);
            }
            if fallback.len() > limit {
                println!("  ... and {} more unique tags", fallback.len() - limit);
            }
        }
        let generic: Vec<&TagStats> =
            self.tags.iter().filter(|t| t.bucket == Bucket::Generic).collect();
        if !generic.is_empty() {
            println!(
                "\n--- Generic-shell tags (Container/Inline only) [first {}] ---",
                limit
            );
            for t in generic.iter().take(limit) {
                println!(
                    "  {:>6}×  <{}>  → {:?}",
                    t.count,
                    t.tag,
                    t.role.unwrap_or(Role::Container)
                );
            }
            if generic.len() > limit {
                println!("  ... and {} more unique tags", generic.len() - limit);
            }
        }
    }
}

pub fn validate(epub_bytes: &[u8]) -> Result<Report, String> {
    let counts = collect_tag_counts(epub_bytes)?;
    let mut tags: Vec<TagStats> = counts
        .into_iter()
        .map(|(tag, count)| {
            let local = LocalName::from(tag.as_str());
            let role = element_to_role_known(&local);
            let bucket = classify(role);
            TagStats { tag, count, role, bucket }
        })
        .collect();
    tags.sort_by(|a, b| b.count.cmp(&a.count).then(a.tag.cmp(&b.tag)));

    let mut by_bucket: HashMap<Bucket, usize> = HashMap::new();
    let mut total = 0;
    for t in &tags {
        total += t.count;
        *by_bucket.entry(t.bucket).or_insert(0) += t.count;
    }

    Ok(Report {
        total_elements: total,
        total_unique_tags: tags.len(),
        by_bucket,
        tags,
    })
}

fn classify(role: Option<Role>) -> Bucket {
    match role {
        None => Bucket::Fallback,
        Some(Role::Container) | Some(Role::Inline) => Bucket::Generic,
        Some(_) => Bucket::Semantic,
    }
}

fn collect_tag_counts(epub_bytes: &[u8]) -> Result<HashMap<String, usize>, String> {
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

    let mut counts: HashMap<String, usize> = HashMap::new();
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
        count_tags_in_xhtml(&xhtml, &mut counts);
    }
    Ok(counts)
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

/// Walk one XHTML doc and count every Start/Empty element by local name.
/// We deliberately do not skip `<head>` here — `<title>`, `<link>`, `<meta>`
/// are real source elements and the role_map should account for them
/// (currently they fall through to Container, which is the right surfacing).
pub fn count_tags_in_xhtml(xhtml: &str, counts: &mut HashMap<String, usize>) {
    let mut reader = Reader::from_str(xhtml);
    reader.config_mut().trim_text(false);

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref())
                    .to_ascii_lowercase();
                *counts.entry(name).or_insert(0) += 1;
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_per_tag() {
        let mut counts = HashMap::new();
        count_tags_in_xhtml(
            "<html><body><p>a</p><p>b</p><div><span>c</span></div></body></html>",
            &mut counts,
        );
        assert_eq!(counts.get("p"), Some(&2));
        assert_eq!(counts.get("div"), Some(&1));
        assert_eq!(counts.get("span"), Some(&1));
        assert_eq!(counts.get("body"), Some(&1));
    }

    #[test]
    fn classify_buckets() {
        assert_eq!(classify(None), Bucket::Fallback);
        assert_eq!(classify(Some(Role::Container)), Bucket::Generic);
        assert_eq!(classify(Some(Role::Inline)), Bucket::Generic);
        assert_eq!(classify(Some(Role::Paragraph)), Bucket::Semantic);
        assert_eq!(classify(Some(Role::Heading(1))), Bucket::Semantic);
        assert_eq!(classify(Some(Role::Table)), Bucket::Semantic);
    }
}
