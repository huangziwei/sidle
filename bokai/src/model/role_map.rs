//! Maps HTML element names to IR `Role` values.

use html5ever::LocalName;

use crate::model::Role;

/// Map an HTML element name to its semantic role, `None` for an element with
/// no role of its own.
pub fn element_to_role_known(local_name: &LocalName) -> Option<Role> {
    Some(match local_name.as_ref() {
        "div" | "section" | "article" | "nav" | "header" | "footer" | "main" | "address"
        | "details" | "summary" | "hgroup" | "center" => Role::Container,

        "br" => Role::Break,

        "hr" => Role::Rule,

        "aside" => Role::Sidebar,

        "figure" => Role::Figure,
        "figcaption" | "caption" => Role::Caption,

        "p" => Role::Paragraph,

        "pre" => Role::CodeBlock,

        "span" | "em" | "i" | "cite" | "var" | "dfn" | "strong" | "b" | "code" | "kbd" | "samp"
        | "tt" | "sup" | "sub" | "u" | "ins" | "s" | "strike" | "del" | "small" | "big"
        | "mark" | "abbr" | "acronym" | "time" | "q" | "font" | "nobr" => Role::Inline,

        "h1" => Role::Heading(1),
        "h2" => Role::Heading(2),
        "h3" => Role::Heading(3),
        "h4" => Role::Heading(4),
        "h5" => Role::Heading(5),
        "h6" => Role::Heading(6),

        "a" => Role::Link,

        // `image` is SVG's `<image>`.
        "img" | "image" => Role::Image,

        "ul" => Role::UnorderedList,
        "ol" => Role::OrderedList,
        "li" => Role::ListItem,

        "blockquote" => Role::BlockQuote,

        "dl" => Role::DefinitionList,
        "dt" => Role::DefinitionTerm,
        "dd" => Role::DefinitionDescription,

        "table" => Role::Table,
        "colgroup" => Role::ColumnGroup,
        "col" => Role::Column,
        "thead" => Role::TableHead,
        "tbody" => Role::TableBody,
        "tr" => Role::TableRow,
        "td" | "th" => Role::TableCell,

        "ruby" => Role::Ruby,
        "rt" => Role::RubyText,
        "rb" | "rtc" => Role::Inline,
        // `<rp>` holds fallback parentheses for renderers without ruby support.
        "rp" => Role::Inline,

        "label" | "legend" | "output" | "data" | "bdi" | "bdo" | "wbr" => Role::Inline,

        "html" | "body" => Role::Container,

        "head" | "title" | "meta" | "link" | "style" | "script" | "noscript" => Role::Container,

        "svg" | "math" | "audio" | "video" | "source" | "track" | "object" | "embed" | "iframe"
        | "canvas" => Role::Container,

        "form" | "input" | "button" | "select" | "option" | "optgroup" | "textarea"
        | "fieldset" | "datalist" | "progress" | "meter" => Role::Container,

        _ => return None,
    })
}
