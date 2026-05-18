//! KFX symbol catalogs (mirrors `yj_symbol_catalog.py`).
//!
//! Calibre keeps two shared symbol tables:
//!
//! - `$ion` (Ion 1.0 system symbols, 9 entries with their textual names).
//! - `YJ_symbols` (Amazon KFX domain symbols). Calibre's catalog names every
//!   entry literally as `$N` — `$10`, `$11`, ... — where N is the absolute
//!   symbol ID. Some entries carry a trailing `?` marking them as "unexpected
//!   if used"; the suffix is stripped on import, so the canonical form is
//!   always plain `$N`.
//!
//! Because the canonical YJ symbol name equals its ID as a string, we don't
//! need to materialize 825 individual strings — the symtab resolves YJ IDs
//! via a `$<id>` formatter and never has to walk a catalog vec.

pub const SYSTEM_SYMBOLS_NAME: &str = "$ion";
pub const SYSTEM_SYMBOLS_VERSION: u32 = 1;

/// Ion 1.0 system symbols, indexed by `id - 1` (so `SYSTEM_SYMBOLS[0]` is the
/// symbol for ID 1).
pub const SYSTEM_SYMBOLS: &[&str] = &[
    "$ion",
    "$ion_1_0",
    "$ion_symbol_table",
    "name",
    "version",
    "imports",
    "symbols",
    "max_id",
    "$ion_shared_symbol_table",
];

pub const YJ_SYMBOLS_NAME: &str = "YJ_symbols";
pub const YJ_SYMBOLS_VERSION: u32 = 10;

/// Number of entries calibre's YJ_symbols catalog defines. Counted from
/// `ref/calibre-kfx-input/kfxlib/yj_symbol_catalog.py`; matches what the
/// `KfxContainer.deserialize` import truncates against.
pub const YJ_SYMBOLS_LEN: usize = 825;
