//! Fast `.kfx-zip` → `.kfx` merge — pass entity bodies through verbatim.
//!
//! ## Why this can be fast
//!
//! In Amazon-distributed `.kfx-zip` bundles, **exactly one** of the inner
//! `.kfx` files (`metadata.kfx`) carries a `doc_symbols` Ion symbol table.
//! The other three (`CR!*.kfx` resource containers + the main content file)
//! are authored against that same symtab — every symbol ID inside their
//! entity bodies resolves through `metadata.kfx`'s locals.
//!
//! The mechanical port treats each entity body as opaque Ion that must be
//! parsed → walked → re-encoded. That parse + serialize is ~75% of the merge
//! time on a typical book. Once we know we can preserve the source symtab
//! intact, none of that work is required: every entity body's bytes are
//! **already valid** in the merged container with the same symtab attached.
//!
//! So this path:
//!  - parses only the 4 container headers (~100 bytes each),
//!  - parses `doc_symbols` once and reuses it byte-for-byte,
//!  - parses the source `$419` body once to keep its `$253` deps,
//!  - **copies the other 349/350 entity bodies verbatim**,
//!  - synthesizes a fresh `$270` container_info + `$419` entity_map.
//!
//! ## When this path bails out
//!
//! The fast path requires:
//!  - at most one source container with `doc_symbols`, and
//!  - the source `$419` (if any) entity-table row uses `id_idnum == $348`
//!    (singleton form) so its body can be unwrapped cleanly.
//!
//! Both hold for every Amazon `.kfx-zip` we've seen. When they don't, the
//! caller should fall back to [`crate::kfx::merge::mechanical`].

use std::io::{self, Read};
use std::path::Path;

use super::node::{parse_single_value, serialize_single_value, IonNode, ION_BVM};
use super::symtab::{LocalSymbolTable, SymbolTableImport, SYSTEM_SIZE};
use crate::trace::Trace;

const CONT_SIGNATURE: &[u8] = b"CONT";
const ENTY_SIGNATURE: &[u8] = b"ENTY";
const CONTAINER_VERSION: u16 = 2;
const ENTITY_VERSION: u16 = 1;
const DEFAULT_CHUNK_SIZE: i64 = 4096;
const SYM_DOLLAR_348: u32 = 348;
const SYM_DOLLAR_419: u32 = 419;
const SYM_DOLLAR_270: u32 = 270;

/// Compact view of one source `.kfx` after the cheap parse phase.
struct RawContainer {
    data: Vec<u8>,
    container_id: String,
    kfxgen_app_version: String,
    kfxgen_pkg_version: String,
    version: i64,
    doc_symbols_range: Option<(usize, usize)>,
    format_capabilities_range: Option<(usize, usize)>,
    entity_rows: Vec<RawEntityRow>,
}

#[derive(Clone, Copy)]
struct RawEntityRow {
    id_idnum: u32,
    type_idnum: u32,
    /// Absolute offset within the source container's `data`.
    body_offset: usize,
    body_length: usize,
}

pub fn merge_kfx_zip(path: &Path) -> io::Result<Vec<u8>> {
    let trace = Trace::new("merge-fast", "BOKO_MERGE_TRACE");

    // ---- 1. Read zip + sort entries.
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
    trace.mark("zip + entry list");

    // ---- 2. Lightweight per-container parse (header + entity table only).
    //
    // We have to walk container_info Ion to learn the entity-table offset
    // and the doc_symbols byte range. Each source `.kfx` is decompressed +
    // shallow-parsed on its own thread — deflate is the merge's largest
    // single phase (~5 ms on a 1.3 MB bundle) and fans out cleanly when
    // each thread owns its own `ZipArchive` (parsing the central directory
    // is cheap; the win is overlapping the deflate work).
    //
    // The per-container parser uses a *fresh* symtab because container_info
    // only ever references SYSTEM + YJ catalog symbols (numeric `$N` form)
    // — no local symbols, no shared state needed.
    //
    // The decompress and SHA1 phases overlap: the decompression threads
    // join first (we need every container's entity table before we know
    // what bytes to skip / hash), then a hasher thread runs in parallel
    // with the main thread's symtab parse + `$419` build + body memcpy.
    //
    // We also experimented with *per-container pipelined* SHA1 (hash each
    // container the moment it decompresses, in alphabetical/output order).
    // That variant did shave ~1 ms off the hot trace, but its OnceLock +
    // mpsc machinery cost more on average across the 30-book corpus
    // (50-run-avg median went from 28 → 30 ms — process startup dominates
    // and the channel overhead pushes the warm work up). The batch-join
    // variant below is the corpus sweet spot.
    let raws = decompress_containers_parallel(path, &kfx_names)?;
    let raws_refs: Vec<&RawContainer> = raws.iter().collect();
    trace.mark("unzip + shallow parse");

    let (new_419_tx, new_419_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
    let bytes = std::thread::scope(|scope| -> io::Result<Vec<u8>> {
        let raws_for_sha = &raws_refs;
        let sha1_handle = scope.spawn(move || -> io::Result<[u8; 20]> {
            let mut hasher = sha1_smol::Sha1::new();
            for rc in raws_for_sha {
                for &(off, len) in &per_container_hash_chunks(rc) {
                    hasher.update(&rc.data[off..off + len]);
                }
            }
            if let Ok(new_419) = new_419_rx.recv() {
                hasher.update(&new_419);
            }
            Ok(hasher.digest().bytes())
        });
        finish_merge(scope, &raws_refs, new_419_tx, sha1_handle, trace)
    })?;
    Ok(bytes)
}

fn decompress_containers_parallel(
    path: &Path,
    kfx_names: &[String],
) -> io::Result<Vec<RawContainer>> {
    // Spawn one OS thread per source `.kfx`. Each thread opens its own
    // `ZipArchive` (central-directory parse is ~tens of microseconds, so
    // the duplication is cheap) and decompresses one entry. The deflate
    // work overlaps across threads; wall time is bound by the largest
    // single source.
    if kfx_names.len() <= 1 {
        return kfx_names.iter().map(|n| decompress_one(path, n)).collect();
    }
    let mut results: Vec<Option<io::Result<RawContainer>>> =
        (0..kfx_names.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(kfx_names.len());
        for name in kfx_names {
            handles.push(scope.spawn(move || decompress_one(path, name)));
        }
        for (i, h) in handles.into_iter().enumerate() {
            results[i] = Some(
                h.join()
                    .unwrap_or_else(|_| Err(io::Error::other("decompression thread panicked"))),
            );
        }
    });
    let mut out = Vec::with_capacity(kfx_names.len());
    for r in results.into_iter() {
        out.push(r.expect("slot filled")?);
    }
    Ok(out)
}

/// Body byte ranges (offset, length) within a single source container that
/// should be hashed — i.e. all entity bodies except the source `$419`'s body.
/// Within one container the entity bodies are laid out contiguously, so this
/// is at most two `(off, len)` runs.
fn per_container_hash_chunks(rc: &RawContainer) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(2);
    let mut cur: Option<(usize, usize)> = None;
    for row in &rc.entity_rows {
        if row.type_idnum == SYM_DOLLAR_419 {
            if let Some(c) = cur.take() {
                out.push(c);
            }
            continue;
        }
        match cur {
            Some((off, len)) if off + len == row.body_offset => {
                cur = Some((off, len + row.body_length));
            }
            Some(prev) => {
                out.push(prev);
                cur = Some((row.body_offset, row.body_length));
            }
            None => {
                cur = Some((row.body_offset, row.body_length));
            }
        }
    }
    if let Some(c) = cur {
        out.push(c);
    }
    out
}

fn finish_merge<'a>(
    _scope: &'a std::thread::Scope<'a, '_>,
    raws: &[&RawContainer],
    new_419_tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    sha1_handle: std::thread::ScopedJoinHandle<'a, io::Result<[u8; 20]>>,
    trace: Trace,
) -> io::Result<Vec<u8>> {
    let mut symtab = LocalSymbolTable::new();
    let mut doc_symbols_bytes_owned: Option<Vec<u8>> = None;
    let mut doc_symbols_count = 0;
    for r in raws {
        if let Some((off, len)) = r.doc_symbols_range {
            doc_symbols_count += 1;
            doc_symbols_bytes_owned = Some(r.data[off..off + len].to_vec());
        }
    }
    if doc_symbols_count > 1 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "fast path: multiple source containers carry doc_symbols",
        ));
    }
    if let Some(ds_bytes) = doc_symbols_bytes_owned.as_ref() {
        populate_symtab_from_doc_symbols(&mut symtab, ds_bytes)?;
    }
    let mut format_capabilities_bytes: Option<Vec<u8>> = None;
    for r in raws {
        if let Some((off, len)) = r.format_capabilities_range {
            format_capabilities_bytes = Some(r.data[off..off + len].to_vec());
            break;
        }
    }
    trace.mark("symtab + format_capabilities");

    let (merged_id, app_version, pkg_version, version) =
        pick_merged_metadata(raws, &symtab)?;
    trace.mark("pick metadata");

    let mut merged_entities: Vec<(RawEntityRow, usize)> = Vec::new();
    let mut existing_419_deps: Option<IonNode> = None;
    for (c_idx, r) in raws.iter().enumerate() {
        for row in &r.entity_rows {
            if row.type_idnum == SYM_DOLLAR_419 {
                let body =
                    extract_entity_body(&r.data, row.body_offset, row.body_length)?;
                let mut node = parse_single_value(body, &symtab)?;
                if let IonNode::Annotated(_, inner) = node {
                    node = *inner;
                }
                if let Some(deps) = node.get_field("$253") {
                    existing_419_deps = Some(deps.clone());
                }
                continue;
            }
            merged_entities.push((*row, c_idx));
        }
    }

    let mut entity_fids: Vec<String> = Vec::with_capacity(merged_entities.len() + 1);
    let mut seen_fids: std::collections::HashSet<String> = Default::default();
    for (row, _) in &merged_entities {
        if row.id_idnum == SYM_DOLLAR_348 {
            continue;
        }
        let fid = symtab.get_symbol(row.id_idnum);
        if seen_fids.insert(fid.clone()) {
            entity_fids.push(fid);
        }
    }
    trace.mark("aggregate entities");

    let chunks = coalesce_body_chunks(raws, &merged_entities);
    let new_419_body = build_419_body(&merged_id, &entity_fids, existing_419_deps);
    let new_419_ion_bytes = serialize_single_value(&new_419_body, &symtab);
    let new_419_entity = wrap_entity_body(&new_419_ion_bytes, &symtab);
    let _ = new_419_tx.send(new_419_entity.clone());
    trace.mark("build $419 + sha1-thread fed");

    let out = emit_container_streaming(
        raws,
        &merged_entities,
        &new_419_entity,
        &chunks,
        &symtab,
        doc_symbols_bytes_owned.as_deref(),
        format_capabilities_bytes.as_deref(),
        &merged_id,
        &app_version,
        &pkg_version,
        version,
        &trace,
    );

    let digest_bytes = sha1_handle.join().expect("hash thread panicked")?;
    trace.mark("sha1 thread joined");
    Ok(finalize_sha1_backfill(out, digest_bytes))
}

fn finalize_sha1_backfill(mut out: Vec<u8>, digest: [u8; 20]) -> Vec<u8> {
    // The placeholder is the first run of 40 ASCII '0' bytes inside
    // kfxgen_info — which is the only place 40 consecutive '0's can appear
    // (entity body bytes can hold zeros but never 40 ASCII zeros in a row in
    // practice, and we sized the placeholder specifically so it's findable).
    // We use a fixed kfxgen_info template, so the offset was recorded into
    // the buffer's `sha1_abs_off` slot via [`emit_container_streaming`].
    let sha1_abs_off = u32::from_le_bytes([
        out[out.len() - 4],
        out[out.len() - 3],
        out[out.len() - 2],
        out[out.len() - 1],
    ]) as usize;
    out.truncate(out.len() - 4);
    let mut hex = [0u8; 40];
    write_hex_lower(&digest, &mut hex);
    out[sha1_abs_off..sha1_abs_off + 40].copy_from_slice(&hex);
    out
}

// =========================================================================
// Parallel zip decompression
// =========================================================================

fn decompress_one(path: &Path, name: &str) -> io::Result<RawContainer> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut entry = archive
        .by_name(name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buf)?;
    parse_container_shallow(buf)
}

// =========================================================================
// Shallow parse
// =========================================================================

fn parse_container_shallow(data: Vec<u8>) -> io::Result<RawContainer> {
    if data.len() < 18 || &data[0..4] != CONT_SIGNATURE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad container signature",
        ));
    }
    let version = u16::from_le_bytes([data[4], data[5]]) as i64;
    let header_len = u32_le(&data, 6) as usize;
    let ci_off = u32_le(&data, 10) as usize;
    let ci_len = u32_le(&data, 14) as usize;

    // Container_info uses only SYSTEM + YJ catalog symbols, so a fresh
    // (system-only) symtab resolves $409 / $413 / etc. just fine — those are
    // all `$<id>` canonical names.
    let probe_symtab = LocalSymbolTable::new();
    let ci_node = parse_single_value(&data[ci_off..ci_off + ci_len], &probe_symtab)?;
    let ci_fields = ci_node.as_struct().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "container_info is not a struct")
    })?;

    let container_id = field_string(ci_fields, "$409").unwrap_or_default();
    let doc_symbol_offset = field_int(ci_fields, "$415");
    let doc_symbol_length = field_int(ci_fields, "$416").unwrap_or(0);
    let fc_offset = field_int(ci_fields, "$594");
    let fc_length = field_int(ci_fields, "$595").unwrap_or(0);
    let idx_offset = field_int(ci_fields, "$413");
    let idx_length = field_int(ci_fields, "$414").unwrap_or(0);

    let doc_symbols_range = match (doc_symbol_offset, doc_symbol_length) {
        (Some(o), l) if l > 0 => Some((o as usize, l as usize)),
        _ => None,
    };
    let format_capabilities_range = match (fc_offset, fc_length) {
        (Some(o), l) if l > 0 => Some((o as usize, l as usize)),
        _ => None,
    };

    let mut entity_rows = Vec::new();
    if let Some(idx_off) = idx_offset {
        let idx_off = idx_off as usize;
        let idx_len = idx_length as usize;
        const ROW: usize = 24;
        let n = idx_len / ROW;
        let table = &data[idx_off..idx_off + idx_len];
        for i in 0..n {
            let base = i * ROW;
            entity_rows.push(RawEntityRow {
                id_idnum: u32_le(table, base),
                type_idnum: u32_le(table, base + 4),
                body_offset: header_len + u64_le(table, base + 8) as usize,
                body_length: u64_le(table, base + 16) as usize,
            });
        }
    }

    let kfxgen_info_bytes = &data[ci_off + ci_len..header_len];
    let kfxgen_info_str = String::from_utf8_lossy(kfxgen_info_bytes);
    let app_version = extract_kfxgen_field(&kfxgen_info_str, "kfxgen_application_version")
        .or_else(|| extract_kfxgen_field(&kfxgen_info_str, "appVersion"))
        .unwrap_or_default();
    let pkg_version = extract_kfxgen_field(&kfxgen_info_str, "kfxgen_package_version")
        .or_else(|| extract_kfxgen_field(&kfxgen_info_str, "buildVersion"))
        .unwrap_or_default();

    Ok(RawContainer {
        data,
        container_id,
        kfxgen_app_version: app_version,
        kfxgen_pkg_version: pkg_version,
        version,
        doc_symbols_range,
        format_capabilities_range,
        entity_rows,
    })
}

fn populate_symtab_from_doc_symbols(
    symtab: &mut LocalSymbolTable,
    bytes: &[u8],
) -> io::Result<()> {
    // We need to parse doc_symbols just to populate the in-memory symtab.
    // Symbol-table parse uses only the SYSTEM symbols, so an initial empty
    // symtab is fine.
    let probe = LocalSymbolTable::new();
    let mut value = parse_single_value(bytes, &probe)?;
    if let IonNode::Annotated(_, inner) = value {
        value = *inner;
    }
    let fields = value.as_struct().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "doc_symbols payload is not a struct",
        )
    })?;
    let mut imports: Vec<SymbolTableImport> = Vec::new();
    let mut locals: Vec<String> = Vec::new();
    for (k, v) in fields {
        match k.as_str() {
            "imports" => {
                if let Some(items) = v.as_list() {
                    for item in items {
                        let Some(f) = item.as_struct() else { continue };
                        let name = field_string(f, "name").unwrap_or_default();
                        let version = field_int(f, "version").unwrap_or(1) as u32;
                        let max_id = field_int(f, "max_id").unwrap_or(0) as u32;
                        // Calibre subtracts SYSTEM size from wire max_id.
                        let max_id = max_id.saturating_sub(SYSTEM_SIZE);
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
            "symbols" => {
                if let Some(items) = v.as_list() {
                    for item in items {
                        if let Some(s) = item.as_string() {
                            locals.push(s.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    symtab.create(&imports, &locals);
    Ok(())
}

// =========================================================================
// Metadata picker
// =========================================================================

fn pick_merged_metadata(
    raws: &[&RawContainer],
    symtab: &LocalSymbolTable,
) -> io::Result<(String, String, String, i64)> {
    let mut app_version = String::new();
    let mut pkg_version = String::new();
    let mut version: i64 = 2;
    let mut largest_container: Option<(usize, &RawContainer)> = None;

    for &r in raws {
        if !r.kfxgen_app_version.is_empty() {
            app_version = r.kfxgen_app_version.clone();
        }
        if !r.kfxgen_pkg_version.is_empty() {
            pkg_version = r.kfxgen_pkg_version.clone();
        }
        version = r.version;
        match largest_container {
            Some((n, _)) if n >= r.entity_rows.len() => {}
            _ => largest_container = Some((r.entity_rows.len(), r)),
        }
    }

    // Prefer asset_id from $490 (kindle_title_metadata). Same rule the
    // mechanical path uses for parity.
    let merged_id = if let Some(asset) = find_asset_id(raws, symtab)? {
        asset
    } else if let Some((_, r)) = largest_container {
        if r.container_id.is_empty() {
            super::common::generate_container_id()
        } else {
            r.container_id.clone()
        }
    } else {
        super::common::generate_container_id()
    };

    if app_version.is_empty() {
        app_version = format!("kfxlib-{}", env!("CARGO_PKG_VERSION"));
    }
    Ok((merged_id, app_version, pkg_version, version))
}

fn find_asset_id(
    raws: &[&RawContainer],
    symtab: &LocalSymbolTable,
) -> io::Result<Option<String>> {
    // $490 metadata lives in metadata.kfx as an entity. We must parse it.
    // It's small (~3 KB), so the cost is negligible.
    for r in raws {
        for row in &r.entity_rows {
            if row.type_idnum != 490 {
                continue;
            }
            let body = extract_entity_body(&r.data, row.body_offset, row.body_length)?;
            let mut node = parse_single_value(body, symtab)?;
            if let IonNode::Annotated(_, inner) = node {
                node = *inner;
            }
            let Some(categories) = node.get_field("$491").and_then(|n| n.as_list()) else {
                continue;
            };
            for cat in categories {
                let cat_name = cat
                    .get_field("$495")
                    .and_then(|n| n.as_string())
                    .unwrap_or("");
                if cat_name != "kindle_title_metadata" {
                    continue;
                }
                let Some(kvs) = cat.get_field("$258").and_then(|n| n.as_list()) else {
                    continue;
                };
                for kv in kvs {
                    let k = kv.get_field("$492").and_then(|n| n.as_string()).unwrap_or("");
                    if k == "asset_id" {
                        if let Some(v) = kv.get_field("$307").and_then(|n| n.as_string()) {
                            return Ok(Some(v.to_string()));
                        }
                    }
                }
            }
        }
    }
    Ok(None)
}

// =========================================================================
// $419 body
// =========================================================================

fn build_419_body(
    container_id: &str,
    entity_fids: &[String],
    existing_deps: Option<IonNode>,
) -> IonNode {
    let container_contents = IonNode::Struct(vec![
        ("$155".into(), IonNode::String(container_id.to_string())),
        (
            "$181".into(),
            IonNode::List(
                entity_fids
                    .iter()
                    .map(|s| IonNode::Symbol(s.clone()))
                    .collect(),
            ),
        ),
    ]);
    let mut fields = vec![("$252".into(), IonNode::List(vec![container_contents]))];
    if let Some(deps) = existing_deps {
        fields.push(("$253".into(), deps));
    }
    IonNode::Struct(fields)
}

fn wrap_entity_body(body: &[u8], symtab: &LocalSymbolTable) -> Vec<u8> {
    let info_node = IonNode::Struct(vec![
        ("$410".into(), IonNode::Int(0)),
        ("$411".into(), IonNode::Int(0)),
    ]);
    let mut info_bytes = Vec::from(ION_BVM);
    info_bytes.extend_from_slice(&super::node::serialize_value(&info_node, symtab));
    let header_len = (10 + info_bytes.len()) as u32;
    let mut out = Vec::with_capacity(header_len as usize + body.len());
    out.extend_from_slice(ENTY_SIGNATURE);
    out.extend_from_slice(&ENTITY_VERSION.to_le_bytes());
    out.extend_from_slice(&header_len.to_le_bytes());
    out.extend_from_slice(&info_bytes);
    out.extend_from_slice(body);
    out
}

// =========================================================================
// Container layout
// =========================================================================

/// Streaming container emit. Writes the full container minus the SHA1 digest
/// (which a side thread is computing in parallel). The SHA1 placeholder slot's
/// absolute offset is appended to the buffer as 4 trailing LE bytes — the
/// caller strips them off after backfilling the real digest.
#[allow(clippy::too_many_arguments)]
fn emit_container_streaming(
    raws: &[&RawContainer],
    merged_entities: &[(RawEntityRow, usize)],
    new_419_entity: &[u8],
    chunks: &[(usize, usize, usize)],
    symtab: &LocalSymbolTable,
    doc_symbols_bytes: Option<&[u8]>,
    format_capabilities_bytes: Option<&[u8]>,
    merged_id: &str,
    app_version: &str,
    pkg_version: &str,
    version: i64,
    trace: &Trace,
) -> Vec<u8> {
    // Pre-compute total output size: header + entity table + doc_symbols
    //   + format_capabilities + container_info (≤300 B est.) + kfxgen_info
    //   + sum(entity bodies).
    let n_rows = merged_entities.len() + 1; // +1 for synthesized $419
    let entity_table_size = n_rows * 24;
    let bodies_size: usize = merged_entities
        .iter()
        .map(|(r, _)| r.body_length)
        .sum::<usize>()
        + new_419_entity.len();
    let ds_len = doc_symbols_bytes.map_or(0, |b| b.len());
    let fc_len = format_capabilities_bytes.map_or(0, |b| b.len());
    let mut out = Vec::with_capacity(
        18 + entity_table_size + ds_len + fc_len + 512 + bodies_size,
    );

    // Fixed header (18 bytes), patched at the end.
    out.extend_from_slice(CONT_SIGNATURE);
    out.extend_from_slice(&CONTAINER_VERSION.to_le_bytes());
    let header_len_pack = out.len();
    out.extend_from_slice(&[0u8; 4]);
    let ci_off_pack = out.len();
    out.extend_from_slice(&[0u8; 4]);
    let ci_len_pack = out.len();
    out.extend_from_slice(&[0u8; 4]);

    let entity_table_off = out.len();
    // Entity-table rows. Source rows first (in source-walk order), then
    // synthesized $419 row last.
    let mut entity_offset: u64 = 0;
    for (row, _) in merged_entities {
        out.extend_from_slice(&row.id_idnum.to_le_bytes());
        out.extend_from_slice(&row.type_idnum.to_le_bytes());
        out.extend_from_slice(&entity_offset.to_le_bytes());
        out.extend_from_slice(&(row.body_length as u64).to_le_bytes());
        entity_offset += row.body_length as u64;
    }
    // Synthesized $419: id_idnum = $348 (singleton form).
    out.extend_from_slice(&SYM_DOLLAR_348.to_le_bytes());
    out.extend_from_slice(&SYM_DOLLAR_419.to_le_bytes());
    out.extend_from_slice(&entity_offset.to_le_bytes());
    out.extend_from_slice(&(new_419_entity.len() as u64).to_le_bytes());

    // doc_symbols verbatim. Container_info captures its offset/length.
    let doc_symbols_off = out.len();
    if let Some(ds) = doc_symbols_bytes {
        out.extend_from_slice(ds);
    }

    let format_caps_off = out.len();
    if let Some(fc) = format_capabilities_bytes {
        out.extend_from_slice(fc);
    }

    // Build container_info struct. Same field ordering as the mechanical
    // path so calibre's deserializer sees the same shape.
    let mut ci_fields: Vec<(String, IonNode)> = Vec::new();
    ci_fields.push((
        "$409".into(),
        IonNode::String(merged_id.to_string()),
    ));
    ci_fields.push(("$410".into(), IonNode::Int(0)));
    ci_fields.push(("$411".into(), IonNode::Int(0)));
    ci_fields.push(("$413".into(), IonNode::Int(entity_table_off as i64)));
    ci_fields.push((
        "$414".into(),
        IonNode::Int(entity_table_size as i64),
    ));
    ci_fields.push(("$415".into(), IonNode::Int(doc_symbols_off as i64)));
    ci_fields.push(("$416".into(), IonNode::Int(ds_len as i64)));
    ci_fields.push(("$412".into(), IonNode::Int(DEFAULT_CHUNK_SIZE)));
    if fc_len > 0 && symtab.local_min_id() > 595 {
        ci_fields.push(("$594".into(), IonNode::Int(format_caps_off as i64)));
        ci_fields.push(("$595".into(), IonNode::Int(fc_len as i64)));
    }
    let ci_bytes = serialize_single_value(&IonNode::Struct(ci_fields), symtab);
    let ci_off_value = out.len() as u32;
    patch_u32_le(&mut out, ci_off_pack, ci_off_value);
    patch_u32_le(&mut out, ci_len_pack, ci_bytes.len() as u32);
    out.extend_from_slice(&ci_bytes);

    // kfxgen_info trailer (calibre's JSON-textish form). We write the SHA1
    // field with a known-length placeholder (40 zeros), copy the entity
    // bodies while incrementally hashing them, then backfill the real digest
    // into the placeholder slot. One pass over the body bytes instead of two.
    const SHA1_PLACEHOLDER: &str = "0000000000000000000000000000000000000000";
    let kfxgen_info = format!(
        r#"[{{key:"kfxgen_package_version",value:"{}"}},{{key:"kfxgen_application_version",value:"{}"}},{{key:"kfxgen_payload_sha1",value:"{}"}},{{key:"kfxgen_acr",value:"{}"}}]"#,
        escape_json(pkg_version),
        escape_json(app_version),
        SHA1_PLACEHOLDER,
        escape_json(merged_id),
    );
    // Locate the placeholder inside kfxgen_info so we can backfill after we
    // know the real hash. The placeholder is unique within the trailer (all
    // other fields are non-hex prose).
    let sha1_local_off = kfxgen_info
        .find(SHA1_PLACEHOLDER)
        .expect("SHA1 placeholder we just wrote must be present");
    let sha1_abs_off = out.len() + sha1_local_off;
    out.extend_from_slice(kfxgen_info.as_bytes());

    let header_len_value = out.len() as u32;
    patch_u32_le(&mut out, header_len_pack, header_len_value);
    trace.mark("header laid out");

    // Entity body memcpy only — hashing is handled by the side thread spawned
    // earlier in `merge_kfx_zip`. The chunks were already coalesced (per-
    // source contiguous runs, ~5 for a typical book), so this is a tiny
    // number of large memcpys.
    for &(c_idx, off, len) in chunks {
        out.extend_from_slice(&raws[c_idx].data[off..off + len]);
    }
    out.extend_from_slice(new_419_entity);
    trace.mark("entity bodies copied");

    // Pass the SHA1-placeholder offset back to the caller via a 4-byte LE
    // trailer that they strip + read.
    out.extend_from_slice(&(sha1_abs_off as u32).to_le_bytes());

    // version + format alignment with the mechanical path (we don't actually
    // need `version` in the output; calibre reads it from the on-wire header
    // u16). The local already has version=2 in the header.
    let _ = version;

    out
}

// =========================================================================
// Helpers
// =========================================================================

fn extract_entity_body(data: &[u8], offset: usize, length: usize) -> io::Result<&[u8]> {
    let body = &data[offset..offset + length];
    if body.len() < 10 || &body[0..4] != ENTY_SIGNATURE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad ENTY signature",
        ));
    }
    let header_len = u32_le(body, 6) as usize;
    Ok(&body[header_len..])
}

fn field_string(fields: &[(String, IonNode)], key: &str) -> Option<String> {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| match v {
            IonNode::String(s) => Some(s.clone()),
            _ => None,
        })
}

fn field_int(fields: &[(String, IonNode)], key: &str) -> Option<i64> {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| match v {
            IonNode::Int(n) => Some(*n),
            _ => None,
        })
}

fn extract_kfxgen_field(s: &str, key: &str) -> Option<String> {
    let key_pat = format!("key:\"{}\"", key);
    let pos = s.find(&key_pat)?;
    let rest = &s[pos + key_pat.len()..];
    let val_start = rest.find("value:\"")? + "value:\"".len();
    let val_rest = &rest[val_start..];
    let val_end = val_rest.find('"')?;
    Some(val_rest[..val_end].to_string())
}

fn escape_json(s: &str) -> String {
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

/// Group contiguous entity-body byte ranges per source container into one
/// (or two) merge-able chunks. Returns `(source_container_idx, offset_into_data, length)`.
///
/// Within a source `.kfx`, entity bodies are written contiguously starting at
/// `header_len`. Two adjacent kept entities can be coalesced into a single
/// `&[u8]` slice — and SHA1 + memcpy of one big slice is much faster than of
/// many tiny ones (each `sha1::Sha1::update` call has fixed overhead).
fn coalesce_body_chunks(
    raws: &[&RawContainer],
    merged_entities: &[(RawEntityRow, usize)],
) -> Vec<(usize, usize, usize)> {
    let mut out: Vec<(usize, usize, usize)> = Vec::with_capacity(8);
    let mut cur: Option<(usize, usize, usize)> = None; // (c_idx, off, len)
    for (row, c_idx) in merged_entities {
        match cur {
            Some((ci, off, len))
                if ci == *c_idx && off + len == row.body_offset =>
            {
                cur = Some((ci, off, len + row.body_length));
            }
            Some(prev) => {
                out.push(prev);
                cur = Some((*c_idx, row.body_offset, row.body_length));
            }
            None => {
                cur = Some((*c_idx, row.body_offset, row.body_length));
            }
        }
    }
    if let Some(c) = cur {
        out.push(c);
    }
    // We don't actually need `raws` here for the math, but borrowing it
    // keeps the call site obvious and lets a future change (e.g. bounds
    // assertions per container) plug in here.
    let _ = raws;
    out
}

/// Write 20 raw SHA1 bytes as 40 lowercase hex characters.
fn write_hex_lower(src: &[u8; 20], out: &mut [u8; 40]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, &b) in src.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0x0f) as usize];
    }
}

// Suppress an unused-warning for SYM_DOLLAR_270 — kept for symmetry with the
// `$270` discussion in the module docs even though the constant isn't read
// (we emit $270 as a synthesized container_info struct, not an entity row).
#[allow(dead_code)]
const _: u32 = SYM_DOLLAR_270;
