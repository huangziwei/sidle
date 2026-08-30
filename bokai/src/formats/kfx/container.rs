//! KFX container format parsing.
//!
//! Pure functions over byte slices. No I/O.

use super::ion::{IonParser, IonValue};
use super::symbols::KFX_SYMBOL_TABLE;

/// KFX container header (18 bytes).
#[derive(Debug, Clone, Copy)]
pub struct ContainerHeader {
    /// Format version of the container layer.
    pub version: u16,
    /// Header length (offset to entity data).
    pub header_len: usize,
    /// Container info offset.
    pub container_info_offset: usize,
    /// Container info length.
    pub container_info_length: usize,
}

/// Location of an entity within the container.
#[derive(Debug, Clone, Copy)]
pub struct EntityLoc {
    /// Entity ID.
    pub id: u32,
    /// Entity type ID (symbol ID).
    pub type_id: u32,
    /// Byte offset within container (after header).
    pub offset: usize,
    /// Length in bytes.
    pub length: usize,
}

/// Parsed container info fields.
#[derive(Debug, Clone, Default)]
pub struct ContainerInfo {
    /// Index table offset and length.
    pub index: Option<(usize, usize)>,
    /// Document symbols offset and length.
    pub doc_symbols: Option<(usize, usize)>,
    /// `bcDRMScheme` ($411): `0` for unencrypted entity payloads.
    pub drm_scheme: i64,
    /// `bcComprType` ($410): `0` for uncompressed entity payloads.
    pub compr_type: i64,
    /// `bcContId` ($409): `CR!` plus 28 uppercase alphanumerics.
    pub cont_id: Option<String>,
}

/// Error type for container parsing.
#[derive(Debug)]
pub enum ContainerError {
    /// Invalid magic bytes.
    InvalidMagic,
    /// Data too short.
    TooShort,
    /// Invalid Ion data.
    InvalidIon(String),
    /// Missing required field.
    MissingField(&'static str),
}

impl std::fmt::Display for ContainerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContainerError::InvalidMagic => write!(f, "Not a valid KFX container"),
            ContainerError::TooShort => write!(f, "Data too short"),
            ContainerError::InvalidIon(msg) => write!(f, "Invalid Ion data: {}", msg),
            ContainerError::MissingField(field) => write!(f, "Missing field: {}", field),
        }
    }
}

impl std::error::Error for ContainerError {}

impl From<std::io::Error> for ContainerError {
    fn from(e: std::io::Error) -> Self {
        ContainerError::InvalidIon(e.to_string())
    }
}

// --- Byte reading helpers ---

/// Read a little-endian u32 from a byte slice at the given offset.
#[inline]
pub fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

/// Read a little-endian u64 from a byte slice at the given offset.
#[inline]
pub fn read_u64_le(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ])
}

/// The `length` bytes at `offset`. `None` when the range runs past the end of
/// `data` or its end overflows `usize`.
#[inline]
pub fn slice_at(data: &[u8], offset: usize, length: usize) -> Option<&[u8]> {
    data.get(offset..offset.checked_add(length)?)
}

/// `value` as a `usize`, saturating at `usize::MAX` where the target's pointer
/// width cannot hold it.
#[inline]
pub fn clamp_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

// --- Container header parsing ---

/// Parse the KFX container header (first 18 bytes).
///
/// Returns the header structure containing offsets and lengths.
pub fn parse_container_header(data: &[u8]) -> Result<ContainerHeader, ContainerError> {
    if data.len() < 18 {
        return Err(ContainerError::TooShort);
    }

    if &data[0..4] != b"CONT" {
        return Err(ContainerError::InvalidMagic);
    }

    Ok(ContainerHeader {
        version: u16::from_le_bytes([data[4], data[5]]),
        header_len: read_u32_le(data, 6) as usize,
        container_info_offset: read_u32_le(data, 10) as usize,
        container_info_length: read_u32_le(data, 14) as usize,
    })
}

// --- Container info parsing ---

/// Parse container info to extract index table and doc symbols locations.
pub fn parse_container_info(data: &[u8]) -> Result<ContainerInfo, ContainerError> {
    let mut parser = IonParser::new(data);
    let elem = parser.parse()?;

    let fields = elem.as_struct().ok_or(ContainerError::InvalidIon(
        "Container info is not a struct".to_string(),
    ))?;

    let mut info = ContainerInfo::default();

    // Index table
    if let (Some(offset), Some(length)) = (
        get_field_int(fields, "bcIndexTabOffset"),
        get_field_int(fields, "bcIndexTabLength"),
    ) {
        info.index = Some((offset as usize, length as usize));
    }

    // Document symbols
    if let (Some(offset), Some(length)) = (
        get_field_int(fields, "bcDocSymbolOffset"),
        get_field_int(fields, "bcDocSymbolLength"),
    ) {
        info.doc_symbols = Some((offset as usize, length as usize));
    }

    info.drm_scheme = get_field_int(fields, "bcDRMScheme").unwrap_or(0);
    info.compr_type = get_field_int(fields, "bcComprType").unwrap_or(0);
    info.cont_id = get_field_str(fields, "bcContId").map(str::to_string);

    Ok(info)
}

/// Get an integer field from a struct by field name.
fn get_field_int(fields: &[(u64, IonValue)], name: &str) -> Option<i64> {
    let sym_id = symbol_id_for_name(name)?;
    fields
        .iter()
        .find(|(k, _)| *k == sym_id)
        .and_then(|(_, v)| v.as_int())
}

/// Get a string field from a struct by field name.
fn get_field_str<'a>(fields: &'a [(u64, IonValue)], name: &str) -> Option<&'a str> {
    let sym_id = symbol_id_for_name(name)?;
    fields
        .iter()
        .find(|(k, _)| *k == sym_id)
        .and_then(|(_, v)| v.as_string())
}

/// Look up a symbol ID by name from the static symbol table.
pub fn symbol_id_for_name(name: &str) -> Option<u64> {
    KFX_SYMBOL_TABLE
        .iter()
        .position(|&s| s == name)
        .map(|i| i as u64)
}

// --- Generator trailer parsing ---

/// The generator trailer (§3.5): an Ion **text** list of `key`/`value` pairs
/// naming the toolchain that produced the container.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeneratorTrailer {
    /// `kfxgen_application_version`, or the older `appVersion`.
    pub application_version: String,
    /// `kfxgen_package_version`, or the older `buildVersion`.
    pub package_version: String,
    /// `kfxgen_payload_sha1`: SHA-1 of the entity payload region, as 40 hex
    /// digits. Empty when the trailer states none.
    pub payload_sha1: String,
    /// `kfxgen_acr`: the container id, restating `bcContId`.
    pub acr: String,
}

impl GeneratorTrailer {
    /// Read the four fields out of the trailer's text.
    pub fn parse(text: &str) -> Self {
        let field = |key: &str| trailer_value(text, key).unwrap_or_default().to_string();
        Self {
            application_version: trailer_value(text, "kfxgen_application_version")
                .or_else(|| trailer_value(text, "appVersion"))
                .unwrap_or_default()
                .to_string(),
            package_version: trailer_value(text, "kfxgen_package_version")
                .or_else(|| trailer_value(text, "buildVersion"))
                .unwrap_or_default()
                .to_string(),
            payload_sha1: field("kfxgen_payload_sha1"),
            acr: field("kfxgen_acr"),
        }
    }
}

/// The bytes between the container info and the first entity payload, which
/// is where a retail container writes its [`GeneratorTrailer`]. `None` when
/// that region runs past the end of `data`.
pub fn trailer_bytes<'a>(data: &'a [u8], header: &ContainerHeader) -> Option<&'a [u8]> {
    let start = header
        .container_info_offset
        .checked_add(header.container_info_length)?;
    data.get(start..header.header_len)
}

/// The entity payload region: everything from `header_len` to the end of the
/// container, which is what `kfxgen_payload_sha1` digests.
pub fn payload_region<'a>(data: &'a [u8], header: &ContainerHeader) -> Option<&'a [u8]> {
    data.get(header.header_len..)
}

/// The value stated for `key`, out of one `key:"…",value:"…"` pair.
fn trailer_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("key:\"{key}\"");
    let after_key = &text[text.find(&pattern)? + pattern.len()..];
    let value = &after_key[after_key.find("value:\"")? + "value:\"".len()..];
    Some(&value[..value.find('"')?])
}

// --- Index table parsing ---

/// Parse the entity index table, 24 bytes per entry: id(4) + type_id(4) +
/// offset(8) + length(8). Each offset is returned with `header_len` added.
pub fn parse_index_table(data: &[u8], header_len: usize) -> Vec<EntityLoc> {
    const ENTRY_SIZE: usize = 24;
    let num_entries = data.len() / ENTRY_SIZE;
    let mut entities = Vec::with_capacity(num_entries);

    for i in 0..num_entries {
        let entry_offset = i * ENTRY_SIZE;
        if entry_offset + ENTRY_SIZE > data.len() {
            break;
        }

        entities.push(EntityLoc {
            id: read_u32_le(data, entry_offset),
            type_id: read_u32_le(data, entry_offset + 4),
            offset: header_len.saturating_add(clamp_usize(read_u64_le(data, entry_offset + 8))),
            length: clamp_usize(read_u64_le(data, entry_offset + 16)),
        });
    }

    entities
}

// --- Entity header parsing ---

/// Skip the ENTY header if present and return the payload data.
///
/// Returns the slice after the ENTY header, or the original slice if no header.
pub fn skip_enty_header(data: &[u8]) -> &[u8] {
    if data.len() >= 10 && &data[0..4] == b"ENTY" {
        let header_len = read_u32_le(data, 6) as usize;
        if header_len < data.len() {
            return &data[header_len..];
        }
    }
    data
}

/// The raw payload bytes of an entity (after its ENTY header). For media
/// entities (e.g. `bcRawMedia`) this is the stored bytes verbatim.
pub fn entity_media<'a>(data: &'a [u8], ent: &EntityLoc) -> Option<&'a [u8]> {
    Some(skip_enty_header(slice_at(data, ent.offset, ent.length)?))
}

/// Parse an entity's payload as an Ion value (for structured fragments).
/// Returns `None` if the entity is out of bounds or not valid Ion.
pub fn parse_entity(data: &[u8], ent: &EntityLoc) -> Option<IonValue> {
    let media = entity_media(data, ent)?;
    IonParser::new(media).parse().ok()
}

// --- Document symbols parsing ---

/// The doc-symbols section's local symbols, extending the base KFX table.
pub fn extract_doc_symbols(data: &[u8]) -> Vec<String> {
    // BVM + $3::{ $6: [{ $4: "YJ_symbols", $5: 10, $8: 851 }], $7: [...] }
    let mut parser = IonParser::new(data);
    let value = match parser.parse() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // Unwrap annotation ($3 = $ion_symbol_table)
    let inner = value.unwrap_annotated();

    // Get the struct fields
    let fields = match inner.as_struct() {
        Some(f) => f,
        None => return Vec::new(),
    };

    // Field $7 = "symbols" in Ion system symbols
    let symbols_list = match get_field(fields, 7) {
        Some(list) => list,
        None => return Vec::new(),
    };

    let items = match symbols_list.as_list() {
        Some(l) => l,
        None => return Vec::new(),
    };

    items
        .iter()
        .filter_map(|v| {
            if let IonValue::String(s) = v {
                Some(s.clone())
            } else {
                None
            }
        })
        .collect()
}

// --- Symbol resolution ---

/// Base for a doc_symbol fragment declaring no imports.
const FALLBACK_BASE_LEN: u64 = 833;

/// Summed import `max_id` of a doc-symbols fragment, whose `imports` list
/// carries `[{name: "YJ_symbols", version: 10, max_id: N}]`; local symbols
/// occupy ids `N+1..`. `None` when no import declares one.
pub fn parse_imports_max_id(doc_bytes: &[u8]) -> Option<u64> {
    let mut parser = IonParser::new(doc_bytes);
    let value = parser.parse().ok()?;
    let inner = value.unwrap_annotated();
    let fields = inner.as_struct()?;

    // Symbol-table field ids: 4=name, 5=version, 6=imports, 7=symbols,
    // 8=max_id. Field 6 → list of structs → field 8.
    let imports_field = fields.iter().find(|(k, _)| *k == 6).map(|(_, v)| v)?;
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

/// A doc-symbols fragment's own declared `max_id`: the highest symbol id the
/// container claims to define, imports included. `None` for a fragment that
/// does not parse, or that declares none.
pub fn parse_local_max_id(doc_bytes: &[u8]) -> Option<u64> {
    let mut parser = IonParser::new(doc_bytes);
    let value = parser.parse().ok()?;
    let fields = value.unwrap_annotated().as_struct()?;
    // Symbol-table field id 8 = max_id, at the table's own level.
    fields
        .iter()
        .find(|(k, _)| *k == 8)
        .and_then(|(_, v)| v.as_int())
        .and_then(|n| u64::try_from(n).ok())
}

/// The `name` of every import a doc-symbols fragment declares, in order.
pub fn parse_import_names(doc_bytes: &[u8]) -> Vec<String> {
    let mut parser = IonParser::new(doc_bytes);
    let Ok(value) = parser.parse() else {
        return Vec::new();
    };
    let inner = value.unwrap_annotated();
    let Some(fields) = inner.as_struct() else {
        return Vec::new();
    };
    // Symbol-table field ids: 4=name, 6=imports.
    let Some(imports) = fields
        .iter()
        .find(|(k, _)| *k == 6)
        .and_then(|(_, v)| v.as_list())
    else {
        return Vec::new();
    };
    imports
        .iter()
        .filter_map(|entry| {
            entry
                .as_struct()?
                .iter()
                .find(|(k, _)| *k == 4)
                .and_then(|(_, v)| v.as_string())
                .map(str::to_string)
        })
        .collect()
}

/// Resolved symbol table — base + per-container doc_symbols. `base_len` is the
/// doc-symbols fragment's imports `max_id` plus one.
pub struct SymbolTable {
    base_len: u64,
    doc_symbols: Vec<String>,
}

impl SymbolTable {
    /// Build from explicit parts (tests, pre-extracted symbol lists).
    pub fn new(base_len: u64, doc_symbols: Vec<String>) -> Self {
        Self {
            base_len,
            doc_symbols,
        }
    }

    /// Build from a container's doc-symbols fragment bytes (`None` when the
    /// container has no such fragment): reads the declared import max_id for
    /// the base and extracts the local symbol strings.
    pub fn from_fragment(doc_bytes: Option<&[u8]>) -> Self {
        match doc_bytes {
            Some(bytes) => Self {
                base_len: parse_imports_max_id(bytes)
                    .map(|m| m + 1)
                    .unwrap_or(FALLBACK_BASE_LEN),
                doc_symbols: extract_doc_symbols(bytes),
            },
            None => Self {
                base_len: FALLBACK_BASE_LEN,
                doc_symbols: Vec::new(),
            },
        }
    }

    /// First local symbol id (== declared import count).
    pub fn base_len(&self) -> u64 {
        self.base_len
    }

    /// A symbol id's text, `"?"` for an out-of-range id. Ids below `base_len`
    /// come from `KFX_SYMBOL_TABLE`, ids at or above it from `doc_symbols`.
    pub fn resolve(&self, id: u64) -> &str {
        self.resolve_opt(id).unwrap_or("?")
    }

    /// [`Self::resolve`] with `None` for an out-of-range id.
    pub fn resolve_opt(&self, id: u64) -> Option<&str> {
        if id < self.base_len {
            KFX_SYMBOL_TABLE.get(id as usize).copied()
        } else {
            self.doc_symbols
                .get((id - self.base_len) as usize)
                .map(String::as_str)
        }
    }

    /// Resolve a value that may be a Symbol or a String to its text.
    /// Unresolvable symbols come back as `"?"` (see [`Self::text_of_opt`]).
    pub fn text_of<'a>(&'a self, v: &'a IonValue) -> Option<&'a str> {
        match v {
            IonValue::Symbol(id) => Some(self.resolve(*id)),
            IonValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// [`Self::text_of`] with `None` for an unresolvable symbol.
    pub fn text_of_opt<'a>(&'a self, v: &'a IonValue) -> Option<&'a str> {
        match v {
            IonValue::Symbol(id) => self.resolve_opt(*id),
            IonValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// Symbol id of a doc-local symbol by its text (`base + position`), for
    /// field-id lookups of extension fields like `yj.semantics.type`.
    pub fn local_symbol_id(&self, text: &str) -> Option<u64> {
        self.doc_symbols
            .iter()
            .position(|s| s == text)
            .map(|i| self.base_len + i as u64)
    }

    /// Decompose into `(base_len, doc_symbols)`.
    pub fn into_parts(self) -> (u64, Vec<String>) {
        (self.base_len, self.doc_symbols)
    }
}

/// Get a field from a struct by symbol ID.
#[inline]
pub fn get_field(fields: &[(u64, IonValue)], symbol_id: u64) -> Option<&IonValue> {
    fields.iter().find(|(k, _)| *k == symbol_id).map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_u32_le() {
        let data = [0x01, 0x02, 0x03, 0x04];
        assert_eq!(read_u32_le(&data, 0), 0x04030201);
    }

    #[test]
    fn test_read_u64_le() {
        let data = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        assert_eq!(read_u64_le(&data, 0), 0x0807060504030201);
    }

    #[test]
    fn test_parse_container_header() {
        let mut data = vec![0u8; 18];
        data[0..4].copy_from_slice(b"CONT");
        // Skip 2 bytes (unknown)
        data[6..10].copy_from_slice(&100u32.to_le_bytes());
        // container_info_offset at offset 10
        data[10..14].copy_from_slice(&200u32.to_le_bytes());
        // container_info_length at offset 14
        data[14..18].copy_from_slice(&50u32.to_le_bytes());

        let header = parse_container_header(&data).unwrap();
        assert_eq!(header.header_len, 100);
        assert_eq!(header.container_info_offset, 200);
        assert_eq!(header.container_info_length, 50);
    }

    #[test]
    fn test_parse_container_header_invalid_magic() {
        let data = [0u8; 18];
        let result = parse_container_header(&data);
        assert!(matches!(result, Err(ContainerError::InvalidMagic)));
    }

    #[test]
    fn test_parse_container_header_too_short() {
        let data = [0u8; 10];
        let result = parse_container_header(&data);
        assert!(matches!(result, Err(ContainerError::TooShort)));
    }

    #[test]
    fn test_parse_index_table() {
        // An index table of two entries.
        let mut data = vec![0u8; 48];

        // Entry 1: id=1, type_id=100, offset=1000, length=500
        data[0..4].copy_from_slice(&1u32.to_le_bytes());
        data[4..8].copy_from_slice(&100u32.to_le_bytes());
        data[8..16].copy_from_slice(&1000u64.to_le_bytes());
        data[16..24].copy_from_slice(&500u64.to_le_bytes());

        // Entry 2: id=2, type_id=200, offset=2000, length=300
        data[24..28].copy_from_slice(&2u32.to_le_bytes());
        data[28..32].copy_from_slice(&200u32.to_le_bytes());
        data[32..40].copy_from_slice(&2000u64.to_le_bytes());
        data[40..48].copy_from_slice(&300u64.to_le_bytes());

        let entities = parse_index_table(&data, 50);

        assert_eq!(entities.len(), 2);

        assert_eq!(entities[0].id, 1);
        assert_eq!(entities[0].type_id, 100);
        assert_eq!(entities[0].offset, 50 + 1000);
        assert_eq!(entities[0].length, 500);

        assert_eq!(entities[1].id, 2);
        assert_eq!(entities[1].type_id, 200);
        assert_eq!(entities[1].offset, 50 + 2000);
        assert_eq!(entities[1].length, 300);
    }

    #[test]
    fn test_skip_enty_header() {
        // Data with ENTY header
        let mut data = vec![0u8; 20];
        data[0..4].copy_from_slice(b"ENTY");
        // header_len at offset 6
        data[6..10].copy_from_slice(&10u32.to_le_bytes());
        // Payload after header
        data[10..20].copy_from_slice(b"0123456789");

        let payload = skip_enty_header(&data);
        assert_eq!(payload, b"0123456789");
    }

    #[test]
    fn test_skip_enty_header_no_header() {
        let data = b"no enty header here";
        let payload = skip_enty_header(data);
        assert_eq!(payload, data.as_slice());
    }

    #[test]
    fn symbol_table_resolves_base_and_doc_local() {
        let table = SymbolTable::new(
            KFX_SYMBOL_TABLE.len() as u64,
            vec!["custom_symbol".to_string()],
        );
        // Symbol 10 is "language" in the base table
        assert_eq!(table.resolve_opt(10), Some("language"));
        assert_eq!(
            table.resolve_opt(KFX_SYMBOL_TABLE.len() as u64),
            Some("custom_symbol")
        );
        assert_eq!(table.resolve(u64::MAX), "?");
    }

    #[test]
    fn symbol_table_honors_declared_base_smaller_than_static_table() {
        // A smaller declared import max_id seats doc symbols below
        // `KFX_SYMBOL_TABLE.len()`.
        let base = KFX_SYMBOL_TABLE.len() as u64 - 13;
        let table = SymbolTable::new(base, vec!["jZK3Kk0dQPOTMEngNHyfig1".to_string()]);
        assert_eq!(table.resolve_opt(base), Some("jZK3Kk0dQPOTMEngNHyfig1"));
        assert_eq!(table.local_symbol_id("jZK3Kk0dQPOTMEngNHyfig1"), Some(base));
        // An id below the declared base resolves in the static table.
        assert_eq!(table.resolve_opt(10), Some("language"));
    }

    #[test]
    fn test_symbol_id_for_name() {
        assert_eq!(symbol_id_for_name("language"), Some(10));
        assert_eq!(symbol_id_for_name("nonexistent"), None);
    }

    #[test]
    fn test_extract_doc_symbols() {
        use crate::formats::kfx::ion::IonWriter;

        // A valid $ion_symbol_table: $3::{ $7: ["hello", "world"] }
        let mut writer = IonWriter::new();
        writer.write_bvm();
        let symtab = IonValue::Struct(vec![(
            7,
            IonValue::List(vec![
                IonValue::String("hello".into()),
                IonValue::String("world".into()),
            ]),
        )]);
        writer.write_annotated(&[3], &symtab);
        let data = writer.into_bytes();

        let symbols = extract_doc_symbols(&data);
        assert_eq!(symbols, vec!["hello", "world"]);
    }

    #[test]
    fn test_extract_doc_symbols_with_imports() {
        use crate::formats::kfx::ion::IonWriter;

        // An $ion_symbol_table carrying imports and symbols.
        let mut writer = IonWriter::new();
        writer.write_bvm();
        let import_entry = IonValue::Struct(vec![
            (4, IonValue::String("YJ_symbols".into())),
            (5, IonValue::Int(10)),
            (8, IonValue::Int(851)),
        ]);
        let symtab = IonValue::Struct(vec![
            (6, IonValue::List(vec![import_entry])),
            (
                7,
                IonValue::List(vec![IonValue::String("custom_sym".into())]),
            ),
        ]);
        writer.write_annotated(&[3], &symtab);
        let data = writer.into_bytes();

        let symbols = extract_doc_symbols(&data);
        assert_eq!(symbols, vec!["custom_sym"]);
    }
}
