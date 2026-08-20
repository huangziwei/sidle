//! Minimal OPF parser tailored to the EPUB-3 standalone validator.
//!
//! Independent from [`crate::formats::epub::parse_opf`] by design (per
//! `crate::validate` philosophy: a parser-side bug should surface in the
//! validator instead of being silently mirrored). Captures the attributes
//! the standalone validator needs that the full parser drops: `<item
//! properties>`/`<item fallback>`, `<itemref linear>`, the package
//! `unique-identifier`, `<dc:identifier>` ids, the `<spine toc>` NCX pointer,
//! and `<guide>` references.

use std::collections::HashSet;
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
    /// The package element's `prefix` attribute (EPUB 3 vocabulary prefix
    /// declarations), raw. Drives the undeclared-prefix check (OPF-028): a
    /// property token whose prefix is neither reserved nor declared here is
    /// undeclared.
    pub prefix_decl: Option<String>,
    /// The value of the publication-level (non-`refines`) `<meta
    /// property="rendition:layout">`, if declared — `"pre-paginated"` means the
    /// publication default is fixed-layout. Drives the fixed-layout resolution
    /// that gates the FXL viewport check (HTM-046).
    pub rendition_layout: Option<String>,
    /// Every property token used by any package-document property attribute
    /// (`<meta property>`/`scheme`, `<item properties>`, `<itemref properties>`,
    /// `<link rel>`/`properties`), in document order. Drives the undeclared-prefix
    /// check (OPF-028); a set-membership signal, so duplicates are harmless.
    pub property_tokens: Vec<String>,
    /// The `property` of every publication-level `<meta>` — one directly under
    /// `<metadata>` with no `refines`, which is what states a fact about the
    /// publication rather than refining another element. epubcheck keys its
    /// publication-wide features (the media-overlay styling classes among them)
    /// on exactly this set.
    pub publication_properties: HashSet<String>,
    /// Every property-bearing attribute in the package document, with the
    /// context that decides its vocabulary. Drives the property rules
    /// (OPF-012/025/026/027), which need to know *where* a token appeared —
    /// unlike [`Self::property_tokens`], which only needs its prefix.
    pub property_attrs: Vec<PropertyAttr>,
    /// `(id, refined-id)` for every metadata expression that both carries an
    /// `id` and refines another one. The refines graph must be acyclic
    /// (OPF-065); only an expression with an `id` can take part in a cycle,
    /// which is why an edge needs both halves.
    pub refines_edges: Vec<(String, String)>,
    /// Every `<meta property=…>` expression under `<metadata>`, with its text.
    /// The `<dc:*>` elements are [`Self::metadata`]; this is the other half of
    /// the metadata vocabulary.
    pub metas: Vec<MetaExpr>,
    /// Every `<reference href>` inside `<guide>` (raw, fragment kept).
    pub guide_hrefs: Vec<String>,
    /// Every `<link>` element in the package document (EPUB 3 metadata/collection
    /// links). Drives the OPF link rules (OPF-089/093/094/095/098/067, RSC-029).
    pub links: Vec<OpfLink>,
    /// Every `<collection>` in the package document, flattened. A collection
    /// groups resources under a *role* — index, preview, dictionary — and each
    /// role has its own membership rules (OPF-071/075/076/078/081…084). Nesting
    /// is flattened because every rule that recurses (only the index one does)
    /// applies the same test at every depth.
    pub collections: Vec<Collection>,
    pub manifest: Vec<ManifestItem>,
    pub spine: Vec<SpineItem>,
}

/// One property-bearing attribute, kept whole so the property rules can judge
/// it against the right vocabulary.
#[derive(Debug, Clone)]
pub struct PropertyAttr {
    pub context: super::vocab::Context,
    /// The attribute's raw value.
    pub value: String,
    /// The `<item media-type>` this attribute sits on, for OPF-012. `None`
    /// outside [`super::vocab::Context::Item`].
    pub media_type: Option<String>,
}

/// An EPUB 3 `<link>` element in the package document.
#[derive(Debug, Clone)]
pub struct OpfLink {
    /// The `href` attribute (raw, fragment kept). Empty if absent.
    pub href: String,
    /// Whitespace-separated `rel` keywords (deduped, order lost — matches how
    /// epubcheck's vocab parser treats them as a set). Empty if absent.
    pub rel: Vec<String>,
    /// Whitespace-separated `properties` keywords (order lost, like `rel`). Empty
    /// if absent. Feeds the undeclared-prefix check (OPF-028).
    pub properties: Vec<String>,
    /// The `media-type` attribute, if present.
    pub media_type: Option<String>,
    /// True if this `<link>` is a child of `<metadata>` (vs a collection link).
    pub in_metadata: bool,
}

/// An EPUB 3 `<meta property=…>` metadata expression.
#[derive(Debug, Clone, Default)]
pub struct MetaExpr {
    pub property: String,
    /// The element's trimmed text.
    pub value: String,
    /// The `refines` target, with any leading `#` stripped. `None` for a
    /// publication-level expression.
    pub refines: Option<String>,
}

/// An EPUB 3 `<collection>`: a group of resources under a role.
#[derive(Debug, Clone, Default)]
pub struct Collection {
    /// The `role` attribute's whitespace-separated tokens. Empty if absent.
    pub roles: Vec<String>,
    /// The `href` of every `<link>` directly inside this collection (raw,
    /// fragment kept — the preview rule judges the fragment).
    pub hrefs: Vec<String>,
}

impl Collection {
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }
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
    /// The `opf:role` attribute, if present — the EPUB 2 MARC relator code on
    /// a `<dc:creator>`, which OPF-052 judges.
    pub role: Option<String>,
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
    /// The EPUB 2 `fallback-style` attribute (a manifest id), if present. The
    /// EPUB 3 manifest grammar has no such attribute.
    pub fallback_style: Option<String>,
    /// The `media-overlay` attribute (a manifest id naming the SMIL document
    /// that narrates this item), if present. Its presence is what makes this
    /// item the *text* of an overlay, which the overlay-styling rule keys on.
    pub media_overlay: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpineItem {
    pub idref: String,
    /// `Some(true)` if `linear="yes"`, `Some(false)` if `linear="no"`, `None`
    /// if attribute is absent. Per OPF 3.3 the default is `yes`, but we
    /// preserve the distinction so the reachability rule only flags
    /// explicitly-non-linear entries.
    pub linear: Option<bool>,
    /// Space-separated tokens from `properties=`. Empty if absent. Carries the
    /// per-spine-item `rendition:layout-*` override (fixed-layout resolution for
    /// HTM-046) and feeds the undeclared-prefix check (OPF-028).
    pub properties: Vec<String>,
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
    // In-progress `<meta property=…>` expression, accumulated the same way and
    // finalized into `pkg.metas` on `End`.
    let mut pending_meta: Option<MetaExpr> = None;
    // Open `<collection>` elements, innermost last — they nest.
    let mut collections: Vec<Collection> = Vec::new();

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
                    role: role_attr(&e),
                    value: String::new(),
                });
            }
            Ok(Event::Text(t)) if dc.is_some() => {
                if let Some(m) = dc.as_mut() {
                    m.value.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
            }
            Ok(Event::Text(t)) if pending_meta.is_some() => {
                if let Some(m) = pending_meta.as_mut() {
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
                        role: role_attr(&e),
                        value: String::new(),
                    });
                }
                // A metadata expression that refines another and can itself be
                // refined is an edge in the refines graph (OPF-065). A leading
                // `#` makes the value a same-document reference; epubcheck
                // strips it and compares ids.
                if in_metadata
                    && let (Some(id), Some(refines)) = (attr(&e, b"id"), attr(&e, b"refines"))
                {
                    let target = refines.trim().strip_prefix('#').unwrap_or(refines.trim());
                    pkg.refines_edges
                        .push((id.trim().to_string(), target.to_string()));
                }
                match local {
                    b"package" => {
                        pkg.unique_identifier = attr(&e, b"unique-identifier");
                        pkg.version = attr(&e, b"version");
                        pkg.prefix_decl = attr(&e, b"prefix");
                    }
                    b"manifest" => in_manifest = true,
                    b"spine" => {
                        in_spine = true;
                        pkg.spine_toc = attr(&e, b"toc");
                    }
                    b"metadata" => in_metadata = true,
                    // A `<collection>` may nest, so the open ones form a stack;
                    // a `<link>` belongs to the innermost.
                    b"collection" => collections.push(Collection {
                        roles: attr(&e, b"role")
                            .map(|r| r.split_whitespace().map(str::to_string).collect())
                            .unwrap_or_default(),
                        hrefs: Vec::new(),
                    }),
                    // Capture `<meta>` property/scheme tokens for the undeclared-
                    // prefix check (OPF-028), and open the expression so its text
                    // accumulates until `End`. A self-closed `<meta/>` emits no
                    // `End`, so it is closed by whatever `End` comes next — with
                    // the empty value it correctly has, since the arms that
                    // consume text run before this one only when a `<dc:*>` is
                    // open.
                    b"meta" if in_metadata => {
                        for (key, context) in [
                            (b"property".as_slice(), super::vocab::Context::Meta),
                            (b"scheme".as_slice(), super::vocab::Context::Scheme),
                        ] {
                            if let Some(val) = attr(&e, key) {
                                pkg.property_tokens
                                    .extend(val.split_whitespace().map(str::to_string));
                                pkg.property_attrs.push(PropertyAttr {
                                    context,
                                    value: val,
                                    media_type: None,
                                });
                            }
                        }
                        if attr(&e, b"refines").is_none()
                            && let Some(val) = attr(&e, b"property")
                        {
                            pkg.publication_properties
                                .extend(val.split_whitespace().map(str::to_string));
                        }
                        if let Some(property) = attr(&e, b"property") {
                            pending_meta = Some(MetaExpr {
                                property: property.trim().to_string(),
                                value: String::new(),
                                refines: attr(&e, b"refines").map(|r| {
                                    let r = r.trim();
                                    r.strip_prefix('#').unwrap_or(r).to_string()
                                }),
                            });
                        }
                    }
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
                        let properties: Vec<String> = attr(&e, b"properties")
                            .map(|raw| raw.split_whitespace().map(str::to_string).collect())
                            .unwrap_or_default();
                        // `rel` and `properties` are both property lists → feed the
                        // undeclared-prefix check (OPF-028).
                        pkg.property_tokens.extend(rel.iter().cloned());
                        pkg.property_tokens.extend(properties.iter().cloned());
                        for (key, context) in [
                            (b"rel".as_slice(), super::vocab::Context::LinkRel),
                            (
                                b"properties".as_slice(),
                                super::vocab::Context::LinkProperties,
                            ),
                        ] {
                            if let Some(value) = attr(&e, key) {
                                pkg.property_attrs.push(PropertyAttr {
                                    context,
                                    value,
                                    media_type: None,
                                });
                            }
                        }
                        let href = attr(&e, b"href").unwrap_or_default();
                        if let Some(open) = collections.last_mut() {
                            open.hrefs.push(href.clone());
                        }
                        pkg.links.push(OpfLink {
                            href,
                            rel,
                            properties,
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
                        let mut fallback_style = None;
                        let mut media_overlay = None;
                        for a in e.attributes().flatten() {
                            let val = super::attr_value(&a);
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
                                b"fallback-style" => fallback_style = Some(val.trim().to_string()),
                                b"media-overlay" => media_overlay = Some(val.trim().to_string()),
                                _ => {}
                            }
                        }
                        if !id.is_empty() {
                            pkg.property_tokens.extend(props.iter().cloned());
                            if !props.is_empty() {
                                pkg.property_attrs.push(PropertyAttr {
                                    context: super::vocab::Context::Item,
                                    value: props.join(" "),
                                    media_type: Some(mt.clone()),
                                });
                            }
                            pkg.manifest.push(ManifestItem {
                                id,
                                href,
                                media_type: mt,
                                properties: props,
                                fallback,
                                fallback_style,
                                media_overlay,
                            });
                        }
                    }
                    b"itemref" if in_spine => {
                        let mut idref = String::new();
                        let mut linear: Option<bool> = None;
                        let mut properties: Vec<String> = Vec::new();
                        for a in e.attributes().flatten() {
                            let val = super::attr_value(&a);
                            match a.key.as_ref() {
                                // idref is IDREF-typed — normalize like id above.
                                b"idref" => idref = val.trim().to_string(),
                                b"linear" => linear = Some(val.eq_ignore_ascii_case("yes")),
                                b"properties" => {
                                    properties =
                                        val.split_whitespace().map(str::to_string).collect()
                                }
                                _ => {}
                            }
                        }
                        if !idref.is_empty() {
                            pkg.property_tokens.extend(properties.iter().cloned());
                            if !properties.is_empty() {
                                pkg.property_attrs.push(PropertyAttr {
                                    context: super::vocab::Context::Itemref,
                                    value: properties.join(" "),
                                    media_type: None,
                                });
                            }
                            pkg.spine.push(SpineItem {
                                idref,
                                linear,
                                properties,
                            });
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
                // Finalize an in-progress `<meta>` expression.
                if let Some(mut m) = pending_meta.take() {
                    m.value = m.value.trim().to_string();
                    pkg.metas.push(m);
                }
                match local_name(e.name().as_ref()) {
                    b"manifest" => in_manifest = false,
                    b"spine" => in_spine = false,
                    b"metadata" => in_metadata = false,
                    b"guide" => in_guide = false,
                    b"collection" => {
                        if let Some(finished) = collections.pop() {
                            pkg.collections.push(finished);
                        }
                    }
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
    // The publication-level layout default. A `refines`-scoped one is a per-item
    // override, which epubcheck does not use for the fixed-layout viewport gate.
    pkg.rendition_layout = pkg
        .metas
        .iter()
        .find(|m| m.property == "rendition:layout" && m.refines.is_none())
        .map(|m| m.value.clone());

    Ok(pkg)
}

/// First value of attribute `key` (exact, unprefixed) on `e`, if present.
fn attr(e: &BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .map(|a| super::attr_value(&a))
}

/// The `opf:scheme` (or bare `scheme`) attribute value, matched by attribute
/// local name so either the prefixed or unprefixed form is found.
fn scheme_attr(e: &BytesStart) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| local_name(a.key.as_ref()) == b"scheme")
        .map(|a| super::attr_value(&a))
}

fn role_attr(e: &BytesStart) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| local_name(a.key.as_ref()) == b"role")
        .map(|a| super::attr_value(&a))
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
        // The derived views agree with the parsed metadata.
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
    fn captures_prefix_layout_and_property_tokens() {
        // The itemref carries a per-item fixed-layout override plus an undeclared
        // `access:` prefix; the package declares `foaf:`; the publication default
        // is reflowable via a primary rendition:layout meta.
        let opf = r##"<package version="3.0" unique-identifier="uid"
              prefix="foaf: http://xmlns.com/foaf/spec/">
          <metadata>
            <dc:title>T</dc:title>
            <dc:identifier id="uid">urn:uuid:1</dc:identifier>
            <meta property="rendition:layout">reflowable</meta>
            <meta property="schema:accessMode">textual</meta>
          </metadata>
          <manifest>
            <item id="c" href="c.xhtml" media-type="application/xhtml+xml" properties="svg"/>
            <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
          </manifest>
          <spine>
            <itemref idref="c" properties="rendition:layout-pre-paginated access:scroll-both"/>
            <itemref idref="nav" linear="no"/>
          </spine>
        </package>"##;
        let pkg = parse(opf).unwrap();
        assert_eq!(
            pkg.prefix_decl.as_deref(),
            Some("foaf: http://xmlns.com/foaf/spec/")
        );
        assert_eq!(pkg.rendition_layout.as_deref(), Some("reflowable"));
        assert_eq!(
            pkg.spine[0].properties,
            vec!["rendition:layout-pre-paginated", "access:scroll-both"]
        );
        // Every property token, across meta/item/itemref, is pooled for OPF-028.
        for tok in [
            "rendition:layout",
            "schema:accessMode",
            "svg",
            "nav",
            "rendition:layout-pre-paginated",
            "access:scroll-both",
        ] {
            assert!(
                pkg.property_tokens.iter().any(|t| t == tok),
                "property_tokens missing {tok:?}: {:?}",
                pkg.property_tokens
            );
        }
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
