//! Opens a book and shows it a page at a time.
//!
//! ```text
//! sidle-render-view [options] <book>
//!
//!   --panel <file>     panel ladders, in the form `Panel::parse` reads. The
//!                      page is laid out at the panel's own size and scaled
//!                      into the window. Without one the window is the page.
//!   --fonts <dir>      faces to search ahead of the host's installed ones
//!   --serif <family>   the family the reading settings choose for Latin
//!   --cjk <family>     the same for Chinese, Japanese and Korean
//!   --chapter <n>      open at this chapter
//!   --pages <n>        print the geometry of the first `n` pages and exit,
//!                      in the columns `sidle-render-capture` prints
//!   --lines            list every line of every page reported
//! ```
//!
//! Arrow keys, space and the wheel turn pages; `n` and `p` change chapter;
//! `+` and `-` walk the font-size ladder, `[` and `]` the line spacing, `m`
//! the margins; `q` quits.

use std::error::Error;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;

use bokai::model::{Book, ChapterId};
use sidle_render::font::Script as FaceScript;
use sidle_render::paint::Painter;
use sidle_render::settings::{Direction, Panel, Stop};
use sidle_render::{Axis, BookResources, Fonts, Layout, Pages, Settings, Size, Viewport};
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

const USAGE: &str = "usage: sidle-render-view [--panel <file>] [--fonts <dir>] \
[--serif <family>] [--cjk <family>] [--chapter <n>] <book>";

/// What the command line asked for.
#[derive(Default)]
struct Options {
    book: Option<PathBuf>,
    panel: Option<PathBuf>,
    fonts: Vec<PathBuf>,
    serif: Option<String>,
    cjk: Option<String>,
    chapter: usize,
    /// Report this many pages and open no window.
    pages: Option<usize>,
    /// List every line as well as each page.
    per_line: bool,
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
            "--panel" => options.panel = Some(PathBuf::from(value()?)),
            "--fonts" => options.fonts.push(PathBuf::from(value()?)),
            "--serif" => options.serif = Some(value()?),
            "--cjk" => options.cjk = Some(value()?),
            "--chapter" => options.chapter = value()?.parse()?,
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
    let resources = BookResources::declared(&mut book);

    let panel = options.panel.as_ref().map(Panel::read).transpose()?;
    let settings = panel.as_ref().map(Settings::default_for);

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

    let mut harness = Harness {
        chapter: options.chapter.min(spine.len() - 1),
        book,
        title,
        language,
        axis,
        spine,
        resources,
        fonts,
        panel,
        settings,
        per_line: options.per_line,
        page: 0,
        laid: None,
        view: None,
    };

    if let Some(pages) = options.pages {
        harness.report(pages);
        return Ok(());
    }

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut harness)?;
    Ok(())
}

struct Harness {
    book: Book,
    title: String,
    language: String,
    axis: Axis,
    spine: Vec<ChapterId>,
    resources: BookResources,
    fonts: Fonts,
    /// The panel being laid out for. Without one the window is the page.
    panel: Option<Panel>,
    /// Where each reading setting sits, alongside `panel`.
    settings: Option<Settings>,
    /// Whether `report` lists every line as well as each page.
    per_line: bool,
    chapter: usize,
    page: usize,
    laid: Option<Laid>,
    view: Option<View>,
}

/// The chapter in hand, and the viewport it was laid out for.
struct Laid {
    pages: Pages,
    root: sidle_render::Fragment,
    viewport: Viewport,
}

struct View {
    window: Rc<Window>,
    // Held for as long as the surface it created is alive.
    _context: softbuffer::Context<Rc<Window>>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
}

impl Harness {
    /// The area a page is laid out into: `panel`'s, or `window`.
    fn viewport(&self, window: Size) -> Viewport {
        let direction = if self.axis.is_vertical() {
            Direction::Vertical
        } else {
            Direction::Horizontal
        };
        match (&self.panel, &self.settings) {
            (Some(panel), Some(settings)) => settings.viewport(panel, &self.language, direction),
            _ => Viewport::new(window.width, window.height)
                .with_language(Some(self.language.clone())),
        }
    }

    /// Lay the current chapter out. A chapter laid out for this viewport is
    /// kept.
    fn relayout(&mut self, window: Size) {
        let viewport = self.viewport(window);
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
        self.resources.load(&mut self.book, &chapter);
        let laid = Layout {
            viewport: viewport.clone(),
            fonts: &mut self.fonts,
            resources: &self.resources,
            axis: self.axis,
        }
        .chapter(&chapter);

        let pages = Pages::of(&laid, &viewport);
        self.page = self.page.min(pages.count().saturating_sub(1));
        self.laid = Some(Laid {
            pages,
            root: laid.root,
            viewport,
        });
        self.retitle();
    }

    /// What the first `pages` pages of this chapter settled on.
    fn report(&mut self, pages: usize) {
        let window = self
            .panel
            .as_ref()
            .map_or(Size::new(600.0, 800.0), |panel| panel.size);
        self.relayout(window);
        let Some(laid) = &self.laid else {
            eprintln!("nothing laid out");
            return;
        };
        let vertical = laid.pages.axis().is_vertical();
        // A `Fragment` is measured from the content box's corner.
        let (left_margin, top_margin) = laid.pages.origin();
        let origin = if vertical { top_margin } else { left_margin };
        let block_origin = if vertical { left_margin } else { top_margin };

        println!(
            "{:<6} {:>5} {:>5} {:>6} {:>6} {:>6} {:>6} {:>6}",
            "page", "elem", "line", "top", "pitch", "left", "right", "adv"
        );
        for number in 0..pages.min(laid.pages.count()) {
            let window = laid.pages.window(number);
            let runs: Vec<&sidle_render::Fragment> = laid
                .root
                .of_kind(sidle_render::fragment::Node::Run)
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
                "{:<6} {:>5} {:>5} {:>6.0} {:>6.0} {:>6.0} {:>6.0} {:>6.0}",
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

    fn go_to(&mut self, chapter: usize) {
        if chapter == self.chapter || chapter >= self.spine.len() {
            return;
        }
        self.chapter = chapter;
        self.page = 0;
        self.laid = None;
        self.request_redraw();
    }

    fn turn(&mut self, by: isize) {
        let Some(laid) = &self.laid else { return };
        let last = laid.pages.count().saturating_sub(1);
        let next = (self.page as isize + by).clamp(0, last as isize) as usize;
        if next != self.page {
            self.page = next;
            self.retitle();
            self.request_redraw();
        }
    }

    /// Change a reading setting and lay the chapter out again.
    fn adjust(&mut self, change: impl FnOnce(&mut Settings, &Panel)) {
        let (Some(panel), Some(settings)) = (&self.panel, &mut self.settings) else {
            return;
        };
        change(settings, panel);
        self.laid = None;
        self.request_redraw();
    }

    fn retitle(&self) {
        let (Some(view), Some(laid)) = (&self.view, &self.laid) else {
            return;
        };
        let stops = match (&self.panel, &self.settings) {
            (Some(panel), Some(settings)) => format!(
                " — {:.2} pt, spacing {:?}, margins {:?}",
                settings.font_size_pt(panel, sidle_render::settings::Script::of(&self.language)),
                settings.line_spacing,
                settings.margins,
            ),
            _ => String::new(),
        };
        view.window.set_title(&format!(
            "{} — chapter {}/{} — page {}/{}{stops}",
            self.title,
            self.chapter + 1,
            self.spine.len(),
            self.page + 1,
            laid.pages.count()
        ));
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
        let scale = view.window.scale_factor() as f32;

        self.relayout(Size::new(
            physical.width as f32 / scale,
            physical.height as f32 / scale,
        ));
        let Some(laid) = &self.laid else { return };

        // The page is drawn at its own size and fitted into the window.
        let page = laid.viewport.size;
        let mut sheet = match Pixmap::new(page.width.max(1.0) as u32, page.height.max(1.0) as u32) {
            Some(pixmap) => pixmap,
            None => return,
        };
        sheet.fill(tiny_skia::Color::WHITE);
        Painter::new(&self.fonts, &self.resources).paint(
            &laid.root,
            laid.pages.window(self.page),
            laid.pages.origin(),
            1.0,
            &mut sheet.as_mut(),
        );

        let fit = (physical.width as f32 / page.width).min(physical.height as f32 / page.height);
        let mut target =
            Pixmap::new(physical.width, physical.height).expect("the window has a nonzero size");
        target.fill(tiny_skia::Color::from_rgba8(0x20, 0x20, 0x20, 0xff));
        let left = (physical.width as f32 - page.width * fit) / 2.0;
        let top = (physical.height as f32 - page.height * fit) / 2.0;
        target.draw_pixmap(
            0,
            0,
            sheet.as_ref(),
            &PixmapPaint {
                quality: FilterQuality::Bilinear,
                ..PixmapPaint::default()
            },
            Transform::from_translate(left, top).pre_scale(fit, fit),
            None,
        );

        let view = self.view.as_mut().expect("checked at the top of draw");
        view.surface
            .resize(width, height)
            .expect("surface resize failed");
        let mut buffer = view.surface.buffer_mut().expect("no buffer to draw into");
        for (out, pixel) in buffer.iter_mut().zip(target.pixels()) {
            *out = (pixel.red() as u32) << 16 | (pixel.green() as u32) << 8 | pixel.blue() as u32;
        }
        buffer.present().expect("present failed");
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

impl ApplicationHandler for Harness {
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
        });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                // Only a window-sized page depends on the window.
                if self.panel.is_none() {
                    self.laid = None;
                }
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => self.draw(),
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
                    Key::Named(NamedKey::ArrowRight)
                    | Key::Named(NamedKey::ArrowDown)
                    | Key::Named(NamedKey::PageDown)
                    | Key::Named(NamedKey::Space) => self.turn(1),
                    Key::Named(NamedKey::ArrowLeft)
                    | Key::Named(NamedKey::ArrowUp)
                    | Key::Named(NamedKey::PageUp) => self.turn(-1),
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Character("n") => self.go_to(self.chapter + 1),
                    Key::Character("p") => self.go_to(self.chapter.saturating_sub(1)),
                    Key::Character("q") => event_loop.exit(),
                    Key::Character("+") | Key::Character("=") => {
                        self.adjust(|settings, panel| {
                            let stops = panel.font_sizes.values().map(Vec::len).max().unwrap_or(1);
                            settings.font_size = (settings.font_size + 1).min(stops - 1);
                        });
                    }
                    Key::Character("-") => self.adjust(|settings, _| {
                        settings.font_size = settings.font_size.saturating_sub(1);
                    }),
                    Key::Character("]") => self.adjust(|settings, _| {
                        settings.line_spacing = next_stop(settings.line_spacing);
                    }),
                    Key::Character("[") => self.adjust(|settings, _| {
                        settings.line_spacing = next_stop(next_stop(settings.line_spacing));
                    }),
                    Key::Character("m") => self.adjust(|settings, _| {
                        settings.margins = next_stop(settings.margins);
                    }),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
