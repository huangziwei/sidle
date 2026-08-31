//! The furniture around a page: the bars, the `Aa` panel and `Go To`, all
//! drawn in panel dots. A draw pass collects a [`Hit`] per control, which
//! [`Chrome::acted`] reads a click out of.

pub mod aa;
pub mod bars;
pub mod goto;
pub mod text;

use bokai::style::Color;
use tiny_skia::{FillRule, Paint, PathBuilder, PixmapMut, Stroke, Transform};

use crate::font::Fonts;
use crate::geom::{Rect, Size};
use crate::paint::{Cache, Painter};
use crate::resource::Resources;
use crate::settings::Stop;

/// Which overlay is open over the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overlay {
    #[default]
    None,
    Aa,
    GoTo,
}

/// Which panel of `Aa` is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AaTab {
    #[default]
    Themes,
    Font,
    Layout,
    More,
}

impl AaTab {
    pub const ALL: [AaTab; 4] = [AaTab::Themes, AaTab::Font, AaTab::Layout, AaTab::More];

    pub fn label(self) -> &'static str {
        match self {
            AaTab::Themes => "Themes",
            AaTab::Font => "Font",
            AaTab::Layout => "Layout",
            AaTab::More => "More",
        }
    }
}

/// What a click on a control asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    TurnPage(isize),
    Open(Overlay),
    Close,
    Tab(AaTab),
    FontSize(usize),
    Bold(usize),
    Spacing(Stop),
    Margins(Stop),
    Vertical(bool),
    Justified(bool),
    Family(usize),
    PageColor(bool),
    GoToChapter(usize),
    GoToBeginning,
}

/// One control's area, and what clicking it asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub rect: Rect,
    pub action: Action,
}

/// Ink and page colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub ink: Color,
    pub page: Color,
    /// A rule, an empty slider stop, a label with nothing behind it.
    pub faint: Color,
}

impl Theme {
    pub fn light() -> Self {
        Self {
            ink: grey(0x11),
            page: grey(0xff),
            faint: grey(0x99),
        }
    }

    pub fn dark() -> Self {
        Self {
            ink: grey(0xee),
            page: grey(0x11),
            faint: grey(0x77),
        }
    }
}

pub fn grey(level: u8) -> Color {
    Color {
        r: level,
        g: level,
        b: level,
        a: 0xff,
    }
}

/// What the bars say about where the reader is.
pub struct Position {
    pub title: String,
    pub chapter_title: String,
    pub location: i64,
    pub locations: i64,
    pub percent: u32,
    pub minutes_left: u32,
}

/// Where each stop of a ladder sits, as `Aa` shows it.
pub struct Ladder {
    pub font_size: usize,
    pub font_sizes: usize,
    pub bold: usize,
    pub bolds: usize,
    pub spacing: Stop,
    pub margins: Stop,
    pub vertical: bool,
    pub justified: bool,
    pub family: usize,
    pub families: Vec<String>,
}

/// The chrome's own state.
#[derive(Default)]
pub struct Chrome {
    pub overlay: Overlay,
    pub tab: AaTab,
    pub dark: bool,
    hits: Vec<Hit>,
}

impl Chrome {
    pub fn theme(&self) -> Theme {
        if self.dark {
            Theme::dark()
        } else {
            Theme::light()
        }
    }

    /// Forget the controls the last draw pass placed.
    pub fn begin(&mut self) {
        self.hits.clear();
    }

    pub fn add(&mut self, rect: Rect, action: Action) {
        self.hits.push(Hit { rect, action });
    }

    /// What a click at `point`, in panel dots, asks for.
    pub fn acted(&self, point: (f32, f32)) -> Option<Action> {
        self.hits
            .iter()
            .rev()
            .find(|hit| {
                point.0 >= hit.rect.x
                    && point.0 < hit.rect.right()
                    && point.1 >= hit.rect.y
                    && point.1 < hit.rect.bottom()
            })
            .map(|hit| hit.action.clone())
    }

    /// How far down the page the text may start, and where it must stop.
    pub fn bands(&self, panel: Size) -> (f32, f32) {
        (
            bars::HEADER * panel.height / 1696.0,
            bars::FOOTER * panel.height / 1696.0,
        )
    }
}

/// Somewhere to draw a control, with the fonts to letter it.
pub struct Canvas<'a, 'p> {
    pub target: &'a mut PixmapMut<'p>,
    pub fonts: &'a mut Fonts,
    /// Glyph outlines, kept between draws.
    pub cache: &'a mut Cache,
    pub theme: Theme,
    /// The panel being drawn, in dots.
    pub panel: Size,
    /// Panel dots to buffer pixels.
    pub scale: f32,
    /// Where the panel's own origin sits in the buffer.
    pub offset: (f32, f32),
}

impl Canvas<'_, '_> {
    fn view(&self) -> Transform {
        Transform::from_translate(self.offset.0, self.offset.1).post_scale(self.scale, self.scale)
    }

    pub fn fill(&mut self, rect: Rect, color: Color) {
        crate::paint::fill(rect, color, self.view(), self.target);
    }

    pub fn stroke(&mut self, rect: Rect, color: Color, width: f32) {
        crate::paint::outline(rect, color, width, self.view(), self.target);
    }

    /// A rule `width` dots thick along `y`.
    pub fn rule(&mut self, x0: f32, x1: f32, y: f32, width: f32, color: Color) {
        self.fill(Rect::new(x0, y - width / 2.0, x1 - x0, width), color);
    }

    pub fn circle(&mut self, centre: (f32, f32), radius: f32, color: Color, filled: bool) {
        let mut builder = PathBuilder::new();
        builder.push_circle(centre.0, centre.1, radius);
        let Some(path) = builder.finish() else { return };
        let mut paint = Paint::default();
        paint.set_color_rgba8(color.r, color.g, color.b, color.a);
        paint.anti_alias = true;
        let view = self.view();
        if filled {
            self.target
                .fill_path(&path, &paint, FillRule::Winding, view, None);
        } else {
            let stroke = Stroke {
                width: radius * 0.24,
                ..Stroke::default()
            };
            self.target.stroke_path(&path, &paint, &stroke, view, None);
        }
    }

    /// A run of `content`, returning the area it covered.
    pub fn text(
        &mut self,
        content: &str,
        size: f32,
        color: Color,
        bold: bool,
        at: (f32, f32),
        align: text::Align,
    ) -> Rect {
        let line = text::place(self.fonts, content, size, color, bold, at, align);
        let width = line.width;
        let height = line.height;
        let tree = text::tree(line.fragments);
        let page = Rect::new(
            0.0,
            0.0,
            self.panel.width.max(1.0),
            self.panel.height.max(1.0),
        );
        Painter::cached(self.fonts, &NoResources, self.cache).paint(
            &tree,
            page,
            self.offset,
            self.scale,
            self.target,
        );
        let left = match align {
            text::Align::Left => at.0,
            text::Align::Center => at.0 - width / 2.0,
            text::Align::Right => at.0 - width,
        };
        Rect::new(left, at.1, width, height)
    }

    /// How wide `content` sets at `size`.
    pub fn width_of(&mut self, content: &str, size: f32, bold: bool) -> f32 {
        let ink = self.theme.ink;
        text::lay(self.fonts, content, size, ink, bold).width
    }
}

/// A book's chrome draws no pictures.
struct NoResources;

impl Resources for NoResources {
    fn image_size(&self, _src: &str) -> Option<Size> {
        None
    }
}
