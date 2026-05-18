//! `.kfx-zip` → `.kfx` merge pipeline.
//!
//! This is a mechanical port of calibre's `YJ_Book.convert_to_single_kfx`
//! pipeline (see `ref/calibre-kfx-input/kfxlib/yj_book.py:78`). It reads the
//! component `.kfx` files inside the `.kfx-zip`, aggregates their fragments,
//! rebuilds the container_entity_map and symbol table, and serializes a
//! single merged `.kfx`.
//!
//! Submodules:
//!  - [`catalog`]   — shared symbol catalogs (`$ion`, `YJ_symbols`).
//!  - [`symtab`]    — `LocalSymbolTable`.
//!  - [`node`]      — `IonNode` AST + binary parser/writer (string symbols).
//!  - [`fragment`]  — `YJFragment` + sort orders + ftype constants.
//!  - [`container`] — `KfxContainer` deserialize + serialize.
//!  - [`structure`] — fragment-list rebuild + symtab GC.

mod catalog;
mod container;
mod fragment;
mod node;
mod structure;
mod symtab;

use std::io::{self, Read};
use std::path::Path;

use container::{deserialize_container_phase1, loaded_container_into_fragments, LoadedContainer};
use fragment::YJFragment;
use structure::{finalize, rebuild_fragments_and_container_map, rebuild_symbol_table};
use symtab::LocalSymbolTable;

/// Merge a `.kfx-zip` bundle into a single `.kfx` container payload (bytes).
///
/// Pipeline (mirrors `YJ_Book.convert_to_single_kfx`):
///   1. Locate `.kfx` files inside the zip and sort them alphabetically.
///   2. Phase-1 deserialize each container; this mutates the shared symtab.
///   3. Phase-2: turn each loaded container into a fragment list and
///      aggregate.
///   4. Pick a merged-container id, kfxgen versions, and version number.
///   5. Rebuild `$270`, `$419`, sort fragments by preferred type order.
///   6. Rebuild the symbol table (drop unused locals, sort by natural-key).
///   7. Serialize.
pub fn merge_kfx_zip(path: &Path) -> io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    // Collect & sort .kfx entries (matches calibre's locate_book_datafiles
    // path that ends with `container_datafiles = sorted(...)`).
    let mut kfx_names: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        if entry.is_file()
            && Path::new(entry.name())
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("kfx"))
        {
            kfx_names.push(entry.name().to_string());
        }
    }
    kfx_names.sort();
    if kfx_names.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kfx-zip contains no .kfx entries",
        ));
    }

    // Phase 1: load each container; mutates symtab in place.
    let mut symtab = LocalSymbolTable::new();
    let mut loaded: Vec<LoadedContainer> = Vec::with_capacity(kfx_names.len());
    for name in &kfx_names {
        let mut entry = archive
            .by_name(name)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf)?;
        let lc = deserialize_container_phase1(buf, &mut symtab)?;
        loaded.push(lc);
    }

    // Phase 2: with the symtab fully populated, turn each container into a
    // YJFragment list. Aggregate.
    let mut fragments: Vec<YJFragment> = Vec::new();
    for lc in &loaded {
        fragments.extend(loaded_container_into_fragments(lc, &symtab)?);
    }

    // Pick the merged container's metadata. Calibre's rule:
    //  - If all source $270 fragments agree on container_id → use it.
    //  - Else use `get_asset_id()` from the $490 metadata.
    //  - Else generate a random `CR!*` id.
    // kfxgen versions: last non-empty across containers (calibre uses
    // `value.get("$587") or kfxgen_application_version`).
    let (merged_id, app_version, pkg_version, version) =
        decide_merged_container_metadata(&fragments);

    // Rebuild $270 + $419, sort fragments.
    let mut fragments = rebuild_fragments_and_container_map(
        fragments,
        merged_id,
        app_version,
        pkg_version,
        version,
    );

    // Rebuild symtab (GC unused locals, sort, replace $ion_symbol_table).
    rebuild_symbol_table(&mut fragments, &mut symtab);

    // Serialize.
    Ok(finalize(&fragments, &symtab))
}

fn decide_merged_container_metadata(
    fragments: &[YJFragment],
) -> (String, String, String, i64) {
    let mut container_ids: std::collections::BTreeSet<String> = Default::default();
    let mut app_version = String::new();
    let mut pkg_version = String::new();
    let mut version: i64 = 2;

    for f in fragments {
        if f.ftype != "$270" {
            continue;
        }
        if let Some(s) = f.value.get_field("$409").and_then(|v| v.as_string()) {
            if !s.is_empty() {
                container_ids.insert(s.to_string());
            }
        }
        if let Some(s) = f.value.get_field("$587").and_then(|v| v.as_string()) {
            if !s.is_empty() {
                app_version = s.to_string();
            }
        }
        if let Some(s) = f.value.get_field("$588").and_then(|v| v.as_string()) {
            if !s.is_empty() {
                pkg_version = s.to_string();
            }
        }
        if let Some(n) = f.value.get_field("version").and_then(|v| v.as_int()) {
            version = n;
        }
    }

    let merged_id = if container_ids.len() == 1 {
        container_ids.iter().next().unwrap().clone()
    } else {
        // Try $490 → kindle_title_metadata → asset_id.
        if let Some(asset_id) = lookup_asset_id(fragments) {
            asset_id
        } else if let Some(main_id) = lookup_main_container_id(fragments) {
            main_id
        } else {
            generate_container_id()
        }
    };

    if app_version.is_empty() {
        app_version = format!("kfxlib-{}", env!("CARGO_PKG_VERSION"));
    }
    (merged_id, app_version, pkg_version, version)
}

fn lookup_asset_id(fragments: &[YJFragment]) -> Option<String> {
    let frag = fragments.iter().find(|f| f.ftype == "$490")?;
    let categories = frag.value.get_field("$491")?.as_list()?;
    for cat in categories {
        let cat_name = cat
            .get_field("$495")
            .and_then(|n| n.as_string())
            .unwrap_or("");
        if cat_name != "kindle_title_metadata" {
            continue;
        }
        let kvs = cat.get_field("$258")?.as_list()?;
        for kv in kvs {
            let k = kv.get_field("$492").and_then(|n| n.as_string()).unwrap_or("");
            if k == "asset_id" {
                if let Some(v) = kv.get_field("$307").and_then(|n| n.as_string()) {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Heuristic fallback: pick the $270 with the most entities in its
/// `$181` list (the main content container).
fn lookup_main_container_id(fragments: &[YJFragment]) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for f in fragments {
        if f.ftype != "$270" {
            continue;
        }
        let cid = f
            .value
            .get_field("$409")
            .and_then(|v| v.as_string())
            .unwrap_or("")
            .to_string();
        let count = f
            .value
            .get_field("$181")
            .and_then(|v| v.as_list())
            .map(|l| l.len())
            .unwrap_or(0);
        if best.as_ref().map(|(c, _)| count > *c).unwrap_or(true) {
            best = Some((count, cid));
        }
    }
    best.map(|(_, id)| id).filter(|s| !s.is_empty())
}

fn generate_container_id() -> String {
    // Calibre uses random.choice; we use SystemTime + a simple LCG so the
    // output is deterministic-per-process-second (not load-bearing — calibre's
    // bcContId varies per run too).
    let mut state: u128 = {
        #[cfg(target_arch = "wasm32")]
        {
            (js_sys::Date::now() as u128) * 1_000_000
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        }
    };
    let chars: Vec<char> = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ".chars().collect();
    let mut id = String::from("CR!");
    for _ in 0..28 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let idx = ((state >> 56) as usize) % chars.len();
        id.push(chars[idx]);
    }
    id
}

