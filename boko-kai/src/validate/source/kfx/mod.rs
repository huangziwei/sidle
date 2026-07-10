//! KFX standalone structural validator — the "is this KFX well-formed on its
//! own?" check (job 2). No such tool exists anywhere else; this is boko's
//! `epubcheck` equivalent for KFX. It reads a single container natively (never a
//! derived/converted copy) and reports defects **in the source book** for the
//! book editor to repair.
//!
//! Every rule is a check over structures boko already parses (`kfx/container.rs`,
//! `kfx_to_epub/loader.rs` → [`BookData`]), so the checker just re-asks the
//! resolution questions the converter answers — and flags the cases the
//! converter silently tolerates (a dropped image, a chapterless nav).
//!
//! Rule catalog (see the validator-architecture plan). This increment implements
//! four rules; the deeper reference-resolution walks are the next increment.
//!
//! - **Rule 1, container integrity** — the container parses at all (`CONT`
//!   magic, info + index in bounds). A parse failure is one hard
//!   `container-unreadable` error; the fine-grained per-entity offset check is
//!   folded into this (an out-of-bounds entity surfaces transitively via rules
//!   2/7).
//! - **Rule 2, required entities** — `document_data`, ≥1 `section`, ≥1
//!   `storyline` (hard errors); `book_navigation` (a warning — no chapter list).
//! - **Rule 7, resource refs resolve** — every `external_resource` that names a
//!   `location` has its bytes embedded in the container.
//! - **Rule 9, cover present + resolves** — the declared cover resource exists
//!   and has embedded bytes (missing cover = warning; dangling = error).
//!
//! Deferred: rule 3 (section → storyline), 4 (content refs), 5 (nav reachability
//! — must reuse the `section_position_id_map` exemption from `fidelity::nav` to
//! avoid false-flagging legitimate cover / section-root positions), 6 (style
//! refs), 8 (position-map coverage). Rule 10 (TOC deficiency) is contributed by
//! the cross-format `source::toc` check via [`crate::validate::source::validate`].

use std::collections::HashMap;

use crate::kfx::container::get_field;
use crate::kfx::ion::IonValue;
use crate::kfx::symbols::KfxSymbol;
use crate::kfx_to_epub::loader::{self, BookData};

use crate::validate::{Finding, FixHint, Severity};

/// Run the KFX structural rules over `bytes` and return the defects as unified
/// [`Finding`]s. If the container will not even parse, that is the single
/// `container-unreadable` error (rule 1's catastrophic case) and no further
/// rules run. Consumed by [`crate::validate::source::validate`] for the KFX
/// branch; the TOC audit (rule 10) is added there separately.
pub fn validate(bytes: &[u8]) -> Vec<Finding> {
    let book = match loader::load(bytes) {
        Ok(book) => book,
        Err(e) => {
            return vec![error(
                "container-unreadable",
                "<container>",
                format!("KFX container did not parse: {e}"),
            )];
        }
    };

    let mut findings = Vec::new();
    findings.extend(check_required_entities(&book.by_type));
    findings.extend(check_resource_bytes(&book.by_type, &book.raw_media));
    findings.extend(check_cover(&book));
    findings
}

// ============================================================================
// Rule 2 — required entities present
// ============================================================================

fn check_required_entities(by_type: &HashMap<u64, HashMap<String, IonValue>>) -> Vec<Finding> {
    let present = |sym: KfxSymbol| {
        by_type
            .get(&(sym as u64))
            .is_some_and(|entities| !entities.is_empty())
    };
    let mut out = Vec::new();

    if !present(KfxSymbol::DocumentData) {
        out.push(error(
            "no-document-data",
            "<container>",
            "no document_data ($538) entity — the book declares no reading order",
        ));
    }
    if !present(KfxSymbol::Section) {
        out.push(error(
            "no-sections",
            "<container>",
            "no section ($260) entities — the book has no pages",
        ));
    }
    if !present(KfxSymbol::Storyline) {
        out.push(error(
            "no-storylines",
            "<container>",
            "no storyline ($259) entities — the book has no content",
        ));
    }
    if !present(KfxSymbol::BookNavigation) {
        out.push(warning(
            "no-nav",
            "<container>",
            "no book_navigation ($389) — the reader will show no chapter list",
            Some(FixHint::new(
                "add-nav",
                "generate a book_navigation container from the book's headings",
            )),
        ));
    }
    out
}

// ============================================================================
// Rule 7 — external-resource bytes are embedded
// ============================================================================

fn check_resource_bytes(
    by_type: &HashMap<u64, HashMap<String, IonValue>>,
    raw_media: &HashMap<String, Vec<u8>>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(resources) = by_type.get(&(KfxSymbol::ExternalResource as u64)) else {
        return out;
    };

    // Deterministic order — HashMap iteration is random and findings are shown
    // to the user / diffed across corpus runs.
    let mut names: Vec<&String> = resources.keys().collect();
    names.sort();

    for name in names {
        let Some(fields) = resources[name].unwrap_annotated().as_struct() else {
            continue;
        };
        // A resource with no `location` ($165) is a location-less descriptor,
        // not a dangling byte reference — nothing to resolve, so skip it.
        let Some(location) =
            get_field(fields, KfxSymbol::Location as u64).and_then(|v| v.as_string())
        else {
            continue;
        };
        if !raw_media.contains_key(location) {
            out.push(error(
                "resource-missing-bytes",
                location,
                format!(
                    "external_resource {name:?} points to location {location:?} but no embedded bytes exist for it"
                ),
            ));
        }
    }
    out
}

// ============================================================================
// Rule 9 — cover present and resolves
// ============================================================================

fn check_cover(book: &BookData) -> Vec<Finding> {
    let Some(cover_name) = book.metadata.cover_resource_name.as_deref() else {
        return vec![warning(
            "cover-missing",
            "<metadata>",
            "no cover image is declared for this book",
            Some(FixHint::new(
                "add-cover",
                "set a cover image in the book's metadata",
            )),
        )];
    };

    if cover_resource_resolves(book, cover_name) {
        Vec::new()
    } else {
        vec![error(
            "cover-unresolved",
            "<metadata>",
            format!("cover names resource {cover_name:?} but it has no embedded image bytes"),
        )]
    }
}

/// True when some `external_resource` is named `cover_name` (by its
/// `resource_name` field, or its entity id as a fallback) and its `location`
/// resolves to embedded bytes.
fn cover_resource_resolves(book: &BookData, cover_name: &str) -> bool {
    let Some(resources) = book.by_type.get(&(KfxSymbol::ExternalResource as u64)) else {
        return false;
    };
    resources.iter().any(|(fid, value)| {
        let Some(fields) = value.unwrap_annotated().as_struct() else {
            return false;
        };
        let resource_name = get_field(fields, KfxSymbol::ResourceName as u64)
            .and_then(|v| book.symbols.text_of(v))
            .unwrap_or(fid);
        if resource_name != cover_name {
            return false;
        }
        get_field(fields, KfxSymbol::Location as u64)
            .and_then(|v| v.as_string())
            .is_some_and(|location| book.raw_media.contains_key(location))
    })
}

// ============================================================================
// Finding constructors
// ============================================================================

fn error(rule: &str, location: &str, message: impl Into<String>) -> Finding {
    Finding {
        check: "kfx",
        rule: rule.to_string(),
        severity: Severity::Error,
        location: location.to_string(),
        message: message.into(),
        fix: None,
    }
}

fn warning(
    rule: &str,
    location: &str,
    message: impl Into<String>,
    fix: Option<FixHint>,
) -> Finding {
    Finding {
        check: "kfx",
        rule: rule.to_string(),
        severity: Severity::Warning,
        location: location.to_string(),
        message: message.into(),
        fix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(location: Option<&str>) -> IonValue {
        let mut fields = Vec::new();
        if let Some(loc) = location {
            fields.push((
                KfxSymbol::Location as u64,
                IonValue::String(loc.to_string()),
            ));
        }
        IonValue::Struct(fields)
    }

    fn entities(pairs: Vec<(u64, &str, IonValue)>) -> HashMap<u64, HashMap<String, IonValue>> {
        let mut by_type: HashMap<u64, HashMap<String, IonValue>> = HashMap::new();
        for (ftype, fid, value) in pairs {
            by_type
                .entry(ftype)
                .or_default()
                .insert(fid.to_string(), value);
        }
        by_type
    }

    #[test]
    fn required_entities_flags_all_missing() {
        let out = check_required_entities(&HashMap::new());
        let rules: Vec<&str> = out.iter().map(|f| f.rule.as_str()).collect();
        assert!(rules.contains(&"no-document-data"));
        assert!(rules.contains(&"no-sections"));
        assert!(rules.contains(&"no-storylines"));
        assert!(rules.contains(&"no-nav"));
        // document_data / section / storyline are hard errors; nav is a warning.
        assert_eq!(
            out.iter().filter(|f| f.severity == Severity::Error).count(),
            3
        );
        let nav = out.iter().find(|f| f.rule == "no-nav").unwrap();
        assert_eq!(nav.severity, Severity::Warning);
        assert_eq!(nav.fix.as_ref().unwrap().action, "add-nav");
    }

    #[test]
    fn required_entities_clean_when_all_present() {
        let by_type = entities(vec![
            (
                KfxSymbol::DocumentData as u64,
                "d",
                IonValue::Struct(vec![]),
            ),
            (KfxSymbol::Section as u64, "s1", IonValue::Struct(vec![])),
            (KfxSymbol::Storyline as u64, "st1", IonValue::Struct(vec![])),
            (
                KfxSymbol::BookNavigation as u64,
                "n",
                IonValue::Struct(vec![]),
            ),
        ]);
        assert!(check_required_entities(&by_type).is_empty());
    }

    #[test]
    fn resource_bytes_flags_dangling_location() {
        let by_type = entities(vec![
            (
                KfxSymbol::ExternalResource as u64,
                "r1",
                resource(Some("resource/present")),
            ),
            (
                KfxSymbol::ExternalResource as u64,
                "r2",
                resource(Some("resource/missing")),
            ),
            // Location-less descriptor — must be skipped, not flagged.
            (KfxSymbol::ExternalResource as u64, "r3", resource(None)),
        ]);
        let mut raw_media: HashMap<String, Vec<u8>> = HashMap::new();
        raw_media.insert("resource/present".to_string(), vec![0xFF, 0xD8]);

        let out = check_resource_bytes(&by_type, &raw_media);
        assert_eq!(out.len(), 1, "only the missing-bytes resource is flagged");
        assert_eq!(out[0].rule, "resource-missing-bytes");
        assert_eq!(out[0].severity, Severity::Error);
        assert_eq!(out[0].location, "resource/missing");
    }

    #[test]
    fn resource_bytes_clean_when_no_resources() {
        assert!(check_resource_bytes(&HashMap::new(), &HashMap::new()).is_empty());
    }
}
