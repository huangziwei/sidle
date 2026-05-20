//! Sidle native — Milestone 7 download flow end-to-end.
//!
//! Render the cover grid (M6). Tap a cover → overlay "Downloading…" →
//! stream `.kfx` to `/mnt/us/documents/Sidle/<filename>` → `touch
//! /mnt/us/system/.cleanindex` so the Kindle library indexer picks it up
//! → overlay "Downloaded" briefly → restore gallery.
//!
//! Single-tap = immediate download (no two-tap confirm). For personal use
//! with a 9-book library, accidental triggers are recoverable (re-download
//! is idempotent in the Kindle library — same filename overwrites).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod api;
mod config;
mod eink;
mod ui;

use eink::fb::{Framebuffer, MxcfbRect, WAVEFORM_MODE_GC16};
use eink::pillow::Pillow;
use eink::touch::Touch;
use image::DynamicImage;
use ui::grid;
use ui::text::TextRenderer;
use ui::toast;

const LOG_PATH: &str = "/mnt/us/sidle-native.log";
const CONFIG_PATH: &str = "/mnt/us/extensions/sidle/etc/server.conf";
const FONT_PX: f32 = 28.0;
const QUIT_BOX: u32 = 200;
const TOP_MARGIN: u32 = QUIT_BOX + 40;
/// Stock Kindle indexer watches `documents/` subfolders too (verified via
/// the existing `documents/Downloads/Items01/` indexed tree). Land here so
/// our books are grouped and easy to find in the library.
const DOWNLOAD_DIR: &str = "/mnt/us/documents/Sidle";
const CLEANINDEX: &str = "/mnt/us/system/.cleanindex";
const TOAST_LINGER: Duration = Duration::from_millis(1200);

fn main() {
    let result = run();
    log(format!("done: {result:?}"));
}

fn run() -> anyhow::Result<()> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    log(format!("sidle-native M7 start: ts={ts}"));

    let cfg = config::load(Path::new(CONFIG_PATH))?;
    log(format!("server: http://{}:{}", cfg.host, cfg.port));

    let t0 = Instant::now();
    let books = api::list_books(&cfg)?;
    log(format!("books: {} (list in {:?})", books.len(), t0.elapsed()));

    let t_cov = Instant::now();
    let covers: Vec<Option<DynamicImage>> = books
        .iter()
        .map(|book| {
            match api::fetch_cover(&cfg, book.id).and_then(|b| grid::decode_resize(&b)) {
                Ok(img) => Some(img),
                Err(err) => {
                    log(format!("cover {}: {err}", book.id));
                    None
                }
            }
        })
        .collect();
    log(format!("covers decoded in {:?}", t_cov.elapsed()));

    let mut renderer = TextRenderer::load(FONT_PX)?;

    let _pillow = Pillow::disable()?;
    let mut fb = Framebuffer::open()?;
    let mut touch = Touch::open()?;

    let (grid_left, grid_top) = grid::grid_origin(fb.var.xres, TOP_MARGIN);
    draw_gallery(&mut fb, &mut renderer, &books, &covers, grid_left, grid_top)?;
    log("initial render");

    loop {
        let (tx, ty) = touch.next_tap()?;
        log(format!("tap: ({tx},{ty})"));

        if tx < QUIT_BOX && ty < QUIT_BOX {
            log("quit-corner tap");
            break;
        }

        let Some(idx) = grid::cell_at_tap(tx, ty, grid_left, grid_top, books.len()) else {
            continue;
        };
        let book = &books[idx];
        log(format!("tap on book {}: {}", book.id, book.title));

        // Show "Downloading <title>" overlay before the blocking HTTP call,
        // so the user has immediate visual feedback.
        let msg = format!("Downloading {}…", truncate_title(&book.title, 40));
        let dirty = toast::draw(&mut fb, &mut renderer, &msg);
        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;

        let dl_t0 = Instant::now();
        let result = api::download_book(&cfg, book.id).and_then(|d| {
            persist(&d.filename, &d.bytes).map(|saved| (saved, d.bytes.len()))
        });

        let banner_msg = match result {
            Ok((saved, n)) => {
                log(format!(
                    "downloaded {} bytes to {} in {:?}",
                    n,
                    saved.display(),
                    dl_t0.elapsed()
                ));
                let _ = Command::new("touch").arg(CLEANINDEX).output();
                format!("Downloaded → Library will refresh shortly")
            }
            Err(err) => {
                log(format!("download failed: {err:#}"));
                format!("Failed: {err}")
            }
        };
        let dirty = toast::draw(&mut fb, &mut renderer, &banner_msg);
        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
        thread::sleep(TOAST_LINGER);

        // Restore the gallery underneath. Easier than tracking the toast's
        // exact pixels — a full-screen GC16 redraw is ~600ms but only once
        // per download.
        draw_gallery(&mut fb, &mut renderer, &books, &covers, grid_left, grid_top)?;
    }
    Ok(())
}

fn draw_gallery(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    books: &[api::Book],
    covers: &[Option<DynamicImage>],
    grid_left: i32,
    grid_top: i32,
) -> anyhow::Result<()> {
    fb.fill_rect(0, 0, fb.var.xres, fb.var.yres, 0xFF);
    for (i, cover) in covers.iter().enumerate() {
        let (cx, cy) = grid::cell_xy(grid_left, grid_top, i);
        if cx < 0 || cy < 0 {
            continue;
        }
        match cover {
            Some(img) => grid::blit_cell(fb, cx, cy, img),
            None => {
                grid::blit_placeholder(fb, cx, cy, 0xDD);
                let baseline = cy + grid::CELL_H as i32 / 2;
                renderer.draw(fb, cx + 16, baseline, &books[i].title, false);
            }
        }
    }
    fb.send_update(
        MxcfbRect { top: 0, left: 0, width: fb.var.xres, height: fb.var.yres },
        WAVEFORM_MODE_GC16,
    )?;
    Ok(())
}

/// Persist downloaded bytes to `/mnt/us/documents/Sidle/<filename>`. Creates
/// the Sidle dir on first download. Returns the final on-disk path.
fn persist(filename: &str, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    let dir = Path::new(DOWNLOAD_DIR);
    std::fs::create_dir_all(dir)?;
    // Strip any path components the server might have set in the filename
    // — defense against `..` traversal even though the server controls it.
    let safe_name = Path::new(filename)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("book.kfx");
    let path = dir.join(safe_name);
    std::fs::write(&path, bytes)?;
    Ok(path)
}

fn truncate_title(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn log(line: impl AsRef<str>) {
    let line = line.as_ref();
    let log_path = if std::path::Path::new("/mnt/us").is_dir() {
        LOG_PATH
    } else {
        "./sidle-native.log"
    };
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(f, "{line}");
    }
    let _ = writeln!(std::io::stderr(), "{line}");
}
