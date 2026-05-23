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
mod cover_cache;
mod device_state;
mod eink;
mod orientation;
mod ui;
mod wrap;

use eink::fb::{Framebuffer, MxcfbRect, WAVEFORM_MODE_DU, WAVEFORM_MODE_GC16};
use eink::buttons::{Buttons, PageButton};
use eink::input::{Input, InputEvent};
use eink::touch::{Touch, TouchEvent};
use image::DynamicImage;
use ui::diag;
use ui::filter::{self, Filters};
use ui::filtermenu;
use ui::grid;
use ui::pager::{self, PAGE_SIZE, PagerHit};
use ui::sort::SortState;
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
/// On-device cover thumbnail cache, under the extension dir (not documents/,
/// so the stock indexer never sees it). See [`cover_cache`].
const COVER_CACHE_DIR: &str = "/mnt/us/extensions/sidle/cache/covers";
const CLEANINDEX: &str = "/mnt/us/system/.cleanindex";
const TOAST_LINGER: Duration = Duration::from_millis(1200);
/// Minimum hold duration on a cell for a long-press to fire a download.
/// Shorter than this and we treat the touch as a misclick, showing a
/// "hold to download" discovery hint instead of starting the download.
/// 1s is on the slow side (stock Kindle's long-press is ~500ms) but
/// the cost of a wrong download (1–10s + a wrong file on the device)
/// is high enough that deliberateness wins over speed. Tune on
/// hardware if it feels sluggish.
const LONG_PRESS_THRESHOLD: Duration = Duration::from_millis(1000);

/// Cell currently outlined and awaiting either a long-enough hold to
/// fire a download or a too-short release to fire the discovery hint.
struct Armed {
    book_idx: usize,
    down_at: Instant,
}

fn main() {
    // X11-window proof-of-concept (see eink::x11poc): validates that a
    // Sidle-created window is WM-managed + recomposited on teardown before we
    // port the renderer off raw /dev/fb0. Bypasses all fb/config setup.
    if std::env::args().any(|a| a == "--x11-poc") {
        let r = eink::x11poc::run(|m| log(m));
        log(format!("x11poc done: {r:?}"));
        return;
    }
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

    // One agent for the whole session: HTTP keep-alive across list + covers +
    // download over a single warm connection (see api::get_with_token).
    let agent = ureq::AgentBuilder::new().build();
    let cache_dir = Path::new(COVER_CACHE_DIR);

    // Open the X11 window, input, and renderer *before* the first network
    // call. The Diagnostics screen (shown when list_books can't reach the
    // server) needs the surface to render and `input` to take taps, so all
    // device setup is hoisted above list_books and the call is wrapped in a
    // retry loop below. The surface is now a real WM-managed X window (see
    // eink::fb): on every exit path the window is torn down on Drop and the
    // lab126 compositor recomposites the screen (home library + status bar
    // repaint) — no cvm freeze, no chrome poking. The old `Pillow` guard is
    // gone for that reason.
    let mut renderer = TextRenderer::load(FONT_PX)?;

    let orient = orientation::Orientation::detect();
    log(format!("orientation: {orient:?}"));

    // The X server auto-rotates our window to the framework orientation, so the
    // surface renders identity. Only the raw evdev touch/buttons need orienting
    // (done below + re-applied on rotation via InputEvent::Tick).
    let mut fb = Framebuffer::open()?;
    let touch = Touch::open(orient, fb.var.xres, fb.var.yres)?;
    // Bezel page-turn buttons are a separate evdev device (gpio-keys). Grab
    // them so the stock framework stops repainting the library over our
    // gallery on a press, and map them to prev/next via the input multiplexer.
    // Grabbing them here (before list_books) also shields the Diagnostics
    // screen from that same repaint-on-press corruption (#7).
    // Best-effort: a missing device or open failure just means touch-only
    // navigation — never fail the picker over the buttons.
    let buttons = match Buttons::open() {
        Ok(Some(b)) => {
            log("buttons: grabbed gpio-keys");
            Some(b)
        }
        Ok(None) => {
            log("buttons: no gpio-keys device — touch-only");
            None
        }
        Err(e) => {
            log(format!("buttons: open failed: {e:#} — touch-only"));
            None
        }
    };
    let mut input = Input::new(touch, buttons);
    // Sync the initial orientation onto both devices (Buttons default to Up).
    // `current_orient` tracks it so InputEvent::Tick can detect a later flip.
    input.set_orientation(orient);
    let mut current_orient = orient;

    // Fetch the library, retrying through the Diagnostics screen on
    // failure. The old behavior here was draw_boot_toast + return (KUAL
    // flashes back, no recourse). Now a failure renders diag::run, which
    // blocks on a Retry/Exit tap: Retry re-runs list_books (server may now
    // be up), Exit returns cleanly (window torn down on drop → WM recomposites
    // the home + status bar). diag::run is
    // called fresh each failed attempt, so its "Last" row tracks the
    // latest error across retries.
    let t0 = Instant::now();
    let books = loop {
        match api::list_books(&agent, &cfg) {
            Ok(b) => break b,
            Err(err) => {
                log(format!("list_books failed: {err}"));
                match diag::run(&mut fb, &mut input, &mut renderer, &cfg, &err)? {
                    diag::Action::Retry => continue,
                    diag::Action::Exit => return Ok(()),
                }
            }
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
    let all_books: Vec<api::Book> = books
        .into_iter()
        .filter(|b| match b.kfx_sha256.as_deref() {
            Some(sha) if sha.len() >= 8 => !downloaded.contains(&sha[..8]),
            _ => true,
        })
        .collect();

    // `all_books` is the master (hide-downloaded) set; `books` is the view the
    // grid actually pages over — `all_books` sorted (and, in phase 2, filtered).
    // A re-sort rebuilds the view from the master, and `total_pages`/`covers`/
    // `page` all re-derive from it (see the `PagerHit::Sort` handler below).
    // Default sort is Date-added-desc: the same order the server already returns
    // (`ORDER BY imported_at DESC`), but now labelled in the grid header instead
    // of reading as random. (Phase 3 will seed `sort` from a persisted file.)
    let mut sort = SortState::default();
    let mut filters = Filters::default();
    let mut books = rebuild_view(&all_books, &filters, sort);

    let mut total_pages = pager::n_pages(books.len());
    log(format!(
        "books: {} of {} ({} on device, {} pages, list in {:?})",
        books.len(),
        total_from_server,
        downloaded.len(),
        total_pages,
        t0.elapsed()
    ));

    // Lazy cover fetch: start with all None, populate per-page as the
    // user navigates. Initial paint shows placeholders + titles
    // immediately so the picker never blanks during boot; each cover
    // arrives via a per-cell GC16 partial refresh from
    // fetch_and_paint_page. Cached across page revisits, so paging
    // back is instant.
    let mut covers: Vec<Option<DynamicImage>> = vec![None; books.len()];

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
        sort,
        filters.active_facets(),
    )?;
    log("initial render (placeholders)");
    fetch_and_paint_page(
        &agent, &cfg, cache_dir, &mut fb, &books, &mut covers, page, grid_left, grid_top,
    )?;

    let mut armed: Option<Armed> = None;
    loop {
        let event = input.next()?;

        match event {
            InputEvent::Touch(TouchEvent::Down { x, y }) => {
                log(format!("down: ({x},{y})"));
                // Down on a cell arms it. Down on the strip or in margins
                // is a no-op — strip actions fire on Up regardless of
                // hold time, so they don't need to arm.
                let visible_count = books
                    .len()
                    .saturating_sub(page * PAGE_SIZE)
                    .min(PAGE_SIZE);
                if let Some(cell_idx) =
                    grid::cell_at_tap(x, y, grid_left, grid_top, visible_count)
                {
                    let book_idx = page * PAGE_SIZE + cell_idx;
                    let (cx, cy) = grid::cell_xy(grid_left, grid_top, cell_idx);
                    if cx >= 0 && cy >= 0 {
                        grid::outline_cell(&mut fb, cx, cy, true);
                        fb.send_update(
                            MxcfbRect {
                                top: cy as u32,
                                left: cx as u32,
                                width: grid::CELL_W,
                                height: grid::CELL_H,
                            },
                            WAVEFORM_MODE_DU,
                        )?;
                        armed = Some(Armed {
                            book_idx,
                            down_at: Instant::now(),
                        });
                        log(format!(
                            "armed cell {} (book {}: {})",
                            cell_idx,
                            books[book_idx].id,
                            books[book_idx].title,
                        ));
                    }
                }
            }
            InputEvent::Touch(TouchEvent::Up { x, y }) => {
                log(format!("up: ({x},{y})"));
                if let Some(a) = armed.take() {
                    let held = a.down_at.elapsed();
                    if held >= LONG_PRESS_THRESHOLD {
                        // Long press → download. No need to clear the
                        // outline first — the download toast paints
                        // over the gallery, and the post-download
                        // redraw paints the clean cell.
                        let book = &books[a.book_idx];
                        log(format!(
                            "long press fired ({held:?}) on book {}: {}",
                            book.id, book.title,
                        ));
                        let msg =
                            format!("Downloading {}…", truncate_title(&book.title, 40));
                        let dirty = toast::draw(&mut fb, &mut renderer, &msg);
                        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;

                        let dl_t0 = Instant::now();
                        let banner_msg = match api::download_book(&agent, &cfg, book) {
                            Ok(d) => match persist(&d.filename, &d.bytes) {
                                Ok(saved) => {
                                    log(format!(
                                        "downloaded {} bytes to {} in {:?}",
                                        d.bytes.len(),
                                        saved.display(),
                                        dl_t0.elapsed()
                                    ));
                                    let _ =
                                        Command::new("touch").arg(CLEANINDEX).output();
                                    "Downloaded → Library will refresh shortly"
                                        .to_string()
                                }
                                Err(err) => {
                                    log(format!("persist failed: {err:#}"));
                                    format!("Failed: {err}")
                                }
                            },
                            Err(api::SidleError::TokenMismatch) => {
                                log(
                                    "token rejected during download — resync via sidle desktop app"
                                        .to_string(),
                                );
                                "Token mismatch.\nPlug Kindle into sidle and click Update KUAL."
                                    .to_string()
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
                            &mut fb, &mut renderer, &books, &covers, page,
                            total_pages, grid_left, grid_top, sort, filters.active_facets(),
                        )?;
                    } else {
                        // Short tap on a cover — discovery hint. Without
                        // this, a tap-trained user keeps tapping and
                        // wondering why nothing happens.
                        log(format!("short tap ({held:?}), showing hint"));
                        let dirty = toast::draw(
                            &mut fb,
                            &mut renderer,
                            "Hold cover to download",
                        );
                        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                        thread::sleep(TOAST_LINGER);
                        // Redraw the page to clear both the toast and
                        // the cell outline that Down left behind.
                        draw_gallery_page(
                            &mut fb, &mut renderer, &books, &covers, page,
                            total_pages, grid_left, grid_top, sort, filters.active_facets(),
                        )?;
                    }
                    continue;
                }

                // No armed cell — Up on the strip means a strip action.
                // Off-cell-off-strip is ignored.
                if let Some(hit) = pager::hit(x, y, fb.var.xres, fb.var.yres, total_pages) {
                    match hit {
                        PagerHit::Exit => {
                            log("exit-button tap");
                            break;
                        }
                        PagerHit::Filter => {
                            log("filter-button tap");
                            // Blocking overlay (filter menu → value pickers / sort
                            // picker). Mutates `filters`/`sort` in place and keeps
                            // `current_orient` in sync. Snapshot to detect whether
                            // the view actually needs rebuilding.
                            let before_filters = filters.clone();
                            let before_sort = sort;
                            filtermenu::run(
                                &mut fb, &mut input, &mut renderer, &all_books,
                                &mut filters, &mut sort, &mut current_orient,
                            )?;
                            if filters != before_filters || sort != before_sort {
                                // Rebuild the view from the master, reset paging,
                                // and drop the positional cover vec — it re-fills
                                // from the id-keyed disk cache on the paint below
                                // (no re-fetch). See `rebuild_view` / cover_cache.
                                books = rebuild_view(&all_books, &filters, sort);
                                total_pages = pager::n_pages(books.len());
                                covers = vec![None; books.len()];
                                page = 0;
                                log(format!(
                                    "view rebuilt: {} of {} books, {total_pages} pages, {}",
                                    books.len(),
                                    all_books.len(),
                                    sort.header(),
                                ));
                            }
                            // Repaint regardless — the overlay painted over the grid.
                            draw_gallery_page(
                                &mut fb, &mut renderer, &books, &covers, page,
                                total_pages, grid_left, grid_top, sort, filters.active_facets(),
                            )?;
                            fetch_and_paint_page(
                                &agent, &cfg, cache_dir, &mut fb, &books,
                                &mut covers, page, grid_left, grid_top,
                            )?;
                        }
                        PagerHit::Prev => {
                            let new_page = page.saturating_sub(1);
                            if new_page != page {
                                page = new_page;
                                draw_gallery_page(
                                    &mut fb, &mut renderer, &books, &covers, page,
                                    total_pages, grid_left, grid_top, sort, filters.active_facets(),
                                )?;
                                fetch_and_paint_page(
                                    &agent, &cfg, cache_dir, &mut fb, &books,
                                    &mut covers, page, grid_left, grid_top,
                                )?;
                            }
                        }
                        PagerHit::Next => {
                            let new_page = (page + 1).min(total_pages.saturating_sub(1));
                            if new_page != page {
                                page = new_page;
                                draw_gallery_page(
                                    &mut fb, &mut renderer, &books, &covers, page,
                                    total_pages, grid_left, grid_top, sort, filters.active_facets(),
                                )?;
                                fetch_and_paint_page(
                                    &agent, &cfg, cache_dir, &mut fb, &books,
                                    &mut covers, page, grid_left, grid_top,
                                )?;
                            }
                        }
                    }
                }
            }
            InputEvent::Page(pb) => {
                log(format!("page button: {pb:?}"));
                // A hardware page-turn cancels any in-progress long-press: the
                // finger may still be down on a now-stale cell, so drop the
                // armed state (the redraw below clears its outline) and a later
                // finger-up won't fire a download on the wrong page.
                armed = None;
                let new_page = match pb {
                    PageButton::Prev => page.saturating_sub(1),
                    PageButton::Next => (page + 1).min(total_pages.saturating_sub(1)),
                };
                if new_page != page {
                    page = new_page;
                    draw_gallery_page(
                        &mut fb, &mut renderer, &books, &covers, page,
                        total_pages, grid_left, grid_top, sort, filters.active_facets(),
                    )?;
                    fetch_and_paint_page(
                        &agent, &cfg, cache_dir, &mut fb, &books,
                        &mut covers, page, grid_left, grid_top,
                    )?;
                }
            }
            InputEvent::Tick => {
                // Idle poll. The X server rotates our window to the framework
                // orientation but leaves it blank until we repaint, and raw
                // touch/buttons don't follow the rotation. So on a detected
                // flip: re-orient input, then repaint the current page (the X
                // server rotates the repaint correctly, clearing the blank).
                // Skip while a cell is armed so we don't disrupt a long-press.
                if armed.is_none() {
                    let o = orientation::Orientation::detect();
                    if o != current_orient {
                        log(format!("orientation: {current_orient:?} -> {o:?}"));
                        current_orient = o;
                        input.set_orientation(o);
                        draw_gallery_page(
                            &mut fb, &mut renderer, &books, &covers, page,
                            total_pages, grid_left, grid_top, sort, filters.active_facets(),
                        )?;
                        fetch_and_paint_page(
                            &agent, &cfg, cache_dir, &mut fb, &books,
                            &mut covers, page, grid_left, grid_top,
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Build the view the grid pages over: the master `all_books` filtered by the
/// active facets, then sorted. Cloning a few hundred small `Book` structs per
/// rebuild is trivial and keeps the `draw_gallery_page` / `fetch_and_paint_page`
/// signatures (`&[Book]`) unchanged versus threading index lists through them.
fn rebuild_view(all_books: &[api::Book], filters: &Filters, sort: SortState) -> Vec<api::Book> {
    let mut view: Vec<api::Book> = all_books
        .iter()
        .filter(|b| filter::matches(b, filters, None))
        .cloned()
        .collect();
    sort.apply(&mut view);
    view
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
    sort: SortState,
    filter_count: usize,
) -> anyhow::Result<()> {
    fb.fill_rect(0, 0, fb.var.xres, fb.var.yres, 0xFF);

    // Header line in the top margin (above the grid): name the sort order so it
    // isn't a mystery — the default (newest import first) otherwise reads as a
    // random shuffle, especially with the hide-downloaded gaps.
    let header = format!("Sorted by {}", sort.header());
    let hw = renderer.measure_width(&header);
    let hx = ((fb.var.xres as i32 - hw as i32) / 2).max(0);
    let hbaseline = grid_top * 60 / 100;
    renderer.draw(fb, hx, hbaseline, &header, false);

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
                draw_placeholder_title(
                    fb,
                    renderer,
                    cx,
                    cy,
                    &books[book_idx].title,
                );
            }
        }
    }
    // Strip is the only path to Exit — always draw, even on a single
    // page. `pager::draw` internally returns early after Exit when
    // total_pages <= 1, so no prev/next labels are shown then.
    pager::draw(fb, renderer, page, total_pages, filter_count);
    fb.send_update(
        MxcfbRect { top: 0, left: 0, width: fb.var.xres, height: fb.var.yres },
        WAVEFORM_MODE_GC16,
    )?;
    Ok(())
}

/// Render the book title centered inside a placeholder cell, wrapped
/// to the cell's interior width and truncated with `…` on the last
/// visible line if it overflows vertically. Used when the cover
/// hasn't arrived (or failed to decode) so the user still sees what
/// the cell *is*.
fn draw_placeholder_title(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    cx: i32,
    cy: i32,
    title: &str,
) {
    // Symmetric padding inside the cell so the text doesn't kiss the
    // edges; matches typical print-tile margins.
    const PAD: u32 = 16;
    let max_text_w = grid::CELL_W.saturating_sub(PAD * 2);
    let max_text_h = grid::CELL_H.saturating_sub(PAD * 2);
    let line_h = renderer.line_height().max(1);
    let max_lines = (max_text_h / line_h).max(1) as usize;

    let lines = renderer.wrap_and_clamp(title, max_text_w, max_lines);

    // Center the block vertically.
    let total_h = (lines.len() as u32) * line_h;
    let start_y = cy + ((grid::CELL_H.saturating_sub(total_h)) / 2) as i32;

    for (i, line) in lines.iter().enumerate() {
        let line_w = renderer.measure_width(line);
        let line_x = cx + ((grid::CELL_W.saturating_sub(line_w)) / 2) as i32;
        // Baseline ≈ 80% down each line box (above descender, below
        // cap height) — matches the existing `pager` baseline ratio.
        let baseline = start_y + ((i as u32) * line_h + line_h * 80 / 100) as i32;
        renderer.draw(fb, line_x, baseline, line, false);
    }
}

/// Populate `covers[start..end]` for the given page by HTTP-fetching
/// any cells whose slot is still `None`, painting each into its cell
/// with a GC16 partial refresh as it arrives. Already-loaded cells
/// (page revisits) are skipped — paint is no-op since the cached cover
/// is already on screen from the preceding `draw_gallery_page` call.
fn fetch_and_paint_page(
    agent: &ureq::Agent,
    cfg: &config::ServerConfig,
    cache_dir: &Path,
    fb: &mut Framebuffer,
    books: &[api::Book],
    covers: &mut [Option<DynamicImage>],
    page: usize,
    grid_left: i32,
    grid_top: i32,
) -> anyhow::Result<()> {
    let start = page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(books.len());
    let t_pg = Instant::now();
    let mut fetched = 0usize;
    for book_idx in start..end {
        if covers[book_idx].is_some() {
            continue;
        }
        let book = &books[book_idx];

        // Disk cache first (instant, no network); on a miss, fetch over the LAN
        // and write through so the next launch is a hit. Timing is split into
        // get (cache-read or network) vs decode so a hardware log tells us
        // where the per-cover cost actually lands — the whole point of the
        // thumbnail change was to shrink both.
        let t_get = Instant::now();
        let (bytes, source) = match cover_cache::load(cache_dir, book.id) {
            Some(b) => (Some(b), "cache"),
            None => match api::fetch_cover(agent, cfg, book.id) {
                Ok(b) => {
                    if let Err(e) = cover_cache::store(cache_dir, book.id, &b) {
                        log(format!("cover {}: cache store failed: {e}", book.id));
                    }
                    (Some(b), "net")
                }
                Err(err) => {
                    log(format!("cover {}: {err}", book.id));
                    (None, "net")
                }
            },
        };
        let get_ms = t_get.elapsed();

        let img = match bytes {
            Some(b) => {
                let t_dec = Instant::now();
                match grid::decode_resize(&b) {
                    Ok(img) => {
                        log(format!(
                            "cover {} {} ({}B) get={:?} decode={:?}",
                            book.id,
                            source,
                            b.len(),
                            get_ms,
                            t_dec.elapsed()
                        ));
                        Some(img)
                    }
                    Err(err) => {
                        log(format!("cover {}: decode {err}", book.id));
                        None
                    }
                }
            }
            None => None,
        };

        if let Some(img) = img.as_ref() {
            let cell_idx = book_idx - start;
            let (cx, cy) = grid::cell_xy(grid_left, grid_top, cell_idx);
            if cx >= 0 && cy >= 0 {
                grid::blit_cell(fb, cx, cy, img);
                fb.send_update(
                    MxcfbRect {
                        top: cy as u32,
                        left: cx as u32,
                        width: grid::CELL_W,
                        height: grid::CELL_H,
                    },
                    WAVEFORM_MODE_GC16,
                )?;
            }
        }
        covers[book_idx] = img;
        fetched += 1;
    }
    if fetched > 0 {
        log(format!(
            "page {} filled {} covers in {:?}",
            page,
            fetched,
            t_pg.elapsed()
        ));
    }
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
