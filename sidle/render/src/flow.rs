//! Block layout: boxes stacked along the block axis, measured as an inline
//! and a block size and turned into rectangles once by `to_physical`.
//! Adjacent margins add. Inline children go to [`crate::inline`].

use std::sync::LazyLock;

use bokai::model::{Chapter, NodeId, Role};
use bokai::style::{
    BorderStyle, BoxAlign, Color, ComputedStyle, Display, Length, ListStyleType, ROOT_FONT_SIZE_PX,
    StyleId, TextAlign, WritingMode,
};
use bokai::text::hyphenation::{self, Hyphenator};

use crate::font::Fonts;
use crate::fragment::{Border, Content, Fragment, GlyphRun, Node};
use crate::geom::{Axis, Edges, LogicalEdges, Rect, Size};
use crate::inline::{self, Inline, Item, TextStyle};
use crate::resolve::{NORMAL_LINE_HEIGHT, Resolver};
use crate::resource::Resources;
use crate::settings::Script;
use crate::units::Metrics;

/// The area a chapter is laid out into.
#[derive(Debug, Clone, PartialEq)]
pub struct Viewport {
    /// The whole page, margins included, in device dots.
    pub size: Size,
    /// Blank border the text does not enter.
    pub margins: Edges,
    /// Font size `rem` resolves against, in dots.
    pub root_font_size: f32,
    /// The book's language, which chooses the hyphenation dictionary.
    pub language: Option<String>,
    /// How a value the source declared becomes a dot. The source format
    /// decides it, and the caller that opened the book states it.
    pub metrics: Metrics,
    /// Passed to `Resolver::line_spacing`.
    pub line_spacing: f32,
    /// Used where a block declares no `text_align` of its own.
    pub align: TextAlign,
    /// Passed to `Resolver::embolden_weight`.
    pub embolden_weight: f32,
    /// Passed to `Resolver::character_spacing`.
    pub character_spacing: f32,
    /// Passed to `Resolver::word_spacing`.
    pub word_spacing: f32,
    /// Extra space before a paragraph, in ems.
    pub paragraph_spacing: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            size: Size::new(600.0, 800.0),
            margins: Edges::new(48.0, 40.0, 48.0, 40.0),
            root_font_size: ROOT_FONT_SIZE_PX,
            language: None,
            metrics: Metrics::default(),
            line_spacing: 1.0,
            align: TextAlign::Start,
            embolden_weight: 0.0,
            character_spacing: 0.0,
            word_spacing: 0.0,
            paragraph_spacing: 0.0,
        }
    }
}

impl Viewport {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            size: Size::new(width, height),
            ..Self::default()
        }
    }

    pub fn with_margins(mut self, margins: Edges) -> Self {
        self.margins = margins;
        self
    }

    pub fn with_language(mut self, language: Option<String>) -> Self {
        self.language = language;
        self
    }

    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = metrics;
        self
    }

    /// A `Resolver` at these settings.
    pub fn resolver(&self) -> Resolver {
        Resolver {
            metrics: self.metrics,
            root_font_size: self.root_font_size,
            line_spacing: self.line_spacing,
            embolden_weight: self.embolden_weight,
            character_spacing: self.character_spacing,
            word_spacing: self.word_spacing,
            paragraph_spacing: self.paragraph_spacing,
        }
    }

    /// The page less its margins, as an inline and a block extent.
    /// On an em grid the inline extent is a whole number of ems.
    pub fn content(&self, axis: Axis) -> (f32, f32) {
        let margins = axis.logical_edges(self.margins);
        let available = (axis.inline_of(self.size) - margins.inline()).max(1.0);
        let block = (axis.block_of(self.size) - margins.block()).max(1.0);
        (available - self.grid_remainder(axis), block)
    }

    /// Half `grid_remainder`, rounded down.
    pub fn inline_lead(&self, axis: Axis) -> f32 {
        (self.grid_remainder(axis) / 2.0).floor()
    }

    /// The measure no whole cell claims, rounded to a dot. Zero off the grid.
    fn grid_remainder(&self, axis: Axis) -> f32 {
        if !self.on_an_em_grid() || self.root_font_size <= 0.0 {
            return 0.0;
        }
        let margins = axis.logical_edges(self.margins);
        let available = (axis.inline_of(self.size) - margins.inline()).max(1.0);
        let cells = (available / self.root_font_size).floor().max(1.0);
        (available - cells * self.root_font_size).max(0.0).round()
    }

    /// Whether `language` reads as `Script::Cjk`.
    fn on_an_em_grid(&self) -> bool {
        self.language
            .as_deref()
            .is_some_and(|tag| Script::of(tag) == Script::Cjk)
    }
}

/// Everything layout needs besides the chapter itself.
pub struct Layout<'a> {
    pub viewport: Viewport,
    pub fonts: &'a mut Fonts,
    pub resources: &'a dyn Resources,
    /// The axis the book states it is written along. A chapter whose own
    /// styles declare an axis overrides it.
    pub axis: Axis,
}

/// A laid-out chapter.
pub struct Page {
    /// The outermost box, spanning the whole chapter — which pagination
    /// later cuts into pages.
    pub root: Fragment,
    /// The direction it was written along, which states the edge a page
    /// opens at and the way its pages turn.
    pub axis: Axis,
    /// How far the chapter reaches along that direction.
    pub block_extent: f32,
    /// Whether the outermost styled box states `BoxAlign::Center`.
    pub centred: bool,
}

impl Layout<'_> {
    /// Lay a chapter out.
    pub fn chapter(&mut self, chapter: &Chapter) -> Page {
        let axis = axis_of(chapter).unwrap_or(self.axis);
        let (inline_size, page_block) = self.viewport.content(axis);
        let language = self.viewport.language.as_deref().unwrap_or("");
        let resolver = self.viewport.resolver();
        let across = axis.is_vertical() != self.axis.is_vertical();
        let mut flow = Flow {
            chapter,
            fonts: self.fonts,
            resources: self.resources,
            resolver,
            axis,
            across,
            across_pitch: across.then(|| {
                resolver
                    .normal_line_height(self.viewport.root_font_size)
                    .round()
            }),
            across_from_end: self.axis == Axis::VerticalRl,
            page_block,
            grid: self
                .viewport
                .on_an_em_grid()
                .then_some(self.viewport.root_font_size),
            hyphenator: hyphenation::for_language(language),
        };

        let inherited = Inherited {
            font_size: self.viewport.root_font_size,
            line_height: resolver.normal_line_height(self.viewport.root_font_size),
            color: Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
            align: self.viewport.align,
            centred: false,
        };
        let mut root = flow
            .block(chapter.root(), 0.0, 0.0, inline_size, inherited)
            .fragment;

        // A vertical page measures its blocks back from the far edge. The
        // conversion takes the chapter's whole block extent.
        let page_block = root.rect.height + root.rect.y;
        to_physical(&mut root, axis, page_block);
        Page {
            root,
            axis,
            block_extent: page_block,
            centred: centred(chapter),
        }
    }
}

/// CSS's size for a replaced box whose own proportions are unknown.
const DEFAULT_OBJECT: Size = Size {
    width: 300.0,
    height: 150.0,
};

/// The `Axis` `chapter` is written along: the writing mode of its outermost
/// styled box, which a section may state against the book's own. `None` where
/// no box carries a style of its own.
fn axis_of(chapter: &Chapter) -> Option<Axis> {
    Some(
        match style_of(chapter, outermost_styled(chapter)?).writing_mode {
            WritingMode::VerticalRl => Axis::VerticalRl,
            WritingMode::VerticalLr => Axis::VerticalLr,
            WritingMode::HorizontalTb => Axis::HorizontalTb,
        },
    )
}

/// Whether `chapter`'s outermost styled box states [`BoxAlign::Center`].
fn centred(chapter: &Chapter) -> bool {
    outermost_styled(chapter)
        .map(|node| style_of(chapter, node).box_align)
        .is_some_and(|align| align == BoxAlign::Center)
}

/// The first box in `chapter` carrying a style of its own.
fn outermost_styled(chapter: &Chapter) -> Option<NodeId> {
    chapter.iter_dfs().find(|node| {
        chapter
            .node(*node)
            .is_some_and(|n| n.style != StyleId::DEFAULT)
    })
}

static INITIAL_STYLE: LazyLock<ComputedStyle> = LazyLock::new(ComputedStyle::default);

fn style_of(chapter: &Chapter, node: NodeId) -> &ComputedStyle {
    chapter
        .node(node)
        .and_then(|n| chapter.styles.get(n.style))
        .unwrap_or(&INITIAL_STYLE)
}

fn role_of(chapter: &Chapter, node: NodeId) -> Role {
    chapter.node(node).map_or(Role::Container, |n| n.role)
}

/// Style a box takes from its parent where it declares none of its own.
#[derive(Debug, Clone, Copy)]
struct Inherited {
    font_size: f32,
    line_height: f32,
    color: Color,
    align: TextAlign,
    /// Whether an enclosing box states `BoxAlign::Center`, which places a
    /// picture it holds.
    centred: bool,
}

/// A laid-out box and the block extent it claims, its own margins included.
struct Laid {
    fragment: Fragment,
    outer: f32,
}

struct Flow<'a, 'f> {
    chapter: &'a Chapter,
    fonts: &'f mut Fonts,
    resources: &'a dyn Resources,
    resolver: Resolver,
    axis: Axis,
    /// Whether `axis` crosses the one the book at large is written along.
    across: bool,
    /// How far a page reaches along the block axis, which is the most a
    /// picture may take.
    page_block: f32,
    /// The em-grid cell a book holds its boxes to, `None` off the grid.
    grid: Option<f32>,
    /// The pitch of the book's lines where they cross the chapter, in dots.
    across_pitch: Option<f32>,
    /// Whether those lines start at the chapter's inline end.
    across_from_end: bool,
    hyphenator: Option<&'static Hyphenator>,
}

impl<'a> Flow<'a, '_> {
    /// Lay out one block-level node whose margin box starts at
    /// `(inline, block)`.
    fn block(
        &mut self,
        node: NodeId,
        inline: f32,
        block: f32,
        available: f32,
        parent: Inherited,
    ) -> Laid {
        let chapter = self.chapter;
        let style = style_of(chapter, node);
        let role = role_of(chapter, node);
        let inherited = self.inherit(parent, style);

        let mut margin = self.logical(
            [
                style.margin_top,
                style.margin_right,
                style.margin_bottom,
                style.margin_left,
            ],
            available,
            inherited,
        );
        if role == Role::Paragraph {
            margin.block_start += self.resolver.paragraph_spacing * inherited.font_size;
        }
        let padding = self.logical(
            [
                style.padding_top,
                style.padding_right,
                style.padding_bottom,
                style.padding_left,
            ],
            available,
            inherited,
        );
        let border = self.border(style, available, inherited);
        let frame = LogicalEdges {
            block_start: padding.block_start + border.widths_logical(self.axis).block_start,
            block_end: padding.block_end + border.widths_logical(self.axis).block_end,
            inline_start: padding.inline_start + border.widths_logical(self.axis).inline_start,
            inline_end: padding.inline_end + border.widths_logical(self.axis).inline_end,
        };

        // A declared inline size is the content's own; a declared cap holds
        // the whole box, padding and border in.
        let outer_inline = (available - margin.inline()).max(0.0);
        let content_inline = match self.declared_inline(style, available, inherited) {
            Some(declared) => declared.max(0.0),
            None => (outer_inline - frame.inline()).max(0.0),
        };
        // The measure no whole cell of the em grid claims splits either side
        // of the content, leaving the box the size the cap gave it.
        let (content_inline, gutter) = match self.capped_inline(style, available, inherited) {
            Some(cap) => {
                let held = content_inline.min((cap - frame.inline()).max(0.0));
                let cells = self.on_grid(held);
                (cells, (held - cells) / 2.0)
            }
            None => (content_inline, 0.0),
        };

        let box_inline = content_inline + frame.inline() + 2.0 * gutter;
        let spare = match self.across {
            true => self.across_lead(outer_inline, box_inline),
            false => 0.0,
        };
        let content_inline_origin =
            inline + margin.inline_start + frame.inline_start + spare + gutter;
        let content_block_origin = block + margin.block_start + frame.block_start;

        let mut children = Vec::new();
        // A block takes its block size from what it holds.
        let content_block = self.children(
            node,
            content_inline_origin,
            content_block_origin,
            content_inline,
            inherited,
            style,
            &mut children,
        );

        let rect = Rect::new(
            inline + margin.inline_start + spare,
            block + margin.block_start,
            box_inline,
            content_block + frame.block(),
        );
        let mut fragment = Fragment::new(node, role, rect);
        fragment.background = style.background_color;
        fragment.border = border.is_visible().then_some(border);
        fragment.children = children;

        Laid {
            outer: margin.block() + rect.height,
            fragment,
        }
    }

    /// Lay out a node's children into its content box, returning the block
    /// extent they took.
    #[allow(clippy::too_many_arguments)]
    fn children(
        &mut self,
        node: NodeId,
        inline: f32,
        block: f32,
        available: f32,
        inherited: Inherited,
        style: &ComputedStyle,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        let chapter = self.chapter;
        let mut cursor = block;
        let mut items: Vec<Item<'a>> = Vec::new();

        if role_of(chapter, node) == Role::ListItem {
            self.marker(node, inherited, &mut items);
        }

        for child in chapter.children(node) {
            let role = role_of(chapter, child);
            let child_style = style_of(chapter, child);
            if !produces_a_box(role) || child_style.display == Display::None {
                continue;
            }

            if role == Role::Table {
                cursor += self.flush(&mut items, inline, cursor, available, inherited, style, out);
                let laid = self.table(child, inline, cursor, available, inherited);
                cursor += laid.outer;
                out.push(laid.fragment);
            } else if is_block_level(role, child_style.display) {
                cursor += self.flush(&mut items, inline, cursor, available, inherited, style, out);
                let laid = self.block(child, inline, cursor, available, inherited);
                cursor += laid.outer;
                out.push(laid.fragment);
            } else {
                self.collect(child, available, inherited, &mut items);
            }
        }

        cursor += self.flush(&mut items, inline, cursor, available, inherited, style, out);
        cursor - block
    }

    /// Break the inline items gathered into lines and place them.
    #[allow(clippy::too_many_arguments)]
    fn flush(
        &mut self,
        items: &mut Vec<Item<'a>>,
        inline: f32,
        block: f32,
        available: f32,
        inherited: Inherited,
        style: &ComputedStyle,
        out: &mut Vec<Fragment>,
    ) -> f32 {
        if items.is_empty() {
            return 0.0;
        }
        let gathered = std::mem::take(items);
        let indent = self
            .length(style.text_indent, available, inherited)
            .unwrap_or(0.0);
        // A box that states `BoxAlign::Center` places the picture it holds;
        // text in it keeps the alignment the block itself carries.
        let picture = gathered
            .iter()
            .all(|item| matches!(item, Item::Replaced { .. }));
        let align = match inherited.centred && picture {
            true => TextAlign::Center,
            false => inherited.align,
        };
        let mut lines = Inline {
            fonts: self.fonts,
            axis: self.axis,
            hyphenator: self.hyphenator,
        }
        .lay_out(&gathered, available, indent, align, inherited.line_height);

        if lines.fragments.is_empty() {
            return lines.block_size;
        }
        for fragment in &mut lines.fragments {
            fragment.translate(inline, block);
        }
        // The lines of one block are a `Node::Column`.
        let source = lines
            .fragments
            .first()
            .map_or(NodeId(0), |fragment| fragment.source);
        let mut column = Fragment::new(
            source,
            Role::Container,
            Rect::new(inline, block, available, lines.block_size),
        )
        .as_kind(Node::Column);
        column.children = std::mem::take(&mut lines.fragments);
        out.push(column);
        lines.block_size
    }

    /// Gather an inline subtree into items.
    fn collect(
        &mut self,
        node: NodeId,
        available: f32,
        parent: Inherited,
        out: &mut Vec<Item<'a>>,
    ) {
        let chapter = self.chapter;
        let style = style_of(chapter, node);
        if style.display == Display::None {
            return;
        }
        let role = role_of(chapter, node);
        let inherited = self.inherit(parent, style);

        match role {
            Role::Text => {
                let Some(entry) = chapter.node(node) else {
                    return;
                };
                let raw = chapter.text(entry.text);
                if raw.is_empty() {
                    return;
                }
                let preserve = inline::preserves_spaces(style.white_space);
                let text = if preserve {
                    raw.to_string()
                } else {
                    inline::collapse(raw)
                };
                out.push(Item::Text {
                    source: node,
                    style: self.text_style(style, inherited, preserve),
                    text,
                });
            }
            Role::Break => out.push(Item::Break { source: node }),
            Role::Ruby => {
                if let Some(item) = self.ruby(node, inherited) {
                    out.push(item);
                    return;
                }
            }
            Role::Image => {
                let size = self.replaced_size(node, style, available, inherited);
                out.push(Item::Replaced {
                    source: node,
                    inline_size: self.axis.inline_of(size),
                    block_size: self.axis.block_of(size),
                    src: chapter.semantics.src(node).unwrap_or_default().to_string(),
                    background: style.background_color,
                });
            }
            _ => {}
        }

        for child in chapter.children(node) {
            self.collect(child, available, inherited, out);
        }
    }

    /// A ruby group: the base text and the annotation set beside it. `None`
    /// where the group carries no annotation, which leaves the base to be
    /// collected as ordinary text.
    fn ruby(&mut self, node: NodeId, parent: Inherited) -> Option<Item<'a>> {
        let chapter = self.chapter;
        let style = style_of(chapter, node);
        let inherited = self.inherit(parent, style);

        let mut base = String::new();
        let mut annotation = String::new();
        let mut annotation_node = None;
        for child in chapter.children(node) {
            if role_of(chapter, child) == Role::RubyText {
                annotation_node.get_or_insert(child);
                gather_text(chapter, child, &mut annotation);
            } else {
                gather_text(chapter, child, &mut base);
            }
        }
        let annotation_node = annotation_node?;
        if annotation.trim().is_empty() || base.is_empty() {
            return None;
        }

        let annotation_style = style_of(chapter, annotation_node);
        let mut marks = self.inherit(inherited, annotation_style);
        // `rt` with no declared size is set at half its parent's.
        if annotation_style.font_size == Length::Auto {
            marks.font_size = inherited.font_size / 2.0;
            marks.line_height = marks.font_size * NORMAL_LINE_HEIGHT;
        }

        Some(Item::Ruby {
            source: node,
            style: self.text_style(style, inherited, false),
            base: inline::collapse(&base),
            annotation_style: self.text_style(annotation_style, marks, false),
            annotation: inline::collapse(&annotation),
        })
    }

    /// A list item's bullet or number, as the first thing on its line.
    fn marker(&mut self, node: NodeId, inherited: Inherited, out: &mut Vec<Item<'a>>) {
        let chapter = self.chapter;
        let style = style_of(chapter, node);
        let kind = list_style_of(chapter, node);
        let Some(text) = marker_text(kind, || ordinal(chapter, node)) else {
            return;
        };
        out.push(Item::Text {
            source: node,
            style: self.text_style(style, inherited, false),
            text,
        });
    }

    fn text_style(
        &self,
        style: &'a ComputedStyle,
        inherited: Inherited,
        preserve_spaces: bool,
    ) -> TextStyle<'a> {
        TextStyle {
            computed: style,
            font_size: inherited.font_size,
            line_height: inherited.line_height,
            color: style.color.unwrap_or(inherited.color),
            letter_spacing: self
                .length(style.letter_spacing, 0.0, inherited)
                .unwrap_or(0.0)
                + self.resolver.tracking(inherited.font_size),
            word_spacing: self
                .length(style.word_spacing, 0.0, inherited)
                .unwrap_or(0.0)
                + self.resolver.word_gap(inherited.font_size),
            embolden: self.resolver.embolden(inherited.font_size),
            underline: style.text_decoration_underline,
            line_through: style.text_decoration_line_through,
            preserve_spaces,
        }
    }

    /// The used size of a replaced box: what the document declares, else what
    /// the resource measures, else CSS's size for an object of unknown
    /// proportions. Scaled down to `available`, keeping its proportions.
    fn replaced_size(
        &self,
        node: NodeId,
        style: &ComputedStyle,
        available: f32,
        inherited: Inherited,
    ) -> Size {
        let intrinsic = self
            .chapter
            .semantics
            .src(node)
            .and_then(|src| self.resources.image_size(src))
            .map(|size| {
                Size::new(
                    self.resolver.metrics.image_px(size.width),
                    self.resolver.metrics.image_px(size.height),
                )
            });
        let unknown = Size::new(
            self.resolver.metrics.css_px(DEFAULT_OBJECT.width),
            self.resolver.metrics.css_px(DEFAULT_OBJECT.height),
        );
        let ratio = intrinsic
            .filter(|size| size.height > 0.0)
            .map(|size| size.width / size.height);
        let declared_w = self.length(style.width, available, inherited);
        let declared_h = self.length(style.height, available, inherited);

        let (mut width, mut height) = match (declared_w, declared_h) {
            (Some(w), Some(h)) => (w, h),
            (Some(w), None) => (
                w,
                ratio.map_or_else(
                    || intrinsic.map_or(unknown.height, |size| size.height),
                    |r| w / r,
                ),
            ),
            (None, Some(h)) => (
                ratio.map_or_else(
                    || intrinsic.map_or(unknown.width, |size| size.width),
                    |r| h * r,
                ),
                h,
            ),
            (None, None) => {
                let size = intrinsic.unwrap_or(unknown);
                (size.width, size.height)
            }
        };

        if let Some(max) = self.length(style.max_width, available, inherited)
            && width > max
            && width > 0.0
        {
            height *= max / width;
            width = max;
        }

        // The page is the last constraint on both axes: a picture wider or
        // taller than it is scaled down to fit.
        let (inline, block) = if self.axis.is_vertical() {
            (height, width)
        } else {
            (width, height)
        };
        let fit = [
            (available / inline, inline > available),
            (self.page_block / block, block > self.page_block),
        ]
        .into_iter()
        .filter(|(ratio, over)| *over && ratio.is_finite() && *ratio > 0.0)
        .map(|(ratio, _)| ratio)
        .fold(1.0f32, f32::min);
        width *= fit;
        height *= fit;

        Size::new(width.max(0.0), height.max(0.0))
    }

    /// The context a node passes to its children: its own declarations where
    /// it made them, its parent's where it did not.
    fn inherit(&self, parent: Inherited, style: &ComputedStyle) -> Inherited {
        let font_size = self.resolver.font_size(style.font_size, parent.font_size);
        let line_height = match style.line_height {
            // A silent style at the parent's size keeps the parent's line.
            Length::Auto if font_size == parent.font_size => parent.line_height,
            declared => self.resolver.line_height(declared, font_size),
        };

        Inherited {
            font_size,
            line_height,
            color: style.color.unwrap_or(parent.color),
            // `start` is what a style carries when the source declared no
            // alignment, and takes `parent.align`.
            align: match style.text_align {
                TextAlign::Start => parent.align,
                declared => declared,
            },
            centred: parent.centred || style.box_align == BoxAlign::Center,
        }
    }

    /// The inline size a style declares, which is `width` on a horizontal
    /// page and `height` on a vertical one — both are physical properties.
    fn declared_inline(
        &self,
        style: &ComputedStyle,
        available: f32,
        inherited: Inherited,
    ) -> Option<f32> {
        let declared = if self.axis.is_vertical() {
            style.height
        } else {
            style.width
        };
        self.length(declared, available, inherited)
    }

    /// `inline` cut to a whole number of cells on the em grid, which a book
    /// off the grid takes whole.
    fn on_grid(&self, inline: f32) -> f32 {
        match self.grid {
            Some(cell) if cell > 0.0 => (inline / cell).floor() * cell,
            _ => inline,
        }
    }

    /// Where a box of `box_inline` starts in `available` on a page whose own
    /// lines cross it: the whole lines `available` holds stand in the middle
    /// of it, and the box takes the edge they start at.
    fn across_lead(&self, available: f32, box_inline: f32) -> f32 {
        let Some(pitch) = self.across_pitch.filter(|pitch| *pitch > 0.0) else {
            return ((available - box_inline) / 2.0).max(0.0);
        };
        let lines = (available / pitch).floor().max(1.0) * pitch;
        let lead = ((available - lines) / 2.0).max(0.0);
        match self.across_from_end {
            true => (lead + lines - box_inline).max(0.0),
            false => lead,
        }
    }

    /// The most a box may measure across the inline axis, where the style
    /// caps it.
    fn capped_inline(
        &self,
        style: &ComputedStyle,
        available: f32,
        inherited: Inherited,
    ) -> Option<f32> {
        let cap = if self.axis.is_vertical() {
            style.max_height
        } else {
            style.max_width
        };
        self.length(cap, available, inherited)
    }

    fn border(&self, style: &ComputedStyle, available: f32, inherited: Inherited) -> Border {
        let side = |width: Length, kind: BorderStyle, color: Option<Color>| {
            if matches!(kind, BorderStyle::None | BorderStyle::Unset) {
                return (0.0, None);
            }
            let width = self.length(width, available, inherited).unwrap_or(0.0);
            (width, Some(color.unwrap_or(inherited.color)))
        };
        let (top, top_color) = side(
            style.border_width_top,
            style.border_style_top,
            style.border_color_top,
        );
        let (right, right_color) = side(
            style.border_width_right,
            style.border_style_right,
            style.border_color_right,
        );
        let (bottom, bottom_color) = side(
            style.border_width_bottom,
            style.border_style_bottom,
            style.border_color_bottom,
        );
        let (left, left_color) = side(
            style.border_width_left,
            style.border_style_left,
            style.border_color_left,
        );

        Border {
            widths: Edges::new(top, right, bottom, left),
            top: top_color,
            right: right_color,
            bottom: bottom_color,
            left: left_color,
        }
    }

    fn logical(&self, sides: [Length; 4], available: f32, inherited: Inherited) -> LogicalEdges {
        let [top, right, bottom, left] =
            sides.map(|side| self.length(side, available, inherited).unwrap_or(0.0));
        self.axis
            .logical_edges(Edges::new(top, right, bottom, left))
    }

    /// Resolve a length to device dots, or `None` where the source declared
    /// `auto`.
    fn length(&self, value: Length, available: f32, inherited: Inherited) -> Option<f32> {
        self.resolver.length(value, available, inherited.font_size)
    }
}

impl Border {
    /// The border widths read along the writing axis.
    fn widths_logical(&self, axis: Axis) -> LogicalEdges {
        axis.logical_edges(self.widths)
    }
}

/// Append the text of every [`Role::Text`] node under `node`.
fn gather_text(chapter: &Chapter, node: NodeId, out: &mut String) {
    if let Some(entry) = chapter.node(node)
        && entry.role == Role::Text
    {
        out.push_str(chapter.text(entry.text));
    }
    for child in chapter.children(node) {
        gather_text(chapter, child, out);
    }
}

/// Whether a node lays out at all. Table column geometry describes the
/// columns' widths and occupies no box.
fn produces_a_box(role: Role) -> bool {
    !matches!(role, Role::ColumnGroup | Role::Column)
}

/// Whether a node establishes a block box.
fn is_block_level(role: Role, display: Display) -> bool {
    match display {
        Display::None | Display::Inline | Display::InlineBlock => false,
        Display::ListItem | Display::TableCell | Display::TableRow => true,
        // `block` is also what a style carries when the source declared no
        // display at all. The role decides.
        Display::Block => !matches!(
            role,
            Role::Text
                | Role::Inline
                | Role::Link
                | Role::Image
                | Role::Break
                | Role::Ruby
                | Role::RubyText
        ),
    }
}

/// The marker a list item takes, `None` where the list draws none.
fn marker_text(kind: ListStyleType, ordinal: impl Fn() -> u32) -> Option<String> {
    Some(match kind {
        ListStyleType::None => return None,
        ListStyleType::Disc => "• ".to_string(),
        ListStyleType::Circle => "◦ ".to_string(),
        ListStyleType::Square => "▪ ".to_string(),
        ListStyleType::Decimal => format!("{}. ", ordinal()),
        ListStyleType::LowerAlpha => format!("{}. ", alphabetic(ordinal(), 'a')),
        ListStyleType::UpperAlpha => format!("{}. ", alphabetic(ordinal(), 'A')),
        ListStyleType::LowerRoman => format!("{}. ", roman(ordinal()).to_lowercase()),
        ListStyleType::UpperRoman => format!("{}. ", roman(ordinal())),
    })
}

/// Where a list item sits in its list, counting from the list's own start.
fn ordinal(chapter: &Chapter, node: NodeId) -> u32 {
    if let Some(stated) = chapter.semantics.list_start(node) {
        return stated;
    }
    let Some(parent) = chapter.node(node).and_then(|n| n.parent) else {
        return 1;
    };
    let start = chapter.semantics.list_start(parent).unwrap_or(1);
    let mut seen = 0;
    for sibling in chapter.children(parent) {
        if sibling == node {
            break;
        }
        if role_of(chapter, sibling) == Role::ListItem {
            seen += 1;
        }
    }
    start + seen
}

/// The marker style in force for an item — its own if it states one, else
/// the list's.
fn list_style_of(chapter: &Chapter, node: NodeId) -> ListStyleType {
    let own = style_of(chapter, node).list_style_type;
    if own != ListStyleType::Disc {
        return own;
    }
    match chapter.node(node).and_then(|n| n.parent) {
        Some(parent) if role_of(chapter, parent) == Role::OrderedList => {
            let stated = style_of(chapter, parent).list_style_type;
            if stated == ListStyleType::Disc {
                ListStyleType::Decimal
            } else {
                stated
            }
        }
        Some(parent) => style_of(chapter, parent).list_style_type,
        None => own,
    }
}

/// `a`, `b`, … `z`, `aa`, `ab`, … from a 1-based ordinal.
fn alphabetic(mut n: u32, first: char) -> String {
    if n == 0 {
        return first.to_string();
    }
    let mut out = String::new();
    while n > 0 {
        n -= 1;
        out.insert(0, char::from(first as u8 + (n % 26) as u8));
        n /= 26;
    }
    out
}

/// Roman numerals from a 1-based ordinal.
fn roman(n: u32) -> String {
    const VALUES: [(u32, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut n = n.max(1);
    let mut out = String::new();
    for (value, numeral) in VALUES {
        while n >= value {
            out.push_str(numeral);
            n -= value;
        }
    }
    out
}

/// Turn a tree of logical rectangles into physical ones.
fn to_physical(fragment: &mut Fragment, axis: Axis, page_block: f32) {
    let logical = fragment.rect;
    fragment.rect = axis.rect(
        logical.x,
        logical.y,
        logical.width,
        logical.height,
        page_block,
    );
    if let Content::Glyphs(run) = &mut fragment.content {
        run.baseline = physical_baseline(run, axis, logical.height);
    }
    for child in &mut fragment.children {
        to_physical(child, axis, page_block);
    }
}

/// A baseline measured from the fragment's block-start edge, restated as a
/// distance from its top or left.
fn physical_baseline(run: &GlyphRun, axis: Axis, block_size: f32) -> f32 {
    match axis {
        Axis::HorizontalTb | Axis::VerticalLr => run.baseline,
        Axis::VerticalRl => block_size - run.baseline,
    }
}

// --- Tables ---------------------------------------------------------------

/// One cell, and how much of the grid it claims.
struct Cell {
    node: NodeId,
    /// Which column it starts in, and how many it spans.
    column: usize,
    span: usize,
}

impl<'a> Flow<'a, '_> {
    /// Columns size to what they hold; a row is as deep as its deepest cell.
    fn table(
        &mut self,
        node: NodeId,
        inline: f32,
        block: f32,
        available: f32,
        parent: Inherited,
    ) -> Laid {
        let style = style_of(self.chapter, node);
        let inherited = self.inherit(parent, style);
        let margin = self.logical(
            [
                style.margin_top,
                style.margin_right,
                style.margin_bottom,
                style.margin_left,
            ],
            available,
            inherited,
        );
        let measure = (available - margin.inline()).max(0.0);

        let rows = self.rows(node);
        if rows.is_empty() {
            return self.block(node, inline, block, available, parent);
        }
        let widths = self.columns(&rows, measure, inherited);

        let origin_inline = inline + margin.inline_start;
        let origin_block = block + margin.block_start;
        let mut children = Vec::new();
        let mut cursor = origin_block;
        for row in &rows {
            let mut depth = 0.0f32;
            let start = children.len();
            for cell in row {
                let offset: f32 = widths[..cell.column].iter().sum();
                let width: f32 = widths[cell.column..(cell.column + cell.span).min(widths.len())]
                    .iter()
                    .sum();
                let laid = self.block(cell.node, origin_inline + offset, cursor, width, inherited);
                depth = depth.max(laid.outer);
                children.push(laid.fragment);
            }
            // Every cell reaches the row's own depth.
            for fragment in &mut children[start..] {
                fragment.rect = grow_block(fragment.rect, self.axis, depth);
            }
            cursor += depth;
        }

        let width: f32 = widths.iter().sum();
        let rect = Rect::new(origin_inline, origin_block, width, cursor - origin_block);
        let border = self.border(style, available, inherited);
        let mut fragment = Fragment::new(node, Role::Table, rect);
        fragment.background = style.background_color;
        fragment.border = border.is_visible().then_some(border);
        fragment.children = children;

        Laid {
            outer: margin.block() + rect.height,
            fragment,
        }
    }

    /// The table's cells, row by row, in the columns their spans claim.
    fn rows(&self, table: NodeId) -> Vec<Vec<Cell>> {
        let chapter = self.chapter;
        let mut rows = Vec::new();
        let mut walk = vec![table];
        while let Some(node) = walk.pop() {
            for child in chapter.children(node) {
                match role_of(chapter, child) {
                    Role::TableRow => rows.push(self.cells(child)),
                    // A head and a body hold rows and lay out no box.
                    Role::TableHead | Role::TableBody => walk.push(child),
                    _ => {}
                }
            }
        }
        rows.retain(|row: &Vec<Cell>| !row.is_empty());
        rows
    }

    /// One row's cells, in the columns they claim.
    fn cells(&self, row: NodeId) -> Vec<Cell> {
        let chapter = self.chapter;
        let mut cells = Vec::new();
        let mut column = 0;
        for child in chapter.children(row) {
            if role_of(chapter, child) != Role::TableCell {
                continue;
            }
            let span = chapter.semantics.col_span(child).unwrap_or(1).max(1) as usize;
            cells.push(Cell {
                node: child,
                column,
                span,
            });
            column += span;
        }
        cells
    }

    /// A width per column, shrunk in proportion past `measure`.
    fn columns(&mut self, rows: &[Vec<Cell>], measure: f32, inherited: Inherited) -> Vec<f32> {
        let count = rows
            .iter()
            .filter_map(|row| row.last().map(|cell| cell.column + cell.span))
            .max()
            .unwrap_or(0);
        let mut least = vec![0.0f32; count];
        let mut most = vec![0.0f32; count];

        for row in rows {
            for cell in row {
                let (min, max) = self.cell_width(cell.node, measure, inherited);
                // A cell over several columns sizes none of them.
                if cell.span != 1 {
                    continue;
                }
                least[cell.column] = least[cell.column].max(min);
                most[cell.column] = most[cell.column].max(max);
            }
        }

        let wanted: f32 = most.iter().sum();
        if wanted <= measure || wanted <= 0.0 {
            return most;
        }
        let floor: f32 = least.iter().sum();
        if floor >= measure {
            return least;
        }
        // Share `slack` over each column's own `appetite`.
        let slack = measure - floor;
        let appetite = wanted - floor;
        least
            .iter()
            .zip(&most)
            .map(|(min, max)| min + (max - min) / appetite * slack)
            .collect()
    }

    /// How wide `cell`'s content wants to be, unbroken and whole.
    fn cell_width(&mut self, cell: NodeId, available: f32, inherited: Inherited) -> (f32, f32) {
        let style = style_of(self.chapter, cell);
        let inherited = self.inherit(inherited, style);
        let padding = self.logical(
            [
                style.padding_top,
                style.padding_right,
                style.padding_bottom,
                style.padding_left,
            ],
            available,
            inherited,
        );
        let mut items: Vec<Item<'a>> = Vec::new();
        for child in self.chapter.children(cell) {
            self.collect(child, available, inherited, &mut items);
        }
        let (min, max) = Inline {
            fonts: self.fonts,
            axis: self.axis,
            hyphenator: self.hyphenator,
        }
        .measure(&items);
        let frame = padding.inline();
        (min + frame, max + frame)
    }
}

/// `rect` stretched to `depth` along the block axis.
fn grow_block(rect: Rect, axis: Axis, depth: f32) -> Rect {
    if axis.is_vertical() {
        Rect::new(rect.x, rect.y, depth, rect.height)
    } else {
        Rect::new(rect.x, rect.y, rect.width, depth)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use bokai::model::Node as IrNode;

    use crate::fragment::Node as Kind;
    use crate::resource::Unknown;

    /// A chapter of one paragraph holding `text`.
    fn one_paragraph(text: &str) -> Chapter {
        let mut chapter = Chapter::new();
        let paragraph = chapter.alloc_node(IrNode::new(Role::Paragraph));
        chapter.append_child(chapter.root(), paragraph);
        let range = chapter.append_text(text);
        let mut leaf = IrNode::new(Role::Text);
        leaf.text = range;
        let leaf = chapter.alloc_node(leaf);
        chapter.append_child(paragraph, leaf);
        chapter
    }

    fn lay_out(chapter: &Chapter, viewport: Viewport) -> Page {
        lay_out_along(chapter, viewport, Axis::HorizontalTb)
    }

    /// Lay `chapter` out in a book written along `axis`.
    fn lay_out_along(chapter: &Chapter, viewport: Viewport, axis: Axis) -> Page {
        let mut fonts = Fonts::empty();
        Layout {
            viewport,
            fonts: &mut fonts,
            resources: &Unknown,
            axis,
        }
        .chapter(chapter)
    }

    /// `one_paragraph`, its paragraph styled to read along `mode`. The
    /// alignment stands for everything else a real style names: initial
    /// values alone are the pool's default style, which an unstyled box reads.
    fn one_paragraph_along(text: &str, mode: WritingMode) -> Chapter {
        let mut chapter = one_paragraph(text);
        let style = chapter.styles.intern(ComputedStyle {
            writing_mode: mode,
            text_align: TextAlign::Center,
            ..ComputedStyle::default()
        });
        let paragraph = chapter.children(chapter.root()).next().expect("added");
        chapter.node_mut(paragraph).expect("added").style = style;
        chapter
    }

    #[test]
    fn a_chapter_of_horizontal_styles_reads_across_a_vertical_book() {
        // A section that reads across the book says so in its own styles.
        let chapter = one_paragraph_along("title", WritingMode::HorizontalTb);

        let page = lay_out_along(&chapter, Viewport::new(400.0, 600.0), Axis::VerticalRl);

        assert_eq!(page.axis, Axis::HorizontalTb);
    }

    #[test]
    fn a_chapter_carrying_no_style_reads_the_way_the_book_does() {
        let chapter = one_paragraph("body text");

        let page = lay_out_along(&chapter, Viewport::new(400.0, 600.0), Axis::VerticalRl);

        assert_eq!(page.axis, Axis::VerticalRl);
    }

    #[test]
    fn a_chapter_of_vertical_styles_reads_down_a_horizontal_book() {
        let chapter = one_paragraph_along("縦書き", WritingMode::VerticalRl);

        let page = lay_out(&chapter, Viewport::new(400.0, 600.0));

        assert_eq!(page.axis, Axis::VerticalRl);
    }

    #[test]
    fn a_paragraph_lays_out_as_a_column_of_lines_of_runs() {
        let chapter = one_paragraph("one two three four five six seven eight nine ten");
        let page = lay_out(&chapter, Viewport::new(200.0, 400.0));

        let columns: Vec<&Fragment> = page.root.of_kind(Kind::Column).collect();
        assert_eq!(columns.len(), 1, "one block, one column");
        let lines: Vec<&Fragment> = page.root.lines().collect();
        assert!(lines.len() > 1, "the text is longer than one line");
        // Every line hangs off the column, and nothing else does.
        assert_eq!(columns[0].children.len(), lines.len());
        for line in &columns[0].children {
            assert_eq!(line.kind, Kind::Line);
        }
    }

    #[test]
    fn a_line_spans_its_columns_whole_inline_size() {
        let chapter = one_paragraph("one two three four five six seven eight nine ten");
        let viewport = Viewport::new(200.0, 400.0);
        let content = viewport.content(Axis::HorizontalTb).0;
        let page = lay_out(&chapter, viewport);

        for line in page.root.lines() {
            assert_eq!(line.rect.width, content);
        }
    }

    #[test]
    fn lines_stack_down_the_page_in_order() {
        let chapter = one_paragraph("one two three four five six seven eight nine ten");
        let page = lay_out(&chapter, Viewport::new(200.0, 400.0));

        let tops: Vec<f32> = page.root.lines().map(|line| line.rect.y).collect();
        assert!(
            tops.windows(2).all(|pair| pair[1] > pair[0]),
            "lines out of order: {tops:?}"
        );
    }

    #[test]
    fn a_declared_block_size_moves_nothing() {
        // A block takes its block size from what it holds, `height` or no
        // `height`.
        let chapter = one_paragraph("one two three");
        let plain = lay_out(&chapter, Viewport::new(200.0, 400.0));

        let mut tall = Chapter::new();
        let paragraph = tall.alloc_node(IrNode::new(Role::Paragraph));
        tall.append_child(tall.root(), paragraph);
        let range = tall.append_text("one two three");
        let mut leaf = IrNode::new(Role::Text);
        leaf.text = range;
        let leaf = tall.alloc_node(leaf);
        tall.append_child(paragraph, leaf);
        let id = tall.styles.intern(ComputedStyle {
            height: Length::Px(500.0),
            ..ComputedStyle::default()
        });
        tall.node_mut(paragraph).expect("just added").style = id;

        let declared = lay_out(&tall, Viewport::new(200.0, 400.0));

        assert_eq!(declared.block_extent, plain.block_extent);
    }

    #[test]
    fn a_cjk_measure_is_a_whole_number_of_ems() {
        // The Scribe's vertical ladder: 2480 tall less 158 and 102 leaves
        // 2220, which holds 50 ems of 44 with 20 dots over.
        let viewport = Viewport {
            size: Size::new(1860.0, 2480.0),
            margins: Edges::new(158.0, 158.0, 102.0, 158.0),
            root_font_size: 44.0,
            language: Some("ja".to_string()),
            ..Viewport::default()
        };

        let (inline, _) = viewport.content(Axis::VerticalRl);

        assert_eq!(inline, 2200.0);
        assert_eq!(viewport.inline_lead(Axis::VerticalRl), 10.0);
    }

    #[test]
    fn an_odd_remainder_leaves_the_smaller_half_at_the_start() {
        // The Colorsoft's: 1696 less 82 and 17 leaves 1597, which holds 36
        // ems with 13 over — 6 before the text and 7 after.
        let viewport = Viewport {
            size: Size::new(1272.0, 1696.0),
            margins: Edges::new(82.0, 82.0, 17.0, 82.0),
            root_font_size: 44.0,
            language: Some("ja".to_string()),
            ..Viewport::default()
        };

        assert_eq!(viewport.content(Axis::VerticalRl).0, 1584.0);
        assert_eq!(viewport.inline_lead(Axis::VerticalRl), 6.0);
    }

    /// A box capped at 22 ems, padded 1.5 before the text and 4 after it.
    fn one_capped_paragraph(text: &str) -> Chapter {
        let mut chapter = one_paragraph(text);
        let style = chapter.styles.intern(ComputedStyle {
            max_width: Length::Em(22.0),
            padding_left: Length::Em(1.5),
            padding_right: Length::Em(4.0),
            ..ComputedStyle::default()
        });
        let paragraph = chapter.children(chapter.root()).next().expect("added");
        chapter.node_mut(paragraph).expect("added").style = style;
        chapter
    }

    /// A cap holds the whole box, padding in — 22 ems less the 5.5 the
    /// padding takes — and a book on the em grid holds the content it leaves
    /// to a whole number of cells: 16.5 ems become 16.
    #[test]
    fn a_capped_box_holds_its_padding_and_stands_on_the_em_grid() {
        let chapter = one_capped_paragraph("組版");
        let measured = |language: &str| {
            let viewport = Viewport {
                size: Size::new(1272.0, 1696.0),
                margins: Edges::new(82.0, 82.0, 17.0, 82.0),
                root_font_size: 44.0,
                language: Some(language.to_string()),
                ..Viewport::default()
            };
            let page = lay_out(&chapter, viewport);
            page.root
                .lines()
                .map(|line| line.rect.width)
                .next()
                .expect("a line")
        };

        assert_eq!(measured("ja"), 704.0);
        assert_eq!(measured("en"), 726.0);
    }

    /// A capped box on a page whose own lines cross it takes the edge those
    /// lines start at, its content centred in what the em grid leaves.
    #[test]
    fn a_capped_box_across_the_book_starts_where_its_lines_do() {
        let chapter = one_capped_paragraph("組版");
        let viewport = Viewport {
            size: Size::new(1272.0, 1696.0),
            margins: Edges::new(82.0, 82.0, 17.0, 82.0),
            root_font_size: 44.0,
            language: Some("ja".to_string()),
            line_spacing: 1.51,
            ..Viewport::default()
        };

        let page = lay_out_along(&chapter, viewport, Axis::VerticalRl);

        let cells: f32 = 25.0 * 44.0;
        let lines = (cells / 80.0).floor() * 80.0;
        let cap = 22.0 * 44.0;
        let padding = 1.5 * 44.0;
        let gutter = (cap - padding - 4.0 * 44.0 - 704.0) / 2.0;

        let line = page.root.lines().next().expect("a line");
        assert_eq!(line.rect.width, 704.0);
        assert_eq!(
            line.rect.x,
            (cells - lines) / 2.0 + lines - cap + padding + gutter
        );
    }

    #[test]
    fn a_book_in_a_proportional_script_takes_the_whole_measure() {
        let viewport = Viewport {
            size: Size::new(1272.0, 1696.0),
            margins: Edges::new(65.0, 82.0, 0.0, 82.0),
            root_font_size: 44.0,
            language: Some("en".to_string()),
            ..Viewport::default()
        };

        assert_eq!(viewport.content(Axis::HorizontalTb).0, 1108.0);
        assert_eq!(viewport.inline_lead(Axis::HorizontalTb), 0.0);
    }

    /// A table of one row, `cells` wide.
    fn one_row(cells: &[&str], spans: &[u32]) -> Chapter {
        let mut chapter = Chapter::new();
        let table = chapter.alloc_node(IrNode::new(Role::Table));
        chapter.append_child(chapter.root(), table);
        let row = chapter.alloc_node(IrNode::new(Role::TableRow));
        chapter.append_child(table, row);
        for (index, text) in cells.iter().enumerate() {
            let cell = chapter.alloc_node(IrNode::new(Role::TableCell));
            chapter.append_child(row, cell);
            if let Some(span) = spans.get(index) {
                chapter.semantics.set_col_span(cell, *span);
            }
            let range = chapter.append_text(text);
            let mut leaf = IrNode::new(Role::Text);
            leaf.text = range;
            let leaf = chapter.alloc_node(leaf);
            chapter.append_child(cell, leaf);
        }
        chapter
    }

    #[test]
    fn a_rows_cells_sit_side_by_side() {
        let chapter = one_row(&["one", "two", "three"], &[]);
        let page = lay_out(&chapter, Viewport::new(400.0, 400.0));

        let cells: Vec<&Fragment> = page
            .root
            .walk()
            .filter(|f| f.role == Role::TableCell)
            .collect();
        assert_eq!(cells.len(), 3);
        let lefts: Vec<f32> = cells.iter().map(|cell| cell.rect.x).collect();
        assert!(
            lefts.windows(2).all(|pair| pair[1] > pair[0]),
            "cells stacked instead of ranging along the row: {lefts:?}"
        );
        // One row: every cell starts at the same block position.
        assert!(cells.iter().all(|cell| cell.rect.y == cells[0].rect.y));
    }

    #[test]
    fn a_table_narrower_than_the_page_takes_only_what_it_holds() {
        let chapter = one_row(&["a", "b", "c"], &[]);
        let viewport = Viewport::new(400.0, 400.0);
        let measure = viewport.content(Axis::HorizontalTb).0;
        let page = lay_out(&chapter, viewport);

        let table = page
            .root
            .walk()
            .find(|f| f.role == Role::Table)
            .expect("a table box");
        assert!(
            table.rect.width < measure,
            "a table of three letters filled {measure} dots"
        );
    }

    #[test]
    fn a_spanning_cell_covers_the_columns_it_claims() {
        let chapter = one_row(&["wide", "one", "two"], &[2, 1, 1]);
        let page = lay_out(&chapter, Viewport::new(400.0, 400.0));

        let cells: Vec<&Fragment> = page
            .root
            .walk()
            .filter(|f| f.role == Role::TableCell)
            .collect();
        assert_eq!(cells.len(), 3);
        // The spanning cell reaches the third column's own start.
        assert!(
            cells[0].rect.right() >= cells[1].rect.x,
            "the spanning cell stopped short: {:?}",
            cells.iter().map(|c| c.rect).collect::<Vec<_>>()
        );
    }
}
