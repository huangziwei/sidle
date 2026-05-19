//! Mechanical port of calibre's `YJ_Book.convert_to_single_kfx`.
//!
//! Every fragment is fully deserialized into [`IonNode`] form, all of
//! calibre's structure-rebuild + symtab-GC passes run, and every entity is
//! re-encoded from scratch. This path is the correctness ground truth — it's
//! what we measured "30/30 EPUB content match" against. The [`fast`] path
//! short-circuits most of this.
//!
//! [`IonNode`]: super::node::IonNode
//! [`fast`]: super::fast

use std::io::{self, Read};
use std::path::Path;

use super::container::{
    deserialize_container_phase1, loaded_container_into_fragments, LoadedContainer,
};
use super::fragment::YJFragment;
use super::structure::{finalize, rebuild_fragments_and_container_map, rebuild_symbol_table};
use super::symtab::LocalSymbolTable;
use crate::trace::Trace;

pub fn merge_kfx_zip(path: &Path) -> io::Result<Vec<u8>> {
    let trace = Trace::new("merge-mechanical", "BOKO_MERGE_TRACE");
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

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
    trace.mark("zip open + entry list");

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
    trace.mark("phase 1 (unzip + container headers)");

    let mut fragments: Vec<YJFragment> = Vec::new();
    for lc in &loaded {
        fragments.extend(loaded_container_into_fragments(lc, &symtab)?);
    }
    trace.mark("phase 2 (parse entity bodies)");

    let (merged_id, app_version, pkg_version, version) =
        decide_merged_container_metadata(&fragments);
    trace.mark("pick container metadata");

    let mut fragments = rebuild_fragments_and_container_map(
        fragments,
        merged_id,
        app_version,
        pkg_version,
        version,
    );
    trace.mark("rebuild $270/$419 + sort");

    rebuild_symbol_table(&mut fragments, &mut symtab);
    trace.mark("rebuild symbol table");

    let bytes = finalize(&fragments, &symtab);
    trace.mark("serialize");
    Ok(bytes)
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
    } else if let Some(asset_id) = lookup_asset_id(fragments) {
        asset_id
    } else if let Some(main_id) = lookup_main_container_id(fragments) {
        main_id
    } else {
        super::common::generate_container_id()
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
