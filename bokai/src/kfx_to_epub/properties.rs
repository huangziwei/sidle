//! KFX style → CSS translation, `BookData`-side lookups.
//!
//! The conversion machinery (property table, value translation, layout-hint
//! extraction) lives in [`crate::kfx::yj_properties`], shared with the IR
//! route so both KFX→EPUB engines translate styles identically. This module
//! re-exports it and keeps the lookups that resolve a named `$157 style`
//! entity out of a loaded [`BookData`].

pub use crate::export::css::{CssDecl, safe_class_name};
pub use crate::kfx::yj_properties::*;

use super::loader::BookData;

/// Resolve a KFX style entity to its CSS declarations. Looks up the
/// `style_name` in `book.by_type[$157]`, walks the fields, and returns
/// a `CssDecl`.
pub fn style_decl_for(style_name: &str, book: &BookData) -> CssDecl {
    let Some(styles) = book
        .by_type
        .get(&(crate::kfx::symbols::KfxSymbol::Style as u64))
    else {
        return CssDecl::new();
    };
    let Some(value) = styles.get(style_name) else {
        return CssDecl::new();
    };
    let Some(fields) = value.unwrap_annotated().as_struct() else {
        return CssDecl::new();
    };
    convert_yj_properties(fields, &book.symbols)
}

/// Layout hints + heading level from a named `$style` entity (see
/// [`style_fields_layout_hints`] for the field semantics).
pub fn style_layout_hints_for(style_name: &str, book: &BookData) -> (Vec<String>, Option<String>) {
    use crate::kfx::symbols::KfxSymbol;
    let Some(styles) = book.by_type.get(&(KfxSymbol::Style as u64)) else {
        return (Vec::new(), None);
    };
    let Some(value) = styles.get(style_name) else {
        return (Vec::new(), None);
    };
    let Some(fields) = value.unwrap_annotated().as_struct() else {
        return (Vec::new(), None);
    };
    style_fields_layout_hints(fields, &book.symbols)
}
