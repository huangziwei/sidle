//! The transformations an EPUB 3 document undergoes *before* a schema sees it.
//!
//! epubcheck does not hand the parsed document straight to its grammars. A SAX
//! filter rewrites it first, and the grammars are written against the rewritten
//! form — so a validator that skips this step reports errors epubcheck does not.
//! The clearest case is `data-*`: HTML allows any `data-` attribute anywhere,
//! RELAX NG cannot express "any name with this prefix", and epubcheck resolves
//! the mismatch by *deleting* those attributes on the way in. Run the grammar
//! without deleting them and every `data-*` in the book becomes a false RSC-005.
//!
//! Two of the transformations also report on their way past, which is why this
//! is a validation step and not a private detail of the tree:
//!
//! - a `data-*` name that HTML does not allow — `HTM-061`;
//! - an attribute in a namespace that is not one of the ones XHTML knows, but
//!   whose host claims `w3.org` or `idpf.org` — `HTM-054`.
//!
//! Everything here is EPUB 3 only. An EPUB 2 document goes to `20/rng` exactly
//! as written.

use super::tree::Document;

/// The namespaces an XHTML content document may use. An attribute in any other
/// namespace is dropped before validation.
const KNOWN_XHTML_NAMESPACES: &[&str] = &[
    "http://www.w3.org/1999/xhtml",
    "http://www.w3.org/XML/1998/namespace",
    "http://www.idpf.org/2007/ops",
    "http://www.w3.org/2000/svg",
    "http://www.w3.org/1998/Math/MathML",
    "http://www.w3.org/2001/10/synthesis",
    "http://www.w3.org/2001/xml-events",
    "http://www.w3.org/1999/xlink",
];

/// The namespace an HTML custom element is moved into, which is where the
/// vendored html5 grammar declares the wildcard that accepts it.
const CUSTOM_ELEMENTS_NS: &str = "http://n.validator.nu/custom-elements/";

const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";

/// Attributes HTML defines as having a case-insensitive value — the boolean and
/// enumerated ones. Their values are folded to lower case so a grammar can
/// enumerate them in one case only.
const CASE_INSENSITIVE_ATTRIBUTES: &[&str] = &[
    "align",
    "allowfullscreen",
    "allowpaymentrequest",
    "allowusermedia",
    "async",
    "autocapitalize",
    "autocomplete",
    "autofocus",
    "autoplay",
    "checked",
    "contenteditable",
    "controls",
    "crossorigin",
    "default",
    "defer",
    "dir",
    "disabled",
    "draggable",
    "formnovalidate",
    "hidden",
    "http-equiv",
    "ismap",
    "itemscope",
    "kind",
    "loop",
    "multiple",
    "muted",
    "nomodule",
    "novalidate",
    "open",
    "playsinline",
    "preload",
    "readonly",
    "required",
    "reversed",
    "scope",
    "selected",
    "shape",
    "sizes",
    "spellcheck",
    "step",
    "translate",
    "type",
    "typemustmatch",
    "valign",
    "value",
    "wrap",
];

/// Which document is being preprocessed. The rules differ: an SVG content
/// document loses its `data-*` attributes like an XHTML one, but keeps its
/// foreign-namespace attributes and its case as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    Xhtml,
    Svg,
}

/// Something the preprocessing pass found on its way past.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// `HTM-061` — a `data-` attribute whose name HTML does not permit.
    InvalidDataAttribute { line: u32, name: String },
    /// `HTM-054` — an attribute in a namespace outside XHTML's own set, whose
    /// host claims a reserved organisation.
    ReservedCustomNamespace {
        line: u32,
        namespace: String,
        reserved: String,
    },
}

/// Apply the EPUB 3 preprocessing to `doc` in place, returning what it found.
pub fn preprocess(doc: &mut Document, kind: DocumentKind) -> Vec<Note> {
    let mut notes = Vec::new();
    let custom_ns = doc.intern_namespace(CUSTOM_ELEMENTS_NS);
    let xhtml_ns = doc.ns_id(XHTML_NS);

    for id in doc.descendants(doc.root()) {
        let Some(element) = doc.element(id) else {
            continue;
        };
        let line = doc.line(id);
        let in_xhtml = element.name.ns == xhtml_ns && xhtml_ns.is_some();
        // A custom element is any XHTML-namespace element whose name contains a
        // hyphen. HTML says such a name is always an author's own element, so
        // it moves to the namespace where the grammar's wildcard accepts it.
        let is_custom_element =
            kind == DocumentKind::Xhtml && in_xhtml && element.name.local.contains('-');

        // Decide everything against the immutable view first; the borrow
        // checker is right that reading and editing at once is not safe here,
        // and the two-pass shape keeps the rules readable besides.
        let mut drop = Vec::new();
        let mut lower = Vec::new();
        for (index, attr) in element.attrs.iter().enumerate() {
            let namespace = doc.expanded(&attr.name).0;
            let local = attr.name.local.as_str();
            match namespace {
                None if local.starts_with("data-") => {
                    if !is_valid_data_attribute(local) {
                        notes.push(Note::InvalidDataAttribute {
                            line,
                            name: local.to_string(),
                        });
                    }
                    drop.push(index);
                }
                None if local.starts_with("its-") => drop.push(index),
                None if kind == DocumentKind::Xhtml
                    && in_xhtml
                    && CASE_INSENSITIVE_ATTRIBUTES.contains(&local) =>
                {
                    lower.push(index);
                }
                Some(uri)
                    if kind == DocumentKind::Xhtml
                        && !KNOWN_XHTML_NAMESPACES.contains(&uri.trim()) =>
                {
                    if let Some(reserved) = reserved_host(uri) {
                        notes.push(Note::ReservedCustomNamespace {
                            line,
                            namespace: uri.to_string(),
                            reserved: reserved.to_string(),
                        });
                    }
                    drop.push(index);
                }
                _ => {}
            }
        }

        if drop.is_empty() && lower.is_empty() && !is_custom_element {
            continue;
        }
        let element = doc.element_mut(id).expect("checked above");
        if is_custom_element {
            element.name.ns = Some(custom_ns);
        }
        for index in lower {
            element.attrs[index].value = element.attrs[index].value.to_lowercase();
        }
        for index in drop.into_iter().rev() {
            element.attrs.remove(index);
        }
    }
    notes
}

/// HTML's rule for a custom data attribute: something after `data-`, an XML
/// name, and no upper-case letter (because HTML's own parser lower-cases them,
/// so an upper-case one could never round-trip).
fn is_valid_data_attribute(name: &str) -> bool {
    let rest = &name["data-".len()..];
    !rest.is_empty() && is_ncname(rest) && !rest.chars().any(|c| c.is_ascii_uppercase())
}

/// Whether `s` is an XML `NCName` — a name with no colon in it.
fn is_ncname(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | '\u{b7}'))
}

/// The organisation a custom namespace is impersonating, if any. epubcheck
/// reports a namespace whose *host* contains `w3.org` or `idpf.org` but which is
/// not one of the namespaces XHTML actually uses, because that is nearly always
/// a typo in a real namespace rather than a deliberate private one.
fn reserved_host(uri: &str) -> Option<&'static str> {
    let host = uri
        .split_once("://")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split(['/', '?', '#']).next())?;
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host.split_once(':').map_or(host, |(h, _)| h);
    ["w3.org", "idpf.org"]
        .into_iter()
        .find(|reserved| host.contains(reserved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::source::epub::xml::tree::NodeId;

    fn parse(xml: &str) -> Document {
        Document::parse(xml).expect("well-formed test document")
    }

    fn attrs(doc: &Document, id: NodeId) -> Vec<String> {
        doc.element(id)
            .expect("element")
            .attrs
            .iter()
            .map(|a| a.name.local.clone())
            .collect()
    }

    #[test]
    fn data_attributes_are_removed_and_bad_ones_reported() {
        let mut doc = parse(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
            <p data-ok="1" data-Bad="2" data-="3" id="k">x</p></body></html>"#,
        );
        let notes = preprocess(&mut doc, DocumentKind::Xhtml);
        let p = doc
            .descendants(doc.root())
            .into_iter()
            .find(|n| doc.element(*n).is_some_and(|e| e.name.local == "p"))
            .expect("the p element");
        assert_eq!(
            attrs(&doc, p),
            ["id"],
            "every data-* attribute is gone, valid or not"
        );
        assert_eq!(
            notes,
            [
                Note::InvalidDataAttribute {
                    line: 2,
                    name: "data-Bad".into()
                },
                Note::InvalidDataAttribute {
                    line: 2,
                    name: "data-".into()
                },
            ],
            "an upper-case letter and an empty name are both invalid"
        );
    }

    #[test]
    fn custom_namespace_attributes_are_removed_and_reserved_hosts_reported() {
        let mut doc = parse(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"
                     xmlns:mine="http://example.org/ns"
                     xmlns:typo="http://www.idpf.org/2007/opps"
                     xmlns:ops="http://www.idpf.org/2007/ops"><body>
            <p mine:a="1" typo:b="2" ops:type="pagebreak">x</p></body></html>"#,
        );
        let notes = preprocess(&mut doc, DocumentKind::Xhtml);
        let p = doc
            .descendants(doc.root())
            .into_iter()
            .find(|n| doc.element(*n).is_some_and(|e| e.name.local == "p"))
            .expect("the p element");
        assert_eq!(
            attrs(&doc, p),
            ["type"],
            "a known namespace survives, unknown ones do not"
        );
        assert_eq!(
            notes,
            [Note::ReservedCustomNamespace {
                line: 5,
                namespace: "http://www.idpf.org/2007/opps".into(),
                reserved: "idpf.org".into(),
            }],
            "only the one impersonating a reserved host is reported"
        );
    }

    #[test]
    fn custom_elements_move_to_their_own_namespace() {
        let mut doc =
            parse(r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><my-widget/></body></html>"#);
        preprocess(&mut doc, DocumentKind::Xhtml);
        let widget = doc
            .descendants(doc.root())
            .into_iter()
            .find(|n| doc.element(*n).is_some_and(|e| e.name.local == "my-widget"))
            .expect("the custom element");
        let name = &doc.element(widget).expect("element").name;
        assert_eq!(doc.expanded(name).0, Some(CUSTOM_ELEMENTS_NS));
    }

    #[test]
    fn case_insensitive_attribute_values_are_folded() {
        let mut doc = parse(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
            <p dir="LTR" title="Keep This">x</p></body></html>"#,
        );
        preprocess(&mut doc, DocumentKind::Xhtml);
        let p = doc
            .descendants(doc.root())
            .into_iter()
            .find(|n| doc.element(*n).is_some_and(|e| e.name.local == "p"))
            .expect("the p element");
        assert_eq!(doc.attr(p, None, "dir"), Some("ltr"));
        assert_eq!(
            doc.attr(p, None, "title"),
            Some("Keep This"),
            "only the attributes HTML defines as case-insensitive are folded"
        );
    }

    #[test]
    fn svg_keeps_what_only_xhtml_loses() {
        let mut doc = parse(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:mine="http://example.org/ns"
                    data-x="1" mine:a="2" type="TEXT"/>"#,
        );
        let notes = preprocess(&mut doc, DocumentKind::Svg);
        let root = doc.root_element().expect("root");
        assert_eq!(
            attrs(&doc, root),
            ["a", "type"],
            "data-* goes in SVG too, but a foreign-namespace attribute stays"
        );
        assert_eq!(
            doc.attr(root, None, "type"),
            Some("TEXT"),
            "no case folding"
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn a_reserved_host_is_matched_on_the_host_only() {
        assert_eq!(
            reserved_host("http://www.idpf.org/2007/opps"),
            Some("idpf.org")
        );
        assert_eq!(reserved_host("https://w3.org/ns/x"), Some("w3.org"));
        assert_eq!(reserved_host("http://example.org/w3.org/x"), None);
        assert_eq!(reserved_host("urn:x:y"), None);
    }
}
