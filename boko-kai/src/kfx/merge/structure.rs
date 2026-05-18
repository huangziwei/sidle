//! Fragment-list rebuild + symtab GC (mirrors a subset of `yj_structure.py`).
//!
//! For the kfx-zip → kfx mechanical port we need:
//!
//! 1. **Consolidate `$270` fragments** — every source container contributes a
//!    `$270` (container_info). Calibre's `check_fragment_usage(rebuild=True)`
//!    drops them all and emits a single fresh `$270` for the merged container.
//! 2. **Rebuild `$419` (container_entity_map)** — the on-wire entity list and
//!    dependency map for the merged container.
//! 3. **Sort fragments** by `PREFERED_FRAGMENT_TYPE_ORDER`.
//! 4. **GC the symtab** — keep only locally-defined symbols that are actually
//!    referenced by a non-container fragment, sorted by `natural_sort_key`.
//!
//! Steps 1–3 mirror `check_fragment_usage`; step 4 mirrors
//! `check_symbol_table`. Walk traversal (`walk_fragment`) is reused by 2 + 4.

use std::collections::HashSet;

use super::container::serialize_container;
use super::fragment::{fragment_sort_key, is_container_fragment, YJFragment};
use super::node::IonNode;
use super::symtab::LocalSymbolTable;

/// Drives the per-fragment walker. Mirrors calibre's `walk_fragment` for the
/// subset of behavior we need: gather all `IonSymbol` references that appear
/// inside any value (struct keys, struct values, list/sexp items,
/// annotations), plus a string-key reference for `$165` (file location, which
/// calibre escalates from `IonString` to `IonSymbol`).
fn collect_symbol_references(fragment: &YJFragment, into: &mut HashSet<String>) {
    walk_node(&fragment.value, into);
    // calibre also adds the ftype itself when used in fragment.is_root.
    into.insert(fragment.ftype.clone());
    if !fragment.is_single() {
        into.insert(fragment.fid.clone());
    }
}

fn walk_node(node: &IonNode, into: &mut HashSet<String>) {
    match node {
        IonNode::Symbol(s) => {
            into.insert(s.clone());
        }
        IonNode::List(items) => {
            for it in items {
                walk_node(it, into);
            }
        }
        IonNode::Struct(fields) => {
            for (k, v) in fields {
                into.insert(k.clone());
                walk_node(v, into);
            }
        }
        IonNode::Annotated(anns, inner) => {
            for a in anns {
                into.insert(a.clone());
            }
            walk_node(inner, into);
        }
        _ => {}
    }
}

/// Walk every IonSymbol reachable from `node` and call `cb` for each.
fn for_each_symbol(node: &IonNode, cb: &mut impl FnMut(&str)) {
    match node {
        IonNode::Symbol(s) => cb(s),
        IonNode::List(items) => {
            for it in items {
                for_each_symbol(it, cb);
            }
        }
        IonNode::Struct(fields) => {
            for (_, v) in fields {
                for_each_symbol(v, cb);
            }
        }
        IonNode::Annotated(_, inner) => for_each_symbol(inner, cb),
        _ => {}
    }
}

/// Build calibre's `entity_dependencies` list (`$253` payload).
///
/// For each `$260` (section) fragment we list every `$164` (external_resource)
/// it references — transitively any `$417` (bcRawMedia) the resource itself
/// names. For each `$164` we list its `$417`(s). This is a generic walk: we
/// look at every IonSymbol inside the fragment value and check whether it
/// matches the fid of a known `$164` or `$417` fragment.
pub fn determine_entity_dependencies(fragments: &[YJFragment]) -> Vec<IonNode> {
    use std::collections::{BTreeMap, BTreeSet};

    // Index: fid -> ftype, for the two target types we care about.
    let mut fid_to_ftype: BTreeMap<String, String> = BTreeMap::new();
    for f in fragments {
        if (f.ftype == "$164" || f.ftype == "$417") && f.fid != f.ftype {
            fid_to_ftype.insert(f.fid.clone(), f.ftype.clone());
        }
    }

    // 164_refs_per_164: fid of $164 -> set of $417 fids it references.
    let mut refs_164: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    // 260_refs: fid of $260 -> set of $164 fids it references.
    let mut refs_260: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for f in fragments {
        if f.ftype == "$164" {
            let entry = refs_164.entry(f.fid.clone()).or_default();
            for_each_symbol(&f.value, &mut |sym| {
                if fid_to_ftype.get(sym).map(|s| s.as_str()) == Some("$417") {
                    entry.insert(sym.to_string());
                }
            });
        } else if f.ftype == "$260" {
            let entry = refs_260.entry(f.fid.clone()).or_default();
            for_each_symbol(&f.value, &mut |sym| {
                if fid_to_ftype.get(sym).map(|s| s.as_str()) == Some("$164") {
                    entry.insert(sym.to_string());
                }
            });
        }
    }

    let mut entries: Vec<(String, Vec<String>)> = Vec::new();

    // Calibre's order: walk `sorted(deep_references)` — that's YJFragmentKey
    // sort order (preferred type first, then fid). So $260 entries come
    // before $164 entries.
    let mut sec_keys: Vec<&String> = refs_260.keys().collect();
    sec_keys.sort();
    for k in sec_keys {
        let v: Vec<String> = refs_260[k].iter().cloned().collect();
        if !v.is_empty() {
            entries.push((k.clone(), v));
        }
    }
    let mut res_keys: Vec<&String> = refs_164.keys().collect();
    res_keys.sort();
    for k in res_keys {
        let mut v: Vec<String> = refs_164[k].iter().cloned().collect();
        v.sort();
        if !v.is_empty() {
            entries.push((k.clone(), v));
        }
    }

    let mut out = Vec::new();
    for (src_fid, mand) in entries {
        out.push(IonNode::Struct(vec![
            ("$155".into(), IonNode::Symbol(src_fid)),
            (
                "$254".into(),
                IonNode::List(mand.into_iter().map(IonNode::Symbol).collect()),
            ),
        ]));
    }
    out
}

/// Consolidates fragments, rebuilds `$270` and `$419`, sorts the list.
/// Mirrors calibre's `check_fragment_usage(rebuild=True)`.
pub fn rebuild_fragments_and_container_map(
    fragments: Vec<YJFragment>,
    container_id: String,
    kfxgen_app_version: String,
    kfxgen_pkg_version: String,
    version: i64,
) -> Vec<YJFragment> {
    let mut new_fragments: Vec<YJFragment> = Vec::with_capacity(fragments.len());
    let mut existing_entity_deps: Option<IonNode> = None;

    for f in fragments {
        match f.ftype.as_str() {
            "$270" => {} // drop — emitted fresh below.
            "$419" => {
                if let Some(deps) = f.value.get_field("$253") {
                    existing_entity_deps = Some(deps.clone());
                }
            }
            _ => new_fragments.push(f),
        }
    }

    // Fresh $270 (KFX main container).
    let cinfo = IonNode::Struct(vec![
        ("$409".into(), IonNode::String(container_id.clone())),
        ("$161".into(), IonNode::String("KFX main".into())),
        ("$587".into(), IonNode::String(kfxgen_app_version)),
        ("$588".into(), IonNode::String(kfxgen_pkg_version)),
        ("version".into(), IonNode::Int(version)),
    ]);
    new_fragments.push(YJFragment::singleton("$270", cinfo));

    // Sort BEFORE collecting entity_fids — calibre's `rebuild_container_entity_map`
    // walks the already-sorted fragment list and pushes fids in that order.
    new_fragments.sort_by_key(fragment_sort_key);

    let mut entity_fids: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for f in &new_fragments {
        if is_container_fragment(&f.ftype) {
            continue;
        }
        if f.fid == f.ftype {
            continue; // singleton, contributes id_idnum=$348 not a real fid.
        }
        if seen.insert(f.fid.clone()) {
            entity_fids.push(f.fid.clone());
        }
    }

    // Prefer the source's $253 verbatim when available — KFXGEN already
    // populated it with the transitive `$260 → $164 → $417` graph (the same
    // graph calibre's `walk_fragment` would rebuild). Falling back to a
    // computed list only when no source $419 was present keeps us closer to
    // wire fidelity without needing to port the full reachability walker.
    let deps = if let Some(src) = existing_entity_deps {
        Some(src)
    } else {
        let computed = determine_entity_dependencies(&new_fragments);
        if !computed.is_empty() {
            Some(IonNode::List(computed))
        } else {
            None
        }
    };

    let mut cem_fields: Vec<(String, IonNode)> = Vec::new();
    let container_contents = IonNode::Struct(vec![
        ("$155".into(), IonNode::String(container_id)),
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
    cem_fields.push(("$252".into(), IonNode::List(vec![container_contents])));
    if let Some(deps_list) = deps {
        cem_fields.push(("$253".into(), deps_list));
    }
    if !entity_fids.is_empty() {
        new_fragments.push(YJFragment::singleton(
            "$419",
            IonNode::Struct(cem_fields),
        ));
    }

    new_fragments
}

/// Calibre's `natural_sort_key`: split on digit runs, lower-case each chunk,
/// and zero-pad numeric chunks to 8 digits so `"a2"` < `"a10"`.
pub fn natural_sort_key(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::with_capacity(lower.len() + 8);
    let mut cur = String::new();
    let mut cur_is_digit: Option<bool> = None;
    for c in lower.chars() {
        let d = c.is_ascii_digit();
        if cur_is_digit == Some(d) {
            cur.push(c);
        } else {
            flush_natural_chunk(&cur, cur_is_digit, &mut out);
            cur.clear();
            cur.push(c);
            cur_is_digit = Some(d);
        }
    }
    flush_natural_chunk(&cur, cur_is_digit, &mut out);
    out
}

fn flush_natural_chunk(chunk: &str, is_digit: Option<bool>, out: &mut String) {
    if chunk.is_empty() {
        return;
    }
    if is_digit == Some(true) {
        let pad = 8usize.saturating_sub(chunk.len());
        for _ in 0..pad {
            out.push('0');
        }
        out.push_str(chunk);
    } else {
        out.push_str(chunk);
    }
}

/// Mirrors `check_symbol_table(rebuild=True)`: collect every symbol used by
/// any non-container fragment, filter to those that resolve to local IDs,
/// sort by `natural_sort_key`, and `replace_local_symbols` on the symtab. The
/// `$ion_symbol_table` fragment is rebuilt to match.
pub fn rebuild_symbol_table(fragments: &mut Vec<YJFragment>, symtab: &mut LocalSymbolTable) {
    let mut used: HashSet<String> = HashSet::new();
    for f in fragments.iter() {
        if is_container_fragment(&f.ftype) {
            continue;
        }
        collect_symbol_references(f, &mut used);
    }

    // Filter to local symbols: those whose current ID is >= local_min_id.
    let mut book_symbols: Vec<String> = used
        .into_iter()
        .filter(|s| symtab.get_id(s) >= symtab.local_min_id())
        // calibre's filter also excludes `$N` literal forms (those are YJ
        // catalog entries unknown to our truncated import — they shouldn't
        // appear as fresh locals). Drop them here too.
        .filter(|s| !is_canonical_id_form(s))
        .collect();

    book_symbols.sort_by(|a, b| natural_sort_key(a).cmp(&natural_sort_key(b)));

    symtab.replace_local_symbols(book_symbols.clone());

    // Rebuild the `$ion_symbol_table` fragment to match the new symtab.
    let imports: Vec<IonNode> = symtab
        .table_imports()
        .iter()
        .map(|imp| {
            IonNode::Struct(vec![
                ("name".into(), IonNode::String(imp.name.clone())),
                ("version".into(), IonNode::Int(imp.version as i64)),
                ("max_id".into(), IonNode::Int(imp.max_id as i64)),
            ])
        })
        .collect();

    let symbols_list: Vec<IonNode> = symtab
        .local_symbols()
        .iter()
        .map(|s| IonNode::String(s.clone()))
        .collect();

    let symtab_value = IonNode::Struct(vec![
        (
            "max_id".into(),
            IonNode::Int(symtab.total_count() as i64),
        ),
        ("imports".into(), IonNode::List(imports)),
        ("symbols".into(), IonNode::List(symbols_list)),
    ]);

    // Drop any existing $ion_symbol_table fragment, then insert at index 0.
    fragments.retain(|f| f.ftype != "$ion_symbol_table");
    fragments.insert(
        0,
        YJFragment::singleton("$ion_symbol_table", symtab_value),
    );
}

fn is_canonical_id_form(s: &str) -> bool {
    if let Some(rest) = s.strip_prefix('$') {
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
    } else {
        false
    }
}

/// Top-level pipeline gate: take the post-rebuild fragments + symtab and
/// emit the merged `.kfx` bytes.
pub fn finalize(fragments: &[YJFragment], symtab: &LocalSymbolTable) -> Vec<u8> {
    serialize_container(fragments, symtab)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_key_orders_numerics() {
        let mut v = vec!["a10".to_string(), "a2".to_string(), "a1".to_string()];
        v.sort_by(|a, b| natural_sort_key(a).cmp(&natural_sort_key(b)));
        assert_eq!(v, vec!["a1", "a2", "a10"]);
    }

    #[test]
    fn canonical_id_form_detected() {
        assert!(is_canonical_id_form("$165"));
        assert!(!is_canonical_id_form("$abc"));
        assert!(!is_canonical_id_form("hello"));
    }
}
