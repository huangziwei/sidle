//! `.kfx-zip` → `.kfx` fragment-level merge.
//!
//! Amazon distributes KFX books as `.kfx-zip` bundles holding several `.kfx`
//! containers (main storyline + `metadata.kfx` + `CR!*.kfx` resources). This
//! module merges them into a single `.kfx` container by operating at the
//! fragment level — every entity is preserved verbatim, only the doc-local
//! symbol IDs get renumbered against a unified table and the container
//! packaging is rebuilt.
//!
//! Mirrors `YJ_Book.convert_to_single_kfx` from jhowell's kfxlib (see
//! `ref/calibre-kfx-input/kfxlib/yj_book.py:78`). The chapter/section/storyline
//! pipeline is intentionally bypassed, so books with section types boko-kai
//! cannot resolve through the IR (e.g. `document_regions`) still merge fine.
//!
//! ## Algorithm
//!
//! 1. Read each `.kfx` entry from the zip into a `MemorySource` and parse its
//!    container header, info, index, and doc_symbols.
//! 2. For every entity, decode the Ion payload (or keep raw bytes if it's
//!    `bcRawMedia`/`bcRawFont`).
//! 3. Build a unified doc_symbols list (de-duplicated string set, insertion
//!    order preserved) and a per-container remap from old local symbol IDs to
//!    new ones.
//! 4. Walk every IonValue tree and rewrite Symbol IDs, struct field keys, and
//!    annotation IDs against that container's remap.
//! 5. Re-serialize via `kfx::serialization::serialize_container`.
//!
//! Symbol IDs `< KFX_SYMBOL_TABLE_SIZE` (852) reference the shared YJ_symbols
//! base table and never need rewriting. Local IDs (≥ 852) are container-scoped
//! and get rewritten.
//!
//! The entity `id` (u32) in the container index table is itself a symbol ID
//! and gets the same remap treatment.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;

use crate::io::{ByteSource, MemorySource};
use crate::kfx::container::{
    extract_doc_symbols, parse_container_header, parse_container_info, parse_index_table,
    skip_enty_header,
};
use crate::kfx::ion::{IonParser, IonValue, IonWriter};
use crate::kfx::serialization::{
    SerializedEntity, create_entity_data, create_raw_media_data, generate_container_id,
    serialize_container,
};
use crate::kfx::symbols::{KFX_MAX_SYMBOL_ID, KFX_SYMBOL_TABLE_SIZE, KfxSymbol};

/// First valid local doc_symbols ID. IDs below this resolve against the
/// static YJ_symbols base table and never need remapping.
const BASE_LEN: u64 = KFX_SYMBOL_TABLE_SIZE as u64;

/// Merge a `.kfx-zip` bundle into a single `.kfx` container payload (bytes).
pub fn merge_kfx_zip(path: &Path) -> io::Result<Vec<u8>> {
    let containers = load_all_containers(path)?;
    if containers.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kfx-zip contains no .kfx entries",
        ));
    }

    let (merged_symbols, per_container_remap) = unify_symbols(&containers);
    let serialized_entities = build_serialized_entities(containers, &per_container_remap);
    let symtab_ion = build_symbol_table_ion(&merged_symbols);
    let container_id = generate_container_id();
    Ok(serialize_container(
        &container_id,
        &serialized_entities,
        &symtab_ion,
        &[],
    ))
}

// --- Loading ---

struct LoadedContainer {
    doc_symbols: Vec<String>,
    entities: Vec<LoadedEntity>,
}

struct LoadedEntity {
    id: u32,
    type_id: u32,
    payload: EntityPayload,
}

enum EntityPayload {
    Ion(IonValue),
    Raw(Vec<u8>),
}

fn load_all_containers(path: &Path) -> io::Result<Vec<LoadedContainer>> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    let mut kfx_names: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        if entry.is_file()
            && std::path::Path::new(entry.name())
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("kfx"))
        {
            kfx_names.push(entry.name().to_string());
        }
    }

    let mut out: Vec<LoadedContainer> = Vec::with_capacity(kfx_names.len());
    for name in &kfx_names {
        let mut entry = archive
            .by_name(name)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf)?;
        let source: Arc<dyn ByteSource> = Arc::new(MemorySource::new(buf));
        out.push(load_container(source)?);
    }

    Ok(out)
}

fn load_container(source: Arc<dyn ByteSource>) -> io::Result<LoadedContainer> {
    let header_data = source.read_at(0, 18)?;
    let header = parse_container_header(&header_data)?;

    let info_data = source.read_at(
        header.container_info_offset as u64,
        header.container_info_length,
    )?;
    let info = parse_container_info(&info_data)?;

    let (idx_off, idx_len) = info.index.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Missing index table in container",
        )
    })?;

    let doc_symbols = if let Some((off, len)) = info.doc_symbols
        && len > 0
    {
        extract_doc_symbols(&source.read_at(off as u64, len)?)
    } else {
        Vec::new()
    };

    let index_data = source.read_at(idx_off as u64, idx_len)?;
    let entity_locs = parse_index_table(&index_data, header.header_len);

    let mut entities = Vec::with_capacity(entity_locs.len());
    for loc in &entity_locs {
        let raw = source.read_at(loc.offset as u64, loc.length)?;
        let payload_bytes = skip_enty_header(&raw);

        let payload = if is_raw_payload_type(loc.type_id) {
            EntityPayload::Raw(payload_bytes.to_vec())
        } else {
            let mut parser = IonParser::new(payload_bytes);
            let value = parser.parse()?;
            EntityPayload::Ion(value)
        };

        entities.push(LoadedEntity {
            id: loc.id,
            type_id: loc.type_id,
            payload,
        });
    }

    Ok(LoadedContainer {
        doc_symbols,
        entities,
    })
}

/// Returns true for entity types whose payload after the ENTY header is raw
/// bytes (image/font data), not Ion. Currently only bcRawMedia and bcRawFont.
fn is_raw_payload_type(type_id: u32) -> bool {
    type_id == KfxSymbol::Bcrawmedia as u32 || type_id == KfxSymbol::Bcrawfont as u32
}

// --- Symbol unification ---

/// Build a unified doc_symbols list and a per-container old→new local-ID remap.
/// The remap value is the **local** index in the merged list (0-based); callers
/// add `BASE_LEN` to get the absolute symbol ID.
fn unify_symbols(containers: &[LoadedContainer]) -> (Vec<String>, Vec<Vec<u64>>) {
    let mut merged: Vec<String> = Vec::new();
    let mut seen: HashMap<String, u64> = HashMap::new();
    let mut per_container: Vec<Vec<u64>> = Vec::with_capacity(containers.len());

    for c in containers {
        let mut remap = Vec::with_capacity(c.doc_symbols.len());
        for sym in &c.doc_symbols {
            let new_idx = match seen.get(sym) {
                Some(&idx) => idx,
                None => {
                    let idx = merged.len() as u64;
                    merged.push(sym.clone());
                    seen.insert(sym.clone(), idx);
                    idx
                }
            };
            remap.push(new_idx);
        }
        per_container.push(remap);
    }

    (merged, per_container)
}

#[inline]
fn rewrite_symbol_id(id: u64, remap: &[u64]) -> u64 {
    if id < BASE_LEN {
        id
    } else {
        let local = (id - BASE_LEN) as usize;
        match remap.get(local) {
            Some(&new_local) => BASE_LEN + new_local,
            None => id, // beyond declared doc_symbols; pass through verbatim
        }
    }
}

fn rewrite_ion(value: &mut IonValue, remap: &[u64]) {
    match value {
        IonValue::Symbol(id) => *id = rewrite_symbol_id(*id, remap),
        IonValue::List(items) => {
            for item in items {
                rewrite_ion(item, remap);
            }
        }
        IonValue::Struct(fields) => {
            for (key, val) in fields {
                *key = rewrite_symbol_id(*key, remap);
                rewrite_ion(val, remap);
            }
        }
        IonValue::Annotated(annotations, inner) => {
            for ann in annotations {
                *ann = rewrite_symbol_id(*ann, remap);
            }
            rewrite_ion(inner, remap);
        }
        _ => {}
    }
}

// --- Entity serialization ---

fn build_serialized_entities(
    containers: Vec<LoadedContainer>,
    per_container_remap: &[Vec<u64>],
) -> Vec<SerializedEntity> {
    let mut out = Vec::new();
    for (c_idx, container) in containers.into_iter().enumerate() {
        let remap = &per_container_remap[c_idx];
        for ent in container.entities {
            let new_id = rewrite_symbol_id(ent.id as u64, remap) as u32;
            let new_type_id = rewrite_symbol_id(ent.type_id as u64, remap) as u32;
            let data = match ent.payload {
                EntityPayload::Ion(mut value) => {
                    rewrite_ion(&mut value, remap);
                    create_entity_data(&value)
                }
                EntityPayload::Raw(bytes) => create_raw_media_data(&bytes),
            };
            out.push(SerializedEntity {
                id: new_id,
                entity_type: new_type_id,
                data,
            });
        }
    }
    out
}

// --- Symbol table serialization ---

/// Build the `$ion_symbol_table` annotated struct ION blob:
///
/// ```ion
/// $ion_symbol_table::{
///   imports: [{ name: "YJ_symbols", version: 10, max_id: 851 }],
///   symbols: ["local_sym1", ...]
/// }
/// ```
fn build_symbol_table_ion(local_symbols: &[String]) -> Vec<u8> {
    let import_entry = IonValue::Struct(vec![
        (4, IonValue::String("YJ_symbols".to_string())),
        (5, IonValue::Int(10)),
        (8, IonValue::Int(KFX_MAX_SYMBOL_ID as i64)),
    ]);

    let symbols_list: Vec<IonValue> = local_symbols
        .iter()
        .map(|s| IonValue::String(s.clone()))
        .collect();

    let symbol_table = IonValue::Struct(vec![
        (6, IonValue::List(vec![import_entry])),
        (7, IonValue::List(symbols_list)),
    ]);

    let mut writer = IonWriter::new();
    writer.write_bvm();
    writer.write_annotated(&[3], &symbol_table);
    writer.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_symbol_id_passes_base_through() {
        let remap = vec![0u64];
        assert_eq!(rewrite_symbol_id(0, &remap), 0);
        assert_eq!(rewrite_symbol_id(490, &remap), 490);
        assert_eq!(rewrite_symbol_id(BASE_LEN - 1, &remap), BASE_LEN - 1);
    }

    #[test]
    fn rewrite_symbol_id_remaps_doc_local() {
        // Container had ["A", "B", "C"]; merged has ["X", "A", "B", "C"]
        // so old-local 0 (= "A") -> new-local 1
        let remap = vec![1, 2, 3];
        assert_eq!(rewrite_symbol_id(BASE_LEN, &remap), BASE_LEN + 1);
        assert_eq!(rewrite_symbol_id(BASE_LEN + 1, &remap), BASE_LEN + 2);
        assert_eq!(rewrite_symbol_id(BASE_LEN + 2, &remap), BASE_LEN + 3);
    }

    #[test]
    fn rewrite_ion_walks_struct_keys_and_annotations() {
        let remap = vec![5, 6];
        // {(BASE_LEN+1): Annotated([BASE_LEN], Symbol(BASE_LEN+1))}
        let mut v = IonValue::Struct(vec![(
            BASE_LEN + 1,
            IonValue::Annotated(vec![BASE_LEN], Box::new(IonValue::Symbol(BASE_LEN + 1))),
        )]);
        rewrite_ion(&mut v, &remap);
        let IonValue::Struct(fields) = &v else {
            panic!("not a struct")
        };
        let (k, val) = &fields[0];
        assert_eq!(*k, BASE_LEN + 6);
        let IonValue::Annotated(anns, inner) = val else {
            panic!("not annotated")
        };
        assert_eq!(anns[0], BASE_LEN + 5);
        let IonValue::Symbol(s) = **inner else {
            panic!("not symbol")
        };
        assert_eq!(s, BASE_LEN + 6);
    }

    #[test]
    fn unify_symbols_dedupes_across_containers() {
        let cs = vec![
            LoadedContainer {
                doc_symbols: vec!["foo".into(), "bar".into()],
                entities: vec![],
            },
            LoadedContainer {
                doc_symbols: vec!["bar".into(), "baz".into()],
                entities: vec![],
            },
        ];
        let (merged, remaps) = unify_symbols(&cs);
        assert_eq!(merged, vec!["foo", "bar", "baz"]);
        // Container 0: foo→0, bar→1
        assert_eq!(remaps[0], vec![0, 1]);
        // Container 1: bar→1, baz→2
        assert_eq!(remaps[1], vec![1, 2]);
    }
}
