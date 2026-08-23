//! KFX → `BookData` loader.
//!
//! Parses the container, resolves every fragment's name from the symbol
//! table, and groups the fragments by ftype symbol id. A `bcRawMedia` ($417)
//! value is the raw payload bytes; every other value is a parsed `IonValue`.
//!
//! Loading is eager: a KFX container runs to a few hundred entities under
//! 100 MB.

use std::collections::HashMap;

use crate::formats::kfx::container::{
    EntityLoc, get_field, parse_container_header, parse_container_info, parse_index_table,
    skip_enty_header,
};
use crate::formats::kfx::ion::{IonParser, IonValue};
use crate::formats::kfx::symbols::KfxSymbol;

use super::error::KfxError;

/// Book-wide metadata pulled out of `book_metadata` ($490).
#[derive(Debug, Clone, Default)]
pub struct BookMetadata {
    pub title: String,
    pub authors: Vec<String>,
    pub language: String,
    pub identifier: String,
    pub publisher: Option<String>,
    /// Cover image's `resource_name` (e.g. `"eF"`), when KFX declares one.
    pub cover_resource_name: Option<String>,
    /// `kindle_title_metadata/ASIN` — Amazon catalogue id, emitted in OPF as
    /// `<dc:identifier opf:scheme="ASIN">` and `opf:scheme="MOBI-ASIN"`.
    pub asin: Option<String>,
    /// `kindle_title_metadata/issue_date` — publication date, `YYYY-MM-DD`,
    /// raw for the OPF emitter to format.
    pub issue_date: Option<String>,
    /// `kindle_title_metadata/title_pronunciation` — title sort key
    /// (yomigana for Japanese books). Surfaces in OPF as
    /// `<dc:title opf:file-as="...">`.
    pub title_pronunciation: Option<String>,
    /// `kindle_title_metadata/author_pronunciation` — per-author sort keys,
    /// one per repeated key in source order, positional with `authors`.
    pub author_pronunciations: Vec<String>,
}

/// A KFX container's fragments, media and symbols, as an EPUB write reads
/// them.
pub struct BookData {
    /// All entities indexed by `(ftype_symbol_id, fid_string)`: the KFX type
    /// id (164 = `external_resource`) and the symbol table's resolved name
    /// (`"content_30"`). `bcRawMedia` ($417) lives in `raw_media`.
    pub by_type: HashMap<u64, HashMap<String, IonValue>>,

    /// `bcRawMedia` payloads, keyed by entity name (e.g. `"resource/rsrc562"`).
    ///
    /// Raw bytes, often large, held apart from `by_type`'s parsed Ion.
    pub raw_media: HashMap<String, Vec<u8>>,

    /// Resolved symbol table (KFX base + container doc_symbols), indexed by
    /// full symbol id: a base symbol below the container's declared import
    /// size, a doc_symbol at or above it.
    pub symbols: SymbolTable,

    /// Book metadata derived from `book_metadata` ($490).
    pub metadata: BookMetadata,
}

// Re-export for the `loader::SymbolTable` paths.
pub use crate::formats::kfx::container::SymbolTable;

/// Load a KFX container in memory into `BookData`.
pub fn load(kfx_bytes: &[u8]) -> Result<BookData, KfxError> {
    let header =
        parse_container_header(kfx_bytes).map_err(|e| KfxError::InvalidKfx(e.to_string()))?;

    if header.container_info_offset + header.container_info_length > kfx_bytes.len() {
        return Err(KfxError::InvalidKfx("container info out of bounds".into()));
    }
    let info_bytes = &kfx_bytes
        [header.container_info_offset..header.container_info_offset + header.container_info_length];
    let info = parse_container_info(info_bytes).map_err(|e| KfxError::InvalidKfx(e.to_string()))?;

    if info.drm_scheme != 0 {
        return Err(KfxError::Encrypted(info.drm_scheme));
    }
    if info.compr_type != 0 {
        return Err(KfxError::Compressed(info.compr_type));
    }

    let symbols = match info.doc_symbols {
        Some((off, len)) if off + len <= kfx_bytes.len() => {
            SymbolTable::from_fragment(Some(&kfx_bytes[off..off + len]))
        }
        _ => SymbolTable::from_fragment(None),
    };

    let (idx_off, idx_len) = info
        .index
        .ok_or_else(|| KfxError::InvalidKfx("missing index table".into()))?;
    if idx_off + idx_len > kfx_bytes.len() {
        return Err(KfxError::InvalidKfx("index out of bounds".into()));
    }
    let entities = parse_index_table(&kfx_bytes[idx_off..idx_off + idx_len], header.header_len);

    let mut by_type: HashMap<u64, HashMap<String, IonValue>> = HashMap::new();
    let mut raw_media: HashMap<String, Vec<u8>> = HashMap::new();

    for ent in &entities {
        if ent.offset + ent.length > kfx_bytes.len() {
            continue;
        }
        let entity_bytes = &kfx_bytes[ent.offset..ent.offset + ent.length];
        let payload = skip_enty_header(entity_bytes);

        // bcRawMedia ($417): raw image/font bytes, keyed by the entity id's
        // resolved symbol name (e.g. "resource/rsrc562") — the `location` of
        // the matching external_resource.
        if ent.type_id == KfxSymbol::Bcrawmedia as u32 {
            let key = symbols.resolve(ent.id as u64).to_string();
            if !key.is_empty() && key != "?" {
                raw_media.insert(key, payload.to_vec());
            }
            continue;
        }

        // bcRawFont ($418): raw bytes, keyed the same way as bcRawMedia.
        if ent.type_id == KfxSymbol::Bcrawfont as u32 {
            let key = symbols.resolve(ent.id as u64).to_string();
            if !key.is_empty() && key != "?" {
                raw_media.insert(key, payload.to_vec());
            }
            continue;
        }

        // Everything else is Ion-encoded.
        let Ok(value) = IonParser::new(payload).parse() else {
            continue;
        };

        let fid = resolve_fid(ent, &value, &symbols);
        by_type
            .entry(ent.type_id as u64)
            .or_default()
            .insert(fid, value);
    }

    let metadata = extract_book_metadata(&by_type, &symbols);

    Ok(BookData {
        by_type,
        raw_media,
        symbols,
        metadata,
    })
}

/// An empty `BookData` for unit tests that read no fragments.
#[cfg(test)]
pub(crate) fn empty_book_for_test() -> BookData {
    BookData {
        by_type: HashMap::new(),
        raw_media: HashMap::new(),
        symbols: SymbolTable::from_fragment(None),
        metadata: BookMetadata::default(),
    }
}

/// An entity's fragment id, taken from the index entry's `id` symbol. The
/// payload's own `resource_name` / `style_name` restates it.
fn resolve_fid(ent: &EntityLoc, _value: &IonValue, symbols: &SymbolTable) -> String {
    crate::formats::kfx::resource_index::entity_fid(ent.id as u64, symbols)
}

/// Walk `book_metadata` ($490) to fill the `BookMetadata` struct. Both the
/// `book_metadata::{ categorised_metadata: [...] }` wrapper and a plain
/// unannotated struct are read.
fn extract_book_metadata(
    by_type: &HashMap<u64, HashMap<String, IonValue>>,
    symbols: &SymbolTable,
) -> BookMetadata {
    let mut meta = BookMetadata::default();
    // Preferred source: Amazon's categorised `book_metadata` ($490) wrapper.
    extract_categorised_metadata(&mut meta, by_type, symbols);
    // Fallback: the flat `metadata` ($258) fragment, the sole home of
    // `cover_image` in older Amazon KFX.
    fill_missing_from_flat_metadata(&mut meta, by_type, symbols);
    // Last resort: a KFX declaring no `cover_image` opening on a full-page
    // cover image.
    if meta.cover_resource_name.is_none() {
        meta.cover_resource_name = resolve_cover_from_first_section(by_type, symbols);
    }
    meta
}

/// The cover image of a book with no `cover_image` metadata, walking
/// `reading_orders[0].sections[0]` → `section.$141[0].$176` → `storyline.$146`
/// → the first `resource_name` ($175), confirmed as an `external_resource`.
fn resolve_cover_from_first_section(
    by_type: &HashMap<u64, HashMap<String, IonValue>>,
    symbols: &SymbolTable,
) -> Option<String> {
    let first_section = first_reading_order_section(by_type, symbols)?;
    let section = by_type
        .get(&(KfxSymbol::Section as u64))?
        .get(&first_section)?;
    let sfields = section.unwrap_annotated().as_struct()?;
    let page_templates = get_field(sfields, KfxSymbol::PageTemplates as u64)?.as_list()?;
    let pt0 = page_templates.first()?.unwrap_annotated().as_struct()?;
    let story = get_field(pt0, KfxSymbol::StoryName as u64).and_then(|v| symbols.text_of(v))?;
    let storyline = by_type.get(&(KfxSymbol::Storyline as u64))?.get(story)?;
    let cfields = storyline.unwrap_annotated().as_struct()?;
    let content_list = get_field(cfields, KfxSymbol::ContentList as u64)?;
    let candidate = first_content_resource_name(content_list, symbols)?;
    // A raster `external_resource` only: a PDF-backed KFX's first section
    // renders a `format: pdf` page.
    cover_candidate_is_image(by_type, symbols, &candidate).then_some(candidate)
}

/// `reading_orders[0].sections[0]` from `document_data` ($538), falling back to
/// the flat `metadata` ($258) fragment (older KFX carry it there).
fn first_reading_order_section(
    by_type: &HashMap<u64, HashMap<String, IonValue>>,
    symbols: &SymbolTable,
) -> Option<String> {
    for ftype in [KfxSymbol::DocumentData as u64, KfxSymbol::Metadata as u64] {
        let Some((_, raw)) = by_type.get(&ftype).and_then(|m| m.iter().next()) else {
            continue;
        };
        let Some(fields) = raw.unwrap_annotated().as_struct() else {
            continue;
        };
        if let Some(name) = get_field(fields, KfxSymbol::ReadingOrders as u64)
            .and_then(|v| v.as_list())
            .and_then(|ro| ro.first())
            .and_then(|first| first.as_struct())
            .and_then(|rof| get_field(rof, KfxSymbol::Sections as u64))
            .and_then(|v| v.as_list())
            .and_then(|secs| secs.first())
            .and_then(|s0| symbols.text_of(s0))
        {
            return Some(name.to_string());
        }
    }
    None
}

use crate::formats::kfx::resource_index::first_content_resource_name;

/// True when `name` matches an `external_resource` ($164) whose `format` is a
/// raster image. `pdf`, `kvg` and unrecognised formats are excluded.
fn cover_candidate_is_image(
    by_type: &HashMap<u64, HashMap<String, IonValue>>,
    symbols: &SymbolTable,
    name: &str,
) -> bool {
    const IMAGE_FORMATS: [&str; 7] = ["jpg", "jpeg", "jxr", "png", "gif", "webp", "bmp"];
    let Some(res) = by_type.get(&(KfxSymbol::ExternalResource as u64)) else {
        return false;
    };
    res.values().any(|v| {
        let Some(fields) = v.unwrap_annotated().as_struct() else {
            return false;
        };
        let matches_name = get_field(fields, KfxSymbol::ResourceName as u64)
            .and_then(|x| symbols.text_of(x))
            == Some(name);
        let is_image = get_field(fields, KfxSymbol::Format as u64)
            .and_then(|x| symbols.text_of(x))
            .is_some_and(|fmt| IMAGE_FORMATS.contains(&fmt));
        matches_name && is_image
    })
}

/// Fill `meta` from the categorised `book_metadata` ($490) wrapper's
/// `categorised_metadata / kindle_title_metadata` key/value pairs.
fn extract_categorised_metadata(
    meta: &mut BookMetadata,
    by_type: &HashMap<u64, HashMap<String, IonValue>>,
    symbols: &SymbolTable,
) {
    let Some(entries) = by_type.get(&(KfxSymbol::BookMetadata as u64)) else {
        return;
    };
    let Some((_, raw)) = entries.iter().next() else {
        return;
    };
    let inner = raw.unwrap_annotated();
    let Some(fields) = inner.as_struct() else {
        return;
    };

    let Some(cat_list) =
        get_field(fields, KfxSymbol::CategorisedMetadata as u64).and_then(|v| v.as_list())
    else {
        return;
    };

    for cat in cat_list {
        let Some(cat_fields) = cat.as_struct() else {
            continue;
        };
        let category = get_field(cat_fields, KfxSymbol::Category as u64)
            .and_then(|v| symbols.text_of(v))
            .unwrap_or("");
        if category != "kindle_title_metadata" {
            continue;
        }
        let Some(items) =
            get_field(cat_fields, KfxSymbol::Metadata as u64).and_then(|v| v.as_list())
        else {
            continue;
        };
        for item in items {
            let Some(item_fields) = item.as_struct() else {
                continue;
            };
            let key = get_field(item_fields, KfxSymbol::Key as u64)
                .and_then(|v| v.as_string())
                .unwrap_or("");
            let value_raw = get_field(item_fields, KfxSymbol::Value as u64);
            let value = value_raw.and_then(|v| v.as_string()).unwrap_or("");
            match key {
                "title" if meta.title.is_empty() => {
                    meta.title = value.into();
                }
                // `kindle_title_metadata/author` entries hold source order,
                // which `authors` keeps.
                "author" if !value.is_empty() => {
                    meta.authors.push(value.into());
                }
                "publisher" => meta.publisher = Some(value.trim().into()),
                "language" => meta.language = value.into(),
                "book_id" => meta.identifier = value.into(),
                "ASIN" if meta.asin.is_none() && !value.is_empty() => {
                    meta.asin = Some(value.into());
                }
                "issue_date" if meta.issue_date.is_none() && !value.is_empty() => {
                    meta.issue_date = Some(value.into());
                }
                "cover_image" => {
                    if let Some(name) = resolve_cover_value(value_raw, symbols) {
                        meta.cover_resource_name = Some(name);
                    }
                }
                "title_pronunciation" if !value.is_empty() => {
                    meta.title_pronunciation = Some(value.into());
                }
                // KFX emits one `author_pronunciation` per `author` in source
                // order — collect them all, positional with `authors`.
                "author_pronunciation" if !value.is_empty() => {
                    meta.author_pronunciations.push(value.into());
                }
                _ => {}
            }
        }
    }
}

/// Fall back to the flat `metadata` ($258) fragment for any field `$490`
/// leaves unset. Older Amazon KFX carry their metadata only here, keyed by
/// symbol id — `cover_image` ($424) among them.
fn fill_missing_from_flat_metadata(
    meta: &mut BookMetadata,
    by_type: &HashMap<u64, HashMap<String, IonValue>>,
    symbols: &SymbolTable,
) {
    let Some(entries) = by_type.get(&(KfxSymbol::Metadata as u64)) else {
        return;
    };
    let Some((_, raw)) = entries.iter().next() else {
        return;
    };
    let Some(fields) = raw.unwrap_annotated().as_struct() else {
        return;
    };

    let text = |sym: KfxSymbol| get_field(fields, sym as u64).and_then(IonValue::as_string);

    if meta.cover_resource_name.is_none() {
        meta.cover_resource_name =
            resolve_cover_value(get_field(fields, KfxSymbol::CoverImage as u64), symbols);
    }
    if meta.title.is_empty()
        && let Some(t) = text(KfxSymbol::Title)
    {
        meta.title = t.into();
    }
    if meta.authors.is_empty() {
        // `$222` is a single author string in this shape; tolerate a list too.
        match get_field(fields, KfxSymbol::Author as u64) {
            Some(IonValue::String(s)) if !s.is_empty() => meta.authors.push(s.clone()),
            Some(IonValue::List(items)) => {
                for it in items {
                    if let Some(s) = it.as_string().filter(|s| !s.is_empty()) {
                        meta.authors.push(s.into());
                    }
                }
            }
            _ => {}
        }
    }
    if meta.language.is_empty()
        && let Some(l) = text(KfxSymbol::Language)
    {
        meta.language = l.into();
    }
    if meta.publisher.is_none()
        && let Some(p) = text(KfxSymbol::Publisher)
    {
        meta.publisher = Some(p.trim().into());
    }
    if meta.asin.is_none()
        && let Some(a) = text(KfxSymbol::Asin).filter(|s| !s.is_empty())
    {
        meta.asin = Some(a.into());
    }
}

/// Resolve a `cover_image` value to its external_resource `resource_name`. The
/// value may be a plain string (the categorised `$490` shape), a bare symbol
/// (the flat `$258` shape), or a list whose first element is a symbol/string.
fn resolve_cover_value(value: Option<&IonValue>, symbols: &SymbolTable) -> Option<String> {
    let v = value?;
    if let Some(list) = v.as_list() {
        return list
            .first()
            .and_then(|first| symbols.text_of(first))
            .map(str::to_string);
    }
    // `text_of` resolves both a plain string and a bare symbol id.
    symbols.text_of(v).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load check on the committed 黒死館殺人事件 KFX fixture.
    #[test]
    fn load_ningen_shikkaku_kfx_smoke() {
        let path = "tests/fixtures/[小栗 虫太郎] 黒死館殺人事件 (2012).kfx";
        let bytes = std::fs::read(path).expect("read fixture");
        let _ = load(&bytes).expect("load 黒死館殺人事件.kfx");
    }

    /// Doc-symbol table where symbol id `i` resolves to `names[i]`.
    fn doc_symbols(names: &[&str]) -> SymbolTable {
        SymbolTable::new(0, names.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn resolve_cover_value_accepts_string_symbol_and_list() {
        let symbols = doc_symbols(&["resource/cover"]);
        // Plain string (categorised `$490` shape).
        assert_eq!(
            resolve_cover_value(Some(&IonValue::String("e6".into())), &symbols).as_deref(),
            Some("e6")
        );
        // Bare symbol (flat `$258` shape).
        assert_eq!(
            resolve_cover_value(Some(&IonValue::Symbol(0)), &symbols).as_deref(),
            Some("resource/cover")
        );
        // List whose first element is a symbol.
        assert_eq!(
            resolve_cover_value(Some(&IonValue::List(vec![IonValue::Symbol(0)])), &symbols)
                .as_deref(),
            Some("resource/cover")
        );
        assert_eq!(resolve_cover_value(None, &symbols), None);
    }

    #[test]
    fn first_content_resource_name_walks_nested_content() {
        let symbols = doc_symbols(&["img-1"]);
        // storyline content_list → [ { content_list: [ { resource_name: $0 } ] } ]
        let leaf = IonValue::Struct(vec![(KfxSymbol::ResourceName as u64, IonValue::Symbol(0))]);
        let mid = IonValue::Struct(vec![(
            KfxSymbol::ContentList as u64,
            IonValue::List(vec![leaf]),
        )]);
        let content_list = IonValue::List(vec![mid]);
        assert_eq!(
            first_content_resource_name(&content_list, &symbols).as_deref(),
            Some("img-1")
        );
    }

    #[test]
    fn cover_candidate_requires_image_format() {
        // symbols: 0=name, 1="jpg", 2="pdf"
        let symbols = doc_symbols(&["cover", "jpg", "pdf"]);
        let resource = |fmt_sym: u64| {
            let mut m = HashMap::new();
            m.insert(
                "cover".to_string(),
                IonValue::Struct(vec![
                    (KfxSymbol::ResourceName as u64, IonValue::Symbol(0)),
                    (KfxSymbol::Format as u64, IonValue::Symbol(fmt_sym)),
                ]),
            );
            let mut by_type = HashMap::new();
            by_type.insert(KfxSymbol::ExternalResource as u64, m);
            by_type
        };
        // jpg → accepted; pdf → rejected (guards PDF-backed first sections).
        assert!(cover_candidate_is_image(&resource(1), &symbols, "cover"));
        assert!(!cover_candidate_is_image(&resource(2), &symbols, "cover"));
        // unknown name → rejected.
        assert!(!cover_candidate_is_image(&resource(1), &symbols, "other"));
    }
}
