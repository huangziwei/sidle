//! Book-level writing-mode derivation shared by every KFX reader.
//!
//! A KFX `document_data`'s `writing_mode` field is an unreliable default: a
//! mixed book can report `horizontal_tb` while every text block is styled
//! vertical-rl (the doc-level field tracks the first/dominant section, not
//! the body). Consumers correct that default with the mode the book's own
//! `$style` pool predominantly declares — [`majority_vertical_mode`].

use std::collections::HashMap;

use super::container::SymbolTable;
use super::ion::IonValue;
use super::symbols::KfxSymbol;

/// Normalize a KFX writing-mode symbol name to its CSS spelling.
pub fn normalize_writing_mode(name: &str) -> &str {
    match name {
        "horizontal_tb" => "horizontal-tb",
        "vertical_rl" => "vertical-rl",
        "vertical_lr" => "vertical-lr",
        other => other,
    }
}

/// The vertical writing mode (`"vertical-rl"` / `"vertical-lr"`) the given
/// style values predominantly declare, or `None` when horizontal text
/// dominates (or no style declares a mode). Counting the style pool — not
/// every entity — keeps the cost bounded and matches what becomes CSS
/// classes; a book whose document-level mode is genuinely horizontal is left
/// alone unless its own styles say otherwise by majority.
pub fn majority_vertical_mode<'a>(
    styles: impl Iterator<Item = &'a IonValue>,
    symbols: &SymbolTable,
) -> Option<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for style in styles {
        count_writing_modes(style, symbols, &mut counts);
    }
    let vrl = *counts.get("vertical-rl").unwrap_or(&0);
    let vlr = *counts.get("vertical-lr").unwrap_or(&0);
    let htb = *counts.get("horizontal-tb").unwrap_or(&0);
    let vertical = vrl + vlr;
    if vertical > 0 && vertical > htb {
        Some(
            if vlr > vrl {
                "vertical-lr"
            } else {
                "vertical-rl"
            }
            .to_string(),
        )
    } else {
        None
    }
}

/// Tally every `writing_mode` ($560) symbol value reachable from `value`
/// (normalised to its CSS spelling) into `out`.
pub fn count_writing_modes(
    value: &IonValue,
    symbols: &SymbolTable,
    out: &mut HashMap<String, usize>,
) {
    match value.unwrap_annotated() {
        IonValue::Struct(fields) => {
            for (k, v) in fields {
                if *k == KfxSymbol::WritingMode as u64
                    && let Some(name) = symbols.text_of(v)
                {
                    *out.entry(normalize_writing_mode(name).to_string())
                        .or_insert(0) += 1;
                }
                count_writing_modes(v, symbols, out);
            }
        }
        IonValue::List(items) => {
            for item in items {
                count_writing_modes(item, symbols, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style_with_mode(mode_symbol: u64) -> IonValue {
        IonValue::Struct(vec![(
            KfxSymbol::WritingMode as u64,
            IonValue::Symbol(mode_symbol),
        )])
    }

    #[test]
    fn majority_vertical_wins_over_horizontal() {
        let symbols = SymbolTable::new(700, Vec::new());
        let styles = [
            style_with_mode(KfxSymbol::VerticalRl as u64),
            style_with_mode(KfxSymbol::VerticalRl as u64),
            style_with_mode(KfxSymbol::HorizontalTb as u64),
        ];
        assert_eq!(
            majority_vertical_mode(styles.iter(), &symbols),
            Some("vertical-rl".to_string())
        );
    }

    #[test]
    fn horizontal_majority_yields_none() {
        let symbols = SymbolTable::new(700, Vec::new());
        let styles = [
            style_with_mode(KfxSymbol::VerticalRl as u64),
            style_with_mode(KfxSymbol::HorizontalTb as u64),
            style_with_mode(KfxSymbol::HorizontalTb as u64),
        ];
        assert_eq!(majority_vertical_mode(styles.iter(), &symbols), None);
        assert_eq!(majority_vertical_mode(std::iter::empty(), &symbols), None);
    }

    #[test]
    fn counts_nested_modes() {
        let symbols = SymbolTable::new(700, Vec::new());
        let nested = IonValue::List(vec![IonValue::Struct(vec![(
            0u64,
            style_with_mode(KfxSymbol::VerticalLr as u64),
        )])]);
        let mut counts = HashMap::new();
        count_writing_modes(&nested, &symbols, &mut counts);
        assert_eq!(counts.get("vertical-lr"), Some(&1));
    }
}
