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
mod collate;
mod config;
mod cover_cache;
mod device_state;
mod eink;
mod orientation;
mod selfupdate;
mod series;
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
use series::{Cell, CellKind};

const LOG_PATH: &str = "/mnt/us/sidle-native.log";
/// Dedicated log for the "Update over Wi-Fi" flow (`--update`), so its trail
/// isn't interleaved with the gallery's `LOG_PATH`. Written by `update_log` +
/// `bin/update.sh` (which also redirects the binary's stderr here).
const UPDATE_LOG_PATH: &str = "/mnt/us/sidle-update.log";
const CONFIG_PATH: &str = "/mnt/us/extensions/sidle/etc/server.conf";
/// On-device KUAL bundle root. `--update` stages its pulled binary under here as
/// `bin/sidle.new` (manifest names are relative to this dir), and the launcher
/// swaps it in. Parent of [`CONFIG_PATH`]'s `etc/`.
const BUNDLE_DIR: &str = "/mnt/us/extensions/sidle";
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

/// Cell currently outlined and awaiting release. On a book cell, release
/// decides between a long-enough hold (download) and a too-short tap (discovery
/// hint); on a series cell, any release drills in (collections aren't
/// downloadable, only navigable — so hold time is irrelevant there).
struct Armed {
    /// Index into the current `cells` view (top-level entries when at the
    /// grouped top level, or drilled-in members) of the outlined tile.
    cell_idx: usize,
    down_at: Instant,
}

fn main() {
    // `--version`/`-V`: print the compiled version and exit. Cheap — no device
    // setup, no framebuffer. `bin/update.sh` calls this on its launch line to
    // record which picker version is actually on the Kindle: the binary is the
    // only source that stays accurate after a Wi-Fi self-update (that swaps the
    // binary but not config.xml). Inherits the workspace version through
    // `version.workspace = true`.
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("sidle {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // X11-window proof-of-concept (see eink::x11poc): validates that a
    // Sidle-created window is WM-managed + recomposited on teardown before we
    // port the renderer off raw /dev/fb0. Bypasses all fb/config setup.
    if std::env::args().any(|a| a == "--x11-poc") {
        let r = eink::x11poc::run(log);
        log(format!("x11poc done: {r:?}"));
        return;
    }
    // "Update over Wi-Fi": a dedicated KUAL menu entry runs `bin/sidle.sh
    // --update`. A separate, focused launch — so it doubles as a recovery path
    // when the gallery is crashing in its list/grid logic — that pulls the
    // picker's own next binary from sidle-server and stages it for the launcher.
    if std::env::args().any(|a| a == "--update") {
        let result = run_update();
        update_log(format!("--update done: {result:?}"));
        return;
    }
    let result = run();
    log(format!("done: {result:?}"));
}

/// Paint a clean centered banner panel: white-fill the screen, draw `message`,
/// then one full-screen GC16 refresh so it lands without DU ghosting from
/// whatever the framework left on screen. Used for both the `--update` progress
/// and result toasts (mirrors `diag`'s clean-panel approach).
fn draw_panel(fb: &mut Framebuffer, renderer: &mut TextRenderer, message: &str) -> anyhow::Result<()> {
    fb.fill_rect(0, 0, fb.var.xres, fb.var.yres, 0xFF);
    let _ = toast::draw(fb, renderer, message);
    fb.send_update(
        MxcfbRect { top: 0, left: 0, width: fb.var.xres, height: fb.var.yres },
        WAVEFORM_MODE_GC16,
    )?;
    Ok(())
}

/// `--update` mode: pull a staged self-update from sidle-server over the LAN and
/// stage it as `bin/sidle.new` for the launcher to swap in on next start. Shares
/// the HTTP + config code with the gallery (`api`/`config`/`selfupdate`) and
/// reuses the gallery's device setup (framebuffer + X11 window + renderer +
/// input) to show a result toast and block on a tap to exit — the same shape as
/// `diag::run`. Not a recovery path for a crash in this device setup itself: a
/// graphics-init failure ends `--update`; only the network/list/grid failures
/// it's launched separately from are dodged.
fn run_update() -> anyhow::Result<()> {
    update_log("=== Update over Wi-Fi: start ===");
    update_log(format!("argv: {:?}", std::env::args().collect::<Vec<_>>()));
    let cfg = config::load(Path::new(CONFIG_PATH))?;
    update_log(format!("server: http://{}:{}", cfg.host, cfg.port));
    let agent = ureq::AgentBuilder::new().build();

    let mut renderer = TextRenderer::load(FONT_PX)?;
    let orient = orientation::Orientation::detect();
    let mut fb = Framebuffer::open()?;
    let touch = Touch::open(orient, fb.var.xres, fb.var.yres)?;
    let buttons = Buttons::open().ok().flatten();
    let mut input = Input::new(touch, buttons);
    input.set_orientation(orient);

    // Progress banner — the manifest fetch + ~1.8 MB download can take a beat on
    // a sleepy radio.
    draw_panel(&mut fb, &mut renderer, "Checking for update…")?;

    // Step-level breadcrumbs go to the dedicated update log via the closure.
    let message = match selfupdate::run_pull(
        &agent,
        &cfg,
        Path::new(BUNDLE_DIR),
        selfupdate::self_build_ts(),
        |m| update_log(m),
    ) {
        Ok(selfupdate::UpdateOutcome::UpToDate) => "Already up to date".to_string(),
        Ok(selfupdate::UpdateOutcome::Staged(_)) => {
            "Update staged — relaunch to apply".to_string()
        }
        // The server's binary is older/equal — kept the newer one on the device.
        Ok(selfupdate::UpdateOutcome::RefusedOlder(_)) => {
            "Server build not newer — kept current".to_string()
        }
        // Reuse the gallery's token-mismatch breadcrumb verbatim (see `diag`).
        Err(api::SidleError::TokenMismatch) => {
            "Plug Kindle into sidle, click Update KUAL".to_string()
        }
        Err(e) => {
            update_log(format!("FAILED: {e}"));
            "Update failed — see log".to_string()
        }
    };
    update_log(format!("result: {message}"));

    // Result panel, then block until a tap or page button. On return the window
    // tears down and the framework recomposites the home screen (every exit
    // path's behavior).
    draw_panel(&mut fb, &mut renderer, &message)?;
    loop {
        match input.next()? {
            InputEvent::Touch(TouchEvent::Up { .. }) => break,
            InputEvent::Touch(TouchEvent::Screenshot) => {
                let _ = eink::screenshot::capture(&mut fb);
            }
            InputEvent::Page(_) => break,
            _ => {}
        }
    }
    Ok(())
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

    // `all_books` is the master (hide-downloaded) set. The picker is **grouped
    // by series, always** (no flat toggle — see series-grouping.md): the master
    // is filtered+sorted by `rebuild_view`, then folded into `entries` (series
    // collections + standalone books) at each series' first-seen position so the
    // active sort drives tile order for free. `cells` is what the grid pages over
    // — the top-level entries, or a drilled-in series' members. `series_view` is
    // the ephemeral drill-in target (None = top level); nothing is persisted, so
    // a drill-in resets each launch. A re-sort/filter rebuilds `entries` from the
    // master and `total_pages`/`covers`/`page` re-derive (see `PagerHit::Filter`).
    // Default sort is Date-added-desc — the order the server already returns
    // (`ORDER BY imported_at DESC`), now labelled in the grid header.
    let mut sort = SortState::default();
    let mut filters = Filters::default();
    let mut entries = series::group_by_series(rebuild_view(&all_books, &filters, sort));
    let mut series_view: Option<String> = None;
    let mut cells = series::cells_for_top(&entries);

    let mut total_pages = pager::n_pages(cells.len());
    log(format!(
        "books: {} in {} tiles of {} ({} on device, {} pages, list in {:?})",
        all_books.len(),
        cells.len(),
        total_from_server,
        downloaded.len(),
        total_pages,
        t0.elapsed()
    ));

    // Lazy cover fetch: start with all None, populate per-page as the
    // user navigates. `covers` is parallel to `cells` (a series cell's cover is
    // its lead member). Initial paint shows placeholders + titles immediately so
    // the picker never blanks during boot; each cover arrives via a per-cell
    // GC16 partial refresh from `fetch_and_paint_page`, cached on disk so paging
    // back and re-grouping are instant.
    let mut covers: Vec<Option<DynamicImage>> = vec![None; cells.len()];

    let (grid_left, grid_top) = grid::grid_origin(fb.var.xres, TOP_MARGIN);
    let mut page: usize = 0;
    log("initial render (placeholders)");
    repaint_page(
        &mut fb, &mut renderer, &agent, &cfg, cache_dir, &cells, &mut covers, page,
        total_pages, grid_left, grid_top, sort, filters.active_facets(), series_view.as_deref(),
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
                let visible_count = cells
                    .len()
                    .saturating_sub(page * PAGE_SIZE)
                    .min(PAGE_SIZE);
                if let Some(cell_pos) =
                    grid::cell_at_tap(x, y, grid_left, grid_top, visible_count)
                {
                    let cell_idx = page * PAGE_SIZE + cell_pos;
                    let (cx, cy) = grid::cell_xy(grid_left, grid_top, cell_pos);
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
                            cell_idx,
                            down_at: Instant::now(),
                        });
                        match &cells[cell_idx].kind {
                            CellKind::Series { name, count } => log(format!(
                                "armed cell {cell_pos} (series {name}, {count} books)"
                            )),
                            CellKind::Book => log(format!(
                                "armed cell {cell_pos} (book {}: {})",
                                cells[cell_idx].cover_book.id, cells[cell_idx].cover_book.title,
                            )),
                        }
                    }
                }
            }
            InputEvent::Touch(TouchEvent::Up { x, y }) => {
                log(format!("up: ({x},{y})"));
                if let Some(a) = armed.take() {
                    // Resolve the armed tile to an owned decision *before* acting:
                    // a `match &cells[..]` borrow would otherwise still be live when
                    // a drill-in reassigns `cells`. Series → drill (hold time
                    // irrelevant, collections aren't downloadable); book → the
                    // existing long-press-vs-tap split below.
                    let drill_target = match &cells[a.cell_idx].kind {
                        CellKind::Series { name, .. } => Some(name.clone()),
                        CellKind::Book => None,
                    };
                    if let Some(name) = drill_target {
                        log(format!("drill into series: {name}"));
                        // The series we just tapped is in `entries` by construction,
                        // so `members_of` is Some; the `if let` is defensive.
                        if let Some(members) = series::members_of(&entries, &name) {
                            cells = series::cells_for_series(members);
                            total_pages = pager::n_pages(cells.len());
                            covers = vec![None; cells.len()];
                            page = 0;
                            series_view = Some(name);
                            repaint_page(
                                &mut fb, &mut renderer, &agent, &cfg, cache_dir, &cells,
                                &mut covers, page, total_pages, grid_left, grid_top, sort,
                                filters.active_facets(), series_view.as_deref(),
                            )?;
                        }
                        continue;
                    }
                    let held = a.down_at.elapsed();
                    if held >= LONG_PRESS_THRESHOLD {
                        // Long press → download. No need to clear the
                        // outline first — the download toast paints
                        // over the gallery, and the post-download
                        // repaint paints the clean cell.
                        let book = &cells[a.cell_idx].cover_book;
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
                                    "token rejected during download — resync via sidle desktop app",
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
                        repaint_page(
                            &mut fb, &mut renderer, &agent, &cfg, cache_dir, &cells,
                            &mut covers, page, total_pages, grid_left, grid_top, sort,
                            filters.active_facets(), series_view.as_deref(),
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
                        // Repaint to clear both the toast and the cell
                        // outline that Down left behind.
                        repaint_page(
                            &mut fb, &mut renderer, &agent, &cfg, cache_dir, &cells,
                            &mut covers, page, total_pages, grid_left, grid_top, sort,
                            filters.active_facets(), series_view.as_deref(),
                        )?;
                    }
                    continue;
                }

                // No armed cell — Up on the strip means a strip action.
                // Off-cell-off-strip is ignored. `drilled` swaps the Filter slot
                // for Back (see pager::hit).
                if let Some(hit) =
                    pager::hit(x, y, fb.var.xres, fb.var.yres, total_pages, series_view.is_some())
                {
                    match hit {
                        PagerHit::Exit => {
                            log("exit-button tap");
                            break;
                        }
                        PagerHit::Filter => {
                            log("filter-button tap");
                            // Blocking overlay (filter menu → value pickers / sort
                            // picker). Mutates `filters`/`sort` in place and keeps
                            // `current_orient` in sync. Only reachable at the top
                            // level (drilled in, this slot is Back), so the rebuild
                            // always re-folds from the top. Snapshot to detect
                            // whether the view actually needs rebuilding.
                            let before_filters = filters.clone();
                            let before_sort = sort;
                            filtermenu::run(
                                &mut fb, &mut input, &mut renderer, &all_books,
                                &mut filters, &mut sort, &mut current_orient,
                            )?;
                            if filters != before_filters || sort != before_sort {
                                // Re-filter+sort the master, re-fold into series
                                // collections, reset paging, and drop the positional
                                // cover vec — it re-fills from the id-keyed disk
                                // cache on the paint below (no re-fetch).
                                entries = series::group_by_series(
                                    rebuild_view(&all_books, &filters, sort),
                                );
                                cells = series::cells_for_top(&entries);
                                total_pages = pager::n_pages(cells.len());
                                covers = vec![None; cells.len()];
                                page = 0;
                                log(format!(
                                    "view rebuilt: {} tiles from {} books, {total_pages} pages, {}",
                                    cells.len(),
                                    all_books.len(),
                                    sort.header(),
                                ));
                            }
                            // Repaint regardless — the overlay painted over the grid.
                            repaint_page(
                                &mut fb, &mut renderer, &agent, &cfg, cache_dir, &cells,
                                &mut covers, page, total_pages, grid_left, grid_top, sort,
                                filters.active_facets(), series_view.as_deref(),
                            )?;
                        }
                        PagerHit::Back => {
                            // Pop the drill-in back to the grouped top level. Same
                            // strip slot as Filter, swapped in while drilled.
                            log("back to series top level");
                            series_view = None;
                            cells = series::cells_for_top(&entries);
                            total_pages = pager::n_pages(cells.len());
                            covers = vec![None; cells.len()];
                            page = 0;
                            repaint_page(
                                &mut fb, &mut renderer, &agent, &cfg, cache_dir, &cells,
                                &mut covers, page, total_pages, grid_left, grid_top, sort,
                                filters.active_facets(), series_view.as_deref(),
                            )?;
                        }
                        PagerHit::Prev => {
                            let new_page = page.saturating_sub(1);
                            if new_page != page {
                                page = new_page;
                                repaint_page(
                                    &mut fb, &mut renderer, &agent, &cfg, cache_dir, &cells,
                                    &mut covers, page, total_pages, grid_left, grid_top, sort,
                                    filters.active_facets(), series_view.as_deref(),
                                )?;
                            }
                        }
                        PagerHit::Next => {
                            let new_page = (page + 1).min(total_pages.saturating_sub(1));
                            if new_page != page {
                                page = new_page;
                                repaint_page(
                                    &mut fb, &mut renderer, &agent, &cfg, cache_dir, &cells,
                                    &mut covers, page, total_pages, grid_left, grid_top, sort,
                                    filters.active_facets(), series_view.as_deref(),
                                )?;
                            }
                        }
                        PagerHit::Sync => {
                            // Push this device's reading-state sidecars to the Mac
                            // — the LAN twin of a USB annotation sync. The grid
                            // doesn't change, so toast the report and repaint the
                            // page underneath (in whichever view is showing).
                            log("sync-button tap");
                            let dirty =
                                toast::draw(&mut fb, &mut renderer, "Syncing annotations…");
                            fb.send_update(dirty, WAVEFORM_MODE_GC16)?;

                            let sync_t0 = Instant::now();
                            let banner_msg = match api::push_annotations(
                                &agent,
                                &cfg,
                                std::path::Path::new(DOWNLOAD_DIR),
                            ) {
                                Ok(report) => {
                                    let summary = report.summary();
                                    log(format!(
                                        "annotation sync ok in {:?}: {summary}",
                                        sync_t0.elapsed()
                                    ));
                                    summary
                                }
                                Err(api::SidleError::TokenMismatch) => {
                                    log("token rejected during sync — resync via sidle desktop app");
                                    "Token mismatch.\nPlug Kindle into sidle and click Update KUAL."
                                        .to_string()
                                }
                                Err(api::SidleError::Other(err)) => {
                                    log(format!("annotation sync failed: {err:#}"));
                                    format!("Sync failed: {err}")
                                }
                            };
                            let dirty = toast::draw(&mut fb, &mut renderer, &banner_msg);
                            fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                            thread::sleep(TOAST_LINGER);
                            repaint_page(
                                &mut fb, &mut renderer, &agent, &cfg, cache_dir, &cells,
                                &mut covers, page, total_pages, grid_left, grid_top, sort,
                                filters.active_facets(), series_view.as_deref(),
                            )?;
                        }
                    }
                }
            }
            InputEvent::Touch(TouchEvent::Screenshot) => {
                // Two-corner gesture. Cancel any cell the first finger armed so
                // its lift can't fire a download, then capture + flash.
                armed = None;
                match eink::screenshot::capture(&mut fb) {
                    Ok(p) => log(format!("screenshot saved: {}", p.display())),
                    Err(e) => log(format!("screenshot failed: {e:#}")),
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
                    repaint_page(
                        &mut fb, &mut renderer, &agent, &cfg, cache_dir, &cells,
                        &mut covers, page, total_pages, grid_left, grid_top, sort,
                        filters.active_facets(), series_view.as_deref(),
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
                        repaint_page(
                            &mut fb, &mut renderer, &agent, &cfg, cache_dir, &cells,
                            &mut covers, page, total_pages, grid_left, grid_top, sort,
                            filters.active_facets(), series_view.as_deref(),
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// The filtered+sorted master view, ready to fold into series collections
/// (`series::group_by_series`): `all_books` filtered by the active facets, then
/// sorted. Cloning a few hundred small `Book` structs per rebuild is trivial.
fn rebuild_view(all_books: &[api::Book], filters: &Filters, sort: SortState) -> Vec<api::Book> {
    let mut view: Vec<api::Book> = all_books
        .iter()
        .filter(|b| filter::matches(b, filters, None))
        .cloned()
        .collect();
    sort.apply(&mut view);
    view
}

/// Draw one page of `cells` with placeholders, the header, and the bottom
/// strip, then one full GC16 refresh. Series cells get the collection art
/// (`grid::draw_series_cell`); book cells the cover-or-title-placeholder.
/// `header` is the precomputed top-margin line (series name when drilled in,
/// else the sort order); `drilled` swaps the strip's Filter slot for Back.
#[allow(clippy::too_many_arguments)]
fn draw_gallery_page(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    cells: &[Cell],
    covers: &[Option<DynamicImage>],
    page: usize,
    total_pages: usize,
    grid_left: i32,
    grid_top: i32,
    header: &str,
    filter_count: usize,
    drilled: bool,
) -> anyhow::Result<()> {
    fb.fill_rect(0, 0, fb.var.xres, fb.var.yres, 0xFF);

    // Header line in the top margin: the sort order (top level) or the drilled-in
    // series name. Clamp to one line so a long series name can't overrun the panel.
    let hlines = renderer.wrap_and_clamp(header, fb.var.xres.saturating_sub(80), 1);
    if let Some(h) = hlines.first() {
        let hw = renderer.measure_width(h);
        let hx = ((fb.var.xres as i32 - hw as i32) / 2).max(0);
        let hbaseline = grid_top * 60 / 100;
        renderer.draw(fb, hx, hbaseline, h, false);
    }

    let start = page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(cells.len());
    for (cell_pos, idx) in (start..end).enumerate() {
        let (cx, cy) = grid::cell_xy(grid_left, grid_top, cell_pos);
        if cx < 0 || cy < 0 {
            continue;
        }
        match &cells[idx].kind {
            CellKind::Book => grid::draw_book_cell(
                fb,
                renderer,
                cx,
                cy,
                covers[idx].as_ref(),
                &cells[idx].cover_book.title,
            ),
            CellKind::Series { name, count } => {
                grid::draw_series_cell(fb, renderer, cx, cy, covers[idx].as_ref(), *count, name);
            }
        }
    }
    // Strip is the only path to Exit — always draw, even on a single
    // page. `pager::draw` internally returns early after Exit when
    // total_pages <= 1, so no prev/next labels are shown then.
    pager::draw(fb, renderer, page, total_pages, filter_count, drilled);
    fb.send_update(
        MxcfbRect { top: 0, left: 0, width: fb.var.xres, height: fb.var.yres },
        WAVEFORM_MODE_GC16,
    )?;
    Ok(())
}

/// Draw the current page and then lazily fill its covers — the draw+fetch pair
/// every navigation/redraw path runs. `series_view` (Some = drilled into that
/// series) decides the header and the Filter↔Back strip slot; the fetch is a
/// no-op when the page's covers are already loaded (e.g. a post-toast repaint),
/// so call sites use this uniformly.
#[allow(clippy::too_many_arguments)]
fn repaint_page(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    agent: &ureq::Agent,
    cfg: &config::ServerConfig,
    cache_dir: &Path,
    cells: &[Cell],
    covers: &mut [Option<DynamicImage>],
    page: usize,
    total_pages: usize,
    grid_left: i32,
    grid_top: i32,
    sort: SortState,
    filter_count: usize,
    series_view: Option<&str>,
) -> anyhow::Result<()> {
    let drilled = series_view.is_some();
    let header = match series_view {
        Some(name) => format!("{name}  ({})", cells.len()),
        None => format!("Sorted by {}", sort.header()),
    };
    draw_gallery_page(
        fb, renderer, cells, covers, page, total_pages, grid_left, grid_top, &header,
        filter_count, drilled,
    )?;
    fetch_and_paint_page(fb, renderer, agent, cfg, cache_dir, cells, covers, page, grid_left, grid_top)?;
    Ok(())
}

/// Populate `covers[start..end]` for the given page by fetching any cell whose
/// slot is still `None`, painting each into its cell with a GC16 partial refresh
/// as it arrives. Already-loaded cells (page revisits) are skipped — paint is a
/// no-op since the cached cover is already on screen from the `draw_gallery_page`
/// call. A series cell paints its full collection art; a book cell the cover.
#[allow(clippy::too_many_arguments)]
fn fetch_and_paint_page(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    agent: &ureq::Agent,
    cfg: &config::ServerConfig,
    cache_dir: &Path,
    cells: &[Cell],
    covers: &mut [Option<DynamicImage>],
    page: usize,
    grid_left: i32,
    grid_top: i32,
) -> anyhow::Result<()> {
    let start = page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(cells.len());
    let t_pg = Instant::now();
    let mut fetched = 0usize;
    for idx in start..end {
        if covers[idx].is_some() {
            continue;
        }
        // The cover source is the cell's own book (standalone) or its series'
        // lead member — one fetch per collection, not one per member.
        let img = load_cover(agent, cfg, cache_dir, &cells[idx].cover_book);

        if let Some(img) = img.as_ref() {
            let (cx, cy) = grid::cell_xy(grid_left, grid_top, idx - start);
            if cx >= 0 && cy >= 0 {
                match &cells[idx].kind {
                    CellKind::Book => grid::draw_book_cell(
                        fb,
                        renderer,
                        cx,
                        cy,
                        Some(img),
                        &cells[idx].cover_book.title,
                    ),
                    CellKind::Series { name, count } => {
                        grid::draw_series_cell(fb, renderer, cx, cy, Some(img), *count, name)
                    }
                }
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
        covers[idx] = img;
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

/// Load one book's cover into a decoded image: disk cache first (instant, no
/// network); on a miss, fetch over the LAN and write through so the next launch
/// is a hit. Returns `None` on a fetch or decode failure (cell stays a
/// placeholder). Timing is split into get (cache-read or network) vs decode so a
/// hardware log shows where the per-cover cost lands.
fn load_cover(
    agent: &ureq::Agent,
    cfg: &config::ServerConfig,
    cache_dir: &Path,
    book: &api::Book,
) -> Option<DynamicImage> {
    let t_get = Instant::now();
    let (bytes, source) = match cover_cache::load(cache_dir, book.id, book.cover_rev) {
        Some(b) => (Some(b), "cache"),
        None => match api::fetch_cover(agent, cfg, book.id) {
            Ok(b) => {
                if let Err(e) = cover_cache::store(cache_dir, book.id, book.cover_rev, &b) {
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

    let bytes = bytes?;
    let t_dec = Instant::now();
    match grid::decode_resize(&bytes) {
        Ok(img) => {
            log(format!(
                "cover {} {} ({}B) get={:?} decode={:?}",
                book.id,
                source,
                bytes.len(),
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
    // File only — NOT also stderr. `sidle.sh` runs the binary with `2>> "$LOG"`
    // (to capture panics), so writing to stderr here too would land a SECOND
    // copy of every line in the same file — the double-entry bug. Genuine stderr
    // (panics, library output) is still captured once via that redirect.
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Append a line to the dedicated "Update over Wi-Fi" log (`--update`), so the
/// self-update trail isn't buried in the gallery's `LOG_PATH`. File only (no
/// stderr echo) — `bin/update.sh` redirects the binary's stderr to this same
/// file, so panics still land without double-logging these explicit lines.
fn update_log(line: impl AsRef<str>) {
    let line = line.as_ref();
    let path = if std::path::Path::new("/mnt/us").is_dir() {
        UPDATE_LOG_PATH
    } else {
        "./sidle-update.log"
    };
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}
