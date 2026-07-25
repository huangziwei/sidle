//! The DTD entity catalogue — which named entities a `<!DOCTYPE>` brings into
//! scope.
//!
//! XML predefines five entities and always resolves numeric character
//! references; every other `&name;` must be *declared*, or the document is not
//! well-formed. Most EPUB 2 content declares nothing itself and writes `&nbsp;`
//! anyway, relying on the DTD its DOCTYPE names — so deciding whether an entity
//! reference is undeclared means knowing what each public identifier defines.
//!
//! epubcheck resolves those identifiers against a fixed table of vendored DTDs
//! ([`DefaultResolver`'s system-id map][resolver]) and never fetches anything
//! else while offline. The same DTDs are vendored here, so this module reads
//! them rather than transcribing their contents: [`entities`] parses the
//! `<!ENTITY name …>` declarations out of the file a DOCTYPE resolves to and
//! follows its external parameter entities through the vendored set.
//!
//! [resolver]: https://github.com/w3c/epubcheck/blob/main/src/main/java/com/adobe/epubcheck/xml/handlers/DefaultResolver.java
//!
//! **What is judged.** A DOCTYPE this table does not have, or one whose DTD
//! references a file that is not vendored, yields `None` — the caller then knows
//! nothing about that document's entities and must stay silent. Only a
//! completely resolved DTD produces a name set.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// The vendored DTDs, by the file name other DTDs reference them with.
///
/// `.ent` files are vendored under a `.dtdinc` extension, which is how
/// epubcheck stores them; [`resolve`] maps the reference back.
static FILES: &[(&str, &str)] = &[
    (
        "dtbook-2005-2.dtd",
        include_str!("schema/20/dtd/dtbook-2005-2.dtd"),
    ),
    (
        "ncx-2005-1.dtd",
        include_str!("schema/20/dtd/ncx-2005-1.dtd"),
    ),
    ("oeb12.ent", include_str!("schema/20/dtd/oeb12.dtdinc")),
    ("oebdoc12.dtd", include_str!("schema/20/dtd/oebdoc12.dtd")),
    ("oebpkg12.dtd", include_str!("schema/20/dtd/oebpkg12.dtd")),
    ("opf20.dtd", include_str!("schema/20/dtd/opf20.dtd")),
    ("svg11.dtd", include_str!("schema/20/dtd/svg11.dtd")),
    (
        "xhtml-lat1.ent",
        include_str!("schema/20/dtd/xhtml-lat1.dtdinc"),
    ),
    (
        "xhtml-special.ent",
        include_str!("schema/20/dtd/xhtml-special.dtdinc"),
    ),
    (
        "xhtml-symbol.ent",
        include_str!("schema/20/dtd/xhtml-symbol.dtdinc"),
    ),
    (
        "xhtml1-strict.dtd",
        include_str!("schema/20/dtd/xhtml1-strict.dtd"),
    ),
    (
        "xhtml1-transitional.dtd",
        include_str!("schema/20/dtd/xhtml1-transitional.dtd"),
    ),
    (
        "xhtml11-ent.dtd",
        include_str!("schema/20/dtd/xhtml11-ent.dtd"),
    ),
];

/// The system identifiers epubcheck resolves, and the vendored file each maps
/// to. A DOCTYPE naming anything else is not resolved offline, so this table's
/// keys are exactly the DOCTYPEs whose entities can be known.
static SYSTEM_IDS: &[(&str, &str)] = &[
    // OEB 1.2
    (
        "http://openebook.org/dtds/oeb-1.2/oebpkg12.dtd",
        "oebpkg12.dtd",
    ),
    (
        "http://http://idpf.org/dtds/oeb-1.2/oebpkg12.dtd",
        "oebpkg12.dtd",
    ),
    ("http://openebook.org/dtds/oeb-1.2/oeb12.ent", "oeb12.ent"),
    (
        "http://openebook.org/dtds/oeb-1.2/oebdoc12.dtd",
        "oebdoc12.dtd",
    ),
    // OPF 2.0
    ("http://www.idpf.org/dtds/2007/opf.dtd", "opf20.dtd"),
    // XHTML 1.0
    (
        "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd",
        "xhtml1-transitional.dtd",
    ),
    (
        "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd",
        "xhtml1-strict.dtd",
    ),
    (
        "http://www.w3.org/TR/xhtml1/DTD/xhtml-lat1.ent",
        "xhtml-lat1.ent",
    ),
    (
        "http://www.w3.org/TR/xhtml1/DTD/xhtml-symbol.ent",
        "xhtml-symbol.ent",
    ),
    (
        "http://www.w3.org/TR/xhtml1/DTD/xhtml-special.ent",
        "xhtml-special.ent",
    ),
    // SVG 1.1
    (
        "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd",
        "svg11.dtd",
    ),
    // DAISY
    (
        "http://www.daisy.org/z3986/2005/dtbook-2005-2.dtd",
        "dtbook-2005-2.dtd",
    ),
    (
        "http://www.daisy.org/z3986/2005/ncx-2005-1.dtd",
        "ncx-2005-1.dtd",
    ),
    // XHTML 1.1 resolves to the entity declarations only: epubcheck validates
    // the grammar with RELAX NG, so the DTD is wanted for its characters alone.
    (
        "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd",
        "xhtml11-ent.dtd",
    ),
    (
        "http://www.w3.org/MarkUp/DTD/xhtml11.dtd",
        "xhtml11-ent.dtd",
    ),
];

/// The general entity names a DOCTYPE's external subset declares.
///
/// `None` means "unknown, do not judge": the system identifier is not one
/// epubcheck resolves offline, or the DTD it names pulls in a file that is not
/// vendored (SVG 1.1 and DTBook are modular, and their modules are not), so the
/// set would be incomplete. `Some` is always a complete set.
///
/// An EPUB 3 document resolves *nothing* — epubcheck hands its parser an empty
/// source for every external identifier — so callers pass no system id there and
/// get the empty set from [`no_external_subset`].
pub fn entities(system_id: &str) -> Option<&'static HashSet<String>> {
    static CACHE: OnceLock<HashMap<&'static str, HashSet<String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        let files: HashMap<&str, &str> = FILES.iter().copied().collect();
        SYSTEM_IDS
            .iter()
            .filter_map(|(system_id, file)| Some((*system_id, resolve(file, &files)?)))
            .collect()
    });
    cache.get(system_id)
}

/// The empty set, for a document with no external subset to resolve — a DOCTYPE
/// with no external identifier (`<!DOCTYPE html>`), no DOCTYPE at all, or any
/// EPUB 3 document.
pub fn no_external_subset() -> &'static HashSet<String> {
    static EMPTY: OnceLock<HashSet<String>> = OnceLock::new();
    EMPTY.get_or_init(HashSet::new)
}

/// Every general entity `file` declares, following its external parameter
/// entities. `None` when any of them is not vendored, which makes the set
/// incomplete and so unusable.
fn resolve(file: &str, files: &HashMap<&str, &str>) -> Option<HashSet<String>> {
    let mut out = HashSet::new();
    let mut pending = vec![file.to_string()];
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(name) = pending.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let text = files.get(name.as_str())?;
        let (declared, includes) = declarations(text);
        out.extend(declared);
        pending.extend(includes);
    }
    Some(out)
}

/// The general entity names declared in one DTD, and the file names its
/// external parameter entities reference.
///
/// Deliberately simple: the vendored DTDs are hand-written W3C/IDPF files whose
/// declarations all take the plain `<!ENTITY name "value">` /
/// `<!ENTITY % name PUBLIC "…" "file">` shapes. A declaration inside an ignored
/// marked section would be over-counted, which can only *silence* a finding.
fn declarations(text: &str) -> (Vec<String>, Vec<String>) {
    let (mut declared, mut includes) = (Vec::new(), Vec::new());
    let mut rest = text;
    while let Some(at) = rest.find("<!ENTITY") {
        rest = &rest[at + "<!ENTITY".len()..];
        let Some(end) = rest.find('>') else { break };
        let body = &rest[..end];
        rest = &rest[end..];
        let body = body.trim_start();
        match body.strip_prefix('%') {
            // A general entity: the name, then its replacement text.
            None => {
                let name = body.split_whitespace().next().unwrap_or_default();
                if !name.is_empty() {
                    declared.push(name.to_string());
                }
            }
            // A parameter entity. Only an external one contributes: its last
            // quoted string is the system identifier of the file it pulls in.
            Some(parameter) => {
                if (parameter.contains("PUBLIC") || parameter.contains("SYSTEM"))
                    && let Some(system_id) = quoted(parameter).last()
                {
                    includes.push(base_name(system_id).to_string());
                }
            }
        }
    }
    (declared, includes)
}

/// Every `"…"`/`'…'`-quoted run in a declaration body, in order.
fn quoted(body: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(open) = rest.find(['"', '\'']) {
        let quote = rest.as_bytes()[open] as char;
        let after = &rest[open + 1..];
        let Some(close) = after.find(quote) else {
            break;
        };
        out.push(&after[..close]);
        rest = &after[close + 1..];
    }
    out
}

/// The file name part of a system identifier, which is how [`FILES`] is keyed —
/// the DTDs reference each other both relatively (`xhtml-lat1.ent`) and by
/// absolute URL.
fn base_name(system_id: &str) -> &str {
    system_id.rsplit('/').next().unwrap_or(system_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_xhtml_entity_sets_resolve_completely() {
        // XHTML 1.1 declares all 253 characters in one file; XHTML 1.0 reaches
        // the same set through three external parameter entities. Both must
        // arrive at the same place, or the include-following is broken.
        let xhtml11 = entities("http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd").expect("xhtml 1.1");
        let strict = entities("http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd")
            .expect("xhtml 1.0 strict");
        for name in ["nbsp", "mdash", "eacute", "hellip", "trade", "alpha"] {
            assert!(xhtml11.contains(name), "xhtml 1.1 is missing {name}");
            assert!(strict.contains(name), "xhtml 1.0 is missing {name}");
        }
        assert_eq!(xhtml11.len(), 253);
        assert_eq!(xhtml11, strict, "the two spellings must agree exactly");
    }

    #[test]
    fn a_dtd_whose_modules_are_not_vendored_is_unknown() {
        // SVG 1.1 and DTBook are modular and their modules are not vendored, so
        // the catalogue must refuse to answer rather than answer incompletely.
        for system_id in [
            "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd",
            "http://www.daisy.org/z3986/2005/dtbook-2005-2.dtd",
        ] {
            assert!(entities(system_id).is_none(), "{system_id}");
        }
        // A DTD nobody vendored is equally unknown.
        assert!(entities("http://example.org/made-up.dtd").is_none());
    }

    #[test]
    fn a_self_contained_dtd_resolves_to_exactly_what_it_declares() {
        // The NCX declares no general entities at all, so `&nbsp;` in an NCX is
        // undeclared — an empty set is an answer, not an absence of one.
        let ncx = entities("http://www.daisy.org/z3986/2005/ncx-2005-1.dtd").expect("ncx");
        assert!(ncx.is_empty());
        // The OEB 1.2 package DTD reaches its 253 characters through one
        // external parameter entity.
        let oeb = entities("http://openebook.org/dtds/oeb-1.2/oebpkg12.dtd").expect("oeb 1.2");
        assert!(oeb.contains("nbsp"), "the OEB entity set did not resolve");
    }

    #[test]
    fn declarations_reads_names_and_includes() {
        let (declared, includes) = declarations(
            r#"<!ENTITY nbsp "&#160;">
               <!ENTITY % HTMLlat1 PUBLIC "-//W3C//ENTITIES Latin 1//EN" "xhtml-lat1.ent">
               %HTMLlat1;
               <!ENTITY % URI "CDATA">
               <!ENTITY  mdash   "&#8212;" >"#,
        );
        assert_eq!(declared, ["nbsp", "mdash"]);
        // An *internal* parameter entity declares nothing and pulls in nothing.
        assert_eq!(includes, ["xhtml-lat1.ent"]);
    }
}
