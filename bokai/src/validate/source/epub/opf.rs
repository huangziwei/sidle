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
    /// The package element's `unique-identifier` attribute (the id it says
    /// points at the publication's `<dc:identifier>`). `None` if absent.
    pub unique_identifier: Option<String>,
    /// The `id` of every `<dc:identifier>` in the metadata.
    pub identifier_ids: Vec<String>,
    /// The `<spine toc>` attribute (a manifest id for the NCX), if present.
    pub spine_toc: Option<String>,
    /// Every `<reference href>` inside `<guide>` (raw, fragment kept).
    pub guide_hrefs: Vec<String>,
    pub manifest: Vec<ManifestItem>,
    pub spine: Vec<SpineItem>,
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

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                match local {
                    b"package" => pkg.unique_identifier = attr(&e, b"unique-identifier"),
                    b"manifest" => in_manifest = true,
                    b"spine" => {
                        in_spine = true;
                        pkg.spine_toc = attr(&e, b"toc");
                    }
                    b"metadata" => in_metadata = true,
                    b"guide" => in_guide = true,
                    b"identifier" if in_metadata => {
                        if let Some(id) = attr(&e, b"id") {
                            pkg.identifier_ids.push(id);
                        }
                    }
                    b"reference" if in_guide => {
                        if let Some(href) = attr(&e, b"href") {
                            pkg.guide_hrefs.push(href);
                        }
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
                                b"id" => id = val,
                                b"href" => href = val,
                                b"media-type" => mt = val,
                                b"properties" => {
                                    props = val.split_whitespace().map(str::to_string).collect()
                                }
                                b"fallback" => fallback = Some(val),
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
                                b"idref" => idref = val,
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
            Ok(Event::End(e)) => match local_name(e.name().as_ref()) {
                b"manifest" => in_manifest = false,
                b"spine" => in_spine = false,
                b"metadata" => in_metadata = false,
                b"guide" => in_guide = false,
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(io::Error::other(e)),
            _ => {}
        }
    }

    Ok(pkg)
}

/// First value of attribute `key` (exact, unprefixed) on `e`, if present.
fn attr(e: &BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
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
        assert_eq!(pkg.spine_toc.as_deref(), Some("ncx"));
        assert_eq!(pkg.guide_hrefs, vec!["text/cover.xhtml"]);
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
