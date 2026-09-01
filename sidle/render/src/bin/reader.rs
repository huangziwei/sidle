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
//!   --page <n>         open at this page of it
//!   --font-size <n>    open at this stop of the size ladder
//!   --pages <n>        print the geometry of the first `n` pages and exit
//!   --lines            list every line of every page reported
//!   --shot <file>      write one page to a PNG and exit
//!   --open <panel>     open aa, goto, scrub, search or none
//!   --tab <name>       which `Aa` tab the sheet opens on
//!   --scroll <n>       how many rows down the open list stands
//!   --query <text>     what the search card looks for
//!   --reveal           draw the bars over the page
//!   --grid             rule the page at the margin ladder
//!   --dark             draw the page and the chrome dark
//!   --hits             list the controls a shot placed
//! ```
//!
//! Click the page to turn it, `Aa` and the contents mark to open a panel.
//! Arrows, space and the wheel turn pages, and scroll an open list; `n` and
//! `p` change chapter; `t` opens the contents, `a` the settings and `s` the
//! search card, which takes what is typed; escape closes them, `q` quits.

use std::error::Error;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;

use bokai::model::{AnchorTarget, Book, ChapterId, LandmarkType, Match, PositionMap};
use sidle_render::chrome::{
    AaPane, AaTab, Action, Canvas, Chrome, Overlay, Position, Reading, aa, bars, goto, scrub,
    search,
};
use sidle_render::font::Script as FaceScript;
use sidle_render::paint::{Cache, Painter};
use sidle_render::settings::{Device, Direction, Panel, Script, Stop, reading_families};
use sidle_render::{Axis, BookResources, Fonts, Layout, Node, Pages, Settings, Size, Viewport};
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

const USAGE: &str = "usage: sidle-render [--device <name>] [--panel <file>] [--fonts <dir>] \
[--serif <family>] [--cjk <family>] [--chapter <n>] [--page <n>] [--font-size <n>] \
[--pages <n>] [--lines] [--shot <file>] [--open <panel>] [--tab <name>] [--scroll <n>] \
[--query <text>] [--reveal] [--grid] [--dark] [--hits] <book>";

/// Words a reader gets through in a minute, which sets the time left.
const WORDS_A_MINUTE: f32 = 220.0;

/// Locations one screen of text covers, before a page is laid out.
const LOCATIONS_A_PAGE: i64 = 14;

/// How much of the text either side of a match a result states.
const LEAD_IN: usize = 16;
const LEAD_OUT: usize = 64;

/// How tall the window opens, in logical pixels.
const WINDOW_HEIGHT: f32 = 900.0;

/// How many rows of a list a page key moves it by.
const ROWS_A_LEAP: f32 = 5.0;

/// The host faces standing in for the reading fonts a Kindle carries, best
/// first: Bookerly's old-style serif for Latin, 明朝 for Japanese — the face
/// Amazon's own iOS reader sets Japanese in. `--serif` and `--cjk` name others.
const SERIF: [&str; 3] = ["Iowan Old Style", "Charter", "Georgia"];
const MINCHO: [&str; 3] = ["Hiragino Mincho ProN", "Hiragino Mincho Pro", "YuMincho"];
const GOTHIC: [&str; 3] = ["Hiragino Sans", "Hiragino Kaku Gothic ProN", "YuGothic"];

/// The host faces the reading font `family` is set in, best first. A name the
/// host has a face for stands for itself.
fn faces_for(family: &str, script: FaceScript) -> Vec<String> {
    let stand_ins: &[&str] = match (family, script) {
        ("Publisher Font", FaceScript::Cjk) => &MINCHO,
        ("Publisher Font" | "Bookerly" | "Caecilia", _) => &SERIF,
        ("Mincho" | "Song", _) => &MINCHO,
        ("Gothic" | "Hei", _) => &GOTHIC,
        _ => return vec![family.to_string()],
    };
    stand_ins.iter().map(|name| (*name).to_string()).collect()
}

#[derive(Default)]
struct Options {
    book: Option<PathBuf>,
    device: Option<Device>,
    panel: Option<PathBuf>,
    fonts: Vec<PathBuf>,
    serif: Option<String>,
    cjk: Option<String>,
    chapter: usize,
    /// Which page of the chapter a shot shows.
    page: usize,
    pages: Option<usize>,
    per_line: bool,
    font_size: Option<usize>,
    shot: Option<PathBuf>,
    reveal: bool,
    grid: bool,
    open: Overlay,
    tab: AaTab,
    /// How many rows down the open list stands.
    scroll: f32,
    /// Whether a shot lists the controls it placed.
    hits: bool,
    /// Whether the page and the chrome are drawn dark.
    dark: bool,
    /// What a shot of the search card has been asked to look for.
    query: String,
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
            "--page" => options.page = value()?.parse()?,
            "--font-size" => options.font_size = Some(value()?.parse()?),
            "--pages" => options.pages = Some(value()?.parse()?),
            "--lines" => options.per_line = true,
            "--shot" => options.shot = Some(PathBuf::from(value()?)),
            "--reveal" => options.reveal = true,
            "--grid" => options.grid = true,
            "--dark" => options.dark = true,
            "--open" => options.open = named_overlay(&value()?)?,
            "--tab" => options.tab = named_tab(&value()?)?,
            "--scroll" => options.scroll = value()?.parse()?,
            "--hits" => options.hits = true,
            "--query" => options.query = value()?,
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

/// The [`Overlay`] `name` opens.
fn named_overlay(name: &str) -> Result<Overlay, Box<dyn Error>> {
    match name {
        "aa" => Ok(Overlay::Aa),
        "goto" => Ok(Overlay::GoTo),
        "scrub" => Ok(Overlay::Scrubber),
        "search" => Ok(Overlay::Search),
        "none" => Ok(Overlay::None),
        _ => Err(format!("no such panel: {name} (try aa, goto, scrub, search, none)").into()),
    }
}

/// The [`AaTab`] `name` picks.
fn named_tab(name: &str) -> Result<AaTab, Box<dyn Error>> {
    AaTab::ALL
        .into_iter()
        .find(|tab| tab.label().eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("no such tab: {name}").into())
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
    fonts.reading_family(FaceScript::Latin, &SERIF);
    fonts.reading_family(FaceScript::Cjk, &MINCHO);
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
    // A family is offered where `faces_for` names a face carrying `sample`.
    let script = match Script::of(&language) {
        Script::Cjk => FaceScript::Cjk,
        _ => FaceScript::Latin,
    };
    let sample = match script {
        FaceScript::Cjk => '日',
        FaceScript::Latin => 'A',
    };
    let families: Vec<String> = reading_families(&language)
        .iter()
        .map(|name| (*name).to_string())
        .filter(|name| fonts.carries(&faces_for(name, script), sample))
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
        query: String::new(),
        found: Vec::new(),
        opens: Vec::new(),
        searched: false,
        wanted: None,
    };

    if let Some(pages) = options.pages {
        reader.report(pages);
        return Ok(());
    }

    if let Some(path) = options.shot {
        reader.page = options.page;
        reader.chrome.at = options.page;
        reader.chrome.revealed = options.reveal;
        reader.chrome.grid = options.grid;
        reader.chrome.dark = options.dark;
        reader.chrome.overlay = options.open;
        reader.chrome.tab = options.tab;
        reader.chrome.scroll = options.scroll * reader.scroll_step();
        if !options.query.is_empty() {
            reader.query = options.query.clone();
            reader.look();
        }
        reader.shot(&path)?;
        if options.hits {
            for opens in &reader.opens {
                println!("chapter {} loc {}", opens.chapter, opens.location);
            }
            for hit in reader.chrome.hits() {
                let rect = hit.rect;
                println!(
                    "{:>7.1} {:>7.1} {:>7.1} {:>7.1}  {:?}",
                    rect.x, rect.y, rect.width, rect.height, hit.action
                );
            }
        }
        return Ok(());
    }

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut reader)?;
    Ok(())
}

/// The element a navigation href names outright, as `#941` or `#941:4`.
fn named_element(href: &str) -> Option<i64> {
    let name = href.trim().strip_prefix('#')?;
    name.split(':').next()?.parse().ok()
}

/// The chapter each [`goto::Fixed`] row opens, where the book marks one.
fn fixed_rows(book: &Book, spine: &[ChapterId]) -> Vec<(goto::Fixed, Option<usize>)> {
    let chapter_of = |kind: LandmarkType| -> Option<usize> {
        let landmark = book
            .landmarks()
            .iter()
            .find(|landmark| landmark.landmark_type == kind)?;
        let target = landmark
            .target
            .clone()
            .or_else(|| book.resolve_toc_href(ChapterId(0), &landmark.href))?;
        match target {
            AnchorTarget::Chapter(id) => spine.iter().position(|entry| *entry == id),
            AnchorTarget::Internal(global) => {
                spine.iter().position(|entry| *entry == global.chapter)
            }
            _ => None,
        }
    };
    let beginning = chapter_of(LandmarkType::StartReading)
        .or_else(|| chapter_of(LandmarkType::BodyMatter))
        .unwrap_or(0);

    vec![
        (goto::Fixed::Beginning, Some(beginning)),
        (goto::Fixed::PageOrLocation, None),
    ]
}

/// Every contents row, in the order the book lists them.
fn contents_of(
    book: &mut Book,
    spine: &[ChapterId],
    positions: &Option<PositionMap>,
) -> Vec<goto::Entry> {
    // A nav href names an element id, which places itself in a chapter only
    // once that chapter has been read. The cache holds what layout reads next.
    let chapters = book.load_chapters_cached(spine).unwrap_or_default();
    let toc = book.toc().to_vec();
    let mut rows = Vec::new();
    gather(&toc, 0, spine, book, &mut rows);
    if rows.is_empty() {
        return spine
            .iter()
            .enumerate()
            .map(|(n, _)| goto::Entry {
                title: format!("Chapter {}", n + 1),
                chapter: n,
                location: n as i64 * LOCATIONS_A_PAGE,
                depth: 0,
            })
            .collect();
    }

    // Each row's own location: the element its href names, else the element
    // its node was built from, else the chapter's first.
    rows.into_iter()
        .map(|row| {
            let location = named_element(&row.href)
                .or_else(|| {
                    chapters
                        .get(row.chapter)
                        .and_then(|chapter| match row.node {
                            Some(node) if node != bokai::model::NodeId::ROOT => {
                                source_element(chapter, node)
                            }
                            _ => chapter.source_elements().into_iter().next(),
                        })
                })
                .zip(positions.as_ref().filter(|map| map.has_locations()))
                .and_then(|(element, map)| Some(map.location_for(map.position(element, 0)?)))
                .unwrap_or(0);
            goto::Entry {
                title: row.title,
                chapter: row.chapter,
                location,
                depth: row.depth,
            }
        })
        .collect()
}

/// One contents row before its location is read.
struct Row {
    title: String,
    /// What the book's own navigation points at.
    href: String,
    chapter: usize,
    /// The node the row's own href names, absent where it names a chapter.
    node: Option<bokai::model::NodeId>,
    depth: usize,
}

fn gather(
    toc: &[bokai::model::TocEntry],
    depth: usize,
    spine: &[ChapterId],
    book: &Book,
    out: &mut Vec<Row>,
) {
    for entry in toc {
        // An entry carries a resolved target only where the book was opened
        // through a route that resolves them; its own href states the rest.
        let target = entry
            .target
            .clone()
            .or_else(|| book.resolve_toc_href(ChapterId(0), &entry.href));
        let placed = match target {
            Some(AnchorTarget::Chapter(id)) => {
                spine.iter().position(|s| *s == id).map(|at| (at, None))
            }
            Some(AnchorTarget::Internal(global)) => spine
                .iter()
                .position(|s| *s == global.chapter)
                .map(|at| (at, Some(global.node))),
            _ => None,
        };
        if let Some((chapter, node)) = placed {
            out.push(Row {
                title: entry.title.clone(),
                href: entry.href.clone(),
                chapter,
                node,
                depth,
            });
        }
        gather(&entry.children, depth + 1, spine, book, out);
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
    /// The rows above the book's own contents, and what each opens.
    fixed: Vec<(goto::Fixed, Option<usize>)>,
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
    /// The phrase the search card is looking for.
    query: String,
    /// Where it was found, as the card states each place.
    found: Vec<search::Found>,
    /// Where each of `found` opens, in the same order.
    opens: Vec<Opens>,
    /// Whether the book has been searched for the phrase in hand.
    searched: bool,
    /// The location the chapter being laid out is to open at.
    wanted: Option<i64>,
}

/// Where a search result opens: its chapter, and the location inside it.
struct Opens {
    chapter: usize,
    location: i64,
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

/// A copy of what the panels show, taken before the canvas borrows the fonts.
struct Shown {
    settings: Settings,
    device: Option<Device>,
    families: Vec<String>,
    vertical: bool,
    numbered: bool,
    chaptered: bool,
    hyphenates: bool,
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
            hyphenates: self.hyphenates,
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
    /// The panel in force: the one named, else the screen being emulated,
    /// held the way the settings hold it.
    fn panel_for(&self) -> Panel {
        self.panel
            .clone()
            .unwrap_or_else(|| self.device().panel())
            .held(self.settings.orientation)
    }

    /// The area a page is laid out into: the panel and the margin ladder
    /// the book's own direction takes. The bars are drawn over it.
    fn viewport(&self) -> Viewport {
        let panel = self.panel_for();
        let direction = if self.axis.is_vertical() {
            Direction::Vertical
        } else {
            Direction::Horizontal
        };
        self.settings.viewport(&panel, &self.language, direction)
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

        let pages = Pages::of(&laid, &viewport, self.axis);
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
        if let Some(location) = self.wanted.take() {
            self.page = self.page_of(location);
            self.sheet = None;
        }
    }

    /// The page of the chapter laid out that `location` falls on.
    fn page_of(&self, location: i64) -> usize {
        self.laid.as_ref().map_or(0, |laid| {
            laid.locations
                .iter()
                .rposition(|at| *at <= location)
                .unwrap_or(0)
        })
    }

    /// Look through every chapter for the phrase in hand.
    fn look(&mut self) {
        self.found.clear();
        self.opens.clear();
        self.searched = true;
        self.chrome.scroll = 0.0;
        let needle = self.query.trim().to_string();
        if needle.is_empty() {
            return;
        }
        for chapter in 0..self.spine.len() {
            let Ok(text) = self.book.load_chapter_cached(self.spine[chapter]) else {
                continue;
            };
            for found in text.find(&needle) {
                let (before, hit, after) = text.around(found, LEAD_IN, LEAD_OUT);
                let location = self.location_of(&text, found);
                self.found.push(search::Found {
                    before: before.to_string(),
                    found: hit.to_string(),
                    after: after.to_string(),
                    location,
                });
                self.opens.push(Opens { chapter, location });
            }
        }
    }

    /// The location a match falls in, or 0 where the book states no scale.
    fn location_of(&self, chapter: &bokai::model::Chapter, found: Match) -> i64 {
        let Some(positions) = self.positions.as_ref().filter(|map| map.has_locations()) else {
            return 0;
        };
        chapter
            .node_at(found.at)
            .and_then(|node| source_element(chapter, node))
            .and_then(|element| positions.position(element, 0))
            .map_or(0, |position| positions.location_for(position))
    }

    /// A key the search field takes, reporting whether it was the field's.
    fn typed(&mut self, key: &Key<&str>) -> bool {
        match key {
            Key::Named(NamedKey::Backspace) => {
                self.query.pop();
                self.searched = false;
            }
            Key::Named(NamedKey::Space) => {
                self.query.push(' ');
                self.searched = false;
            }
            Key::Named(NamedKey::Enter) => self.look(),
            Key::Character(text) => {
                self.query.push_str(text);
                self.searched = false;
            }
            _ => return false,
        }
        self.request_redraw();
        true
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

    /// Move an open list by `dots`, reporting whether one took it.
    fn scroll(&mut self, dots: f32) -> bool {
        let listed = match self.chrome.overlay {
            Overlay::GoTo | Overlay::Search => true,
            Overlay::Aa => self.chrome.tab == AaTab::More && self.chrome.pane == AaPane::Tab,
            _ => false,
        };
        if !listed {
            return false;
        }
        self.chrome.scroll = (self.chrome.scroll + dots).max(0.0);
        self.request_redraw();
        true
    }

    /// One step of a list, in panel dots: a row of it.
    fn scroll_step(&self) -> f32 {
        let panel = self.panel_for();
        let row = match self.chrome.overlay {
            Overlay::Aa => return aa::row_of(panel.size.height, panel.dpi),
            Overlay::Search => search::ROW,
            _ => goto::ROW,
        };
        row * panel.size.height / bars::REFERENCE
    }

    /// Whether the page after this one lies to its left, which is what a
    /// book stacking blocks right to left does.
    fn pages_leftward(&self) -> bool {
        self.axis == Axis::VerticalRl
    }

    /// Turn `by` pages, running on into the next chapter at either end. The
    /// bars go away with the page they were drawn over.
    fn turn(&mut self, by: isize) {
        self.chrome.revealed = false;
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
        // How far in the book stands: the location's share of `locations`.
        let read = (location as f32 / locations.max(1) as f32).clamp(0.0, 1.0);
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
            percent: (read * 100.0) as u32,
            minutes_left_in_chapter: minutes(words as f32 * left),
            minutes_left: minutes(words as f32 * (1.0 - read) * chapters as f32),
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
            hyphenates: !matches!(Script::of(&self.language), Script::Cjk),
        }
    }

    /// Act on a click or a key.
    fn act(&mut self, action: Action) {
        match action {
            Action::TurnPage(by) => self.turn(by),
            Action::Grid(grid) => {
                self.chrome.grid = grid;
                self.chrome.at = self.page;
                self.chrome.overlay = Overlay::Scrubber;
                self.request_redraw();
            }
            Action::Scrub(page) => {
                self.chrome.at = page;
                self.request_redraw();
            }
            Action::GoToPage(page) => {
                self.chrome.overlay = Overlay::None;
                self.chrome.at = page;
                self.page = page;
                self.sheet = None;
                self.request_redraw();
            }
            Action::Jump(by) => {
                let next = self.chapter as isize + by;
                if (0..self.spine.len() as isize).contains(&next) {
                    self.go_to(next as usize);
                    self.chrome.at = 0;
                }
            }
            Action::Open(overlay) => {
                self.chrome.overlay = overlay;
                self.chrome.pane = AaPane::Tab;
                self.chrome.scroll = 0.0;
                self.chrome.at = self.page;
                self.request_redraw();
            }
            Action::Close => {
                self.chrome.overlay = Overlay::None;
                self.chrome.scroll = 0.0;
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
            Action::Margins(stop) => self.restyle(|s| s.margins = stop),
            Action::Justified(on) => self.restyle(|s| s.justified = on),
            Action::Hyphenate(on) => self.restyle(|s| s.hyphenate = on),
            Action::Columns(columns) => self.restyle(|s| s.columns = columns),
            Action::Orient(orientation) => {
                self.settings.orientation = orientation;
                self.laid = None;
                self.sheet = None;
                self.resize_to_panel();
                self.request_redraw();
            }
            Action::Reveal(on) => {
                self.chrome.revealed = on;
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
            Action::GoToFound(found) => {
                self.chrome.overlay = Overlay::None;
                if let Some(opens) = self.opens.get(found) {
                    let (chapter, location) = (opens.chapter, opens.location);
                    self.wanted = Some(location);
                    self.go_to(chapter);
                }
            }
        }
    }

    /// The pages the scrubber offers: the one it stands at, or the nine
    /// around it, each rendered small enough to lay out at once.
    fn leaves(&mut self) -> Vec<(Pixmap, i64, usize)> {
        let panel = self.panel_for().size;
        let (grid, at) = (self.chrome.grid, self.chrome.at);
        let dark = self.chrome.dark;
        let Some(laid) = &self.laid else {
            return Vec::new();
        };
        let count = laid.pages.count();
        let (first, wanted) = if grid {
            (at.saturating_sub(4).min(count.saturating_sub(9)), 9)
        } else {
            (at, 1)
        };
        let scale = if grid { 0.3 } else { 0.86 };

        let mut out = Vec::new();
        for page in first..(first + wanted).min(count) {
            let Some(mut sheet) = Pixmap::new(
                (panel.width * scale).max(1.0) as u32,
                (panel.height * scale).max(1.0) as u32,
            ) else {
                continue;
            };
            sheet.fill(tiny_skia::Color::from_rgba8(0xff, 0xff, 0xff, 0xff));
            Painter::cached(&self.fonts, &self.resources, &mut self.cache).paint(
                &laid.root,
                laid.pages.window(page),
                laid.pages.origin(page),
                scale,
                &mut sheet.as_mut(),
            );
            if dark {
                turn_inside_out(&mut sheet);
            }
            out.push((sheet, laid.locations.get(page).copied().unwrap_or(0), page));
        }
        out
    }

    /// Hold the panel's shape through a resize: the window keeps the width it
    /// was given and takes the height the panel's own ratio states.
    fn hold_shape(&mut self, size: PhysicalSize<u32>) {
        let Some(view) = &self.view else { return };
        let panel = self.panel_for().size;
        let wanted = (size.width as f32 * panel.height / panel.width).round();
        if (wanted - size.height as f32).abs() > 2.0 {
            let _ = view
                .window
                .request_inner_size(PhysicalSize::new(size.width, wanted as u32));
        }
    }

    /// Ask the window for the panel's shape at the height it stands at.
    fn resize_to_panel(&mut self) {
        let Some(view) = &self.view else { return };
        let size = view.window.inner_size();
        let panel = self.panel_for().size;
        let width = (size.height as f32 * panel.width / panel.height).round();
        let _ = view
            .window
            .request_inner_size(PhysicalSize::new(width as u32, size.height));
    }

    /// The screen being emulated, which a named profile stands in for.
    fn device(&self) -> Device {
        self.device.unwrap_or_default()
    }

    /// Point the reading settings at the family the font list chose.
    fn apply_family(&mut self) {
        let Some(family) = self.families.get(self.settings.family).cloned() else {
            return;
        };
        let script = match Script::of(&self.language) {
            Script::Cjk => FaceScript::Cjk,
            _ => FaceScript::Latin,
        };
        let faces = faces_for(&family, script);
        let named: Vec<&str> = faces.iter().map(String::as_str).collect();
        self.fonts.reading_family(script, &named);
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
        let Some(target) = self.frame(physical.width, physical.height) else {
            return;
        };

        let view = self.view.as_mut().expect("checked at the top of draw");
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

    /// Write one frame at the panel's own size to `path`.
    fn shot(&mut self, path: &std::path::Path) -> Result<(), Box<dyn Error>> {
        let panel = self.panel_for().size;
        let frame = self
            .frame(panel.width as u32, panel.height as u32)
            .ok_or("nothing to draw")?;
        frame.save_png(path)?;
        Ok(())
    }

    /// Compose one frame `width` by `height` pixels: the page scaled to fit
    /// it, with the bars and whichever panel is open drawn over it.
    fn frame(&mut self, width: u32, height: u32) -> Option<Pixmap> {
        self.relayout();

        let panel = self.panel_for();
        let theme = self.chrome.theme();
        let fit = (width as f32 / panel.size.width).min(height as f32 / panel.size.height);
        let offset = (
            (width as f32 - panel.size.width * fit) / 2.0,
            (height as f32 - panel.size.height * fit) / 2.0,
        );

        let mut target = Pixmap::new(width, height)?;
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
            let mut sheet = Pixmap::new(
                panel.size.width.max(1.0) as u32,
                panel.size.height.max(1.0) as u32,
            )?;
            sheet.fill(tiny_skia::Color::from_rgba8(0xff, 0xff, 0xff, 0xff));
            if let Some(laid) = &self.laid {
                Painter::cached(&self.fonts, &self.resources, &mut self.cache).paint(
                    &laid.root,
                    laid.pages.window(self.page),
                    laid.pages.origin(self.page),
                    1.0,
                    &mut sheet.as_mut(),
                );
            }
            if self.chrome.dark {
                turn_inside_out(&mut sheet);
            }
            self.sheet = Some((sheet, key));
        }
        let (sheet, _) = self.sheet.as_ref()?;
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
        let pages_here = self.laid.as_ref().map_or(1, |laid| laid.pages.count());
        let leaves = match overlay {
            Overlay::Scrubber => self.leaves(),
            _ => Vec::new(),
        };
        let mut canvas = Canvas {
            target: &mut target.as_mut(),
            fonts: &mut self.fonts,
            cache: &mut self.cache,
            theme,
            panel: panel.size,
            dpi: panel.dpi,
            scale: fit,
            offset,
            clip: None,
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
            Overlay::Search => search::draw(
                &mut self.chrome,
                &mut canvas,
                &search::Search {
                    query: &self.query,
                    found: &self.found,
                    searched: self.searched,
                },
            ),
            Overlay::Scrubber => {
                let offered: Vec<scrub::Leaf<'_>> = leaves
                    .iter()
                    .map(|(sheet, location, page)| scrub::Leaf {
                        sheet,
                        location: *location,
                        page: *page,
                    })
                    .collect();
                scrub::draw(
                    &mut self.chrome,
                    &mut canvas,
                    &scrub::Scrub {
                        chapter_title: at.chapter_title.clone(),
                        leaves: &offered,
                        here: self.page,
                        pages: pages_here,
                        locations: at.locations,
                        leftward,
                    },
                );
            }
        }

        if let Some(view) = self.view.as_mut() {
            view.fit = (fit, offset);
        }
        self.prefetch();
        Some(target)
    }

    /// What the first `pages` pages of this chapter settled on.
    fn report(&mut self, pages: usize) {
        self.relayout();
        let Some(laid) = &self.laid else {
            eprintln!("nothing laid out");
            return;
        };
        let vertical = laid.pages.axis().is_vertical();
        let across = laid.pages.origin(0);
        let origin = if vertical { across.1 } else { across.0 };
        println!(
            "{:<6} {:>5} {:>5} {:>6} {:>6} {:>6} {:>6} {:>6} {:>5} {:>6}",
            "page", "elem", "line", "blk0", "blk1", "win", "in0", "in1", "adv", "loc"
        );
        for number in 0..pages.min(laid.pages.count()) {
            let window = laid.pages.window(number);
            let runs: Vec<&sidle_render::Fragment> = sidle_render::paint::shown(&laid.root, window)
                .into_iter()
                .filter(|fragment| fragment.kind == Node::Run)
                .collect();
            // Block positions on the page, against the window's own corner.
            let shift = if vertical { window.x } else { window.y };
            let lines: Vec<&sidle_render::Fragment> =
                sidle_render::paint::shown(&laid.root, window)
                    .into_iter()
                    .filter(|fragment| fragment.kind == Node::Line)
                    .collect();
            let mut blocks: Vec<f32> = lines
                .iter()
                .map(|run| if vertical { run.rect.x } else { run.rect.y } - shift)
                .collect();
            let far = lines
                .iter()
                .map(|run| {
                    if vertical {
                        run.rect.right()
                    } else {
                        run.rect.bottom()
                    }
                })
                .fold(f32::NEG_INFINITY, f32::max)
                - shift;
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
            println!(
                "{:<6} {:>5} {:>5} {:>6.0} {:>6.0} {:>6.0} {:>6.0} {:>6.0} {:>5.0} {:>6}",
                number + 1,
                runs.len(),
                blocks.len(),
                blocks.first().copied().unwrap_or(0.0) + laid.pages.origin(number).0,
                far + laid.pages.origin(number).0,
                if vertical {
                    window.width
                } else {
                    window.height
                },
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
        let panel = self.panel_for().size;
        let attributes = Window::default_attributes()
            .with_title(&self.title)
            .with_inner_size(LogicalSize::new(
                (WINDOW_HEIGHT * panel.width / panel.height) as f64,
                WINDOW_HEIGHT as f64,
            ));
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
            WindowEvent::Resized(size) => {
                self.hold_shape(size);
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => self.request_redraw(),
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
                // A wheel scrolls an open list and turns the page under none.
                let fit = self.view.as_ref().map_or(1.0, |view| view.fit.0).max(0.001);
                let step = self.scroll_step();
                let (lines, dots) = match delta {
                    MouseScrollDelta::LineDelta(_, lines) => (-lines, -lines * step),
                    MouseScrollDelta::PixelDelta(position) => {
                        (-position.y as f32 / 120.0, -position.y as f32 / fit)
                    }
                };
                if !self.scroll(dots) && lines.abs() >= 1.0 {
                    self.turn(lines.signum() as isize);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let key = event.logical_key.as_ref();
                if self.chrome.overlay == Overlay::Search && self.typed(&key) {
                    return;
                }
                match key {
                    Key::Named(NamedKey::ArrowLeft) => {
                        self.turn(if self.pages_leftward() { 1 } else { -1 })
                    }
                    Key::Named(NamedKey::ArrowRight) => {
                        self.turn(if self.pages_leftward() { -1 } else { 1 })
                    }
                    Key::Named(NamedKey::ArrowDown) => {
                        if !self.scroll(self.scroll_step()) {
                            self.turn(1);
                        }
                    }
                    Key::Named(NamedKey::ArrowUp) => {
                        if !self.scroll(-self.scroll_step()) {
                            self.turn(-1);
                        }
                    }
                    Key::Named(NamedKey::PageDown) | Key::Named(NamedKey::Space) => {
                        if !self.scroll(ROWS_A_LEAP * self.scroll_step()) {
                            self.turn(1);
                        }
                    }
                    Key::Named(NamedKey::PageUp) => {
                        if !self.scroll(-ROWS_A_LEAP * self.scroll_step()) {
                            self.turn(-1);
                        }
                    }
                    Key::Named(NamedKey::Escape) => self.act(Action::Close),
                    Key::Character("a") => self.act(Action::Open(Overlay::Aa)),
                    Key::Character("t") => self.act(Action::Open(Overlay::GoTo)),
                    Key::Character("s") => self.act(Action::Open(Overlay::Search)),
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

/// Turn a page's ink and ground inside out, which is what a dark page is.
fn turn_inside_out(sheet: &mut Pixmap) {
    for pixel in sheet.pixels_mut() {
        let alpha = pixel.alpha();
        if let Some(turned) = tiny_skia::PremultipliedColorU8::from_rgba(
            alpha - pixel.red(),
            alpha - pixel.green(),
            alpha - pixel.blue(),
            alpha,
        ) {
            *pixel = turned;
        }
    }
}
