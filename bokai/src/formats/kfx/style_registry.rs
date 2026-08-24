//! Style registry for KFX export.
//!
//! Handles style deduplication and ID assignment during the two-pass export:
//! - Pass 1: Collect unique style combinations, assign IDs via hashing
//! - Pass 2: Emit style fragment with all definitions, reference by ID in content

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::formats::kfx::ion::IonValue;
use crate::formats::kfx::style_schema::{KfxValue, StyleContext, StyleSchema, extract_ir_field};
use crate::formats::kfx::symbols::KfxSymbol;
use crate::style as ir_style;

// ============================================================================
// Computed Style
// ============================================================================

/// A set of resolved KFX property values, hashed for deduplication: identical
/// property sets share one style ID.
#[derive(Debug, Clone, Default)]
pub struct ComputedStyle {
    /// Resolved properties: (KfxSymbol, KfxValue)
    properties: Vec<(KfxSymbol, KfxValue)>,
}

impl ComputedStyle {
    /// Create an empty computed style.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a property to this style.
    pub fn set(&mut self, symbol: KfxSymbol, value: KfxValue) {
        // Remove any existing value for this symbol
        self.properties.retain(|(s, _)| *s != symbol);
        self.properties.push((symbol, value));
    }

    /// Get a property value.
    pub fn get(&self, symbol: KfxSymbol) -> Option<&KfxValue> {
        self.properties
            .iter()
            .find(|(s, _)| *s == symbol)
            .map(|(_, v)| v)
    }

    /// Check if the style is empty.
    pub fn is_empty(&self) -> bool {
        self.properties.is_empty()
    }

    /// Get the number of properties.
    pub fn len(&self) -> usize {
        self.properties.len()
    }

    /// Iterate over properties.
    pub fn iter(&self) -> impl Iterator<Item = &(KfxSymbol, KfxValue)> {
        self.properties.iter()
    }

    /// Check if this style contains any block-only properties.
    pub fn has_block_properties(&self, schema: &StyleSchema) -> bool {
        for (symbol, _) in &self.properties {
            // Find the rule for this symbol
            for rule in schema.rules() {
                if rule.kfx_symbol == *symbol && rule.context == StyleContext::BlockOnly {
                    return true;
                }
            }
        }
        false
    }

    /// Check if this style contains any inline-safe properties.
    pub fn has_inline_properties(&self, schema: &StyleSchema) -> bool {
        for (symbol, _) in &self.properties {
            for rule in schema.rules() {
                if rule.kfx_symbol == *symbol && rule.context == StyleContext::InlineSafe {
                    return true;
                }
            }
        }
        false
    }

    /// Split into `(block_style, inline_style)`, each holding the properties
    /// its `StyleContext` admits.
    pub fn split_by_context(&self, schema: &StyleSchema) -> (ComputedStyle, ComputedStyle) {
        let mut block = ComputedStyle::new();
        let mut inline = ComputedStyle::new();

        for (symbol, value) in &self.properties {
            let mut found_context = None;
            for rule in schema.rules() {
                if rule.kfx_symbol == *symbol {
                    found_context = Some(rule.context);
                    break;
                }
            }

            match found_context {
                Some(StyleContext::BlockOnly) => block.set(*symbol, value.clone()),
                Some(StyleContext::InlineSafe) => inline.set(*symbol, value.clone()),
                Some(StyleContext::Any) | None => {
                    // Properties with Any context go to both (or default to block)
                    block.set(*symbol, value.clone());
                }
            }
        }

        (block, inline)
    }

    /// Compute a hash for this style (for deduplication).
    pub fn compute_hash(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;

        // Sort properties for consistent hashing
        let mut sorted: Vec<_> = self.properties.clone();
        sorted.sort_by_key(|(s, _)| *s as u64);

        let mut hasher = DefaultHasher::new();
        for (symbol, value) in &sorted {
            (*symbol as u64).hash(&mut hasher);
            hash_kfx_value(value, &mut hasher);
        }
        hasher.finish()
    }

    /// Convert to KFX Ion struct for the style entity.
    pub fn to_ion(&self, style_name_symbol: u64) -> IonValue {
        let mut fields = Vec::new();

        // style_name field first
        fields.push((
            KfxSymbol::StyleName as u64,
            IonValue::Symbol(style_name_symbol),
        ));

        // Add all properties
        for (symbol, value) in &self.properties {
            fields.push((*symbol as u64, value.to_ion()));
        }

        IonValue::Struct(fields)
    }
}

/// Hash a KfxValue for style deduplication.
fn hash_kfx_value<H: Hasher>(value: &KfxValue, hasher: &mut H) {
    // Discriminant first
    std::mem::discriminant(value).hash(hasher);
    match value {
        KfxValue::Symbol(s) => (*s as u64).hash(hasher),
        KfxValue::SymbolId(id) => id.hash(hasher),
        KfxValue::Integer(n) => n.hash(hasher),
        KfxValue::Float(f) => f.to_bits().hash(hasher),
        KfxValue::String(s) => s.hash(hasher),
        KfxValue::Bool(b) => b.hash(hasher),
        KfxValue::Null => 0u8.hash(hasher),
        KfxValue::Dimensioned { value, unit } => {
            value.to_bits().hash(hasher);
            (*unit as u64).hash(hasher);
        }
        KfxValue::StructField { field, value } => {
            (*field as u64).hash(hasher);
            value.hash(hasher);
        }
    }
}

// ============================================================================
// Style Registry
// ============================================================================

/// Registry for collecting and deduplicating styles during export.
pub struct StyleRegistry {
    /// Hash -> (style_id, style_name_symbol, name_string, computed_style, uses).
    /// `name_string` is a hint's source class name or `format!("s{:X}", style_id)`;
    /// `uses` counts the `register_with_hint` calls that hit the entry.
    styles: HashMap<u64, (u64, u64, String, ComputedStyle, u64)>,

    /// Source-class names held as style symbols. The first registration keeps
    /// the name; a later `ComputedStyle` under the same hint takes `s<N>`.
    taken_names: std::collections::HashSet<String>,

    /// Next style ID to assign
    next_style_id: u64,

    /// The default style ID (for elements without specific styles)
    default_style_id: u64,

    /// Default style name symbol
    default_style_symbol: u64,

    /// Set by [`Self::cite_default`]. [`Self::drain_to_ion`] emits the `s0`
    /// fragment only for a book some element resolves to it.
    default_cited: bool,
}

impl StyleRegistry {
    /// Create a new style registry. `default_style_symbol` is the symbol id
    /// for `"s0"`, interned ahead of this call.
    pub fn new(default_style_symbol: u64) -> Self {
        let mut taken_names = std::collections::HashSet::new();
        // `"s0"` names the default style; `next_style_id` opens at 1.
        taken_names.insert("s0".to_string());
        Self {
            styles: HashMap::new(),
            taken_names,
            next_style_id: 1, // Start at 1, 0 is default
            default_style_id: 0,
            default_style_symbol,
            default_cited: false,
        }
    }

    /// Record that an element resolves to the default style.
    pub fn cite_default(&mut self) {
        self.default_cited = true;
    }

    /// Get the default style ID.
    pub fn default_style_id(&self) -> u64 {
        self.default_style_id
    }

    /// Get the default style symbol.
    pub fn default_style_symbol(&self) -> u64 {
        self.default_style_symbol
    }

    /// Register a computed style and get its symbol. An identical style
    /// returns the symbol it holds; a new one takes the name `s<N>`.
    pub fn register(
        &mut self,
        style: ComputedStyle,
        symbols: &mut crate::formats::kfx::context::SymbolTable,
    ) -> u64 {
        self.register_with_hint(style, None, symbols)
    }

    /// Register a computed style under `class_hint`. A hint `usable_class_hint`
    /// accepts and `taken_names` leaves free becomes the style symbol. Dedup
    /// keys on `(computed_style, usable_hint)`: one fragment per class name.
    pub fn register_with_hint(
        &mut self,
        style: ComputedStyle,
        class_hint: Option<&str>,
        symbols: &mut crate::formats::kfx::context::SymbolTable,
    ) -> u64 {
        if style.is_empty() {
            self.default_cited = true;
            return self.default_style_symbol;
        }

        let usable = class_hint.and_then(usable_class_hint);
        let mut hash = style.compute_hash();
        if let Some(ref name) = usable {
            // Mix the hint into the hash so distinct class names get distinct
            // entries even when their computed styles are byte-identical.
            use std::collections::hash_map::DefaultHasher;
            use std::hash::Hasher;
            let mut h = DefaultHasher::new();
            h.write_u64(hash);
            h.write_u8(0xFF);
            h.write(name.as_bytes());
            hash = h.finish();
        }

        if let Some(entry) = self.styles.get_mut(&hash) {
            entry.4 = entry.4.saturating_add(1);
            return entry.1;
        }

        let style_id = self.next_style_id;
        self.next_style_id += 1;

        let synthesized = format!("s{:X}", style_id);
        let style_name = usable
            .filter(|n| !self.taken_names.contains(n))
            .unwrap_or(synthesized);

        let name_symbol = symbols.get_or_intern(&style_name);
        self.taken_names.insert(style_name.clone());
        self.styles
            .insert(hash, (style_id, name_symbol, style_name, style, 1));

        name_symbol
    }

    /// Get the number of unique styles.
    pub fn len(&self) -> usize {
        self.styles.len()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.styles.is_empty()
    }

    /// Drain all styles into `(style_name, IonValue)` pairs. `style_name` is
    /// the symbol attached to the style: a source class name or `s<N>`.
    pub fn drain_to_ion(&mut self, language: &str) -> Vec<(String, IonValue)> {
        let mut result = Vec::new();

        // The default style carries `language`, which drives CJK font and
        // orientation selection. A book whose every element carries a style of
        // its own resolves to it nowhere, and ships no `s0` fragment.
        if self.default_cited {
            let mut default_fields = vec![(
                KfxSymbol::StyleName as u64,
                IonValue::Symbol(self.default_style_symbol),
            )];
            if !language.is_empty() {
                default_fields.push((
                    KfxSymbol::Language as u64,
                    IonValue::String(language.to_string()),
                ));
            }
            result.push(("s0".to_string(), IonValue::Struct(default_fields)));
        }

        // The `sort_by_key` on `style_id` fixes registration order: `styles`
        // drains in the map's order, and these become entities in container
        // order.
        let mut drained: Vec<_> = self.styles.drain().map(|(_, entry)| entry).collect();
        drained.sort_by_key(|(style_id, _, _, _, _)| *style_id);
        for (_style_id, name_symbol, name, style, _uses) in drained {
            let mut ion = style.to_ion(name_symbol);
            if !language.is_empty()
                && let IonValue::Struct(fields) = &mut ion
            {
                fields.push((
                    KfxSymbol::Language as u64,
                    IonValue::String(language.to_string()),
                ));
            }
            result.push((name, ion));
        }

        result
    }

    /// Get all styles without draining.
    pub fn styles(&self) -> impl Iterator<Item = (&u64, &ComputedStyle)> {
        self.styles.values().map(|(id, _, _, style, _)| (id, style))
    }

    /// Normalise per-paragraph `line_height` so the dominant em value becomes
    /// `1.0 lh` and the rest carry proportional `lh` ratios. Dominant is the
    /// largest summed `uses`. Returns that em value when any were normalised.
    pub fn normalize_line_heights_to_lh(&mut self) -> Option<f32> {
        // Pass 1: tally em-based line-heights weighted by usage.
        let mut tally: HashMap<u32, u64> = HashMap::new();
        for (_, _, _, style, uses) in self.styles.values() {
            if let Some(KfxValue::Dimensioned { value, unit }) = style.get(KfxSymbol::LineHeight)
                && matches!(unit, KfxSymbol::Em | KfxSymbol::Rem)
            {
                // Bucket by float bit-pattern to count exact-equal values.
                *tally.entry((*value as f32).to_bits()).or_insert(0) += *uses;
            }
        }
        // Tie-break by bit-pattern so an equal-usage tie picks the same
        // dominant every run — the map's iteration order must not decide
        // (it flipped every lh ratio in the book between two exports).
        let dominant_bits = tally
            .into_iter()
            .max_by_key(|(bits, c)| (*c, *bits))
            .map(|(b, _)| b)?;
        let dominant = f32::from_bits(dominant_bits);
        if dominant <= 0.0 || !dominant.is_finite() {
            return None;
        }

        // Pass 2: rewrite each style's line_height as `(value / dominant) lh`.
        for (_, _, _, style, _) in self.styles.values_mut() {
            if let Some(KfxValue::Dimensioned { value, unit }) =
                style.get(KfxSymbol::LineHeight).cloned()
                && matches!(unit, KfxSymbol::Em | KfxSymbol::Rem)
            {
                let ratio = (value as f32) / dominant;
                style.set(
                    KfxSymbol::LineHeight,
                    KfxValue::Dimensioned {
                        value: ratio as f64,
                        unit: KfxSymbol::Lh,
                    },
                );
            }
        }
        Some(dominant)
    }
}

impl Default for StyleRegistry {
    fn default() -> Self {
        Self::new(0)
    }
}

/// The trimmed `class` when it is one token of ASCII alphanumerics, `_` and
/// `-` that opens with a non-digit — the shape a KFX style symbol takes.
/// `None` for a multi-token, empty, or special-character string.
fn usable_class_hint(class: &str) -> Option<String> {
    let trimmed = class.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains(char::is_whitespace) {
        return None;
    }
    let first = trimmed.chars().next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some(trimmed.to_string())
}

// ============================================================================
// Style Builder
// ============================================================================

/// Builds a ComputedStyle from IR style properties using the schema.
pub struct StyleBuilder<'a> {
    schema: &'a StyleSchema,
    style: ComputedStyle,
}

impl<'a> StyleBuilder<'a> {
    /// Create a new style builder.
    pub fn new(schema: &'a StyleSchema) -> Self {
        Self {
            schema,
            style: ComputedStyle::new(),
        }
    }

    /// Apply a CSS property.
    pub fn apply(&mut self, property: &str, value: &str) -> &mut Self {
        // Handle shorthand properties first
        if let Some(expanded) = expand_shorthand(property, value) {
            for (prop, val) in expanded {
                self.apply_single(&prop, &val);
            }
        } else {
            self.apply_single(property, value);
        }
        self
    }

    /// Apply a single (non-shorthand) property.
    fn apply_single(&mut self, property: &str, value: &str) {
        if let Some(rules) = self.schema.get(property) {
            for rule in rules {
                if let Some(kfx_value) = rule.transform.apply(value) {
                    self.style.set(rule.kfx_symbol, kfx_value);
                }
            }
        }
    }

    /// Ingest an IR `ComputedStyle` through the schema: each rule carrying an
    /// `ir_field` reads its CSS value with `extract_ir_field` and applies the
    /// rule's transform. The `WritingMode` arm reads `doc_writing_mode`.
    pub fn ingest_ir_style(
        &mut self,
        ir_style: &ir_style::ComputedStyle,
        doc_writing_mode: ir_style::WritingMode,
    ) -> &mut Self {
        // Iterate over all schema rules that have IR field mappings
        for rule in self.schema.ir_mapped_rules() {
            if let Some(ir_field) = rule.ir_field {
                // Extract CSS string from IR struct (returns None for default values)
                if let Some(css_value) = extract_ir_field(ir_style, ir_field, doc_writing_mode) {
                    // Apply schema transform to convert CSS → KFX
                    self.apply_single(rule.ir_key, &css_value);
                }
            }
        }

        self
    }

    /// Build the final computed style.
    pub fn build(self) -> ComputedStyle {
        self.style
    }
}

/// Expand CSS shorthand properties into individual properties.
fn expand_shorthand(property: &str, value: &str) -> Option<Vec<(String, String)>> {
    let parts: Vec<&str> = value.split_whitespace().collect();

    match property {
        "margin" => Some(expand_box_shorthand("margin", &parts)),
        "padding" => Some(expand_box_shorthand("padding", &parts)),
        "border-width" => Some(
            expand_box_shorthand("border", &parts)
                .into_iter()
                .map(|(p, v)| (format!("{}-width", p), v))
                .collect(),
        ),
        "font" => expand_font_shorthand(value),
        _ => None,
    }
}

/// Expand a box model shorthand (margin, padding) into four individual properties.
fn expand_box_shorthand(prefix: &str, parts: &[&str]) -> Vec<(String, String)> {
    let (top, right, bottom, left) = match parts.len() {
        1 => (parts[0], parts[0], parts[0], parts[0]),
        2 => (parts[0], parts[1], parts[0], parts[1]),
        3 => (parts[0], parts[1], parts[2], parts[1]),
        4 => (parts[0], parts[1], parts[2], parts[3]),
        _ => return vec![],
    };

    vec![
        (format!("{}-top", prefix), top.to_string()),
        (format!("{}-right", prefix), right.to_string()),
        (format!("{}-bottom", prefix), bottom.to_string()),
        (format!("{}-left", prefix), left.to_string()),
    ]
}

/// Expand font shorthand (complex, partial support).
fn expand_font_shorthand(value: &str) -> Option<Vec<(String, String)>> {
    // font: [style] [weight] size[/line-height] family
    // Positional parse over a whitespace split — whatever the arms below do not
    // recognize is left out of the expansion.
    let mut result = Vec::new();
    let parts: Vec<&str> = value.split_whitespace().collect();

    for part in &parts {
        let lower = part.to_lowercase();
        if lower == "italic" || lower == "oblique" {
            result.push(("font-style".to_string(), lower));
        } else if lower == "bold" || lower == "normal" || lower == "lighter" || lower == "bolder" {
            result.push(("font-weight".to_string(), lower));
        } else if part.contains("px")
            || part.contains("em")
            || part.contains("pt")
            || part.contains('%')
        {
            // This might be size or size/line-height
            if part.contains('/') {
                let size_parts: Vec<&str> = part.split('/').collect();
                if size_parts.len() == 2 {
                    result.push(("font-size".to_string(), size_parts[0].to_string()));
                    result.push(("line-height".to_string(), size_parts[1].to_string()));
                }
            } else {
                result.push(("font-size".to_string(), part.to_string()));
            }
        }
        // Font family is not extracted: it is the comma-separated tail, and a
        // whitespace split cannot delimit it.
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_computed_style_hash_consistency() {
        let mut style1 = ComputedStyle::new();
        style1.set(KfxSymbol::FontWeight, KfxValue::Integer(700));
        style1.set(KfxSymbol::FontStyle, KfxValue::Integer(1));

        let mut style2 = ComputedStyle::new();
        style2.set(KfxSymbol::FontStyle, KfxValue::Integer(1));
        style2.set(KfxSymbol::FontWeight, KfxValue::Integer(700));

        // Order shouldn't matter
        assert_eq!(style1.compute_hash(), style2.compute_hash());
    }

    #[test]
    fn test_computed_style_hash_difference() {
        let mut style1 = ComputedStyle::new();
        style1.set(KfxSymbol::FontWeight, KfxValue::Integer(700));

        let mut style2 = ComputedStyle::new();
        style2.set(KfxSymbol::FontWeight, KfxValue::Integer(400));

        assert_ne!(style1.compute_hash(), style2.compute_hash());
    }

    #[test]
    fn test_style_registry_deduplication() {
        let mut symbols = crate::formats::kfx::context::SymbolTable::new();
        let default_sym = symbols.get_or_intern("s0");
        let mut registry = StyleRegistry::new(default_sym);

        let mut style1 = ComputedStyle::new();
        style1.set(KfxSymbol::FontWeight, KfxValue::Integer(700));

        let mut style2 = ComputedStyle::new();
        style2.set(KfxSymbol::FontWeight, KfxValue::Integer(700));

        let id1 = registry.register(style1, &mut symbols);
        let id2 = registry.register(style2, &mut symbols);

        assert_eq!(id1, id2);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn test_expand_margin_shorthand() {
        let parts = vec!["10px"];
        let expanded = expand_box_shorthand("margin", &parts);
        assert_eq!(expanded.len(), 4);
        assert_eq!(expanded[0], ("margin-top".to_string(), "10px".to_string()));

        let parts = vec!["10px", "20px"];
        let expanded = expand_box_shorthand("margin", &parts);
        assert_eq!(expanded[0].1, "10px"); // top
        assert_eq!(expanded[1].1, "20px"); // right
        assert_eq!(expanded[2].1, "10px"); // bottom
        assert_eq!(expanded[3].1, "20px"); // left
    }

    #[test]
    fn test_style_builder() {
        let schema = StyleSchema::standard();
        let mut builder = StyleBuilder::new(schema);

        builder.apply("font-weight", "bold");
        builder.apply("font-style", "italic");

        let style = builder.build();
        assert_eq!(style.len(), 2);
        assert!(style.get(KfxSymbol::FontWeight).is_some());
        assert!(style.get(KfxSymbol::FontStyle).is_some());
    }

    #[test]
    fn ingest_ir_style_emits_writing_mode_vertical_rl() {
        use crate::style::{ComputedStyle as IrStyle, WritingMode};
        let schema = StyleSchema::standard();
        let mut builder = StyleBuilder::new(schema);

        let ir = IrStyle {
            writing_mode: WritingMode::VerticalRl,
            ..Default::default()
        };

        builder.ingest_ir_style(&ir, WritingMode::default());
        let style = builder.build();

        let value = style
            .get(KfxSymbol::WritingMode)
            .expect("writing_mode should be set in KFX style");
        assert!(
            matches!(value, KfxValue::Symbol(KfxSymbol::VerticalRl)),
            "expected Symbol(VerticalRl), got {:?}",
            value
        );
    }
}
