//! Opens a KFX book and pages through it.
//!
//! ```text
//! sidle-render [options] <book>
//!
//!   --device <name>    which screen to emulate: colorsoft or scribe
//!   --panel <file>     panel ladders, in the form `Panel::parse` reads
//!   --fonts <dir>      faces to search ahead of the host's installed ones
//!   --serif <family>   what the reading settings choose for Latin
//!   --cjk <family>     the same for Chinese, Japanese and Korean
//!   --chapter <n>      open at this chapter
//!   --font-size <n>    open at this stop of the size ladder
//!   --pages <n>        print the geometry of the first `n` pages and exit
//!   --lines            list every line of every page reported
//! ```
//!
//! Click the page to turn it, `Aa` and the contents mark to open a panel.
//! Arrows, space and the wheel turn pages; `n` and `p` change chapter; `t`
//! opens the contents, `a` the settings, escape closes them, `q` quits.

use std::error::Error;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;

use bokai::model::{AnchorTarget, Book, ChapterId, LandmarkType, PositionMap};
use sidle_render::chrome::{
    AaPane, Action, Canvas, Chrome, Overlay, Position, Reading, aa, bars, goto,
};
use sidle_render::font::Script as FaceScript;
use sidle_render::paint::{Cache, Painter};
use sidle_render::settings::{Device, Direction, Panel, Script, Stop, reading_families};
use sidle_render::{Axis, BookResources, Fonts, Layout, Node, Pages, Settings, Size, Viewport};
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

const USAGE: &str = "usage: sidle-render [--device <name>] [--panel <file>] [--fonts <dir>] \
[--serif <family>] [--cjk <family>] [--chapter <n>] [--font-size <n>] [--pages <n>] [--lines] \
<book>";

/// Words a reader gets through in a minute, which sets the time left.
const WORDS_A_MINUTE: f32 = 220.0;

/// Locations one screen of text covers, before a page is laid out.
const LOCATIONS_A_PAGE: i64 = 14;

#[derive(Default)]
struct Options {
    book: Option<PathBuf>,
    device: Option<Device>,
    panel: Option<PathBuf>,
    fonts: Vec<PathBuf>,
    serif: Option<String>,
    cjk: Option<String>,
    chapter: usize,
    pages: Option<usize>,
    per_line: bool,
    font_size: Option<usize>,
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{argument} needs a value"))
        };
        match argument.as_str() {
            "--device" => options.device = Some(named_device(&value()?)?),
            "--panel" => options.panel = Some(PathBuf::from(value()?)),
            "--fonts" => options.fonts.push(PathBuf::from(value()?)),
            "--serif" => options.serif = Some(value()?),
            "--cjk" => options.cjk = Some(value()?),
            "--chapter" => options.chapter = value()?.parse()?,
            "--font-size" => options.font_size = Some(value()?.parse()?),
            "--pages" => options.pages = Some(value()?.parse()?),
            "--lines" => options.per_line = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ if options.book.is_none() => options.book = Some(PathBuf::from(argument)),
            _ => return Err(format!("unexpected argument: {argument}").into()),
        }
    }
    Ok(options)
}

/// The [`Device`] `name` picks, matched however it is cased.
fn named_device(name: &str) -> Result<Device, Box<dyn Error>> {
    Device::ALL
        .into_iter()
        .find(|device| device.name().eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let names: Vec<&str> = Device::ALL.iter().map(|d| d.name()).collect();
            format!("no such device: {name} (try {})", names.join(", ")).into()
        })
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse_options()?;
    let Some(path) = options.book.clone() else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };

    let mut book = Book::open(&path)?;
    let title = book.metadata().title.clone();
    let language = book.metadata().language.clone();
    let axis: Axis = book.writing_mode().into();
    let spine: Vec<ChapterId> = book.spine().iter().map(|entry| entry.id).collect();
    if spine.is_empty() {
        return Err(format!("{} has no chapters", path.display()).into());
    }
    let positions = book.position_map();
    let contents = contents_of(&mut book, &spine, &positions);
    let fixed = fixed_rows(&book, &spine);
    let resources = BookResources::declared(&mut book);

    let panel = options.panel.as_ref().map(Panel::read).transpose()?;
    let mut fonts = Fonts::new();
    for directory in &options.fonts {
        fonts.add_directory(directory);
    }
    if let Some(family) = &options.serif {
        fonts.reading_family(FaceScript::Latin, &[family]);
    }
    if let Some(family) = &options.cjk {
        fonts.reading_family(FaceScript::Cjk, &[family]);
    }

    let device = options.device.or(panel.is_none().then(Device::default));
    let in_force = panel
        .clone()
        .unwrap_or_else(|| device.unwrap_or_default().panel());
    let numbered = !book.page_list().is_empty();
    let families: Vec<String> = reading_families(&language)
        .iter()
        .map(|name| (*name).to_string())
        .collect();

    let mut settings = Settings::default_for(&in_force);
    settings.progress =
        sidle_render::settings::Progress::default_for(numbered, !contents.is_empty());
    if let Some(stop) = options.font_size {
        settings.font_size = stop;
    }
    let mut reader = Reader {
        chapter: options.chapter.min(spine.len() - 1),
        book,
        title,
        language,
        axis,
        spine,
        contents,
        positions,
        resources,
        fonts,
        panel,
        chapter_starts: Vec::new(),
        fixed,
        device,
        families,
        numbered,
        settings,
        chrome: Chrome::default(),
        per_line: options.per_line,
        cache: Cache::default(),
        sheet: None,
        page: 0,
        dpi: sidle_render::units::CSS_DPI,
        pointer: None,
        laid: None,
        view: None,
    };

    if let Some(pages) = options.pages {
        reader.report(pages);
        return Ok(());
    }

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut reader)?;
    Ok(())
}

/// The chapter each [`goto::Fixed`] row opens, where the book marks one.
fn fixed_rows(book: &Book, spine: &[ChapterId]) -> Vec<(goto::Fixed, usize)> {
    let chapter_of = |kind: LandmarkType| -> Option<usize> {
        let landmark = book
            .landmarks()
            .iter()
            .find(|landmark| landmark.landmark_type == kind)?;
        match book.resolve_toc_href(ChapterId(0), &landmark.href)? {
            AnchorTarget::Chapter(id) => spine.iter().position(|entry| *entry == id),
            AnchorTarget::Internal(global) => {
                spine.iter().position(|entry| *entry == global.chapter)
            }
            _ => None,
        }
    };
    let last = spine.len().saturating_sub(1);
    let beginning = chapter_of(LandmarkType::StartReading)
        .or_else(|| chapter_of(LandmarkType::BodyMatter))
        .unwrap_or(0);

    let mut rows = vec![(
        goto::Fixed::Cover,
        chapter_of(LandmarkType::Cover).unwrap_or(0),
    )];
    if let Some(front) = chapter_of(LandmarkType::FrontMatter) {
        rows.push((goto::Fixed::FrontMatter, front));
    }
    rows.push((goto::Fixed::Beginning, beginning));
    rows.push((goto::Fixed::End, last));
    rows
}

/// Every contents row, in the order the book lists them.
fn contents_of(
    book: &mut Book,
    spine: &[ChapterId],
    positions: &Option<PositionMap>,
) -> Vec<goto::Entry> {
    let mut entries = Vec::new();
    let starts: Vec<i64> = spine
        .iter()
        .enumerate()
        .map(|(n, _)| n as i64 * LOCATIONS_A_PAGE)
        .collect();
    gather(book.toc(), 0, spine, positions, &starts, &mut entries);
    if entries.is_empty() {
        entries = spine
            .iter()
            .enumerate()
            .map(|(n, _)| goto::Entry {
                title: format!("Chapter {}", n + 1),
                chapter: n,
                location: starts.get(n).copied().unwrap_or(0),
                depth: 0,
            })
            .collect();
    }
    entries
}

fn gather(
    toc: &[bokai::model::TocEntry],
    depth: usize,
    spine: &[ChapterId],
    positions: &Option<PositionMap>,
    starts: &[i64],
    out: &mut Vec<goto::Entry>,
) {
    for entry in toc {
        let chapter = match &entry.target {
            Some(AnchorTarget::Chapter(id)) => spine.iter().position(|s| s == id),
            Some(AnchorTarget::Internal(global)) => spine.iter().position(|s| *s == global.chapter),
            _ => None,
        };
        if let Some(chapter) = chapter {
            let location = positions
                .as_ref()
                .map(|_| starts.get(chapter).copied().unwrap_or(0))
                .unwrap_or_default();
            out.push(goto::Entry {
                title: entry.title.clone(),
                chapter,
                location,
                depth,
            });
        }
        gather(&entry.children, depth + 1, spine, positions, starts, out);
    }
}

struct Reader {
    book: Book,
    title: String,
    language: String,
    axis: Axis,
    spine: Vec<ChapterId>,
    contents: Vec<goto::Entry>,
    positions: Option<PositionMap>,
    resources: BookResources,
    fonts: Fonts,
    /// A panel read from a file, which stands in for a device's own.
    panel: Option<Panel>,
    /// The location each chapter opens at, filled by `chapter_locations`.
    chapter_starts: Vec<i64>,
    /// The rows above the book's own contents, and what each opens.
    fixed: Vec<(goto::Fixed, usize)>,
    /// Which [`Device`] is in force.
    device: Option<Device>,
    /// The reading fonts this book's language offers.
    families: Vec<String>,
    /// Whether the book carries page numbers.
    numbered: bool,
    settings: Settings,
    chrome: Chrome,
    per_line: bool,
    /// Glyph outlines, kept across draws.
    cache: Cache,
    /// The page as last drawn, and what it was drawn for.
    sheet: Option<(Pixmap, PageKey)>,
    chapter: usize,
    page: usize,
    /// Dots per inch the window's own pixels sit at.
    dpi: f32,
    /// Where the pointer last sat, in panel dots.
    pointer: Option<(f32, f32)>,
    laid: Option<Laid>,
    view: Option<View>,
}

/// The location the first element drawn in each page of `pages` falls in.
/// An empty vector where the book states no location scale.
fn page_locations(
    root: &sidle_render::Fragment,
    pages: &Pages,
    chapter: &bokai::model::Chapter,
    positions: Option<&PositionMap>,
) -> Vec<i64> {
    let Some(positions) = positions.filter(|map| map.has_locations()) else {
        return Vec::new();
    };
    let along = |rect: sidle_render::Rect| match pages.axis() {
        Axis::VerticalRl => -rect.right(),
        _ => rect.y,
    };
    let mut drawn: Vec<(f32, i64)> = root
        .walk()
        .filter(|fragment| !matches!(fragment.content, sidle_render::Content::Empty))
        .filter_map(|fragment| {
            let element = source_element(chapter, fragment.source)?;
            let position = positions.position(element, 0)?;
            Some((along(fragment.rect), positions.location_for(position)))
        })
        .collect();
    drawn.sort_by(|a, b| a.0.total_cmp(&b.0));

    (0..pages.count())
        .map(|n| {
            let start = along(pages.window(n));
            drawn
                .iter()
                .find(|(at, _)| *at >= start - 0.01)
                .or(drawn.last())
                .map_or(1, |(_, location)| *location)
        })
        .collect()
}

/// The source element `node` belongs to: its own, else its nearest ancestor's.
/// A text node carries none; the block that holds it does.
fn source_element(chapter: &bokai::model::Chapter, node: bokai::model::NodeId) -> Option<i64> {
    let mut at = Some(node);
    while let Some(id) = at {
        if let Some(element) = chapter.semantics.source_element(id) {
            return Some(element);
        }
        at = chapter.node(id).and_then(|node| node.parent);
    }
    None
}

/// The location `chapter` opens at.
fn first_location(chapter: &bokai::model::Chapter, positions: Option<&PositionMap>) -> Option<i64> {
    let positions = positions.filter(|map| map.has_locations())?;
    let element = chapter.source_elements().into_iter().next()?;
    Some(positions.location_for(positions.position(element, 0)?))
}

/// A copy of what the panels show, taken before the canvas borrows the fonts.
struct Shown {
    settings: Settings,
    device: Option<Device>,
    families: Vec<String>,
    vertical: bool,
    numbered: bool,
    chaptered: bool,
}

impl Shown {
    fn reading<'a>(&'a self, panel: &'a Panel) -> Reading<'a> {
        Reading {
            panel,
            settings: &self.settings,
            device: self.device,
            families: &self.families,
            vertical: self.vertical,
            numbered: self.numbered,
            chaptered: self.chaptered,
        }
    }
}

/// What a drawn page depends on: another value means another page.
#[derive(PartialEq)]
struct PageKey {
    chapter: usize,
    page: usize,
    dark: bool,
    panel: Size,
}

/// The chapter in hand, and the viewport it was laid out for.
struct Laid {
    pages: Pages,
    root: sidle_render::Fragment,
    viewport: Viewport,
    words: usize,
    /// The location each page opens at, empty without a location scale.
    locations: Vec<i64>,
}

struct View {
    window: Rc<Window>,
    // Held for as long as the surface it created is alive.
    _context: softbuffer::Context<Rc<Window>>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    /// Panel dots to buffer pixels, and where the panel's origin sits.
    fit: (f32, (f32, f32)),
}

impl Reader {
    /// The panel in force: the one named, else the screen being emulated.
    fn panel_for(&self) -> Panel {
        self.panel.clone().unwrap_or_else(|| self.device().panel())
    }

    /// The area a page is laid out into, less the bars above and below it.
    fn viewport(&self) -> Viewport {
        let panel = self.panel_for();
        let direction = if self.axis.is_vertical() {
            Direction::Vertical
        } else {
            Direction::Horizontal
        };
        let mut viewport = self.settings.viewport(&panel, &self.language, direction);
        let (header, footer) = self.chrome.bands(panel.size);
        viewport.margins.top = viewport.margins.top.max(header);
        viewport.margins.bottom = viewport.margins.bottom.max(footer);
        viewport
    }

    fn relayout(&mut self) {
        let viewport = self.viewport();
        if self
            .laid
            .as_ref()
            .is_some_and(|laid| laid.viewport == viewport)
        {
            return;
        }
        let chapter = match self.book.load_chapter(self.spine[self.chapter]) {
            Ok(chapter) => chapter,
            Err(error) => {
                eprintln!("chapter {}: {error}", self.chapter);
                self.laid = None;
                return;
            }
        };
        // `BookResources::declared` sized every picture from the manifest.
        let words = chapter.text_buffer().split_whitespace().count();
        let laid = Layout {
            viewport: viewport.clone(),
            fonts: &mut self.fonts,
            resources: &self.resources,
            axis: self.axis,
        }
        .chapter(&chapter);

        let pages = Pages::of(&laid, &viewport);
        self.page = self.page.min(pages.count().saturating_sub(1));
        self.sheet = None;
        let locations = page_locations(&laid.root, &pages, &chapter, self.positions.as_ref());
        self.laid = Some(Laid {
            pages,
            root: laid.root,
            viewport,
            words,
            locations,
        });
    }

    /// Give every contents row the location its chapter opens at.
    fn number_the_contents(&mut self) {
        let starts = self.chapter_locations().to_vec();
        for entry in &mut self.contents {
            if let Some(start) = starts.get(entry.chapter) {
                entry.location = *start;
            }
        }
    }

    /// The location each chapter opens at, read once and kept.
    fn chapter_locations(&mut self) -> &[i64] {
        if self.chapter_starts.is_empty() {
            let spine = self.spine.clone();
            let mut starts = Vec::with_capacity(spine.len());
            for id in spine {
                let start = self
                    .book
                    .load_chapter(id)
                    .ok()
                    .and_then(|chapter| first_location(&chapter, self.positions.as_ref()))
                    .unwrap_or(0);
                starts.push(start);
            }
            self.chapter_starts = starts;
        }
        &self.chapter_starts
    }

    /// Every picture drawn on page `n`.
    fn pictures_on(&self, n: usize) -> Vec<String> {
        let Some(laid) = &self.laid else {
            return Vec::new();
        };
        let window = laid.pages.window(n);
        laid.root
            .walk()
            .filter(|f| f.rect.intersects(&window))
            .filter_map(|f| match &f.content {
                sidle_render::Content::Image(src) => Some(src.clone()),
                _ => None,
            })
            .collect()
    }

    /// Decode what the pages either side of this one draw.
    fn prefetch(&mut self) {
        let Some(laid) = &self.laid else { return };
        let last = laid.pages.count().saturating_sub(1);
        let mut wanted = Vec::new();
        if self.page < last {
            wanted.extend(self.pictures_on(self.page + 1));
        }
        if self.page > 0 {
            wanted.extend(self.pictures_on(self.page - 1));
        }
        if !wanted.is_empty() {
            self.resources.load_named(&mut self.book, &wanted);
        }
    }

    fn go_to(&mut self, chapter: usize) {
        if chapter >= self.spine.len() {
            return;
        }
        self.chapter = chapter;
        self.page = 0;
        self.laid = None;
        self.request_redraw();
    }

    /// Whether the page after this one lies to its left, which is what a
    /// book stacking blocks right to left does.
    fn pages_leftward(&self) -> bool {
        self.axis == Axis::VerticalRl
    }

    /// Turn `by` pages, running on into the next chapter at either end.
    fn turn(&mut self, by: isize) {
        let Some(laid) = &self.laid else { return };
        let last = laid.pages.count().saturating_sub(1) as isize;
        let next = self.page as isize + by;
        if next < 0 {
            if self.chapter == 0 {
                return;
            }
            self.chapter -= 1;
            self.laid = None;
            self.page = usize::MAX;
        } else if next > last {
            if self.chapter + 1 >= self.spine.len() {
                return;
            }
            self.chapter += 1;
            self.page = 0;
            self.laid = None;
        } else {
            self.page = next as usize;
        }
        self.request_redraw();
    }

    /// What [`bars::draw`] states about this page.
    fn position(&self) -> Position {
        let pages = self
            .laid
            .as_ref()
            .map_or(1, |laid| laid.pages.count().max(1));
        let words = self.laid.as_ref().map_or(0, |laid| laid.words);
        let chapters = self.spine.len().max(1);
        let through =
            (self.chapter as f32 + (self.page as f32 + 1.0) / pages as f32) / chapters as f32;
        let locations = self
            .positions
            .as_ref()
            .map(|map| map.location_count())
            .filter(|count| *count > 0)
            .unwrap_or(chapters as i64 * LOCATIONS_A_PAGE);
        // The book's own number where it states one, else a share of the axis.
        let location = self
            .laid
            .as_ref()
            .and_then(|laid| laid.locations.get(self.page).copied())
            .unwrap_or_else(|| ((through * locations as f32) as i64).max(1));
        let left = pages.saturating_sub(self.page + 1) as f32 / pages as f32;
        let minutes = |words: f32| (words / WORDS_A_MINUTE).ceil() as u32;

        Position {
            title: self.title.clone(),
            chapter_title: self
                .contents
                .iter()
                .rfind(|entry| entry.chapter <= self.chapter)
                .map(|entry| entry.title.clone())
                .unwrap_or_default(),
            location,
            locations,
            page: (self.chapter * pages + self.page + 1) as i64,
            pages: (chapters * pages) as i64,
            percent: (through * 100.0).round() as u32,
            minutes_left_in_chapter: minutes(words as f32 * left),
            minutes_left: minutes(words as f32 * (1.0 - through) * chapters as f32),
        }
    }

    /// A copy of the fields [`Reading`] borrows, taken before [`Canvas`]
    /// borrows `self.fonts`.
    fn shown(&self) -> Shown {
        Shown {
            settings: self.settings.clone(),
            device: self.device,
            families: self.families.clone(),
            vertical: self.axis.is_vertical(),
            numbered: self.numbered,
            chaptered: self.contents.len() > 1,
        }
    }

    /// Act on a click or a key.
    fn act(&mut self, action: Action) {
        match action {
            Action::TurnPage(by) => self.turn(by),
            Action::Open(overlay) => {
                self.chrome.overlay = overlay;
                self.chrome.pane = AaPane::Tab;
                if overlay == Overlay::GoTo {
                    self.number_the_contents();
                }
                self.request_redraw();
            }
            Action::Close => {
                self.chrome.overlay = Overlay::None;
                self.request_redraw();
            }
            Action::Tab(tab) => {
                self.chrome.tab = tab;
                self.chrome.pane = AaPane::Tab;
                self.request_redraw();
            }
            Action::Pane(pane) => {
                self.chrome.pane = pane;
                self.request_redraw();
            }
            Action::Preset(preset) => {
                let panel = self.panel.clone();
                let panel = panel.unwrap_or_else(|| self.device().panel());
                self.restyle(|s| *s = s.preset(&panel, preset));
            }
            Action::Screen(device) => {
                self.device = Some(device);
                self.panel = None;
                self.settings = Settings::default_for(&device.panel());
                self.laid = None;
                self.sheet = None;
                self.request_redraw();
            }
            Action::FontSize(stop) => self.restyle(|s| s.font_size = stop),
            Action::Bold(stop) => self.restyle(|s| s.boldness = stop),
            Action::Spacing(stop) => self.restyle(|s| {
                s.line_spacing = stop;
                s.fine_spacing = false;
            }),
            Action::FineLineSpacing(stop) => self.restyle(|s| {
                s.fine_line_spacing = stop;
                s.fine_spacing = true;
            }),
            Action::ParagraphSpacing(stop) => self.restyle(|s| s.paragraph_spacing = stop),
            Action::WordSpacing(stop) => self.restyle(|s| s.word_spacing = stop),
            Action::CharacterSpacing(stop) => self.restyle(|s| s.character_spacing = stop),
            Action::Margins(stop) => self.restyle(|s| s.margins = stop),
            Action::Justified(on) => self.restyle(|s| s.justified = on),
            Action::Hyphenate(on) => self.restyle(|s| s.hyphenate = on),
            Action::Columns(columns) => self.restyle(|s| s.columns = columns),
            Action::Vertical(on) => {
                self.axis = if on {
                    Axis::VerticalRl
                } else {
                    Axis::HorizontalTb
                };
                self.laid = None;
                self.request_redraw();
            }
            Action::PageColor(dark) => {
                self.chrome.dark = dark;
                self.request_redraw();
            }
            Action::Progress(mode) => {
                self.settings.progress = mode;
                self.request_redraw();
            }
            Action::Family(family) => {
                self.settings.family = family;
                self.apply_family();
                self.laid = None;
                self.request_redraw();
            }
            Action::GoToChapter(chapter) => {
                self.chrome.overlay = Overlay::None;
                self.go_to(chapter);
            }
            Action::GoToBeginning => {
                self.chrome.overlay = Overlay::None;
                self.go_to(0);
            }
            Action::GoToEnd => {
                self.chrome.overlay = Overlay::None;
                self.go_to(self.spine.len().saturating_sub(1));
            }
        }
    }

    /// The screen being emulated, which a named profile stands in for.
    fn device(&self) -> Device {
        self.device.unwrap_or_default()
    }

    /// Point the reading settings at the family the font list chose.
    fn apply_family(&mut self) {
        let Some(family) = self.families.get(self.settings.family) else {
            return;
        };
        let script = match Script::of(&self.language) {
            Script::Cjk => FaceScript::Cjk,
            _ => FaceScript::Latin,
        };
        self.fonts.reading_family(script, &[family.as_str()]);
    }

    fn restyle(&mut self, change: impl FnOnce(&mut Settings)) {
        change(&mut self.settings);
        self.laid = None;
        self.request_redraw();
    }

    fn request_redraw(&self) {
        if let Some(view) = &self.view {
            view.window.request_redraw();
        }
    }

    fn draw(&mut self) {
        let Some(view) = &self.view else { return };
        let physical = view.window.inner_size();
        let (Some(width), Some(height)) = (
            NonZeroU32::new(physical.width),
            NonZeroU32::new(physical.height),
        ) else {
            return;
        };
        // The panel is the window's own pixels at the resolution they sit at.
        let scale = view.window.scale_factor() as f32;
        self.dpi = sidle_render::units::CSS_DPI * scale;
        self.relayout();

        let panel = self.panel_for();
        let theme = self.chrome.theme();
        let fit = (physical.width as f32 / panel.size.width)
            .min(physical.height as f32 / panel.size.height);
        let offset = (
            (physical.width as f32 - panel.size.width * fit) / 2.0,
            (physical.height as f32 - panel.size.height * fit) / 2.0,
        );

        let mut target =
            Pixmap::new(physical.width, physical.height).expect("the window has a nonzero size");
        target.fill(tiny_skia::Color::from_rgba8(0x20, 0x20, 0x20, 0xff));

        // The page is drawn once per turn and kept: opening a panel over it
        // costs only the chrome.
        let key = PageKey {
            chapter: self.chapter,
            page: self.page,
            dark: self.chrome.dark,
            panel: panel.size,
        };
        if self.sheet.as_ref().is_none_or(|(_, held)| *held != key) {
            let wanted: Vec<String> = self
                .laid
                .as_ref()
                .map(|laid| {
                    let window = laid.pages.window(self.page);
                    laid.root
                        .walk()
                        .filter(|f| f.rect.intersects(&window))
                        .filter_map(|f| match &f.content {
                            sidle_render::Content::Image(src) => Some(src.clone()),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.resources.load_named(&mut self.book, &wanted);
            let Some(mut sheet) = Pixmap::new(
                panel.size.width.max(1.0) as u32,
                panel.size.height.max(1.0) as u32,
            ) else {
                return;
            };
            sheet.fill(tiny_skia::Color::from_rgba8(
                theme.page.r,
                theme.page.g,
                theme.page.b,
                0xff,
            ));
            if let Some(laid) = &self.laid {
                Painter::cached(&self.fonts, &self.resources, &mut self.cache).paint(
                    &laid.root,
                    laid.pages.window(self.page),
                    laid.pages.origin(),
                    1.0,
                    &mut sheet.as_mut(),
                );
            }
            self.sheet = Some((sheet, key));
        }
        let Some((sheet, _)) = &self.sheet else {
            return;
        };
        target.draw_pixmap(
            0,
            0,
            sheet.as_ref(),
            &PixmapPaint {
                quality: FilterQuality::Bilinear,
                ..PixmapPaint::default()
            },
            Transform::from_translate(offset.0, offset.1).pre_scale(fit, fit),
            None,
        );

        self.chrome.begin();
        let at = self.position();
        let shown = self.shown();
        let mode = shown.settings.progress;
        let overlay = self.chrome.overlay;
        let leftward = self.pages_leftward();
        let contents_here = self.chapter;
        let fixed = self.fixed.clone();
        let mut canvas = Canvas {
            target: &mut target.as_mut(),
            fonts: &mut self.fonts,
            cache: &mut self.cache,
            theme,
            panel: panel.size,
            scale: fit,
            offset,
        };
        bars::draw(&mut self.chrome, &mut canvas, &at, mode, leftward);
        match overlay {
            Overlay::None => {}
            Overlay::Aa => aa::draw(&mut self.chrome, &mut canvas, &shown.reading(&panel)),
            Overlay::GoTo => goto::draw(
                &mut self.chrome,
                &mut canvas,
                &fixed,
                &self.contents,
                contents_here,
            ),
        }

        let view = self.view.as_mut().expect("checked at the top of draw");
        view.fit = (fit, offset);
        view.surface
            .resize(width, height)
            .expect("surface resize failed");
        let mut buffer = view.surface.buffer_mut().expect("no buffer to draw into");
        for (out, pixel) in buffer.iter_mut().zip(target.pixels()) {
            *out = (pixel.red() as u32) << 16 | (pixel.green() as u32) << 8 | pixel.blue() as u32;
        }
        buffer.present().expect("present failed");
        self.prefetch();
    }

    /// What the first `pages` pages of this chapter settled on.
    fn report(&mut self, pages: usize) {
        self.relayout();
        let Some(laid) = &self.laid else {
            eprintln!("nothing laid out");
            return;
        };
        let vertical = laid.pages.axis().is_vertical();
        let (left, top) = laid.pages.origin();
        let origin = if vertical { top } else { left };
        let block_origin = if vertical { left } else { top };
        println!(
            "{:<6} {:>5} {:>5} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6}",
            "page", "elem", "line", "top", "pitch", "left", "right", "adv", "loc"
        );
        for number in 0..pages.min(laid.pages.count()) {
            let window = laid.pages.window(number);
            let runs: Vec<&sidle_render::Fragment> = laid
                .root
                .of_kind(Node::Run)
                .filter(|run| run.rect.intersects(&window))
                .collect();
            let mut blocks: Vec<f32> = runs
                .iter()
                .map(|run| if vertical { run.rect.x } else { run.rect.y })
                .collect();
            blocks.sort_by(f32::total_cmp);
            blocks.dedup();

            let start = runs
                .iter()
                .map(|run| if vertical { run.rect.y } else { run.rect.x })
                .fold(f32::INFINITY, f32::min);
            let end = runs
                .iter()
                .map(|run| {
                    if vertical {
                        run.rect.bottom()
                    } else {
                        run.rect.right()
                    }
                })
                .fold(f32::NEG_INFINITY, f32::max);
            let pitch = blocks
                .windows(2)
                .map(|pair| pair[1] - pair[0])
                .fold(f32::INFINITY, f32::min);

            println!(
                "{:<6} {:>5} {:>5} {:>6.0} {:>6.0} {:>6.0} {:>6.0} {:>6.0} {:>6}",
                number + 1,
                runs.len(),
                blocks.len(),
                blocks.first().copied().unwrap_or(0.0) + block_origin,
                if pitch.is_finite() { pitch } else { 0.0 },
                if start.is_finite() {
                    start + origin
                } else {
                    0.0
                },
                if end.is_finite() { end + origin } else { 0.0 },
                median_advance(&runs),
                laid.locations.get(number).copied().unwrap_or(0),
            );
            if !self.per_line {
                continue;
            }
            for (index, block) in blocks.iter().enumerate() {
                let on_line: Vec<&&sidle_render::Fragment> = runs
                    .iter()
                    .filter(|run| {
                        let at = if vertical { run.rect.x } else { run.rect.y };
                        (at - block).abs() < 0.5
                    })
                    .collect();
                let from = on_line
                    .iter()
                    .map(|run| if vertical { run.rect.y } else { run.rect.x })
                    .fold(f32::INFINITY, f32::min);
                let to = on_line
                    .iter()
                    .map(|run| {
                        if vertical {
                            run.rect.bottom()
                        } else {
                            run.rect.right()
                        }
                    })
                    .fold(f32::NEG_INFINITY, f32::max);
                println!(
                    "  line {:<3}{:>28.0} {:>6.0}",
                    index + 1,
                    from + origin,
                    to + origin
                );
            }
        }
    }
}

/// The advance at the middle of `runs`' glyphs.
fn median_advance(runs: &[&sidle_render::Fragment]) -> f32 {
    let mut advances: Vec<f32> = Vec::new();
    for run in runs {
        let sidle_render::Content::Glyphs(glyphs) = &run.content else {
            continue;
        };
        let along: Vec<f32> = glyphs.glyphs.iter().map(|glyph| glyph.along).collect();
        for pair in along.windows(2) {
            advances.push(pair[1] - pair[0]);
        }
    }
    advances.sort_by(f32::total_cmp);
    advances.get(advances.len() / 2).copied().unwrap_or(0.0)
}

/// The `Stop` after `stop`, wrapping to the first.
fn next_stop(stop: Stop) -> Stop {
    match stop {
        Stop::Narrow => Stop::Normal,
        Stop::Normal => Stop::Wide,
        Stop::Wide => Stop::Narrow,
    }
}

impl ApplicationHandler for Reader {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.view.is_some() {
            return;
        }
        let attributes = Window::default_attributes().with_title(&self.title);
        let window = Rc::new(
            event_loop
                .create_window(attributes)
                .expect("no window available"),
        );
        let context = softbuffer::Context::new(window.clone()).expect("no drawing context");
        let surface =
            softbuffer::Surface::new(&context, window.clone()).expect("no drawing surface");
        self.view = Some(View {
            window,
            _context: context,
            surface,
            fit: (1.0, (0.0, 0.0)),
        });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if self.panel.is_none() {
                    self.laid = None;
                }
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::CursorMoved { position, .. } => {
                // `position` counts buffer pixels, which is what `fit` and
                // `offset` are stated in.
                if let Some(view) = &self.view {
                    let (fit, offset) = view.fit;
                    self.pointer = Some((
                        (position.x as f32 - offset.0) / fit.max(0.001),
                        (position.y as f32 - offset.1) / fit.max(0.001),
                    ));
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if let Some(point) = self.pointer
                    && let Some(action) = self.chrome.acted(point)
                {
                    self.act(action);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, lines) => -lines,
                    MouseScrollDelta::PixelDelta(position) => -position.y as f32 / 120.0,
                };
                if lines.abs() >= 1.0 {
                    self.turn(lines.signum() as isize);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::ArrowLeft) => {
                        self.turn(if self.pages_leftward() { 1 } else { -1 })
                    }
                    Key::Named(NamedKey::ArrowRight) => {
                        self.turn(if self.pages_leftward() { -1 } else { 1 })
                    }
                    Key::Named(NamedKey::ArrowDown)
                    | Key::Named(NamedKey::PageDown)
                    | Key::Named(NamedKey::Space) => self.turn(1),
                    Key::Named(NamedKey::ArrowUp) | Key::Named(NamedKey::PageUp) => self.turn(-1),
                    Key::Named(NamedKey::Escape) => self.act(Action::Close),
                    Key::Character("a") => self.act(Action::Open(Overlay::Aa)),
                    Key::Character("t") => self.act(Action::Open(Overlay::GoTo)),
                    Key::Character("n") => self.go_to(self.chapter + 1),
                    Key::Character("p") => self.go_to(self.chapter.saturating_sub(1)),
                    Key::Character("q") => event_loop.exit(),
                    Key::Character("+") | Key::Character("=") => {
                        let panel = self.panel_for();
                        let script = Script::of(&self.language);
                        let stops = panel.font_sizes.get(&script).map_or(1, Vec::len).max(1);
                        let next = (self.settings.font_size + 1).min(stops - 1);
                        self.act(Action::FontSize(next));
                    }
                    Key::Character("-") => {
                        let next = self.settings.font_size.saturating_sub(1);
                        self.act(Action::FontSize(next));
                    }
                    Key::Character("]") => {
                        self.act(Action::Spacing(next_stop(self.settings.line_spacing)))
                    }
                    Key::Character("[") => self.act(Action::Spacing(next_stop(next_stop(
                        self.settings.line_spacing,
                    )))),
                    Key::Character("m") => {
                        self.act(Action::Margins(next_stop(self.settings.margins)))
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
