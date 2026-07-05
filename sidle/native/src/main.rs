//! Sidle native — Milestone 9 paginated cover grid + download flow.
//!
//! 3×3 grid per page, prev/next bottom-strip controls when the library
//! overflows one page. Tap a cover → overlay "Downloading…" → stream
//! `.kfx` to `/mnt/us/documents/Sidle/<filename>` → `touch
//! /mnt/us/system/.cleanindex` → overlay "Downloaded" → restore gallery.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;

mod api;
mod collate;
mod config;
mod cover_cache;
mod device_state;
mod eink;
mod orientation;
mod search;
mod selfupdate;
mod series;
mod ui;
mod wrap;

use eink::buttons::{Buttons, PageButton};
use eink::fb::{Framebuffer, MxcfbRect, WAVEFORM_MODE_DU, WAVEFORM_MODE_GC16};
use eink::input::{Input, InputEvent};
use eink::touch::{SwipeDir, Touch, TouchEvent, classify_swipe};
use image::DynamicImage;
use series::{Cell, CellKind};
use ui::diag;
use ui::filter::{self, Filters};
use ui::filtermenu;
use ui::grid;
use ui::pager::{self, PAGE_SIZE, PagerHit};
use ui::searchbar;
use ui::sort::SortState;
use ui::text::TextRenderer;
use ui::toast;

const LOG_PATH: &str = "/mnt/us/sidle-native.log";
/// Dedicated log for the LAN self-update, so its trail isn't interleaved with
/// the gallery's `LOG_PATH`. Written by `update_log` from both the in-app
/// **Update** button (inline in `run`) and the `--update` recovery launch.
const UPDATE_LOG_PATH: &str = "/mnt/us/sidle-update.log";
const CONFIG_PATH: &str = "/mnt/us/extensions/sidle/etc/server.conf";
/// On-device KUAL bundle root. `--update` stages its pulled binary under here as
/// `bin/sidle.new` (manifest names are relative to this dir), and the launcher
/// swaps it in. Parent of [`CONFIG_PATH`]'s `etc/`.
const BUNDLE_DIR: &str = "/mnt/us/extensions/sidle";
const FONT_PX: f32 = 28.0;
/// Top margin above the grid. Holds the Amazon-style **search bar** (top level
/// only) plus the sort/results header line below it. Sized to seat both; the
/// grid origin derives from it (`grid::grid_origin`). On the KOA2 (1264×1680)
/// `190 + 3·440 + 2·20 + 80(strip) = 1630 < 1680` — the 3×3 grid still fits.
const TOP_MARGIN: u32 = 190;
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

/// Per-read socket timeout for the session agent. Bounds a genuinely stalled
/// socket (dead radio) without capping total transfer time, so a 300 MB+ book
/// over a slow Wi-Fi link still completes as long as bytes keep arriving. A
/// healthy transfer resets this on every chunk; only a true stall trips it.
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Read-buffer size for streaming a book to disk. Large enough to keep syscall
/// overhead negligible over a ~hundreds-of-MB transfer, small enough that the
/// Cancel poll between reads stays responsive on a healthy connection.
const DL_CHUNK: usize = 256 * 1024;
/// Minimum wall-clock between progress redraws. E-ink can't usefully repaint
/// faster, and throttling keeps the transfer (not the panel) the bottleneck.
const DL_REDRAW_INTERVAL: Duration = Duration::from_millis(700);

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
    // setup, no framebuffer. The binary is the only source of the on-device
    // picker version that stays accurate after a Wi-Fi self-update (that swaps
    // the binary but not config.xml), so anything logging which build is
    // installed shells out to this. Inherits the workspace version through
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
    // `--update`: the LAN self-update as a standalone launch. The everyday path
    // is the in-app **Update** button (inline in `run`); this flag is the
    // break-glass twin — invokable from a shell when the gallery itself won't
    // boot (a crash in its list/grid logic), since it does the same pull with
    // only the minimal device setup, dodging that failing code. No KUAL tile
    // points at it anymore (the button replaced the old "Update Sidle" entry).
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
fn draw_panel(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    message: &str,
) -> anyhow::Result<()> {
    fb.fill_rect(0, 0, fb.var.xres, fb.var.yres, 0xFF);
    let _ = toast::draw(fb, renderer, message);
    fb.send_update(
        MxcfbRect {
            top: 0,
            left: 0,
            width: fb.var.xres,
            height: fb.var.yres,
        },
        WAVEFORM_MODE_GC16,
    )?;
    Ok(())
}

/// Map a [`selfupdate::run_pull`] result to the one-line banner shown to the
/// user, so the in-app **Update** button (inline in [`run`]) and the `--update`
/// recovery launch ([`run_update`]) speak identically. `Staged` tells the user
/// to reopen Sidle — the launcher (`bin/sidle.sh`) swaps the staged `bin/sidle.new`
/// in on the next start (nothing maps the running binary at that moment). A hard
/// error is logged to the update log before it's flattened to the terse banner.
fn update_result_message(result: api::Result<selfupdate::UpdateOutcome>) -> String {
    match result {
        Ok(selfupdate::UpdateOutcome::UpToDate) => "Already up to date".to_string(),
        Ok(selfupdate::UpdateOutcome::Staged(_)) => {
            "Update staged — reopen Sidle to apply".to_string()
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
    }
}

/// `--update` mode: the LAN self-update as a standalone launch (the break-glass
/// twin of the in-app **Update** button — see the `--update` dispatch in `main`).
/// Pulls a staged self-update from sidle-server over the LAN and stages it as
/// `bin/sidle.new` for the launcher to swap in on next start. Shares the pull +
/// message code with the button (`selfupdate::run_pull` + [`update_result_message`])
/// but brings up its OWN minimal device setup (framebuffer + X11 window +
/// renderer + input) to show a result toast and block on a tap to exit — the same
/// shape as `diag::run`. That minimal setup is the point: it dodges the gallery's
/// list/grid code, so it still works when a crash there makes the in-app button
/// unreachable. Not a recovery path for a crash in this device setup itself: a
/// graphics-init failure ends `--update` too.
fn run_update() -> anyhow::Result<()> {
    update_log("=== LAN self-update (--update): start ===");
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
    let message = update_result_message(selfupdate::run_pull(
        &agent,
        &cfg,
        Path::new(BUNDLE_DIR),
        selfupdate::self_build_ts(),
        |m| update_log(m),
    ));
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
    // download over a single warm connection (see api::get_with_token). The
    // per-read timeout bounds a stalled socket without capping total transfer
    // time, so a 300 MB+ book over a slow radio still completes; a dead
    // connection fails in `SOCKET_READ_TIMEOUT` instead of hanging the picker.
    // (list/cover keep their own short *overall* deadlines on top, via
    // get_with_token — this per-read bound only ever tightens them.)
    let agent = ureq::AgentBuilder::new()
        .timeout_read(SOCKET_READ_TIMEOUT)
        .build();
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
    // `mut`: a mid-session download removes its book from this master set so the
    // tile hides immediately (see the long-press handler), matching the
    // boot-time hide of books already on the device.
    let mut all_books: Vec<api::Book> = books
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
    // Romaji search query (top-level only). Typed on the on-screen keyboard
    // (`ui::keyboard`), folded into `rebuild_view` alongside the facets.
    let mut query = String::new();
    let mut entries = series::group_by_series(rebuild_view(&all_books, &filters, sort, &query));
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
        &mut fb,
        &mut renderer,
        &agent,
        &cfg,
        cache_dir,
        &cells,
        &mut covers,
        page,
        total_pages,
        grid_left,
        grid_top,
        sort,
        filters.active_facets(),
        series_view.as_deref(),
        &query,
    )?;

    let mut armed: Option<Armed> = None;
    // Where the current touch landed (set on Down, cleared on Up) — lets the Up
    // handler tell a tap from a horizontal page-flip swipe. Independent of
    // `armed`, since a swipe can start anywhere, not just on a cover cell.
    let mut down_pos: Option<(u32, u32)> = None;
    loop {
        let event = input.next()?;

        match event {
            InputEvent::Touch(TouchEvent::Down { x, y }) => {
                log(format!("down: ({x},{y})"));
                // Remember the landing point so the matching Up can tell a tap
                // from a horizontal page-flip swipe (the Colorsoft has no bezel
                // page buttons).
                down_pos = Some((x, y));
                // Down on a cell arms it. Down on the strip or in margins
                // is a no-op — strip actions fire on Up regardless of
                // hold time, so they don't need to arm.
                let visible_count = cells.len().saturating_sub(page * PAGE_SIZE).min(PAGE_SIZE);
                if let Some(cell_pos) = grid::cell_at_tap(x, y, grid_left, grid_top, visible_count)
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

                // A horizontal swipe flips the page — the page-turn affordance
                // the buttonless Colorsoft otherwise lacks. Checked before any
                // tap/long-press/drill the Down armed, so a deliberate drag
                // never downloads or drills. `take()` clears the landing point
                // for this stroke whether or not it turns out to be a swipe.
                if let Some(dir) = down_pos
                    .take()
                    .and_then(|(x0, y0)| classify_swipe(x0, y0, x, y, fb.var.xres))
                {
                    // Cancel whatever the Down armed; the repaint clears its
                    // outline. `had_armed` forces a repaint even at a page
                    // boundary so a lingering outline doesn't stay on screen.
                    let had_armed = armed.take().is_some();
                    let new_page = match dir {
                        SwipeDir::Next => (page + 1).min(total_pages.saturating_sub(1)),
                        SwipeDir::Prev => page.saturating_sub(1),
                    };
                    log(format!("swipe {dir:?}: page {page} -> {new_page}"));
                    if new_page != page || had_armed {
                        page = new_page;
                        repaint_page(
                            &mut fb,
                            &mut renderer,
                            &agent,
                            &cfg,
                            cache_dir,
                            &cells,
                            &mut covers,
                            page,
                            total_pages,
                            grid_left,
                            grid_top,
                            sort,
                            filters.active_facets(),
                            series_view.as_deref(),
                            &query,
                        )?;
                    }
                    continue;
                }

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
                                &mut fb,
                                &mut renderer,
                                &agent,
                                &cfg,
                                cache_dir,
                                &cells,
                                &mut covers,
                                page,
                                total_pages,
                                grid_left,
                                grid_top,
                                sort,
                                filters.active_facets(),
                                series_view.as_deref(),
                                &query,
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
                        // Grab the identity now: `book` borrows `cells`, and the
                        // hide-on-success rebuild below reassigns `cells`.
                        let dl_id = book.id;
                        log(format!(
                            "long press fired ({held:?}) on book {}: {}",
                            book.id, book.title,
                        ));
                        let dl_t0 = Instant::now();
                        let (banner_msg, saved) =
                            download_flow(&mut fb, &mut renderer, &mut input, &agent, &cfg, book)
                                .unwrap_or_else(|err| {
                                    log(format!("download flow error: {err:#}"));
                                    (format!("Failed: {err}"), false)
                                });
                        log(format!(
                            "download flow for book {dl_id} finished in {:?}",
                            dl_t0.elapsed()
                        ));
                        let dirty = toast::draw(&mut fb, &mut renderer, &banner_msg);
                        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                        thread::sleep(TOAST_LINGER);
                        // Hide the just-downloaded book: it's now on the device,
                        // and the picker's rule is "on device → not shown". Drop it
                        // from the master set and re-derive the current view (top
                        // level, or the drilled-in series' members) so the tile
                        // vanishes in the repaint below instead of lingering until
                        // the next launch. Keep the user on their page, clamped if
                        // its last tile just left.
                        if saved {
                            all_books.retain(|b| b.id != dl_id);
                            entries = series::group_by_series(rebuild_view(
                                &all_books, &filters, sort, &query,
                            ));
                            let drilled = series_view.clone();
                            cells = match drilled {
                                Some(name) => match series::members_of(&entries, &name) {
                                    Some(members) => series::cells_for_series(members),
                                    // The series' last undownloaded member was the
                                    // one we grabbed — it's gone; pop to top level.
                                    None => {
                                        series_view = None;
                                        series::cells_for_top(&entries)
                                    }
                                },
                                None => series::cells_for_top(&entries),
                            };
                            total_pages = pager::n_pages(cells.len());
                            covers = vec![None; cells.len()];
                            page = page.min(total_pages.saturating_sub(1));
                            log(format!(
                                "hid downloaded book {dl_id}: {} tiles, {total_pages} pages",
                                cells.len(),
                            ));
                        }
                        repaint_page(
                            &mut fb,
                            &mut renderer,
                            &agent,
                            &cfg,
                            cache_dir,
                            &cells,
                            &mut covers,
                            page,
                            total_pages,
                            grid_left,
                            grid_top,
                            sort,
                            filters.active_facets(),
                            series_view.as_deref(),
                            &query,
                        )?;
                    } else {
                        // Short tap on a cover — discovery hint. Without
                        // this, a tap-trained user keeps tapping and
                        // wondering why nothing happens.
                        log(format!("short tap ({held:?}), showing hint"));
                        let dirty = toast::draw(&mut fb, &mut renderer, "Hold cover to download");
                        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                        thread::sleep(TOAST_LINGER);
                        // Repaint to clear both the toast and the cell
                        // outline that Down left behind.
                        repaint_page(
                            &mut fb,
                            &mut renderer,
                            &agent,
                            &cfg,
                            cache_dir,
                            &cells,
                            &mut covers,
                            page,
                            total_pages,
                            grid_left,
                            grid_top,
                            sort,
                            filters.active_facets(),
                            series_view.as_deref(),
                            &query,
                        )?;
                    }
                    continue;
                }

                // Search bar (top level only — the bar isn't drawn when drilled).
                // Tap the field → on-screen keyboard; tap the `clear` zone → drop
                // the query. Either way re-filter (only if the query changed) and
                // repaint, since the keyboard overwrote the screen.
                if series_view.is_none()
                    && let Some(tap) =
                        ui::searchbar::hit(x, y, fb.var.xres, !query.is_empty(), true)
                {
                    // Update and Sync are actions (LAN self-update / annotation
                    // push), not query edits — run inline and loop, leaving the
                    // query and view untouched.
                    match tap {
                        ui::searchbar::Tap::Update => {
                            log("update-button tap");
                            let dirty = toast::draw(&mut fb, &mut renderer, "Checking for update…");
                            fb.send_update(dirty, WAVEFORM_MODE_GC16)?;

                            let banner_msg = update_result_message(selfupdate::run_pull(
                                &agent,
                                &cfg,
                                Path::new(BUNDLE_DIR),
                                selfupdate::self_build_ts(),
                                |m| update_log(m),
                            ));
                            update_log(format!("in-app update: {banner_msg}"));
                            let dirty = toast::draw(&mut fb, &mut renderer, &banner_msg);
                            fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                            thread::sleep(TOAST_LINGER);
                            repaint_page(
                                &mut fb,
                                &mut renderer,
                                &agent,
                                &cfg,
                                cache_dir,
                                &cells,
                                &mut covers,
                                page,
                                total_pages,
                                grid_left,
                                grid_top,
                                sort,
                                filters.active_facets(),
                                series_view.as_deref(),
                                &query,
                            )?;
                            continue;
                        }
                        ui::searchbar::Tap::Sync => {
                            // Push this device's reading-state sidecars to the Mac
                            // — the LAN twin of a USB annotation sync. The grid
                            // doesn't change, so toast the report and repaint the
                            // page underneath.
                            log("sync-button tap");
                            let dirty = toast::draw(&mut fb, &mut renderer, "Syncing annotations…");
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
                                    log(
                                        "token rejected during sync — resync via sidle desktop app",
                                    );
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
                                &mut fb,
                                &mut renderer,
                                &agent,
                                &cfg,
                                cache_dir,
                                &cells,
                                &mut covers,
                                page,
                                total_pages,
                                grid_left,
                                grid_top,
                                sort,
                                filters.active_facets(),
                                series_view.as_deref(),
                                &query,
                            )?;
                            continue;
                        }
                        _ => {}
                    }

                    let before = query.clone();
                    match tap {
                        ui::searchbar::Tap::Open => {
                            log("search-bar tap → keyboard");
                            query = ui::keyboard::run(
                                &mut fb,
                                &mut input,
                                &mut renderer,
                                &all_books,
                                &filters,
                                &query,
                                &mut current_orient,
                            )?;
                        }
                        ui::searchbar::Tap::Clear => {
                            log("search cleared");
                            query.clear();
                        }
                        // Handled above (early `continue`); the field only yields
                        // Open/Clear here.
                        ui::searchbar::Tap::Update | ui::searchbar::Tap::Sync => unreachable!(),
                    }
                    if query != before {
                        entries = series::group_by_series(rebuild_view(
                            &all_books, &filters, sort, &query,
                        ));
                        cells = series::cells_for_top(&entries);
                        total_pages = pager::n_pages(cells.len());
                        covers = vec![None; cells.len()];
                        page = 0;
                        log(format!("search {:?}: {} tiles", query, cells.len()));
                    }
                    repaint_page(
                        &mut fb,
                        &mut renderer,
                        &agent,
                        &cfg,
                        cache_dir,
                        &cells,
                        &mut covers,
                        page,
                        total_pages,
                        grid_left,
                        grid_top,
                        sort,
                        filters.active_facets(),
                        series_view.as_deref(),
                        &query,
                    )?;
                    continue;
                }

                // No armed cell — Up on the strip means a strip action.
                // Off-cell-off-strip is ignored. `drilled` swaps the Filter slot
                // for Back (see pager::hit).
                if let Some(hit) = pager::hit(
                    x,
                    y,
                    fb.var.xres,
                    fb.var.yres,
                    total_pages,
                    series_view.is_some(),
                ) {
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
                                &mut fb,
                                &mut input,
                                &mut renderer,
                                &all_books,
                                &mut filters,
                                &mut sort,
                                &mut current_orient,
                            )?;
                            if filters != before_filters || sort != before_sort {
                                // Re-filter+sort the master, re-fold into series
                                // collections, reset paging, and drop the positional
                                // cover vec — it re-fills from the id-keyed disk
                                // cache on the paint below (no re-fetch).
                                entries = series::group_by_series(rebuild_view(
                                    &all_books, &filters, sort, &query,
                                ));
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
                                &mut fb,
                                &mut renderer,
                                &agent,
                                &cfg,
                                cache_dir,
                                &cells,
                                &mut covers,
                                page,
                                total_pages,
                                grid_left,
                                grid_top,
                                sort,
                                filters.active_facets(),
                                series_view.as_deref(),
                                &query,
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
                                &mut fb,
                                &mut renderer,
                                &agent,
                                &cfg,
                                cache_dir,
                                &cells,
                                &mut covers,
                                page,
                                total_pages,
                                grid_left,
                                grid_top,
                                sort,
                                filters.active_facets(),
                                series_view.as_deref(),
                                &query,
                            )?;
                        }
                        PagerHit::Prev => {
                            let new_page = page.saturating_sub(1);
                            if new_page != page {
                                page = new_page;
                                repaint_page(
                                    &mut fb,
                                    &mut renderer,
                                    &agent,
                                    &cfg,
                                    cache_dir,
                                    &cells,
                                    &mut covers,
                                    page,
                                    total_pages,
                                    grid_left,
                                    grid_top,
                                    sort,
                                    filters.active_facets(),
                                    series_view.as_deref(),
                                    &query,
                                )?;
                            }
                        }
                        PagerHit::Next => {
                            let new_page = (page + 1).min(total_pages.saturating_sub(1));
                            if new_page != page {
                                page = new_page;
                                repaint_page(
                                    &mut fb,
                                    &mut renderer,
                                    &agent,
                                    &cfg,
                                    cache_dir,
                                    &cells,
                                    &mut covers,
                                    page,
                                    total_pages,
                                    grid_left,
                                    grid_top,
                                    sort,
                                    filters.active_facets(),
                                    series_view.as_deref(),
                                    &query,
                                )?;
                            }
                        }
                        PagerHit::Source => {
                            // Library-switch button (former Sync slot): a stub that
                            // toasts, pending the on-device DRM-books source. The
                            // grid doesn't change, so repaint the page underneath.
                            log("source-button tap");
                            let dirty =
                                toast::draw(&mut fb, &mut renderer, "DRM books — coming soon");
                            fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                            thread::sleep(TOAST_LINGER);
                            repaint_page(
                                &mut fb,
                                &mut renderer,
                                &agent,
                                &cfg,
                                cache_dir,
                                &cells,
                                &mut covers,
                                page,
                                total_pages,
                                grid_left,
                                grid_top,
                                sort,
                                filters.active_facets(),
                                series_view.as_deref(),
                                &query,
                            )?;
                        }
                    }
                }
            }
            InputEvent::Touch(TouchEvent::Screenshot) => {
                // Two-corner gesture. Cancel any cell the first finger armed so
                // its lift can't fire a download, then capture + flash. Also drop
                // the swipe landing point — the suppressed lift won't clear it.
                armed = None;
                down_pos = None;
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
                // finger-up won't fire a download on the wrong page. Same for the
                // swipe landing point — it'd be stale across this page change.
                armed = None;
                down_pos = None;
                let new_page = match pb {
                    PageButton::Prev => page.saturating_sub(1),
                    PageButton::Next => (page + 1).min(total_pages.saturating_sub(1)),
                };
                if new_page != page {
                    page = new_page;
                    repaint_page(
                        &mut fb,
                        &mut renderer,
                        &agent,
                        &cfg,
                        cache_dir,
                        &cells,
                        &mut covers,
                        page,
                        total_pages,
                        grid_left,
                        grid_top,
                        sort,
                        filters.active_facets(),
                        series_view.as_deref(),
                        &query,
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
                            &mut fb,
                            &mut renderer,
                            &agent,
                            &cfg,
                            cache_dir,
                            &cells,
                            &mut covers,
                            page,
                            total_pages,
                            grid_left,
                            grid_top,
                            sort,
                            filters.active_facets(),
                            series_view.as_deref(),
                            &query,
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
fn rebuild_view(
    all_books: &[api::Book],
    filters: &Filters,
    sort: SortState,
    query: &str,
) -> Vec<api::Book> {
    // Search and facets are ANDed; survivors regroup into series tiles (a tile
    // shows iff ≥1 member matches — members carry the series romaji in their
    // search_key, so searching a series name surfaces the collection).
    let cq = search::canon(query);
    let mut view: Vec<api::Book> = all_books
        .iter()
        .filter(|b| filter::matches(b, filters, None) && search::matches(b, &cq))
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
    query: &str,
) -> anyhow::Result<()> {
    fb.fill_rect(0, 0, fb.var.xres, fb.var.yres, 0xFF);

    // Top chrome. At the top level: the search bar (the shared widget — same in
    // the keyboard overlay), then the sort header just below it. Drilled into a
    // series: no bar (search is a top-level action), just the series-name header.
    let hbaseline = if drilled {
        grid_top * 60 / 100
    } else {
        searchbar::draw(fb, renderer, query, true);
        searchbar::draw_buttons(fb);
        (searchbar::TOP + searchbar::HEIGHT) as i32 + renderer.line_height() as i32
    };
    // Header line, clamped to one line so a long series name can't overrun.
    let hlines = renderer.wrap_and_clamp(header, fb.var.xres.saturating_sub(80), 1);
    if let Some(h) = hlines.first() {
        let hw = renderer.measure_width(h);
        let hx = ((fb.var.xres as i32 - hw as i32) / 2).max(0);
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
        MxcfbRect {
            top: 0,
            left: 0,
            width: fb.var.xres,
            height: fb.var.yres,
        },
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
    query: &str,
) -> anyhow::Result<()> {
    let drilled = series_view.is_some();
    let header = match series_view {
        Some(name) => format!("{name}  ({})", cells.len()),
        None => format!("Sorted by {}", sort.header()),
    };
    draw_gallery_page(
        fb,
        renderer,
        cells,
        covers,
        page,
        total_pages,
        grid_left,
        grid_top,
        &header,
        filter_count,
        drilled,
        query,
    )?;
    fetch_and_paint_page(
        fb, renderer, agent, cfg, cache_dir, cells, covers, page, grid_left, grid_top,
    )?;
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

/// Download a book to `/mnt/us/documents/Sidle/<filename>` while showing a live
/// `transferred / total` overlay with a Cancel button. Returns the toast
/// message to display when it settles **and** whether the book actually landed
/// on the device (`true` only on a verified, renamed-into-place file) — the
/// caller uses that to hide the now-downloaded tile immediately.
///
/// The body streams to disk one [`DL_CHUNK`] at a time — never buffered whole
/// in device RAM, which a 300 MB+ book would otherwise risk OOMing on a 512 MB
/// Kindle. Bytes land in a `<name>.part` sidecar (which the gallery's `.kfx`
/// filter ignores) and are renamed into place only once the transfer completes
/// **and** matches the server's `Content-Length`. So a cancel, a dropped
/// radio, or a capped stream leaves a `.part` that the next attempt overwrites
/// — never a truncated `.kfx` masquerading as a finished book (the very bug the
/// old 256 MB in-RAM cap produced). Mirrors the self-update `.download`
/// staging.
///
/// Between chunks it redraws progress at most every [`DL_REDRAW_INTERVAL`] and
/// polls input non-blocking: a tap inside the Cancel button, or any bezel
/// page-button press, aborts the transfer.
fn download_flow(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    input: &mut Input,
    agent: &ureq::Agent,
    cfg: &config::ServerConfig,
    book: &api::Book,
) -> anyhow::Result<(String, bool)> {
    // Paint the overlay before the GET so a long-press gets instant feedback:
    // the server reads the whole file before it sends headers, so on a big book
    // `download_book` can block for a beat — that shouldn't look like a dead
    // gallery. `title` drives the overlay for the rest of the flow.
    let title = format!("Downloading {}…", truncate_title(&book.title, 32));
    let (rect, _) = toast::draw_download(fb, renderer, &title, "Connecting…");
    fb.send_update(rect, WAVEFORM_MODE_GC16)?;

    let dl = match api::download_book(agent, cfg, book) {
        Ok(dl) => dl,
        Err(api::SidleError::TokenMismatch) => {
            log("token rejected during download — resync via sidle desktop app");
            return Ok((
                "Token mismatch.\nPlug Kindle into sidle and click Update KUAL.".to_string(),
                false,
            ));
        }
        Err(api::SidleError::Other(err)) => {
            log(format!("download failed: {err:#}"));
            return Ok((format!("Failed: {err}"), false));
        }
    };
    let expected = dl.expected_len;
    let mut reader = dl.reader;

    let dir = Path::new(DOWNLOAD_DIR);
    std::fs::create_dir_all(dir)?;
    // Strip any path components the server might have set in the filename —
    // defense against `..` traversal even though the server controls it.
    let safe_name = Path::new(&dl.filename)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("book.kfx");
    let path = dir.join(safe_name);
    let part = dir.join(format!("{safe_name}.part"));
    let mut file =
        std::fs::File::create(&part).with_context(|| format!("create {}", part.display()))?;

    let mut written: u64 = 0;
    let (rect, mut cancel_rect) =
        toast::draw_download(fb, renderer, &title, &progress_line(written, expected));
    fb.send_update(rect, WAVEFORM_MODE_DU)?;

    let mut buf = vec![0u8; DL_CHUNK];
    let mut last_draw = Instant::now();
    let mut chunks: u64 = 0;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) => {
                cleanup_part(file, &part);
                log(format!("download read failed after {written} bytes: {err}"));
                return Ok((format!("Failed: {err}"), false));
            }
        };
        if let Err(err) = file.write_all(&buf[..n]) {
            cleanup_part(file, &part);
            log(format!(
                "download write failed after {written} bytes: {err}"
            ));
            return Ok((format!("Failed: {err}"), false));
        }
        written += n as u64;
        chunks += 1;

        if last_draw.elapsed() >= DL_REDRAW_INTERVAL {
            let (rect, cr) =
                toast::draw_download(fb, renderer, &title, &progress_line(written, expected));
            cancel_rect = cr;
            fb.send_update(rect, WAVEFORM_MODE_DU)?;
            last_draw = Instant::now();
        }

        // Drain ALL pending input between chunks, not just one event: a
        // two-corner screenshot is two contacts, and both may have queued during
        // one network read — a single poll would surface only the first. The
        // gesture captures the live toast and keeps downloading; a Cancel-button
        // tap or any bezel press aborts.
        while let Some(ev) = input.poll_now()? {
            log(format!("dl input (chunk {chunks}): {ev:?}"));
            match ev {
                InputEvent::Touch(TouchEvent::Screenshot) => {
                    match eink::screenshot::capture(fb) {
                        Ok(p) => log(format!("screenshot saved: {}", p.display())),
                        Err(e) => log(format!("screenshot failed: {e:#}")),
                    }
                    // capture() already did a full GC16 restore of the toast;
                    // don't stack a DU redraw straight on top of it.
                    last_draw = Instant::now();
                }
                InputEvent::Touch(TouchEvent::Up { x, y }) if rect_hit(&cancel_rect, x, y) => {
                    cleanup_part(file, &part);
                    log(format!("download cancelled by user after {written} bytes"));
                    return Ok(("Download cancelled".to_string(), false));
                }
                InputEvent::Page(_) => {
                    cleanup_part(file, &part);
                    log(format!("download cancelled by user after {written} bytes"));
                    return Ok(("Download cancelled".to_string(), false));
                }
                _ => {}
            }
        }
    }

    file.sync_all().ok();
    drop(file);
    log(format!("dl streamed {written} bytes over {chunks} chunks"));
    // A short transfer that ended in a clean EOF (dropped connection, or the
    // KFX_MAX_BYTES backstop) reads as success up to here — the Content-Length
    // check is what turns it back into a visible failure.
    if let Some(exp) = expected
        && written != exp
    {
        let _ = std::fs::remove_file(&part);
        log(format!("incomplete download: {written} of {exp} bytes"));
        return Ok((
            format!(
                "Failed: incomplete ({} of {})",
                human_mb(written),
                human_mb(exp)
            ),
            false,
        ));
    }
    std::fs::rename(&part, &path)
        .with_context(|| format!("rename {} -> {}", part.display(), path.display()))?;
    log(format!("downloaded {written} bytes to {}", path.display()));
    let _ = Command::new("touch").arg(CLEANINDEX).output();
    Ok((
        "Downloaded → Library will refresh shortly".to_string(),
        true,
    ))
}

/// Close and delete a partial `.part` sidecar after a failed/cancelled
/// transfer, so it never lingers or gets mistaken for a finished download.
fn cleanup_part(file: std::fs::File, part: &Path) {
    drop(file);
    let _ = std::fs::remove_file(part);
}

/// `transferred / total (pct)` for the download overlay, e.g.
/// `"12.3 MB / 305.7 MB  (4%)"`. Falls back to just the transferred size when
/// the server sent no `Content-Length`.
fn progress_line(written: u64, total: Option<u64>) -> String {
    match total {
        Some(t) if t > 0 => {
            let pct = (written as f64 / t as f64 * 100.0).round() as u32;
            format!("{} / {}  ({pct}%)", human_mb(written), human_mb(t))
        }
        _ => human_mb(written),
    }
}

/// Bytes as a one-decimal MB string (`1 MB == 1024*1024`).
fn human_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// Whether `(x, y)` (touch coords) fall inside `r`.
fn rect_hit(r: &MxcfbRect, x: u32, y: u32) -> bool {
    x >= r.left && x < r.left + r.width && y >= r.top && y < r.top + r.height
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

/// Append a line to the dedicated LAN self-update log, so the update trail isn't
/// buried in the gallery's `LOG_PATH`. Both the in-app **Update** button and the
/// `--update` recovery launch write here. File only (no stderr echo): the
/// standalone `--update` run's own stderr goes wherever its shell caller points
/// it, and an in-app update's panics land in `LOG_PATH` (that's `run` executing),
/// so these explicit breadcrumbs never double-log.
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

#[cfg(test)]
mod tests {
    use super::{human_mb, progress_line, rect_hit};
    use crate::eink::fb::MxcfbRect;

    #[test]
    fn human_mb_is_binary_megabytes_one_decimal() {
        assert_eq!(human_mb(0), "0.0 MB");
        assert_eq!(human_mb(1024 * 1024), "1.0 MB");
        // 305.7 MB — the ballpark of the book that surfaced the 256 MB cap.
        assert_eq!(human_mb(320_593_920), "305.7 MB");
    }

    #[test]
    fn progress_line_shows_fraction_and_percent_when_total_known() {
        let total = 100 * 1024 * 1024;
        assert_eq!(progress_line(0, Some(total)), "0.0 MB / 100.0 MB  (0%)");
        assert_eq!(
            progress_line(25 * 1024 * 1024, Some(total)),
            "25.0 MB / 100.0 MB  (25%)"
        );
        assert_eq!(
            progress_line(total, Some(total)),
            "100.0 MB / 100.0 MB  (100%)"
        );
    }

    #[test]
    fn progress_line_falls_back_to_transferred_only_without_total() {
        assert_eq!(progress_line(5 * 1024 * 1024, None), "5.0 MB");
        // A zero Content-Length can't be a denominator — same fallback.
        assert_eq!(progress_line(5 * 1024 * 1024, Some(0)), "5.0 MB");
    }

    #[test]
    fn rect_hit_is_inclusive_of_the_top_left_and_exclusive_of_the_far_edge() {
        let r = MxcfbRect {
            top: 100,
            left: 200,
            width: 320,
            height: 84,
        };
        assert!(rect_hit(&r, 200, 100), "top-left corner is inside");
        assert!(rect_hit(&r, 519, 183), "last pixel inside");
        assert!(
            !rect_hit(&r, 520, 183),
            "one past the right edge is outside"
        );
        assert!(
            !rect_hit(&r, 519, 184),
            "one past the bottom edge is outside"
        );
        assert!(!rect_hit(&r, 199, 150), "left of the box is outside");
    }
}
