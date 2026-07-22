//! Fixed-layout (manga / comic) validation — verify that an image-based
//! fixed-layout source KFX produced a conformant pre-paginated EPUB.
//!
//! KFX side: read `content_features` ($585). `yj_*fixed_layout` ⇒ the book is
//! image-based fixed layout; `yj_double_page_spread` ⇒ a spread comic.
//!
//! EPUB side (mirrors what the EPUB export must emit, calibre
//! `epub_output.py:926` + `yj_to_epub_content.py:210`):
//!
//! - `<meta property="rendition:layout">pre-paginated` in the OPF metadata.
//! - A `<meta name="viewport">` in **every** spine document.
//! - `page-spread-left`/`page-spread-right` itemref properties for a spread
//!   comic (so readers pair facing pages).
//! - No orphan images: every manifest image is referenced by a page (manga
//!   ships a full page-thumbnail set the reading order never uses).
//!
//! Gates fire only when the KFX is fixed layout; a reflowable book passes
//! trivially.

use std::collections::HashSet;
use std::io::{Cursor, Read};

use zip::ZipArchive;

use crate::formats::epub::{parse_container_xml, parse_opf};
use crate::formats::kfx::container::get_field;
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::symbols::KfxSymbol;

#[derive(Debug, Default)]
pub struct Report {
    /// KFX `content_features` carries a `yj_*fixed_layout` key.
    pub kfx_fixed_layout: bool,
    /// KFX `content_features` carries `yj_double_page_spread`.
    pub kfx_double_page_spread: bool,

    /// OPF declares `rendition:layout` = `pre-paginated`.
    pub epub_pre_paginated: bool,
    /// Number of XHTML spine documents.
    pub epub_spine_docs: usize,
    /// Spine documents carrying a `<meta name="viewport">`.
    pub epub_docs_with_viewport: usize,
    /// `page-spread-left` + `page-spread-right` itemref properties in the spine.
    pub epub_page_spread_props: usize,
    /// OPF `original-resolution` meta content (e.g. `900x1280`), if present.
    pub epub_original_resolution: Option<String>,
    /// OPF `book-type` meta content (e.g. `comic`), if present.
    pub epub_book_type: Option<String>,
    /// `primary-writing-mode` meta content (e.g. `horizontal-rl`), if present.
    pub epub_primary_writing_mode: Option<String>,
    /// Manifest image resources.
    pub epub_manifest_images: usize,
    /// Manifest image resources referenced by at least one spine `<img src>`.
    pub epub_referenced_images: usize,
}

impl Report {
    /// Clean iff: the EPUB matches the KFX's fixed-layout shape. A reflowable
    /// source (not fixed layout) passes trivially. For a fixed-layout source we
    /// require pre-paginated layout, a viewport on every page, no orphan images,
    /// and — for a spread comic — at least one page-spread property.
    pub fn is_clean(&self) -> bool {
        if !self.kfx_fixed_layout {
            return true;
        }
        self.epub_pre_paginated
            && self.epub_spine_docs > 0
            && self.epub_docs_with_viewport == self.epub_spine_docs
            && self.epub_manifest_images == self.epub_referenced_images
            && (!self.kfx_double_page_spread || self.epub_page_spread_props > 0)
    }

    pub fn print_summary(&self, dir: super::Direction) {
        println!("Fixed layout (manga / comic):");
        println!(
            "  KFX:  fixed_layout={} double_page_spread={}",
            self.kfx_fixed_layout, self.kfx_double_page_spread
        );
        if !self.kfx_fixed_layout {
            println!("  (source is reflowable — fixed-layout gate not applicable)");
            return;
        }
        println!(
            "  EPUB: pre-paginated={} viewport={}/{} pages  page-spread props={}",
            self.epub_pre_paginated,
            self.epub_docs_with_viewport,
            self.epub_spine_docs,
            self.epub_page_spread_props,
        );
        println!(
            "        original-resolution={:?} book-type={:?} primary-writing-mode={:?}",
            self.epub_original_resolution, self.epub_book_type, self.epub_primary_writing_mode,
        );
        println!(
            "        images: {} referenced / {} manifest ({} orphan)",
            self.epub_referenced_images,
            self.epub_manifest_images,
            self.epub_manifest_images
                .saturating_sub(self.epub_referenced_images),
        );
        if self.is_clean() {
            println!(
                "  fixed-layout structure preserved on {} side",
                dir.target_label()
            );
        } else {
            if !self.epub_pre_paginated {
                println!("  MISSING: rendition:layout pre-paginated");
            }
            if self.epub_docs_with_viewport != self.epub_spine_docs {
                println!(
                    "  MISSING: viewport on {} page(s)",
                    self.epub_spine_docs - self.epub_docs_with_viewport
                );
            }
            if self.kfx_double_page_spread && self.epub_page_spread_props == 0 {
                println!("  MISSING: page-spread-left/right itemref properties");
            }
            if self.epub_manifest_images != self.epub_referenced_images {
                println!(
                    "  ORPHAN: {} manifest image(s) not referenced by any page",
                    self.epub_manifest_images - self.epub_referenced_images
                );
            }
        }
    }
}

pub fn validate(epub_bytes: &[u8], kfx_bytes: &[u8]) -> Result<Report, String> {
    let (kfx_fixed_layout, kfx_double_page_spread) = kfx_fxl_signals(kfx_bytes)?;
    let mut report = Report {
        kfx_fixed_layout,
        kfx_double_page_spread,
        ..Default::default()
    };

    let mut archive =
        ZipArchive::new(Cursor::new(epub_bytes)).map_err(|e| format!("not a valid zip: {}", e))?;
    let container_bytes = read_zip_entry(&mut archive, "META-INF/container.xml")
        .map_err(|e| format!("container.xml: {}", e))?;
    let opf_path = parse_container_xml(&container_bytes)
        .map_err(|e| format!("container.xml parse: {:?}", e))?;
    let opf_bytes =
        read_zip_entry(&mut archive, &opf_path).map_err(|e| format!("opf {}: {}", opf_path, e))?;
    let enc = crate::util::extract_xml_encoding(&opf_bytes);
    let opf_str = crate::util::decode_text(&opf_bytes, enc);
    let opf = parse_opf(&opf_str).map_err(|e| format!("opf parse: {:?}", e))?;

    // OPF metadata / itemref scans (the OPF parser doesn't surface spine
    // itemref properties or the FXL `<meta>` set, so read them from the text).
    report.epub_pre_paginated = opf_str.contains("pre-paginated");
    report.epub_page_spread_props =
        opf_str.matches("page-spread-left").count() + opf_str.matches("page-spread-right").count();
    report.epub_original_resolution = meta_name_content(&opf_str, "original-resolution");
    report.epub_book_type = meta_name_content(&opf_str, "book-type");
    report.epub_primary_writing_mode = meta_name_content(&opf_str, "primary-writing-mode");

    // Manifest images (by basename, since bokai ships a flat OEBPS/ layout).
    let manifest_images: HashSet<String> = opf
        .manifest
        .values()
        .filter(|(_, mt)| mt.starts_with("image/"))
        .map(|(href, _)| basename(href))
        .collect();
    report.epub_manifest_images = manifest_images.len();

    // Walk every XHTML spine document: count viewports + collect referenced
    // image basenames. Resolve hrefs relative to the OPF's directory.
    let opf_dir = opf_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut referenced: HashSet<String> = HashSet::new();
    for id in &opf.spine_ids {
        let Some((href, media_type)) = opf.manifest.get(id) else {
            continue;
        };
        if !media_type.contains("xml") {
            continue;
        }
        let zip_path = if opf_dir.is_empty() {
            href.clone()
        } else {
            format!("{}/{}", opf_dir, href)
        };
        let Ok(bytes) = read_zip_entry(&mut archive, &zip_path) else {
            continue;
        };
        let html = String::from_utf8_lossy(&bytes);
        report.epub_spine_docs += 1;
        if html.contains("name=\"viewport\"") {
            report.epub_docs_with_viewport += 1;
        }
        for src in img_srcs(&html) {
            referenced.insert(basename(&src));
        }
    }
    report.epub_referenced_images = manifest_images.intersection(&referenced).count();

    Ok(report)
}

/// KFX `content_features` ($585) → `(fixed_layout, double_page_spread)`.
fn kfx_fxl_signals(kfx_bytes: &[u8]) -> Result<(bool, bool), String> {
    let book = crate::formats::kfx::loader::load(kfx_bytes).map_err(|e| e.to_string())?;
    let mut fixed = false;
    let mut dps = false;
    if let Some(map) = book.by_type.get(&(KfxSymbol::ContentFeatures as u64)) {
        for v in map.values() {
            let inner = v.unwrap_annotated();
            let Some(fields) = inner.as_struct() else {
                continue;
            };
            let Some(features) =
                get_field(fields, KfxSymbol::Features as u64).and_then(|x| x.as_list())
            else {
                continue;
            };
            for feat in features {
                let Some(ff) = feat.unwrap_annotated().as_struct() else {
                    continue;
                };
                let key = match get_field(ff, KfxSymbol::Key as u64) {
                    Some(v) => match v.unwrap_annotated() {
                        IonValue::String(s) => s.clone(),
                        other => book
                            .symbols
                            .text_of(other)
                            .map(|s| s.to_string())
                            .unwrap_or_default(),
                    },
                    None => continue,
                };
                if key.contains("fixed_layout") {
                    fixed = true;
                }
                if key == "yj_double_page_spread" {
                    dps = true;
                }
            }
        }
    }
    Ok((fixed, dps))
}

/// Extract `<meta name="NAME" content="VALUE"/>` value from OPF text.
fn meta_name_content(opf: &str, name: &str) -> Option<String> {
    let needle = format!("name=\"{}\"", name);
    let i = opf.find(&needle)?;
    let after = &opf[i..];
    let c = after.find("content=\"")? + "content=\"".len();
    let end = after[c..].find('"')? + c;
    Some(after[c..end].to_string())
}

/// Every `src="..."` from `<img>` tags (a plain scan — bokai emits one `src`
/// attribute per img and never on other tags in a fixed-layout page).
fn img_srcs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("<img") {
        rest = &rest[i + 4..];
        let Some(s) = rest.find("src=\"") else { break };
        let start = s + "src=\"".len();
        let Some(e) = rest[start..].find('"') else {
            break;
        };
        out.push(rest[start..start + e].to_string());
        rest = &rest[start + e..];
    }
    out
}

fn basename(href: &str) -> String {
    href.rsplit('/').next().unwrap_or(href).to_string()
}

fn read_zip_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, std::io::Error> {
    let mut file = archive.by_name(name)?;
    let mut buf = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_name_content_extracts_value() {
        let opf = r#"<meta name="original-resolution" content="900x1280"/>"#;
        assert_eq!(
            meta_name_content(opf, "original-resolution"),
            Some("900x1280".into())
        );
        assert_eq!(meta_name_content(opf, "book-type"), None);
    }

    #[test]
    fn img_srcs_and_basename() {
        let html = r#"<body><div><img src="image_rsrc1SX.jpg" alt=""/></div>
            <div><img src="OEBPS/cover.jpeg"/></div></body>"#;
        let srcs = img_srcs(html);
        assert_eq!(srcs, vec!["image_rsrc1SX.jpg", "OEBPS/cover.jpeg"]);
        assert_eq!(basename("OEBPS/cover.jpeg"), "cover.jpeg");
        assert_eq!(basename("cover.jpeg"), "cover.jpeg");
    }

    #[test]
    fn reflowable_source_passes_trivially() {
        let r = Report {
            kfx_fixed_layout: false,
            ..Default::default()
        };
        assert!(
            r.is_clean(),
            "a reflowable source is not subject to FXL gates"
        );
    }

    #[test]
    fn fixed_layout_gates_fire() {
        // Conformant fixed-layout report.
        let good = Report {
            kfx_fixed_layout: true,
            kfx_double_page_spread: true,
            epub_pre_paginated: true,
            epub_spine_docs: 10,
            epub_docs_with_viewport: 10,
            epub_page_spread_props: 8,
            epub_manifest_images: 10,
            epub_referenced_images: 10,
            ..Default::default()
        };
        assert!(good.is_clean());

        // Missing viewport on one page → fails.
        let mut missing_vp = good;
        missing_vp.epub_docs_with_viewport = 9;
        assert!(!missing_vp.is_clean());

        // Orphan images → fails.
        let good2 = Report {
            kfx_fixed_layout: true,
            kfx_double_page_spread: true,
            epub_pre_paginated: true,
            epub_spine_docs: 10,
            epub_docs_with_viewport: 10,
            epub_page_spread_props: 8,
            epub_manifest_images: 20,
            epub_referenced_images: 10,
            ..Default::default()
        };
        assert!(!good2.is_clean(), "10 orphan images must fail the gate");

        // Spread comic with no page-spread props → fails.
        let no_spread = Report {
            kfx_fixed_layout: true,
            kfx_double_page_spread: true,
            epub_pre_paginated: true,
            epub_spine_docs: 10,
            epub_docs_with_viewport: 10,
            epub_page_spread_props: 0,
            epub_manifest_images: 10,
            epub_referenced_images: 10,
            ..Default::default()
        };
        assert!(!no_spread.is_clean());
    }
}
