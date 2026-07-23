//! Minimal OPF parser tailored to the EPUB-3 standalone validator.
//!
//! Independent from [`crate::formats::epub::parse_opf`] by design (per
//! `crate::validate` philosophy: a parser-side bug should surface in the
//! validator instead of being silently mirrored). Captures the attributes
//! the standalone validator needs that the full parser drops: `<item
//! properties>`/`<item fallback>`, `<itemref linear>`, the package
//! `unique-identifier`, `<dc:identifier>` ids, the `<spine toc>` NCX pointer,
//! and `<guide>` references.

use std::io;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

#[derive(Debug, Default)]
pub struct Package {
    /// The package element's `version` attribute (`"3.0"`, `"2.0"`, …). `None`
    /// if absent. Version-gated rules (DOCTYPE conformance) key on this.
    pub version: Option<String>,
    /// The package element's `unique-identifier` attribute (the id it says
    /// points at the publication's `<dc:identifier>`). `None` if absent.
    pub unique_identifier: Option<String>,
    /// The `id` of every `<dc:identifier>` in the metadata.
    pub identifier_ids: Vec<String>,
    /// The trimmed text of the `<dc:identifier>` whose `id` matches
    /// [`Self::unique_identifier`] — the publication's unique identifier
    /// *value* (what NCX-001 compares the NCX `dtb:uid` against). `None` if the
    /// package declares no unique-identifier or no matching identifier element.
    pub unique_identifier_value: Option<String>,
    /// Every Dublin Core metadata element (`<dc:*>`) under `<metadata>`, in
    /// document order. Drives the metadata-value checks (date, identifier UUID,
    /// empty element, required title/language). [`Self::identifier_ids`] and
    /// [`Self::unique_identifier_value`] are derived from this.
    pub metadata: Vec<DcMeta>,
    /// The `<spine toc>` attribute (a manifest id for the NCX), if present.
    pub spine_toc: Option<String>,
    /// Every `<reference href>` inside `<guide>` (raw, fragment kept).
    pub guide_hrefs: Vec<String>,
    /// Every `<link>` element in the package document (EPUB 3 metadata/collection
    /// links). Drives the OPF link rules (OPF-089/093/094/095/098/067, RSC-029).
    pub links: Vec<OpfLink>,
    pub manifest: Vec<ManifestItem>,
    pub spine: Vec<SpineItem>,
}

/// An EPUB 3 `<link>` element in the package document.
#[derive(Debug, Clone)]
pub struct OpfLink {
    /// The `href` attribute (raw, fragment kept). Empty if absent.
    pub href: String,
    /// Whitespace-separated `rel` keywords (deduped, order lost — matches how
    /// epubcheck's vocab parser treats them as a set). Empty if absent.
    pub rel: Vec<String>,
    /// The `media-type` attribute, if present.
    pub media_type: Option<String>,
    /// True if this `<link>` is a child of `<metadata>` (vs a collection link).
    pub in_metadata: bool,
}

/// A Dublin Core metadata element (`<dc:title>`, `<dc:identifier>`, …).
#[derive(Debug, Clone)]
pub struct DcMeta {
    /// The DC element's local name (`"title"`, `"date"`, `"language"`, `"identifier"`, …).
    pub name: String,
    /// The element's trimmed text value (empty if the element had no text).
    pub value: String,
    /// The `id` attribute, if present.
    pub id: Option<String>,
    /// The `opf:scheme` / `scheme` attribute, if present.
    pub scheme: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ManifestItem {
    pub id: String,
    pub href: String,
    pub media_type: String,
    /// Space-separated tokens from `properties=`. Empty if absent.
    pub properties: Vec<String>,
    /// The `fallback` attribute (a manifest id), if present.
    pub fallback: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpineItem {
    pub idref: String,
    /// `Some(true)` if `linear="yes"`, `Some(false)` if `linear="no"`, `None`
    /// if attribute is absent. Per OPF 3.3 the default is `yes`, but we
    /// preserve the distinction so the reachability rule only flags
    /// explicitly-non-linear entries.
    pub linear: Option<bool>,
}

impl Package {
    pub fn manifest_by_id(&self, id: &str) -> Option<&ManifestItem> {
        self.manifest.iter().find(|m| m.id == id)
    }
}

pub fn parse(content: &str) -> io::Result<Package> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut pkg = Package::default();
    let mut in_manifest = false;
    let mut in_spine = false;
    let mut in_metadata = false;
    let mut in_guide = false;
    // In-progress Dublin Core element: opened on a `<dc:*>` Start, its text
    // accumulated over `Text` events, finalized into `pkg.metadata` on `End`.
    let mut dc: Option<DcMeta> = None;

    loop {
        match reader.read_event() {
            // A `<dc:*>` element with text content: start accumulating. (A
            // self-closed `<dc:*/>` has no text and is recorded in the shared
            // start arm below.)
            Ok(Event::Start(e)) if in_metadata && e.name().as_ref().starts_with(b"dc:") => {
                dc = Some(DcMeta {
                    name: String::from_utf8_lossy(local_name(e.name().as_ref())).into_owned(),
                    id: attr(&e, b"id"),
                    scheme: scheme_attr(&e),
                    value: String::new(),
                });
            }
            Ok(Event::Text(t)) if dc.is_some() => {
                if let Some(m) = dc.as_mut() {
                    m.value.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
            }
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                // A self-closed `<dc:*/>` metadata element (Empty): no text.
                if in_metadata && name.as_ref().starts_with(b"dc:") {
                    pkg.metadata.push(DcMeta {
                        name: String::from_utf8_lossy(local).into_owned(),
                        id: attr(&e, b"id"),
                        scheme: scheme_attr(&e),
                        value: String::new(),
                    });
                }
                match local {
                    b"package" => {
                        pkg.unique_identifier = attr(&e, b"unique-identifier");
                        pkg.version = attr(&e, b"version");
                    }
                    b"manifest" => in_manifest = true,
                    b"spine" => {
                        in_spine = true;
                        pkg.spine_toc = attr(&e, b"toc");
                    }
                    b"metadata" => in_metadata = true,
                    b"guide" => in_guide = true,
                    b"reference" if in_guide => {
                        if let Some(href) = attr(&e, b"href") {
                            pkg.guide_hrefs.push(href);
                        }
                    }
                    b"link" => {
                        // Dedup rel keywords into a set-like Vec (epubcheck's
                        // vocab parser treats `rel` as a Set).
                        let mut rel: Vec<String> = Vec::new();
                        if let Some(raw) = attr(&e, b"rel") {
                            for tok in raw.split_whitespace() {
                                if !rel.iter().any(|r| r == tok) {
                                    rel.push(tok.to_string());
                                }
                            }
                        }
                        pkg.links.push(OpfLink {
                            href: attr(&e, b"href").unwrap_or_default(),
                            rel,
                            media_type: attr(&e, b"media-type"),
                            in_metadata,
                        });
                    }
                    b"item" if in_manifest => {
                        let (mut id, mut href, mut mt, mut props, mut fallback) = (
                            String::new(),
                            String::new(),
                            String::new(),
                            Vec::new(),
                            None,
                        );
                        for a in e.attributes().flatten() {
                            let val = String::from_utf8_lossy(&a.value).to_string();
                            match a.key.as_ref() {
                                // id / fallback are XML ID/IDREF-typed: normalize
                                // by trimming (epubcheck matches on the normalized
                                // value, so a padded id="… x …" still resolves).
                                b"id" => id = val.trim().to_string(),
                                b"href" => href = val,
                                b"media-type" => mt = val,
                                b"properties" => {
                                    props = val.split_whitespace().map(str::to_string).collect()
                                }
                                b"fallback" => fallback = Some(val.trim().to_string()),
                                _ => {}
                            }
                        }
                        if !id.is_empty() {
                            pkg.manifest.push(ManifestItem {
                                id,
                                href,
                                media_type: mt,
                                properties: props,
                                fallback,
                            });
                        }
                    }
                    b"itemref" if in_spine => {
                        let mut idref = String::new();
                        let mut linear: Option<bool> = None;
                        for a in e.attributes().flatten() {
                            let val = String::from_utf8_lossy(&a.value).to_string();
                            match a.key.as_ref() {
                                // idref is IDREF-typed — normalize like id above.
                                b"idref" => idref = val.trim().to_string(),
                                b"linear" => linear = Some(val.eq_ignore_ascii_case("yes")),
                                _ => {}
                            }
                        }
                        if !idref.is_empty() {
                            pkg.spine.push(SpineItem { idref, linear });
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                // Finalize an in-progress `<dc:*>` element.
                if e.name().as_ref().starts_with(b"dc:")
                    && let Some(mut m) = dc.take()
                {
                    m.value = m.value.trim().to_string();
                    pkg.metadata.push(m);
                }
                match local_name(e.name().as_ref()) {
                    b"manifest" => in_manifest = false,
                    b"spine" => in_spine = false,
                    b"metadata" => in_metadata = false,
                    b"guide" => in_guide = false,
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(io::Error::other(e)),
            _ => {}
        }
    }

    // Derive the identifier views used by OPF-030 / NCX-001 from the metadata.
    pkg.identifier_ids = pkg
        .metadata
        .iter()
        .filter(|m| m.name == "identifier")
        .filter_map(|m| m.id.clone())
        .collect();
    pkg.unique_identifier_value = pkg.unique_identifier.as_deref().and_then(|uid| {
        pkg.metadata
            .iter()
            .find(|m| m.name == "identifier" && m.id.as_deref() == Some(uid))
            .map(|m| m.value.clone())
    });

    Ok(pkg)
}

/// First value of attribute `key` (exact, unprefixed) on `e`, if present.
fn attr(e: &BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .map(|a| String::from_utf8_lossy(&a.value).to_string())
}

/// The `opf:scheme` (or bare `scheme`) attribute value, matched by attribute
/// local name so either the prefixed or unprefixed form is found.
fn scheme_attr(e: &BytesStart) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| local_name(a.key.as_ref()) == b"scheme")
        .map(|a| String::from_utf8_lossy(&a.value).to_string())
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|b| *b == b':').next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest_properties_and_spine_linear() {
        let opf = r#"<?xml version="1.0"?>
<package unique-identifier="pub-id">
  <metadata>
    <dc:identifier id="pub-id">urn:uuid:1234</dc:identifier>
    <dc:title>T</dc:title>
  </metadata>
  <manifest>
    <item id="cover" href="text/cover.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch1" href="text/ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="nav" href="toc.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="cov-img" href="images/cover.jpg" media-type="image/jpeg" properties="cover-image"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="cover" linear="no"/>
    <itemref idref="ch1"/>
  </spine>
  <guide>
    <reference type="cover" href="text/cover.xhtml"/>
  </guide>
</package>"#;
        let pkg = parse(opf).unwrap();
        assert_eq!(pkg.manifest.len(), 5);
        assert_eq!(pkg.manifest[2].properties, vec!["nav"]);
        assert_eq!(pkg.manifest[3].properties, vec!["cover-image"]);
        assert_eq!(pkg.spine.len(), 2);
        assert_eq!(pkg.spine[0].idref, "cover");
        assert_eq!(pkg.spine[0].linear, Some(false));
        assert_eq!(pkg.spine[1].linear, None);
        // Extended captures.
        assert_eq!(pkg.unique_identifier.as_deref(), Some("pub-id"));
        assert_eq!(pkg.identifier_ids, vec!["pub-id"]);
        assert_eq!(
            pkg.unique_identifier_value.as_deref(),
            Some("urn:uuid:1234")
        );
        assert_eq!(pkg.spine_toc.as_deref(), Some("ncx"));
        assert_eq!(pkg.guide_hrefs, vec!["text/cover.xhtml"]);
    }

    #[test]
    fn captures_dc_metadata_elements() {
        let opf = r#"<?xml version="1.0"?>
<package version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>The Title</dc:title>
    <dc:language>en</dc:language>
    <dc:date>2015-08-05</dc:date>
    <dc:identifier id="uid" opf:scheme="uuid">urn:uuid:abcd</dc:identifier>
    <dc:subject/>
    <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>
  </metadata>
  <manifest><item id="a" href="a.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="a"/></spine>
</package>"#;
        let pkg = parse(opf).unwrap();
        // <meta> is not a Dublin Core element, so it is not captured.
        let names: Vec<&str> = pkg.metadata.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["title", "language", "date", "identifier", "subject"]
        );
        let date = pkg.metadata.iter().find(|m| m.name == "date").unwrap();
        assert_eq!(date.value, "2015-08-05");
        let ident = pkg
            .metadata
            .iter()
            .find(|m| m.name == "identifier")
            .unwrap();
        assert_eq!(ident.scheme.as_deref(), Some("uuid"));
        assert_eq!(ident.value, "urn:uuid:abcd");
        // A self-closed <dc:subject/> is captured with an empty value.
        let subject = pkg.metadata.iter().find(|m| m.name == "subject").unwrap();
        assert!(subject.value.is_empty());
        // Derived views still hold after the refactor.
        assert_eq!(pkg.identifier_ids, vec!["uid"]);
        assert_eq!(
            pkg.unique_identifier_value.as_deref(),
            Some("urn:uuid:abcd")
        );
    }

    #[test]
    fn captures_package_link_elements() {
        let opf = r#"<package version="3.0">
  <metadata>
    <link rel="alternate  mapping alternate" href="m.xml" media-type="application/xml"/>
  </metadata>
  <link rel="record" href="onix.xml"/>
</package>"#;
        let pkg = parse(opf).unwrap();
        assert_eq!(pkg.links.len(), 2);
        let m = &pkg.links[0];
        assert_eq!(m.href, "m.xml");
        // rel is deduped and whitespace-collapsed.
        assert_eq!(m.rel, vec!["alternate", "mapping"]);
        assert_eq!(m.media_type.as_deref(), Some("application/xml"));
        assert!(m.in_metadata);
        // The package-level <link> is captured but not flagged in_metadata.
        assert!(!pkg.links[1].in_metadata);
    }

    #[test]
    fn captures_version_and_only_the_unique_identifier_value() {
        // Two identifiers; only the one matching unique-identifier is captured
        // as the value. A leading/trailing-whitespace value is trimmed.
        let opf = r#"<?xml version="1.0"?>
<package version="3.0" unique-identifier="uid">
  <metadata>
    <dc:identifier id="other">urn:isbn:0000</dc:identifier>
    <dc:identifier id="uid">  urn:uuid:abcd  </dc:identifier>
  </metadata>
  <manifest><item id="a" href="a.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="a"/></spine>
</package>"#;
        let pkg = parse(opf).unwrap();
        assert_eq!(pkg.version.as_deref(), Some("3.0"));
        assert_eq!(pkg.identifier_ids, vec!["other", "uid"]);
        assert_eq!(
            pkg.unique_identifier_value.as_deref(),
            Some("urn:uuid:abcd")
        );
    }

    #[test]
    fn captures_fallback_attribute() {
        let opf = r#"<package>
  <manifest>
    <item id="a" href="a.svg" media-type="image/svg+xml" fallback="b"/>
    <item id="b" href="b.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="b"/></spine>
</package>"#;
        let pkg = parse(opf).unwrap();
        assert_eq!(pkg.manifest[0].fallback.as_deref(), Some("b"));
        assert_eq!(pkg.manifest[1].fallback, None);
    }

    #[test]
    fn handles_namespaced_elements() {
        // Some publishers emit `opf:item` etc. Verify local-name matching works.
        let opf = r#"<opf:package xmlns:opf="http://www.idpf.org/2007/opf">
  <opf:manifest>
    <opf:item id="a" href="a.xhtml" media-type="application/xhtml+xml"/>
  </opf:manifest>
  <opf:spine>
    <opf:itemref idref="a"/>
  </opf:spine>
</opf:package>"#;
        let pkg = parse(opf).unwrap();
        assert_eq!(pkg.manifest.len(), 1);
        assert_eq!(pkg.spine.len(), 1);
    }
}
