//! Geometry in device dots — see [`crate::units`] for how a declared value
//! becomes one.
//!
//! One coordinate space per chapter: the origin is the top-left of its first
//! box and `y` grows downward. A chapter is one tall strip, which
//! [`crate::pages`] cuts.

/// A width and a height.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

/// An axis-aligned rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    /// Whether this rectangle overlaps `other`. Touching edges do not count.
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    /// This rectangle inset on every side by `edges`. A box whose insets
    /// exceed its own size collapses to zero.
    pub fn inset(&self, edges: Edges) -> Rect {
        let width = (self.width - edges.left - edges.right).max(0.0);
        let height = (self.height - edges.top - edges.bottom).max(0.0);
        Rect::new(self.x + edges.left, self.y + edges.top, width, height)
    }
}

/// Four per-side lengths — the shape of margin, border width and padding.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Edges {
    pub const ZERO: Edges = Edges {
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
        left: 0.0,
    };

    pub const fn new(top: f32, right: f32, bottom: f32, left: f32) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    /// Total inset along the inline axis.
    pub fn horizontal(&self) -> f32 {
        self.left + self.right
    }

    /// Total inset along the block axis.
    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
    }
}

/// The direction lines run and blocks stack.
///
/// [`Axis::rect`] turns an inline offset and a block offset into a physical
/// rectangle, one implementation for all three values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Axis {
    /// Lines run left to right; blocks stack downward.
    #[default]
    HorizontalTb,
    /// Lines run top to bottom; blocks stack right to left.
    VerticalRl,
    /// Lines run top to bottom; blocks stack left to right.
    VerticalLr,
}

impl From<bokai::style::WritingMode> for Axis {
    fn from(mode: bokai::style::WritingMode) -> Self {
        match mode {
            bokai::style::WritingMode::HorizontalTb => Axis::HorizontalTb,
            bokai::style::WritingMode::VerticalRl => Axis::VerticalRl,
            bokai::style::WritingMode::VerticalLr => Axis::VerticalLr,
        }
    }
}

impl Axis {
    pub fn is_vertical(self) -> bool {
        !matches!(self, Axis::HorizontalTb)
    }

    /// The inline extent of a physical size — the page's line length.
    pub fn inline_of(self, size: Size) -> f32 {
        if self.is_vertical() {
            size.height
        } else {
            size.width
        }
    }

    /// The block extent of a physical size — how far the page stacks.
    pub fn block_of(self, size: Size) -> f32 {
        if self.is_vertical() {
            size.width
        } else {
            size.height
        }
    }

    /// A physical rectangle for a logical box, given the block extent of the
    /// whole page (which is what `vertical-rl` measures its blocks back from).
    pub fn rect(
        self,
        inline: f32,
        block: f32,
        inline_size: f32,
        block_size: f32,
        page: f32,
    ) -> Rect {
        match self {
            Axis::HorizontalTb => Rect::new(inline, block, inline_size, block_size),
            Axis::VerticalLr => Rect::new(block, inline, block_size, inline_size),
            Axis::VerticalRl => {
                Rect::new(page - block - block_size, inline, block_size, inline_size)
            }
        }
    }

    /// Physical edges read as logical ones. CSS keeps `margin-top` physical,
    /// so which side of a box it insets depends on the axis.
    pub fn logical_edges(self, edges: Edges) -> LogicalEdges {
        match self {
            Axis::HorizontalTb => LogicalEdges {
                block_start: edges.top,
                block_end: edges.bottom,
                inline_start: edges.left,
                inline_end: edges.right,
            },
            Axis::VerticalRl => LogicalEdges {
                block_start: edges.right,
                block_end: edges.left,
                inline_start: edges.top,
                inline_end: edges.bottom,
            },
            Axis::VerticalLr => LogicalEdges {
                block_start: edges.left,
                block_end: edges.right,
                inline_start: edges.top,
                inline_end: edges.bottom,
            },
        }
    }
}

/// A box's insets in logical terms.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LogicalEdges {
    pub block_start: f32,
    pub block_end: f32,
    pub inline_start: f32,
    pub inline_end: f32,
}

impl LogicalEdges {
    /// Total inset along the line.
    pub fn inline(&self) -> f32 {
        self.inline_start + self.inline_end
    }

    /// Total inset across it.
    pub fn block(&self) -> f32 {
        self.block_start + self.block_end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_horizontal_box_keeps_its_logical_measurements() {
        let rect = Axis::HorizontalTb.rect(10.0, 20.0, 100.0, 50.0, 600.0);

        assert_eq!(rect, Rect::new(10.0, 20.0, 100.0, 50.0));
    }

    #[test]
    fn vertical_rl_stacks_blocks_leftward_from_the_right_edge() {
        // A 50-wide block at block offset 0 sits against the right edge of a
        // 600-wide page; the next one sits to its left.
        let first = Axis::VerticalRl.rect(0.0, 0.0, 800.0, 50.0, 600.0);
        let second = Axis::VerticalRl.rect(0.0, 50.0, 800.0, 50.0, 600.0);

        assert_eq!(first, Rect::new(550.0, 0.0, 50.0, 800.0));
        assert_eq!(second, Rect::new(500.0, 0.0, 50.0, 800.0));
    }

    #[test]
    fn vertical_lr_stacks_blocks_rightward_from_the_left_edge() {
        let first = Axis::VerticalLr.rect(0.0, 0.0, 800.0, 50.0, 600.0);

        assert_eq!(first, Rect::new(0.0, 0.0, 50.0, 800.0));
    }

    #[test]
    fn a_top_margin_starts_the_line_in_vertical_writing() {
        let edges = Edges::new(4.0, 3.0, 2.0, 1.0);

        let horizontal = Axis::HorizontalTb.logical_edges(edges);
        assert_eq!(horizontal.block_start, 4.0);
        assert_eq!(horizontal.inline_start, 1.0);

        // Turned a quarter turn: the top edge is where the line begins and
        // the right edge is where the blocks do.
        let vertical = Axis::VerticalRl.logical_edges(edges);
        assert_eq!(vertical.block_start, 3.0);
        assert_eq!(vertical.inline_start, 4.0);
    }

    #[test]
    fn a_box_narrower_than_its_own_padding_collapses_instead_of_inverting() {
        let outer = Rect::new(0.0, 0.0, 10.0, 10.0);
        let inner = outer.inset(Edges::new(20.0, 20.0, 20.0, 20.0));

        assert_eq!(inner.width, 0.0);
        assert_eq!(inner.height, 0.0);
    }

    #[test]
    fn touching_rectangles_do_not_intersect() {
        let above = Rect::new(0.0, 0.0, 10.0, 10.0);
        let below = Rect::new(0.0, 10.0, 10.0, 10.0);

        assert!(!above.intersects(&below));
        assert!(above.intersects(&Rect::new(0.0, 9.0, 10.0, 10.0)));
    }
}
