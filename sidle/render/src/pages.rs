//! Cutting a chapter into pages.
//!
//! A laid-out chapter is one long strip along the block axis. A page is a cut
//! across it, falling only where no [`Fragment`] is drawn.
//!
//! Every measurement is in a reading coordinate that grows the way pages
//! advance: down the page for horizontal text, leftward for `vertical-rl`.

use crate::flow::{Page, Viewport};
use crate::fragment::{Content, Fragment};
use crate::geom::{Axis, Edges, Rect, Size};

/// Where a chapter divides.
pub struct Pages {
    axis: Axis,
    size: Size,
    margins: Edges,
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
        let (inline, _) = self.content();
        // Back from the reading coordinate to a physical block position: the
        // page's near edge in reading order is its right edge on a
        // `vertical-rl` page and its top edge on a horizontal one.
        let block_start = match self.axis {
            Axis::VerticalRl => -(start + self.extent),
            _ => start,
        };
        match self.axis {
            Axis::HorizontalTb => Rect::new(0.0, block_start, inline, self.extent),
            _ => Rect::new(block_start, 0.0, self.extent, inline),
        }
    }

    /// Where a page's content sits on the page, in CSS pixels — the margin
    /// the window is drawn at.
    pub fn origin(&self) -> (f32, f32) {
        (self.margins.left, self.margins.top)
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

/// Every drawn fragment's extent along the reading direction.
fn spans(root: &Fragment, axis: Axis) -> Vec<(f32, f32)> {
    root.walk()
        .filter(|fragment| !matches!(fragment.content, Content::Empty))
        .map(|fragment| {
            let (near, far) = if axis.is_vertical() {
                (fragment.rect.x, fragment.rect.right())
            } else {
                (fragment.rect.y, fragment.rect.bottom())
            };
            match axis {
                Axis::VerticalRl => (-far, -near),
                _ => (near, far),
            }
        })
        .collect()
}

/// Page starts, in reading order. A page ends at the furthest point no span
/// crosses; a single item taller than a page is given the page anyway rather
/// than stalling.
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
