//! Sidle native — Milestone 9 paginated cover grid + download flow.
//!
//! 3×3 grid per page, prev/next bottom-strip controls when the library
//! overflows one page. Tap a cover → overlay "Downloading…" → stream
//! `.kfx` to `/mnt/us/documents/Sidle/<filename>` → `touch
//! /mnt/us/system/.cleanindex` → overlay "Downloaded" → restore gallery.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod api;
mod config;
mod device_state;
mod eink;
mod orientation;
mod ui;

use eink::fb::{Framebuffer, MxcfbRect, WAVEFORM_MODE_GC16};
use eink::pillow::Pillow;
use eink::touch::Touch;
use image::DynamicImage;
use ui::grid;
use ui::pager::{self, PAGE_SIZE, PagerHit};
use ui::text::TextRenderer;
use ui::toast;

const LOG_PATH: &str = "/mnt/us/sidle-native.log";
const CONFIG_PATH: &str = "/mnt/us/extensions/sidle/etc/server.conf";
const FONT_PX: f32 = 28.0;
/// Quit moved off the implicit top-left corner (too easy to trigger
/// accidentally with stray touch events near the panel edge) — exit is
/// now an explicit `✕ Exit` tap zone in the bottom strip. Top margin can
/// drop back to a small visual gap.
const TOP_MARGIN: u32 = 80;
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
    log(format!("sidle-native M9 start: ts={ts}"));

    let cfg = config::load(Path::new(CONFIG_PATH))?;
    log(format!("server: http://{}:{}", cfg.host, cfg.port));

    let t0 = Instant::now();
    let books = match api::list_books(&cfg) {
        Ok(b) => b,
        Err(err) => {
            // Any failure here is the user's experience of "I tapped
            // Sidle and nothing happened" — the binary used to just
            // exit and KUAL would flash back. We now always render a
            // boot toast with whatever we know, even if it's a vague
            // network error, so the user has *something* on screen to
            // act on instead of having to tail the log.
            let (log_line, screen_msg) = match &err {
                api::SidleError::TokenMismatch => (
                    "token rejected by sidle-server (401/403); resync via sidle desktop app".to_string(),
                    "Token mismatch.\nPlug Kindle into sidle and click Update KUAL.".to_string(),
                ),
                api::SidleError::Other(e) => (
                    format!("list_books failed: {e:#}"),
                    // Keep the e-ink message terse — multi-line tiny
                    // text is hard to read. First line names the
                    // class, second is a hint.
                    format!(
                        "Couldn't reach sidle server.\nIs it running on {}:{}?",
                        cfg.host, cfg.port
                    ),
                ),
            };
            log(log_line);
            let _ = draw_boot_toast(&screen_msg);
            return Ok(());
        }
    };
    let total_from_server = books.len();

    // Hide books that already live on this Kindle. The picker is a
    // transfer queue, not a library viewer — stock library answers
    // "what do I have" perfectly. Source of truth is the sha8 embedded
    // in each filename under /mnt/us/documents/Sidle/ (see
    // device_state.rs). Missing kfx_sha256 on the row means we can't
    // dedupe; show it anyway so the user isn't silently dropped books.
    let downloaded = device_state::scan_downloaded_shas(Path::new(DOWNLOAD_DIR));
    let books: Vec<api::Book> = books
        .into_iter()
        .filter(|b| match b.kfx_sha256.as_deref() {
            Some(sha) if sha.len() >= 8 => !downloaded.contains(&sha[..8]),
            _ => true,
        })
        .collect();

    let total_pages = pager::n_pages(books.len());
    log(format!(
        "books: {} of {} ({} on device, {} pages, list in {:?})",
        books.len(),
        total_from_server,
        downloaded.len(),
        total_pages,
        t0.elapsed()
    ));

    let t_cov = Instant::now();
    let covers: Vec<Option<DynamicImage>> = books
        .iter()
        .map(|book| match api::fetch_cover(&cfg, book.id) {
            Ok(bytes) => match grid::decode_resize(&bytes) {
                Ok(img) => Some(img),
                Err(err) => {
                    log(format!("cover {}: {err}", book.id));
                    None
                }
            },
            Err(err) => {
                log(format!("cover {}: {err}", book.id));
                None
            }
        })
        .collect();
    log(format!("covers decoded in {:?}", t_cov.elapsed()));

    let mut renderer = TextRenderer::load(FONT_PX)?;

    let orient = orientation::Orientation::detect();
    log(format!("orientation: {orient:?}"));

    let _pillow = Pillow::disable()?;
    // Open fb in Up — Kindle kernel rotates fb writes itself based on
    // framework orientation. Our transform double-rotates. Touch needs
    // the transform because raw evdev events are not pre-rotated.
    let mut fb = Framebuffer::open(orientation::Orientation::Up)?;
    let mut touch = Touch::open(orient, fb.var.xres, fb.var.yres)?;

    let (grid_left, grid_top) = grid::grid_origin(fb.var.xres, TOP_MARGIN);
    let mut page: usize = 0;
    draw_gallery_page(
        &mut fb,
        &mut renderer,
        &books,
        &covers,
        page,
        total_pages,
        grid_left,
        grid_top,
    )?;
    log("initial render");

    loop {
        let (tx, ty) = touch.next_tap()?;
        log(format!("tap: ({tx},{ty})"));

        // Toolbar strip always wins over cell hit-test. Exit lives there
        // now; page nav too when there's more than one page.
        if let Some(hit) = pager::hit(tx, ty, fb.var.xres, fb.var.yres, total_pages) {
            match hit {
                PagerHit::Exit => {
                    log("exit-button tap");
                    break;
                }
                PagerHit::Prev => {
                    let new_page = page.saturating_sub(1);
                    if new_page != page {
                        page = new_page;
                        draw_gallery_page(
                            &mut fb, &mut renderer, &books, &covers, page,
                            total_pages, grid_left, grid_top,
                        )?;
                    }
                }
                PagerHit::Next => {
                    let new_page = (page + 1).min(total_pages.saturating_sub(1));
                    if new_page != page {
                        page = new_page;
                        draw_gallery_page(
                            &mut fb, &mut renderer, &books, &covers, page,
                            total_pages, grid_left, grid_top,
                        )?;
                    }
                }
            }
            continue;
        }

        let visible_count = books.len().saturating_sub(page * PAGE_SIZE).min(PAGE_SIZE);
        let Some(cell_idx) = grid::cell_at_tap(tx, ty, grid_left, grid_top, visible_count) else {
            continue;
        };
        let book_idx = page * PAGE_SIZE + cell_idx;
        let book = &books[book_idx];
        log(format!("tap on book {}: {}", book.id, book.title));

        let msg = format!("Downloading {}…", truncate_title(&book.title, 40));
        let dirty = toast::draw(&mut fb, &mut renderer, &msg);
        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;

        let dl_t0 = Instant::now();
        let banner_msg = match api::download_book(&cfg, book) {
            Ok(d) => match persist(&d.filename, &d.bytes) {
                Ok(saved) => {
                    log(format!(
                        "downloaded {} bytes to {} in {:?}",
                        d.bytes.len(),
                        saved.display(),
                        dl_t0.elapsed()
                    ));
                    let _ = Command::new("touch").arg(CLEANINDEX).output();
                    "Downloaded → Library will refresh shortly".to_string()
                }
                Err(err) => {
                    log(format!("persist failed: {err:#}"));
                    format!("Failed: {err}")
                }
            },
            Err(api::SidleError::TokenMismatch) => {
                // Token rotated mid-session (unusual: list_books got
                // through, then the server rotated). Same breadcrumb
                // as the boot-time mismatch.
                log("token rejected during download — resync via sidle desktop app".to_string());
                "Token mismatch.\nPlug Kindle into sidle and click Update KUAL.".to_string()
            }
            Err(api::SidleError::Other(err)) => {
                log(format!("download failed: {err:#}"));
                format!("Failed: {err}")
            }
        };
        let dirty = toast::draw(&mut fb, &mut renderer, &banner_msg);
        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
        thread::sleep(TOAST_LINGER);

        draw_gallery_page(
            &mut fb,
            &mut renderer,
            &books,
            &covers,
            page,
            total_pages,
            grid_left,
            grid_top,
        )?;
    }
    Ok(())
}

fn draw_gallery_page(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    books: &[api::Book],
    covers: &[Option<DynamicImage>],
    page: usize,
    total_pages: usize,
    grid_left: i32,
    grid_top: i32,
) -> anyhow::Result<()> {
    fb.fill_rect(0, 0, fb.var.xres, fb.var.yres, 0xFF);
    let start = page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(books.len());
    for (cell_idx, book_idx) in (start..end).enumerate() {
        let (cx, cy) = grid::cell_xy(grid_left, grid_top, cell_idx);
        if cx < 0 || cy < 0 {
            continue;
        }
        match &covers[book_idx] {
            Some(img) => grid::blit_cell(fb, cx, cy, img),
            None => {
                grid::blit_placeholder(fb, cx, cy, 0xDD);
                let baseline = cy + grid::CELL_H as i32 / 2;
                renderer.draw(fb, cx + 16, baseline, &books[book_idx].title, false);
            }
        }
    }
    if total_pages > 1 {
        pager::draw(fb, renderer, page, total_pages);
    }
    fb.send_update(
        MxcfbRect { top: 0, left: 0, width: fb.var.xres, height: fb.var.yres },
        WAVEFORM_MODE_GC16,
    )?;
    Ok(())
}

/// Boot-time toast for unrecoverable startup errors (token mismatch
/// today; could grow). Opens the framebuffer just long enough to
/// flash a message, then returns. Best-effort: any error short-
/// circuits silently — caller has already logged the underlying
/// problem and the worst-case is the same "nothing happened" symptom
/// users were getting before.
fn draw_boot_toast(msg: &str) -> anyhow::Result<()> {
    let mut renderer = TextRenderer::load(FONT_PX)?;
    let orient = orientation::Orientation::detect();
    let _pillow = Pillow::disable()?;
    let mut fb = Framebuffer::open(orientation::Orientation::Up)?;
    let dirty = toast::draw(&mut fb, &mut renderer, msg);
    fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
    // Linger 3× normal — boot toast is the only thing the user will
    // see, vs in-app toasts that get redrawn over by the gallery.
    thread::sleep(TOAST_LINGER * 3);
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
