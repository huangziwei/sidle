//! The package document's property vocabularies.

/// Where a property appears, which decides its default vocabulary and whether
/// a whitespace-separated list is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    /// `<item properties>`.
    Item,
    /// `<itemref properties>`.
    Itemref,
    /// `<meta property>`.
    Meta,
    /// `<meta scheme>`.
    Scheme,
    /// `<link rel>`.
    LinkRel,
    /// `<link properties>`.
    LinkProperties,
}

impl Context {
    /// The attribute this context names, for a message.
    pub fn attribute(self) -> &'static str {
        match self {
            Context::Item | Context::Itemref | Context::LinkProperties => "properties",
            Context::Meta => "property",
            Context::Scheme => "scheme",
            Context::LinkRel => "rel",
        }
    }

    /// Whether the attribute holds a whitespace-separated *list*. Only the two
    /// `<meta>` attributes take a single value, which is what `OPF-025` is for.
    pub fn allows_list(self) -> bool {
        !matches!(self, Context::Meta | Context::Scheme)
    }

    /// The names the default (unprefixed) vocabulary defines here.
    fn default_vocabulary(self) -> &'static [&'static str] {
        match self {
            Context::Item => &[
                "cover-image",
                "data-nav",
                "dictionary",
                "glossary",
                "index",
                "mathml",
                "nav",
                "remote-resources",
                "scripted",
                "search-key-map",
                "svg",
                "switch",
            ],
            Context::Itemref => &["page-spread-left", "page-spread-right"],
            // `META_VOCAB` plus the one camel-cased member, `pageBreakSource`.
            Context::Meta | Context::Scheme => &[
                "alternate-script",
                "authority",
                "belongs-to-collection",
                "collection-type",
                "dictionary-type",
                "display-seq",
                "file-as",
                "group-position",
                "identifier-type",
                "meta-auth",
                "pageBreakSource",
                "role",
                "source-language",
                "source-of",
                "target-language",
                "term",
                "title-type",
            ],
            Context::LinkRel => &[
                "acquire",
                "alternate",
                "marc21xml-record",
                "mods-record",
                "onix-record",
                "record",
                "voicing",
                "xml-signature",
                "xmp-record",
            ],
            Context::LinkProperties => &["onix"],
        }
    }

    /// The names a *reserved-prefix* vocabulary defines here, or `None` when
    /// this module does not have that vocabulary's complete member list and so
    /// must not judge it.
    fn prefixed_vocabulary(self, prefix: &str) -> Option<&'static [&'static str]> {
        match (prefix, self) {
            ("rendition", Context::Meta) => Some(&["flow", "layout", "orientation", "spread"]),
            ("rendition", Context::Itemref) => Some(&[
                "flow-auto",
                "flow-paginated",
                "flow-scrolled-continuous",
                "flow-scrolled-doc",
                "layout-pre-paginated",
                "layout-reflowable",
                "orientation-auto",
                "orientation-landscape",
                "orientation-portrait",
                "page-spread-center",
                "page-spread-left",
                "page-spread-right",
                "spread-auto",
                "spread-both",
                "spread-landscape",
                "spread-none",
            ]),
            ("media", Context::Meta) => Some(&[
                "active-class",
                "duration",
                "narrator",
                "playback-active-class",
            ]),
            // The media-overlays vocabulary defines nothing for an `<item>` or
            ("media", Context::Item | Context::Itemref) => Some(&[]),
            _ => None,
        }
    }
}

/// The media types an `<item>` property is defined for. `None` means this
/// module does not constrain it (which never fires `OPF-012`).
fn item_property_types(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "cover-image" => &["image/"],
        "data-nav" => &["application/xhtml+xml"],
        "dictionary" => &["application/vnd.epub.search-key-map+xml"],
        "glossary" => &[
            "application/vnd.epub.search-key-map+xml",
            "application/xhtml+xml",
        ],
        "index" => &["application/xhtml+xml"],
        "mathml" => &["application/xhtml+xml", "image/svg+xml"],
        "nav" => &["application/xhtml+xml"],
        "remote-resources" => &[
            "application/xhtml+xml",
            "application/smil+xml",
            "image/svg+xml",
            "text/css",
        ],
        "scripted" => &["application/xhtml+xml", "image/svg+xml"],
        "search-key-map" => &["application/vnd.epub.search-key-map+xml"],
        "svg" => &["application/xhtml+xml"],
        "switch" => &["application/xhtml+xml", "image/svg+xml"],
        _ => return None,
    })
}

/// What is wrong with one property, if anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Defect {
    /// `OPF-026` — a `prefix:name` with an empty half.
    Malformed { property: String },
    /// `OPF-027` — a name the vocabulary for its prefix does not define.
    Undefined { property: String },
    /// `OPF-012` — defined, but not for this item's media type.
    WrongMediaType {
        property: String,
        media_type: String,
    },
}

/// Judge one property attribute's value.
pub fn check(
    context: Context,
    value: &str,
    media_type: Option<&str>,
) -> Result<Vec<Defect>, String> {
    let properties: Vec<&str> = value.split_whitespace().collect();
    if !context.allows_list() && properties.len() > 1 {
        return Err(value.to_string());
    }
    let mut out = Vec::new();
    for property in properties {
        let (prefix, name) = match property.split_once(':') {
            None => ("", property),
            Some((prefix, name)) => (prefix, name),
        };
        if property.contains(':') && (prefix.is_empty() || name.is_empty()) {
            out.push(Defect::Malformed {
                property: property.to_string(),
            });
            continue;
        }
        let vocabulary = match prefix.is_empty() {
            true => Some(context.default_vocabulary()),
            false => context.prefixed_vocabulary(prefix),
        };
        // A vocabulary this module does not have in full is not judged.
        let Some(vocabulary) = vocabulary else {
            continue;
        };
        if !vocabulary.contains(&name) {
            out.push(Defect::Undefined {
                property: property.to_string(),
            });
            continue;
        }
        if context != Context::Item {
            continue;
        }
        let media_type = media_type.unwrap_or_default().trim();
        if let Some(types) = item_property_types(name)
            && !types
                .iter()
                .any(|t| *t == media_type || t.ends_with('/') && media_type.starts_with(t))
        {
            out.push(Defect::WrongMediaType {
                property: property.to_string(),
                media_type: media_type.to_string(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defects(context: Context, value: &str, media_type: Option<&str>) -> Vec<Defect> {
        check(context, value, media_type).expect("not a list violation")
    }

    #[test]
    fn a_well_formed_package_produces_nothing() {
        assert!(defects(Context::Item, "nav scripted", Some("application/xhtml+xml")).is_empty());
        assert!(defects(Context::Item, "cover-image", Some("image/jpeg")).is_empty());
        assert!(defects(Context::Itemref, "page-spread-left", None).is_empty());
        assert!(defects(Context::Meta, "dcterms:modified", None).is_empty());
        assert!(defects(Context::Meta, "media:duration", None).is_empty());
        assert!(defects(Context::Meta, "rendition:layout", None).is_empty());
        assert!(defects(Context::Itemref, "rendition:spread-none", None).is_empty());
        assert!(defects(Context::LinkRel, "record voicing", None).is_empty());
        assert!(defects(Context::Meta, "pageBreakSource", None).is_empty());
    }

    #[test]
    fn a_malformed_property_is_reported_and_not_looked_up() {
        for bad in [":name", "prefix:"] {
            assert_eq!(
                defects(Context::Meta, bad, None),
                [Defect::Malformed {
                    property: bad.to_string()
                }]
            );
        }
    }

    #[test]
    fn an_undefined_name_is_reported_only_in_a_known_vocabulary() {
        assert_eq!(
            defects(Context::Item, "nosuch", None),
            [Defect::Undefined {
                property: "nosuch".to_string()
            }]
        );
        assert_eq!(
            defects(Context::Meta, "rendition:nosuch", None),
            [Defect::Undefined {
                property: "rendition:nosuch".to_string()
            }]
        );
        // `media:` defines nothing for an item, so any name there is undefined.
        assert_eq!(
            defects(
                Context::Item,
                "media:duration",
                Some("application/xhtml+xml")
            ),
            [Defect::Undefined {
                property: "media:duration".to_string()
            }]
        );
        // A vocabulary this module does not carry in full is never judged.
        for unknown in ["a11y:certifiedBy", "schema:accessMode", "mine:whatever"] {
            assert!(
                defects(Context::Meta, unknown, None).is_empty(),
                "{unknown}"
            );
        }
        // A property defined in one context can be undefined in another.
        assert!(defects(Context::Itemref, "page-spread-left", None).is_empty());
        assert_eq!(
            defects(
                Context::Item,
                "page-spread-left",
                Some("application/xhtml+xml")
            )
            .len(),
            1
        );
    }

    #[test]
    fn an_item_property_must_suit_the_items_media_type() {
        assert_eq!(
            defects(Context::Item, "nav", Some("image/svg+xml")),
            [Defect::WrongMediaType {
                property: "nav".to_string(),
                media_type: "image/svg+xml".to_string()
            }]
        );
        // `image/*` is a prefix match.
        assert!(defects(Context::Item, "cover-image", Some("image/png")).is_empty());
        assert_eq!(
            defects(Context::Item, "cover-image", Some("application/xhtml+xml")).len(),
            1
        );
        // `mathml` suits both content document types.
        for mt in ["application/xhtml+xml", "image/svg+xml"] {
            assert!(defects(Context::Item, "mathml", Some(mt)).is_empty());
        }
    }

    #[test]
    fn only_the_meta_attributes_take_a_single_value() {
        assert_eq!(
            check(Context::Meta, "role file-as", None),
            Err("role file-as".to_string())
        );
        assert!(check(Context::Item, "nav scripted", Some("application/xhtml+xml")).is_ok());
        assert!(check(Context::LinkRel, "record voicing", None).is_ok());
    }
}
