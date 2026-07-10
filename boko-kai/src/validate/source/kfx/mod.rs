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
//! Rule catalog (see the validator-architecture plan). Each rule re-asks a
//! resolution question the converter answers, so it can never flag a shape the
//! converter itself resolves.
//!
//! - **Rule 1, container integrity** — the container parses at all (`CONT`
//!   magic, info + index in bounds). A parse failure is one hard
//!   `container-unreadable` error; the fine-grained per-entity offset check is
//!   folded into this (an out-of-bounds entity surfaces transitively via rules
//!   2/7).
//! - **Rule 2, required entities** — `document_data`, ≥1 `section`, ≥1
//!   `storyline` (hard errors); `book_navigation` (a warning — no chapter list).
//! - **Rule 3, reading order resolves** — every reading-order section names a
//!   real `section` ($260), and every `story_name` ($176) reachable from a
//!   section names a real `storyline` ($259). A dangling ref is a missing
//!   chapter / missing chapter body (hard errors). Mirrors the converter's
//!   `process_section` / `collect_element_ids` walk.
//! - **Rule 4, content refs resolve** — every `$145 content` `{name,index}`
//!   indirection resolves to a real shared `$145 content` block (hard error —
//!   that text is otherwise silently dropped). Mirrors `resolve_content_text`.
//! - **Rule 5, nav reachability** — every navigation entry targets an element a
//!   storyline contains; a dangling target tap-jumps to nowhere (a warning).
//!   Delegates to `fidelity::nav`'s corpus-tested extraction (cover / section-
//!   root positions exempt via `cover_target`).
//! - **Rule 6, style refs resolve** — every `style` ($157) an element cites
//!   names a real `style` entity. A dangling style renders unstyled (a warning —
//!   the converter tolerates it by emitting no CSS). Mirrors `style_decl_for`.
//! - **Rule 7, resource refs resolve** — every `external_resource` that names a
//!   `location` has its bytes embedded in the container.
//! - **Rule 8, position-map coverage** — every reading-order section appears in
//!   the `position_map` ($264), so a device "go to location" can reach it (a
//!   warning; only checked when a position_map exists, since some valid KFX
//!   address purely by `position_id_map` $265).
//! - **Rule 9, cover present + resolves** — the declared cover resource exists
//!   and has embedded bytes (missing cover = warning; dangling = error).
//!
//! Rule 10 (TOC deficiency) is contributed by the cross-format `source::toc`
//! check via [`crate::validate::source::validate`]. Not yet handled: `.kfx-zip`
//! bundles sniff as EPUB by their `PK` magic (single `.kfx` containers are the
//! editor's case).

use std::collections::{BTreeSet, HashMap, HashSet};

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
    findings.extend(check_references(&book));
    findings.extend(check_resource_bytes(&book.by_type, &book.raw_media));
    findings.extend(check_cover(&book));
    findings.extend(check_nav_reachability(bytes));
    findings.extend(check_position_map_coverage(&book));
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
// Rules 3 & 6 — reference resolution (section → storyline, element → style)
// ============================================================================

/// Reference-resolution defects, accumulated **deduped by target name**: a style
/// cited by 10 000 elements, or a storyline referenced from many places, yields
/// one finding — not one per citation. `BTreeSet` also sorts the names, so the
/// finding order is deterministic across runs.
#[derive(Default)]
struct RefDefects {
    /// Reading-order entries that don't resolve to a `section` ($260).
    missing_sections: BTreeSet<String>,
    /// `story_name` ($176) refs that don't resolve to a `storyline` ($259).
    missing_stories: BTreeSet<String>,
    /// `style` ($157) refs that don't resolve to a `style` entity.
    missing_styles: BTreeSet<String>,
    /// `content` ($145) `{name,index}` indirections that don't resolve to a
    /// `$145 content` string (rule 4).
    missing_content: BTreeSet<String>,
}

/// Rules 3 & 6. Walk the content graph the converter renders — reading order →
/// section → page_templates → (referenced storylines) — and flag every named
/// reference that doesn't resolve to a real entity. Starting from the reading
/// order (not every `section` entity) matches the converter: an orphan section
/// outside the reading order is dead content the reader never sees, so a
/// dangling ref inside it is not a defect worth surfacing.
fn check_references(book: &BookData) -> Vec<Finding> {
    let mut defects = RefDefects::default();
    // A single visited set guards against cycles for both storyline and
    // structure fragments; names are namespaced so a storyline and a structure
    // that happen to share a name don't shadow each other.
    let mut visited: HashSet<String> = HashSet::new();

    for section_name in reading_order_sections(book) {
        match lookup(book, KfxSymbol::Section, &section_name) {
            Some(section) => walk_refs(section, book, &mut visited, &mut defects),
            None => {
                defects.missing_sections.insert(section_name);
            }
        }
    }

    let mut findings = Vec::new();
    for name in &defects.missing_sections {
        findings.push(error(
            "section-unresolved",
            name,
            format!(
                "reading order names section {name:?} but no such section entity exists — the chapter is missing"
            ),
        ));
    }
    for name in &defects.missing_stories {
        findings.push(error(
            "story-unresolved",
            name,
            format!(
                "a story_name references storyline {name:?} but no such storyline entity exists — its body is missing"
            ),
        ));
    }
    for name in &defects.missing_styles {
        findings.push(warning(
            "style-unresolved",
            name,
            format!("an element cites style {name:?} but no such style entity exists — it renders unstyled"),
            Some(FixHint::new(
                "define-style",
                "add the missing style entity, or drop the dangling reference from the element",
            )),
        ));
    }
    for name in &defects.missing_content {
        findings.push(error(
            "content-unresolved",
            name,
            format!(
                "an element's content references shared block {name:?} ($145) but it doesn't resolve — that text is missing"
            ),
        ));
    }
    findings
}

/// If `value` is a `$145 content` `{name,index}` indirection (per the converter's
/// `resolve_content_text`) that does **not** resolve to a `$145 content` string,
/// return the dangling block name. Returns `None` for inline text (a plain
/// string) and for anything without a `name` — those aren't indirections.
fn dangling_content_ref(value: &IonValue, book: &BookData) -> Option<String> {
    let fields = value.unwrap_annotated().as_struct()?;
    let name = get_field(fields, KfxSymbol::Name as u64).and_then(|v| book.symbols.text_of(v))?;
    if name.is_empty() {
        return None;
    }
    let index = get_field(fields, KfxSymbol::Index as u64)
        .and_then(|v| v.as_int())
        .unwrap_or(0) as usize;
    let resolves = lookup(book, KfxSymbol::Content, name)
        .and_then(|entry| entry.unwrap_annotated().as_struct())
        .and_then(|fs| get_field(fs, KfxSymbol::ContentList as u64))
        .and_then(|v| v.as_list())
        .and_then(|list| list.get(index))
        .and_then(|item| item.as_string())
        .is_some();
    (!resolves).then(|| name.to_string())
}

/// The flat, in-order list of reading-order section names, from `document_data`
/// ($538) or the older flat `metadata` ($258). A read-only mirror of the
/// converter's `extract_reading_orders` — kept here rather than exposing that
/// `pub(super)` helper across the module tree.
fn reading_order_sections(book: &BookData) -> Vec<String> {
    for type_id in [KfxSymbol::DocumentData as u64, KfxSymbol::Metadata as u64] {
        let Some(map) = book.by_type.get(&type_id) else {
            continue;
        };
        let mut names = Vec::new();
        for value in map.values() {
            let Some(fields) = value.unwrap_annotated().as_struct() else {
                continue;
            };
            let Some(orders) =
                get_field(fields, KfxSymbol::ReadingOrders as u64).and_then(|v| v.as_list())
            else {
                continue;
            };
            for order in orders {
                let Some(order_fields) = order.unwrap_annotated().as_struct() else {
                    continue;
                };
                let Some(sections) =
                    get_field(order_fields, KfxSymbol::Sections as u64).and_then(|v| v.as_list())
                else {
                    continue;
                };
                for section in sections {
                    if let Some(name) = book.symbols.text_of(section) {
                        names.push(name.to_string());
                    }
                }
            }
        }
        if !names.is_empty() {
            return names;
        }
    }
    Vec::new()
}

/// Depth-first walk of a content value, recording dangling `style` (rule 6) and
/// `story_name` (rule 3) references. Follows `story_name` into its storyline and
/// `$608 structure` symbols into their fragment — the two indirections the
/// converter follows (`walk_ids_recursive`, `process_content`) — so the walk
/// reaches the whole rendered body. A bare symbol that *doesn't* resolve to a
/// structure is left alone: bare symbols are overwhelmingly enum values, not
/// fragment references, so flagging them would false-positive.
fn walk_refs(
    value: &IonValue,
    book: &BookData,
    visited: &mut HashSet<String>,
    out: &mut RefDefects,
) {
    match value.unwrap_annotated() {
        IonValue::Symbol(id) => {
            let name = book.symbols.resolve(*id).to_string();
            if let Some(frag) = lookup(book, KfxSymbol::Structure, &name)
                && visited.insert(format!("struct:{name}"))
            {
                walk_refs(frag, book, visited, out);
            }
        }
        IonValue::Struct(fields) => {
            // Rule 6 — the element's `$157 style`, when it's a *named* reference
            // (a symbol/string, per `text_of`); an inline style struct has no
            // name to resolve and is walked as an ordinary child below.
            if let Some(style_name) =
                get_field(fields, KfxSymbol::Style as u64).and_then(|v| book.symbols.text_of(v))
                && !style_exists(book, style_name)
            {
                out.missing_styles.insert(style_name.to_string());
            }

            // Rule 4 — a `$145 content` value that's a `{name,index}`
            // indirection must resolve to a `$145 content` string.
            if let Some(content_val) = get_field(fields, KfxSymbol::Content as u64)
                && let Some(name) = dangling_content_ref(content_val, book)
            {
                out.missing_content.insert(name);
            }

            // Rule 3 — `$176 story_name` must resolve; descend into the storyline
            // (once) so nested story references are checked too.
            if let Some(story_name) =
                get_field(fields, KfxSymbol::StoryName as u64).and_then(|v| book.symbols.text_of(v))
            {
                match lookup(book, KfxSymbol::Storyline, story_name) {
                    Some(storyline) => {
                        if visited.insert(format!("story:{story_name}")) {
                            walk_refs(storyline, book, visited, out);
                        }
                    }
                    None => {
                        out.missing_stories.insert(story_name.to_string());
                    }
                }
            }

            for (_, v) in fields {
                walk_refs(v, book, visited, out);
            }
        }
        IonValue::List(items) => {
            for item in items {
                walk_refs(item, book, visited, out);
            }
        }
        _ => {}
    }
}

/// True when `name` is a defined `style` ($157) entity.
fn style_exists(book: &BookData, name: &str) -> bool {
    book.by_type
        .get(&(KfxSymbol::Style as u64))
        .is_some_and(|styles| styles.contains_key(name))
}

/// Look up a fragment by type and name. A local mirror of the converter's
/// `lookup_fragment` (which is `pub(super)` to `kfx_to_epub`).
fn lookup<'b>(book: &'b BookData, ftype: KfxSymbol, fid: &str) -> Option<&'b IonValue> {
    book.by_type.get(&(ftype as u64)).and_then(|m| m.get(fid))
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
// Rule 5 — nav reachability
// ============================================================================

/// Rule 5. Every navigation entry (chapter list / headings) must jump to an
/// element some storyline actually contains; a dangling target tap-jumps to
/// nowhere on device. Delegates to `fidelity::nav`'s corpus-tested KFX
/// extraction so this checker and the conversion-fidelity `nav` diff apply the
/// exact same reachability rule (cover / section-root positions exempt). A
/// warning, not an error: the book still reads, only the broken entry misbehaves.
///
/// Takes raw `bytes` because the nav extractor reads the container directly
/// (independent of `loader::load`); a parse failure here is already reported as
/// `container-unreadable` by rule 1, so it adds nothing.
fn check_nav_reachability(bytes: &[u8]) -> Vec<Finding> {
    let Ok(dangling) = crate::validate::fidelity::nav::dangling_nav_targets(bytes) else {
        return Vec::new();
    };
    dangling
        .into_iter()
        .map(|eid| {
            warning(
                "nav-unreachable",
                &format!("eid:{eid}"),
                format!(
                    "a navigation entry targets element {eid} but no storyline contains it — tapping it jumps nowhere"
                ),
                Some(FixHint::new(
                    "fix-nav-target",
                    "repoint the navigation entry at a real element, or remove it",
                )),
            )
        })
        .collect()
}

// ============================================================================
// Rule 8 — position-map coverage
// ============================================================================

/// Rule 8. The `position_map` ($264) maps each section to the element ids it
/// contains; the device uses it to resolve "go to location N". A reading-order
/// section absent from it can't be reached by a location jump. A warning (the
/// book still opens and scrolls); only checked when a `position_map` exists at
/// all, since some valid KFX address purely by `position_id_map` ($265).
fn check_position_map_coverage(book: &BookData) -> Vec<Finding> {
    let sections = reading_order_sections(book);
    if sections.is_empty() {
        return Vec::new(); // no reading order → rules 2/3 already speak.
    }
    let Some(pmaps) = book.by_type.get(&(KfxSymbol::PositionMap as u64)) else {
        return Vec::new(); // no position_map — not this rule's business.
    };

    // Every section the position_map covers (its `section_name` $174 symbols).
    let mut covered: HashSet<String> = HashSet::new();
    for value in pmaps.values() {
        let Some(list) = value.unwrap_annotated().as_list() else {
            continue;
        };
        for section in list {
            let Some(fields) = section.unwrap_annotated().as_struct() else {
                continue;
            };
            if let Some(name) = get_field(fields, KfxSymbol::SectionName as u64)
                .and_then(|v| book.symbols.text_of(v))
            {
                covered.insert(name.to_string());
            }
        }
    }
    if covered.is_empty() {
        return Vec::new(); // a position_map we couldn't read — don't guess.
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for section in sections {
        if !covered.contains(&section) && seen.insert(section.clone()) {
            out.push(warning(
                "position-map-gap",
                &section,
                format!(
                    "section {section:?} is in the reading order but absent from the position_map — device \"go to location\" can't reach it"
                ),
                Some(FixHint::new(
                    "rebuild-position-map",
                    "regenerate the position_map so it covers every reading-order section",
                )),
            ));
        }
    }
    out
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

    // --- Rules 3 & 6 (reference resolution) --------------------------------
    //
    // References are built as `IonValue::String`s, not symbols, so `text_of`
    // resolves them without a populated doc-symbol table (`empty_book_for_test`
    // has none). `by_type` is a public field we can populate directly.

    /// `document_data` whose one reading order lists `section_names` in order.
    fn doc_data(section_names: &[&str]) -> HashMap<String, IonValue> {
        let secs: Vec<IonValue> = section_names
            .iter()
            .map(|s| IonValue::String(s.to_string()))
            .collect();
        let order = IonValue::Struct(vec![(KfxSymbol::Sections as u64, IonValue::List(secs))]);
        let dd = IonValue::Struct(vec![(
            KfxSymbol::ReadingOrders as u64,
            IonValue::List(vec![order]),
        )]);
        HashMap::from([("doc".to_string(), dd)])
    }

    /// One section per `(section_name, story_name, cited_style)`: its single
    /// page_template references the story via `$176` and, when `cited_style` is
    /// `Some`, cites a style via `$157`.
    fn sections(specs: &[(&str, &str, Option<&str>)]) -> HashMap<String, IonValue> {
        let mut m = HashMap::new();
        for (sec, story, style) in specs {
            let mut tpl = vec![(
                KfxSymbol::StoryName as u64,
                IonValue::String(story.to_string()),
            )];
            if let Some(sty) = style {
                tpl.push((KfxSymbol::Style as u64, IonValue::String(sty.to_string())));
            }
            let section = IonValue::Struct(vec![(
                KfxSymbol::PageTemplates as u64,
                IonValue::List(vec![IonValue::Struct(tpl)]),
            )]);
            m.insert(sec.to_string(), section);
        }
        m
    }

    /// A minimal storyline entity — existence is all the reference check needs.
    fn one_entity(name: &str) -> HashMap<String, IonValue> {
        HashMap::from([(name.to_string(), IonValue::Struct(vec![]))])
    }

    #[test]
    fn references_clean_when_all_resolve() {
        let mut book = loader::empty_book_for_test();
        book.by_type
            .insert(KfxSymbol::DocumentData as u64, doc_data(&["sec1"]));
        book.by_type.insert(
            KfxSymbol::Section as u64,
            sections(&[("sec1", "story1", Some("st1"))]),
        );
        book.by_type
            .insert(KfxSymbol::Storyline as u64, one_entity("story1"));
        book.by_type
            .insert(KfxSymbol::Style as u64, one_entity("st1"));
        assert!(check_references(&book).is_empty());
    }

    #[test]
    fn references_flag_missing_section_and_story() {
        let mut book = loader::empty_book_for_test();
        book.by_type.insert(
            KfxSymbol::DocumentData as u64,
            doc_data(&["sec1", "secMissing"]),
        );
        // sec1 exists but points at a storyline that doesn't; secMissing has no
        // section entity at all.
        book.by_type.insert(
            KfxSymbol::Section as u64,
            sections(&[("sec1", "storyMissing", None)]),
        );

        let out = check_references(&book);
        assert!(
            out.iter()
                .all(|f| f.severity == Severity::Error && f.check == "kfx")
        );
        assert!(
            out.iter()
                .any(|f| f.rule == "section-unresolved" && f.location == "secMissing")
        );
        assert!(
            out.iter()
                .any(|f| f.rule == "story-unresolved" && f.location == "storyMissing")
        );
    }

    #[test]
    fn references_flag_missing_style_as_warning() {
        let mut book = loader::empty_book_for_test();
        book.by_type
            .insert(KfxSymbol::DocumentData as u64, doc_data(&["sec1"]));
        book.by_type.insert(
            KfxSymbol::Section as u64,
            sections(&[("sec1", "story1", Some("styMissing"))]),
        );
        book.by_type
            .insert(KfxSymbol::Storyline as u64, one_entity("story1"));
        // No style entities exist.

        let out = check_references(&book);
        assert_eq!(out.len(), 1, "only the dangling style is flagged");
        assert_eq!(out[0].rule, "style-unresolved");
        assert_eq!(out[0].severity, Severity::Warning);
        assert_eq!(out[0].location, "styMissing");
        assert_eq!(out[0].fix.as_ref().unwrap().action, "define-style");
    }

    #[test]
    fn references_dedupe_repeated_missing_reference() {
        // Two sections cite the same missing style → exactly one finding.
        let mut book = loader::empty_book_for_test();
        book.by_type
            .insert(KfxSymbol::DocumentData as u64, doc_data(&["s1", "s2"]));
        book.by_type.insert(
            KfxSymbol::Section as u64,
            sections(&[
                ("s1", "story1", Some("styMissing")),
                ("s2", "story1", Some("styMissing")),
            ]),
        );
        book.by_type
            .insert(KfxSymbol::Storyline as u64, one_entity("story1"));

        let out = check_references(&book);
        assert_eq!(
            out.iter().filter(|f| f.rule == "style-unresolved").count(),
            1
        );
    }

    /// A section whose one page_template holds a text element whose `$145
    /// content` is a `{name}` indirection at `block_name`.
    fn section_with_content_ref(block_name: &str) -> HashMap<String, IonValue> {
        let text_elem = IonValue::Struct(vec![(
            KfxSymbol::Content as u64,
            IonValue::Struct(vec![(
                KfxSymbol::Name as u64,
                IonValue::String(block_name.to_string()),
            )]),
        )]);
        let template = IonValue::Struct(vec![(
            KfxSymbol::ContentList as u64,
            IonValue::List(vec![text_elem]),
        )]);
        let section = IonValue::Struct(vec![(
            KfxSymbol::PageTemplates as u64,
            IonValue::List(vec![template]),
        )]);
        HashMap::from([("sec1".to_string(), section)])
    }

    #[test]
    fn references_flag_dangling_content_indirection() {
        let mut book = loader::empty_book_for_test();
        book.by_type
            .insert(KfxSymbol::DocumentData as u64, doc_data(&["sec1"]));
        book.by_type.insert(
            KfxSymbol::Section as u64,
            section_with_content_ref("blockMissing"),
        );
        // No `$145 content` block "blockMissing" exists.
        let out = check_references(&book);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rule, "content-unresolved");
        assert_eq!(out[0].severity, Severity::Error);
        assert_eq!(out[0].location, "blockMissing");
    }

    #[test]
    fn references_content_indirection_resolves() {
        let mut book = loader::empty_book_for_test();
        book.by_type
            .insert(KfxSymbol::DocumentData as u64, doc_data(&["sec1"]));
        book.by_type.insert(
            KfxSymbol::Section as u64,
            section_with_content_ref("block1"),
        );
        // `$145 content` block "block1" whose content_list[0] is a string.
        book.by_type.insert(
            KfxSymbol::Content as u64,
            HashMap::from([(
                "block1".to_string(),
                IonValue::Struct(vec![(
                    KfxSymbol::ContentList as u64,
                    IonValue::List(vec![IonValue::String("hi".to_string())]),
                )]),
            )]),
        );
        assert!(check_references(&book).is_empty());
    }

    // --- Rule 8 (position-map coverage) ------------------------------------

    /// A `position_map` ($264) covering `sections` (each a `{section_name,
    /// contains}` struct; `contains` is irrelevant to the coverage check).
    fn position_map(covered: &[&str]) -> HashMap<String, IonValue> {
        let secs: Vec<IonValue> = covered
            .iter()
            .map(|s| {
                IonValue::Struct(vec![
                    (
                        KfxSymbol::SectionName as u64,
                        IonValue::String(s.to_string()),
                    ),
                    (KfxSymbol::Contains as u64, IonValue::List(vec![])),
                ])
            })
            .collect();
        HashMap::from([("pmap".to_string(), IonValue::List(secs))])
    }

    #[test]
    fn position_map_gap_flags_uncovered_section() {
        let mut book = loader::empty_book_for_test();
        book.by_type
            .insert(KfxSymbol::DocumentData as u64, doc_data(&["sec1", "sec2"]));
        // The position_map covers sec1 but not sec2.
        book.by_type
            .insert(KfxSymbol::PositionMap as u64, position_map(&["sec1"]));

        let out = check_position_map_coverage(&book);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rule, "position-map-gap");
        assert_eq!(out[0].severity, Severity::Warning);
        assert_eq!(out[0].location, "sec2");
    }

    #[test]
    fn position_map_clean_when_all_covered() {
        let mut book = loader::empty_book_for_test();
        book.by_type
            .insert(KfxSymbol::DocumentData as u64, doc_data(&["sec1", "sec2"]));
        book.by_type.insert(
            KfxSymbol::PositionMap as u64,
            position_map(&["sec1", "sec2"]),
        );
        assert!(check_position_map_coverage(&book).is_empty());
    }

    #[test]
    fn position_map_absent_is_skipped() {
        let mut book = loader::empty_book_for_test();
        book.by_type
            .insert(KfxSymbol::DocumentData as u64, doc_data(&["sec1"]));
        // No position_map: some valid KFX address purely by position_id_map.
        assert!(check_position_map_coverage(&book).is_empty());
    }
}
