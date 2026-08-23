//! KFX structural validation: one container read natively, never a derived
//! copy, reported as defects in the source book.
//!
//! Every rule reads the structures `kfx/container.rs` and `kfx/loader.rs`
//! ([`BookData`]) parse.
//!
//! - **Rule 1, container integrity** — `CONT` magic, info + index in bounds.
//!   A parse failure is one `container-unreadable` error; a non-zero
//!   `bcDRMScheme` or `bcComprType` is one `container-encrypted` /
//!   `container-compressed` error over unreadable payloads. The header
//!   `version`, the index table's entry alignment, the `bcContId` shape and
//!   the symbol table's declared `max_id` are warnings. Each indexed entity is
//!   read against the bytes it addresses: a range past the end of the file, a
//!   payload outside the media types that is not Ion, and a name a second
//!   fragment of the same type repeats over different bytes are errors, one
//!   per lost fragment.
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
//! - **Rule 12, metadata** — `title` and `language` are stated, and
//!   `author_pronunciation` stays positional with `author`.
//! - **Rule 13, element arithmetic** — an element id names one element
//!   book-wide; `style_events` and `word_boundary_list` ranges stay inside the
//!   base text they count characters of; a length states a CSS unit and a
//!   numeric magnitude; an `important_cells` coordinate lands inside its
//!   table's grid. Gathered by rule 3's walk and a sweep of the `style`
//!   entities.
//! - **Rule 14, position arithmetic** — the `position_id_map` span shape
//!   partitions the pid axis from 0 with no gap or overlap, and
//!   `yj.location_pid_map` boundaries never go backwards.
//!
//! Rule 10 (TOC deficiency) comes from the cross-format `source::toc` check
//! via [`crate::validate::source::validate`]. A `.kfx-zip` bundle sniffs as
//! EPUB by its `PK` magic.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::formats::kfx::container::{ContainerHeader, ContainerInfo, get_field};
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::loader::{self, BookData};
use crate::formats::kfx::symbols::KfxSymbol;

use crate::validate::{Finding, FixHint, Severity};

/// Run the KFX structural rules over `bytes` and return the defects as
/// [`Finding`]s. A container that does not parse yields the single
/// `container-unreadable` error and ends the run.
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
        Err(crate::formats::kfx::error::KfxError::Compressed(kind)) => {
            return vec![error(
                "container-compressed",
                "<container>",
                format!(
                    "container declares bcComprType {kind}: its payloads are compressed, so \
                     nothing inside can be checked"
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
    findings.extend(check_container_scalars(bytes));
    findings.extend(check_container_inventory(bytes));
    findings.extend(check_entity_index(bytes));
    findings.extend(check_required_entities(&book.by_type));
    findings.extend(check_references(&book));
    findings.extend(check_resource_bytes(&book));
    findings.extend(check_cover(&book));
    findings.extend(check_metadata(&book, container_id(bytes).as_deref()));
    findings.extend(check_nav_reachability(bytes));
    findings.extend(check_nav_vocabulary(&book));
    findings.extend(check_position_map_coverage(&book));
    findings.extend(check_position_arithmetic(&book));
    findings.extend(check_feature_content_agreement(&book));
    findings
}

// ============================================================================
// Rule 1 — container-layer scalars
// ============================================================================

/// The container-layer version every KFX declares.
const CONTAINER_VERSION: u16 = 2;

/// Bytes per entity index table entry: id(4) + type(4) + offset(8) + len(8).
const INDEX_ENTRY_SIZE: usize = 24;

/// Rule 1's scalar half at warning severity: the header's `version`, the
/// index table's entry alignment, and the `bcContId` shape. [`validate`]
/// reports `bcDRMScheme` and `bcComprType` from the load error.
fn check_container_scalars(bytes: &[u8]) -> Vec<Finding> {
    use crate::formats::kfx::container::parse_container_header;

    let Ok(header) = parse_container_header(bytes) else {
        return Vec::new();
    };
    let mut out = Vec::new();

    if header.version != CONTAINER_VERSION {
        out.push(warning(
            "container-version-unexpected",
            "<container>",
            format!(
                "container header declares version {} — KFX declares {CONTAINER_VERSION}",
                header.version
            ),
            None,
        ));
    }

    let Some(info) = container_info(bytes, &header) else {
        return out;
    };

    if let Some((_, index_length)) = info.index
        && !index_length.is_multiple_of(INDEX_ENTRY_SIZE)
    {
        let trailing = index_length % INDEX_ENTRY_SIZE;
        out.push(warning(
            "index-table-ragged",
            "<container>",
            format!(
                "entity index table is {index_length} bytes, {trailing} past a whole \
                 {INDEX_ENTRY_SIZE}-byte entry — the trailing bytes name no entity"
            ),
            None,
        ));
    }

    match info.cont_id.as_deref() {
        None => out.push(warning(
            "container-id-missing",
            "<container>",
            "container info declares no bcContId ($409)",
            None,
        )),
        Some(id) if !is_container_id(id) => out.push(warning(
            "container-id-malformed",
            "<container>",
            format!("bcContId {id:?} is not \"CR!\" followed by 28 uppercase alphanumerics"),
            None,
        )),
        Some(_) => {}
    }

    out
}

/// The container info a header locates, `None` when its range runs past the
/// end of `bytes` or the info does not parse. Such a container is reported
/// once as `container-unreadable` from the load error.
fn container_info(bytes: &[u8], header: &ContainerHeader) -> Option<ContainerInfo> {
    use crate::formats::kfx::container::{parse_container_info, slice_at};

    let info_bytes = slice_at(
        bytes,
        header.container_info_offset,
        header.container_info_length,
    )?;
    parse_container_info(info_bytes).ok()
}

/// The container's `bcContId` ($409).
fn container_id(bytes: &[u8]) -> Option<String> {
    use crate::formats::kfx::container::parse_container_header;

    let header = parse_container_header(bytes).ok()?;
    container_info(bytes, &header)?.cont_id
}

/// The shared symbol table every container imports.
const SHARED_SYMBOL_TABLE: &str = "YJ_symbols";

/// Index-table fragment types a book holds at most one of. `content_features`
/// is absent: a book states one standalone and one inside its metadata. `$270`
/// and `$593` are absent too — the container info locates both by offset.
const SINGLETON_TYPES: [KfxSymbol; 10] = [
    KfxSymbol::Metadata,
    KfxSymbol::PositionMap,
    KfxSymbol::PositionIdMap,
    KfxSymbol::BookNavigation,
    KfxSymbol::ResourcePath,
    KfxSymbol::ContainerEntityMap,
    KfxSymbol::BookMetadata,
    KfxSymbol::DocumentData,
    KfxSymbol::LocationMap,
    KfxSymbol::YjLocationPidMap,
];

/// The index table and the doc-symbols fragment: the container imports
/// [`SHARED_SYMBOL_TABLE`], its declared `max_id` accounts for exactly the
/// local symbols it lists, and no [`SINGLETON_TYPES`] entry appears twice.
fn check_container_inventory(bytes: &[u8]) -> Vec<Finding> {
    use crate::formats::kfx::container::{
        extract_doc_symbols, parse_container_header, parse_import_names, parse_imports_max_id,
        parse_index_table, parse_local_max_id, slice_at,
    };

    let Ok(header) = parse_container_header(bytes) else {
        return Vec::new();
    };
    let Some(info) = container_info(bytes, &header) else {
        return Vec::new();
    };
    let mut out = Vec::new();

    if let Some(table) = info
        .doc_symbols
        .and_then(|(off, len)| slice_at(bytes, off, len))
    {
        let names = parse_import_names(table);
        if !names.is_empty() && !names.iter().any(|n| n == SHARED_SYMBOL_TABLE) {
            out.push(warning(
                "symbol-import-unexpected",
                "<symbols>",
                format!(
                    "the symbol table imports {names:?}, not {SHARED_SYMBOL_TABLE:?} — every \
                     shared id resolves against a table the container never named"
                ),
                None,
            ));
        }

        // §5.4: the table's own `max_id` is the summed import `max_id` plus
        // the local symbols it lists.
        if let (Some(declared), Some(imported)) =
            (parse_local_max_id(table), parse_imports_max_id(table))
        {
            let expected = imported + extract_doc_symbols(table).len() as u64;
            if declared != expected {
                out.push(warning(
                    "symbol-table-max-id-mismatch",
                    "<symbols>",
                    format!(
                        "the symbol table declares max_id {declared} but imports {imported} ids \
                         and lists {} local symbols, ending at {expected} — a reader seating \
                         local ids by the declaration resolves them off by {}",
                        expected - imported,
                        declared.abs_diff(expected)
                    ),
                    None,
                ));
            }
        }
    }

    if let Some(index) = info.index.and_then(|(off, len)| slice_at(bytes, off, len)) {
        let entities = parse_index_table(index, header.header_len);
        for singleton in SINGLETON_TYPES {
            let count = entities
                .iter()
                .filter(|e| e.type_id as u64 == singleton as u64)
                .count();
            if count > 1 {
                let name = format!("{singleton:?}");
                out.push(warning(
                    "singleton-repeated",
                    &name,
                    format!(
                        "{count} {} (${}) fragments — a book holds at most one, and the later \
                         one hides the rest",
                        name.to_lowercase(),
                        singleton as u64
                    ),
                    None,
                ));
            }
        }
    }

    out
}

/// Rule 1's per-entity half: each index entry against the bytes it addresses.
/// The range lies inside the file, the payload parses as Ion outside the media
/// types, and no two entities of one type carry both the same name and
/// different bytes. `loader::load` drops each of these silently, leaving every
/// later rule a fragment short.
fn check_entity_index(bytes: &[u8]) -> Vec<Finding> {
    use crate::formats::kfx::container::{
        SymbolTable, entity_media, parse_container_header, parse_index_table, slice_at,
    };
    use crate::formats::kfx::ion::IonParser;
    use crate::formats::kfx::resource_index::entity_fid;

    let Ok(header) = parse_container_header(bytes) else {
        return Vec::new();
    };
    let Some(info) = container_info(bytes, &header) else {
        return Vec::new();
    };
    let Some(index) = info.index.and_then(|(off, len)| slice_at(bytes, off, len)) else {
        return Vec::new();
    };
    let symbols = SymbolTable::from_fragment(
        info.doc_symbols
            .and_then(|(off, len)| slice_at(bytes, off, len)),
    );

    let mut out = Vec::new();
    let mut seen: HashMap<(u32, String), &[u8]> = HashMap::new();
    for ent in parse_index_table(index, header.header_len) {
        let name = entity_fid(ent.id as u64, &symbols);
        let ftype = symbols.resolve(ent.type_id as u64);
        let location = format!("{ftype}/{name}");

        let Some(payload) = entity_media(bytes, &ent) else {
            out.push(error(
                "entity-out-of-bounds",
                &location,
                format!(
                    "the index places this fragment at byte {} for {} bytes, past the end of a \
                     {}-byte container — nothing it holds reaches the book",
                    ent.offset,
                    ent.length,
                    bytes.len()
                ),
            ));
            continue;
        };

        // §11.2: a media payload is the media file verbatim; every other
        // payload is binary Ion.
        let media = ent.type_id == KfxSymbol::Bcrawmedia as u32
            || ent.type_id == KfxSymbol::Bcrawfont as u32;
        if !media && IonParser::new(payload).parse().is_err() {
            out.push(error(
                "entity-payload-unparsable",
                &location,
                format!(
                    "the {} payload bytes are not binary Ion — nothing this fragment holds \
                     reaches the book",
                    payload.len()
                ),
            ));
            continue;
        }

        // §6.1: `(type, name)` is a fragment's key, so a reader keeps one of
        // any two that share it. A repeat carrying identical bytes costs
        // nothing and is not reported. Singletons all carry the reserved id
        // `$348` and are counted by `singleton-repeated` instead.
        if ent.id as u64 != KfxSymbol::Null as u64 {
            let first = *seen.entry((ent.type_id, name.clone())).or_insert(payload);
            if first != payload {
                out.push(error(
                    "fragment-name-collision",
                    &location,
                    format!(
                        "a second {ftype} fragment carries the name {name:?} with different \
                         bytes — a reader keying by (type, name) keeps one of the two and loses \
                         the other"
                    ),
                ));
            }
        }
    }

    out
}

/// True for `CR!` followed by exactly 28 uppercase alphanumerics.
fn is_container_id(id: &str) -> bool {
    let Some(suffix) = id.strip_prefix("CR!") else {
        return false;
    };
    suffix.len() == 28
        && suffix
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
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

/// Defects gathered by the one walk over the rendered content graph, deduped
/// by the name or value at fault: a style cited by 10 000 elements yields one
/// finding. `BTreeSet` sorts them, holding finding order steady across runs.
#[derive(Default)]
struct WalkDefects {
    /// Reading-order entries that don't resolve to a `section` ($260).
    missing_sections: BTreeSet<String>,
    /// `story_name` ($176) refs that don't resolve to a `storyline` ($259).
    missing_stories: BTreeSet<String>,
    /// `style` ($157) refs that don't resolve to a `style` entity.
    missing_styles: BTreeSet<String>,
    /// `content` ($145) `{name,index}` indirections that don't resolve to a
    /// `$145 content` string (rule 4).
    missing_content: BTreeSet<String>,
    /// `type` ($159) values the import schema holds no strategy for.
    unknown_types: BTreeSet<String>,
    /// `writing_mode` ($560) values outside [`WRITING_MODES`].
    unknown_writing_modes: BTreeSet<String>,
    /// `listitem` ($277) elements whose parent is no `list` ($276).
    orphan_list_items: usize,
    /// Reading-order sections carrying no `page_templates` ($141) entry.
    empty_sections: BTreeSet<String>,
    /// Elements holding both `content` ($145) and `content_list` ($146).
    content_and_children: usize,
    /// `table` ($279) elements holding no `table_row` ($279) descendant.
    rowless_tables: usize,
    /// Every element id ($155) the walk has met.
    seen_eids: HashSet<i64>,
    /// Element ids ($155) carried by more than one element.
    duplicate_eids: BTreeSet<i64>,
    /// Elements whose `style_events` ($142) reach past their base text.
    style_events_past_text: usize,
    /// Elements whose `word_boundary_list` ($696) reaches past their base
    /// text, or whose entries don't pair up.
    word_boundaries_past_text: usize,
    /// `unit` ($306) values outside the CSS length vocabulary.
    unknown_units: BTreeSet<String>,
    /// Length structs whose `value` ($307) is no number.
    non_numeric_lengths: usize,
    /// `important_cells` ($700) coordinates outside their table's grid,
    /// rendered `[row, column] in RxC`.
    out_of_range_cells: BTreeSet<String>,
}

/// Rules 3, 6 & 13. Walk reading order → section → page_templates →
/// referenced storylines for unresolved references and element-level
/// arithmetic defects, then sweep the `style` entities for their lengths.
fn check_references(book: &BookData) -> Vec<Finding> {
    let mut defects = WalkDefects::default();
    // One visited set guards cycles in storyline and structure fragments,
    // namespaced against a shared name.
    let mut visited: HashSet<String> = HashSet::new();

    for section_name in reading_order_sections(book) {
        match lookup(book, KfxSymbol::Section, &section_name) {
            Some(section) => {
                if !has_page_template(section) {
                    defects.empty_sections.insert(section_name.clone());
                }
                walk_refs(section, book, None, &mut visited, &mut defects);
            }
            None => {
                defects.missing_sections.insert(section_name);
            }
        }
    }

    if let Some(styles) = book.by_type.get(&(KfxSymbol::Style as u64)) {
        for style in styles.values() {
            scan_lengths(style, book, &mut defects);
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
    for name in &defects.unknown_types {
        findings.push(info(
            "element-type-unknown",
            name,
            format!("an element declares type {name:?}, which bokai lays out as a container"),
        ));
    }
    for name in &defects.unknown_writing_modes {
        findings.push(warning(
            "writing-mode-unknown",
            name,
            format!("a writing_mode of {name:?} is outside {WRITING_MODES:?}"),
            None,
        ));
    }
    if defects.orphan_list_items > 0 {
        findings.push(warning(
            "list-item-outside-list",
            "<storyline>",
            format!(
                "{} listitem elements sit under no list element — each renders as a bare block",
                defects.orphan_list_items
            ),
            None,
        ));
    }
    for name in &defects.empty_sections {
        findings.push(warning(
            "section-empty",
            name,
            format!("section {name:?} lists no page_templates ($141) — it renders nothing"),
            None,
        ));
    }
    if defects.rowless_tables > 0 {
        findings.push(warning(
            "table-without-rows",
            "<storyline>",
            format!(
                "{} table elements hold no table_row — each renders as an empty grid",
                defects.rowless_tables
            ),
            None,
        ));
    }
    if defects.content_and_children > 0 {
        findings.push(info(
            "element-content-and-children",
            "<storyline>",
            format!(
                "{} elements carry both content ($145) and content_list ($146)",
                defects.content_and_children
            ),
        ));
    }
    for eid in &defects.duplicate_eids {
        findings.push(error(
            "element-id-duplicate",
            &eid.to_string(),
            format!(
                "element id {eid} is carried by more than one element — every position, anchor \
                 and highlight addressing it reaches whichever the reader finds first"
            ),
        ));
    }
    if defects.style_events_past_text > 0 {
        findings.push(warning(
            "style-event-past-text",
            "<storyline>",
            format!(
                "{} elements carry a style_event ($142) range reaching past their base text — \
                 the styling it names has no characters to cover",
                defects.style_events_past_text
            ),
            None,
        ));
    }
    if defects.word_boundaries_past_text > 0 {
        findings.push(warning(
            "word-boundaries-past-text",
            "<storyline>",
            format!(
                "{} elements carry a word_boundary_list ($696) that doesn't fit their base text \
                 — word selection and hyphenation run off the end",
                defects.word_boundaries_past_text
            ),
            None,
        ));
    }
    for unit in &defects.unknown_units {
        findings.push(warning(
            "length-unit-unknown",
            unit,
            format!("a length declares unit {unit:?}, which is no CSS length unit"),
            None,
        ));
    }
    if defects.non_numeric_lengths > 0 {
        findings.push(warning(
            "length-value-not-numeric",
            "<storyline>",
            format!(
                "{} length structs carry a non-numeric value ($307) — the dimension they state \
                 cannot be read",
                defects.non_numeric_lengths
            ),
            None,
        ));
    }
    for cell in &defects.out_of_range_cells {
        findings.push(warning(
            "important-cell-out-of-range",
            cell,
            format!("important_cells ($700) names cell {cell}, which the table has no room for"),
            None,
        ));
    }
    findings
}

/// The `writing_mode` ($560) values, matching CSS.
const WRITING_MODES: [&str; 3] = ["horizontal_tb", "vertical_rl", "vertical_lr"];

/// True when `section` lists at least one `page_templates` ($141) entry, the
/// only content a section contributes.
fn has_page_template(section: &IonValue) -> bool {
    section
        .unwrap_annotated()
        .as_struct()
        .and_then(|fields| get_field(fields, KfxSymbol::PageTemplates as u64))
        .and_then(|v| v.unwrap_annotated().as_list())
        .is_some_and(|list| !list.is_empty())
}

/// True when `value` holds a `table_row` ($279) at any depth.
fn holds_table_row(value: &IonValue) -> bool {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            if get_field(fields, KfxSymbol::Type as u64).and_then(|v| v.as_symbol())
                == Some(KfxSymbol::TableRow as u64)
            {
                return true;
            }
            fields.iter().any(|(_, v)| holds_table_row(v))
        }
        IonValue::List(items) => items.iter().any(holds_table_row),
        _ => false,
    }
}

/// An element's base text (§7.5), whose characters `style_events` and
/// `word_boundary_list` offsets count: a literal `$145 content` string, or a
/// `{name, index}` indirection into the `$145 content` fragment it names.
fn base_text<'b>(content: &'b IonValue, book: &'b BookData) -> Option<&'b str> {
    let inner = content.unwrap_annotated();
    if let Some(text) = inner.as_string() {
        return Some(text);
    }
    let fields = inner.as_struct()?;
    let name = get_field(fields, KfxSymbol::Name as u64).and_then(|v| book.symbols.text_of(v))?;
    let index = get_field(fields, KfxSymbol::Index as u64)
        .and_then(|v| v.as_int())
        .unwrap_or(0) as usize;
    lookup(book, KfxSymbol::Content, name)
        .and_then(|entry| entry.unwrap_annotated().as_struct())
        .and_then(|fs| get_field(fs, KfxSymbol::ContentList as u64))
        .and_then(|v| v.as_list())
        .and_then(|list| list.get(index))
        .and_then(|item| item.as_string())
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
    base_text(value, book).is_none().then(|| name.to_string())
}

/// A `table` ($278) element's grid: `table_row` ($279) descendants, and the
/// widest row's cell count. A row's children *are* its cells (§7.6); a
/// nested table's rows stop at the row that holds it.
fn table_grid(table: &IonValue) -> (usize, usize) {
    let mut rows = Vec::new();
    collect_rows(table, &mut rows);
    let widest = rows.iter().copied().max().unwrap_or(0);
    (rows.len(), widest)
}

/// Push each `table_row` ($279) descendant's cell count into `out`.
fn collect_rows(value: &IonValue, out: &mut Vec<usize>) {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            if get_field(fields, KfxSymbol::Type as u64).and_then(|v| v.as_symbol())
                == Some(KfxSymbol::TableRow as u64)
            {
                out.push(
                    get_field(fields, KfxSymbol::ContentList as u64)
                        .and_then(|v| v.unwrap_annotated().as_list())
                        .map_or(0, <[IonValue]>::len),
                );
                return;
            }
            for (_, v) in fields {
                collect_rows(v, out);
            }
        }
        IonValue::List(items) => {
            for item in items {
                collect_rows(item, out);
            }
        }
        _ => {}
    }
}

/// The unit a finding names for a `unit` ($306) that is no symbol or string.
const NAMELESS_UNIT: &str = "(not a name)";

/// One `{value, unit}` length (§8.2): the unit is a CSS length unit and the
/// magnitude is a number. A struct carrying no `unit` ($306) is no length.
fn check_length(fields: &[(u64, IonValue)], book: &BookData, out: &mut WalkDefects) {
    use crate::formats::kfx::yj_properties::length_unit_for;

    let Some(unit) = get_field(fields, KfxSymbol::Unit as u64) else {
        return;
    };
    match book.symbols.text_of(unit) {
        Some(name) if length_unit_for(name).is_some() => {}
        Some(name) => {
            out.unknown_units.insert(name.to_string());
        }
        // A unit that is neither symbol nor string names no unit at all.
        None => {
            out.unknown_units.insert(NAMELESS_UNIT.to_string());
        }
    }

    if let Some(magnitude) = get_field(fields, KfxSymbol::Value as u64)
        && !matches!(
            magnitude.unwrap_annotated(),
            IonValue::Int(_) | IonValue::Float(_) | IonValue::Decimal(_)
        )
    {
        out.non_numeric_lengths += 1;
    }
}

/// True when any `style_events` ($142) entry reaches past `chars` characters
/// of base text (§8.4). Events may overlap; each is measured on its own.
fn style_events_overrun(fields: &[(u64, IonValue)], chars: usize) -> bool {
    let Some(events) = get_field(fields, KfxSymbol::StyleEvents as u64)
        .and_then(|v| v.unwrap_annotated().as_list())
    else {
        return false;
    };
    events.iter().any(|event| {
        let Some(ef) = event.unwrap_annotated().as_struct() else {
            return false;
        };
        let offset = get_field(ef, KfxSymbol::Offset as u64)
            .and_then(|v| v.as_int())
            .unwrap_or(0);
        let length = get_field(ef, KfxSymbol::Length as u64)
            .and_then(|v| v.as_int())
            .unwrap_or(0);
        offset < 0 || length < 0 || offset.saturating_add(length) as usize > chars
    })
}

/// True when the `word_boundary_list` ($696) doesn't fit `chars` characters of
/// base text: a flat run of `(gap, length)` pairs walking the text from its
/// start, where an odd entry count leaves a pair unclosed.
fn word_boundaries_overrun(fields: &[(u64, IonValue)], chars: usize) -> bool {
    let Some(entries) = get_field(fields, KfxSymbol::WordBoundaryList as u64)
        .and_then(|v| v.unwrap_annotated().as_list())
    else {
        return false;
    };
    if !entries.len().is_multiple_of(2) {
        return true;
    }
    let mut walked: i64 = 0;
    for entry in entries {
        let Some(step) = entry.as_int() else {
            return true;
        };
        if step < 0 {
            return true;
        }
        walked = walked.saturating_add(step);
    }
    walked as usize > chars
}

/// Every `important_cells` ($700) `[row, column]` coordinate lands inside the
/// grid `table` spans (§7.6).
fn check_important_cells(fields: &[(u64, IonValue)], table: &IonValue, out: &mut WalkDefects) {
    let Some(cells) = get_field(fields, KfxSymbol::ImportantCells as u64)
        .and_then(|v| v.unwrap_annotated().as_list())
    else {
        return;
    };
    let (rows, columns) = table_grid(table);
    for cell in cells {
        let Some(pair) = cell.unwrap_annotated().as_list() else {
            continue;
        };
        let (Some(row), Some(column)) = (
            pair.first().and_then(IonValue::as_int),
            pair.get(1).and_then(IonValue::as_int),
        ) else {
            continue;
        };
        if row < 0 || column < 0 || row as usize >= rows || column as usize >= columns {
            out.out_of_range_cells
                .insert(format!("[{row}, {column}] in {rows}x{columns}"));
        }
    }
}

/// [`check_length`] over every struct in `value`.
fn scan_lengths(value: &IonValue, book: &BookData, out: &mut WalkDefects) {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            check_length(fields, book, out);
            for (_, v) in fields {
                scan_lengths(v, book, out);
            }
        }
        IonValue::List(items) => {
            for item in items {
                scan_lengths(item, book, out);
            }
        }
        _ => {}
    }
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

/// Depth-first walk of a content value into [`WalkDefects`], following
/// `story_name` and `$608 structure` symbols into their fragments.
/// `parent_type` is the enclosing element's `type` ($159) symbol id.
fn walk_refs(
    value: &IonValue,
    book: &BookData,
    parent_type: Option<u64>,
    visited: &mut HashSet<String>,
    out: &mut WalkDefects,
) {
    match value.unwrap_annotated() {
        IonValue::Symbol(id) => {
            let name = book.symbols.resolve(*id).to_string();
            if let Some(frag) = lookup(book, KfxSymbol::Structure, &name)
                && visited.insert(format!("struct:{name}"))
            {
                walk_refs(frag, book, parent_type, visited, out);
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
                            walk_refs(storyline, book, None, visited, out);
                        }
                    }
                    None => {
                        out.missing_stories.insert(story_name.to_string());
                    }
                }
            }

            let element_type =
                get_field(fields, KfxSymbol::Type as u64).and_then(|v| v.as_symbol());
            if let Some(type_id) = element_type {
                if crate::formats::kfx::schema::schema()
                    .element_strategy(type_id as u32)
                    .is_none()
                {
                    out.unknown_types
                        .insert(book.symbols.resolve(type_id).to_string());
                }
                if type_id == KfxSymbol::Listitem as u64
                    && parent_type != Some(KfxSymbol::List as u64)
                {
                    out.orphan_list_items += 1;
                }
                if type_id == KfxSymbol::Table as u64 {
                    if !holds_table_row(value) {
                        out.rowless_tables += 1;
                    }
                    check_important_cells(fields, value, out);
                }
            }

            // §7.4 — one element per id, book-wide.
            if let Some(eid) = get_field(fields, KfxSymbol::Id as u64).and_then(|v| v.as_int())
                && !out.seen_eids.insert(eid)
            {
                out.duplicate_eids.insert(eid);
            }

            // §8.2 — the element's own inline lengths.
            check_length(fields, book, out);

            // §8.4 / §7.4 — ranges over the base text. An element with no
            // `content` draws its text from interleaved children.
            if let Some(text) = get_field(fields, KfxSymbol::Content as u64)
                .and_then(|content| base_text(content, book))
            {
                let chars = text.chars().count();
                if style_events_overrun(fields, chars) {
                    out.style_events_past_text += 1;
                }
                if word_boundaries_overrun(fields, chars) {
                    out.word_boundaries_past_text += 1;
                }
            }

            if get_field(fields, KfxSymbol::Content as u64).is_some()
                && get_field(fields, KfxSymbol::ContentList as u64).is_some()
            {
                out.content_and_children += 1;
            }

            if let Some(mode) = get_field(fields, KfxSymbol::WritingMode as u64)
                .and_then(|v| book.symbols.text_of(v))
                && !WRITING_MODES.contains(&mode)
            {
                out.unknown_writing_modes.insert(mode.to_string());
            }

            // A struct with no `type` inherits `parent_type`, carrying it
            // through a `content_list` wrapper.
            let child_parent = element_type.or(parent_type);
            for (_, v) in fields {
                walk_refs(v, book, child_parent, visited, out);
            }
        }
        IonValue::List(items) => {
            for item in items {
                walk_refs(item, book, parent_type, visited, out);
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
// Rule 7 — external-resource bytes are embedded, format is a known one
// ============================================================================

fn check_resource_bytes(book: &BookData) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(resources) = book.by_type.get(&(KfxSymbol::ExternalResource as u64)) else {
        return out;
    };

    // Sorted names hold the finding order steady across HashMap iterations.
    let mut names: Vec<&String> = resources.keys().collect();
    names.sort();

    for name in names {
        let Some(fields) = resources[name].unwrap_annotated().as_struct() else {
            continue;
        };

        if let Some(format) =
            get_field(fields, KfxSymbol::Format as u64).and_then(|v| book.symbols.text_of(v))
            && !crate::formats::kfx::resource_index::DECLARED_FORMATS.contains(&format)
        {
            out.push(info(
                "resource-format-unknown",
                name,
                format!(
                    "external_resource {name:?} declares format {format:?}, outside the set a \
                     Kindle decodes"
                ),
            ));
        }

        // A resource with no `location` ($165) names no bytes to resolve.
        let Some(location) =
            get_field(fields, KfxSymbol::Location as u64).and_then(|v| v.as_string())
        else {
            continue;
        };
        if !book.raw_media.contains_key(location) {
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
// Rule 12 — book metadata is stated and self-consistent
// ============================================================================

/// Rule 12. `title` and `language` are stated, `author_pronunciation` stays
/// positional with `author`, and `asset_id` restates `bcContId`.
fn check_metadata(book: &BookData, cont_id: Option<&str>) -> Vec<Finding> {
    let meta = &book.metadata;
    let mut out = Vec::new();

    if let Some(cont_id) = cont_id
        && let Some(asset_id) = title_metadata_value(book, "asset_id")
        && asset_id != cont_id
    {
        out.push(warning(
            "metadata-asset-id-mismatch",
            "<metadata>",
            format!("asset_id {asset_id:?} does not match the container's bcContId {cont_id:?}"),
            None,
        ));
    }

    if meta.title.trim().is_empty() {
        out.push(warning(
            "metadata-no-title",
            "<metadata>",
            "no title — a library lists the book by its filename",
            None,
        ));
    }

    match meta.language.as_str() {
        "" => out.push(warning(
            "metadata-no-language",
            "<metadata>",
            "no language — hyphenation and font selection have nothing to key on",
            None,
        )),
        language if !is_language_tag(language) => out.push(warning(
            "metadata-language-malformed",
            "<metadata>",
            format!("language {language:?} is not a BCP-47 tag"),
            None,
        )),
        _ => {}
    }

    if meta.author_pronunciations.len() > meta.authors.len() {
        out.push(warning(
            "metadata-pronunciation-surplus",
            "<metadata>",
            format!(
                "{} author_pronunciation values for {} author values — the surplus names nobody",
                meta.author_pronunciations.len(),
                meta.authors.len()
            ),
            None,
        ));
    }

    out
}

/// One `kindle_title_metadata` key's value, from the categorised
/// `book_metadata` ($490) wrapper.
fn title_metadata_value<'b>(book: &'b BookData, key: &str) -> Option<&'b str> {
    let entities = book.by_type.get(&(KfxSymbol::BookMetadata as u64))?;
    for value in entities.values() {
        let Some(fields) = value.unwrap_annotated().as_struct() else {
            continue;
        };
        let Some(categories) = get_field(fields, KfxSymbol::CategorisedMetadata as u64)
            .and_then(|v| v.unwrap_annotated().as_list())
        else {
            continue;
        };
        for category in categories {
            let Some(cf) = category.unwrap_annotated().as_struct() else {
                continue;
            };
            let Some(entries) = get_field(cf, KfxSymbol::Metadata as u64)
                .and_then(|v| v.unwrap_annotated().as_list())
            else {
                continue;
            };
            for entry in entries {
                let Some(ef) = entry.unwrap_annotated().as_struct() else {
                    continue;
                };
                if get_field(ef, KfxSymbol::Key as u64).and_then(|v| v.as_string()) == Some(key) {
                    return get_field(ef, KfxSymbol::Value as u64).and_then(|v| v.as_string());
                }
            }
        }
    }
    None
}

/// True for a BCP-47 tag: a 2–3 letter primary subtag, then `-` subtags of
/// alphanumerics.
fn is_language_tag(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let Some(primary) = parts.next() else {
        return false;
    };
    if !(2..=3).contains(&primary.len()) || !primary.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    parts.all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

// ============================================================================
// Rule 5 — nav reachability and vocabulary
// ============================================================================

/// The `nav_type` ($235) values a `nav_container` states.
const NAV_TYPES: [&str; 4] = ["toc", "landmarks", "page_list", "headings"];

/// Every `nav_type` a `book_navigation` ($389) or `nav_container` ($391)
/// states outside [`NAV_TYPES`]. An unrecognised `nav_type` contributes
/// nothing to the chapter list.
fn check_nav_vocabulary(book: &BookData) -> Vec<Finding> {
    let mut unknown: BTreeSet<String> = BTreeSet::new();
    for ftype in [KfxSymbol::BookNavigation, KfxSymbol::NavContainer] {
        let Some(entities) = book.by_type.get(&(ftype as u64)) else {
            continue;
        };
        for value in entities.values() {
            collect_nav_types(value, book, &mut unknown);
        }
    }
    unknown
        .into_iter()
        .map(|name| {
            info(
                "nav-type-unknown",
                &name,
                format!("a nav_container states nav_type {name:?}, outside {NAV_TYPES:?}"),
            )
        })
        .collect()
}

/// Record every `nav_type` ($235) in `value` that is outside [`NAV_TYPES`].
fn collect_nav_types(value: &IonValue, book: &BookData, out: &mut BTreeSet<String>) {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            if let Some(nav_type) =
                get_field(fields, KfxSymbol::NavType as u64).and_then(|v| book.symbols.text_of(v))
                && !NAV_TYPES.contains(&nav_type)
            {
                out.insert(nav_type.to_string());
            }
            for (_, v) in fields {
                collect_nav_types(v, book, out);
            }
        }
        IonValue::List(items) => {
            for item in items {
                collect_nav_types(item, book, out);
            }
        }
        _ => {}
    }
}

/// Rule 5. Every navigation entry (chapter list / headings) jumps to an
/// element some storyline contains; a dangling target tap-jumps to nowhere
/// (warning). `fidelity::nav` reads `bytes` without `loader::load`.
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

/// Rule 8. The `position_map` ($264) maps each section to its element ids,
/// resolving a device "go to location N". A reading-order section absent from
/// it takes no location jump (warning; skipped without a `position_map`).
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
// Rule 14 — position and location arithmetic
// ============================================================================

/// Rule 14. The `position_id_map` ($265) span shape partitions the pid axis
/// (§10.2): its spans run from 0, each starting where the last ended.
/// `yj.location_pid_map` ($621) pids never go backwards (§10.3).
fn check_position_arithmetic(book: &BookData) -> Vec<Finding> {
    let mut out = Vec::new();

    let spans = position_spans(book);
    if let Some((name, pid, _)) = spans.first()
        && *pid != 0
    {
        out.push(warning(
            "position-spans-ragged",
            name,
            format!(
                "the position_id_map starts section {name:?} at pid {pid} — the pids below it \
                 belong to no section"
            ),
            None,
        ));
    }
    for pair in spans.windows(2) {
        let (prev_name, prev_pid, prev_len) = &pair[0];
        let (name, pid, _) = &pair[1];
        let end = prev_pid + prev_len;
        if *pid != end {
            let gap = if *pid > end {
                "leaves a gap"
            } else {
                "overlaps"
            };
            out.push(warning(
                "position-spans-ragged",
                name,
                format!(
                    "section {prev_name:?} spans pids {prev_pid}..{end} and section {name:?} \
                     starts at {pid} — the partition {gap}"
                ),
                None,
            ));
            break;
        }
    }

    for (index, pid, prev) in descending_locations(book) {
        out.push(warning(
            "location-boundaries-unordered",
            &index.to_string(),
            format!(
                "yj.location_pid_map boundary {index} is pid {pid}, below the {prev} before it — \
                 the Location numbers it feeds don't advance"
            ),
            None,
        ));
    }

    out
}

/// The `position_id_map` ($265) span shape as `(section_name, pid, length)` in
/// fragment order. Empty for the `{eid, pid}` pair shape, which states no
/// section length.
fn position_spans(book: &BookData) -> Vec<(String, i64, i64)> {
    let mut spans = Vec::new();
    let Some(maps) = book.by_type.get(&(KfxSymbol::PositionIdMap as u64)) else {
        return spans;
    };
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
            let (Some(name), Some(pid), Some(length)) = (
                get_field(ef, KfxSymbol::SectionName as u64).and_then(|v| book.symbols.text_of(v)),
                get_field(ef, KfxSymbol::Pid as u64).and_then(|v| v.as_int()),
                get_field(ef, KfxSymbol::Length as u64).and_then(|v| v.as_int()),
            ) else {
                continue;
            };
            spans.push((name.to_string(), pid, length));
        }
    }
    spans
}

/// Every `yj.location_pid_map` ($621) boundary that sits below its
/// predecessor, as `(index, pid, predecessor)`. Amazon repeats a pid where two
/// Locations fall in one place; equality is not a defect.
fn descending_locations(book: &BookData) -> Vec<(usize, i64, i64)> {
    let mut out = Vec::new();
    let Some(maps) = book.by_type.get(&(KfxSymbol::YjLocationPidMap as u64)) else {
        return out;
    };
    for value in maps.values() {
        let Some(groups) = value.unwrap_annotated().as_list() else {
            continue;
        };
        for group in groups {
            let Some(pids) = group
                .unwrap_annotated()
                .as_struct()
                .and_then(|fields| get_field(fields, KfxSymbol::Locations as u64))
                .and_then(|v| v.unwrap_annotated().as_list())
            else {
                continue;
            };
            let mut previous: Option<i64> = None;
            for (index, entry) in pids.iter().enumerate() {
                let Some(pid) = entry.as_int() else {
                    continue;
                };
                if let Some(prev) = previous
                    && pid < prev
                {
                    out.push((index, pid, prev));
                }
                previous = Some(pid);
            }
        }
    }
    out
}

// ============================================================================
// Rule 11 — declared features agree with the content
// ============================================================================

/// Positions a section holds below the large-section declaration.
const SECTION_PID_BOUND: i64 = 65536;

/// Container contents, for the `content_features` claims to be checked
/// against.
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

/// Positions in the longest section, from the `position_id_map` ($265) span
/// shape. The `{eid, pid}` pair shape states no section length and yields
/// `None`.
fn max_section_pids(book: &BookData) -> Option<i64> {
    position_spans(book)
        .into_iter()
        .map(|(_, _, length)| length)
        .max()
}

/// The two namespaces a `content_features` entry is declared under.
const FEATURE_NAMESPACES: [&str; 2] = ["com.amazon.yjconversion", "SDK.Marker"];

/// Declared features keyed by feature name, valued by major version, with one
/// [`Finding`] per entry outside [`FEATURE_NAMESPACES`].
fn declared_features(book: &BookData) -> (HashMap<String, i64>, Vec<Finding>) {
    let mut out = HashMap::new();
    let mut findings = Vec::new();
    let Some(entities) = book.by_type.get(&(KfxSymbol::ContentFeatures as u64)) else {
        return (out, findings);
    };
    let mut unknown_namespaces: BTreeSet<String> = BTreeSet::new();
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
            if let Some(namespace) =
                get_field(ff, KfxSymbol::Namespace as u64).and_then(|v| book.symbols.text_of(v))
                && !FEATURE_NAMESPACES.contains(&namespace)
            {
                unknown_namespaces.insert(namespace.to_string());
            }
            let Some(key) =
                get_field(ff, KfxSymbol::Key as u64).and_then(|v| book.symbols.text_of(v))
            else {
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
    for namespace in unknown_namespaces {
        findings.push(info(
            "feature-namespace-unknown",
            "<content_features>",
            format!("a feature is declared under namespace {namespace:?}"),
        ));
    }
    (out, findings)
}

fn check_feature_content_agreement(book: &BookData) -> Vec<Finding> {
    let (declared, mut out) = declared_features(book);
    if declared.is_empty() {
        return out; // No content_features: no claim to check.
    }
    let facts = ContentFacts::read(book);

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

/// A [`Severity::Info`] finding: a value the format permits and no rule here
/// recognises.
fn info(rule: &str, location: &str, message: impl Into<String>) -> Finding {
    Finding {
        check: "kfx",
        rule: rule.to_string(),
        severity: Severity::Info,
        location: location.to_string(),
        message: message.into(),
        fix: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header + container_info over an empty index table, with each
    /// container-layer scalar settable. [`Container::default`] builds one that
    /// loads as an entity-less book and raises no rule-1 finding.
    struct Container {
        version: u16,
        drm_scheme: i64,
        compr_type: i64,
        cont_id: Option<String>,
        index_length: i64,
        /// The entities the index table names, in file order.
        entities: Vec<Entity>,
        /// Names for the doc-symbols fragment's `imports` list.
        imports: Vec<String>,
        /// Local symbol names for the doc-symbols fragment's `symbols` list.
        local_symbols: Vec<String>,
        /// The doc-symbols fragment's own `max_id`. `None` declares the value
        /// §5.4 asks for: the import ceiling plus the local symbols listed.
        local_max_id: Option<i64>,
    }

    impl Default for Container {
        fn default() -> Self {
            Self {
                version: CONTAINER_VERSION,
                drm_scheme: 0,
                compr_type: 0,
                cont_id: Some("CR!2V5GMJ5B652W7ED0CNV1210FAXAR".to_string()),
                index_length: 0,
                entities: Vec::new(),
                imports: vec![SHARED_SYMBOL_TABLE.to_string()],
                local_symbols: Vec::new(),
                local_max_id: None,
            }
        }
    }

    /// One entity: an index-table row and the bytes it addresses.
    struct Entity {
        type_id: u32,
        id: u32,
        payload: Vec<u8>,
        /// The length the row declares, where it differs from the payload
        /// written — an out-of-bounds row names more bytes than the file holds.
        declared_length: Option<u64>,
    }

    /// An index row naming a zero-length entity, enough for the rules that
    /// read only the table.
    fn row(ftype: KfxSymbol, id: u32) -> Entity {
        Entity {
            type_id: ftype as u64 as u32,
            id,
            payload: Vec::new(),
            declared_length: None,
        }
    }

    impl Entity {
        fn holding(mut self, payload: Vec<u8>) -> Self {
            self.payload = payload;
            self
        }

        fn declaring_length(mut self, length: u64) -> Self {
            self.declared_length = Some(length);
            self
        }
    }

    /// A minimal Ion payload: a struct naming one section.
    fn ion_payload() -> Vec<u8> {
        use crate::formats::kfx::ion::IonWriter;

        let mut writer = IonWriter::new();
        writer.write_bvm();
        writer.write_value(&IonValue::Struct(vec![(
            KfxSymbol::SectionName as u64,
            IonValue::Symbol(KfxSymbol::Null as u64),
        )]));
        writer.into_bytes()
    }

    impl Container {
        fn build(&self) -> Vec<u8> {
            use crate::formats::kfx::ion::IonWriter;

            const HEADER_LEN: u32 = 18;

            // Entity payloads sit first, so the index rows can name offsets
            // relative to the header. Index table and doc symbols follow.
            let mut payloads: Vec<u8> = Vec::new();
            let mut index_table = Vec::new();
            for ent in &self.entities {
                let offset = payloads.len() as u64;
                let length = ent.declared_length.unwrap_or(ent.payload.len() as u64);
                payloads.extend_from_slice(&ent.payload);
                index_table.extend_from_slice(&ent.id.to_le_bytes());
                index_table.extend_from_slice(&ent.type_id.to_le_bytes());
                index_table.extend_from_slice(&offset.to_le_bytes());
                index_table.extend_from_slice(&length.to_le_bytes());
            }
            let index_length = if self.entities.is_empty() {
                self.index_length
            } else {
                index_table.len() as i64
            };

            let imports: Vec<IonValue> = self
                .imports
                .iter()
                .map(|name| {
                    IonValue::Struct(vec![
                        (4, IonValue::String(name.clone())),
                        (8, IonValue::Int(FALLBACK_IMPORT_MAX_ID)),
                    ])
                })
                .collect();
            let locals: Vec<IonValue> = self
                .local_symbols
                .iter()
                .map(|name| IonValue::String(name.clone()))
                .collect();
            let local_max_id = self.local_max_id.unwrap_or(
                FALLBACK_IMPORT_MAX_ID * self.imports.len() as i64 + locals.len() as i64,
            );
            let mut symbol_writer = IonWriter::new();
            symbol_writer.write_bvm();
            symbol_writer.write_value(&IonValue::Annotated(
                vec![3],
                Box::new(IonValue::Struct(vec![
                    (6, IonValue::List(imports)),
                    (7, IonValue::List(locals)),
                    (8, IonValue::Int(local_max_id)),
                ])),
            ));
            let doc_symbols = symbol_writer.into_bytes();

            let index_offset = HEADER_LEN as i64 + payloads.len() as i64;
            let symbols_offset = index_offset + index_length.max(index_table.len() as i64);

            let mut fields = vec![
                (
                    KfxSymbol::Bcdrmscheme as u64,
                    IonValue::Int(self.drm_scheme),
                ),
                (
                    KfxSymbol::Bccomprtype as u64,
                    IonValue::Int(self.compr_type),
                ),
                (
                    KfxSymbol::Bcindextaboffset as u64,
                    IonValue::Int(index_offset),
                ),
                (
                    KfxSymbol::Bcindextablength as u64,
                    IonValue::Int(index_length),
                ),
                (
                    KfxSymbol::Bcdocsymboloffset as u64,
                    IonValue::Int(symbols_offset),
                ),
                (
                    KfxSymbol::Bcdocsymbollength as u64,
                    IonValue::Int(doc_symbols.len() as i64),
                ),
            ];
            if let Some(id) = &self.cont_id {
                fields.push((KfxSymbol::Bccontid as u64, IonValue::String(id.clone())));
            }

            let mut writer = IonWriter::new();
            writer.write_bvm();
            writer.write_value(&IonValue::Struct(fields));
            let info = writer.into_bytes();

            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"CONT");
            bytes.extend_from_slice(&self.version.to_le_bytes());
            bytes.extend_from_slice(&HEADER_LEN.to_le_bytes());
            // The index table and doc symbols sit where container_info says,
            // and container_info follows them.
            bytes.extend_from_slice(
                &((symbols_offset + doc_symbols.len() as i64) as u32).to_le_bytes(),
            );
            bytes.extend_from_slice(&(info.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&payloads);
            bytes.extend_from_slice(&index_table);
            // An index_length past the written rows is the ragged case; the
            // bytes it names are zero-filled here.
            bytes.resize(symbols_offset as usize, 0);
            bytes.extend_from_slice(&doc_symbols);
            bytes.extend_from_slice(&info);
            bytes
        }
    }

    /// Import `max_id` the test containers declare.
    const FALLBACK_IMPORT_MAX_ID: i64 = 851;

    /// The rules a container raises, sorted.
    fn rules_of(bytes: &[u8]) -> Vec<String> {
        let mut rules: Vec<String> = validate(bytes).into_iter().map(|f| f.rule).collect();
        rules.sort();
        rules
    }

    #[test]
    fn singleton_type_appearing_twice_is_flagged() {
        let doubled = Container {
            entities: vec![
                row(KfxSymbol::DocumentData, 1),
                row(KfxSymbol::DocumentData, 2),
                row(KfxSymbol::Section, 3),
                row(KfxSymbol::Section, 4),
            ],
            ..Default::default()
        }
        .build();
        let out = check_container_inventory(&doubled);
        assert_eq!(out.len(), 1, "only the singleton is flagged: {out:?}");
        assert_eq!(out[0].rule, "singleton-repeated");
        assert_eq!(out[0].severity, Severity::Warning);

        let single = Container {
            entities: vec![row(KfxSymbol::DocumentData, 1)],
            ..Default::default()
        }
        .build();
        assert!(check_container_inventory(&single).is_empty());
    }

    #[test]
    fn symbol_import_other_than_the_shared_table_is_flagged() {
        let other = Container {
            imports: vec!["OtherSymbols".to_string()],
            ..Default::default()
        }
        .build();
        let out = check_container_inventory(&other);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "symbol-import-unexpected");

        assert!(check_container_inventory(&Container::default().build()).is_empty());
    }

    #[test]
    fn entity_reaching_past_the_container_is_flagged() {
        let overrun = Container {
            entities: vec![
                row(KfxSymbol::Section, 852)
                    .holding(ion_payload())
                    .declaring_length(1 << 20),
            ],
            local_symbols: vec!["c0".to_string()],
            ..Default::default()
        }
        .build();
        let out = check_entity_index(&overrun);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "entity-out-of-bounds");
        assert_eq!(out[0].severity, Severity::Error);
        assert_eq!(out[0].location, "section/c0");

        // A length that names only the bytes written reads fine.
        let honest = Container {
            entities: vec![row(KfxSymbol::Section, 852).holding(ion_payload())],
            local_symbols: vec!["c0".to_string()],
            ..Default::default()
        }
        .build();
        assert!(check_entity_index(&honest).is_empty());
    }

    #[test]
    fn non_ion_payload_is_flagged_outside_the_media_types() {
        let garbage = vec![0xFFu8; 16];
        let ion_type = Container {
            entities: vec![row(KfxSymbol::Section, 852).holding(garbage.clone())],
            local_symbols: vec!["c0".to_string()],
            ..Default::default()
        }
        .build();
        let out = check_entity_index(&ion_type);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "entity-payload-unparsable");
        assert_eq!(out[0].severity, Severity::Error);

        // §11.2: media bytes are the media file, never Ion.
        for media in [KfxSymbol::Bcrawmedia, KfxSymbol::Bcrawfont] {
            let bytes = Container {
                entities: vec![row(media, 852).holding(garbage.clone())],
                local_symbols: vec!["c0".to_string()],
                ..Default::default()
            }
            .build();
            assert!(check_entity_index(&bytes).is_empty(), "{media:?}");
        }
    }

    #[test]
    fn two_fragments_of_one_type_sharing_a_name_are_flagged() {
        let mut second = ion_payload();
        second.push(0x0F); // an Ion null, so the payload still parses
        let repeated = Container {
            entities: vec![
                row(KfxSymbol::Section, 852).holding(ion_payload()),
                row(KfxSymbol::Section, 852).holding(second),
            ],
            local_symbols: vec!["c0".to_string()],
            ..Default::default()
        }
        .build();
        let out = check_entity_index(&repeated);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "fragment-name-collision");
        assert_eq!(out[0].severity, Severity::Error);

        // The same name over identical bytes costs a reader nothing.
        let identical = Container {
            entities: vec![
                row(KfxSymbol::Section, 852).holding(ion_payload()),
                row(KfxSymbol::Section, 852).holding(ion_payload()),
            ],
            local_symbols: vec!["c0".to_string()],
            ..Default::default()
        }
        .build();
        assert!(check_entity_index(&identical).is_empty());

        // One name per fragment, and the same name under another type, both
        // read fine: §6.1 keys by the pair.
        let distinct = Container {
            entities: vec![
                row(KfxSymbol::Section, 852).holding(ion_payload()),
                row(KfxSymbol::Section, 853).holding(ion_payload()),
                row(KfxSymbol::PositionIdMap, 852).holding(ion_payload()),
            ],
            local_symbols: vec!["c0".to_string(), "c1".to_string()],
            ..Default::default()
        }
        .build();
        assert!(check_entity_index(&distinct).is_empty());

        // Singletons all carry the reserved id `$348`; `singleton-repeated`
        // counts those.
        let singletons = Container {
            entities: vec![
                row(KfxSymbol::DocumentData, KfxSymbol::Null as u64 as u32).holding(ion_payload()),
                row(KfxSymbol::DocumentData, KfxSymbol::Null as u64 as u32).holding(ion_payload()),
            ],
            ..Default::default()
        }
        .build();
        assert!(check_entity_index(&singletons).is_empty());
    }

    // --- Wave 4 (arithmetic over one fragment) -----------------------------

    #[test]
    fn symbol_table_max_id_must_reach_its_last_local_symbol() {
        let names = vec!["c0".to_string(), "c9".to_string(), "cR".to_string()];
        let short = Container {
            local_symbols: names.clone(),
            local_max_id: Some(FALLBACK_IMPORT_MAX_ID + 1),
            ..Default::default()
        }
        .build();
        let out = check_container_inventory(&short);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "symbol-table-max-id-mismatch");
        assert_eq!(out[0].severity, Severity::Warning);

        let exact = Container {
            local_symbols: names,
            ..Default::default()
        }
        .build();
        assert!(check_container_inventory(&exact).is_empty());
    }

    #[test]
    fn encrypted_container_is_reported_as_encrypted() {
        let out = validate(
            &Container {
                drm_scheme: 1,
                ..Default::default()
            }
            .build(),
        );
        assert_eq!(out.len(), 1, "encryption ends the run: {out:?}");
        assert_eq!(out[0].rule, "container-encrypted");
        assert_eq!(out[0].severity, Severity::Error);
        assert!(!out.iter().any(|f| f.rule == "no-sections"));
    }

    #[test]
    fn compressed_container_is_reported_as_compressed() {
        let out = validate(
            &Container {
                compr_type: 1,
                ..Default::default()
            }
            .build(),
        );
        assert_eq!(out.len(), 1, "compression ends the run: {out:?}");
        assert_eq!(out[0].rule, "container-compressed");
        assert_eq!(out[0].severity, Severity::Error);
    }

    #[test]
    fn readable_container_is_read_not_refused() {
        let rules = rules_of(&Container::default().build());
        assert!(!rules.iter().any(|r| r.starts_with("container-")));
        assert!(rules.iter().any(|r| r == "no-sections"));
    }

    #[test]
    fn container_scalars_flag_version_index_and_id() {
        let bytes = Container {
            version: 1,
            cont_id: Some("not-an-acr".to_string()),
            index_length: 30, // one whole 24-byte entry plus 6 bytes
            ..Default::default()
        }
        .build();
        let rules = rules_of(&bytes);
        assert!(rules.iter().any(|r| r == "container-version-unexpected"));
        assert!(rules.iter().any(|r| r == "index-table-ragged"));
        assert!(rules.iter().any(|r| r == "container-id-malformed"));
        for finding in validate(&bytes) {
            if finding.rule.starts_with("container-id")
                || finding.rule.starts_with("container-version")
                || finding.rule == "index-table-ragged"
            {
                assert_eq!(finding.severity, Severity::Warning);
            }
        }
    }

    #[test]
    fn container_id_absent_is_its_own_finding() {
        let rules = rules_of(
            &Container {
                cont_id: None,
                ..Default::default()
            }
            .build(),
        );
        assert!(rules.iter().any(|r| r == "container-id-missing"));
        assert!(!rules.iter().any(|r| r == "container-id-malformed"));
    }

    #[test]
    fn container_id_accepts_the_generated_shape() {
        let id = crate::formats::kfx::serialization::generate_container_id("t");
        assert!(is_container_id(&id), "{id}");
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
        let mut book = loader::empty_book_for_test();
        book.by_type = by_type;
        book.raw_media
            .insert("resource/present".to_string(), vec![0xFF, 0xD8]);

        let out = check_resource_bytes(&book);
        assert_eq!(out.len(), 1, "only the missing-bytes resource is flagged");
        assert_eq!(out[0].rule, "resource-missing-bytes");
        assert_eq!(out[0].severity, Severity::Error);
        assert_eq!(out[0].location, "resource/missing");
    }

    #[test]
    fn resource_bytes_clean_when_no_resources() {
        assert!(check_resource_bytes(&loader::empty_book_for_test()).is_empty());
    }

    #[test]
    fn resource_format_outside_the_decodable_set_is_info() {
        let mut book = loader::empty_book_for_test();
        book.by_type = entities(vec![
            (
                KfxSymbol::ExternalResource as u64,
                "r1",
                IonValue::Struct(vec![(
                    KfxSymbol::Format as u64,
                    IonValue::String("avif".to_string()),
                )]),
            ),
            (
                KfxSymbol::ExternalResource as u64,
                "r2",
                IonValue::Struct(vec![(
                    KfxSymbol::Format as u64,
                    IonValue::String("jxr".to_string()),
                )]),
            ),
        ]);

        let out = check_resource_bytes(&book);
        assert_eq!(out.len(), 1, "only the unknown format is flagged: {out:?}");
        assert_eq!(out[0].rule, "resource-format-unknown");
        assert_eq!(out[0].severity, Severity::Info);
        assert_eq!(out[0].location, "r1");
    }

    // --- Wave 2 (vocabulary) -----------------------------------------------

    /// A book whose one reading-order section holds `element` as its single
    /// page template.
    fn book_with_element(element: IonValue) -> BookData {
        book_with_elements(vec![element])
    }

    /// A book whose one reading-order section lists `elements` as its page
    /// templates, in order.
    fn book_with_elements(elements: Vec<IonValue>) -> BookData {
        let mut book = loader::empty_book_for_test();
        book.by_type
            .insert(KfxSymbol::DocumentData as u64, doc_data(&["sec1"]));
        book.by_type.insert(
            KfxSymbol::Section as u64,
            HashMap::from([(
                "sec1".to_string(),
                IonValue::Struct(vec![(
                    KfxSymbol::PageTemplates as u64,
                    IonValue::List(elements),
                )]),
            )]),
        );
        book
    }

    #[test]
    fn element_type_outside_the_schema_is_info() {
        // $596 horizontal_rule has an import strategy; $445 text_vert_anchor
        // names no element type at all.
        let book = book_with_element(IonValue::Struct(vec![(
            KfxSymbol::Type as u64,
            IonValue::Symbol(KfxSymbol::TextVertAnchor as u64),
        )]));
        let out = check_references(&book);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "element-type-unknown");
        assert_eq!(out[0].severity, Severity::Info);

        let known = book_with_element(IonValue::Struct(vec![(
            KfxSymbol::Type as u64,
            IonValue::Symbol(KfxSymbol::HorizontalRule as u64),
        )]));
        assert!(check_references(&known).is_empty());
    }

    #[test]
    fn writing_mode_outside_the_css_set_is_flagged() {
        let book = book_with_element(IonValue::Struct(vec![(
            KfxSymbol::WritingMode as u64,
            IonValue::String("sideways_lr".to_string()),
        )]));
        let out = check_references(&book);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "writing-mode-unknown");
        assert_eq!(out[0].severity, Severity::Warning);

        for mode in WRITING_MODES {
            let ok = book_with_element(IonValue::Struct(vec![(
                KfxSymbol::WritingMode as u64,
                IonValue::String(mode.to_string()),
            )]));
            assert!(check_references(&ok).is_empty(), "{mode}");
        }
    }

    #[test]
    fn list_item_outside_a_list_is_flagged() {
        let listitem = || {
            IonValue::Struct(vec![(
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::Listitem as u64),
            )])
        };
        let orphan = book_with_element(listitem());
        let out = check_references(&orphan);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "list-item-outside-list");

        // The same item under a `list` parent, through a content_list wrapper.
        let nested = book_with_element(IonValue::Struct(vec![
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::List as u64),
            ),
            (
                KfxSymbol::ContentList as u64,
                IonValue::List(vec![listitem()]),
            ),
        ]));
        assert!(check_references(&nested).is_empty());
    }

    #[test]
    fn nav_type_outside_the_known_set_is_info() {
        let nav = |nav_type: &str| {
            let mut book = loader::empty_book_for_test();
            book.by_type.insert(
                KfxSymbol::BookNavigation as u64,
                HashMap::from([(
                    "nav".to_string(),
                    IonValue::List(vec![IonValue::Struct(vec![(
                        KfxSymbol::NavType as u64,
                        IonValue::String(nav_type.to_string()),
                    )])]),
                )]),
            );
            book
        };
        let out = check_nav_vocabulary(&nav("guide"));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "nav-type-unknown");
        assert_eq!(out[0].severity, Severity::Info);
        for known in NAV_TYPES {
            assert!(check_nav_vocabulary(&nav(known)).is_empty(), "{known}");
        }
    }

    #[test]
    fn feature_namespace_outside_the_known_pair_is_info() {
        let mut book = loader::empty_book_for_test();
        book.by_type.insert(
            KfxSymbol::ContentFeatures as u64,
            HashMap::from([(
                "cf".to_string(),
                IonValue::Struct(vec![(
                    KfxSymbol::Features as u64,
                    IonValue::List(vec![IonValue::Struct(vec![
                        (
                            KfxSymbol::Namespace as u64,
                            IonValue::String("com.example.other".to_string()),
                        ),
                        (KfxSymbol::Key as u64, IonValue::String("k".to_string())),
                    ])]),
                )]),
            )]),
        );
        let (declared, findings) = declared_features(&book);
        assert!(declared.contains_key("k"));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule, "feature-namespace-unknown");
        assert_eq!(findings[0].severity, Severity::Info);
    }

    // --- Wave 3 (presence and shape) ---------------------------------------

    #[test]
    fn section_without_page_templates_is_flagged() {
        let mut book = loader::empty_book_for_test();
        book.by_type
            .insert(KfxSymbol::DocumentData as u64, doc_data(&["sec1"]));
        book.by_type.insert(
            KfxSymbol::Section as u64,
            HashMap::from([("sec1".to_string(), IonValue::Struct(vec![]))]),
        );
        let out = check_references(&book);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "section-empty");
        assert_eq!(out[0].location, "sec1");
    }

    #[test]
    fn table_without_rows_is_flagged() {
        let rowless = book_with_element(IonValue::Struct(vec![(
            KfxSymbol::Type as u64,
            IonValue::Symbol(KfxSymbol::Table as u64),
        )]));
        let out = check_references(&rowless);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "table-without-rows");

        let with_row = book_with_element(IonValue::Struct(vec![
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::Table as u64),
            ),
            (
                KfxSymbol::ContentList as u64,
                IonValue::List(vec![IonValue::Struct(vec![(
                    KfxSymbol::Type as u64,
                    IonValue::Symbol(KfxSymbol::TableRow as u64),
                )])]),
            ),
        ]));
        assert!(check_references(&with_row).is_empty());
    }

    #[test]
    fn element_with_content_and_children_is_info() {
        let book = book_with_element(IonValue::Struct(vec![
            (
                KfxSymbol::Content as u64,
                IonValue::String("text".to_string()),
            ),
            (KfxSymbol::ContentList as u64, IonValue::List(vec![])),
        ]));
        let out = check_references(&book);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "element-content-and-children");
        assert_eq!(out[0].severity, Severity::Info);
    }

    /// A book whose metadata is exactly `fields`.
    fn book_with_metadata(meta: crate::formats::kfx::loader::BookMetadata) -> BookData {
        let mut book = loader::empty_book_for_test();
        book.metadata = meta;
        book
    }

    #[test]
    fn metadata_absences_are_flagged() {
        let out = check_metadata(&book_with_metadata(Default::default()), None);
        let rules: Vec<&str> = out.iter().map(|f| f.rule.as_str()).collect();
        assert!(rules.contains(&"metadata-no-title"), "{rules:?}");
        assert!(rules.contains(&"metadata-no-language"), "{rules:?}");
    }

    #[test]
    fn metadata_clean_when_stated() {
        let meta = crate::formats::kfx::loader::BookMetadata {
            title: "人間失格".to_string(),
            language: "ja".to_string(),
            authors: vec!["太宰 治".to_string()],
            author_pronunciations: vec!["だざい おさむ".to_string()],
            ..Default::default()
        };
        assert!(check_metadata(&book_with_metadata(meta), None).is_empty());
    }

    #[test]
    fn metadata_flags_malformed_language_and_surplus_pronunciations() {
        let meta = crate::formats::kfx::loader::BookMetadata {
            title: "t".to_string(),
            language: "japanese!".to_string(),
            authors: vec!["a".to_string()],
            author_pronunciations: vec!["p1".to_string(), "p2".to_string()],
            ..Default::default()
        };
        let out = check_metadata(&book_with_metadata(meta), None);
        let rules: Vec<&str> = out.iter().map(|f| f.rule.as_str()).collect();
        assert!(rules.contains(&"metadata-language-malformed"), "{rules:?}");
        assert!(
            rules.contains(&"metadata-pronunciation-surplus"),
            "{rules:?}"
        );
    }

    #[test]
    fn metadata_asset_id_must_match_the_container_id() {
        let title_metadata = |asset_id: &str| {
            let entry = IonValue::Struct(vec![
                (
                    KfxSymbol::Key as u64,
                    IonValue::String("asset_id".to_string()),
                ),
                (
                    KfxSymbol::Value as u64,
                    IonValue::String(asset_id.to_string()),
                ),
            ]);
            let category = IonValue::Struct(vec![(
                KfxSymbol::Metadata as u64,
                IonValue::List(vec![entry]),
            )]);
            HashMap::from([(
                "bm".to_string(),
                IonValue::Struct(vec![(
                    KfxSymbol::CategorisedMetadata as u64,
                    IonValue::List(vec![category]),
                )]),
            )])
        };

        let mut book = book_with_metadata(crate::formats::kfx::loader::BookMetadata {
            title: "t".to_string(),
            language: "en".to_string(),
            ..Default::default()
        });
        book.by_type
            .insert(KfxSymbol::BookMetadata as u64, title_metadata("CR!OTHER"));
        let out = check_metadata(&book, Some("CR!REAL"));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "metadata-asset-id-mismatch");

        book.by_type
            .insert(KfxSymbol::BookMetadata as u64, title_metadata("CR!REAL"));
        assert!(check_metadata(&book, Some("CR!REAL")).is_empty());
    }

    // --- Rules 3 & 6 (reference resolution) --------------------------------
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

    // --- Wave 4 (element and position arithmetic) --------------------------

    /// An element carrying `eid`, enough for the id rule to see it.
    fn element_with_id(eid: i64) -> IonValue {
        IonValue::Struct(vec![
            (KfxSymbol::Id as u64, IonValue::Int(eid)),
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::Text as u64),
            ),
        ])
    }

    #[test]
    fn one_element_id_on_two_elements_is_flagged() {
        let book = book_with_elements(vec![element_with_id(7), element_with_id(7)]);
        let out = check_references(&book);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "element-id-duplicate");
        assert_eq!(out[0].severity, Severity::Error);

        let unique = book_with_elements(vec![element_with_id(7), element_with_id(8)]);
        assert!(check_references(&unique).is_empty());
    }

    /// A text element over the five-character base text `あいうえお`, with
    /// whatever ranged fields `extra` adds. Its five characters occupy fifteen
    /// bytes.
    fn element_over_five_chars(extra: Vec<(u64, IonValue)>) -> IonValue {
        let mut fields = vec![(
            KfxSymbol::Content as u64,
            IonValue::String("あいうえお".to_string()),
        )];
        fields.extend(extra);
        IonValue::Struct(fields)
    }

    fn style_event(offset: i64, length: i64) -> Vec<(u64, IonValue)> {
        vec![(
            KfxSymbol::StyleEvents as u64,
            IonValue::List(vec![IonValue::Struct(vec![
                (KfxSymbol::Offset as u64, IonValue::Int(offset)),
                (KfxSymbol::Length as u64, IonValue::Int(length)),
            ])]),
        )]
    }

    #[test]
    fn style_event_reaching_past_the_base_text_is_flagged() {
        let over = book_with_element(element_over_five_chars(style_event(3, 4)));
        let out = check_references(&over);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "style-event-past-text");
        assert_eq!(out[0].severity, Severity::Warning);

        let flush = book_with_element(element_over_five_chars(style_event(3, 2)));
        assert!(check_references(&flush).is_empty());
    }

    fn word_boundaries(steps: &[i64]) -> Vec<(u64, IonValue)> {
        vec![(
            KfxSymbol::WordBoundaryList as u64,
            IonValue::List(steps.iter().copied().map(IonValue::Int).collect()),
        )]
    }

    #[test]
    fn word_boundary_list_must_fit_the_base_text() {
        // The pairs walk (gap, length) from the start of the text.
        let over = book_with_element(element_over_five_chars(word_boundaries(&[0, 3, 0, 4])));
        let out = check_references(&over);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "word-boundaries-past-text");

        let odd = book_with_element(element_over_five_chars(word_boundaries(&[0, 3, 0])));
        assert_eq!(check_references(&odd).len(), 1, "an unclosed pair");

        let fits = book_with_element(element_over_five_chars(word_boundaries(&[0, 3, 0, 2])));
        assert!(check_references(&fits).is_empty());
    }

    /// A `margin_top` whose length struct states `value` and `unit`.
    fn margin(value: IonValue, unit: &str) -> IonValue {
        IonValue::Struct(vec![(
            KfxSymbol::MarginTop as u64,
            IonValue::Struct(vec![
                (KfxSymbol::Value as u64, value),
                (KfxSymbol::Unit as u64, IonValue::String(unit.to_string())),
            ]),
        )])
    }

    #[test]
    fn length_unit_outside_the_css_set_is_flagged() {
        let out = check_references(&book_with_element(margin(IonValue::Int(1), "furlong")));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "length-unit-unknown");
        assert_eq!(out[0].severity, Severity::Warning);

        assert!(check_references(&book_with_element(margin(IonValue::Int(1), "lh"))).is_empty());
    }

    #[test]
    fn length_value_that_is_no_number_is_flagged() {
        let out = check_references(&book_with_element(margin(
            IonValue::String("1.2".to_string()),
            "em",
        )));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "length-value-not-numeric");

        // An Ion decimal is a number held as text for precision.
        let decimal = margin(IonValue::Decimal("0.982286".to_string()), "em");
        assert!(check_references(&book_with_element(decimal)).is_empty());
    }

    #[test]
    fn length_inside_a_style_entity_is_checked_too() {
        // The content walk names styles but never descends into them.
        let mut book = book_with_element(IonValue::Struct(vec![(
            KfxSymbol::Style as u64,
            IonValue::String("st1".to_string()),
        )]));
        book.by_type.insert(
            KfxSymbol::Style as u64,
            HashMap::from([("st1".to_string(), margin(IonValue::Int(1), "furlong"))]),
        );
        let out = check_references(&book);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "length-unit-unknown");
    }

    /// A `rows`×`columns` table naming `cells` as its `important_cells`.
    fn table(rows: usize, columns: usize, cells: &[(i64, i64)]) -> IonValue {
        let row = IonValue::Struct(vec![
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::TableRow as u64),
            ),
            (
                KfxSymbol::ContentList as u64,
                IonValue::List(
                    (0..columns)
                        .map(|_| {
                            IonValue::Struct(vec![(
                                KfxSymbol::Type as u64,
                                IonValue::Symbol(KfxSymbol::Text as u64),
                            )])
                        })
                        .collect(),
                ),
            ),
        ]);
        IonValue::Struct(vec![
            (
                KfxSymbol::Type as u64,
                IonValue::Symbol(KfxSymbol::Table as u64),
            ),
            (
                KfxSymbol::ImportantCells as u64,
                IonValue::List(
                    cells
                        .iter()
                        .map(|(r, c)| IonValue::List(vec![IonValue::Int(*r), IonValue::Int(*c)]))
                        .collect(),
                ),
            ),
            (
                KfxSymbol::ContentList as u64,
                IonValue::List((0..rows).map(|_| row.clone()).collect()),
            ),
        ])
    }

    #[test]
    fn important_cell_outside_the_grid_is_flagged() {
        let out = check_references(&book_with_element(table(2, 2, &[(0, 1), (2, 0)])));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "important-cell-out-of-range");
        assert_eq!(out[0].location, "[2, 0] in 2x2");

        assert!(check_references(&book_with_element(table(2, 2, &[(0, 1), (1, 0)]))).is_empty());
    }

    /// A `position_id_map` whose span shape lists `(section, pid, length)`.
    fn span_map(spans: &[(&str, i64, i64)]) -> HashMap<String, IonValue> {
        let entries: Vec<IonValue> = spans
            .iter()
            .map(|(name, pid, length)| {
                IonValue::Struct(vec![
                    (
                        KfxSymbol::SectionName as u64,
                        IonValue::String((*name).to_string()),
                    ),
                    (KfxSymbol::Pid as u64, IonValue::Int(*pid)),
                    (KfxSymbol::Length as u64, IonValue::Int(*length)),
                ])
            })
            .collect();
        HashMap::from([(
            "pidmap".to_string(),
            IonValue::Struct(vec![(KfxSymbol::Contains as u64, IonValue::List(entries))]),
        )])
    }

    #[test]
    fn position_spans_must_tile_the_pid_axis() {
        let mut book = loader::empty_book_for_test();
        book.by_type.insert(
            KfxSymbol::PositionIdMap as u64,
            span_map(&[("c0", 0, 2), ("c9", 2, 20), ("cU", 22, 234)]),
        );
        assert!(check_position_arithmetic(&book).is_empty());

        for spans in [
            &[("c0", 0, 2), ("c9", 3, 20)][..], // a gap
            &[("c0", 0, 2), ("c9", 1, 20)][..], // an overlap
            &[("c0", 4, 2), ("c9", 6, 20)][..], // pids below the first span
        ] {
            let mut ragged = loader::empty_book_for_test();
            ragged
                .by_type
                .insert(KfxSymbol::PositionIdMap as u64, span_map(spans));
            let out = check_position_arithmetic(&ragged);
            assert_eq!(out.len(), 1, "{spans:?} → {out:?}");
            assert_eq!(out[0].rule, "position-spans-ragged");
        }
    }

    #[test]
    fn location_boundaries_never_go_backwards() {
        let map = |pids: &[i64]| {
            HashMap::from([(
                "locmap".to_string(),
                IonValue::List(vec![IonValue::Struct(vec![(
                    KfxSymbol::Locations as u64,
                    IonValue::List(pids.iter().copied().map(IonValue::Int).collect()),
                )])]),
            )])
        };

        let mut back = loader::empty_book_for_test();
        back.by_type
            .insert(KfxSymbol::YjLocationPidMap as u64, map(&[0, 2, 8, 5, 13]));
        let out = check_position_arithmetic(&back);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].rule, "location-boundaries-unordered");

        // Amazon repeats a pid where two Locations fall in one place.
        let mut repeated = loader::empty_book_for_test();
        repeated
            .by_type
            .insert(KfxSymbol::YjLocationPidMap as u64, map(&[0, 2, 2, 8, 13]));
        assert!(check_position_arithmetic(&repeated).is_empty());
    }
}
