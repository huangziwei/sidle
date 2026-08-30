//! Sidle native — the paginated cover grid and download flow.

use std::fs::OpenOptions;
use std::io::{BufRead, Read, Write};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;

mod api;
mod collate;
mod config;
mod cover_cache;
mod dedrm;
mod device_state;
mod discover;
mod eink;
mod font;
mod handwriting;
mod orientation;
mod readinglog;
mod receipt;
mod running;
mod search;
mod selfupdate;
mod series;
mod ui;
mod updates;
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
use ui::pager::{self, PagerHit};
use ui::searchbar;
use ui::sort::SortState;
use ui::text::TextRenderer;
use ui::toast;

/// Where every app on this Kindle that follows the convention keeps its logs —
const LOG_DIR: &str = "/mnt/us/logs";
const LOG_PATH: &str = "/mnt/us/logs/sidle-native.log";
/// Dedicated log for the LAN self-update, so its trail isn't interleaved with
/// the gallery's `LOG_PATH`. Written by `update_log` from both the in-app
/// **Update** button (inline in `run`) and the `--update` recovery launch.
const UPDATE_LOG_PATH: &str = "/mnt/us/logs/sidle-update.log";
const CONFIG_PATH: &str = "/mnt/us/extensions/sidle/etc/server.conf";
const FONT_PX: f32 = 28.0;
/// Top margin above the grid. Holds the Amazon-style **search bar** (top level
/// only) plus the sort/results header line below it. Sized to seat both; the
/// grid origin derives from it (`grid::grid_origin`). On the KOA2 (1264×1680)
const TOP_MARGIN: u32 = 190;
/// Stock Kindle indexer watches `documents/` subfolders too (verified via
/// the existing `documents/Downloads/Items01/` indexed tree). Land here so
/// our books are grouped and easy to find in the library.
const DOWNLOAD_DIR: &str = "/mnt/us/documents/Sidle";
/// USB-drive root — the base for the misc backup scan: screenshots live in
/// `screenshots/` (and the root itself on KOA2 stock firmware), picker logs at the
/// root. See [`api::push_misc`].
const MNT_US: &str = "/mnt/us";
/// Where the firmware keeps everything the pen writes: ink drawn on sideloaded
const NOTEBOOKS_DIR: &str = "/mnt/us/.notebooks";
/// On-device cover thumbnail cache, under the extension dir (not documents/,
/// so the stock indexer never sees it). See [`cover_cache`].
const COVER_CACHE_DIR: &str = "/mnt/us/extensions/sidle/cache/covers";
/// Records the KFX revision (`Book::kfx_rev`) last written for each on-device
/// file, so the Sync tap can re-pull a book the desktop reconverted — in place,
/// under its frozen filename. Under the extension dir, never in documents/.
const SYNCED_REVS_PATH: &str = "/mnt/us/extensions/sidle/cache/synced_revs.json";
const CLEANINDEX: &str = "/mnt/us/system/.cleanindex";
const TOAST_LINGER: Duration = Duration::from_millis(1200);
/// How long a finger must rest on a book cover — without drifting more than
/// [`ARM_SLOP_PX`] — before the tile "arms" and its action (download / decrypt)
const ARM_THRESHOLD: Duration = Duration::from_millis(1000);
/// Max drift (either axis, user-visible px) from the finger's landing point that
/// still counts as a hold. Past this the stroke is a drag / page-flip swipe in
/// progress, so the arm is cancelled and the eventual `Up` classifies the swipe.
const ARM_SLOP_PX: u32 = 40;
/// Dwell between painting the armed cue and letting the action overlay paint over
const ARM_DWELL: Duration = Duration::from_millis(250);

/// Per-read socket timeout for the session agent. Bounds a genuinely stalled
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Read-buffer size for streaming a book to disk. Large enough to keep syscall
/// overhead negligible over a ~hundreds-of-MB transfer, small enough that the
/// Cancel poll between reads stays responsive on a healthy connection.
const DL_CHUNK: usize = 256 * 1024;
/// Minimum wall-clock between progress redraws. E-ink can't usefully repaint
/// faster, and throttling keeps the transfer (not the panel) the bottleneck.
const DL_REDRAW_INTERVAL: Duration = Duration::from_millis(700);
/// How often the decrypt-all wait loop re-checks the engine child for exit.
const DEDRM_WAIT_POLL: Duration = Duration::from_millis(50);

/// Cell currently outlined and awaiting release. On a book cell, release
struct Armed {
    /// Index into the current `cells` view (top-level entries when at the
    /// grouped top level, or drilled-in members) of the outlined tile.
    cell_idx: usize,
    down_at: Instant,
}

/// Which library the picker is showing. `Library` is the LAN server library (the
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Library,
    Drm,
}

/// The DRM view-models when the DRM source is active, else `None` — the value
fn drm_slice(source: Source, drm: &[dedrm::DrmBook]) -> Option<&[dedrm::DrmBook]> {
    matches!(source, Source::Drm).then_some(drm)
}

fn main() {
    // `--version`/`-V`: print the compiled version and exit. Cheap — no device
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("sidle {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // X11-window proof-of-concept (see eink::x11poc): validates that a
    // Sidle-created window is WM-managed + recomposited on teardown before we
    // port the renderer off raw /dev/fb0. Bypasses all fb/config setup.
    if std::env::args().any(|a| a == "--probe-x") {
        let r = eink::xprobe::run_logged();
        log(format!("xprobe done: {r:?}"));
        return;
    }
    if std::env::args().any(|a| a == "--x11-poc") {
        let r = eink::x11poc::run(log);
        log(format!("x11poc done: {r:?}"));
        return;
    }
    // `--archive-daemon`: keep the reading-event archive current, forever.
    if std::env::args().any(|a| a == readinglog::DAEMON_FLAG) {
        readinglog::claim_archiver();
        log("archive daemon started");
        loop {
            archive_once(false);
            std::thread::sleep(readinglog::ARCHIVE_INTERVAL);
        }
    }
    // `--archive`: a single pass, for running it by hand.
    if std::env::args().any(|a| a == "--archive") {
        archive_once(true);
        return;
    }
    // `--update`: the LAN self-update as a standalone launch. The everyday path
    if std::env::args().any(|a| a == "--update") {
        let result = run_update();
        update_log(format!("--update done: {result:?}"));
        return;
    }
    // Opening the picker is what (re)starts the archiver — after an update, after
    // a reboot, or the first time it is ever installed. Detached, so the gallery
    // never waits on it: the first pass on a fresh device reads a month of dumps.
    let state = readinglog::archiver();
    if let readinglog::Archiver::Outdated(pid) = state {
        readinglog::stop_archiver(pid);
        log(format!(
            "stopped an archiver from an older build (pid {pid})"
        ));
    }
    if state != readinglog::Archiver::Running {
        match readinglog::start_archiver() {
            Ok(pid) => log(format!("started archive daemon (pid {pid})")),
            Err(e) => log(format!("could not start archive daemon: {e}")),
        }
    }
    let result = run();
    log(format!("done: {result:?}"));
}

/// One archive pass: collect every reading event newer than what the archive
/// already holds, and add it.
fn archive_once(verbose: bool) {
    let us = std::path::Path::new(MNT_US);
    // `seen` is empty: that list is the *desktop's* record of dumps it has read,
    let found = readinglog::collect(us, &readinglog::archive_watermark(us), &[]);
    match readinglog::archive(us, &found.lines) {
        Ok(Some(name)) => log(format!("archived {} lines → {name}", found.lines.len())),
        Ok(None) if verbose => log("archive: nothing new"),
        Ok(None) => {}
        Err(e) => log(format!("archive failed: {e}")),
    }
}

/// Paint a clean centered banner panel: white-fill the screen, draw `message`,
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

/// Point `cfg.host` at an address that answers, recorded at [`CONFIG_PATH`].
/// `true` for a `cfg.host` this call replaced.
fn relocate_server(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    agent: &ureq::Agent,
    cfg: &mut config::ServerConfig,
    log: &dyn Fn(&str),
) -> bool {
    if api::is_sidle_server(agent, &cfg.host, cfg.port) {
        return false;
    }
    log(&format!(
        "{} is not answering — searching the LAN",
        cfg.host
    ));
    let _ = draw_panel(fb, renderer, "Looking for sidle on this network…");

    let port = cfg.port;
    let Some(found) = discover::find_server(
        port,
        |ip| api::is_sidle_server(agent, &ip.to_string(), port),
        |m| log(m),
    ) else {
        log("search: no sidle-server on this network");
        return false;
    };

    let host = found.to_string();
    match config::save_host(Path::new(CONFIG_PATH), &host) {
        // `host` serves this run; the next launch searches again.
        Err(e) => log(&format!("search: found {host} but {CONFIG_PATH}: {e:#}")),
        Ok(()) => log(&format!("search: {CONFIG_PATH} now points at {host}")),
    }
    cfg.host = host;
    true
}

/// The one-line banner for a [`selfupdate::run_pull`] result, shared by the
/// in-app **Update** button (inline in [`run`]) and [`run_update`]. One phrase
/// per outcome the pull produced. A hard error reaches the update log whole.
fn update_result_message(result: api::Result<selfupdate::UpdateReport>) -> String {
    let r = match result {
        Ok(r) => r,
        // Reuse the gallery's token-mismatch breadcrumb verbatim (see `diag`).
        Err(api::SidleError::TokenMismatch) => {
            return "Plug Kindle into sidle, click Update on Kindle".to_string();
        }
        Err(e) => {
            update_log(format!("FAILED: {e}"));
            return "Update failed — see log".to_string();
        }
    };
    if r.quiet() {
        return "Already up to date".to_string();
    }
    let mut parts = Vec::new();
    if !r.staged.is_empty() {
        parts.push("Staged — reopen Sidle".to_string());
    }
    if !r.written.is_empty() {
        parts.push(format!("Updated {} file(s)", r.written.len()));
    }
    if !r.kept.is_empty() {
        parts.push(format!("Kept {} changed on Kindle", r.kept.len()));
    }
    if !r.refused.is_empty() {
        parts.push("Server build not newer".to_string());
    }
    if !r.busy.is_empty() {
        parts.push(format!("{} in use — close the app", r.busy.len()));
    }
    if !r.failed.is_empty() {
        parts.push(format!("{} failed — see log", r.failed.len()));
    }
    parts.join(" · ")
}

/// `--update` mode: the LAN self-update as a standalone launch (the break-glass
/// twin of the in-app **Update** button — see the `--update` dispatch in `main`).
fn run_update() -> anyhow::Result<()> {
    update_log("=== LAN self-update (--update): start ===");
    update_log(format!("argv: {:?}", std::env::args().collect::<Vec<_>>()));
    let mut cfg = config::load(Path::new(CONFIG_PATH))?;
    update_log(format!("server: https://{}:{}", cfg.host, cfg.port));
    // A missing or unusable CA ends `--update` here rather than at the first
    // request, so the log names the actual problem — this is the break-glass
    // path, and "cannot reach the server" would send someone hunting the radio.
    let agent = api::build_agent(|c| c).map_err(|e| anyhow::anyhow!("{e}"))?;

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

    // `selfupdate::run_pull` below needs `cfg.host`. One probe for a `cfg.host`
    // that answers, on a connection `agent` keeps for the manifest fetch.
    if relocate_server(&mut fb, &mut renderer, &agent, &mut cfg, &|m| update_log(m)) {
        draw_panel(&mut fb, &mut renderer, "Checking for update…")?;
    }

    // Step-level breadcrumbs go to the dedicated update log via the closure.
    let message = update_result_message(selfupdate::run_pull(
        &agent,
        &cfg,
        Path::new(MNT_US),
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

    let mut cfg = config::load(Path::new(CONFIG_PATH))?;
    log(format!("server: https://{}:{}", cfg.host, cfg.port));

    // One agent for the whole session: keep-alive across list + covers +
    let agent = api::build_agent(|c| c.timeout_recv_body(Some(SOCKET_READ_TIMEOUT)))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let cache_dir = Path::new(COVER_CACHE_DIR);

    // Open the X11 window, input, and renderer *before* the first network
    let mut renderer = TextRenderer::load(FONT_PX)?;
    // Which faces this firmware actually has. A device missing one drops
    // it silently from the chain, and the only other symptom is a character
    // that doesn't draw.
    log(format!("fonts: {}", renderer.chain_description()));

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

    // Fetch the library, retrying through the Diagnostics screen on failure —
    // a toast and a return would flash the home screen back with no recourse.
    let t0 = Instant::now();
    let books = loop {
        match api::list_books(&agent, &cfg) {
            Ok(b) => break b,
            Err(err) => {
                log(format!("list_books failed: {err}"));
                // Ahead of `diag::run`, whose Retry redials `cfg.host`.
                if relocate_server(&mut fb, &mut renderer, &agent, &mut cfg, &|m| log(m)) {
                    continue;
                }
                match diag::run(&mut fb, &mut input, &mut renderer, &cfg, &err)? {
                    diag::Action::Retry => continue,
                    diag::Action::Exit => return Ok(()),
                }
            }
        }
    };
    let total_from_server = books.len();

    // Hide books that already live on this Kindle. The picker is a
    let downloaded = device_state::scan_downloaded_shas(Path::new(DOWNLOAD_DIR));
    // `mut`: a mid-session download removes its book from this master set so the
    // tile hides immediately (see the long-press handler), matching the
    // boot-time hide of books already on the device.
    let mut all_books: Vec<api::Book> = books
        .iter()
        .filter(|b| match b.kfx_sha256.as_deref() {
            Some(sha) if sha.len() >= 8 => !downloaded.contains(&sha[..8]),
            _ => true,
        })
        .cloned()
        .collect();

    // Which source is showing (LAN library vs on-device DRM books) + the DRM
    let mut source = Source::Library;
    let mut drm_books: Vec<dedrm::DrmBook> = Vec::new();
    let mut lib_stash: Vec<api::Book> = Vec::new();

    // `all_books` is the master (hide-downloaded) set. The picker is **grouped
    let mut sort = SortState::default();
    let mut filters = Filters::default();
    // Romaji search query (top-level only). Typed on the on-screen keyboard
    // (`ui::keyboard`), folded into `rebuild_view` alongside the facets.
    let mut query = String::new();
    let mut entries = series::group_by_series(rebuild_view(&all_books, &filters, sort, &query));
    let mut series_view: Option<String> = None;
    let mut cells = series::cells_for_top(&entries);

    // How many cells this panel fits, which sets the page size — so it has to be
    // known before the first page count is taken.
    let layout = grid::Layout::compute(fb.var.xres, fb.var.yres, TOP_MARGIN, pager::STRIP_H);
    log(format!(
        "grid: {}x{} cells of {}x{} ({} per page)",
        layout.cols,
        layout.rows,
        grid::CELL_W,
        layout.cell_h,
        layout.page_size()
    ));

    let mut total_pages = pager::n_pages(cells.len(), layout.page_size());
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
    let mut covers: Vec<Option<DynamicImage>> = vec![None; cells.len()];

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
        layout,
        sort,
        filters.active_facets(),
        series_view.as_deref(),
        &query,
        drm_slice(source, &drm_books),
    )?;

    let mut armed: Option<Armed> = None;
    // Where the current touch landed (set on Down, cleared on Up) — lets the Up
    // handler tell a tap from a horizontal page-flip swipe. Independent of
    // `armed`, since a swipe can start anywhere, not just on a cover cell.
    let mut down_pos: Option<(u32, u32)> = None;
    loop {
        // While a *book* cell is held, wake the loop at the arm threshold so the
        let deadline = match armed.as_ref() {
            Some(a) if matches!(cells.get(a.cell_idx).map(|c| &c.kind), Some(CellKind::Book)) => {
                Some(a.down_at + ARM_THRESHOLD)
            }
            _ => None,
        };
        let event = input.next_deadline(deadline)?;

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
                let visible_count = cells
                    .len()
                    .saturating_sub(page * layout.page_size())
                    .min(layout.page_size());
                if let Some(cell_pos) = layout.cell_at_tap(x, y, visible_count) {
                    let cell_idx = page * layout.page_size() + cell_pos;
                    let (cx, cy) = layout.cell_xy(cell_pos);
                    if cx >= 0 && cy >= 0 {
                        grid::outline_cell(&mut fb, cx, cy, layout.cell_h, true);
                        fb.send_update(
                            MxcfbRect {
                                top: cy as u32,
                                left: cx as u32,
                                width: grid::CELL_W,
                                height: layout.cell_h,
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
                            layout,
                            sort,
                            filters.active_facets(),
                            series_view.as_deref(),
                            &query,
                            drm_slice(source, &drm_books),
                        )?;
                    }
                    continue;
                }

                if let Some(a) = armed.take() {
                    // Resolve the armed tile to an owned decision *before* acting:
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
                            total_pages = pager::n_pages(cells.len(), layout.page_size());
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
                                layout,
                                sort,
                                filters.active_facets(),
                                series_view.as_deref(),
                                &query,
                                drm_slice(source, &drm_books),
                            )?;
                        }
                        continue;
                    }
                    // Book cell released. A hold long enough to act already
                    log(format!(
                        "short tap ({:?}), showing hint",
                        a.down_at.elapsed()
                    ));
                    let hint = match source {
                        Source::Drm => "Hold cover to decrypt",
                        Source::Library => "Hold cover to download",
                    };
                    let dirty = toast::draw(&mut fb, &mut renderer, hint);
                    fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                    thread::sleep(TOAST_LINGER);
                    // Repaint to clear both the toast and the cell outline Down left.
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
                        layout,
                        sort,
                        filters.active_facets(),
                        series_view.as_deref(),
                        &query,
                        drm_slice(source, &drm_books),
                    )?;
                    continue;
                }

                // Search bar (top level only — the bar isn't drawn when drilled).
                if series_view.is_none()
                    && let Some(tap) = ui::searchbar::hit(
                        x,
                        y,
                        fb.var.xres,
                        !query.is_empty(),
                        true,
                        matches!(source, Source::Drm),
                    )
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
                                Path::new(MNT_US),
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
                                layout,
                                sort,
                                filters.active_facets(),
                                series_view.as_deref(),
                                &query,
                                drm_slice(source, &drm_books),
                            )?;
                            continue;
                        }
                        ui::searchbar::Tap::DecryptAll => {
                            // DRM view's right disc (the library view's Update slot,
                            log("decrypt-all button tap");
                            if drm_books.is_empty() {
                                let dirty =
                                    toast::draw(&mut fb, &mut renderer, "No DRM books to decrypt");
                                fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                                thread::sleep(TOAST_LINGER);
                            } else {
                                let summary = decrypt_all_flow(
                                    &mut fb,
                                    &mut renderer,
                                    &mut input,
                                    &agent,
                                    &cfg,
                                    &drm_books,
                                )
                                .unwrap_or_else(|err| {
                                    log(format!("decrypt-all flow error: {err:#}"));
                                    format!("Decrypt-all failed: {err}")
                                });
                                // Full-panel summary: the progress banner is taller
                                // than a plain toast, so a full clear avoids leaving
                                // its top/bottom edges around the result.
                                draw_panel(&mut fb, &mut renderer, &summary)?;
                                thread::sleep(TOAST_LINGER);
                                drm_books = dedrm::scan();
                                all_books = drm_books.iter().map(|d| d.book.clone()).collect();
                                series_view = None;
                                entries = series::group_by_series(rebuild_view(
                                    &all_books, &filters, sort, &query,
                                ));
                                cells = series::cells_for_top(&entries);
                                total_pages = pager::n_pages(cells.len(), layout.page_size());
                                covers = vec![None; cells.len()];
                                page = 0;
                                log(format!(
                                    "post decrypt-all: {} DRM books left",
                                    all_books.len()
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
                                layout,
                                sort,
                                filters.active_facets(),
                                series_view.as_deref(),
                                &query,
                                drm_slice(source, &drm_books),
                            )?;
                            continue;
                        }
                        ui::searchbar::Tap::Sync => {
                            // The Sync button is context-aware: in the library it
                            // pushes reading-state sidecars (annotations); in DRM
                            // mode it re-pushes every decrypted book to the desktop.
                            log("sync-button tap");
                            let banner_msg = match source {
                                Source::Drm => {
                                    let decrypted = dedrm::decrypted_books();
                                    if decrypted.is_empty() {
                                        "No decrypted books to sync".to_string()
                                    } else {
                                        let dirty = toast::draw(
                                            &mut fb,
                                            &mut renderer,
                                            "Syncing decrypted books…",
                                        );
                                        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                                        sync_decrypted(&agent, &cfg, &decrypted)
                                    }
                                }
                                Source::Library => {
                                    let dirty = toast::draw(&mut fb, &mut renderer, "Syncing…");
                                    fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                                    let sync_t0 = Instant::now();
                                    // One walk of `.notebooks/` feeds both halves of the
                                    let hw_t0 = Instant::now();
                                    let pen = handwriting::scan(
                                        std::path::Path::new(NOTEBOOKS_DIR),
                                        &library_asins(&books),
                                    );
                                    if !pen.ink.is_empty()
                                        || !pen.notebooks.is_empty()
                                        || pen.foreign > 0
                                    {
                                        log(format!(
                                            "handwriting scan in {:?}: {} inked books, {} \
                                             notebooks, {} not ours",
                                            hw_t0.elapsed(),
                                            pen.ink.len(),
                                            pen.notebooks.len(),
                                            pen.foreign
                                        ));
                                    }
                                    match api::push_annotations(
                                        &agent,
                                        &cfg,
                                        std::path::Path::new(DOWNLOAD_DIR),
                                        &pen.ink,
                                    ) {
                                        Ok(report) => {
                                            let mut summary = report.summary();
                                            log(format!(
                                                "annotation sync ok in {:?}: {summary}",
                                                sync_t0.elapsed()
                                            ));
                                            // Same Sync tap also backs up screenshots + picker
                                            match api::push_misc(
                                                &agent,
                                                &cfg,
                                                std::path::Path::new(MNT_US),
                                            ) {
                                                Ok(misc) => {
                                                    if let Some(s) = misc.summary() {
                                                        log(format!("misc backup: {s}"));
                                                        summary = format!("{summary}\n{s}");
                                                    }
                                                }
                                                Err(err) => {
                                                    log(format!("misc backup failed: {err}"));
                                                    summary = format!("{summary}\n(backup failed)");
                                                }
                                            }
                                            // The standalone-notebook half of the pen sync.
                                            match api::push_notebooks(&agent, &cfg, &pen.notebooks)
                                            {
                                                Ok(nb) => {
                                                    for f in &nb.failed {
                                                        log(format!("notebook failed: {f}"));
                                                    }
                                                    if nb.suppressed > 0 {
                                                        log(format!(
                                                            "{} notebook(s) not restored — \
                                                             deleted in Sidle",
                                                            nb.suppressed
                                                        ));
                                                    }
                                                    if let Some(s) = nb.summary() {
                                                        log(format!("notebooks: {s}"));
                                                        summary = format!("{summary}\n{s}");
                                                    }
                                                }
                                                Err(err) => {
                                                    log(format!("notebook backup failed: {err}"));
                                                }
                                            }
                                            // Same Sync tap sends the reading sessions the
                                            let rl_t0 = Instant::now();
                                            // Archive first, then push. The
                                            archive_once(false);
                                            match api::push_reading_log(
                                                &agent,
                                                &cfg,
                                                std::path::Path::new(MNT_US),
                                            ) {
                                                Ok(rl) => {
                                                    log(format!(
                                                        "reading log in {:?}: {} new of {} \
                                                         sessions ({} extended), {} named, {} \
                                                         skipped; lines from live={}{} chunks={} \
                                                         dumps={} archive={}",
                                                        rl_t0.elapsed(),
                                                        rl.added,
                                                        rl.sessions,
                                                        rl.extended,
                                                        rl.attributed,
                                                        rl.skipped,
                                                        rl.from.live,
                                                        // The live log is the only source carrying
                                                        if rl.from.live_read {
                                                            ""
                                                        } else {
                                                            " (NO LIVE LOG)"
                                                        },
                                                        rl.from.chunks,
                                                        rl.from.dumps,
                                                        rl.from.archive
                                                    ));
                                                    if let Some(s) = rl.summary() {
                                                        summary = format!("{summary}\n{s}");
                                                    }
                                                }
                                                Err(err) => {
                                                    log(format!("reading log failed: {err}"));
                                                }
                                            }
                                            // Same Sync tap pulls any book the desktop
                                            {
                                                // Re-fetch the list so a reconvert done while the
                                                // picker was already open is still seen; fall back
                                                // to the boot snapshot if the refresh fails.
                                                let fresh = api::list_books(&agent, &cfg).ok();
                                                let for_update =
                                                    fresh.as_deref().unwrap_or(books.as_slice());
                                                let mut on_book =
                                                    |cur: usize, total: usize, title: &str| {
                                                        let dirty = toast::draw(
                                                            &mut fb,
                                                            &mut renderer,
                                                            &format!(
                                                                "Updating {cur}/{total}: {}…",
                                                                truncate_title(title, 22)
                                                            ),
                                                        );
                                                        let _ = fb
                                                            .send_update(dirty, WAVEFORM_MODE_GC16);
                                                    };
                                                let up = updates::pull_updates(
                                                    &agent,
                                                    &cfg,
                                                    for_update,
                                                    std::path::Path::new(DOWNLOAD_DIR),
                                                    std::path::Path::new(SYNCED_REVS_PATH),
                                                    &mut on_book,
                                                    &|line| log(line),
                                                );
                                                if let Some(s) = up.summary() {
                                                    log(format!("book updates: {s}"));
                                                    summary = format!("{summary}\n{s}");
                                                }
                                            }
                                            summary
                                        }
                                        Err(api::SidleError::TokenMismatch) => {
                                            log(
                                                "token rejected during sync — resync via sidle desktop app",
                                            );
                                            "Token mismatch.\nPlug Kindle into sidle and click Update on Kindle."
                                                .to_string()
                                        }
                                        Err(api::SidleError::Other(err)) => {
                                            log(format!("annotation sync failed: {err:#}"));
                                            format!("Sync failed: {err}")
                                        }
                                    }
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
                                layout,
                                sort,
                                filters.active_facets(),
                                series_view.as_deref(),
                                &query,
                                drm_slice(source, &drm_books),
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
                        ui::searchbar::Tap::Update
                        | ui::searchbar::Tap::Sync
                        | ui::searchbar::Tap::DecryptAll => unreachable!(),
                    }
                    if query != before {
                        entries = series::group_by_series(rebuild_view(
                            &all_books, &filters, sort, &query,
                        ));
                        cells = series::cells_for_top(&entries);
                        total_pages = pager::n_pages(cells.len(), layout.page_size());
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
                        layout,
                        sort,
                        filters.active_facets(),
                        series_view.as_deref(),
                        &query,
                        drm_slice(source, &drm_books),
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
                                entries = series::group_by_series(rebuild_view(
                                    &all_books, &filters, sort, &query,
                                ));
                                cells = series::cells_for_top(&entries);
                                total_pages = pager::n_pages(cells.len(), layout.page_size());
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
                                layout,
                                sort,
                                filters.active_facets(),
                                series_view.as_deref(),
                                &query,
                                drm_slice(source, &drm_books),
                            )?;
                        }
                        PagerHit::Back => {
                            // Pop the drill-in back to the grouped top level. Same
                            // strip slot as Filter, swapped in while drilled.
                            log("back to series top level");
                            series_view = None;
                            cells = series::cells_for_top(&entries);
                            total_pages = pager::n_pages(cells.len(), layout.page_size());
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
                                layout,
                                sort,
                                filters.active_facets(),
                                series_view.as_deref(),
                                &query,
                                drm_slice(source, &drm_books),
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
                                    layout,
                                    sort,
                                    filters.active_facets(),
                                    series_view.as_deref(),
                                    &query,
                                    drm_slice(source, &drm_books),
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
                                    layout,
                                    sort,
                                    filters.active_facets(),
                                    series_view.as_deref(),
                                    &query,
                                    drm_slice(source, &drm_books),
                                )?;
                            }
                        }
                        PagerHit::Source => {
                            // Library-switch button (former Sync slot): toggle the
                            // LAN library ↔ on-device DRM books.
                            log("source-button tap");
                            let before = source;
                            match source {
                                Source::Library => {
                                    // → DRM. Gate on the engine being installed (a
                                    if !dedrm::available() {
                                        let dirty = toast::draw(
                                            &mut fb,
                                            &mut renderer,
                                            "kfxdedrm not installed",
                                        );
                                        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                                        thread::sleep(TOAST_LINGER);
                                    } else {
                                        drm_books = dedrm::scan();
                                        if drm_books.is_empty() {
                                            let dirty = toast::draw(
                                                &mut fb,
                                                &mut renderer,
                                                "No DRM books in Items01",
                                            );
                                            fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                                            thread::sleep(TOAST_LINGER);
                                        } else {
                                            // Park the library master so the swap
                                            // back restores it (with any hides).
                                            lib_stash = std::mem::take(&mut all_books);
                                            all_books =
                                                drm_books.iter().map(|d| d.book.clone()).collect();
                                            source = Source::Drm;
                                            log(format!("→ DRM source: {} books", all_books.len()));
                                        }
                                    }
                                }
                                Source::Drm => {
                                    // → Library. Restore the parked master.
                                    all_books = std::mem::take(&mut lib_stash);
                                    source = Source::Library;
                                    log(format!("→ Library source: {} books", all_books.len()));
                                }
                            }
                            // Only rebuild when the source actually flipped — a
                            if source != before {
                                query.clear();
                                filters = Filters::default();
                                series_view = None;
                                entries = series::group_by_series(rebuild_view(
                                    &all_books, &filters, sort, &query,
                                ));
                                cells = series::cells_for_top(&entries);
                                total_pages = pager::n_pages(cells.len(), layout.page_size());
                                covers = vec![None; cells.len()];
                                page = 0;
                            }
                            // Repaint regardless — a toast (or the swap) painted
                            // over the grid.
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
                                layout,
                                sort,
                                filters.active_facets(),
                                series_view.as_deref(),
                                &query,
                                drm_slice(source, &drm_books),
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
                        layout,
                        sort,
                        filters.active_facets(),
                        series_view.as_deref(),
                        &query,
                        drm_slice(source, &drm_books),
                    )?;
                }
            }
            InputEvent::Tick => {
                // A Tick means one of two things now:
                let arm_ready = match armed.as_ref() {
                    Some(a) => {
                        matches!(cells.get(a.cell_idx).map(|c| &c.kind), Some(CellKind::Book))
                            && a.down_at.elapsed() >= ARM_THRESHOLD
                    }
                    None => false,
                };
                if arm_ready {
                    let a = armed.take().unwrap();
                    // Slop guard: a finger that wandered off its landing point is
                    let (px, py) = input.touch_pos();
                    let (dx, dy) = down_pos.unwrap_or((px, py));
                    if px.abs_diff(dx) > ARM_SLOP_PX || py.abs_diff(dy) > ARM_SLOP_PX {
                        log(format!(
                            "arm cancelled: drifted to ({px},{py}) from ({dx},{dy})"
                        ));
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
                            layout,
                            sort,
                            filters.active_facets(),
                            series_view.as_deref(),
                            &query,
                            drm_slice(source, &drm_books),
                        )?;
                    } else {
                        // Flip the tile to the armed cue and present it (partial
                        // refresh + short dwell) before the action overlay paints
                        // over it, so the "held long enough" signal is actually seen.
                        let cell_pos = a.cell_idx.saturating_sub(page * layout.page_size());
                        let (cx, cy) = layout.cell_xy(cell_pos);
                        if cx >= 0 && cy >= 0 {
                            grid::draw_arm_cue(&mut fb, cx, cy, layout.cell_h);
                            fb.send_update(
                                MxcfbRect {
                                    top: cy as u32,
                                    left: cx as u32,
                                    width: grid::CELL_W,
                                    height: layout.cell_h,
                                },
                                WAVEFORM_MODE_DU,
                            )?;
                            thread::sleep(ARM_DWELL);
                        }
                        // Auto-fire: act on the book — download it (library) or
                        let book = &cells[a.cell_idx].cover_book;
                        // Grab the identity now: `book` borrows `cells`, and the
                        // hide-on-success rebuild below reassigns `cells`.
                        let dl_id = book.id;
                        let held = a.down_at.elapsed();
                        log(format!(
                            "arm fired ({held:?}) on book {}: {}",
                            book.id, book.title
                        ));
                        let dl_t0 = Instant::now();
                        let (banner_msg, saved) = match source {
                            Source::Drm => match drm_books.get(dl_id as usize) {
                                Some(drm_book) => decrypt_flow(
                                    &mut fb,
                                    &mut renderer,
                                    &mut input,
                                    &agent,
                                    &cfg,
                                    drm_book,
                                )
                                .unwrap_or_else(|err| {
                                    log(format!("decrypt flow error: {err:#}"));
                                    (format!("Failed: {err}"), false)
                                }),
                                None => ("DRM book not found".to_string(), false),
                            },
                            Source::Library => download_flow(
                                &mut fb,
                                &mut renderer,
                                &mut input,
                                &agent,
                                &cfg,
                                book,
                            )
                            .unwrap_or_else(|err| {
                                log(format!("download flow error: {err:#}"));
                                (format!("Failed: {err}"), false)
                            }),
                        };
                        log(format!(
                            "action for book {dl_id} finished in {:?}",
                            dl_t0.elapsed()
                        ));
                        // Terminal banner. The DRM decrypt overlay is a plain 140px
                        let dirty = match source {
                            Source::Drm => toast::draw(&mut fb, &mut renderer, &banner_msg),
                            Source::Library => {
                                toast::draw_download_done(&mut fb, &mut renderer, &banner_msg)
                            }
                        };
                        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                        thread::sleep(TOAST_LINGER);
                        // Hide the just-acted book: on the device now, and the
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
                            total_pages = pager::n_pages(cells.len(), layout.page_size());
                            covers = vec![None; cells.len()];
                            page = page.min(total_pages.saturating_sub(1));
                            log(format!(
                                "hid book {dl_id}: {} tiles, {total_pages} pages",
                                cells.len(),
                            ));
                        }
                        // The holding finger's eventual lift must not read as a swipe.
                        down_pos = None;
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
                            layout,
                            sort,
                            filters.active_facets(),
                            series_view.as_deref(),
                            &query,
                            drm_slice(source, &drm_books),
                        )?;
                    }
                } else if armed.is_none() {
                    // Idle poll. Two things can leave the window stale, and both
                    // are repaired the same way — repaint the current page.
                    let o = orientation::Orientation::detect();
                    let damaged = fb.pump_events();
                    if o != current_orient || damaged {
                        if o != current_orient {
                            log(format!("orientation: {current_orient:?} -> {o:?}"));
                            current_orient = o;
                            input.set_orientation(o);
                        } else {
                            log("x11: damage — repainting");
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
                            layout,
                            sort,
                            filters.active_facets(),
                            series_view.as_deref(),
                            &query,
                            drm_slice(source, &drm_books),
                        )?;
                    }
                }
                // else: armed but below threshold (the deadline gates this Tick, so
                // this shouldn't occur) — fall through and keep polling.
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
#[allow(clippy::too_many_arguments)]
fn draw_gallery_page(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    cells: &[Cell],
    covers: &[Option<DynamicImage>],
    page: usize,
    total_pages: usize,
    layout: grid::Layout,
    header: &str,
    filter_count: usize,
    drilled: bool,
    query: &str,
    drm_active: bool,
) -> anyhow::Result<()> {
    fb.fill_rect(0, 0, fb.var.xres, fb.var.yres, 0xFF);

    // Top chrome. At the top level: the search bar (the shared widget — same in
    // the keyboard overlay), then the sort header just below it. Drilled into a
    // series: no bar (search is a top-level action), just the series-name header.
    let hbaseline = if drilled {
        layout.top * 60 / 100
    } else {
        searchbar::draw(fb, renderer, query, true);
        searchbar::draw_buttons(fb, drm_active);
        (searchbar::TOP + searchbar::HEIGHT) as i32 + renderer.line_height() as i32
    };
    // Header line, clamped to one line so a long series name can't overrun.
    let hlines = renderer.wrap_and_clamp(header, fb.var.xres.saturating_sub(80), 1);
    if let Some(h) = hlines.first() {
        let hw = renderer.measure_width(h);
        let hx = ((fb.var.xres as i32 - hw as i32) / 2).max(0);
        renderer.draw(fb, hx, hbaseline, h, false);
    }

    let start = page * layout.page_size();
    let end = (start + layout.page_size()).min(cells.len());
    for (cell_pos, idx) in (start..end).enumerate() {
        let (cx, cy) = layout.cell_xy(cell_pos);
        if cx < 0 || cy < 0 {
            continue;
        }
        // A series tile carries its lead member's language: the collection
        // name is that shelf's language too.
        let script = font::Script::of_language(&cells[idx].cover_book.language);
        match &cells[idx].kind {
            CellKind::Book => {
                let title = grid::Label {
                    text: &cells[idx].cover_book.title,
                    script,
                };
                grid::draw_book_cell(
                    fb,
                    renderer,
                    cx,
                    cy,
                    layout.cell_h,
                    covers[idx].as_ref(),
                    title,
                );
            }
            CellKind::Series { name, count } => {
                let name = grid::Label { text: name, script };
                grid::draw_series_cell(
                    fb,
                    renderer,
                    cx,
                    cy,
                    layout.cell_h,
                    covers[idx].as_ref(),
                    *count,
                    name,
                );
            }
        }
    }
    // Strip is the only path to Exit — always draw, even on a single
    // page. `pager::draw` internally returns early after Exit when
    // total_pages <= 1, so no prev/next labels are shown then.
    pager::draw(
        fb,
        renderer,
        page,
        total_pages,
        filter_count,
        drilled,
        drm_active,
    );
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
    layout: grid::Layout,
    sort: SortState,
    filter_count: usize,
    series_view: Option<&str>,
    query: &str,
    drm: Option<&[dedrm::DrmBook]>,
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
        layout,
        &header,
        filter_count,
        drilled,
        query,
        drm.is_some(),
    )?;
    fetch_and_paint_page(
        fb, renderer, agent, cfg, cache_dir, cells, covers, page, layout, drm,
    )?;
    Ok(())
}

/// Populate `covers[start..end]` for the given page by fetching any cell whose
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
    layout: grid::Layout,
    drm: Option<&[dedrm::DrmBook]>,
) -> anyhow::Result<()> {
    let start = page * layout.page_size();
    let end = (start + layout.page_size()).min(cells.len());
    let t_pg = Instant::now();
    let mut fetched = 0usize;
    for idx in start..end {
        if covers[idx].is_some() {
            continue;
        }
        // The cover source is the cell's own book (standalone) or its series'
        // lead member — one fetch per collection, not one per member. In DRM mode
        // it's the book's local device thumbnail (keyed by `book.id` = drm index);
        let book = &cells[idx].cover_book;
        let img = match drm {
            Some(drm_books) => drm_books
                .get(book.id as usize)
                .and_then(|d| d.cover_path.as_deref())
                .and_then(dedrm_cover),
            None => load_cover(agent, cfg, cache_dir, book),
        };

        if let Some(img) = img.as_ref() {
            let (cx, cy) = layout.cell_xy(idx - start);
            if cx >= 0 && cy >= 0 {
                let script = font::Script::of_language(&cells[idx].cover_book.language);
                match &cells[idx].kind {
                    CellKind::Book => {
                        let title = grid::Label {
                            text: &cells[idx].cover_book.title,
                            script,
                        };
                        grid::draw_book_cell(fb, renderer, cx, cy, layout.cell_h, Some(img), title);
                    }
                    CellKind::Series { name, count } => {
                        let name = grid::Label { text: name, script };
                        grid::draw_series_cell(
                            fb,
                            renderer,
                            cx,
                            cy,
                            layout.cell_h,
                            Some(img),
                            *count,
                            name,
                        );
                    }
                }
                fb.send_update(
                    MxcfbRect {
                        top: cy as u32,
                        left: cx as u32,
                        width: grid::CELL_W,
                        height: layout.cell_h,
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

/// Decode a DRM book's local device thumbnail into a grid image, or `None` if
fn dedrm_cover(path: &Path) -> Option<DynamicImage> {
    let bytes = std::fs::read(path).ok()?;
    grid::decode_resize(&bytes).ok()
}

/// Load one book's cover into a decoded image: disk cache first (instant, no
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

/// The content_ids of every book in the library, which is what tells the
fn library_asins(books: &[api::Book]) -> std::collections::HashSet<String> {
    books.iter().filter_map(|b| b.asin.clone()).collect()
}

/// Push every decrypted book to the desktop over the LAN, returning a one-line
fn sync_decrypted(
    agent: &ureq::Agent,
    cfg: &config::ServerConfig,
    decrypted: &[std::path::PathBuf],
) -> String {
    let (mut imported, mut dup, mut failed) = (0u32, 0u32, 0u32);
    for out in decrypted {
        match api::push_book(agent, cfg, out) {
            Ok(outcome) => {
                match outcome {
                    api::BookPush::Imported => imported += 1,
                    api::BookPush::Duplicate => dup += 1,
                }
                // Confirmed on the desktop → remove this book's on-device
                if let Some(book) = dedrm::source_book(out) {
                    for (path, err) in dedrm::cleanup_synced(&book) {
                        log(format!("sync cleanup {}: {err}", path.display()));
                    }
                }
            }
            Err(api::SidleError::TokenMismatch) => {
                return "Token mismatch.\nPlug Kindle into sidle and click Update on Kindle."
                    .to_string();
            }
            Err(api::SidleError::Other(err)) => {
                log(format!("sync {}: {err:#}", out.display()));
                failed += 1;
            }
        }
    }
    format!("Synced: {imported} new, {dup} already, {failed} failed")
}

/// Handle one input event that arrives while a decrypt flow owns the screen.
fn decrypt_input_event(fb: &mut Framebuffer, ev: InputEvent) {
    log(format!("decrypt input: {ev:?}"));
    if ev == InputEvent::Touch(TouchEvent::Screenshot) {
        match eink::screenshot::capture(fb) {
            Ok(p) => log(format!("screenshot saved: {}", p.display())),
            Err(e) => log(format!("screenshot failed: {e:#}")),
        }
    }
}

/// Drain every queued input event through [`decrypt_input_event`]. Called
fn drain_decrypt_input(input: &mut Input, fb: &mut Framebuffer) -> anyhow::Result<()> {
    while let Some(ev) = input.poll_now()? {
        decrypt_input_event(fb, ev);
    }
    Ok(())
}

/// Decrypt one on-device DRM purchase in place via the kfxdedrm engine — the DRM
fn decrypt_flow(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    input: &mut Input,
    agent: &ureq::Agent,
    cfg: &config::ServerConfig,
    book: &dedrm::DrmBook,
) -> anyhow::Result<(String, bool)> {
    let short = truncate_title(&book.book.title, 32);
    let dirty = toast::draw(fb, renderer, &format!("Decrypting {short}…"));
    fb.send_update(dirty, WAVEFORM_MODE_GC16)?;

    let Some(exe) = dedrm::probe_exe() else {
        return Ok(("No working kfxdedrm binary".to_string(), false));
    };

    let mut child = match Command::new(&exe)
        .arg("dedrm")
        .arg(&book.path)
        .stdout(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log(format!("kfxdedrm spawn failed: {e}"));
            return Ok((format!("Decrypt failed: {e}"), false));
        }
    };

    // Stream stdout → toast at e-ink cadence (stderr is inherited to the log).
    // `read_line` blocks; the picker is dedicated to this decrypt.
    if let Some(out) = child.stdout.take() {
        let mut reader = std::io::BufReader::new(out);
        let mut line = String::new();
        let mut last_draw = Instant::now();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            drain_decrypt_input(input, fb)?;
            let msg = line.trim();
            if msg.is_empty() {
                continue;
            }
            log(format!("kfxdedrm: {msg}"));
            if last_draw.elapsed() >= DL_REDRAW_INTERVAL {
                let dirty = toast::draw(fb, renderer, &format!("Decrypting {short}…\n{msg}"));
                fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                last_draw = Instant::now();
            }
        }
    }

    let status = child.wait();
    let out_path = &book.out_path;
    log(format!(
        "kfxdedrm exit={status:?}; {} exists={}",
        out_path.display(),
        out_path.exists()
    ));
    if !matches!(status, Ok(ref s) if s.success()) {
        return Ok(("Decrypt failed — see log".to_string(), false));
    }

    // Decrypt done → auto-push the fresh output to the desktop over the LAN
    // (best effort). The decrypt already succeeded, so the tile hides either way;
    if !out_path.exists() {
        return Ok(("Decrypted (tap Sync to send)".to_string(), true));
    }
    let dirty = toast::draw(fb, renderer, &format!("Syncing {short}…"));
    fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
    let msg = match api::push_book(agent, cfg, out_path) {
        Ok(outcome) => {
            // Confirmed on the desktop → remove both on-device leftovers (this
            for (path, err) in dedrm::cleanup_synced(&book.path) {
                log(format!("cleanup {}: {err}", path.display()));
            }
            match outcome {
                api::BookPush::Imported => "Decrypted → synced to library".to_string(),
                api::BookPush::Duplicate => "Decrypted → already in library".to_string(),
            }
        }
        Err(err) => {
            log(format!("auto-push failed: {err}"));
            "Decrypted (tap Sync to send)".to_string()
        }
    };
    // A gesture during the push queued behind the blocking send — capture it
    // while the Syncing toast is still the live screen.
    drain_decrypt_input(input, fb)?;
    Ok((msg, true))
}

/// Decrypt every on-device DRM purchase in `books` and push each result to the
fn decrypt_all_flow(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    input: &mut Input,
    agent: &ureq::Agent,
    cfg: &config::ServerConfig,
    books: &[dedrm::DrmBook],
) -> anyhow::Result<String> {
    let total = books.len();
    let Some(exe) = dedrm::probe_exe() else {
        return Ok("No working kfxdedrm binary".to_string());
    };
    let t0 = Instant::now();
    let (mut decrypted, mut synced, mut failed) = (0u32, 0u32, 0u32);
    let mut token_bad = false;
    // Set by a completed tap on the Stop button; `stop_armed` carries the
    // Down half of that tap between events.
    let (mut stopping, mut stop_armed) = (false, false);
    for (i, book) in books.iter().enumerate() {
        // Draw progress before the book so the bar reflects completed work while
        // the title names the one now in flight.
        let short = truncate_title(&book.book.title, 28);
        let (rect, stop_rect) =
            toast::draw_progress_stop(fb, renderer, &format!("Decrypting {short}…"), i, total);
        fb.send_update(rect, WAVEFORM_MODE_GC16)?;

        // Spawn + wait-poll rather than a blocking `status()`, so input stays
        let status = match Command::new(&exe).arg("dedrm").arg(&book.path).spawn() {
            Ok(mut child) => loop {
                match child.try_wait() {
                    Ok(Some(s)) => break Ok(s),
                    Ok(None) => {}
                    Err(e) => break Err(e),
                }
                match input.next_deadline(Some(Instant::now() + DEDRM_WAIT_POLL))? {
                    InputEvent::Tick => {}
                    // Once stopped the banner is redrawn without the button, so
                    // there is nothing left on screen to arm.
                    ev if stopping => decrypt_input_event(fb, ev),
                    ev => {
                        if decrypt_all_stop_tap(fb, ev, &stop_rect, &mut stop_armed) {
                            stopping = true;
                            log("decrypt-all: stop requested");
                            // Dropping the button is what marks the tap as taken;
                            // the label says what the batch is now doing.
                            let rect = toast::draw_progress(
                                fb,
                                renderer,
                                "Stopping after this book…",
                                i,
                                total,
                            );
                            fb.send_update(rect, WAVEFORM_MODE_GC16)?;
                        }
                    }
                }
            },
            Err(e) => Err(e),
        };
        let out_path = &book.out_path;
        log(format!(
            "decrypt-all {}/{}: {} exit={status:?} out_exists={}",
            i + 1,
            total,
            book.book.title,
            out_path.exists()
        ));
        // Counted either way, then one exit check for both — a failure must not
        // duck out of the loop body early, or a stop requested during a book
        // that then failed would be dropped and the batch would run on.
        if matches!(status, Ok(ref s) if s.success()) {
            decrypted += 1;

            // Push the fresh output (best effort; skipped once the token is known bad).
            if out_path.exists() && !token_bad {
                match api::push_book(agent, cfg, out_path) {
                    Ok(api::BookPush::Imported | api::BookPush::Duplicate) => {
                        synced += 1;
                        // Confirmed on the desktop → drop this book's leftovers (its
                        // output plus the encrypted book and its `.sdr` under Items01),
                        // scoped to this name. Best effort — failures are just logged.
                        for (path, err) in dedrm::cleanup_synced(&book.path) {
                            log(format!("decrypt-all cleanup {}: {err}", path.display()));
                        }
                    }
                    Err(api::SidleError::TokenMismatch) => {
                        log("decrypt-all: token rejected — pausing pushes, still decrypting");
                        token_bad = true;
                    }
                    Err(api::SidleError::Other(err)) => {
                        log(format!("decrypt-all push {}: {err:#}", out_path.display()));
                    }
                }
                // A gesture during the push queued behind the blocking send —
                // capture it while this book's bar is still the live screen. A
                // Stop tap among them still counts: the loop has not moved on yet.
                while let Some(ev) = input.poll_now()? {
                    if stopping {
                        decrypt_input_event(fb, ev);
                    } else if decrypt_all_stop_tap(fb, ev, &stop_rect, &mut stop_armed) {
                        stopping = true;
                        log("decrypt-all: stop requested during push");
                    }
                }
            }
        } else {
            failed += 1;
        }

        if stopping {
            break;
        }
    }
    // Settle the bar where the batch got to — `total` when it ran out, short of
    let ran = (decrypted + failed) as usize;
    let rect = toast::draw_progress(fb, renderer, "Done", ran, total);
    fb.send_update(rect, WAVEFORM_MODE_GC16)?;
    log(format!(
        "decrypt-all done in {:?}: {decrypted} decrypted, {synced} synced, {failed} failed, {} left, token_bad={token_bad}",
        t0.elapsed(),
        total - ran
    ));

    Ok(decrypt_all_summary(
        decrypted,
        synced,
        failed,
        total - ran,
        token_bad,
    ))
}

/// One input event arriving while a decrypt-all step owns the screen, answering
fn decrypt_all_stop_tap(
    fb: &mut Framebuffer,
    ev: InputEvent,
    stop_rect: &MxcfbRect,
    armed: &mut bool,
) -> bool {
    match ev {
        InputEvent::Touch(TouchEvent::Down { x, y }) => {
            *armed = rect_hit(stop_rect, x, y);
            false
        }
        InputEvent::Touch(TouchEvent::Up { x, y }) => {
            let fired = *armed && rect_hit(stop_rect, x, y);
            *armed = false;
            fired
        }
        ev => {
            decrypt_input_event(fb, ev);
            false
        }
    }
}

/// The summary toast [`decrypt_all_flow`] ends on. `left` counts the books a
/// stop skipped, and is what tells the user the batch ended early rather than
/// running out.
fn decrypt_all_summary(
    decrypted: u32,
    synced: u32,
    failed: u32,
    left: usize,
    token_bad: bool,
) -> String {
    let head = if token_bad {
        format!("Decrypted {decrypted}; sync blocked — plug into sidle, Update on Kindle")
    } else {
        format!("Decrypted {decrypted}, synced {synced}, {failed} failed")
    };
    if left == 0 {
        head
    } else {
        format!("{head}\nStopped, {left} left")
    }
}

/// Download a book to `/mnt/us/documents/Sidle/<filename>` while showing a live
fn download_flow(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    input: &mut Input,
    agent: &ureq::Agent,
    cfg: &config::ServerConfig,
    book: &api::Book,
) -> anyhow::Result<(String, bool)> {
    // Paint the overlay before the GET so a long-press gets instant feedback:
    let title = format!("Downloading {}…", truncate_title(&book.title, 32));
    let (rect, _) = toast::draw_download(fb, renderer, &title, "Connecting…");
    fb.send_update(rect, WAVEFORM_MODE_GC16)?;

    let dl = match api::download_book(agent, cfg, book) {
        Ok(dl) => dl,
        Err(api::SidleError::TokenMismatch) => {
            log("token rejected during download — resync via sidle desktop app");
            return Ok((
                "Token mismatch.\nPlug Kindle into sidle and click Update on Kindle.".to_string(),
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
    // Cancel needs a *full tap* (Down then Up in the button), not just an Up:
    let mut cancel_armed = false;
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
                // A Down inside Cancel arms it; the matching Up inside Cancel then
                // aborts. A Down elsewhere (or the holding finger's Up, which had
                // no Down here) leaves it disarmed.
                InputEvent::Touch(TouchEvent::Down { x, y }) => {
                    cancel_armed = rect_hit(&cancel_rect, x, y);
                }
                InputEvent::Touch(TouchEvent::Up { x, y })
                    if cancel_armed && rect_hit(&cancel_rect, x, y) =>
                {
                    cleanup_part(file, &part);
                    log(format!("download cancelled by user after {written} bytes"));
                    return Ok(("Download cancelled".to_string(), false));
                }
                InputEvent::Touch(TouchEvent::Up { .. }) => {
                    cancel_armed = false;
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
    // Baseline the rev we just wrote, so the Sync tap's update pass won't re-pull
    // this book until the desktop actually reconverts it (bumping `kfx_rev`).
    updates::record_download(Path::new(SYNCED_REVS_PATH), safe_name, book.kfx_rev);
    log(format!("downloaded {written} bytes to {}", path.display()));
    // Land whatever the library already knows about this book — highlights,
    match api::pull_sidecar(agent, cfg, book.id, Path::new(DOWNLOAD_DIR), safe_name) {
        Ok(true) => log("sidecar written with the download"),
        Ok(false) => {}
        Err(e) => log(format!("sidecar not written: {e:#}")),
    }
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
        let _ = std::fs::create_dir_all(LOG_DIR);
        LOG_PATH
    } else {
        "./sidle-native.log"
    };
    // File only — NOT also stderr. `sidle.sh` runs the binary with `2>> "$LOG"`
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Append a line to the dedicated LAN self-update log, so the update trail isn't
fn update_log(line: impl AsRef<str>) {
    let line = line.as_ref();
    let path = if std::path::Path::new("/mnt/us").is_dir() {
        let _ = std::fs::create_dir_all(LOG_DIR);
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
    use super::decrypt_all_summary;

    #[test]
    fn decrypt_all_summary_reports_a_stop() {
        // Ran to the end: no stop clause.
        assert_eq!(
            decrypt_all_summary(3, 3, 0, 0, false),
            "Decrypted 3, synced 3, 0 failed"
        );
        // Stopped: the skipped count is what says it ended early.
        assert_eq!(
            decrypt_all_summary(2, 2, 1, 5, false),
            "Decrypted 2, synced 2, 1 failed\nStopped, 5 left"
        );
        // A bad token replaces the sync count, and still reports the stop.
        assert_eq!(
            decrypt_all_summary(2, 0, 0, 4, true),
            "Decrypted 2; sync blocked — plug into sidle, Update on Kindle\nStopped, 4 left"
        );
    }

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
