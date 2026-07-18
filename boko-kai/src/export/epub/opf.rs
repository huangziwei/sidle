//! Shared `content.opf` (OPF package document) emitter.
//!
//! Every EPUB writer in the crate — the IR exporter's raw and normalized
//! paths and the mechanical `kfx_to_epub` port — builds an [`OpfPackage`]
//! and serializes it through [`emit_opf`], so the package document's shape
//! (element order, refinement style, attribute forms) is identical by
//! construction rather than by parallel maintenance.
//!
//! The emitted package is EPUB 3 throughout: creator roles and sort keys
//! ride `<meta refines>` (the EPUB-2 `opf:role`/`opf:file-as`/`opf:scheme`
//! attributes are rejected by epubcheck under 3.x as RSC-005), the ASIN is
//! a plain `<dc:identifier id="asin">`, and `<meta name>` Kindle hints
//! (cover, primary-writing-mode, fixed-layout) are kept alongside their
//! EPUB 3 equivalents for reader compatibility.

use crate::model::LandmarkType;

/// One `<dc:creator>` / `<dc:contributor>` with optional refinements.
pub struct OpfCreator {
    pub name: String,
    /// MARC relator code (`aut`, `trl`, …) for the `role` refinement.
    pub role: Option<String>,
    /// Sort key for the `file-as` refinement.
    pub file_as: Option<String>,
}

/// Per-creator `file-as` values, one per author. Creator `i` gets the
/// positional `author_sorts[i]`; a creator past the end of the vec (or every
/// creator, when the source declared no sort keys) falls back to the joined
/// author list so EPUB libraries still sort multi-author books. Both EPUB
/// writers derive their creators through this, so the shape can't drift.
pub fn creator_file_as_keys(authors: &[String], author_sorts: &[String]) -> Vec<String> {
    let joined = authors.join(" & ");
    (0..authors.len())
        .map(|i| author_sorts.get(i).cloned().unwrap_or_else(|| joined.clone()))
        .collect()
}

/// `belongs-to-collection` (series) metadata.
pub struct OpfCollection {
    pub name: String,
    pub collection_type: Option<String>,
    pub position: Option<f64>,
}

/// Fixed-layout (pre-paginated) package metadata. Presence of this struct
/// switches the package to FXL: the `rendition:` vocabulary is declared on
/// `<package>` and the pre-paginated layout metas are emitted.
pub struct OpfFixedLayout {
    /// `rendition:spread` value (e.g. `landscape`), when the source declares
    /// a spread preference.
    pub rendition_spread: Option<String>,
    /// EBPAJ `fixed-layout-jp:viewport` twin of `original-resolution`,
    /// emitted for Japanese fixed-layout sources that carried it.
    pub ebpaj_viewport: Option<(u32, u32)>,
    /// Page pixel size for `original-resolution`; also derives the
    /// `rendition:orientation` / `orientation-lock` hints.
    pub original_resolution: Option<(u32, u32)>,
    /// `book-type` OPF hint — `"comic"` for double-page-spread manga.
    pub book_type: Option<String>,
}

/// The `<metadata>` block. Optional fields are omitted entirely when absent,
/// so a caller that populates only the core set gets the minimal document.
pub struct OpfMetadata {
    /// Falls back to `"Untitled"` when empty.
    pub title: String,
    /// Title sort key (`file-as` refinement on the title).
    pub title_file_as: Option<String>,
    pub creators: Vec<OpfCreator>,
    pub contributors: Vec<OpfCreator>,
    /// Falls back to `"en"` when empty.
    pub language: String,
    /// Primary unique identifier (`id="BookId"`). Falls back to the nil-UUID
    /// URN when empty.
    pub identifier: String,
    /// Amazon catalogue id, emitted as `<dc:identifier id="asin">`.
    pub asin: Option<String>,
    /// `dcterms:modified` value — the caller stamps the conversion time.
    pub modified: String,
    /// `<dc:date>`, pre-formatted by the caller (see [`format_opf_date`]).
    pub date: Option<String>,
    /// Trimmed before emission; whitespace-only publishers are dropped (an
    /// empty `<dc:publisher>` is invalid OPF).
    pub publisher: Option<String>,
    pub description: Option<String>,
    pub subjects: Vec<String>,
    pub rights: Option<String>,
    pub collection: Option<OpfCollection>,
    /// Manifest id of the cover image, for the EPUB-2-compat
    /// `<meta name="cover">` (the manifest item itself carries
    /// `properties="cover-image"` — the caller sets that).
    pub cover_manifest_id: Option<String>,
    /// Kindle `primary-writing-mode` hint, already resolved (see
    /// [`primary_writing_mode`]); `horizontal-lr` is the default and should
    /// be passed as `None`.
    pub primary_writing_mode: Option<String>,
    /// Spine `page-progression-direction`. `ltr` (the EPUB default) is
    /// suppressed at emission.
    pub page_progression_direction: Option<String>,
    pub fixed_layout: Option<OpfFixedLayout>,
}

/// One manifest `<item>`. `href` is relative to the OPF. `properties` are
/// emitted space-joined (`cover-image`, `svg`, …); see
/// [`xhtml_content_properties`] for the content-document scan.
pub struct OpfItem {
    pub id: String,
    pub href: String,
    pub media_type: String,
    pub properties: Vec<String>,
}

/// One spine `<itemref>` with optional `properties` (FXL page-spread).
pub struct OpfItemref {
    pub idref: String,
    pub properties: Option<String>,
}

/// One `<guide>` reference (EPUB 2.0 landmarks — still consulted by Apple
/// Books / Kindle / calibre). An empty `title` is emitted as `title=""`.
/// The nav doc's `<nav epub:type="landmarks">` renders from the same
/// entries (see `export::nav::emit_nav`), so the two stay in lockstep.
#[derive(Debug, Clone)]
pub struct OpfGuideRef {
    pub guide_type: String,
    pub title: String,
    pub href: String,
}

/// Everything [`emit_opf`] needs. The manifest never includes the NCX or the
/// nav document — those two items are fixed and written by the emitter.
pub struct OpfPackage {
    pub metadata: OpfMetadata,
    pub manifest: Vec<OpfItem>,
    pub spine: Vec<OpfItemref>,
    pub guide: Vec<OpfGuideRef>,
}

/// Serialize the package document.
pub fn emit_opf(pkg: &OpfPackage) -> String {
    let m = &pkg.metadata;
    let mut s = String::new();

    // Fixed-layout books declare the `rendition:` property vocabulary on
    // `<package>` so the rendition metas below validate.
    let prefix_attr = if m.fixed_layout.is_some() {
        " prefix=\"rendition: http://www.idpf.org/vocab/rendition/#\""
    } else {
        ""
    };
    s.push_str(&format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"BookId\"{prefix_attr}>\n  <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:opf=\"http://www.idpf.org/2007/opf\">\n"
    ));

    let title = if m.title.is_empty() {
        "Untitled"
    } else {
        &m.title
    };
    s.push_str(&format!(
        "    <dc:title id=\"title\">{}</dc:title>\n",
        xml_escape(title)
    ));
    if let Some(file_as) = m.title_file_as.as_deref() {
        s.push_str(&format!(
            "    <meta refines=\"#title\" property=\"file-as\">{}</meta>\n",
            xml_escape(file_as)
        ));
    }

    for (i, c) in m.creators.iter().enumerate() {
        let cid = format!("creator{}", i + 1);
        s.push_str(&format!(
            "    <dc:creator id=\"{}\">{}</dc:creator>\n",
            cid,
            xml_escape(&c.name)
        ));
        push_person_refines(&mut s, &cid, c);
    }
    for (i, c) in m.contributors.iter().enumerate() {
        let cid = format!("contrib{}", i + 1);
        s.push_str(&format!(
            "    <dc:contributor id=\"{}\">{}</dc:contributor>\n",
            cid,
            xml_escape(&c.name)
        ));
        push_person_refines(&mut s, &cid, c);
    }

    let lang = if m.language.is_empty() {
        "en"
    } else {
        &m.language
    };
    s.push_str(&format!(
        "    <dc:language>{}</dc:language>\n",
        xml_escape(lang)
    ));

    let id = if m.identifier.is_empty() {
        "urn:uuid:00000000-0000-0000-0000-000000000000"
    } else {
        &m.identifier
    };
    s.push_str(&format!(
        "    <dc:identifier id=\"BookId\">{}</dc:identifier>\n",
        xml_escape(id)
    ));
    if let Some(asin) = m.asin.as_deref() {
        s.push_str(&format!(
            "    <dc:identifier id=\"asin\">{}</dc:identifier>\n",
            xml_escape(asin)
        ));
    }

    s.push_str(&format!(
        "    <meta property=\"dcterms:modified\">{}</meta>\n",
        xml_escape(&m.modified)
    ));
    if let Some(date) = m.date.as_deref() {
        s.push_str(&format!("    <dc:date>{}</dc:date>\n", xml_escape(date)));
    }
    if let Some(pub_) = m
        .publisher
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        s.push_str(&format!(
            "    <dc:publisher>{}</dc:publisher>\n",
            xml_escape(pub_)
        ));
    }
    if let Some(desc) = m.description.as_deref() {
        s.push_str(&format!(
            "    <dc:description>{}</dc:description>\n",
            xml_escape(desc)
        ));
    }
    for subject in &m.subjects {
        s.push_str(&format!(
            "    <dc:subject>{}</dc:subject>\n",
            xml_escape(subject)
        ));
    }
    if let Some(rights) = m.rights.as_deref() {
        s.push_str(&format!(
            "    <dc:rights>{}</dc:rights>\n",
            xml_escape(rights)
        ));
    }
    if let Some(coll) = &m.collection {
        s.push_str(&format!(
            "    <meta property=\"belongs-to-collection\" id=\"collection1\">{}</meta>\n",
            xml_escape(&coll.name)
        ));
        if let Some(ct) = coll.collection_type.as_deref() {
            s.push_str(&format!(
                "    <meta refines=\"#collection1\" property=\"collection-type\">{}</meta>\n",
                xml_escape(ct)
            ));
        }
        if let Some(pos) = coll.position {
            let pos_str = if pos.fract() == 0.0 {
                format!("{}", pos as i64)
            } else {
                format!("{}", pos)
            };
            s.push_str(&format!(
                "    <meta refines=\"#collection1\" property=\"group-position\">{}</meta>\n",
                pos_str
            ));
        }
    }

    // Cover meta (EPUB2-compat; kept alongside `properties="cover-image"`
    // on the manifest item — most readers honour either, calibre emits both).
    if let Some(cover_id) = m.cover_manifest_id.as_deref() {
        s.push_str(&format!(
            "    <meta name=\"cover\" content=\"{}\"/>\n",
            xml_escape(cover_id)
        ));
    }

    if let Some(pwm) = m.primary_writing_mode.as_deref() {
        s.push_str(&format!(
            "    <meta name=\"primary-writing-mode\" content=\"{}\"/>\n",
            xml_escape(pwm)
        ));
    }

    if let Some(fxl) = &m.fixed_layout {
        s.push_str("    <meta property=\"rendition:layout\">pre-paginated</meta>\n");
        if let Some(spread) = fxl.rendition_spread.as_deref() {
            s.push_str(&format!(
                "    <meta property=\"rendition:spread\">{}</meta>\n",
                xml_escape(spread)
            ));
        }
        if let Some((w, h)) = fxl.ebpaj_viewport {
            s.push_str(&format!(
                "    <meta property=\"fixed-layout-jp:viewport\">width={w}, height={h}</meta>\n"
            ));
        }
        s.push_str("    <meta name=\"fixed-layout\" content=\"true\"/>\n");
        if let Some((w, h)) = fxl.original_resolution {
            s.push_str(&format!(
                "    <meta name=\"original-resolution\" content=\"{w}x{h}\"/>\n"
            ));
            let orientation = if w > h { "landscape" } else { "portrait" };
            s.push_str(&format!(
                "    <meta property=\"rendition:orientation\">{orientation}</meta>\n"
            ));
            s.push_str(&format!(
                "    <meta name=\"orientation-lock\" content=\"{orientation}\"/>\n"
            ));
        }
        if let Some(bt) = fxl.book_type.as_deref() {
            s.push_str(&format!(
                "    <meta name=\"book-type\" content=\"{}\"/>\n",
                xml_escape(bt)
            ));
        }
    }

    s.push_str("  </metadata>\n");

    s.push_str("  <manifest>\n");
    s.push_str("    <item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n");
    s.push_str(
        "    <item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n",
    );
    for item in &pkg.manifest {
        let properties = if item.properties.is_empty() {
            String::new()
        } else {
            format!(" properties=\"{}\"", xml_escape(&item.properties.join(" ")))
        };
        s.push_str(&format!(
            "    <item id=\"{}\" href=\"{}\" media-type=\"{}\"{}/>\n",
            xml_escape(&item.id),
            xml_escape(&item.href),
            xml_escape(&item.media_type),
            properties,
        ));
    }
    s.push_str("  </manifest>\n");

    // `ltr` is the EPUB default; calibre suppresses the attribute for it.
    let ppd_attr = m
        .page_progression_direction
        .as_deref()
        .filter(|v| !v.is_empty() && *v != "ltr")
        .map(|v| format!(" page-progression-direction=\"{}\"", xml_escape(v)))
        .unwrap_or_default();
    s.push_str(&format!("  <spine toc=\"ncx\"{}>\n", ppd_attr));
    for item in &pkg.spine {
        let props = item
            .properties
            .as_deref()
            .map(|p| format!(" properties=\"{}\"", xml_escape(p)))
            .unwrap_or_default();
        s.push_str(&format!(
            "    <itemref idref=\"{}\"{}/>\n",
            xml_escape(&item.idref),
            props,
        ));
    }
    s.push_str("  </spine>\n");

    if !pkg.guide.is_empty() {
        s.push_str("  <guide>\n");
        for g in &pkg.guide {
            s.push_str(&format!(
                "    <reference type=\"{}\" title=\"{}\" href=\"{}\"/>\n",
                xml_escape(&g.guide_type),
                xml_escape(&g.title),
                xml_escape(&g.href),
            ));
        }
        s.push_str("  </guide>\n");
    }

    s.push_str("</package>\n");
    s
}

fn push_person_refines(s: &mut String, cid: &str, c: &OpfCreator) {
    if let Some(role) = c.role.as_deref() {
        s.push_str(&format!(
            "    <meta refines=\"#{}\" property=\"role\" scheme=\"marc:relators\">{}</meta>\n",
            cid,
            xml_escape(role)
        ));
    }
    if let Some(fa) = c.file_as.as_deref() {
        s.push_str(&format!(
            "    <meta refines=\"#{}\" property=\"file-as\">{}</meta>\n",
            cid,
            xml_escape(fa)
        ));
    }
}

/// Derive a manifest id from `filename` (basename stem, non-id characters
/// mapped to `_`, `id_` prefix when the result doesn't start with a letter).
/// `taken` reports ids already in use; collisions get an `_{n}` suffix.
pub fn make_manifest_id(filename: &str, taken: impl Fn(&str) -> bool) -> String {
    let stem = filename
        .rsplit('/')
        .next()
        .unwrap_or(filename)
        .split('.')
        .next()
        .unwrap_or(filename);
    let mut id: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if id.is_empty() || !id.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        id = format!("id_{}", id);
    }
    if !taken(&id) {
        return id;
    }
    let mut n = 1;
    loop {
        let candidate = format!("{}_{}", id, n);
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// EPUB 3 manifest `properties` a content document must declare (OPF-014):
/// `svg` / `mathml` / `scripted` when the XHTML embeds those elements. The
/// scan looks for real element openings — text-node `<` is `&lt;`-escaped in
/// XHTML, so a raw `<svg` is always a genuine element and the inverse
/// (over-declaring, OPF-015) can't happen.
pub fn xhtml_content_properties(xml: &str) -> Vec<&'static str> {
    let mut props = Vec::new();
    if contains_element(xml, "svg") {
        props.push("svg");
    }
    if contains_element(xml, "math") {
        props.push("mathml");
    }
    if contains_element(xml, "script") {
        props.push("scripted");
    }
    props
}

/// True if `xml` contains a real element `<name…>` (open tag). Matches
/// `<name` followed by a tag delimiter so `<svgfoo` doesn't count.
fn contains_element(xml: &str, name: &str) -> bool {
    let needle = format!("<{name}");
    let mut hay = xml;
    while let Some(pos) = hay.find(&needle) {
        let after = pos + needle.len();
        if hay[after..]
            .chars()
            .next()
            .is_none_or(|c| matches!(c, ' ' | '\t' | '\r' | '\n' | '>' | '/'))
        {
            return true;
        }
        hay = &hay[after..];
    }
    false
}

/// Format a source publication date for `<dc:date>`: a bare `YYYY-MM-DD`
/// (the KFX `issue_date` form) gains a UTC midnight time to match calibre's
/// ISO-8601 output; anything else passes through unchanged.
pub fn format_opf_date(date: &str) -> String {
    if date.len() == 10 && date.chars().nth(4) == Some('-') && date.chars().nth(7) == Some('-') {
        format!("{}T00:00:00+00:00", date)
    } else {
        date.to_string()
    }
}

// Port-compat re-export: the frozen mechanical port resolves the Kindle
// `primary-writing-mode` value through `export::opf::`; the implementation
// is direction-shared and lives in `formats::epub::opf_meta`. Deleted
// together with the port.
pub use crate::formats::epub::opf_meta::primary_writing_mode;

/// Map a [`LandmarkType`] to the EPUB 2.0 `<guide>` reference vocabulary
/// (`text` is the guide spelling of "start reading"; the rest match their
/// KFX/EPUB names).
pub fn landmark_guide_type(t: LandmarkType) -> &'static str {
    match t {
        LandmarkType::Cover => "cover",
        LandmarkType::TitlePage => "titlepage",
        LandmarkType::Toc => "toc",
        LandmarkType::StartReading => "text",
        LandmarkType::BodyMatter => "bodymatter",
        LandmarkType::FrontMatter => "frontmatter",
        LandmarkType::BackMatter => "backmatter",
        LandmarkType::Acknowledgements => "acknowledgements",
        LandmarkType::Bibliography => "bibliography",
        LandmarkType::Glossary => "glossary",
        LandmarkType::Index => "index",
        LandmarkType::Preface => "preface",
        LandmarkType::Endnotes => "endnotes",
        LandmarkType::Loi => "loi",
        LandmarkType::Lot => "lot",
    }
}

/// Point the guide's `cover` reference at the synthesized titlepage (the
/// cover the reader actually sees), inserting one when the source had no
/// cover landmark. Apple Books renders the `type="cover"` target as the
/// cover page; without the rewrite it would show the first content page.
pub fn repoint_cover_guide(guide: &mut Vec<OpfGuideRef>, titlepage_href: &str) {
    if let Some(cover_ref) = guide.iter_mut().find(|g| g.guide_type == "cover") {
        cover_ref.href = titlepage_href.to_string();
        if cover_ref.title.is_empty() {
            cover_ref.title = "Cover".to_string();
        }
    } else {
        guide.insert(
            0,
            OpfGuideRef {
                guide_type: "cover".to_string(),
                title: "Cover".to_string(),
                href: titlepage_href.to_string(),
            },
        );
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_metadata() -> OpfMetadata {
        OpfMetadata {
            title: String::new(),
            title_file_as: None,
            creators: Vec::new(),
            contributors: Vec::new(),
            language: String::new(),
            identifier: String::new(),
            asin: None,
            modified: "2026-07-15T00:00:00Z".to_string(),
            date: None,
            publisher: None,
            description: None,
            subjects: Vec::new(),
            rights: None,
            collection: None,
            cover_manifest_id: None,
            primary_writing_mode: None,
            page_progression_direction: None,
            fixed_layout: None,
        }
    }

    #[test]
    fn emit_opf_core_shape() {
        let pkg = OpfPackage {
            metadata: OpfMetadata {
                title: "黒死館殺人事件".to_string(),
                title_file_as: Some("こくしかんさつじんじけん".to_string()),
                creators: vec![OpfCreator {
                    name: "小栗 虫太郎".to_string(),
                    role: Some("aut".to_string()),
                    file_as: Some("おぐり むしたろう".to_string()),
                }],
                language: "ja".to_string(),
                identifier: "abc-123".to_string(),
                asin: Some("B009IY1W5Q".to_string()),
                date: Some("2012-09-27T00:00:00+00:00".to_string()),
                publisher: Some("  ".to_string()),
                cover_manifest_id: Some("cover".to_string()),
                primary_writing_mode: Some("vertical-rl".to_string()),
                page_progression_direction: Some("rtl".to_string()),
                ..minimal_metadata()
            },
            manifest: vec![
                OpfItem {
                    id: "cover".to_string(),
                    href: "cover.jpeg".to_string(),
                    media_type: "image/jpeg".to_string(),
                    properties: vec!["cover-image".to_string()],
                },
                OpfItem {
                    id: "c0".to_string(),
                    href: "c0.xhtml".to_string(),
                    media_type: "application/xhtml+xml".to_string(),
                    properties: Vec::new(),
                },
            ],
            spine: vec![OpfItemref {
                idref: "c0".to_string(),
                properties: None,
            }],
            guide: vec![OpfGuideRef {
                guide_type: "toc".to_string(),
                title: "目次".to_string(),
                href: "c0.xhtml".to_string(),
            }],
        };
        let opf = emit_opf(&pkg);
        let expected = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"BookId\">\n\
\x20 <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:opf=\"http://www.idpf.org/2007/opf\">\n\
\x20   <dc:title id=\"title\">黒死館殺人事件</dc:title>\n\
\x20   <meta refines=\"#title\" property=\"file-as\">こくしかんさつじんじけん</meta>\n\
\x20   <dc:creator id=\"creator1\">小栗 虫太郎</dc:creator>\n\
\x20   <meta refines=\"#creator1\" property=\"role\" scheme=\"marc:relators\">aut</meta>\n\
\x20   <meta refines=\"#creator1\" property=\"file-as\">おぐり むしたろう</meta>\n\
\x20   <dc:language>ja</dc:language>\n\
\x20   <dc:identifier id=\"BookId\">abc-123</dc:identifier>\n\
\x20   <dc:identifier id=\"asin\">B009IY1W5Q</dc:identifier>\n\
\x20   <meta property=\"dcterms:modified\">2026-07-15T00:00:00Z</meta>\n\
\x20   <dc:date>2012-09-27T00:00:00+00:00</dc:date>\n\
\x20   <meta name=\"cover\" content=\"cover\"/>\n\
\x20   <meta name=\"primary-writing-mode\" content=\"vertical-rl\"/>\n\
\x20 </metadata>\n\
\x20 <manifest>\n\
\x20   <item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n\
\x20   <item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n\
\x20   <item id=\"cover\" href=\"cover.jpeg\" media-type=\"image/jpeg\" properties=\"cover-image\"/>\n\
\x20   <item id=\"c0\" href=\"c0.xhtml\" media-type=\"application/xhtml+xml\"/>\n\
\x20 </manifest>\n\
\x20 <spine toc=\"ncx\" page-progression-direction=\"rtl\">\n\
\x20   <itemref idref=\"c0\"/>\n\
\x20 </spine>\n\
\x20 <guide>\n\
\x20   <reference type=\"toc\" title=\"目次\" href=\"c0.xhtml\"/>\n\
\x20 </guide>\n\
</package>\n";
        assert_eq!(opf, expected);
        // Whitespace-only publisher is dropped, empty title/language/id
        // defaults are only used when the fields are empty (they weren't).
        assert!(!opf.contains("dc:publisher"));
    }

    #[test]
    fn emit_opf_defaults_and_ltr_suppression() {
        let mut md = minimal_metadata();
        md.page_progression_direction = Some("ltr".to_string());
        let pkg = OpfPackage {
            metadata: md,
            manifest: Vec::new(),
            spine: Vec::new(),
            guide: Vec::new(),
        };
        let opf = emit_opf(&pkg);
        assert!(opf.contains("<dc:title id=\"title\">Untitled</dc:title>"));
        assert!(opf.contains("<dc:language>en</dc:language>"));
        assert!(opf.contains("urn:uuid:00000000-0000-0000-0000-000000000000"));
        assert!(opf.contains("<spine toc=\"ncx\">"));
        assert!(!opf.contains("page-progression-direction"));
        assert!(!opf.contains("<guide>"));
    }

    #[test]
    fn emit_opf_fixed_layout_block() {
        let mut md = minimal_metadata();
        md.fixed_layout = Some(OpfFixedLayout {
            rendition_spread: None,
            ebpaj_viewport: None,
            original_resolution: Some((1600, 2560)),
            book_type: Some("comic".to_string()),
        });
        let pkg = OpfPackage {
            metadata: md,
            manifest: Vec::new(),
            spine: Vec::new(),
            guide: Vec::new(),
        };
        let opf = emit_opf(&pkg);
        assert!(opf.contains("prefix=\"rendition: http://www.idpf.org/vocab/rendition/#\""));
        let layout = opf.find("rendition:layout\">pre-paginated").unwrap();
        let fxl = opf.find("name=\"fixed-layout\" content=\"true\"").unwrap();
        let res = opf
            .find("name=\"original-resolution\" content=\"1600x2560\"")
            .unwrap();
        let orient = opf.find("rendition:orientation\">portrait").unwrap();
        let lock = opf
            .find("name=\"orientation-lock\" content=\"portrait\"")
            .unwrap();
        let bt = opf.find("name=\"book-type\" content=\"comic\"").unwrap();
        assert!(layout < fxl && fxl < res && res < orient && orient < lock && lock < bt);
    }

    #[test]
    fn make_manifest_id_rules() {
        let none = |_: &str| false;
        assert_eq!(make_manifest_id("image_rsrc562.jpg", none), "image_rsrc562");
        assert_eq!(make_manifest_id("cover.jpeg", none), "cover");
        assert_eq!(make_manifest_id("a/b/c0.xhtml", none), "c0");
        assert_eq!(make_manifest_id("42.png", none), "id_42");
        assert_eq!(make_manifest_id("表紙.jpg", none), "id___");
        assert_eq!(make_manifest_id("style.css", |id| id == "style"), "style_1");
        assert_eq!(
            make_manifest_id("style.css", |id| id == "style" || id == "style_1"),
            "style_2"
        );
    }

    #[test]
    fn xhtml_content_properties_scan() {
        assert_eq!(
            xhtml_content_properties("<body><svg viewBox=\"0 0 1 1\"/></body>"),
            vec!["svg"]
        );
        assert!(xhtml_content_properties("<p>&lt;svg&gt; and <svgfoo/></p>").is_empty());
        assert_eq!(
            xhtml_content_properties("<math><mi>x</mi></math><script/>"),
            vec!["mathml", "scripted"]
        );
    }

    #[test]
    fn format_opf_date_rules() {
        assert_eq!(format_opf_date("2012-09-27"), "2012-09-27T00:00:00+00:00");
        assert_eq!(
            format_opf_date("2012-09-27T12:34:56Z"),
            "2012-09-27T12:34:56Z"
        );
        assert_eq!(format_opf_date("2012"), "2012");
    }

    #[test]
    fn repoint_cover_guide_rules() {
        // Existing cover ref: href repointed, empty title filled.
        let mut guide = vec![
            OpfGuideRef {
                guide_type: "cover".to_string(),
                title: String::new(),
                href: "c0.xhtml".to_string(),
            },
            OpfGuideRef {
                guide_type: "toc".to_string(),
                title: "目次".to_string(),
                href: "c0.xhtml".to_string(),
            },
        ];
        repoint_cover_guide(&mut guide, "titlepage.xhtml");
        assert_eq!(guide[0].href, "titlepage.xhtml");
        assert_eq!(guide[0].title, "Cover");
        assert_eq!(guide[1].href, "c0.xhtml");

        // No cover ref: one is inserted at the front.
        let mut guide = vec![OpfGuideRef {
            guide_type: "toc".to_string(),
            title: "TOC".to_string(),
            href: "c0.xhtml".to_string(),
        }];
        repoint_cover_guide(&mut guide, "titlepage.xhtml");
        assert_eq!(guide[0].guide_type, "cover");
        assert_eq!(guide[0].href, "titlepage.xhtml");
        assert_eq!(guide.len(), 2);
    }
}
