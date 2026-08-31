//! The output of layout: a tree of positioned rectangles. Painting,
//! pagination, hit testing and decoration read this and not the document.
//! Each [`Fragment`] keeps the [`NodeId`] it was produced for.

use bokai::model::{NodeId, Role};
use bokai::style::Color;

use crate::font::FaceId;
use crate::geom::{Edges, Rect};

/// What a box is in the tree layout builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Node {
    /// A block box: a paragraph, a table cell, a picture, the root.
    #[default]
    Container,
    /// The `Line`s one block's inline content breaks into.
    Column,
    /// One line of a `Column`, spanning its whole inline size.
    Line,
    /// Glyphs on one line, from one face at one size.
    Run,
}

/// One positioned box.
#[derive(Debug, Clone, PartialEq)]
pub struct Fragment {
    /// The document node this box was produced for.
    pub source: NodeId,
    /// Where this box sits in the layout tree.
    pub kind: Node,
    /// The node's structural role, which tells a heading from a rule without
    /// the document open.
    pub role: Role,
    /// The border box, in chapter coordinates.
    pub rect: Rect,
    /// What this box draws, beyond its own decorations.
    pub content: Content,
    /// Fill behind the content, inside the border box.
    pub background: Option<Color>,
    /// Borders, drawn just inside `rect`.
    pub border: Option<Border>,
    /// Boxes laid out inside this one, in document order.
    pub children: Vec<Fragment>,
}

/// What a fragment draws.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Content {
    /// Nothing of its own — a block, a line box, an inline box.
    #[default]
    Empty,
    /// Glyphs, positioned relative to the fragment's top-left.
    Glyphs(GlyphRun),
    /// A resource drawn to fill the fragment.
    Image(String),
}

/// Glyphs from one face, at one size, in one colour, laid along one line.
#[derive(Debug, Clone, PartialEq)]
pub struct GlyphRun {
    pub face: FaceId,
    /// Em size in device dots.
    pub size: f32,
    pub color: Color,
    /// How the glyphs sit relative to the line they are on.
    pub orientation: Orientation,
    /// Where the glyphs sit, in order.
    pub glyphs: Vec<Glyph>,
    /// Baseline offset from the fragment's top edge, along the block axis.
    pub baseline: f32,
    /// Underline and strike-through, drawn across the run.
    pub underline: bool,
    pub line_through: bool,
}

/// One glyph, placed along the line and across it. A horizontal line, a
/// vertical one and a run turned on its side share the two measurements.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    /// Index into the face, as shaping produced it — not a character.
    pub id: u16,
    /// Byte in the source node's text, or [`Glyph::NO_SOURCE`].
    pub offset: u32,
    /// Distance along the line from the run's start to this glyph's drawing
    /// origin.
    pub along: f32,
    /// Displacement across the line from the run's baseline, positive away
    /// from the edge the line starts at.
    pub across: f32,
}

/// How a run's glyphs stand relative to the line direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    /// A horizontal line, glyphs upright.
    #[default]
    Horizontal,
    /// A vertical line, glyphs upright — CJK in vertical writing.
    Upright,
    /// A vertical line, glyphs turned a quarter turn clockwise — Latin in
    /// vertical writing.
    Sideways,
}

impl Glyph {
    /// The `offset` of a glyph no source character asked for.
    pub const NO_SOURCE: u32 = u32::MAX;

    /// Whether a source character asked for this glyph.
    pub fn is_from_source(&self) -> bool {
        self.offset != Self::NO_SOURCE
    }
}

impl Orientation {
    /// Whether the line this run sits on runs down the page.
    pub fn is_vertical(self) -> bool {
        !matches!(self, Orientation::Horizontal)
    }
}

/// A box's borders: one width and colour per side, sides with no width
/// drawing nothing.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Border {
    pub widths: Edges,
    pub top: Option<Color>,
    pub right: Option<Color>,
    pub bottom: Option<Color>,
    pub left: Option<Color>,
}

impl Border {
    /// Whether any side draws.
    pub fn is_visible(&self) -> bool {
        (self.widths.top > 0.0 && self.top.is_some())
            || (self.widths.right > 0.0 && self.right.is_some())
            || (self.widths.bottom > 0.0 && self.bottom.is_some())
            || (self.widths.left > 0.0 && self.left.is_some())
    }
}

impl Fragment {
    pub fn new(source: NodeId, role: Role, rect: Rect) -> Self {
        Self {
            source,
            kind: Node::Container,
            role,
            rect,
            content: Content::Empty,
            background: None,
            border: None,
            children: Vec::new(),
        }
    }

    /// The same box, stated as another `Node`.
    pub fn as_kind(mut self, kind: Node) -> Self {
        self.kind = kind;
        self
    }

    /// Every box of `kind` in the tree, in document order.
    pub fn of_kind(&self, kind: Node) -> impl Iterator<Item = &Fragment> {
        self.walk().filter(move |f| f.kind == kind)
    }

    /// Every `Node::Line` in the tree, in document order.
    pub fn lines(&self) -> impl Iterator<Item = &Fragment> {
        self.of_kind(Node::Line)
    }

    /// Every fragment in the tree, this one first, then its children in
    /// document order.
    pub fn walk(&self) -> PreOrder<'_> {
        PreOrder { stack: vec![self] }
    }

    /// The fragment produced for `node`, if layout produced one. A node with
    /// `display: none`, and every node inside it, has none.
    pub fn find(&self, node: NodeId) -> Option<&Fragment> {
        self.walk().find(|f| f.source == node)
    }

    /// How many glyphs this fragment and its descendants draw. A [`Glyph`]
    /// carries a face index, not a character.
    pub fn glyph_count(&self) -> usize {
        self.walk()
            .filter_map(|f| match &f.content {
                Content::Glyphs(run) => Some(run.glyphs.len()),
                _ => None,
            })
            .sum()
    }

    /// Shift this fragment and everything inside it.
    pub fn translate(&mut self, dx: f32, dy: f32) {
        self.rect.x += dx;
        self.rect.y += dy;
        for child in &mut self.children {
            child.translate(dx, dy);
        }
    }
}

/// Depth-first iterator over a fragment tree.
pub struct PreOrder<'a> {
    stack: Vec<&'a Fragment>,
}

impl<'a> Iterator for PreOrder<'a> {
    type Item = &'a Fragment;

    fn next(&mut self) -> Option<Self::Item> {
        let fragment = self.stack.pop()?;
        self.stack.extend(fragment.children.iter().rev());
        Some(fragment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: u32) -> Fragment {
        Fragment::new(NodeId(id), Role::Paragraph, Rect::default())
    }

    #[test]
    fn a_walk_visits_children_in_document_order() {
        let mut root = leaf(0);
        root.children = vec![leaf(1), leaf(2)];
        root.children[0].children = vec![leaf(3)];

        let visited: Vec<u32> = root.walk().map(|f| f.source.0).collect();

        assert_eq!(visited, [0, 1, 3, 2]);
    }

    #[test]
    fn translating_a_tree_moves_every_box_in_it() {
        let mut root = leaf(0);
        root.rect = Rect::new(0.0, 0.0, 10.0, 10.0);
        root.children = vec![leaf(1)];
        root.children[0].rect = Rect::new(2.0, 3.0, 4.0, 5.0);

        root.translate(10.0, 20.0);

        assert_eq!(root.rect.x, 10.0);
        assert_eq!(root.children[0].rect.x, 12.0);
        assert_eq!(root.children[0].rect.y, 23.0);
    }
}
