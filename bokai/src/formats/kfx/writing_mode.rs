//! Book-level writing-mode derivation shared by every KFX reader.
//!
//! `document_data`'s `writing_mode` field states the book's axis and is what a
//! reader honours. [`majority_vertical_mode`] recovers an axis from the book's
//! own `$style` pool for the containers that omit the field.

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
/// every entity — keeps the cost bounded and matches what becomes CSS classes.
///
/// The majority is taken over **every** style, not over the styles that name a
/// mode. `horizontal_tb` is the CSS initial value, so a horizontal style
/// normally declares nothing at all; weighing vertical declarations against
/// only the explicit `horizontal_tb` ones lets a single vertical passage
/// outvote a book that is horizontal throughout.
pub fn majority_vertical_mode<'a>(
    styles: impl Iterator<Item = &'a IonValue>,
    symbols: &SymbolTable,
) -> Option<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut total = 0usize;
    for style in styles {
        total += 1;
        count_writing_modes(style, symbols, &mut counts);
    }
    let vrl = *counts.get("vertical-rl").unwrap_or(&0);
    let vlr = *counts.get("vertical-lr").unwrap_or(&0);
    let vertical = vrl + vlr;
    if vertical > 0 && vertical * 2 > total {
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

    /// A horizontal book carrying one vertical passage stays horizontal. Its
    /// horizontal styles declare no mode at all, so they are invisible to a
    /// tally of declarations and only the full style count reveals them.
    #[test]
    fn one_vertical_passage_does_not_carry_a_horizontal_book() {
        let symbols = SymbolTable::new(700, Vec::new());
        let mut styles = vec![style_with_mode(KfxSymbol::VerticalRl as u64)];
        styles.resize_with(45, || IonValue::Struct(Vec::new()));
        assert_eq!(majority_vertical_mode(styles.iter(), &symbols), None);
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
