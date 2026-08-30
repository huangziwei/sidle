//! Local Ion symbol table (mirrors `ion_symbol_table.py::LocalSymbolTable`).

use std::collections::HashMap;

use super::catalog::{SYSTEM_SYMBOLS, SYSTEM_SYMBOLS_NAME};

#[derive(Debug, Clone)]
pub struct SymbolTableImport {
    pub name: String,
    pub version: u32,
    pub max_id: u32,
}

pub struct LocalSymbolTable {
    table_imports: Vec<SymbolTableImport>,
    local_symbols: Vec<String>,
    name_to_id: HashMap<String, u32>,
    /// IDs 10..local_min_id are the YJ catalog window; canonical name is
    /// `"$<id>"`. We don't store them, but we do store local_min_id.
    local_min_id: u32,
}

impl Default for LocalSymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalSymbolTable {
    pub fn new() -> Self {
        let mut t = Self {
            table_imports: Vec::new(),
            local_symbols: Vec::new(),
            name_to_id: HashMap::new(),
            local_min_id: 0,
        };
        t.clear();
        t
    }

    /// Mirrors calibre's `LocalSymbolTable.clear`: reset state and re-import
    /// the SYSTEM table only. Subsequent `import_shared_symbol_table` calls
    /// extend the import list.
    pub fn clear(&mut self) {
        self.table_imports.clear();
        self.local_symbols.clear();
        self.name_to_id.clear();

        for (i, &name) in SYSTEM_SYMBOLS.iter().enumerate() {
            let id = (i as u32) + 1;
            self.name_to_id.insert(name.to_string(), id);
        }
        self.local_min_id = (SYSTEM_SYMBOLS.len() as u32) + 1; // 10
    }

    /// Append a shared-table import. `max_id` is the number of symbols imported
    /// from this catalog (after calibre's `-= SYSTEM_SYMBOLS.len()` adjustment).
    pub fn import_shared_symbol_table(&mut self, name: &str, version: u32, max_id: u32) {
        if name == SYSTEM_SYMBOLS_NAME {
            return;
        }
        self.table_imports.push(SymbolTableImport {
            name: name.to_string(),
            version,
            max_id,
        });
        // YJ_symbols (and any other shared table imported after SYSTEM): the
        // canonical names are `$<id>` for the absolute id, so we don't add
        // them to `name_to_id` (resolved via the regex path in `get_id`).
        self.local_min_id += max_id;
    }

    /// Add a single local symbol. Returns its assigned ID. Duplicates re-use
    /// the existing ID (matching calibre's create_local_symbol).
    pub fn create_local_symbol(&mut self, symbol: &str) -> u32 {
        if let Some(&id) = self.name_to_id.get(symbol) {
            return id;
        }
        let id = self.local_min_id + (self.local_symbols.len() as u32);
        self.local_symbols.push(symbol.to_string());
        self.name_to_id.insert(symbol.to_string(), id);
        id
    }

    /// Replace the entire local-symbols list. Mirrors calibre's
    /// `replace_local_symbols(sorted(...))` step at the end of
    /// `check_symbol_table`.
    pub fn replace_local_symbols(&mut self, new_symbols: Vec<String>) {
        // Remove old local entries from name_to_id.
        for old in &self.local_symbols {
            self.name_to_id.remove(old);
        }
        self.local_symbols = new_symbols;
        // Re-index.
        for (i, s) in self.local_symbols.iter().enumerate() {
            let id = self.local_min_id + (i as u32);
            self.name_to_id.insert(s.clone(), id);
        }
    }

    /// Calibre's `LocalSymbolTable.create`: when a `$ion_symbol_table`
    /// fragment is encountered, reset the symtab and re-import per its
    /// `imports` field, then append its `symbols`.
    pub fn create(&mut self, imports: &[SymbolTableImport], symbols: &[String]) {
        self.clear();
        for imp in imports {
            self.import_shared_symbol_table(&imp.name, imp.version, imp.max_id);
        }
        for s in symbols {
            self.create_local_symbol(s);
        }
    }

    pub fn local_min_id(&self) -> u32 {
        self.local_min_id
    }

    pub fn local_symbols(&self) -> &[String] {
        &self.local_symbols
    }

    pub fn table_imports(&self) -> &[SymbolTableImport] {
        &self.table_imports
    }

    /// Look up an ID by its symbol name. Mirrors calibre's
    /// `LocalSymbolTable.get_id`. Returns 0 for unknown names (calibre uses
    /// 0 as the "undefined" sentinel).
    pub fn get_id(&self, name: &str) -> u32 {
        if let Some(rest) = name.strip_prefix('$')
            && !rest.is_empty()
            && rest.bytes().all(|b| b.is_ascii_digit())
            && let Ok(id) = rest.parse::<u32>()
        {
            return id;
        }
        self.name_to_id.get(name).copied().unwrap_or(0)
    }

    /// Look up the canonical name for a symbol ID. Mirrors calibre's
    /// `LocalSymbolTable.get_symbol` (returns `$<id>` for any unknown ID).
    pub fn get_symbol(&self, id: u32) -> String {
        if id == 0 {
            return "$0".to_string();
        }
        let idx = (id as usize) - 1;
        if idx < SYSTEM_SYMBOLS.len() {
            return SYSTEM_SYMBOLS[idx].to_string();
        }
        if id < self.local_min_id {
            return format!("${}", id);
        }
        let local_idx = (id - self.local_min_id) as usize;
        match self.local_symbols.get(local_idx) {
            Some(s) => s.clone(),
            None => format!("${}", id),
        }
    }

    /// Total number of symbols (SYSTEM + YJ + local). Equivalent to calibre's
    /// `len(self.symbols)`.
    pub fn total_count(&self) -> u32 {
        self.local_min_id - 1 + (self.local_symbols.len() as u32)
    }
}

/// Constants for callers that need to know the SYSTEM table size etc.
pub const SYSTEM_SIZE: u32 = SYSTEM_SYMBOLS.len() as u32; // 9

#[cfg(test)]
mod tests {
    use super::super::catalog::{YJ_SYMBOLS_NAME, YJ_SYMBOLS_VERSION};
    use super::*;

    #[test]
    fn fresh_symtab_resolves_system() {
        let t = LocalSymbolTable::new();
        assert_eq!(t.get_symbol(1), "$ion");
        assert_eq!(t.get_symbol(3), "$ion_symbol_table");
        assert_eq!(t.get_id("$ion_symbol_table"), 3);
        assert_eq!(t.local_min_id(), 10);
    }

    #[test]
    fn yj_import_extends_local_min() {
        let mut t = LocalSymbolTable::new();
        t.import_shared_symbol_table(YJ_SYMBOLS_NAME, YJ_SYMBOLS_VERSION, 823);
        assert_eq!(t.local_min_id(), 833);
        // YJ symbols resolve as $<id> without any HashMap entry.
        assert_eq!(t.get_symbol(258), "$258");
        assert_eq!(t.get_id("$258"), 258);
    }

    #[test]
    fn local_symbols_get_assigned_ids() {
        let mut t = LocalSymbolTable::new();
        t.import_shared_symbol_table(YJ_SYMBOLS_NAME, YJ_SYMBOLS_VERSION, 823);
        let a = t.create_local_symbol("hello");
        let b = t.create_local_symbol("world");
        assert_eq!(a, 833);
        assert_eq!(b, 834);
        assert_eq!(t.get_symbol(833), "hello");
        assert_eq!(t.get_id("hello"), 833);
    }

    #[test]
    fn create_local_dedupes() {
        let mut t = LocalSymbolTable::new();
        let a = t.create_local_symbol("foo");
        let b = t.create_local_symbol("foo");
        assert_eq!(a, b);
    }

    #[test]
    fn replace_local_reindexes() {
        let mut t = LocalSymbolTable::new();
        t.import_shared_symbol_table(YJ_SYMBOLS_NAME, YJ_SYMBOLS_VERSION, 823);
        t.create_local_symbol("foo");
        t.create_local_symbol("bar");
        t.replace_local_symbols(vec!["x".into(), "y".into(), "z".into()]);
        assert_eq!(t.get_id("x"), 833);
        assert_eq!(t.get_id("z"), 835);
        assert_eq!(t.get_id("foo"), 0); // removed
    }
}
