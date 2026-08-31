//! Scores laid-out pages against captured ones. Both sides come from
//! directories the environment names, and the test skips where they are
//! absent.
//!
//! ```text
//! SIDLE_RENDER_CAPTURES=<dir>  one <name>_element.txt per probe
//! SIDLE_RENDER_BOOKS=<dir>     one <name>.kfx per probe
//! SIDLE_RENDER_PANEL=<file>    the ladders those pages were drawn at
//! SIDLE_RENDER_FONTS=<dir>     faces to search ahead of the host's
//! SIDLE_RENDER_SERIF=<family>  what the reading settings chose
//! SIDLE_RENDER_CJK=<family>
//! ```

#![cfg(all(feature = "oracle", feature = "probe"))]

use std::path::PathBuf;

use bokai::model::{Book, ChapterId};
use sidle_render::font::Script as FaceScript;
use sidle_render::oracle::Capture;
use sidle_render::settings::{Direction, Panel};
use sidle_render::{Axis, BookResources, Fonts, Layout, Node, Pages, Settings, Viewport};

/// Probes whose four numbers must keep agreeing to the dot.
const EXACT: &[&str] = &[
    "border-top-8px-solid",
    "control-japanese",
    "control-latin",
    "control-short-words",
    "kfx-kerning-off",
    "line-height-150p",
    "margin-collapse",
    "margin-top-24px",
    "text-indent-paired",
];

/// The four numbers both sides state.
#[derive(Debug, PartialEq)]
struct Geometry {
    lines: usize,
    pitch: i32,
    start: i32,
    end: i32,
}

fn directory(key: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(key)?);
    path.exists().then_some(path)
}

#[test]
fn every_probe_that_agreed_still_agrees() {
    let (Some(captures), Some(books), Some(panel)) = (
        directory("SIDLE_RENDER_CAPTURES"),
        directory("SIDLE_RENDER_BOOKS"),
        directory("SIDLE_RENDER_PANEL"),
    ) else {
        eprintln!("no captures, books or panel named: nothing to score");
        return;
    };
    let panel = Panel::read(&panel).expect("the panel profile parses");

    let mut fonts = Fonts::new();
    if let Some(faces) = directory("SIDLE_RENDER_FONTS") {
        fonts.add_directory(faces);
    }
    if let Ok(family) = std::env::var("SIDLE_RENDER_SERIF") {
        fonts.reading_family(FaceScript::Latin, &[&family]);
    }
    if let Ok(family) = std::env::var("SIDLE_RENDER_CJK") {
        fonts.reading_family(FaceScript::Cjk, &[&family]);
    }

    let mut scored = 0;
    let mut wrong: Vec<String> = Vec::new();
    for name in EXACT {
        let capture = captures.join(format!("{name}_element.txt"));
        let book = books.join(format!("{name}.kfx"));
        if !capture.exists() || !book.exists() {
            continue;
        }
        scored += 1;
        let device = from_capture(&Capture::read(&capture).expect("the capture reads"));
        let ours = from_layout(&book, &panel, &mut fonts);
        if device != ours {
            wrong.push(format!("{name}: device {device:?}, laid out {ours:?}"));
        }
    }

    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    eprintln!("{scored} probes scored");
}

/// `Geometry` from a captured page.
fn from_capture(capture: &Capture) -> Geometry {
    let vertical = capture.is_vertical();
    let rects: Vec<sidle_render::Rect> = capture.runs().iter().filter_map(|e| e.rect()).collect();
    let mut blocks: Vec<f32> = rects
        .iter()
        .map(|r| if vertical { r.x } else { r.y })
        .collect();
    blocks.sort_by(f32::total_cmp);
    blocks.dedup();

    Geometry {
        lines: blocks.len(),
        pitch: shortest_step(&blocks),
        start: rects
            .iter()
            .map(|r| if vertical { r.y } else { r.x })
            .fold(f32::INFINITY, f32::min)
            .round() as i32,
        end: rects
            .iter()
            .map(|r| if vertical { r.bottom() } else { r.right() })
            .fold(f32::NEG_INFINITY, f32::max)
            .round() as i32,
    }
}

/// `Geometry` from laying `path` out.
fn from_layout(path: &PathBuf, panel: &Panel, fonts: &mut Fonts) -> Geometry {
    let mut book = Book::open(path).expect("the probe book opens");
    let language = book.metadata().language.clone();
    let axis: Axis = book.writing_mode().into();
    let spine: Vec<ChapterId> = book.spine().iter().map(|entry| entry.id).collect();
    let mut resources = BookResources::declared(&mut book);
    let chapter = book
        .load_chapter(spine[0])
        .expect("the probe book has a chapter");
    resources.load(&mut book, &chapter);

    let direction = if axis.is_vertical() {
        Direction::Vertical
    } else {
        Direction::Horizontal
    };
    let viewport: Viewport = Settings::default_for(panel).viewport(panel, &language, direction);
    let laid = Layout {
        viewport: viewport.clone(),
        fonts,
        resources: &resources,
        axis,
    }
    .chapter(&chapter);
    let pages = Pages::of(&laid, &viewport);

    let vertical = pages.axis().is_vertical();
    let (left, top) = pages.origin(0);
    let along = if vertical { top } else { left };
    let window = pages.window(0);
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

    Geometry {
        lines: blocks.len(),
        pitch: shortest_step(&blocks),
        start: (runs
            .iter()
            .map(|run| if vertical { run.rect.y } else { run.rect.x })
            .fold(f32::INFINITY, f32::min)
            + along)
            .round() as i32,
        end: (runs
            .iter()
            .map(|run| {
                if vertical {
                    run.rect.bottom()
                } else {
                    run.rect.right()
                }
            })
            .fold(f32::NEG_INFINITY, f32::max)
            + along)
            .round() as i32,
    }
}

/// The closest two neighbours in `sorted`: a page's line pitch.
fn shortest_step(sorted: &[f32]) -> i32 {
    sorted
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .fold(f32::INFINITY, f32::min)
        .max(0.0)
        .round() as i32
}
