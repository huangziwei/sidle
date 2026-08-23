//! KFX structural validation: one container read natively, never a derived
//! copy, reported as defects in the source book.
//!
//! Every rule reads the structures `kfx/container.rs` and `kfx/loader.rs`
//! ([`BookData`]) parse.
//!
//! - **Rule 1, container integrity** — `CONT` magic, info + index in bounds.
//!   A parse failure is one `container-unreadable` error; a non-zero
//!   `bcDRMScheme` is one `container-encrypted` error over ciphertext
//!   payloads. An out-of-bounds entity reaches rules 2/7.
//! - **Rule 2, required entities** — `document_data`, ≥1 `section`, ≥1
//!   `storyline` (errors); `book_navigation` (warning: no chapter list).
//! - **Rule 3, reading order resolves** — every reading-order section names a
//!   real `section` ($260), and every `story_name` ($176) reachable from a
//!   section names a real `storyline` ($259). A dangling ref is a missing
//!   chapter or a missing chapter body (errors).
//! - **Rule 4, content refs resolve** — every `$145 content` `{name,index}`
//!   indirection resolves to a real shared `$145 content` block (error:
//!   dropped text).
//! - **Rule 5, nav reachability** — every navigation entry targets an element
//!   some storyline contains; a dangling target tap-jumps to nowhere
//!   (warning). Delegates to `fidelity::nav`'s extraction, which exempts
//!   cover / section-root positions via `cover_target`.
//! - **Rule 6, style refs resolve** — every `style` ($157) an element cites
//!   names a real `style` entity (warning: unstyled render).
//! - **Rule 7, resource refs resolve** — every `external_resource` that names
//!   a `location` has its bytes embedded in the container.
//! - **Rule 8, position-map coverage** — every reading-order section appears
//!   in the `position_map` ($264), the device's "go to location" target
//!   (warning; runs only on a container holding a `position_map`, some KFX
//!   addressing purely by `position_id_map` $265).
//! - **Rule 9, cover present + resolves** — the declared cover resource exists
//!   and has embedded bytes (missing cover = warning; dangling = error).
//! - **Rule 11, declarations agree with content** — each `content_features`
//!   entry states what the book contains. See [`ContentFacts`] for the facts
//!   each claim is read against.
//!
//! Rule 10 (TOC deficiency) comes from the cross-format `source::toc` check
//! via [`crate::validate::source::validate`]. A `.kfx-zip` bundle sniffs as
//! EPUB by its `PK` magic.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::formats::kfx::container::get_field;
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::loader::{self, BookData};
use crate::formats::kfx::symbols::KfxSymbol;

use crate::validate::{Finding, FixHint, Severity};

/// Run the KFX structural rules over `bytes` and return the defects as
/// [`Finding`]s. A container that does not parse yields the single
/// `container-unreadable` error and ends the run.
/// [`crate::validate::source::validate`] calls this and adds the TOC audit
/// (rule 10).
pub fn validate(bytes: &[u8]) -> Vec<Finding> {
    let book = match loader::load(bytes) {
        Ok(book) => book,
        Err(crate::formats::kfx::error::KfxError::Encrypted(scheme)) => {
            return vec![error(
                "container-encrypted",
                "<container>",
                format!(
                    "container declares bcDRMScheme {scheme}: its payloads are encrypted under a \
                     device-bound key, so nothing inside can be checked"
                ),
            )];
        }
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
    findings.extend(check_feature_content_agreement(&book));
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

/// Reference-resolution defects, deduped by target name: a style cited by
/// 10 000 elements yields one finding. `BTreeSet` sorts the names, holding
/// finding order steady across runs.
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

/// Rules 3 & 6. Walk reading order → section → page_templates → referenced
/// storylines, flagging every named reference that resolves to no entity. The
/// walk starts at the reading order: a `section` entity outside it renders
/// nowhere.
fn check_references(book: &BookData) -> Vec<Finding> {
    let mut defects = RefDefects::default();
    // One visited set guards cycles in storyline and structure fragments,
    // namespaced against a shared name.
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

/// The block name of a `$145 content` `{name,index}` indirection in `value`
/// that resolves to no `$145 content` string. `None` for inline text (a plain
/// string) and for a value carrying no `name`.
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

/// The flat, in-order list of reading-order section names, from
/// `document_data` ($538) or the older flat `metadata` ($258).
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

/// Depth-first walk of a content value, recording dangling `style` (rule 6)
/// and `story_name` (rule 3) references. `story_name` and `$608 structure`
/// symbols are followed into their fragments, covering the rendered body. A
/// bare symbol resolving to no structure is left alone: most such symbols are
/// enum values.
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
            // Rule 6 — the element's `$157 style` as a named reference (a
            // symbol/string, per `text_of`). An inline style struct carries no
            // name and walks below as an ordinary child.
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

            // Rule 3 — `$176 story_name` resolves, and the storyline is
            // descended once for its own nested story references.
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

/// Look up a fragment by type and name.
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

    // Sorted names hold the finding order steady across HashMap iterations.
    let mut names: Vec<&String> = resources.keys().collect();
    names.sort();

    for name in names {
        let Some(fields) = resources[name].unwrap_annotated().as_struct() else {
            continue;
        };
        // A resource with no `location` ($165) names no bytes to resolve.
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

/// Rule 5. Every navigation entry (chapter list / headings) jumps to an
/// element some storyline contains; a dangling target tap-jumps to nowhere on
/// device, at warning severity. `fidelity::nav`'s extraction applies the
/// reachability rule, exempting cover / section-root positions.
///
/// `bytes` is raw: the nav extractor reads the container without
/// `loader::load`. Rule 1 reports a parse failure here as
/// `container-unreadable`.
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
/// contains, resolving a device "go to location N". A reading-order section
/// absent from it takes no location jump, at warning severity. The rule runs
/// only on a container holding a `position_map`; some KFX address purely by
/// `position_id_map` ($265).
fn check_position_map_coverage(book: &BookData) -> Vec<Finding> {
    let sections = reading_order_sections(book);
    if sections.is_empty() {
        return Vec::new(); // No reading order: rules 2/3 report it.
    }
    let Some(pmaps) = book.by_type.get(&(KfxSymbol::PositionMap as u64)) else {
        return Vec::new(); // No position_map: outside this rule.
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
        return Vec::new(); // An unreadable position_map covers no section.
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
// Rule 11 — declared features agree with the content
// ============================================================================

/// Positions a section holds below the large-section declaration.
const SECTION_PID_BOUND: i64 = 65536;

/// What the book contains, read from the container itself, for its
/// `content_features` claims to be checked against.
#[derive(Default, Debug)]
struct ContentFacts {
    /// A tiled image (`yj.tiles` / `yj.tile_padding` on an
    /// `external_resource`), the content `yj_hdv` covers. Amazon stamps
    /// `yj_hdv` on untiled books too.
    tiled_image: bool,
    /// An `external_resource` whose format is `jxr`.
    jxr_image: bool,
    /// A JPEG payload carrying restart markers (`FF D0`–`FF D7`), read from
    /// the `bcRawMedia` bytes.
    jpeg_restart_markers: bool,
    /// Positions in the longest section, from the `position_id_map` spans.
    /// `None` for a container shipping only the `{eid, pid}` pair shape, which
    /// states no section length.
    max_section_pids: Option<i64>,
}

impl ContentFacts {
    fn read(book: &BookData) -> Self {
        let mut facts = Self::default();

        if let Some(resources) = book.by_type.get(&(KfxSymbol::ExternalResource as u64)) {
            for value in resources.values() {
                let Some(fields) = value.unwrap_annotated().as_struct() else {
                    continue;
                };
                if get_field(fields, KfxSymbol::YjTiles as u64).is_some()
                    || get_field(fields, KfxSymbol::YjTilePadding as u64).is_some()
                {
                    facts.tiled_image = true;
                }
                if get_field(fields, KfxSymbol::Format as u64).and_then(|v| book.symbols.text_of(v))
                    == Some("jxr")
                {
                    facts.jxr_image = true;
                }
            }
        }

        facts.jpeg_restart_markers = book.raw_media.values().any(|bytes| {
            bytes.starts_with(&[0xFF, 0xD8, 0xFF])
                && bytes
                    .windows(2)
                    .any(|w| w[0] == 0xFF && (0xD0..=0xD7).contains(&w[1]))
        });

        facts.max_section_pids = max_section_pids(book);
        facts
    }

    /// The `reflow-section-size` for `max_section_pids`, `None` below
    /// `SECTION_PID_BOUND`.
    fn expected_section_size(&self) -> Option<i64> {
        let max = self.max_section_pids?;
        (max > SECTION_PID_BOUND).then(|| (((max - SECTION_PID_BOUND) / 16384) + 2).min(256))
    }
}

/// Positions in the longest section, read from the `position_id_map` ($265)
/// span shape (`{section_name, pid, length}`). The `{eid, pid}` pair shape
/// states no section length and yields `None`. Neither shape is a format
/// tell — see [`crate::formats::kfx::position`].
fn max_section_pids(book: &BookData) -> Option<i64> {
    let maps = book.by_type.get(&(KfxSymbol::PositionIdMap as u64))?;
    let mut max = None;
    for value in maps.values() {
        let Some(fields) = value.unwrap_annotated().as_struct() else {
            continue;
        };
        let Some(entries) = get_field(fields, KfxSymbol::Contains as u64)
            .and_then(|v| v.unwrap_annotated().as_list())
        else {
            continue;
        };
        for entry in entries {
            let Some(ef) = entry.unwrap_annotated().as_struct() else {
                continue;
            };
            // The span shape names a section; the pair shape carries eids.
            if get_field(ef, KfxSymbol::SectionName as u64).is_none() {
                continue;
            }
            if let Some(length) = get_field(ef, KfxSymbol::Length as u64).and_then(|v| v.as_int()) {
                max = Some(max.map_or(length, |m: i64| m.max(length)));
            }
        }
    }
    max
}

/// The `com.amazon.yjconversion` features the book declares, keyed by feature
/// name, valued by major version.
fn declared_features(book: &BookData) -> HashMap<String, i64> {
    let mut out = HashMap::new();
    let Some(entities) = book.by_type.get(&(KfxSymbol::ContentFeatures as u64)) else {
        return out;
    };
    for value in entities.values() {
        let Some(fields) = value.unwrap_annotated().as_struct() else {
            continue;
        };
        let Some(list) = get_field(fields, KfxSymbol::Features as u64)
            .and_then(|v| v.unwrap_annotated().as_list())
        else {
            continue;
        };
        for feature in list {
            let Some(ff) = feature.unwrap_annotated().as_struct() else {
                continue;
            };
            let Some(key) = get_field(ff, KfxSymbol::Key as u64).and_then(|v| v.as_string()) else {
                continue;
            };
            let major = get_field(ff, KfxSymbol::VersionInfo as u64)
                .and_then(|v| v.unwrap_annotated().as_struct())
                .and_then(|vi| get_field(vi, KfxSymbol::Version as u64))
                .and_then(|v| v.unwrap_annotated().as_struct())
                .and_then(|ver| get_field(ver, KfxSymbol::MajorVersion as u64))
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            out.insert(key.to_string(), major);
        }
    }
    out
}

fn check_feature_content_agreement(book: &BookData) -> Vec<Finding> {
    let declared = declared_features(book);
    if declared.is_empty() {
        return Vec::new(); // No content_features: no claim to check.
    }
    let facts = ContentFacts::read(book);
    let mut out = Vec::new();

    // Media features are checked in the omission direction. A declaration
    // describes the material a book was built from, which outruns the bytes
    // the container holds; undeclared content is the checkable half.
    if facts.tiled_image && !declared.contains_key("yj_hdv") {
        out.push(warning(
            "feature-undeclared",
            "<content_features>",
            "carries tiled imagery but declares no yj_hdv",
            None,
        ));
    }

    if facts.jxr_image && !declared.contains_key("yj_jpegxr_sd") {
        out.push(warning(
            "feature-undeclared",
            "<content_features>",
            "embeds JPEG-XR plates but declares no yj_jpegxr_sd",
            None,
        ));
    }

    if facts.jpeg_restart_markers && !declared.contains_key("yj_jpg_rst_marker_present") {
        out.push(warning(
            "feature-undeclared",
            "<content_features>",
            "a JPEG payload carries restart markers but the book declares no \
             yj_jpg_rst_marker_present, so segmented decoding stays off",
            None,
        ));
    }

    // Section size is checkable over a container stating section lengths.
    if facts.max_section_pids.is_some() {
        let expected = facts.expected_section_size();
        let actual = declared.get("reflow-section-size").copied();
        if expected != actual {
            let max = facts.max_section_pids.unwrap_or_default();
            out.push(warning(
                "feature-content-mismatch",
                "<content_features>",
                format!(
                    "reflow-section-size is {actual:?} but the longest section holds {max} \
                     positions, which calls for {expected:?} — deep paging and \"go to page\" \
                     read the declaration, not the section"
                ),
                Some(FixHint::new(
                    "restate-section-size",
                    "declare reflow-section-size for the longest section's position count",
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

    /// A container of a header and a container_info whose `bcDRMScheme` is
    /// `scheme`, over an empty index table. At `scheme: 0` it loads as an
    /// entity-less book.
    fn container_with_drm_scheme(scheme: i64) -> Vec<u8> {
        use crate::formats::kfx::ion::IonWriter;

        const HEADER_LEN: u32 = 18;
        let mut writer = IonWriter::new();
        writer.write_bvm();
        writer.write_value(&IonValue::Struct(vec![
            (KfxSymbol::Bcdrmscheme as u64, IonValue::Int(scheme)),
            (
                KfxSymbol::Bcindextaboffset as u64,
                IonValue::Int(HEADER_LEN as i64),
            ),
            (KfxSymbol::Bcindextablength as u64, IonValue::Int(0)),
        ]));
        let info = writer.into_bytes();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"CONT");
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&HEADER_LEN.to_le_bytes());
        bytes.extend_from_slice(&HEADER_LEN.to_le_bytes()); // container_info offset
        bytes.extend_from_slice(&(info.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&info);
        bytes
    }

    #[test]
    fn encrypted_container_is_reported_as_encrypted() {
        let out = validate(&container_with_drm_scheme(1));
        assert_eq!(out.len(), 1, "encryption ends the run: {out:?}");
        assert_eq!(out[0].rule, "container-encrypted");
        assert_eq!(out[0].severity, Severity::Error);
        // The structure is unreadable, never absent.
        assert!(!out.iter().any(|f| f.rule == "no-sections"));
    }

    #[test]
    fn unencrypted_container_is_read_not_refused() {
        let out = validate(&container_with_drm_scheme(0));
        assert!(!out.iter().any(|f| f.rule == "container-encrypted"));
        assert!(out.iter().any(|f| f.rule == "no-sections"));
    }

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
        // document_data / section / storyline are errors; nav is a warning.
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
            // Location-less descriptor: skipped, never flagged.
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
    // References are `IonValue::String`s, which `text_of` resolves over
    // `empty_book_for_test`'s empty doc-symbol table.

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

    /// A minimal entity, named for a reference check to resolve.
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
        // sec1 points at an absent storyline; secMissing has no section
        // entity.
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

    /// A `position_map` ($264) covering `covered`, one `{section_name,
    /// contains}` struct each.
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
        // No position_map: some KFX address purely by position_id_map.
        assert!(check_position_map_coverage(&book).is_empty());
    }
}
