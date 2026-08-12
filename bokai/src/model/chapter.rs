//! Chapter representation for normalized ebook content.
//!
//! The Chapter (formerly IRChapter) provides a format-agnostic tree structure
//! for ebook chapters:
//! - Nodes with semantic roles (paragraphs, headings, links, etc.)
//! - Interned styles via StylePool
//! - Sparse semantic attributes (href, src, alt)
//! - Universal link representation (handles both EPUB IDs and Kindle offsets)
//! - Global text buffer with range references

use super::node::{Node, NodeId, Role, TextRange};
use super::semantic::SemanticMap;
use crate::style::StylePool;

/// Unique identifier for a chapter/spine item within a book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChapterId(pub u32);

/// A chapter's content in normalized IR form.
///
/// The IR tree uses a parent-pointer / first-child / next-sibling representation
/// for efficient traversal and minimal memory overhead.
#[derive(Debug, Clone)]
pub struct Chapter {
    /// All nodes in the tree (index 0 is always the root).
    nodes: Vec<Node>,
    /// Style pool with deduplication.
    pub styles: StylePool,
    /// Sparse semantic attributes (href, src, alt, id).
    pub semantics: SemanticMap,
    /// Global text buffer (nodes reference ranges into this).
    text: String,
}

impl Default for Chapter {
    fn default() -> Self {
        Self::new()
    }
}

impl Chapter {
    /// Create a new empty chapter with a root node.
    pub fn new() -> Self {
        Self {
            nodes: vec![Node::new(Role::Root)],
            styles: StylePool::new(),
            semantics: SemanticMap::new(),
            text: String::new(),
        }
    }

    /// Get the root node ID.
    pub fn root(&self) -> NodeId {
        NodeId::ROOT
    }

    /// Get a node by ID.
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.0 as usize)
    }

    /// Get a mutable node by ID.
    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(id.0 as usize)
    }

    /// Get the number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Allocate a new node and return its ID.
    pub fn alloc_node(&mut self, node: Node) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(node);
        id
    }

    /// Append text to the global buffer and return the range.
    pub fn append_text(&mut self, text: &str) -> TextRange {
        let start = self.text.len() as u32;
        self.text.push_str(text);
        TextRange::new(start, text.len() as u32)
    }

    /// Get text from a range.
    pub fn text(&self, range: TextRange) -> &str {
        let start = range.start as usize;
        let end = (range.start + range.len) as usize;
        &self.text[start..end]
    }

    /// Get the entire text buffer.
    pub fn text_buffer(&self) -> &str {
        &self.text
    }

    /// Append a child node to a parent.
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) {
        // Set the child's parent
        if let Some(child_node) = self.nodes.get_mut(child.0 as usize) {
            child_node.parent = Some(parent);
        }

        // Find the last child of parent and append
        if let Some(parent_node) = self.nodes.get(parent.0 as usize) {
            if let Some(first_child) = parent_node.first_child {
                // Find last sibling
                let mut current = first_child;
                while let Some(node) = self.nodes.get(current.0 as usize) {
                    if let Some(next) = node.next_sibling {
                        current = next;
                    } else {
                        break;
                    }
                }
                // Append as next sibling of last child
                if let Some(last_node) = self.nodes.get_mut(current.0 as usize) {
                    last_node.next_sibling = Some(child);
                }
            } else {
                // No children yet, set as first child
                if let Some(parent_node) = self.nodes.get_mut(parent.0 as usize) {
                    parent_node.first_child = Some(child);
                }
            }
        }
    }

    /// Iterate over children of a node.
    pub fn children(&self, parent: NodeId) -> ChildIter<'_> {
        let first_child = self
            .nodes
            .get(parent.0 as usize)
            .and_then(|n| n.first_child);
        ChildIter {
            chapter: self,
            current: first_child,
        }
    }

    /// Iterate over all nodes in depth-first order.
    pub fn iter_dfs(&self) -> DfsIter<'_> {
        DfsIter {
            chapter: self,
            stack: vec![NodeId::ROOT],
        }
    }

    /// The source element ids this chapter's nodes carry, in **document
    /// order** — the order a rendered page presents them, which is what a
    /// consumer needs to answer "which chapter holds element N" and to walk a
    /// selection back to a `(element, offset)` handle. Empty for formats with
    /// no element-id namespace. See
    /// [`SemanticMap::source_element`](crate::model::SemanticMap::source_element).
    pub fn source_elements(&self) -> Vec<i64> {
        let mut out = Vec::new();
        for node in self.iter_dfs() {
            if let Some(element) = self.semantics.source_element(node) {
                out.push(element);
            }
        }
        out
    }

    /// What this chapter holds, without rendering it — see [`ChapterSummary`].
    pub fn summary(&self) -> ChapterSummary {
        let mut out = ChapterSummary::default();
        // Whitespace collapsing spans text nodes: two adjacent runs separated
        // only by markup are one word boundary, not two, and neither leading
        // nor trailing space counts. So the walk carries the "a space is owed"
        // state across nodes and only pays for it when real text follows.
        let mut text_seen = false;
        let mut pending_space = false;
        self.summarize(NodeId::ROOT, &mut out, &mut text_seen, &mut pending_space);
        out.image_only = !text_seen && out.has_image;
        out
    }

    fn summarize(
        &self,
        node: NodeId,
        out: &mut ChapterSummary,
        text_seen: &mut bool,
        pending_space: &mut bool,
    ) {
        for child in self.children(node) {
            let Some(n) = self.node(child) else { continue };
            match n.role {
                // A ruby annotation is a pronunciation gloss above the base
                // text, not part of it: counting it would inflate the reading
                // measure of every CJK book.
                Role::RubyText => continue,
                Role::Text => {
                    for c in self.text(n.text).chars() {
                        // A line break renders as a break element, which
                        // contributes nothing to text content — so it is
                        // neither a character nor a word boundary here.
                        if EOL.contains(&c) {
                            continue;
                        }
                        if c.is_whitespace() {
                            if *text_seen {
                                *pending_space = true;
                            }
                        } else {
                            if *pending_space {
                                out.text_chars += 1;
                                *pending_space = false;
                            }
                            out.text_chars += 1;
                            *text_seen = true;
                        }
                    }
                }
                Role::Image => {
                    out.has_image = true;
                    if let Some(src) = self.semantics.src(child)
                        && !out.images.iter().any(|h| h == src)
                    {
                        out.images.push(src.to_string());
                    }
                }
                _ => {}
            }
            // A picture the stylesheet paints (a section-break ornament, say)
            // is one the renderer has to fetch just the same, so it belongs in
            // the fetch list. It does not make the chapter `image_only`
            // though: a background is decoration behind content, never the
            // content itself.
            if let Some(src) = self
                .styles
                .get(n.style)
                .and_then(|s| s.background_image.as_deref())
                && !out.images.iter().any(|h| h == src)
            {
                out.images.push(src.to_string());
            }
            self.summarize(child, out, text_seen, pending_space);
        }
    }
}

/// Characters that render as a line break rather than as text.
const EOL: &[char] = &['\n', '\r', '\u{2028}', '\u{2029}'];

/// What a chapter holds, answerable without rendering it.
///
/// A renderer needs these before it paints anything: how much reading it
/// represents, whether it is a full-page image rather than prose, and which
/// images to fetch first. Deriving them from the IR keeps a consumer from
/// re-parsing markup it just produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChapterSummary {
    /// Base-text character count: ruby annotations excluded, whitespace runs
    /// counted as one, leading and trailing whitespace not counted. Unicode
    /// scalars, so this differs from a UTF-16 count on astral characters.
    pub text_chars: u64,
    /// No base text at all and at least one image — a cover or full-bleed
    /// illustration, which a reader paginates differently from prose.
    pub image_only: bool,
    /// Image paths referenced, in document order, deduplicated. A reader
    /// fetches them in this order.
    pub images: Vec<String>,
    /// Whether any image was seen at all (`image_only` needs both halves).
    has_image: bool,
}

/// Iterator over children of a node.
pub struct ChildIter<'a> {
    chapter: &'a Chapter,
    current: Option<NodeId>,
}

impl Iterator for ChildIter<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.current?;
        self.current = self
            .chapter
            .nodes
            .get(current.0 as usize)
            .and_then(|n| n.next_sibling);
        Some(current)
    }
}

/// Depth-first iterator over all nodes.
pub struct DfsIter<'a> {
    chapter: &'a Chapter,
    stack: Vec<NodeId>,
}

impl Iterator for DfsIter<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.stack.pop()?;

        // Push children in reverse order so they're visited left-to-right
        let mut children: Vec<NodeId> = self.chapter.children(current).collect();
        children.reverse();
        self.stack.extend(children);

        Some(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ComputedStyle, FontWeight};

    #[test]
    fn test_chapter_creation() {
        let chapter = Chapter::new();
        assert_eq!(chapter.node_count(), 1);
        assert_eq!(chapter.root(), NodeId::ROOT);

        let root = chapter.node(NodeId::ROOT).unwrap();
        assert_eq!(root.role, Role::Root);
        assert!(root.parent.is_none());
    }

    #[test]
    fn test_text_buffer() {
        let mut chapter = Chapter::new();

        let range1 = chapter.append_text("Hello, ");
        let range2 = chapter.append_text("World!");

        assert_eq!(chapter.text(range1), "Hello, ");
        assert_eq!(chapter.text(range2), "World!");
        assert_eq!(chapter.text_buffer(), "Hello, World!");
    }

    #[test]
    fn test_node_tree() {
        let mut chapter = Chapter::new();

        let para = chapter.alloc_node(Node::new(Role::Text));
        chapter.append_child(NodeId::ROOT, para);

        let text_range = chapter.append_text("Test content");
        let text_node = chapter.alloc_node(Node::text(text_range));
        chapter.append_child(para, text_node);

        // Verify structure
        let children: Vec<_> = chapter.children(NodeId::ROOT).collect();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0], para);

        let text_children: Vec<_> = chapter.children(para).collect();
        assert_eq!(text_children.len(), 1);
        assert_eq!(chapter.node(text_children[0]).unwrap().role, Role::Text);
    }

    #[test]
    fn test_dfs_iteration() {
        let mut chapter = Chapter::new();

        let para1 = chapter.alloc_node(Node::new(Role::Text));
        let para2 = chapter.alloc_node(Node::new(Role::Text));
        chapter.append_child(NodeId::ROOT, para1);
        chapter.append_child(NodeId::ROOT, para2);

        let range = chapter.append_text("Text");
        let text = chapter.alloc_node(Node::text(range));
        chapter.append_child(para1, text);

        let nodes: Vec<_> = chapter.iter_dfs().collect();
        assert_eq!(nodes.len(), 4); // root, para1, text, para2
        assert_eq!(nodes[0], NodeId::ROOT);
        assert_eq!(nodes[1], para1);
        assert_eq!(nodes[2], text);
        assert_eq!(nodes[3], para2);
    }

    #[test]
    fn test_style_interning() {
        let mut pool = StylePool::new();

        let style1 = ComputedStyle {
            font_weight: FontWeight::BOLD,
            ..Default::default()
        };
        let style2 = ComputedStyle {
            font_weight: FontWeight::BOLD,
            ..Default::default()
        };
        let style3 = ComputedStyle {
            font_weight: FontWeight::NORMAL,
            ..Default::default()
        };

        let id1 = pool.intern(style1);
        let id2 = pool.intern(style2);
        let id3 = pool.intern(style3);

        // Same style should get same ID
        assert_eq!(id1, id2);
        // Different style should get different ID
        assert_ne!(id1, id3);
        // Pool should have 3 styles (default + 2 unique)
        assert_eq!(pool.len(), 3);
    }

    #[test]
    fn test_semantic_map() {
        let mut semantics = SemanticMap::new();
        let node = NodeId(1);

        semantics.set_href(node, "https://example.com");
        semantics.set_alt(node, "An image");

        assert_eq!(semantics.href(node), Some("https://example.com"));
        assert_eq!(semantics.alt(node), Some("An image"));
        assert_eq!(semantics.src(node), None);
    }
}
