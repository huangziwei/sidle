//! Shared stylesheet emitter for KFX → EPUB conversion.
//!
//! Both KFX→EPUB engines (`kfx_to_epub` and the IR route's
//! `export_normalized`) synthesize one `style.css` from the book's KFX
//! `$157 style` entities. This module is the single implementation of the
//! CSS-side machinery: the declaration container ([`CssDecl`]), class-name
//! sanitization ([`safe_class_name`]), default-value pruning
//! ([`prune_default_decls`]), repeated-inline-style promotion
//! ([`promote_repeated_inline_styles`]), and the final stylesheet assembly
//! ([`StylesheetDoc::emit`]). Sharing the code is what makes the two routes'
//! stylesheets byte-identical by construction.

use std::collections::HashMap;

/// A small CSS rule body: ordered property/value pairs. Used when emitting
/// either an inline `style="..."` attribute or a stylesheet rule.
#[derive(Debug, Default, Clone)]
pub struct CssDecl {
    pub items: Vec<(String, String)>,
}

impl CssDecl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let n = name.into();
        // Last write wins.
        if let Some(slot) = self.items.iter_mut().find(|(k, _)| *k == n) {
            slot.1 = value.into();
        } else {
            self.items.push((n, value.into()));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn to_inline(&self) -> String {
        let mut s = String::new();
        for (i, (k, v)) in self.items.iter().enumerate() {
            if i > 0 {
                s.push_str("; ");
            }
            s.push_str(k);
            s.push_str(": ");
            s.push_str(v);
        }
        s
    }
}

/// Parse a serialized inline declaration (`"k: v; k2: v2"`) back into a
/// [`CssDecl`]. Inverse of [`CssDecl::to_inline`]; also tolerant of plain
/// `style="..."` attribute text.
pub fn parse_inline_decl(s: &str) -> CssDecl {
    let mut decl = CssDecl::new();
    for chunk in s.split(';') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        if let Some(colon) = chunk.find(':') {
            let k = chunk[..colon].trim();
            let v = chunk[colon + 1..].trim();
            decl.set(k, v);
        }
    }
    decl
}

/// Sanitize a KFX style name into a valid CSS class name (and matching HTML
/// `class` attribute). Non-identifier characters become `_`; a leading digit
/// (or `-digit` / lone `-`) is prefixed with `_`, since a CSS identifier can't
/// start with a digit — an unescaped `.0HrDijd…` selector is a parse error
/// (epubcheck CSS-008). Applied identically to the selector and the element's
/// class attribute so they stay in sync.
pub fn safe_class_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let needs_prefix = match out.as_bytes() {
        [] => true,
        [b'-'] => true,
        [b'-', d, ..] if d.is_ascii_digit() => true,
        [d, ..] if d.is_ascii_digit() => true,
        _ => false,
    };
    if needs_prefix {
        out.insert(0, '_');
    }
    out
}

/// Drop declarations whose value matches the CSS spec default (a no-op both
/// in the stylesheet and inline). Mirrors calibre's `simplify_styles` at the
/// high-impact level:
///   - `letter-spacing` / `word-spacing`: `0` / `0em` / `0px` / `0rem` /
///     `normal` → drop
///   - `text-indent`: `0` / `0em` / `0px` / `0rem` / `0%` → drop
///   - `white-space` / `font-style` / `font-weight` / `font-variant` /
///     `font-stretch`: `normal` → drop
///   - `text-decoration` / `text-transform`: `none` → drop
pub fn prune_default_decls(decl: &mut CssDecl) {
    decl.items.retain(|(k, v)| !is_default_decl(k, v));
}

fn is_default_decl(name: &str, value: &str) -> bool {
    let v = value.trim();
    match name {
        "letter-spacing" | "word-spacing" => {
            matches!(v, "0" | "0em" | "0px" | "0rem" | "normal")
        }
        "text-indent" => matches!(v, "0" | "0em" | "0px" | "0rem" | "0%"),
        "white-space" | "font-style" | "font-weight" | "font-variant" | "font-stretch" => {
            v == "normal"
        }
        "text-decoration" | "text-transform" => v == "none",
        _ => false,
    }
}

/// Promote inline-style declarations that repeat across elements into
/// auto-generated class rules.
///
/// Mirrors a subset of calibre's `fixup_styles_and_classes`
/// (yj_to_epub_properties.py:1388): when the same serialized declaration
/// shows up on ≥ 2 elements across the book, it gets a `g<N>` class rule and
/// the caller replaces each matching inline style with the class reference.
/// Single-occurrence styles stay inline — keeps the stylesheet readable.
///
/// `inline_decls` is the multiset of serialized non-empty inline styles
/// (one entry per styled element). Promoted rules are appended to
/// `generated` (numbering continues from its current length so class names
/// stay stable); the returned map is serialized-style → class name for the
/// caller's rewrite pass.
pub fn promote_repeated_inline_styles(
    inline_decls: impl IntoIterator<Item = String>,
    generated: &mut Vec<(String, CssDecl)>,
) -> HashMap<String, String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for s in inline_decls {
        *counts.entry(s).or_insert(0) += 1;
    }
    // Most frequent first; ties broken by the serialized text so class
    // numbering is deterministic run-to-run.
    let mut sorted: Vec<(String, usize)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut promoted: HashMap<String, String> = HashMap::new();
    for (style_str, count) in sorted {
        if count < 2 {
            break; // Remaining entries are all single-occurrence.
        }
        let class_name = format!("g{}", generated.len());
        // Rebuild a CssDecl from the serialized string so the rule is
        // emitted via the same path as named-style rules.
        let decl = parse_inline_decl(&style_str);
        generated.push((class_name.clone(), decl));
        promoted.insert(style_str, class_name);
    }
    promoted
}

/// Everything the stylesheet emitter needs, gathered by either route.
#[derive(Debug, Default)]
pub struct StylesheetDoc {
    /// Image-based fixed-layout book: emit the viewport-fit reset instead of
    /// the reflowable body defaults.
    pub fixed_layout: bool,
    /// Doc-level CSS writing mode (`horizontal-tb` emits no body rule).
    pub writing_mode: String,
    /// Named rules: (raw KFX style name, declarations). Emitted sorted by
    /// name, selectors sanitized via [`safe_class_name`]; empty declarations
    /// are skipped (a class attribute may still reference them — the rule is
    /// simply absent, which renders identically).
    pub named_rules: Vec<(String, CssDecl)>,
    /// Auto-generated `g<N>` classes from [`promote_repeated_inline_styles`],
    /// emitted after the named rules in insertion order.
    pub generated_classes: Vec<(String, CssDecl)>,
}

impl StylesheetDoc {
    /// Assemble the final `style.css` text.
    ///
    /// Layout matches calibre's output: `@charset` first; for fixed-layout
    /// books a reset that sizes images to the viewport (the page wrapper
    /// establishes no definite height, and a vertical-rl body would flip the
    /// block axis — page-turn direction is carried by
    /// `page-progression-direction`, so forcing horizontal-tb is safe); for
    /// reflowable books a `body { writing-mode: … }` rule when the book is
    /// not horizontal-tb. Then one rule per named style (sorted), then the
    /// generated classes.
    pub fn emit(&self) -> String {
        let mut s = String::new();
        s.push_str("@charset \"utf-8\";\n");

        if self.fixed_layout {
            s.push_str("html, body { margin: 0; padding: 0; writing-mode: horizontal-tb; }\n");
            s.push_str("body { text-align: center; }\n");
            s.push_str(
                "img { display: block; width: 100vw; height: 100vh; object-fit: contain; }\n",
            );
        } else if !self.writing_mode.is_empty() && self.writing_mode != "horizontal-tb" {
            s.push_str(&format!(
                "body {{ writing-mode: {wm}; -webkit-writing-mode: {wm}; -epub-writing-mode: {wm}; }}\n",
                wm = self.writing_mode
            ));
        }

        let mut named: Vec<&(String, CssDecl)> = self.named_rules.iter().collect();
        named.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, decl) in named {
            if decl.is_empty() {
                continue;
            }
            s.push_str(&format!(
                ".{} {{ {} }}\n",
                safe_class_name(name),
                decl.to_inline()
            ));
        }
        for (class_name, decl) in &self.generated_classes {
            if decl.is_empty() {
                continue;
            }
            s.push_str(&format!(".{} {{ {} }}\n", class_name, decl.to_inline()));
        }
        s
    }
}

/// Merged-style properties that force a block-flow `<img>` into a wrapper
/// `<div>` carrying them. KFX resolves a percentage `width` against the space
/// left between the element's own margins — how CSS sizes a block wrapper's
/// content box, not how it sizes a replaced element — and `float` / `clear` /
/// `text-align` / the `break-*` family are dead or wrong on a replaced inline
/// element. The boko-emittable subset of calibre's
/// `BLOCK_CONTAINER_PROPERTIES` (`yj_to_epub_content.py:49`), plus `clear`
/// (calibre leaves it dead on the `<img>`; Kindle honors it, the wrapper is
/// where CSS does too).
pub fn img_wrapper_trigger(prop: &str) -> bool {
    matches!(
        prop,
        "margin"
            | "margin-top"
            | "margin-left"
            | "margin-bottom"
            | "margin-right"
            | "float"
            | "clear"
            | "text-indent"
            | "text-align"
            | "text-align-last"
            | "break-before"
            | "break-after"
            | "break-inside"
            | "page-break-before"
            | "page-break-after"
            | "page-break-inside"
            | "overflow"
            | "transform"
            | "transform-origin"
            | "display"
    )
}

/// Properties that belong on the wrapper `<div>` once one exists: every
/// trigger property plus `box-sizing` (meaningless on the replaced element,
/// meaningful on the box that carries the margins).
pub fn img_wrapper_prop(prop: &str) -> bool {
    img_wrapper_trigger(prop) || prop == "box-sizing"
}

/// Partition a block-flow image's merged style (named `$style` + the content
/// element's own inline properties) into `(wrapper, img)` halves, or `None`
/// when nothing triggers a wrapper and the image stays bare.
///
/// Includes calibre's `fit_width` hoist: a float is shrink-to-fit, so a
/// child's percentage width would resolve against the float's own
/// content-derived width — circular; the author meant % of the column. The
/// percentage moves onto the float and the image fills it.
pub fn partition_image_style(merged: CssDecl) -> Option<(CssDecl, CssDecl)> {
    if !merged.items.iter().any(|(k, _)| img_wrapper_trigger(k)) {
        return None;
    }
    let mut wrapper_decl = CssDecl::new();
    let mut img_decl = CssDecl::new();
    for (k, v) in merged.items {
        if img_wrapper_prop(&k) {
            wrapper_decl.set(k, v);
        } else {
            img_decl.set(k, v);
        }
    }
    if wrapper_decl
        .items
        .iter()
        .any(|(k, v)| k == "float" && v != "none")
        && let Some(pos) = img_decl
            .items
            .iter()
            .position(|(k, v)| k == "width" && v.ends_with('%'))
    {
        let (_, w) = img_decl.items.remove(pos);
        wrapper_decl.set("width", w);
        img_decl.set("width", "100%");
    }
    Some((wrapper_decl, img_decl))
}

/// A source format's contribution to the normalized stylesheet: every named
/// style converted to CSS declarations (unpruned — the export pass prunes its
/// own working copies), plus the doc-level layout facts the emitter's header
/// needs. Produced by [`crate::import::Importer::stylesheet_program`]; `None`
/// from an importer means the format ships its own CSS assets instead.
#[derive(Debug, Default)]
pub struct CssProgram {
    /// Raw source style name → converted declarations. A node whose
    /// `semantics.class` names an entry with a non-empty declaration gets
    /// `class="<safe_class_name(name)>"` in synthesized XHTML.
    pub named: HashMap<String, CssDecl>,
    /// Doc-level CSS writing mode (`horizontal-tb` emits no body rule).
    pub writing_mode: String,
    /// Image-based fixed-layout book (viewport-fit reset header).
    pub fixed_layout: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_class_name_sanitizes_and_prefixes() {
        assert_eq!(safe_class_name("HrDijd"), "HrDijd");
        assert_eq!(safe_class_name("0HrDijd"), "_0HrDijd");
        assert_eq!(safe_class_name("-9x"), "_-9x");
        assert_eq!(safe_class_name("-"), "_-");
        assert_eq!(safe_class_name("a.b c"), "a_b_c");
        assert_eq!(safe_class_name(""), "_");
    }

    #[test]
    fn prune_drops_spec_defaults_only() {
        let mut d = CssDecl::new();
        d.set("letter-spacing", "0em");
        d.set("text-indent", "0%");
        d.set("font-weight", "normal");
        d.set("text-decoration", "none");
        d.set("font-weight", "bold"); // last write wins → kept
        d.set("margin-top", "0"); // not in the prune table → kept
        prune_default_decls(&mut d);
        assert_eq!(d.to_inline(), "font-weight: bold; margin-top: 0");
    }

    #[test]
    fn promotion_threshold_and_ordering() {
        let inline = vec![
            "text-align: center".to_string(),
            "text-align: center".to_string(),
            "width: 100%".to_string(),
            "width: 100%".to_string(),
            "width: 100%".to_string(),
            "margin-top: 1em".to_string(), // single occurrence — stays inline
        ];
        let mut generated = Vec::new();
        let promoted = promote_repeated_inline_styles(inline, &mut generated);
        // Highest count first: width×3 → g0, center×2 → g1; the singleton
        // is not promoted.
        assert_eq!(promoted.get("width: 100%").map(String::as_str), Some("g0"));
        assert_eq!(
            promoted.get("text-align: center").map(String::as_str),
            Some("g1")
        );
        assert!(!promoted.contains_key("margin-top: 1em"));
        assert_eq!(generated.len(), 2);
        assert_eq!(generated[0].1.to_inline(), "width: 100%");
    }

    #[test]
    fn promotion_tie_breaks_by_text() {
        let inline = vec![
            "b: 2".to_string(),
            "b: 2".to_string(),
            "a: 1".to_string(),
            "a: 1".to_string(),
        ];
        let mut generated = Vec::new();
        let promoted = promote_repeated_inline_styles(inline, &mut generated);
        assert_eq!(promoted.get("a: 1").map(String::as_str), Some("g0"));
        assert_eq!(promoted.get("b: 2").map(String::as_str), Some("g1"));
    }

    #[test]
    fn emit_reflowable_vertical() {
        let mut doc = StylesheetDoc {
            writing_mode: "vertical-rl".to_string(),
            ..Default::default()
        };
        doc.named_rules.push(("zeta".into(), {
            let mut d = CssDecl::new();
            d.set("font-size", "1rem");
            d
        }));
        doc.named_rules.push(("alpha".into(), {
            let mut d = CssDecl::new();
            d.set("text-align", "justify");
            d
        }));
        doc.named_rules.push(("empty".into(), CssDecl::new()));
        doc.generated_classes.push(("g0".into(), {
            let mut d = CssDecl::new();
            d.set("width", "100%");
            d
        }));
        let css = doc.emit();
        assert_eq!(
            css,
            "@charset \"utf-8\";\n\
             body { writing-mode: vertical-rl; -webkit-writing-mode: vertical-rl; -epub-writing-mode: vertical-rl; }\n\
             .alpha { text-align: justify }\n\
             .zeta { font-size: 1rem }\n\
             .g0 { width: 100% }\n"
        );
    }

    #[test]
    fn emit_fixed_layout_reset() {
        let doc = StylesheetDoc {
            fixed_layout: true,
            writing_mode: "vertical-rl".to_string(),
            ..Default::default()
        };
        let css = doc.emit();
        assert!(css.starts_with("@charset \"utf-8\";\n"));
        assert!(css.contains(
            "img { display: block; width: 100vw; height: 100vh; object-fit: contain; }\n"
        ));
        // The FXL reset replaces (not joins) the body writing-mode rule.
        assert!(!css.contains("-epub-writing-mode"));
    }

    #[test]
    fn emit_horizontal_has_no_body_rule() {
        let doc = StylesheetDoc {
            writing_mode: "horizontal-tb".to_string(),
            ..Default::default()
        };
        assert_eq!(doc.emit(), "@charset \"utf-8\";\n");
    }

    #[test]
    fn parse_inline_decl_round_trip() {
        let decl = parse_inline_decl(" width: 100% ; ; text-align: center ");
        assert_eq!(decl.to_inline(), "width: 100%; text-align: center");
    }
}
