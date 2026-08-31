//! [`Decorations`] reads a [`Fragment`] tree into one [`Mark`] per line a
//! [`Span`] covers.

use bokai::model::{NodeId, Role};
use bokai::style::Color;

use crate::fragment::{Content, Fragment, Node};
use crate::geom::Rect;

/// A byte range of one text node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub node: NodeId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(node: NodeId, start: u32, end: u32) -> Self {
        Self { node, start, end }
    }

    /// Every byte of `node`.
    pub fn whole(node: NodeId) -> Self {
        Self::new(node, 0, u32::MAX)
    }

    fn holds(&self, offset: u32) -> bool {
        offset >= self.start && offset < self.end
    }
}

/// One box to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    /// Chapter coordinates.
    pub rect: Rect,
    /// The node the covered text belongs to.
    pub source: NodeId,
    pub kind: Kind,
}

/// What a `Mark` is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Link,
    Selection,
    Highlight(Tint),
}

/// A `Highlight` colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    Yellow,
    Blue,
    Pink,
    Orange,
}

impl Tint {
    pub fn color(self) -> Color {
        let (r, g, b) = match self {
            Tint::Yellow => (0xff, 0xe9, 0x66),
            Tint::Blue => (0x8e, 0xc7, 0xff),
            Tint::Pink => (0xff, 0xa8, 0xd0),
            Tint::Orange => (0xff, 0xb5, 0x6b),
        };
        Color { r, g, b, a: 0xff }
    }
}

/// `Mark`s over one laid-out chapter.
pub struct Decorations<'a> {
    root: &'a Fragment,
}

impl<'a> Decorations<'a> {
    pub fn of(root: &'a Fragment) -> Self {
        Self { root }
    }

    /// One `Mark` per line each `Role::Link` covers.
    pub fn links(&self) -> Vec<Mark> {
        let mut marks = Vec::new();
        collect_links(self.root, None, &mut marks);
        marks
    }

    /// One `Mark` per line `span` covers.
    pub fn span(&self, span: Span, kind: Kind) -> Vec<Mark> {
        self.root
            .lines()
            .filter_map(|line| {
                covered(line, span).map(|rect| Mark {
                    rect,
                    source: span.node,
                    kind,
                })
            })
            .collect()
    }

    /// The `Span` of one byte drawn at `point`, `None` off every glyph.
    pub fn at(&self, point: (f32, f32)) -> Option<Span> {
        for run in self.root.of_kind(Node::Run) {
            if !run.rect.intersects(&Rect::new(point.0, point.1, 1.0, 1.0)) {
                continue;
            }
            let Content::Glyphs(glyphs) = &run.content else {
                continue;
            };
            let vertical = glyphs.orientation.is_vertical();
            let along = if vertical {
                point.1 - run.rect.y
            } else {
                point.0 - run.rect.x
            };
            let hit = glyphs
                .glyphs
                .iter()
                .filter(|glyph| glyph.is_from_source() && glyph.along <= along)
                .max_by(|a, b| a.along.total_cmp(&b.along))
                .or_else(|| glyphs.glyphs.iter().find(|g| g.is_from_source()))?;
            return Some(Span::new(run.source, hit.offset, hit.offset + 1));
        }
        None
    }
}

/// Every `Role::Link` area under `fragment`, one per line.
fn collect_links(fragment: &Fragment, link: Option<NodeId>, marks: &mut Vec<Mark>) {
    let link = if fragment.role == Role::Link {
        Some(fragment.source)
    } else {
        link
    };
    if let (Some(source), Node::Line) = (link, fragment.kind)
        && let Some(rect) = drawn_extent(fragment)
    {
        marks.push(Mark {
            rect,
            source,
            kind: Kind::Link,
        });
        return;
    }
    for child in &fragment.children {
        collect_links(child, link, marks);
    }
}

/// The part of `line` that `span` covers.
fn covered(line: &Fragment, span: Span) -> Option<Rect> {
    let mut area: Option<Rect> = None;
    for run in line.of_kind(Node::Run) {
        if run.source != span.node {
            continue;
        }
        let Content::Glyphs(glyphs) = &run.content else {
            continue;
        };
        let vertical = glyphs.orientation.is_vertical();
        let hits: Vec<f32> = glyphs
            .glyphs
            .iter()
            .filter(|glyph| glyph.is_from_source() && span.holds(glyph.offset))
            .map(|glyph| glyph.along)
            .collect();
        let (Some(first), Some(last)) = (
            hits.iter().copied().reduce(f32::min),
            hits.iter().copied().reduce(f32::max),
        ) else {
            continue;
        };
        // `run.rect` bounds the last glyph covered: `Glyph` records no
        // advance of its own.
        let extent = if vertical {
            run.rect.height
        } else {
            run.rect.width
        };
        let end = if hits.len() == glyphs.glyphs.len() {
            extent
        } else {
            last + (extent - last).min(glyphs.size)
        };
        let piece = if vertical {
            Rect::new(run.rect.x, run.rect.y + first, run.rect.width, end - first)
        } else {
            Rect::new(run.rect.x + first, run.rect.y, end - first, run.rect.height)
        };
        area = Some(match area {
            Some(held) => union(held, piece),
            None => piece,
        });
    }
    area
}

/// The area `line`'s runs draw in.
fn drawn_extent(line: &Fragment) -> Option<Rect> {
    line.of_kind(Node::Run)
        .filter(|run| !matches!(run.content, Content::Empty))
        .map(|run| run.rect)
        .reduce(union)
}

fn union(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect::new(
        x,
        y,
        a.right().max(b.right()) - x,
        a.bottom().max(b.bottom()) - y,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::FaceId;
    use crate::fragment::{Glyph, GlyphRun, Orientation};

    fn black() -> Color {
        Color {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        }
    }

    /// A line at `y` holding one run of `count` glyphs, ten dots apart.
    fn line(source: u32, y: f32, count: u32, role: Role) -> Fragment {
        let mut run = Fragment::new(
            NodeId(source),
            Role::Text,
            Rect::new(0.0, y, count as f32 * 10.0, 20.0),
        )
        .as_kind(Node::Run);
        run.content = Content::Glyphs(GlyphRun {
            face: FaceId(0),
            size: 10.0,
            color: black(),
            orientation: Orientation::Horizontal,
            glyphs: (0..count)
                .map(|n| Glyph {
                    id: 1,
                    offset: n,
                    along: n as f32 * 10.0,
                    across: 0.0,
                })
                .collect(),
            baseline: 16.0,
            underline: false,
            line_through: false,
        });
        let mut line =
            Fragment::new(NodeId(source), role, Rect::new(0.0, y, 200.0, 20.0)).as_kind(Node::Line);
        line.children = vec![run];
        line
    }

    #[test]
    fn a_span_over_two_lines_draws_a_box_on_each() {
        let mut root = Fragment::new(NodeId(0), Role::Paragraph, Rect::new(0.0, 0.0, 200.0, 40.0));
        root.children = vec![
            line(1, 0.0, 5, Role::Paragraph),
            line(1, 20.0, 5, Role::Paragraph),
        ];

        let marks = Decorations::of(&root).span(Span::whole(NodeId(1)), Kind::Selection);

        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0].rect.y, 0.0);
        assert_eq!(marks[1].rect.y, 20.0);
    }

    #[test]
    fn a_span_over_part_of_a_line_covers_only_that_part() {
        let mut root = Fragment::new(NodeId(0), Role::Paragraph, Rect::new(0.0, 0.0, 200.0, 20.0));
        root.children = vec![line(1, 0.0, 5, Role::Paragraph)];

        let marks = Decorations::of(&root).span(Span::new(NodeId(1), 1, 3), Kind::Selection);

        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].rect.x, 10.0);
        assert!(marks[0].rect.width < 50.0, "{:?}", marks[0].rect);
    }

    #[test]
    fn a_span_naming_another_node_covers_nothing() {
        let mut root = Fragment::new(NodeId(0), Role::Paragraph, Rect::new(0.0, 0.0, 200.0, 20.0));
        root.children = vec![line(1, 0.0, 5, Role::Paragraph)];

        assert!(
            Decorations::of(&root)
                .span(Span::whole(NodeId(9)), Kind::Selection)
                .is_empty()
        );
    }

    #[test]
    fn a_link_gives_one_mark_per_line_it_covers() {
        let mut link = Fragment::new(NodeId(1), Role::Link, Rect::new(0.0, 0.0, 200.0, 40.0));
        link.children = vec![
            line(2, 0.0, 5, Role::Paragraph),
            line(2, 20.0, 5, Role::Paragraph),
        ];
        let mut root = Fragment::new(NodeId(0), Role::Paragraph, Rect::new(0.0, 0.0, 200.0, 40.0));
        root.children = vec![link];

        let marks = Decorations::of(&root).links();

        assert_eq!(marks.len(), 2);
        assert!(marks.iter().all(|mark| mark.source == NodeId(1)));
        assert!(marks.iter().all(|mark| mark.kind == Kind::Link));
    }

    #[test]
    fn a_page_with_no_link_gives_no_marks() {
        let mut root = Fragment::new(NodeId(0), Role::Paragraph, Rect::new(0.0, 0.0, 200.0, 20.0));
        root.children = vec![line(1, 0.0, 5, Role::Paragraph)];

        assert!(Decorations::of(&root).links().is_empty());
    }

    #[test]
    fn a_point_on_a_glyph_names_the_byte_under_it() {
        let mut root = Fragment::new(NodeId(0), Role::Paragraph, Rect::new(0.0, 0.0, 200.0, 20.0));
        root.children = vec![line(1, 0.0, 5, Role::Paragraph)];

        let hit = Decorations::of(&root).at((25.0, 10.0)).expect("a glyph");

        assert_eq!(hit.node, NodeId(1));
        assert_eq!(hit.start, 2);
    }

    #[test]
    fn a_point_off_every_glyph_names_nothing() {
        let mut root = Fragment::new(NodeId(0), Role::Paragraph, Rect::new(0.0, 0.0, 200.0, 20.0));
        root.children = vec![line(1, 0.0, 5, Role::Paragraph)];

        assert_eq!(Decorations::of(&root).at((500.0, 500.0)), None);
    }
}
