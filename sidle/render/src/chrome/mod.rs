//! The furniture around a page: the bars, the `Aa` panel and `Go To`, all
//! drawn in panel dots. A draw pass collects a [`Hit`] per control, which
//! [`Chrome::acted`] reads a click out of.

pub mod aa;
pub mod bars;
pub mod goto;
pub mod scrub;
pub mod text;

use bokai::style::Color;
use tiny_skia::{FillRule, Mask, Paint, PathBuilder, PixmapMut, Stroke, Transform};

use crate::font::Fonts;
use crate::geom::{Rect, Size};
use crate::paint::{Cache, Painter};
use crate::resource::Resources;
use crate::settings::{Device, Orientation, Panel, Preset, Progress, Settings, Stop};

/// Which overlay is open over the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overlay {
    #[default]
    None,
    Aa,
    GoTo,
    /// The pages of the chapter, one at a time or nine at a time.
    Scrubber,
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

/// A screen `Aa` opens over a tab, which a back row leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AaPane {
    /// The tab itself.
    #[default]
    Tab,
    ReadingProgress,
}

impl AaPane {
    /// What this screen calls itself.
    pub fn title(self) -> &'static str {
        match self {
            AaPane::Tab => "",
            AaPane::ReadingProgress => "Reading Progress",
        }
    }
}

/// What a click on a control asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    TurnPage(isize),
    /// Whether the bars are showing.
    Reveal(bool),
    /// Which page of the chapter the scrubber stands at.
    Scrub(usize),
    /// The page a preview stands for, which the reader opens.
    GoToPage(usize),
    /// Whether the scrubber shows nine pages at once.
    Grid(bool),
    /// The chapter before this one, or the one after it.
    Jump(isize),
    Open(Overlay),
    Close,
    Tab(AaTab),
    Pane(AaPane),
    Preset(Preset),
    Screen(Device),
    FontSize(usize),
    Bold(usize),
    Spacing(Stop),
    Margins(Stop),
    Orient(Orientation),
    Justified(bool),
    Hyphenate(bool),
    Columns(u8),
    Family(usize),
    PageColor(bool),
    Progress(Progress),
    GoToChapter(usize),
    GoToBeginning,
    GoToEnd,
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

/// What [`bars::draw`] states about where a page sits in its book.
pub struct Position {
    pub title: String,
    pub chapter_title: String,
    pub location: i64,
    pub locations: i64,
    pub page: i64,
    pub pages: i64,
    pub percent: u32,
    pub minutes_left_in_chapter: u32,
    pub minutes_left: u32,
}

impl Position {
    /// What the bar below the page reads in `mode`.
    pub fn progress(&self, mode: Progress) -> String {
        match mode {
            Progress::PageNumber => format!("Page {} of {}", self.page, self.pages),
            Progress::Location => format!("Loc {} of {}", self.location, self.locations),
            Progress::TimeLeftInChapter => {
                format!("{} left in chapter", duration(self.minutes_left_in_chapter))
            }
            Progress::TimeLeft => format!("{} left in book", duration(self.minutes_left)),
            Progress::None => String::new(),
        }
    }

    /// What the footer reads with the bars showing: the location, the measure
    /// `mode` names beside it, and how far in.
    pub fn stated(&self, mode: Progress) -> String {
        let mut parts = vec![self.progress(Progress::Location)];
        match mode {
            Progress::Location | Progress::None => {}
            mode => parts.push(self.progress(mode)),
        }
        parts.push(format!("{}%", self.percent));
        parts.join(" | ")
    }
}

/// `minutes` in the short form the bar has room for.
fn duration(minutes: u32) -> String {
    match (minutes / 60, minutes % 60) {
        (0, 0) => "less than 1 min".to_string(),
        (0, 1) => "1 min".to_string(),
        (0, minutes) => format!("{minutes} mins"),
        (1, 0) => "1 hr".to_string(),
        (hours, 0) => format!("{hours} hrs"),
        (1, minutes) => format!("1 hr {minutes} mins"),
        (hours, minutes) => format!("{hours} hrs {minutes} mins"),
    }
}

/// Everything the panels show about the book in hand.
pub struct Reading<'a> {
    pub panel: &'a Panel,
    pub settings: &'a Settings,
    /// Which [`Device`] is in force, absent for a panel read from a file.
    pub device: Option<Device>,
    /// The reading fonts this book's script offers.
    pub families: &'a [String],
    /// Whether the book reads down the page.
    pub vertical: bool,
    /// Whether the book carries page numbers, and whether it has chapters.
    pub numbered: bool,
    pub chaptered: bool,
    /// Whether the book breaks words at the margin.
    pub hyphenates: bool,
}

/// The chrome's own state.
#[derive(Default)]
pub struct Chrome {
    pub overlay: Overlay,
    /// Whether the bars are showing over the page.
    pub revealed: bool,
    pub tab: AaTab,
    pub pane: AaPane,
    pub dark: bool,
    /// Whether the scrubber shows nine pages at once.
    pub grid: bool,
    /// Which page the scrubber stands at.
    pub at: usize,
    /// How far the open list is scrolled, in panel dots, held to what the
    /// list has to show.
    pub scroll: f32,
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

    /// The controls the last draw pass placed.
    pub fn hits(&self) -> &[Hit] {
        &self.hits
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
}

/// The density a control's artwork states its own geometry at.
pub const ARTWORK_DPI: f32 = 167.0;

/// The chevron, as its own artwork states it, and the box it sits in.
pub const CHEVRON: [(f32, f32); 6] = [
    (9.0, 2.0),
    (17.369, 12.0),
    (9.0, 22.0),
    (6.5, 22.0),
    (14.87, 12.0),
    (6.5, 2.0),
];
pub const CHEVRON_BOX: f32 = 24.0;

/// Somewhere to draw a control, with the fonts to letter it.
pub struct Canvas<'a, 'p> {
    pub target: &'a mut PixmapMut<'p>,
    pub fonts: &'a mut Fonts,
    /// Glyph outlines, kept between draws.
    pub cache: &'a mut Cache,
    pub theme: Theme,
    /// The panel being drawn, in dots.
    pub panel: Size,
    /// The panel's density, which every control's artwork scales from.
    pub dpi: f32,
    /// Panel dots to buffer pixels.
    pub scale: f32,
    /// Where the panel's own origin sits in the buffer.
    pub offset: (f32, f32),
    /// Where drawing is confined, set by [`Canvas::clip_to`].
    pub clip: Option<Mask>,
}

impl Canvas<'_, '_> {
    fn view(&self) -> Transform {
        Transform::from_translate(self.offset.0, self.offset.1).post_scale(self.scale, self.scale)
    }

    /// Draw nothing outside `rect`, in panel dots, until [`Canvas::unclip`].
    pub fn clip_to(&mut self, rect: Rect) {
        self.clip = crate::paint::mask(rect, self.view(), self.target);
    }

    /// Drop the clip [`Canvas::clip_to`] set.
    pub fn unclip(&mut self) {
        self.clip = None;
    }

    /// One dot of the reference panel every chrome measurement is stated
    /// against, in dots of the panel in hand.
    pub fn unit(&self) -> f32 {
        self.panel.height / bars::REFERENCE
    }

    /// One unit of the artwork a control is drawn from, in panel dots.
    pub fn art(&self) -> f32 {
        self.dpi / ARTWORK_DPI
    }

    pub fn fill(&mut self, rect: Rect, color: Color) {
        let view = self.view();
        crate::paint::fill(rect, color, view, self.clip.as_ref(), self.target);
    }

    pub fn stroke(&mut self, rect: Rect, color: Color, width: f32) {
        let view = self.view();
        crate::paint::outline(rect, color, width, view, self.clip.as_ref(), self.target);
    }

    /// A rule `width` dots thick along `y`.
    pub fn rule(&mut self, x0: f32, x1: f32, y: f32, width: f32, color: Color) {
        self.fill(Rect::new(x0, y - width / 2.0, x1 - x0, width), color);
    }

    /// A rectangle with `radius` corners, outlined `width` dots thick.
    pub fn round_stroke(&mut self, rect: Rect, radius: f32, color: Color, width: f32) {
        let Some(path) = rounded(rect, radius) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color_rgba8(color.r, color.g, color.b, color.a);
        paint.anti_alias = true;
        let stroke = Stroke {
            width,
            ..Stroke::default()
        };
        let view = self.view();
        self.target
            .stroke_path(&path, &paint, &stroke, view, self.clip.as_ref());
    }

    /// The same rectangle filled.
    pub fn round_fill(&mut self, rect: Rect, radius: f32, color: Color) {
        let Some(path) = rounded(rect, radius) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color_rgba8(color.r, color.g, color.b, color.a);
        paint.anti_alias = true;
        let view = self.view();
        self.target
            .fill_path(&path, &paint, FillRule::Winding, view, self.clip.as_ref());
    }

    /// A closed shape through `points`, in the artwork's own units, placed
    /// with its box's corner at `at`.
    pub fn polygon(&mut self, points: &[(f32, f32)], at: (f32, f32), art: f32, color: Color) {
        let mut path = PathBuilder::new();
        for (n, (x, y)) in points.iter().enumerate() {
            let (x, y) = (at.0 + x * art, at.1 + y * art);
            if n == 0 {
                path.move_to(x, y);
            } else {
                path.line_to(x, y);
            }
        }
        path.close();
        let Some(path) = path.finish() else { return };
        let mut paint = Paint::default();
        paint.set_color_rgba8(color.r, color.g, color.b, color.a);
        paint.anti_alias = true;
        let view = self.view();
        self.target
            .fill_path(&path, &paint, FillRule::Winding, view, self.clip.as_ref());
    }

    /// The chevron a row or a page arrow carries, in a box of [`CHEVRON_BOX`],
    /// pointing back where `back`.
    pub fn chevron(&mut self, at: (f32, f32), art: f32, back: bool, color: Color) {
        let points: Vec<(f32, f32)> = CHEVRON
            .iter()
            .map(|(x, y)| {
                if back {
                    (CHEVRON_BOX - x, *y)
                } else {
                    (*x, *y)
                }
            })
            .collect();
        self.polygon(&points, at, art, color);
    }

    /// A rendered page, scaled to fill `rect`.
    pub fn picture(&mut self, rect: Rect, sheet: &tiny_skia::Pixmap) {
        let (width, height) = (sheet.width() as f32, sheet.height() as f32);
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        let fit = (rect.width / width).min(rect.height / height);
        let placed = Transform::from_translate(
            rect.x + (rect.width - width * fit) / 2.0,
            rect.y + (rect.height - height * fit) / 2.0,
        )
        .pre_scale(fit, fit);
        let view = placed.post_concat(self.view());
        self.target.draw_pixmap(
            0,
            0,
            sheet.as_ref(),
            &tiny_skia::PixmapPaint {
                quality: tiny_skia::FilterQuality::Bilinear,
                ..tiny_skia::PixmapPaint::default()
            },
            view,
            self.clip.as_ref(),
        );
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
                .fill_path(&path, &paint, FillRule::Winding, view, self.clip.as_ref());
        } else {
            let stroke = Stroke {
                width: radius * 0.24,
                ..Stroke::default()
            };
            self.target
                .stroke_path(&path, &paint, &stroke, view, self.clip.as_ref());
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
        let painter = Painter::cached(self.fonts, &NoResources, self.cache);
        let mut painter = match &self.clip {
            Some(mask) => painter.within(mask),
            None => painter,
        };
        painter.paint(&tree, page, self.offset, self.scale, self.target);
        let left = match align {
            text::Align::Left => at.0,
            text::Align::Center => at.0 - width / 2.0,
            text::Align::Right => at.0 - width,
        };
        Rect::new(left, at.1, width, height)
    }

    /// How far a line of `size` carries its baseline below its own top.
    pub fn baseline_of(&mut self, size: f32, bold: bool) -> f32 {
        let ink = self.theme.ink;
        text::lay(self.fonts, "A", size, ink, bold).baseline
    }

    /// How wide `content` sets at `size`.
    pub fn width_of(&mut self, content: &str, size: f32, bold: bool) -> f32 {
        let ink = self.theme.ink;
        text::lay(self.fonts, content, size, ink, bold).width
    }
}

/// A rectangle whose corners are `radius` quarter-turns.
fn rounded(rect: Rect, radius: f32) -> Option<tiny_skia::Path> {
    let r = radius.min(rect.width / 2.0).min(rect.height / 2.0).max(0.0);
    let (x0, y0) = (rect.x, rect.y);
    let (x1, y1) = (rect.right(), rect.bottom());
    let mut path = PathBuilder::new();
    path.move_to(x0 + r, y0);
    path.line_to(x1 - r, y0);
    path.quad_to(x1, y0, x1, y0 + r);
    path.line_to(x1, y1 - r);
    path.quad_to(x1, y1, x1 - r, y1);
    path.line_to(x0 + r, y1);
    path.quad_to(x0, y1, x0, y1 - r);
    path.line_to(x0, y0 + r);
    path.quad_to(x0, y0, x0 + r, y0);
    path.close();
    path.finish()
}

/// A book's chrome draws no pictures.
struct NoResources;

impl Resources for NoResources {
    fn image_size(&self, _src: &str) -> Option<Size> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCATION: i64 = 412;
    const LOCATIONS: i64 = 5108;
    const PAGE: i64 = 31;
    const PAGES: i64 = 402;

    fn somewhere() -> Position {
        Position {
            title: "A Book".to_string(),
            chapter_title: "One".to_string(),
            location: LOCATION,
            locations: LOCATIONS,
            page: PAGE,
            pages: PAGES,
            percent: 8,
            minutes_left_in_chapter: 7,
            minutes_left: 194,
        }
    }

    #[test]
    fn each_progress_mode_reads_the_way_the_bar_reads_it() {
        let at = somewhere();

        assert_eq!(
            at.progress(Progress::Location),
            format!("Loc {LOCATION} of {LOCATIONS}")
        );
        assert_eq!(
            at.progress(Progress::PageNumber),
            format!("Page {PAGE} of {PAGES}")
        );
        assert_eq!(
            at.progress(Progress::TimeLeftInChapter),
            "7 mins left in chapter"
        );
        assert_eq!(
            at.progress(Progress::TimeLeft),
            "3 hrs 14 mins left in book"
        );
        assert_eq!(at.progress(Progress::None), "");
    }

    #[test]
    fn a_duration_takes_the_singular_and_the_short_hour() {
        assert_eq!(duration(0), "less than 1 min");
        assert_eq!(duration(1), "1 min");
        assert_eq!(duration(59), "59 mins");
        assert_eq!(duration(60), "1 hr");
        assert_eq!(duration(61), "1 hr 1 mins");
        assert_eq!(duration(120), "2 hrs");
    }

    #[test]
    fn the_topmost_control_takes_a_click() {
        let mut chrome = Chrome::default();
        chrome.add(Rect::new(0.0, 0.0, 100.0, 100.0), Action::Close);
        chrome.add(Rect::new(10.0, 10.0, 20.0, 20.0), Action::TurnPage(1));

        assert_eq!(chrome.acted((15.0, 15.0)), Some(Action::TurnPage(1)));
        assert_eq!(chrome.acted((50.0, 50.0)), Some(Action::Close));
        assert_eq!(chrome.acted((150.0, 50.0)), None);
    }
}
