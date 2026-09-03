//! Maps HTML elements to semantic roles.
//!
//! This module defines the mapping from HTML element names to IR `Role` values.

use html5ever::LocalName;

use crate::model::Role;

/// Map an HTML element name to its semantic role.
pub fn element_to_role(local_name: &LocalName) -> Role {
    element_to_role_known(local_name).unwrap_or(Role::Container)
}

/// Like `element_to_role` but returns `None` for elements not explicitly
pub fn element_to_role_known(local_name: &LocalName) -> Option<Role> {
    Some(match local_name.as_ref() {
        // Block containers. `center` is the deprecated presentational one, which the UA
        // stylesheet gives `display: block; text-align: center`.
        "div" | "section" | "article" | "nav" | "header" | "footer" | "main" | "address"
        | "details" | "summary" | "hgroup" | "center" => Role::Container,

        // Line break (leaf node, not a container)
        "br" => Role::Break,

        // Horizontal rule (thematic break)
        "hr" => Role::Rule,

        // Aside/sidebar
        "aside" => Role::Sidebar,

        // Figure and caption
        "figure" => Role::Figure,
        "figcaption" | "caption" => Role::Caption,

        // Paragraphs - block-level text containers
        "p" => Role::Paragraph,

        // Preformatted code blocks
        "pre" => Role::CodeBlock,

        // Inline elements with styling (rendered via ComputedStyle)
        "span" | "em" | "i" | "cite" | "var" | "dfn" | "strong" | "b" | "code" | "kbd" | "samp"
        | "tt" | "sup" | "sub" | "u" | "ins" | "s" | "strike" | "del" | "small" | "mark"
        | "abbr" | "time" | "q" => Role::Inline,

        // Headings with level
        "h1" => Role::Heading(1),
        "h2" => Role::Heading(2),
        "h3" => Role::Heading(3),
        "h4" => Role::Heading(4),
        "h5" => Role::Heading(5),
        "h6" => Role::Heading(6),

        // Links
        "a" => Role::Link,

        // Images. SVG `<image>` (parsed by xml5ever with the SVG namespace)
        "img" | "image" => Role::Image,

        // Lists
        "ul" => Role::UnorderedList,
        "ol" => Role::OrderedList,
        "li" => Role::ListItem,

        // Block quote
        "blockquote" => Role::BlockQuote,

        // Definition lists
        "dl" => Role::DefinitionList,
        "dt" => Role::DefinitionTerm,
        "dd" => Role::DefinitionDescription,

        // Tables
        "table" => Role::Table,
        "colgroup" => Role::ColumnGroup,
        "col" => Role::Column,
        "thead" => Role::TableHead,
        "tbody" => Role::TableBody,
        "tr" => Role::TableRow,
        "td" | "th" => Role::TableCell,

        // Ruby annotations (furigana). <ruby> wraps base + annotation;
        "ruby" => Role::Ruby,
        "rt" => Role::RubyText,
        "rb" => Role::Inline,
        // <rp> contains fallback parentheses for renderers that don't support
        "rp" => Role::Inline,

        "label" | "legend" | "output" | "data" | "bdi" | "bdo" | "wbr" => Role::Inline,

        // Document roots. `transform()` attaches their children to the chapter root, so
        // mapping them here is for the tag-coverage validator's sake.
        "html" | "body" => Role::Container,

        // <head> and metadata children — content here is not user-visible
        "head" | "title" | "meta" | "link" | "style" | "script" | "noscript" => Role::Container,

        // SVG/MathML/embedded media flow as generic Containers, so only their text leaves
        // survive. Listed so the validator tells known-untreated from unknown.
        "svg" | "math" | "audio" | "video" | "source" | "track" | "object" | "embed" | "iframe"
        | "canvas" => Role::Container,

        // Form elements — not relevant for ebooks but recognised.
        "form" | "input" | "button" | "select" | "option" | "optgroup" | "textarea"
        | "fieldset" | "datalist" | "progress" | "meter" => Role::Container,

        // Not explicitly handled — caller decides the fallback.
        _ => return None,
    })
}
