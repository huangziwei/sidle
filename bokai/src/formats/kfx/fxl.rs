//! Fixed-layout signal derivation shared across the KFX family.
//!
//! Reads the book-level `content_features` ($585) entity for the
//! `yj_*fixed_layout` / `yj_double_page_spread` flags that switch a book onto
//! the image-based fixed-layout path (manga / comic / picture book), plus the
//! pixel-dimension reader used for per-page `fixed_width`/`fixed_height`
//! viewports. Mirrors calibre's `yj_to_epub` derivations so a book is
//! classified the way calibre classifies it.

use crate::formats::kfx::container::{SymbolTable, get_field};
use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::symbols::KfxSymbol;

/// Book-level fixed-layout signals. `fixed_layout` ⇒ image-based fixed layout
/// (each section's page template splits into per-page spine documents);
/// `double_page_spread` ⇒ a spread comic (drives `book-type: comic` and the
/// `page-spread-left`/`-right` pairing).
#[derive(Debug, Clone, Copy, Default)]
pub struct FxlFeatures {
    pub fixed_layout: bool,
    pub double_page_spread: bool,
}

/// Scan one `content_features` entity into `acc`. A book may carry several
/// such entities (one standalone, one nested in metadata) — call once per
/// entity and OR the results.
pub fn scan_content_features(entity: &IonValue, symbols: &SymbolTable, acc: &mut FxlFeatures) {
    let inner = entity.unwrap_annotated();
    let Some(fields) = inner.as_struct() else {
        return;
    };
    let Some(features) = get_field(fields, KfxSymbol::Features as u64).and_then(|x| x.as_list())
    else {
        return;
    };
    for feat in features {
        let Some(ff) = feat.unwrap_annotated().as_struct() else {
            continue;
        };
        // `key` is an IonString in Amazon KFX, but tolerate a symbol too.
        let key = match get_field(ff, KfxSymbol::Key as u64) {
            Some(v) => match v.unwrap_annotated() {
                IonValue::String(s) => s.clone(),
                other => symbols
                    .text_of(other)
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            },
            None => continue,
        };
        if key.contains("fixed_layout") {
            acc.fixed_layout = true;
        }
        if key == "yj_double_page_spread" {
            acc.double_page_spread = true;
        }
    }
}

/// Read a pixel dimension field that may be a bare int (`fixed_width: 900`) or
/// a `{value, unit}` length struct. Returns `None` when absent or non-positive.
pub fn read_px(fields: &[(u64, IonValue)], sym: KfxSymbol) -> Option<u32> {
    let v = get_field(fields, sym as u64)?;
    let inner = v.unwrap_annotated();
    if let Some(n) = inner.as_int() {
        return if n > 0 { Some(n as u32) } else { None };
    }
    if let Some(fs) = inner.as_struct()
        && let Some(n) = get_field(fs, KfxSymbol::Value as u64).and_then(|x| x.as_int())
    {
        return if n > 0 { Some(n as u32) } else { None };
    }
    None
}

/// Whether a page-template `layout` value names a facing-page spread container
/// (its story holds the per-page containers) rather than a leaf page.
pub fn is_spread_layout(layout: &str) -> bool {
    layout == "page_spread" || layout == "facing_page"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features_entity(keys: &[&str]) -> IonValue {
        let feats = keys
            .iter()
            .map(|k| {
                IonValue::Struct(vec![(
                    KfxSymbol::Key as u64,
                    IonValue::String(k.to_string()),
                )])
            })
            .collect();
        IonValue::Struct(vec![(KfxSymbol::Features as u64, IonValue::List(feats))])
    }

    #[test]
    fn detects_fixed_layout_and_spread_keys() {
        let symbols = SymbolTable::from_fragment(None);
        let mut acc = FxlFeatures::default();
        scan_content_features(
            &features_entity(&["yj_non_pdf_fixed_layout"]),
            &symbols,
            &mut acc,
        );
        assert!(acc.fixed_layout);
        assert!(!acc.double_page_spread);
        // Second entity ORs in the spread flag (metadata-nested copy).
        scan_content_features(
            &features_entity(&["yj_double_page_spread", "yj_fixed_layout"]),
            &symbols,
            &mut acc,
        );
        assert!(acc.fixed_layout && acc.double_page_spread);
    }

    #[test]
    fn read_px_accepts_bare_int_and_value_struct() {
        let fields = vec![
            (KfxSymbol::FixedWidth as u64, IonValue::Int(900)),
            (
                KfxSymbol::FixedHeight as u64,
                IonValue::Struct(vec![(KfxSymbol::Value as u64, IonValue::Int(1280))]),
            ),
            (KfxSymbol::Layout as u64, IonValue::Int(0)),
        ];
        assert_eq!(read_px(&fields, KfxSymbol::FixedWidth), Some(900));
        assert_eq!(read_px(&fields, KfxSymbol::FixedHeight), Some(1280));
        // Non-positive and absent → None.
        assert_eq!(read_px(&fields, KfxSymbol::Layout), None);
        assert_eq!(read_px(&fields, KfxSymbol::Width), None);
    }
}
