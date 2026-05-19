//! Fragment representation (mirrors `yj_container.py`).
//!
//! A `YJFragment` is the unit calibre operates on between deserialize and
//! serialize. Each fragment has a *type* (e.g. `$258` metadata, `$417`
//! bcRawMedia) and a *fid* — a within-type identifier. Singletons (one-per-
//! book root types) have `fid == ftype`; non-singletons have a distinct
//! string fid.
//!
//! Calibre encodes the (fid, ftype) pair in IonAnnotation form on the wire
//! when the fragment is loose (e.g. book.ion text). Inside a KFX container,
//! the (fid, ftype) is carried by the entity-table row instead, and the
//! entity body is the bare value.

use super::node::IonNode;

#[derive(Debug, Clone)]
pub struct YJFragment {
    pub ftype: String,
    pub fid: String,
    pub value: IonNode,
}

impl YJFragment {
    pub fn singleton(ftype: impl Into<String>, value: IonNode) -> Self {
        let ftype: String = ftype.into();
        Self {
            fid: ftype.clone(),
            ftype,
            value,
        }
    }

    pub fn is_single(&self) -> bool {
        self.fid == self.ftype
    }
}

/// Calibre's `PREFERED_FRAGMENT_TYPE_ORDER`. Fragments are emitted in this
/// order; types not on the list sort to the end. Within a type, calibre
/// sorts by fid (lexicographic). Both keys feed into
/// `YJFragmentKey.sort_key`.
pub const PREFERED_FRAGMENT_TYPE_ORDER: &[&str] = &[
    "$ion_symbol_table",
    "$270",
    "$593",
    "$585",
    "$490",
    "$258",
    "$538",
    "$389",
    "$390",
    "$260",
    "$259",
    "$608",
    "$145",
    "$756",
    "$692",
    "$157",
    "$391",
    "$266",
    "$394",
    "$264",
    "$265",
    "$550",
    "$609",
    "$621",
    "$611",
    "$610",
    "$597",
    "$267",
    "$387",
    "$395",
    "$262",
    "$164",
    "$418",
    "$417",
    "$419",
];

pub const CONTAINER_FRAGMENT_TYPES: &[&str] = &["$270", "$593", "$ion_symbol_table", "$419"];

pub const RAW_FRAGMENT_TYPES: &[&str] = &["$418", "$417"];

pub fn is_container_fragment(ftype: &str) -> bool {
    CONTAINER_FRAGMENT_TYPES.contains(&ftype)
}

pub fn is_raw(ftype: &str) -> bool {
    RAW_FRAGMENT_TYPES.contains(&ftype)
}

pub fn fragment_type_order(ftype: &str) -> usize {
    PREFERED_FRAGMENT_TYPE_ORDER
        .iter()
        .position(|&s| s == ftype)
        .unwrap_or(PREFERED_FRAGMENT_TYPE_ORDER.len())
}

/// Calibre's `YJFragmentKey.sort_key` returns a tuple `(type_index, fid)`.
/// We reproduce that as `(usize, String)` for use with `sort_by_key`.
pub fn fragment_sort_key(frag: &YJFragment) -> (usize, String) {
    (fragment_type_order(&frag.ftype), frag.fid.clone())
}
