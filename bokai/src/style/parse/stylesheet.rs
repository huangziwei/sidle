//! CSS stylesheet parsing and rule structures.

use cssparser::{
    AtRuleParser, ParseError, Parser, ParserInput, QualifiedRuleParser, RuleBodyItemParser,
    RuleBodyParser, StyleSheetParser,
};
use selectors::parser::Selector;

use crate::html::element_ref::BokoSelectors;
use crate::model::FontFace;
use crate::style::Declaration;

use super::font::parse_font_face_block;

/// A parsed CSS stylesheet.
#[derive(Debug, Default, Clone)]
pub struct Stylesheet {
    pub rules: Vec<CssRule>,
    /// @font-face rules defining font family to file mappings.
    pub font_faces: Vec<FontFace>,
}

/// A CSS rule with selectors and declarations.
///
/// Declarations are separated into normal and important vectors,
/// following the lightningcss pattern for memory efficiency.
#[derive(Debug, Clone)]
pub struct CssRule {
    pub selectors: Vec<Selector<BokoSelectors>>,
    /// Normal (non-important) declarations.
    pub declarations: Vec<Declaration>,
    /// Important declarations (those with !important).
    pub important_declarations: Vec<Declaration>,
    pub specificity: Specificity,
}

/// CSS specificity for cascade ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Specificity {
    pub ids: u16,
    pub classes: u16,
    pub elements: u16,
}

impl Specificity {
    pub fn from_selector(selector: &Selector<BokoSelectors>) -> Self {
        let spec = selector.specificity();
        // selectors crate packs specificity as (id << 20) | (class << 10) | elements
        Self {
            ids: ((spec >> 20) & 0x3FF) as u16,
            classes: ((spec >> 10) & 0x3FF) as u16,
            elements: (spec & 0x3FF) as u16,
        }
    }
}

impl Ord for Specificity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.ids
            .cmp(&other.ids)
            .then(self.classes.cmp(&other.classes))
            .then(self.elements.cmp(&other.elements))
    }
}

impl PartialOrd for Specificity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Origin of a style (for cascade ordering).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    UserAgent = 0,
    Author = 1,
}

impl Stylesheet {
    /// Parse a CSS stylesheet from a string.
    pub fn parse(css: &str) -> Self {
        let mut input = ParserInput::new(css);
        let mut parser = Parser::new(&mut input);
        let mut rules = Vec::new();
        let mut font_faces = Vec::new();

        let mut rule_parser = TopLevelRuleParser {
            rules: &mut rules,
            font_faces: &mut font_faces,
        };
        let stylesheet_parser = StyleSheetParser::new(&mut parser, &mut rule_parser);

        for result in stylesheet_parser {
            // Ignore errors - lenient parsing
            let _ = result;
        }

        Self { rules, font_faces }
    }

    /// Check if the stylesheet is empty.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Rewrite every asset `url()` this stylesheet declares from the form the
    /// author wrote to whatever `resolve` returns.
    ///
    /// A CSS `url()` is relative to the stylesheet, not to the document that
    /// links it, so only the caller that knows where the rules came from can
    /// do this — the parser deliberately keeps the target verbatim. Callers
    /// pass a resolver that canonicalizes into the archive's path space, the
    /// same one image `src` attributes land in.
    pub fn resolve_asset_urls<F>(&mut self, mut resolve: F)
    where
        F: FnMut(&str) -> String,
    {
        for rule in &mut self.rules {
            for decl in rule
                .declarations
                .iter_mut()
                .chain(rule.important_declarations.iter_mut())
            {
                if let Declaration::BackgroundImage(src) = decl {
                    *src = resolve(src);
                }
            }
        }
    }
}

/// Parse a bare declaration list — the contents of a `style=""` attribute.
///
/// Returns (normal, important) declarations. Parsing is lenient like
/// `Stylesheet::parse`: invalid declarations are skipped.
pub fn parse_declaration_list(css: &str) -> (Vec<Declaration>, Vec<Declaration>) {
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    let mut declarations = Vec::new();
    let mut important_declarations = Vec::new();
    let mut decl_parser = DeclarationListParser {
        declarations: &mut declarations,
        important_declarations: &mut important_declarations,
    };

    for result in RuleBodyParser::new(&mut parser, &mut decl_parser) {
        // Ignore errors - lenient parsing
        let _ = result;
    }

    (declarations, important_declarations)
}

/// Parser for top-level stylesheet rules.
struct TopLevelRuleParser<'a> {
    rules: &'a mut Vec<CssRule>,
    font_faces: &'a mut Vec<FontFace>,
}

/// Prelude for @font-face rules (empty, just a marker).
struct FontFacePrelude;

impl<'i> AtRuleParser<'i> for TopLevelRuleParser<'_> {
    type Prelude = FontFacePrelude;
    type AtRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        name: cssparser::CowRcStr<'i>,
        _input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, Self::Error>> {
        if name.eq_ignore_ascii_case("font-face") {
            // @font-face has no prelude, just a block
            Ok(FontFacePrelude)
        } else {
            // Skip other at-rules
            Err(_input.new_custom_error(()))
        }
    }

    fn parse_block<'t>(
        &mut self,
        _prelude: Self::Prelude,
        _start: &cssparser::ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::AtRule, ParseError<'i, Self::Error>> {
        // Parse @font-face declarations
        if let Some(font_face) = parse_font_face_block(input) {
            self.font_faces.push(font_face);
        }
        Ok(())
    }
}

impl<'i> QualifiedRuleParser<'i> for TopLevelRuleParser<'_> {
    type Prelude = Vec<Selector<BokoSelectors>>;
    type QualifiedRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, Self::Error>> {
        parse_selector_list(input)
    }

    fn parse_block<'t>(
        &mut self,
        prelude: Self::Prelude,
        _start: &cssparser::ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::QualifiedRule, ParseError<'i, Self::Error>> {
        let specificity = prelude
            .first()
            .map(Specificity::from_selector)
            .unwrap_or_default();

        let mut declarations = Vec::new();
        let mut important_declarations = Vec::new();
        let mut decl_parser = DeclarationListParser {
            declarations: &mut declarations,
            important_declarations: &mut important_declarations,
        };

        for result in RuleBodyParser::new(input, &mut decl_parser) {
            // Ignore errors - lenient parsing
            let _ = result;
        }

        self.rules.push(CssRule {
            selectors: prelude,
            declarations,
            important_declarations,
            specificity,
        });

        Ok(())
    }
}

/// Parse a comma-separated list of selectors.
fn parse_selector_list<'i>(
    parser: &mut Parser<'i, '_>,
) -> Result<Vec<Selector<BokoSelectors>>, ParseError<'i, ()>> {
    let location = parser.current_source_location();
    let selectors = selectors::parser::SelectorList::parse(
        &BokoSelectors,
        parser,
        selectors::parser::ParseRelative::No,
    )
    .map_err(|_| location.new_custom_error(()))?;

    Ok(selectors.slice().to_vec())
}

struct DeclarationListParser<'a> {
    declarations: &'a mut Vec<Declaration>,
    important_declarations: &'a mut Vec<Declaration>,
}

impl<'i> cssparser::AtRuleParser<'i> for DeclarationListParser<'_> {
    type Prelude = ();
    type AtRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        _name: cssparser::CowRcStr<'i>,
        _input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, Self::Error>> {
        Err(_input.new_custom_error(()))
    }

    fn parse_block<'t>(
        &mut self,
        _prelude: Self::Prelude,
        _start: &cssparser::ParserState,
        _input: &mut Parser<'i, 't>,
    ) -> Result<Self::AtRule, ParseError<'i, Self::Error>> {
        Err(_input.new_custom_error(()))
    }
}

impl<'i> cssparser::QualifiedRuleParser<'i> for DeclarationListParser<'_> {
    type Prelude = ();
    type QualifiedRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        _input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, Self::Error>> {
        Err(_input.new_custom_error(()))
    }

    fn parse_block<'t>(
        &mut self,
        _prelude: Self::Prelude,
        _start: &cssparser::ParserState,
        _input: &mut Parser<'i, 't>,
    ) -> Result<Self::QualifiedRule, ParseError<'i, Self::Error>> {
        Err(_input.new_custom_error(()))
    }
}

impl<'i> cssparser::DeclarationParser<'i> for DeclarationListParser<'_> {
    type Declaration = ();
    type Error = ();

    fn parse_value<'t>(
        &mut self,
        name: cssparser::CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
        _start: &cssparser::ParserState,
    ) -> Result<Self::Declaration, ParseError<'i, Self::Error>> {
        let decls = Declaration::parse(&name, input);
        if !decls.is_empty() {
            let important = input.try_parse(cssparser::parse_important).is_ok();
            let target = if important {
                &mut *self.important_declarations
            } else {
                &mut *self.declarations
            };
            target.extend(decls);
        }
        Ok(())
    }
}

impl<'i> RuleBodyItemParser<'i, (), ()> for DeclarationListParser<'_> {
    fn parse_declarations(&self) -> bool {
        true
    }
    fn parse_qualified(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::WritingMode;

    #[test]
    fn font_shorthand_expands_to_longhands() {
        use crate::style::properties::{FontStyle, FontVariant, FontWeight, Length};

        let find = |css: &str| -> Vec<Declaration> {
            let sheet = Stylesheet::parse(css);
            assert_eq!(sheet.rules.len(), 1, "rule should parse: {css}");
            sheet.rules[0].declarations.clone()
        };

        // Full form with prefix components and line-height
        let decls = find(r#"p { font: italic small-caps bold 24px/1.5 "Gentium", serif; }"#);
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::FontStyle(FontStyle::Italic)))
        );
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::FontVariant(FontVariant::SmallCaps)))
        );
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::FontWeight(w) if *w == FontWeight::BOLD))
        );
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::FontSize(Length::Px(v)) if *v == 24.0))
        );
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::LineHeight(Length::Em(v)) if *v == 1.5))
        );
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::FontFamily(f) if f == "Gentium, serif"))
        );

        // Minimal form: omitted components reset to their initial values
        let decls = find("p { font: 16px serif; }");
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::FontStyle(FontStyle::Normal)))
        );
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::FontWeight(w) if *w == FontWeight::NORMAL))
        );
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::LineHeight(Length::Auto)))
        );
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::FontSize(Length::Px(v)) if *v == 16.0))
        );

        // Numeric weight
        let decls = find("p { font: 700 1em serif; }");
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::FontWeight(w) if *w == FontWeight::BOLD))
        );
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Declaration::FontSize(Length::Em(v)) if *v == 1.0))
        );

        // System-font keyword is not representable — declaration dropped
        let sheet = Stylesheet::parse("p { font: menu; }");
        assert!(sheet.rules.is_empty() || sheet.rules[0].declarations.is_empty());
    }

    #[test]
    fn writing_mode_parses_with_vendor_prefixes() {
        let css = r#"
            .vrtl {
              -webkit-writing-mode: vertical-rl;
              -epub-writing-mode:   vertical-rl;
              writing-mode:         vertical-rl;
            }
        "#;
        let stylesheet = Stylesheet::parse(css);
        assert_eq!(stylesheet.rules.len(), 1, "should parse the .vrtl rule");
        let modes: Vec<WritingMode> = stylesheet.rules[0]
            .declarations
            .iter()
            .filter_map(|d| match d {
                Declaration::WritingMode(m) => Some(*m),
                _ => None,
            })
            .collect();
        assert_eq!(
            modes.len(),
            3,
            "all three forms (standard + -webkit- + -epub-) should produce \
             a WritingMode declaration, got: {:?}",
            stylesheet.rules[0].declarations
        );
        assert!(modes.iter().all(|m| *m == WritingMode::VerticalRl));
    }

    /// An unquoted `<family-name>` is a run of identifiers, not just the first
    /// one: a `@font-face` and the rule referencing it must spell the same name,
    /// or the face is never matched and its bytes ship unused.
    #[test]
    fn unquoted_multi_word_family_names_survive() {
        let sheet = Stylesheet::parse(
            r#"@font-face { font-family: Garamond Premier Pro Caption; src: url(f.otf); }"#,
        );
        assert_eq!(sheet.font_faces.len(), 1);
        assert_eq!(
            sheet.font_faces[0].font_family,
            "Garamond Premier Pro Caption"
        );

        let sheet = Stylesheet::parse(r#"p { font-family: Garamond Premier Pro Caption; }"#);
        assert!(
            sheet.rules[0].declarations.iter().any(
                |d| matches!(d, Declaration::FontFamily(f) if f == "Garamond Premier Pro Caption")
            ),
            "property side keeps every identifier: {:?}",
            sheet.rules[0].declarations
        );
    }

    /// A quoted name stays verbatim; a comma separates alternatives.
    #[test]
    fn family_list_separates_on_commas_only() {
        let sheet = Stylesheet::parse(
            r#"p { font-family: "Toppan Bunkyu", Hiragino Mincho ProN, serif; }"#,
        );
        let family = sheet.rules[0]
            .declarations
            .iter()
            .find_map(|d| match d {
                Declaration::FontFamily(f) => Some(f.clone()),
                _ => None,
            })
            .expect("font-family parses");
        assert_eq!(family, "Toppan Bunkyu, Hiragino Mincho ProN, serif");
    }
}
