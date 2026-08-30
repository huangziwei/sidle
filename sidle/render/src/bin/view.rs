//! A window that lays a book out and shows it a page at a time.
//!
//! Usage: `sidle-render-view <book> [chapter]`
//!
//! Arrow keys, space and the wheel turn pages; `n` and `p` change chapter.

use std::error::Error;
use std::num::NonZeroU32;
use std::rc::Rc;

use bokai::model::{Book, ChapterId};
use sidle_render::paint::Painter;
use sidle_render::{Axis, BookResources, Fonts, Layout, Pages, Viewport};
use tiny_skia::Pixmap;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: sidle-render-view <book> [chapter]");
        std::process::exit(2);
    };
    let chapter: usize = args.next().map_or(Ok(0), |n| n.parse())?;

    let mut book = Book::open(&path)?;
    let title = book.metadata().title.clone();
    let language = book.metadata().language.clone();
    let axis: Axis = book.writing_mode().into();
    let spine: Vec<ChapterId> = book.spine().iter().map(|entry| entry.id).collect();
    if spine.is_empty() {
        return Err(format!("{path} has no chapters").into());
    }
    let resources = BookResources::declared(&mut book);

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut Harness {
        chapter: chapter.min(spine.len() - 1),
        book,
        title,
        language,
        axis,
        spine,
        resources,
        fonts: Fonts::new(),
        page: 0,
        laid: None,
        view: None,
    })?;
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
    chapter: usize,
    page: usize,
    laid: Option<Laid>,
    view: Option<View>,
}

/// The chapter in hand, and the size it was laid out for.
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
    /// Lay the current chapter out for a page of `size`. A chapter laid out
    /// at that size is kept.
    fn relayout(&mut self, size: sidle_render::Size) {
        if self
            .laid
            .as_ref()
            .is_some_and(|laid| laid.viewport.size == size)
        {
            return;
        }
        let viewport =
            Viewport::new(size.width, size.height).with_language(Some(self.language.clone()));
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

    fn retitle(&self) {
        let (Some(view), Some(laid)) = (&self.view, &self.laid) else {
            return;
        };
        view.window.set_title(&format!(
            "{} — chapter {}/{} — page {}/{}",
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
        let scale = view.window.scale_factor() as f32;
        let (Some(width), Some(height)) = (
            NonZeroU32::new(physical.width),
            NonZeroU32::new(physical.height),
        ) else {
            return;
        };

        self.relayout(sidle_render::Size::new(
            physical.width as f32 / scale,
            physical.height as f32 / scale,
        ));
        let Some(laid) = &self.laid else { return };

        let mut pixmap =
            Pixmap::new(physical.width, physical.height).expect("window has a nonzero size");
        pixmap.fill(tiny_skia::Color::WHITE);
        Painter::new(&self.fonts, &self.resources).paint(
            &laid.root,
            laid.pages.window(self.page),
            laid.pages.origin(),
            scale,
            &mut pixmap.as_mut(),
        );

        let view = self.view.as_mut().expect("checked at the top of draw");
        view.surface
            .resize(width, height)
            .expect("surface resize failed");
        let mut buffer = view.surface.buffer_mut().expect("no buffer to draw into");
        for (out, pixel) in buffer.iter_mut().zip(pixmap.pixels()) {
            *out = (pixel.red() as u32) << 16 | (pixel.green() as u32) << 8 | pixel.blue() as u32;
        }
        buffer.present().expect("present failed");
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
                self.laid = None;
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
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
