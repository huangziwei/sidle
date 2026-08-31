//! One line of chrome text, shaped by the same engine that sets a page.

use bokai::model::{NodeId, Role};
use bokai::style::{Color, ComputedStyle, FontWeight, TextAlign};

use crate::font::Fonts;
use crate::fragment::Fragment;
use crate::geom::Axis;
use crate::inline::{Inline, Item, TextStyle};

/// Where a laid-out string sits against the point it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// A shaped string: what to paint, how wide it came out, and how far its
/// baseline sits below its top.
pub struct Line {
    pub fragments: Vec<Fragment>,
    pub width: f32,
    pub height: f32,
    pub baseline: f32,
}

/// Shape `text` at `size` in `color`, on one line.
pub fn lay(fonts: &mut Fonts, text: &str, size: f32, color: Color, bold: bool) -> Line {
    let computed = ComputedStyle {
        font_weight: FontWeight(if bold { 700 } else { 400 }),
        ..ComputedStyle::default()
    };
    let style = TextStyle {
        computed: &computed,
        font_size: size,
        line_height: size * 1.3,
        color,
        letter_spacing: 0.0,
        word_spacing: 0.0,
        embolden: 0.0,
        underline: false,
        line_through: false,
        preserve_spaces: false,
    };
    let items = vec![Item::Text {
        source: NodeId(0),
        style,
        text: text.to_string(),
    }];
    let mut inline = Inline {
        fonts,
        axis: Axis::HorizontalTb,
        hyphenator: None,
    };
    let (_, width) = inline.measure(&items);
    let laid = inline.lay_out(&items, width.max(1.0), 0.0, TextAlign::Start, size * 1.3);

    let baseline = laid
        .fragments
        .iter()
        .flat_map(|fragment| fragment.walk())
        .find_map(|fragment| match &fragment.content {
            crate::fragment::Content::Glyphs(run) => Some(fragment.rect.y + run.baseline),
            _ => None,
        })
        .unwrap_or(size);

    Line {
        width,
        height: laid.block_size.max(size * 1.3),
        baseline,
        fragments: laid.fragments,
    }
}

/// Shape `text` and place it at `at`, whose `1` is the line's top.
pub fn place(
    fonts: &mut Fonts,
    text: &str,
    size: f32,
    color: Color,
    bold: bool,
    at: (f32, f32),
    align: Align,
) -> Line {
    let mut line = lay(fonts, text, size, color, bold);
    let dx = match align {
        Align::Left => at.0,
        Align::Center => at.0 - line.width / 2.0,
        Align::Right => at.0 - line.width,
    };
    for fragment in &mut line.fragments {
        fragment.translate(dx, at.1);
    }
    line
}

/// A tree holding `fragments`, as [`crate::paint::Painter`] takes one.
pub fn tree(fragments: Vec<Fragment>) -> Fragment {
    let mut root = Fragment::new(NodeId(0), Role::Container, crate::geom::Rect::default());
    root.children = fragments;
    root
}
