//! KFX → `BookData` loader.
//!
//! Mirrors calibre's `KFX_EPUB.organize_fragments_by_type`: parses the
//! container, resolves every fragment's name from the symbol table, and
//! groups them by ftype symbol id. For `bcRawMedia` ($417) the value
//! stored is the raw payload bytes; for everything else it's the parsed
//! `IonValue`.
//!
//! Loading is eager. KFX containers we care about are <100 MB and have
//! a few hundred entities, so the simplicity wins over laziness.

use std::collections::HashMap;

use crate::kfx::container::{
    self, EntityLoc, extract_doc_symbols, get_field, parse_container_header,
    parse_container_info, parse_index_table, skip_enty_header,
};
use crate::kfx::ion::{IonParser, IonValue};
use crate::kfx::symbols::{KFX_SYMBOL_TABLE, KfxSymbol};

use super::ConvertError;

/// Book-wide metadata pulled out of `book_metadata` ($490).
///
/// This is the parsed shape we hand to the output stage. It's intentionally
/// minimal; a fuller metadata port can expand it later.
#[derive(Debug, Clone, Default)]
pub struct BookMetadata {
    pub title: String,
    pub authors: Vec<String>,
    pub language: String,
    pub identifier: String,
    pub publisher: Option<String>,
    /// Cover image's `resource_name` (e.g. `"eF"`), if declared by KFX. The
    /// output stage resolves this to the actual file path during cover wiring.
    pub cover_resource_name: Option<String>,
    /// `kindle_title_metadata/ASIN` — Amazon catalogue id. Calibre emits this
    /// as `<dc:identifier opf:scheme="ASIN">B0CPJ2B88T</dc:identifier>` and
    /// (redundantly) also as `opf:scheme="MOBI-ASIN"`.
    pub asin: Option<String>,
    /// `kindle_title_metadata/issue_date` — publication date string. KFX
    /// stores it as `YYYY-MM-DD`; calibre normalises to ISO-8601 with a UTC
    /// offset. Surface raw and let the OPF emitter format it.
    pub issue_date: Option<String>,
    /// `kindle_title_metadata/title_pronunciation` — title sort key
    /// (yomigana for Japanese books). Surfaces in OPF as
    /// `<dc:title opf:file-as="...">`.
    pub title_pronunciation: Option<String>,
    /// `kindle_title_metadata/author_pronunciation` — author sort key.
    /// Surfaces in OPF as `<dc:creator opf:file-as="...">`.
    pub author_pronunciation: Option<String>,
}

/// Everything we need from a KFX container to drive an EPUB write.
///
/// Mirrors calibre's `self.book_data` dict plus a few derived fields.
pub struct BookData {
    /// All entities indexed by `(ftype_symbol_id, fid_string)`.
    ///
    /// `ftype_symbol_id` is the KFX type id (e.g. 164 for `external_resource`).
    /// `fid_string` is the resolved name from the symbol table (e.g.
    /// `"content_30"` for an external_resource named `content_30`).
    ///
    /// `bcRawMedia` ($417) is intentionally NOT in here — see `raw_media`.
    pub by_type: HashMap<u64, HashMap<String, IonValue>>,

    /// `bcRawMedia` payloads, keyed by entity name (e.g. `"resource/rsrc562"`).
    ///
    /// Stored separately from `by_type` because the value is raw bytes (often
    /// large), not a parsed Ion struct.
    pub raw_media: HashMap<String, Vec<u8>>,

    /// Resolved extended symbol table (KFX base + container doc_symbols).
    ///
    /// Indexed by full symbol id; for ids < `KFX_SYMBOL_TABLE.len()` it
    /// returns the base symbol, otherwise the doc_symbol at the offset.
    pub symbols: SymbolTable,

    /// Book metadata derived from `book_metadata` ($490).
    pub metadata: BookMetadata,
}

/// Resolved symbol table — base + per-container doc_symbols.
///
/// `base_len` must match calibre's `LocalSymbolTable.local_min_id - 1`, i.e.
/// the count of imported (system + shared) symbols. Calibre's value is 839
/// (9 system + 830 YJ_SYMBOLS), so doc_symbols start at id 840. Our static
/// `KFX_SYMBOL_TABLE` has 852 entries — 13 trailing additions Amazon shipped
/// after calibre's YJ_SYMBOLS table was last updated — but for reading
/// Amazon-produced KFX containers we must follow calibre's offset, otherwise
/// doc_symbols are looked up 12 positions off and every entity id resolves
/// to the wrong name.
pub struct SymbolTable {
    base_len: u64,
    doc_symbols: Vec<String>,
}

/// Fallback base when the doc_symbol fragment doesn't declare imports.
/// Real KFX files always declare YJ_symbols and we read max_id from there.
const FALLBACK_BASE_LEN: u64 = 833;

impl SymbolTable {
    /// Resolve a symbol id to its text. Returns `"?"` for out-of-range ids.
    ///
    /// For ids below `base_len` we look up the static KFX table (which matches
    /// calibre's system + YJ_SYMBOLS for ids 0..839); above, we index into
    /// the per-container doc_symbols.
    pub fn resolve(&self, id: u64) -> &str {
        if id < self.base_len {
            KFX_SYMBOL_TABLE.get(id as usize).copied().unwrap_or("?")
        } else {
            let idx = (id - self.base_len) as usize;
            self.doc_symbols.get(idx).map(String::as_str).unwrap_or("?")
        }
    }

    /// Resolve a value that may be a Symbol or a String to its text.
    pub fn text_of<'a>(&'a self, v: &'a IonValue) -> Option<&'a str> {
        match v {
            IonValue::Symbol(id) => Some(self.resolve(*id)),
            IonValue::String(s) => Some(s),
            _ => None,
        }
    }
}

/// Load a KFX container in memory into `BookData`.
pub fn load(kfx_bytes: &[u8]) -> Result<BookData, ConvertError> {
    let header = parse_container_header(kfx_bytes)
        .map_err(|e| ConvertError::InvalidKfx(e.to_string()))?;

    if header.container_info_offset + header.container_info_length > kfx_bytes.len() {
        return Err(ConvertError::InvalidKfx(
            "container info out of bounds".into(),
        ));
    }
    let info_bytes = &kfx_bytes[header.container_info_offset
        ..header.container_info_offset + header.container_info_length];
    let info = parse_container_info(info_bytes)
        .map_err(|e| ConvertError::InvalidKfx(e.to_string()))?;

    let (doc_symbols, base_len) = match info.doc_symbols {
        Some((off, len)) if off + len <= kfx_bytes.len() => {
            let doc_bytes = &kfx_bytes[off..off + len];
            let base = parse_imports_max_id(doc_bytes).map(|m| m + 1).unwrap_or(FALLBACK_BASE_LEN);
            (extract_doc_symbols(doc_bytes), base)
        }
        _ => (Vec::new(), FALLBACK_BASE_LEN),
    };
    let symbols = SymbolTable {
        base_len,
        doc_symbols,
    };

    let (idx_off, idx_len) = info
        .index
        .ok_or_else(|| ConvertError::InvalidKfx("missing index table".into()))?;
    if idx_off + idx_len > kfx_bytes.len() {
        return Err(ConvertError::InvalidKfx("index out of bounds".into()));
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

        // bcRawMedia ($417): payload is raw image/font bytes, not Ion.
        // Calibre keys this by `location` field of the corresponding
        // external_resource, which is the resolved symbol name of the
        // entity id (e.g. "resource/rsrc562"). We mirror that here.
        if ent.type_id == KfxSymbol::Bcrawmedia as u32 {
            let key = symbols.resolve(ent.id as u64).to_string();
            if !key.is_empty() && key != "?" {
                raw_media.insert(key, payload.to_vec());
            }
            continue;
        }

        // bcRawFont ($418): also raw bytes; treat the same as bcRawMedia
        // since calibre's process_fonts pulls from book_data["$418"] as bytes.
        // Kept for the (still-deferred) font handling.
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

/// Pick the fragment id for an entity. KFX entities carry their name via the
/// symbol-table-resolvable `id` field on the index entry; some types nest
/// the name inside the payload (`resource_name`, `style_name`, etc.) for
/// redundancy. We prefer the entry-level id because it's always present.
fn resolve_fid(ent: &EntityLoc, _value: &IonValue, symbols: &SymbolTable) -> String {
    let name = symbols.resolve(ent.id as u64);
    if name.is_empty() || name == "?" {
        // Fall back to an opaque identifier so collisions still distinguish.
        format!("#entity_{}", ent.id)
    } else {
        name.to_string()
    }
}

/// Walk `book_metadata` ($490) to fill the `BookMetadata` struct.
///
/// KFX has at least two metadata shapes:
/// - Amazon's own KFX wraps as `book_metadata::{ categorised_metadata: [{
///   category: kindle_title_metadata, metadata: [{key, value}, ...] }] }`.
/// - boko's own KFX exporter emits a plain struct (no annotation).
///
/// We accept both. Cover image is `cover_image` or first occurrence of a
/// `Value` that names an external_resource.
fn extract_book_metadata(
    by_type: &HashMap<u64, HashMap<String, IonValue>>,
    symbols: &SymbolTable,
) -> BookMetadata {
    let mut meta = BookMetadata::default();

    let Some(entries) = by_type.get(&(KfxSymbol::BookMetadata as u64)) else {
        return meta;
    };
    let Some((_, raw)) = entries.iter().next() else {
        return meta;
    };
    let inner = raw.unwrap_annotated();
    let Some(fields) = inner.as_struct() else {
        return meta;
    };

    let Some(cat_list) =
        get_field(fields, KfxSymbol::CategorisedMetadata as u64).and_then(|v| v.as_list())
    else {
        return meta;
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
                "title"
                    if meta.title.is_empty() => {
                        meta.title = value.into();
                    }
                // Authors are stored in source order in
                // `kindle_title_metadata/author` entries. Calibre's library
                // pathway (`yj_metadata.py:get_yj_metadata_from_book`) uses
                // `authors.append(val)`, preserving source order in the OPF —
                // that's the order in `horror.calibre.epub`. The other
                // calibre code path in `yj_to_epub_metadata.py:192` uses
                // `insert(0)` for the intermediate EPUB stage, but that
                // intermediate is discarded by calibre's library importer.
                // We match the library output, which the user reads.
                "author"
                    if !value.is_empty() => {
                        meta.authors.push(value.into());
                    }
                "publisher" => meta.publisher = Some(value.trim().into()),
                "language" => meta.language = value.into(),
                "book_id" => meta.identifier = value.into(),
                "ASIN"
                    if meta.asin.is_none() && !value.is_empty() => {
                        meta.asin = Some(value.into());
                    }
                "issue_date"
                    if meta.issue_date.is_none() && !value.is_empty() => {
                        meta.issue_date = Some(value.into());
                    }
                "cover_image" => {
                    if let Some(name) = resolve_cover_value(value_raw, symbols) {
                        meta.cover_resource_name = Some(name);
                    }
                }
                "title_pronunciation"
                    if !value.is_empty() => {
                        meta.title_pronunciation = Some(value.into());
                    }
                // KFX emits one `author_pronunciation` per `author` in source
                // order. `import::kfx` keeps the last value; mirror that so the
                // OPF `opf:file-as` matches what `boko info` reports.
                "author_pronunciation"
                    if !value.is_empty() => {
                        meta.author_pronunciation = Some(value.into());
                    }
                _ => {}
            }
        }
    }

    meta
}

/// `cover_image` can be encoded as a plain string or as a list whose first
/// element is a symbol/string pointing at an external_resource.
fn resolve_cover_value(value: Option<&IonValue>, symbols: &SymbolTable) -> Option<String> {
    let v = value?;
    if let Some(s) = v.as_string() {
        return Some(s.to_string());
    }
    if let Some(list) = v.as_list()
        && let Some(first) = list.first()
        && let Some(text) = symbols.text_of(first)
    {
        return Some(text.to_string());
    }
    None
}

/// Walk the doc_symbol Ion fragment to find the sum of imports' max_ids.
/// In Amazon's KFX format the imports section is typically
/// `[{name: "YJ_symbols", version: 10, max_id: N}]`, and local symbols
/// occupy ids `N+1..`. We add 1 so the caller can use the return value as
/// the base-id for the first local symbol.
///
/// Returns `None` if the fragment doesn't parse or doesn't declare imports
/// — the caller falls back to a known-good constant.
fn parse_imports_max_id(doc_bytes: &[u8]) -> Option<u64> {
    let mut parser = IonParser::new(doc_bytes);
    let value = parser.parse().ok()?;
    let inner = value.unwrap_annotated();
    let fields = inner.as_struct()?;

    // KFX symbol-table struct field ids: 4=name, 5=version, 6=imports,
    // 7=symbols, 8=max_id. We want imports → list of structs → field 8.
    let imports_field = fields
        .iter()
        .find(|(k, _)| *k == 6)
        .map(|(_, v)| v)?;
    let imports = imports_field.as_list()?;

    let mut total: u64 = 0;
    for entry in imports {
        if let Some(entry_fields) = entry.as_struct()
            && let Some(max_id_val) = entry_fields.iter().find(|(k, _)| *k == 8).map(|(_, v)| v)
            && let Some(n) = max_id_val.as_int()
        {
            total += n as u64;
        }
    }
    Some(total)
}

/// Re-export for `container::resolve_symbol`-style call sites that don't
/// hold a `SymbolTable`. Mirrors `kfx::container::resolve_symbol` for
/// `Vec<String>`-shaped doc_symbols.
#[allow(dead_code)]
pub fn resolve_in_table(id: u64, base_len: u64, doc_symbols: &[String]) -> Option<&str> {
    if id < base_len {
        KFX_SYMBOL_TABLE.get(id as usize).copied()
    } else {
        doc_symbols.get((id - base_len) as usize).map(String::as_str)
    }
}

#[allow(unused_imports)]
use container as _container; // silence unused warning when modules grow

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Smoke test against the gitignored `books/kfx2epub/horror.boko.kfx`
    /// fixture. Skips on machines / CI without the corpus.
    #[test]
    fn load_horror_extracts_metadata_and_raw_media() {
        let candidates = [
            "../books/kfx2epub/horror.boko.kfx",
            "books/kfx2epub/horror.boko.kfx",
        ];
        let Some(path) = candidates.iter().find(|p| Path::new(p).exists()) else {
            eprintln!("Skipping: horror.boko.kfx not present under ../books/kfx2epub/");
            return;
        };
        let bytes = std::fs::read(path).expect("read fixture");
        let book = load(&bytes).expect("load horror");
        assert_eq!(book.raw_media.len(), 14, "14 bcRawMedia in horror");
        assert!(!book.metadata.title.is_empty(), "title was extracted");
        assert!(
            !book.metadata.authors.is_empty(),
            "at least one author extracted"
        );
        assert!(
            book.metadata.cover_resource_name.is_some(),
            "cover_resource_name extracted from book_metadata"
        );
    }

    /// Use the committed `tests/fixtures/epictetus.kfx` (English book) for a
    /// minimum-viable load check that always runs.
    #[test]
    fn load_epictetus_kfx_smoke() {
        let path = "tests/fixtures/epictetus.kfx";
        if !Path::new(path).exists() {
            // boko-kai might be built from the workspace root; try the
            // crate-relative path as a fallback before skipping.
            let alt = "boko-kai/tests/fixtures/epictetus.kfx";
            if !Path::new(alt).exists() {
                eprintln!("Skipping: epictetus.kfx fixture missing");
                return;
            }
            let bytes = std::fs::read(alt).expect("read fixture");
            let _ = load(&bytes).expect("load epictetus");
            return;
        }
        let bytes = std::fs::read(path).expect("read fixture");
        let _ = load(&bytes).expect("load epictetus");
    }
}
