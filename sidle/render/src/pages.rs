//! Cutting a chapter into pages: a cut falls only where no [`Fragment`] is
//! drawn. `reading` grows the way pages advance — down for horizontal text,
//! leftward for `Axis::VerticalRl`.

use crate::flow::{Page, Viewport};
use crate::fragment::{Content, Fragment, Node};
use crate::geom::{Axis, Edges, Rect, Size};

/// Where a chapter divides.
pub struct Pages {
    axis: Axis,
    size: Size,
    margins: Edges,
    /// `Viewport::inline_lead` at this axis.
    lead: f32,
    /// The block extent one page holds, margins excluded.
    extent: f32,
    /// Reading coordinate each page starts at.
    starts: Vec<f32>,
}

impl Pages {
    /// Divide a laid-out chapter into pages the size of the viewport.
    pub fn of(page: &Page, viewport: &Viewport) -> Pages {
        let axis = page.axis;
        let (_, extent) = viewport.content(axis);
        let content = spans(&page.root, axis);
        let first = reading(axis, page.block_extent);
        let last = reading(axis, 0.0);

        Pages {
            axis,
            size: viewport.size,
            margins: viewport.margins,
            lead: viewport.inline_lead(axis),
            extent,
            starts: cut(&content, extent, first.min(last), first.max(last)),
        }
    }

    pub fn count(&self) -> usize {
        self.starts.len()
    }

    pub fn axis(&self) -> Axis {
        self.axis
    }

    /// The region of chapter space page `n` shows: its content, and nothing
    /// of the page after it.
    pub fn window(&self, n: usize) -> Rect {
        let start = self.starts.get(n).copied().unwrap_or(0.0);
        // `starts[n + 1]` is a line's own edge. A window of the whole
        // `extent` reaches past it into the page after.
        let end = self
            .starts
            .get(n + 1)
            .copied()
            .unwrap_or(start + self.extent)
            .min(start + self.extent);
        let span = (end - start).max(0.0);
        let (inline, _) = self.content();
        // A reading coordinate back to a physical block position.
        let block_start = match self.axis {
            Axis::VerticalRl => -end,
            _ => start,
        };
        match self.axis {
            Axis::HorizontalTb => Rect::new(0.0, block_start, inline, span),
            _ => Rect::new(block_start, 0.0, span, inline),
        }
    }

    /// Where page `n`'s content sits: `margins` plus `lead`. A vertical
    /// `axis` splits what no whole block claims of `extent` between
    /// `margins.left` and `margins.right`.
    pub fn origin(&self, n: usize) -> (f32, f32) {
        if self.axis == Axis::HorizontalTb {
            return (self.margins.left + self.lead, self.margins.top);
        }
        let slack = (self.extent - self.window(n).width).max(0.0) / 2.0;
        (self.margins.left + slack, self.margins.top + self.lead)
    }

    /// The content area, as an inline extent and a block extent.
    fn content(&self) -> (f32, f32) {
        let margins = self.axis.logical_edges(self.margins);
        (
            (self.axis.inline_of(self.size) - margins.inline()).max(1.0),
            self.extent,
        )
    }
}

/// A physical block coordinate as a reading coordinate.
fn reading(axis: Axis, block: f32) -> f32 {
    match axis {
        Axis::VerticalRl => -block,
        _ => block,
    }
}

/// What a page is cut between, along the reading direction: a [`Node::Line`]'s
/// own box stands for everything on it, and anything drawn outside a line
/// stands for itself.
fn spans(root: &Fragment, axis: Axis) -> Vec<(f32, f32)> {
    let mut out = Vec::new();
    extents(root, axis, &mut out);
    out
}

fn extents(fragment: &Fragment, axis: Axis, out: &mut Vec<(f32, f32)>) {
    let line = fragment.kind == Node::Line;
    if line || !matches!(fragment.content, Content::Empty) {
        let (near, far) = if axis.is_vertical() {
            (fragment.rect.x, fragment.rect.right())
        } else {
            (fragment.rect.y, fragment.rect.bottom())
        };
        out.push(match axis {
            Axis::VerticalRl => (-far, -near),
            _ => (near, far),
        });
    }
    if line {
        return;
    }
    for child in &fragment.children {
        extents(child, axis, out);
    }
}

/// Page starts, in reading order. A page ends at the furthest point no span
/// crosses. A span longer than `extent` takes the page whole.
fn cut(spans: &[(f32, f32)], extent: f32, first: f32, last: f32) -> Vec<f32> {
    let mut starts = vec![first];
    if extent <= 0.0 {
        return starts;
    }
    let mut at = first;
    while at + extent < last {
        let target = at + extent;
        let settled = spans
            .iter()
            .filter(|(_, end)| *end <= target + 0.01 && *end > at + 0.01)
            .map(|(_, end)| *end)
            .fold(f32::NEG_INFINITY, f32::max);
        at = if settled.is_finite() { settled } else { target };
        starts.push(at);
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_ends_where_nothing_is_drawn_across_it() {
        // Lines 20 tall; a 50-tall page fits two and must not slice the third.
        let lines: Vec<(f32, f32)> = (0..10)
            .map(|n| (n as f32 * 20.0, n as f32 * 20.0 + 20.0))
            .collect();

        let starts = cut(&lines, 50.0, 0.0, 200.0);

        assert_eq!(starts[0], 0.0);
        assert_eq!(starts[1], 40.0);
        assert_eq!(starts[2], 80.0);
    }

    #[test]
    fn something_taller_than_a_page_still_advances() {
        let tall = [(0.0f32, 500.0f32)];

        let starts = cut(&tall, 100.0, 0.0, 500.0);

        assert!(starts.len() > 1, "pagination must not stall");
        assert_eq!(starts[1], 100.0);
    }

    #[test]
    fn a_chapter_shorter_than_a_page_is_one_page() {
        let starts = cut(&[(0.0, 30.0)], 100.0, 0.0, 30.0);

        assert_eq!(starts, [0.0]);
    }
}
