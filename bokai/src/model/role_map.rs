//! Maps HTML elements to semantic roles.
//!
//! This module defines the mapping from HTML element names to IR `Role` values.

use html5ever::LocalName;

use crate::model::Role;

/// Map an HTML element name to its semantic role.
///
/// Unknown element names fall through to `Role::Container` — content still
/// flows but no semantics are preserved. Use [`element_to_role_known`] to
/// distinguish explicit handling from the fallthrough (the tag coverage
/// validator relies on this).
pub fn element_to_role(local_name: &LocalName) -> Role {
    element_to_role_known(local_name).unwrap_or(Role::Container)
}

/// Like `element_to_role` but returns `None` for elements not explicitly
/// handled — the validator uses this to surface "unknown element seen in
/// source, falling back to generic Container."
pub fn element_to_role_known(local_name: &LocalName) -> Option<Role> {
    Some(match local_name.as_ref() {
        // Block containers
        "div" | "section" | "article" | "nav" | "header" | "footer" | "main" | "address"
        | "details" | "summary" | "hgroup" => Role::Container,

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
        // also maps here so its `xlink:href` survives as a KFX image; without
        // this, calibre-generated covers wrapped in `<svg><image/></svg>`
        // would silently disappear from the storyline. HTML `<image>` is a
        // deprecated alias that html5ever already rewrites to `<img>`, so this
        // arm is effectively SVG-only in practice.
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
        "thead" => Role::TableHead,
        "tbody" => Role::TableBody,
        "tr" => Role::TableRow,
        "td" | "th" => Role::TableCell,

        // Ruby annotations (furigana). <ruby> wraps base + annotation;
        // <rt> is the annotation text; <rb> is the explicit base. The Role::Ruby
        // arm in kfx/storyline.rs flatten pairs siblings up so compound rubies
        // like <ruby><rb>漢</rb><rt>かん</rt><rb>字</rb><rt>じ</rt></ruby>
        // emit one annotation per base. <rb> must be inline (not Container)
        // so it doesn't break the inline flow inside a ruby.
        "ruby" => Role::Ruby,
        "rt" => Role::RubyText,
        "rb" => Role::Inline,
        // <rp> contains fallback parentheses for renderers that don't support
        // ruby — we always render ruby, so they should be skipped. None of
        // the reference EPUBs use <rp>; if encountered as Role::Inline its
        // text leaks into base inline content. TODO: dedicated skip role
        // when an actual <rp>-using book shows up.
        "rp" => Role::Inline,

        "label" | "legend" | "output" | "data" | "bdi" | "bdo" | "wbr" => Role::Inline,

        // Document roots. `transform()` treats <html>/<body> specially and
        // attaches their children to the chapter root, so mapping them to
        // Container here is mainly for the tag-coverage validator's sake —
        // it acknowledges we know about these elements rather than letting
        // them fall through to the unknown-element bucket.
        "html" | "body" => Role::Container,

        // <head> and metadata children — content here is not user-visible
        // (title bar, stylesheet links, meta tags, etc.). Currently flows
        // through as Container; ideally the transform would skip the entire
        // subtree, but this mapping at least surfaces honest intent.
        "head" | "title" | "meta" | "link" | "style" | "script" | "noscript" => Role::Container,

        // SVG/MathML/embedded media — bokai has no specialised handling for
        // these yet, so they flow as generic Containers and only their text
        // leaves (if any) survive. Listed explicitly so the validator
        // distinguishes "known-untreated" from "unknown".
        // (`image` is intentionally NOT here — it's handled above as Role::Image
        //  so SVG `<image>` survives as a KFX image.)
        "svg" | "math" | "audio" | "video" | "source" | "track" | "object" | "embed" | "iframe"
        | "canvas" => Role::Container,

        // Form elements — not relevant for ebooks but recognised.
        "form" | "input" | "button" | "select" | "option" | "optgroup" | "textarea"
        | "fieldset" | "datalist" | "progress" | "meter" => Role::Container,

        // Not explicitly handled — caller decides the fallback.
        _ => return None,
    })
}
