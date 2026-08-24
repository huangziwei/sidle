//! KFX container deserialize + serialize.
//!
//! A KFX container looks like:
//!
//! ```text
//! offset 0   : "CONT" + version(u16 LE) + header_len(u32 LE)
//!              + container_info_offset(u32 LE) + container_info_length(u32 LE)
//! offset 18..: entity table (24 B per row: id u32 + type u32 + offset u64 + len u64)
//!              + doc_symbols Ion blob (annotated $ion_symbol_table)
//!              + format_capabilities Ion blob (annotated $593)
//!              + container_info Ion blob (an IonStruct of bcContId/bcChunkSize/etc.)
//!              + kfxgen_info bytes (Ion-textish key/value list)
//! offset header_len..: concatenated entity payloads
//! ```
//!
//! Each entity payload starts with `"ENTY"` + version u16 + header_len u32
//! + entity_info Ion blob (bcComprType+bcDrmScheme) + body bytes.

use std::io;

use super::fragment::{CONTAINER_FRAGMENT_TYPES, YJFragment, is_raw};
use super::node::{ION_BVM, IonNode, parse_single_value, serialize_single_value, serialize_value};
use super::symtab::{LocalSymbolTable, SYSTEM_SIZE, SymbolTableImport};
use crate::formats::kfx::container::{GeneratorTrailer, clamp_usize, slice_at};

const CONT_SIGNATURE: &[u8] = b"CONT";
const ENTY_SIGNATURE: &[u8] = b"ENTY";
const CONTAINER_VERSION: u16 = 2;
const ENTITY_VERSION: u16 = 1;
const DEFAULT_COMPRESSION_TYPE: i64 = 0;
const DEFAULT_DRM_SCHEME: i64 = 0;
const DEFAULT_CHUNK_SIZE: i64 = 4096;
const MIN_CONTAINER_LEN: usize = 18;
const MIN_ENTITY_LEN: usize = 10;
const KFX_CONTAINER_FORMAT_MAIN: &str = "KFX main";

/// Raw entity-table row: indices into the symbol table at deserialize time
/// (their string form is resolved at parse time).
#[derive(Clone)]
struct EntityRow {
    id_idnum: u32,
    type_idnum: u32,
    /// Absolute byte offset of the entity payload within the container.
    offset: usize,
    length: usize,
}

/// State carried out of [`deserialize_container_phase1`] into the fragment
/// build, which runs on a fully populated shared symtab.
pub struct LoadedContainer {
    pub container_id: String,
    pub container_format: String,
    pub kfxgen_application_version: String,
    pub kfxgen_package_version: String,
    pub version: i64,
    pub doc_symbols: Option<IonNode>,
    pub format_capabilities: Option<IonNode>,
    /// Raw container bytes (kept for phase 2 entity payload reads).
    pub data: Vec<u8>,
    entities: Vec<EntityRow>,
}

/// The error a container section whose range falls outside the file yields.
fn out_of_range(section: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{section} offset out of range"),
    )
}

/// Phase 1: read header, container_info, doc_symbols, format_capabilities,
/// entity-table rows. Mutates `symtab`.
pub fn deserialize_container_phase1(
    data: Vec<u8>,
    symtab: &mut LocalSymbolTable,
) -> io::Result<LoadedContainer> {
    if data.len() < MIN_CONTAINER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "container too short",
        ));
    }
    if &data[0..4] != CONT_SIGNATURE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad container signature",
        ));
    }
    let version = u16::from_le_bytes([data[4], data[5]]);
    let header_len = u32_le(&data, 6) as usize;
    let ci_off = u32_le(&data, 10) as usize;
    let ci_len = u32_le(&data, 14) as usize;
    if header_len > data.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "container offsets out of range",
        ));
    }

    // container_info parses against the live shared `symtab`.
    let ci_bytes = slice_at(&data, ci_off, ci_len).ok_or_else(|| out_of_range("container_info"))?;
    let mut container_info = parse_single_value(ci_bytes, symtab)?;
    let ci_fields = container_info.as_struct_mut().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "container_info is not a struct")
    })?;

    let container_id = pop_string(ci_fields, "$409").unwrap_or_default();
    let _compression_type = pop_int(ci_fields, "$410").unwrap_or(DEFAULT_COMPRESSION_TYPE);
    // Serialization declares `DEFAULT_DRM_SCHEME` on the merged container,
    // over ciphertext payloads for an encrypted member.
    let drm_scheme = pop_int(ci_fields, "$411").unwrap_or(DEFAULT_DRM_SCHEME);
    if drm_scheme != DEFAULT_DRM_SCHEME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("container is encrypted (bcDRMScheme {drm_scheme})"),
        ));
    }

    let doc_symbol_offset = pop_int(ci_fields, "$415");
    let doc_symbol_length = pop_int(ci_fields, "$416").unwrap_or(0);
    let mut doc_symbols_node: Option<IonNode> = None;
    if doc_symbol_length > 0
        && let Some(off) = doc_symbol_offset
    {
        let ds_bytes = slice_at(&data, off as usize, doc_symbol_length as usize)
            .ok_or_else(|| out_of_range("doc_symbols"))?;
        let parsed = parse_single_value(ds_bytes, symtab)?;

        // Subtract SYSTEM size from each import.max_id, then build the symtab
        // from `doc_symbols.value`.
        let inner = if let IonNode::Annotated(_, inner) = &parsed {
            inner.as_ref().clone()
        } else {
            parsed.clone()
        };
        let (imports, symbols) = extract_imports_and_symbols(&inner);
        let adjusted_imports: Vec<SymbolTableImport> = imports
            .into_iter()
            .map(|mut imp| {
                imp.max_id = imp.max_id.saturating_sub(SYSTEM_SIZE);
                imp
            })
            .collect();
        symtab.create(&adjusted_imports, &symbols);
        doc_symbols_node = Some(parsed);
    }

    let _chunk_size = pop_int(ci_fields, "$412").unwrap_or(DEFAULT_CHUNK_SIZE);

    // format_capabilities (only when version > 1)
    let mut fc_node: Option<IonNode> = None;
    if version > 1 {
        let fc_off = pop_int(ci_fields, "$594");
        let fc_len = pop_int(ci_fields, "$595").unwrap_or(0);
        if fc_len > 0
            && let Some(off) = fc_off
        {
            let fc_bytes = slice_at(&data, off as usize, fc_len as usize)
                .ok_or_else(|| out_of_range("format_capabilities"))?;
            fc_node = Some(parse_single_value(fc_bytes, symtab)?);
        }
    }

    // entity index table
    let idx_off = pop_int(ci_fields, "$413").map(|n| n as usize);
    let idx_len = pop_int(ci_fields, "$414").map(|n| n as usize).unwrap_or(0);
    let mut entities = Vec::new();
    if let Some(off) = idx_off
        && idx_len > 0
    {
        let table = slice_at(&data, off, idx_len).ok_or_else(|| out_of_range("entity table"))?;
        const ROW: usize = 24;
        let n = idx_len / ROW;
        for i in 0..n {
            let base = i * ROW;
            let id = u32_le(table, base);
            let typ = u32_le(table, base + 4);
            let abs_off = header_len.saturating_add(clamp_usize(u64_le(table, base + 8)));
            let len64 = clamp_usize(u64_le(table, base + 16));
            if slice_at(&data, abs_off, len64).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "entity body overflows container",
                ));
            }
            entities.push(EntityRow {
                id_idnum: id,
                type_idnum: typ,
                offset: abs_off,
                length: len64,
            });
        }
    }

    let kfxgen_start = ci_off.saturating_add(ci_len);
    let kfxgen_info_bytes = data
        .get(kfxgen_start..header_len)
        .ok_or_else(|| out_of_range("kfxgen_info"))?;
    let trailer = GeneratorTrailer::parse(&String::from_utf8_lossy(kfxgen_info_bytes));

    Ok(LoadedContainer {
        container_id,
        container_format: KFX_CONTAINER_FORMAT_MAIN.to_string(),
        kfxgen_application_version: trailer.application_version,
        kfxgen_package_version: trailer.package_version,
        version: version as i64,
        doc_symbols: doc_symbols_node,
        format_capabilities: fc_node,
        data,
        entities,
    })
}

/// Phase 2, over a fully populated shared symtab: turn the entity rows + their
/// raw payload bytes into `YJFragment`s and prepend the synthetic `$270`
/// (container_info), `$ion_symbol_table`, `$593` fragments.
pub fn loaded_container_into_fragments(
    loaded: &LoadedContainer,
    symtab: &LocalSymbolTable,
) -> io::Result<Vec<YJFragment>> {
    let mut out = Vec::new();

    if let Some(ds) = &loaded.doc_symbols {
        // doc_symbols carries its annotation; the YJFragment holds
        // ftype="$ion_symbol_table", fid="$ion_symbol_table" and the
        // unannotated struct.
        let inner = match ds {
            IonNode::Annotated(_, inner) => (**inner).clone(),
            other => other.clone(),
        };
        out.push(YJFragment::singleton("$ion_symbol_table", inner));
    }

    // Synthetic $270 container_info fragment.
    let mut entries: Vec<IonNode> = Vec::with_capacity(loaded.entities.len());
    for e in &loaded.entities {
        entries.push(IonNode::List(vec![
            IonNode::Int(e.type_idnum as i64),
            IonNode::Int(e.id_idnum as i64),
        ]));
    }
    let cinfo = IonNode::Struct(vec![
        ("$409".into(), IonNode::String(loaded.container_id.clone())),
        ("$412".into(), IonNode::Int(DEFAULT_CHUNK_SIZE)),
        ("$410".into(), IonNode::Int(DEFAULT_COMPRESSION_TYPE)),
        ("$411".into(), IonNode::Int(DEFAULT_DRM_SCHEME)),
        (
            "$587".into(),
            IonNode::String(loaded.kfxgen_application_version.clone()),
        ),
        (
            "$588".into(),
            IonNode::String(loaded.kfxgen_package_version.clone()),
        ),
        (
            "$161".into(),
            IonNode::String(loaded.container_format.clone()),
        ),
        ("version".into(), IonNode::Int(loaded.version)),
        ("$181".into(), IonNode::List(entries)),
    ]);
    out.push(YJFragment::singleton("$270", cinfo));

    if let Some(fc) = &loaded.format_capabilities {
        let inner = match fc {
            IonNode::Annotated(_, inner) => (**inner).clone(),
            other => other.clone(),
        };
        out.push(YJFragment::singleton("$593", inner));
    }

    for e in &loaded.entities {
        let body_bytes = slice_at(&loaded.data, e.offset, e.length)
            .ok_or_else(|| out_of_range("entity body"))?;
        let fragment = entity_to_fragment(e, body_bytes, symtab)?;
        out.push(fragment);
    }

    Ok(out)
}

fn entity_to_fragment(
    row: &EntityRow,
    body_bytes: &[u8],
    symtab: &LocalSymbolTable,
) -> io::Result<YJFragment> {
    if body_bytes.len() < MIN_ENTITY_LEN || &body_bytes[..4] != ENTY_SIGNATURE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad entity signature",
        ));
    }
    let header_len = u32_le(body_bytes, 6) as usize;
    if header_len > body_bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "entity header overflows",
        ));
    }
    let _info_bytes = &body_bytes[10..header_len];
    // The entity info holds bcComprType / bcDrmScheme; both take defaults.
    let payload = &body_bytes[header_len..];

    let fid_initial = symtab.get_symbol(row.id_idnum);
    let ftype = symtab.get_symbol(row.type_idnum);

    let mut value = if is_raw(&ftype) {
        IonNode::Blob(payload.to_vec())
    } else {
        parse_single_value(payload, symtab)?
    };

    // Two-step normalization for `id_idnum == $348`: unwrap a body wrapped
    // `ftype::{...}`, and set `fid := ftype` for the singleton.
    let mut fid = fid_initial.clone();
    if fid_initial == "$348" {
        if let IonNode::Annotated(anns, inner) = &value
            && anns.len() == 1
            && anns[0] == ftype
        {
            let inner_owned = (**inner).clone();
            value = inner_owned;
        }
        fid = ftype.clone();
    }

    Ok(YJFragment { fid, ftype, value })
}

// =========================================================================
// Serialization
// =========================================================================

/// Build the final KFX container bytes from a fragment list + symtab.
pub fn serialize_container(fragments: &[YJFragment], symtab: &LocalSymbolTable) -> Vec<u8> {
    // `PREFERED_FRAGMENT_TYPE_ORDER` sorts the fragment list, putting
    // `$ion_symbol_table` at index 0 and `$270`/`$593` near the top.
    let mut container_id = String::new();
    let mut kfxgen_app_version = String::new();
    let mut kfxgen_pkg_version = String::new();
    let mut doc_symbols_fragment: Option<&YJFragment> = None;
    let mut format_caps_fragment: Option<&YJFragment> = None;

    for f in fragments {
        match f.ftype.as_str() {
            "$270" => {
                if let Some(s) = f.value.get_field("$409").and_then(|v| v.as_string()) {
                    container_id = s.to_string();
                }
                if let Some(s) = f.value.get_field("$587").and_then(|v| v.as_string()) {
                    kfxgen_app_version = s.to_string();
                }
                if let Some(s) = f.value.get_field("$588").and_then(|v| v.as_string()) {
                    kfxgen_pkg_version = s.to_string();
                }
            }
            "$593" => format_caps_fragment = Some(f),
            "$ion_symbol_table" => doc_symbols_fragment = Some(f),
            _ => {}
        }
    }

    // Entity payloads: every non-container fragment plus `$419`
    // (container_entity_map).
    let mut entity_data = Vec::new();
    let mut entity_table = Vec::new();
    let mut entity_offset: u64 = 0;

    for f in fragments {
        let in_container_set = CONTAINER_FRAGMENT_TYPES.contains(&f.ftype.as_str());
        if in_container_set && f.ftype != "$419" {
            continue;
        }
        let id_idnum = if f.is_single() {
            symtab.get_id("$348")
        } else {
            symtab.get_id(&f.fid)
        };
        let type_idnum = symtab.get_id(&f.ftype);
        let body = if is_raw(&f.ftype) {
            match &f.value {
                IonNode::Blob(b) => b.clone(),
                _ => Vec::new(),
            }
        } else {
            // Body is the bare value; type information lives in entity_table.
            serialize_single_value(&f.value, symtab)
        };
        let ent_bytes = build_entity_bytes(&body, symtab);
        let ent_len = ent_bytes.len() as u64;
        entity_table.extend_from_slice(&id_idnum.to_le_bytes());
        entity_table.extend_from_slice(&type_idnum.to_le_bytes());
        entity_table.extend_from_slice(&entity_offset.to_le_bytes());
        entity_table.extend_from_slice(&ent_len.to_le_bytes());
        entity_offset += ent_len;
        entity_data.extend_from_slice(&ent_bytes);
    }

    // Buffer for the pre-body section.
    let mut container: Vec<u8> = Vec::with_capacity(header_capacity_hint());
    container.extend_from_slice(CONT_SIGNATURE);
    container.extend_from_slice(&CONTAINER_VERSION.to_le_bytes());
    // header_len, ci_offset, ci_length placeholders (rewritten at the end).
    let header_len_pack = container.len();
    container.extend_from_slice(&[0u8; 4]);
    let ci_off_pack = container.len();
    container.extend_from_slice(&[0u8; 4]);
    let ci_len_pack = container.len();
    container.extend_from_slice(&[0u8; 4]);

    // container_info field order: $409 bcContId, $410 bcComprType, $411
    // bcDRMScheme, $413/$414 entity table, $415/$416 doc_symbols, $412
    // bcChunkSize, $594/$595 format_capabilities (only with symtab locals).
    let mut ci_fields: Vec<(String, IonNode)> = vec![
        ("$409".into(), IonNode::String(container_id.clone())),
        ("$410".into(), IonNode::Int(DEFAULT_COMPRESSION_TYPE)),
        ("$411".into(), IonNode::Int(DEFAULT_DRM_SCHEME)),
        ("$413".into(), IonNode::Int(container.len() as i64)),
        ("$414".into(), IonNode::Int(entity_table.len() as i64)),
    ];
    container.extend_from_slice(&entity_table);

    // doc_symbols: deep-copy the fragment, bump every import's max_id by
    // SYSTEM_SIZE, serialize the annotated value.
    let doc_symbol_data = if let Some(frag) = doc_symbols_fragment {
        let inner = bump_imports_max_id(&frag.value, SYSTEM_SIZE as i64);
        let annotated = IonNode::Annotated(vec!["$ion_symbol_table".into()], Box::new(inner));
        serialize_single_value(&annotated, symtab)
    } else {
        Vec::new()
    };
    ci_fields.push(("$415".into(), IonNode::Int(container.len() as i64)));
    ci_fields.push(("$416".into(), IonNode::Int(doc_symbol_data.len() as i64)));
    container.extend_from_slice(&doc_symbol_data);

    ci_fields.push(("$412".into(), IonNode::Int(DEFAULT_CHUNK_SIZE)));

    let fc_data = if let Some(fc_frag) = format_caps_fragment {
        let annotated = IonNode::Annotated(vec!["$593".into()], Box::new(fc_frag.value.clone()));
        serialize_single_value(&annotated, symtab)
    } else {
        Vec::new()
    };
    // format_capabilities pointers ride on `symtab.local_min_id > 595`, the
    // window where $593 is a known symbol.
    if symtab.local_min_id() > 595 && !fc_data.is_empty() {
        ci_fields.push(("$594".into(), IonNode::Int(container.len() as i64)));
        ci_fields.push(("$595".into(), IonNode::Int(fc_data.len() as i64)));
        container.extend_from_slice(&fc_data);
    }

    let ci_bytes = serialize_single_value(&IonNode::Struct(ci_fields), symtab);
    // Patch ci_length; ci_offset = current container length.
    let ci_offset_value = container.len() as u32;
    patch_u32_le(&mut container, ci_off_pack, ci_offset_value);
    patch_u32_le(&mut container, ci_len_pack, ci_bytes.len() as u32);
    container.extend_from_slice(&ci_bytes);

    // kfxgen_info trailer: a compact JSON array of {"key":…, "value":…}
    // objects, with "key" and "value" unquoted on the wire.
    let payload_sha1 = sha1_smol::Sha1::from(&entity_data).hexdigest();
    let kfxgen_info = format!(
        r#"[{{key:"kfxgen_package_version",value:"{}"}},{{key:"kfxgen_application_version",value:"{}"}},{{key:"kfxgen_payload_sha1",value:"{}"}},{{key:"kfxgen_acr",value:"{}"}}]"#,
        escape_json_string(&kfxgen_pkg_version),
        escape_json_string(&kfxgen_app_version),
        payload_sha1,
        escape_json_string(&container_id),
    );
    container.extend_from_slice(kfxgen_info.as_bytes());

    // Patch header_len: the container length before the entity payloads.
    let header_len_value = container.len() as u32;
    patch_u32_le(&mut container, header_len_pack, header_len_value);

    container.extend_from_slice(&entity_data);
    container
}

fn build_entity_bytes(body: &[u8], symtab: &LocalSymbolTable) -> Vec<u8> {
    // entity_info = struct{ $410: 0, $411: 0 }
    let info_node = IonNode::Struct(vec![
        ("$410".into(), IonNode::Int(DEFAULT_COMPRESSION_TYPE)),
        ("$411".into(), IonNode::Int(DEFAULT_DRM_SCHEME)),
    ]);
    let mut info_bytes = Vec::from(ION_BVM);
    info_bytes.extend_from_slice(&serialize_value(&info_node, symtab));

    let header_len = (10 + info_bytes.len()) as u32;
    let mut out = Vec::with_capacity(header_len as usize + body.len());
    out.extend_from_slice(ENTY_SIGNATURE);
    out.extend_from_slice(&ENTITY_VERSION.to_le_bytes());
    out.extend_from_slice(&header_len.to_le_bytes());
    out.extend_from_slice(&info_bytes);
    out.extend_from_slice(body);
    out
}

fn extract_imports_and_symbols(value: &IonNode) -> (Vec<SymbolTableImport>, Vec<String>) {
    let mut imports = Vec::new();
    let mut symbols = Vec::new();
    let Some(fields) = value.as_struct() else {
        return (imports, symbols);
    };
    for (k, v) in fields {
        match k.as_str() {
            "imports" => {
                if let IonNode::List(items) = v {
                    for item in items {
                        if let IonNode::Struct(f) = item {
                            let mut name = String::new();
                            let mut version: u32 = 1;
                            let mut max_id: u32 = 0;
                            for (kk, vv) in f {
                                match kk.as_str() {
                                    "name" => {
                                        if let Some(s) = vv.as_string() {
                                            name = s.to_string()
                                        }
                                    }
                                    "version" => {
                                        if let Some(n) = vv.as_int() {
                                            version = n as u32
                                        }
                                    }
                                    "max_id" => {
                                        if let Some(n) = vv.as_int() {
                                            max_id = n as u32
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            if !name.is_empty() {
                                imports.push(SymbolTableImport {
                                    name,
                                    version,
                                    max_id,
                                });
                            }
                        }
                    }
                }
            }
            "symbols" => {
                if let IonNode::List(items) = v {
                    for item in items {
                        if let Some(s) = item.as_string() {
                            symbols.push(s.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    (imports, symbols)
}

/// Deep-copy + max_id increment for the on-wire `$ion_symbol_table`.
fn bump_imports_max_id(value: &IonNode, delta: i64) -> IonNode {
    let mut out = value.clone();
    let Some(fields) = out.as_struct_mut() else {
        return out;
    };
    for (k, v) in fields.iter_mut() {
        if k == "imports"
            && let IonNode::List(items) = v
        {
            for item in items {
                if let IonNode::Struct(f) = item {
                    for (kk, vv) in f.iter_mut() {
                        if kk == "max_id"
                            && let IonNode::Int(n) = vv
                        {
                            *n += delta;
                        }
                    }
                }
            }
        }
    }
    out
}

// =========================================================================
// helpers
// =========================================================================

#[inline]
fn u32_le(d: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]])
}
#[inline]
fn u64_le(d: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        d[off],
        d[off + 1],
        d[off + 2],
        d[off + 3],
        d[off + 4],
        d[off + 5],
        d[off + 6],
        d[off + 7],
    ])
}

fn patch_u32_le(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn header_capacity_hint() -> usize {
    1024
}

fn pop_string(fields: &mut Vec<(String, IonNode)>, key: &str) -> Option<String> {
    let pos = fields.iter().position(|(k, _)| k == key)?;
    let (_, v) = fields.remove(pos);
    match v {
        IonNode::String(s) => Some(s),
        _ => None,
    }
}

fn pop_int(fields: &mut Vec<(String, IonNode)>, key: &str) -> Option<i64> {
    let pos = fields.iter().position(|(k, _)| k == key)?;
    let (_, v) = fields.remove(pos);
    match v {
        IonNode::Int(n) => Some(n),
        _ => None,
    }
}

fn escape_json_string(s: &str) -> String {
    // KFXGEN values are container IDs and version strings. A quote or
    // backslash in one takes the minimal escape below.
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out
}
