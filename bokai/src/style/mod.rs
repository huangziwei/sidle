//! Style system for CSS property types, computed styles, and cascade.
//!
//! This module contains:

mod cascade;
mod declaration;
mod font_family;
pub(crate) mod parse;
mod properties;
mod style_pool;
mod to_css;
mod types;

// Re-export the ToCss trait
pub trait ToCss {
    /// Write this value as CSS to the buffer.
    fn to_css(&self, buf: &mut String);

    /// Convert to a CSS string (convenience method).
    fn to_css_string(&self) -> String {
        let mut buf = String::new();
        self.to_css(&mut buf);
        buf
    }
}

pub use font_family::{
    compact_font_stack, font_stack_category, is_generic_font_keyword, preferred_font_face,
};

// Re-export property types
pub use properties::ROOT_FONT_SIZE_PX;
pub use properties::{
    BackgroundRepeat, BackgroundSize, BorderCollapse, BorderStyle, BoxAlign, BoxSizing, BreakValue,
    Clear, Color, DecorationStyle, Display, Float, FontStyle, FontVariant, FontWeight, Hyphens,
    Length, LineBreak, ListStylePosition, ListStyleType, OverflowWrap, TextAlign,
    TextCombineUpright, TextEmphasisOver, TextEmphasisPosition, TextEmphasisRight,
    TextEmphasisStyle, TextOrientation, TextTransform, VerticalAlign, Visibility, WhiteSpace,
    WordBreak, WritingMode,
};

// Re-export core style types
pub use style_pool::StylePool;
pub use types::{ComputedStyle, StyleId};

// Re-export declaration types: the typed enum and its raw string-level sibling
pub use declaration::{CssDecl, Declaration, parse_inline_decl};

// Re-export stylesheet types from parse module
pub use parse::{CssRule, Origin, Specificity, Stylesheet, TextDecorationValue};

// Re-export cascade function
pub use cascade::{CascadeIndex, CascadeScratch, compute_styles, compute_styles_indexed};

// Re-export macro for internal use
#[allow(unused_imports)]
pub(crate) use properties::enum_property;
