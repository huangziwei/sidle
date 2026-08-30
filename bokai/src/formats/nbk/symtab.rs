//! Per-notebook symbol resolution.

use crate::formats::kfx::ion::{IonParser, IonValue};
use crate::formats::kfx::symbols::{KFX_SYMBOL_TABLE, symbol_name};

use super::NbkError;

// Ion field ids inside a $ion_symbol_table struct (Ion system symbols).
const F_IMPORTS: u64 = 6;
const F_SYMBOLS: u64 = 7;
const F_MAX_ID: u64 = 8;

pub struct SymTab {
    /// Id of the first local symbol.
    base: u64,
    /// Local symbol names, indexed by `id - base`.
    locals: Vec<String>,
}

impl SymTab {
    /// Build from the raw `$ion_symbol_table` fragment blob.
    pub fn from_fragment(blob: &[u8]) -> Result<SymTab, NbkError> {
        let value = IonParser::new(blob)
            .parse()
            .map_err(|e| NbkError::Format(format!("symbol table not Ion: {e}")))?;
        let st = value.unwrap_annotated();
        let fields = st
            .as_struct()
            .ok_or_else(|| NbkError::Format("$ion_symbol_table is not a struct".into()))?;

        let mut import_max: u64 = 0;
        if let Some(IonValue::List(imports)) = field(fields, F_IMPORTS) {
            for imp in imports {
                if let Some(m) = imp.get(F_MAX_ID).and_then(|v| v.as_int())
                    && m > 0
                {
                    import_max += m as u64;
                }
            }
        }
        // 1 (id 0 = invalid) + 9 Ion system symbols (ids 1..9) + imported symbols.
        let base = 1 + 9 + import_max;

        let mut locals = Vec::new();
        if let Some(IonValue::List(syms)) = field(fields, F_SYMBOLS) {
            for s in syms {
                locals.push(s.as_string().unwrap_or_default().to_string());
            }
        }

        Ok(SymTab { base, locals })
    }

    /// Resolve a symbol id to its name (base YJ table, then local symbols).
    /// The id→name counterpart of [`SymTab::id_of`]; the template layer uses it
    /// to follow a page's `nmdl.template_id` symbol to its fragment id.
    pub fn name(&self, id: u64) -> Option<&str> {
        if id < self.base {
            symbol_name(id)
        } else {
            self.locals
                .get((id - self.base) as usize)
                .map(|s| s.as_str())
        }
    }

    /// Resolve a name to its symbol id. Locals (e.g. `nmdl.*`) take precedence,
    /// then the base `KFX_SYMBOL_TABLE`.
    pub fn id_of(&self, name: &str) -> Option<u64> {
        if let Some(i) = self.locals.iter().position(|s| s == name) {
            return Some(self.base + i as u64);
        }
        KFX_SYMBOL_TABLE
            .iter()
            .position(|&s| s == name)
            .map(|i| i as u64)
    }
}

fn field(fields: &[(u64, IonValue)], id: u64) -> Option<&IonValue> {
    fields.iter().find(|(k, _)| *k == id).map(|(_, v)| v)
}
