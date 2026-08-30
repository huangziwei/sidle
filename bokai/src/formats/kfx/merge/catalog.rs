//! KFX symbol catalogs (mirrors `yj_symbol_catalog.py`).
//!
//! Calibre keeps two shared symbol tables:

pub const SYSTEM_SYMBOLS_NAME: &str = "$ion";

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

#[cfg(test)]
pub const YJ_SYMBOLS_NAME: &str = "YJ_symbols";
#[cfg(test)]
pub const YJ_SYMBOLS_VERSION: u32 = 10;
