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
mod keyboard;
mod lipc;
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

/// Directory holding [`LOG_PATH`] and [`UPDATE_LOG_PATH`].
const LOG_DIR: &str = "/mnt/us/logs";
const LOG_PATH: &str = "/mnt/us/logs/sidle-native.log";
/// Path [`update_log`] writes.
const UPDATE_LOG_PATH: &str = "/mnt/us/logs/sidle-update.log";
const CONFIG_PATH: &str = "/mnt/us/extensions/sidle/etc/server.conf";
/// Body type size in design pixels at `ui::scale::DESIGN_DPI`, scaled by [`font_px`].
const FONT_PX: f32 = 28.0;
/// Design pixels above the grid, scaled by [`top_margin`] for `grid::Layout::compute`.
const TOP_MARGIN: u32 = 190;

/// The body type size on an `xres`-wide panel.
fn font_px(xres: u32) -> f32 {
    ui::scale::Scale::of_width(xres).font(FONT_PX)
}

/// The margin above the grid on an `xres`-wide panel.
fn top_margin(xres: u32) -> u32 {
    ui::scale::Scale::of_width(xres).u(TOP_MARGIN)
}
/// Directory holding every downloaded `.kfx` and its `.sdr`.
const DOWNLOAD_DIR: &str = "/mnt/us/documents/Sidle";
/// Root [`api::push_misc`] scans.
const MNT_US: &str = "/mnt/us";
/// Directory holding pen ink, read by [`handwriting`].
const NOTEBOOKS_DIR: &str = "/mnt/us/.notebooks";
/// Cover cache [`cover_cache`] reads and writes.
const COVER_CACHE_DIR: &str = "/mnt/us/extensions/sidle/cache/covers";
/// `updates::Revs` store: `Book::kfx_rev` per on-device filename.
const SYNCED_REVS_PATH: &str = "/mnt/us/extensions/sidle/cache/synced_revs.json";
const CLEANINDEX: &str = "/mnt/us/system/.cleanindex";
const TOAST_LINGER: Duration = Duration::from_millis(1200);
/// Hold time within [`ARM_SLOP_PX`] that arms a tile.
const ARM_THRESHOLD: Duration = Duration::from_millis(1000);
/// Drift on either axis, in user-visible px, that counts as a hold.
const ARM_SLOP_PX: u32 = 40;
/// Dwell between the armed cue and the action overlay.
const ARM_DWELL: Duration = Duration::from_millis(250);

/// Per-read socket timeout for the session agent.
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Read-buffer size for streaming a book to disk.
const DL_CHUNK: usize = 256 * 1024;
/// Minimum wall-clock between progress redraws.
const DL_REDRAW_INTERVAL: Duration = Duration::from_millis(700);
/// How often the decrypt-all wait loop re-checks the engine child for exit.
const DEDRM_WAIT_POLL: Duration = Duration::from_millis(50);

/// Cell outlined and awaiting release.
struct Armed {
    /// Index of the outlined tile into the current `cells` view.
    cell_idx: usize,
    down_at: Instant,
}

/// Selects between the LAN library and [`dedrm::DrmBook`] entries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Library,
    Drm,
}

/// `drm` when `source` is [`Source::Drm`], else `None`.
fn drm_slice(source: Source, drm: &[dedrm::DrmBook]) -> Option<&[dedrm::DrmBook]> {
    matches!(source, Source::Drm).then_some(drm)
}

fn main() {
    // `--version`/`-V`: print `CARGO_PKG_VERSION` and exit.
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("sidle {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // `--probe-x` and `--x11-poc` bypass the framebuffer and config setup below.
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
    // `--archive-daemon`: run [`archive_once`] every `ARCHIVE_INTERVAL`, forever.
    if std::env::args().any(|a| a == readinglog::DAEMON_FLAG) {
        readinglog::claim_archiver();
        log("archive daemon started");
        loop {
            archive_once(false);
            std::thread::sleep(readinglog::ARCHIVE_INTERVAL);
        }
    }
    // `--archive`: one [`archive_once`] pass.
    if std::env::args().any(|a| a == "--archive") {
        archive_once(true);
        return;
    }
    // `--update`: [`run_update`] as a standalone launch.
    if std::env::args().any(|a| a == "--update") {
        let result = run_update();
        update_log(format!("--update done: {result:?}"));
        return;
    }
    // `start_archiver` detaches: [`run`] below never waits on the first pass.
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

/// Collect every reading event past `readinglog::archive_watermark` and archive it.
fn archive_once(verbose: bool) {
    let us = std::path::Path::new(MNT_US);
    // An empty `seen` leaves `archive_watermark` the only cutoff.
    let found = readinglog::collect(us, &readinglog::archive_watermark(us), &[]);
    match readinglog::archive(us, &found.lines) {
        Ok(Some(name)) => log(format!("archived {} lines → {name}", found.lines.len())),
        Ok(None) if verbose => log("archive: nothing new"),
        Ok(None) => {}
        Err(e) => log(format!("archive failed: {e}")),
    }
}

/// White-fill, draw `message` centered, then one full-screen GC16 refresh.
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
        // `cfg.host` below serves this run.
        Err(e) => log(&format!("search: found {host} but {CONFIG_PATH}: {e:#}")),
        Ok(()) => log(&format!("search: {CONFIG_PATH} now points at {host}")),
    }
    cfg.host = host;
    true
}

/// One banner line per outcome in a [`selfupdate::run_pull`] result.
/// [`update_log`] takes a hard error whole.
fn update_result_message(result: api::Result<selfupdate::UpdateReport>) -> String {
    let r = match result {
        Ok(r) => r,
        // The wording `diag` shows for the same error.
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

/// Run [`selfupdate::run_pull`] on its own framebuffer, reporting to [`update_log`].
fn run_update() -> anyhow::Result<()> {
    update_log("=== LAN self-update (--update): start ===");
    update_log(format!("argv: {:?}", std::env::args().collect::<Vec<_>>()));
    let mut cfg = config::load(Path::new(CONFIG_PATH))?;
    update_log(format!("server: https://{}:{}", cfg.host, cfg.port));
    // A missing or unusable `api::CA_PATH` ends `run_update` here.
    let agent = api::build_agent(|c| c).map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut renderer = TextRenderer::load(FONT_PX)?;
    let orient = orientation::Orientation::detect();
    let mut fb = Framebuffer::open()?;
    // `font_px` takes the width `fb.var` states.
    renderer.set_px(font_px(fb.var.xres));
    let touch = Touch::open(orient, fb.var.xres, fb.var.yres)?;
    let buttons = Buttons::open().ok().flatten();
    let mut input = Input::new(touch, buttons);
    input.set_orientation(orient);

    draw_panel(&mut fb, &mut renderer, "Checking for update…")?;

    // `selfupdate::run_pull` below reads `cfg.host`.
    if relocate_server(&mut fb, &mut renderer, &agent, &mut cfg, &|m| update_log(m)) {
        draw_panel(&mut fb, &mut renderer, "Checking for update…")?;
    }

    let message = update_result_message(selfupdate::run_pull(
        &agent,
        &cfg,
        Path::new(MNT_US),
        selfupdate::self_build_ts(),
        |m| update_log(m),
    ));
    update_log(format!("result: {message}"));

    // Result panel, then block until a tap or page button.
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

    // One agent for the whole session, keep-alive across list, covers and downloads.
    let agent = api::build_agent(|c| c.timeout_recv_body(Some(SOCKET_READ_TIMEOUT)))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let cache_dir = Path::new(COVER_CACHE_DIR);

    let mut renderer = TextRenderer::load(FONT_PX)?;
    // The faces `TextRenderer::load` found on this device.
    log(format!("fonts: {}", renderer.chain_description()));

    let orient = orientation::Orientation::detect();
    log(format!("orientation: {orient:?}"));

    let mut fb = Framebuffer::open()?;
    // `font_px` takes the width `fb.var` states.
    renderer.set_px(font_px(fb.var.xres));
    log(format!(
        "scale: {} dpi, body {}px",
        ui::scale::Scale::of_width(fb.var.xres).dpi(),
        font_px(fb.var.xres)
    ));
    let touch = Touch::open(orient, fb.var.xres, fb.var.yres)?;
    // `Buttons::open` takes the `gpio-keys` evdev device, apart from `Touch`.
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
    // `current_orient` below compares against this on every `InputEvent::Tick`.
    input.set_orientation(orient);
    let mut current_orient = orient;

    // `diag::run` handles a failed fetch; its Retry re-enters the loop.
    let t0 = Instant::now();
    let books = loop {
        match api::list_books(&agent, &cfg) {
            Ok(b) => break b,
            Err(err) => {
                log(format!("list_books failed: {err}"));
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

    // `downloaded` holds the sha8 of each on-device filename.
    let downloaded = device_state::scan_downloaded_shas(Path::new(DOWNLOAD_DIR));
    // `mut`: a mid-session download removes its book from this set.
    let mut all_books: Vec<api::Book> = books
        .iter()
        .filter(|b| match b.kfx_sha256.as_deref() {
            Some(sha) if sha.len() >= 8 => !downloaded.contains(&sha[..8]),
            _ => true,
        })
        .cloned()
        .collect();

    // `lib_stash` holds `all_books` while `source` is `Source::Drm`.
    let mut source = Source::Library;
    let mut drm_books: Vec<dedrm::DrmBook> = Vec::new();
    let mut lib_stash: Vec<api::Book> = Vec::new();

    let mut sort = SortState::default();
    let mut filters = Filters::default();
    // Romaji query from `ui::keyboard`, read by `rebuild_view`.
    let mut query = String::new();
    let mut entries = series::group_by_series(rebuild_view(&all_books, &filters, sort, &query));
    let mut series_view: Option<String> = None;
    let mut cells = series::cells_for_top(&entries);

    // `layout.page_size` counts the cells this panel fits.
    let mut layout = grid::Layout::compute(
        fb.var.xres,
        fb.var.yres,
        top_margin(fb.var.xres),
        pager::strip_h(fb.var.xres),
    );
    log(format!(
        "grid: {}x{} cells of {}x{} ({} per page)",
        layout.cols,
        layout.rows,
        grid::cell_w(fb.var.xres),
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

    // `covers` is parallel to `cells`, filled a page at a time.
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
    // Landing point of the current touch, set on Down and cleared on Up.
    let mut down_pos: Option<(u32, u32)> = None;
    loop {
        // A held `CellKind::Book` wakes `next_deadline` at `ARM_THRESHOLD`.
        let deadline = match armed.as_ref() {
            Some(a) if matches!(cells.get(a.cell_idx).map(|c| &c.kind), Some(CellKind::Book)) => {
                Some(a.down_at + ARM_THRESHOLD)
            }
            _ => None,
        };
        // `fb.raw_fd` carries `Expose`, cover and resize events.
        input.watch([Some(fb.raw_fd()), None]);
        let event = input.next_deadline(deadline)?;

        match event {
            InputEvent::Touch(TouchEvent::Down { x, y }) => {
                log(format!("down: ({x},{y})"));
                down_pos = Some((x, y));
                // `layout.cell_at_tap` misses the strip and the margins, which never arm.
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
                                width: grid::cell_w(fb.var.xres),
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

                // `classify_swipe` turns a horizontal swipe into a page flip.
                if let Some(dir) = down_pos
                    .take()
                    .and_then(|(x0, y0)| classify_swipe(x0, y0, x, y, fb.var.xres))
                {
                    // `had_armed` forces the repaint at a page boundary, clearing the outline.
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
                    // `drill_target` owns its name, freeing the borrow on `cells`.
                    let drill_target = match &cells[a.cell_idx].kind {
                        CellKind::Series { name, .. } => Some(name.clone()),
                        CellKind::Book => None,
                    };
                    if let Some(name) = drill_target {
                        log(format!("drill into series: {name}"));
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
                    // `CellKind::Book` released short of `ARM_THRESHOLD`.
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
                    // Clear the toast and the cell outline.
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

                // `ui::searchbar` is drawn only when `series_view` is None.
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
                    // `Tap::Update` and `Tap::Sync` run inline, leaving `query` and `cells` alone.
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
                                // `draw_panel` clears the taller progress banner whole.
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
                            // `source` picks between an annotation push and a decrypted-book push.
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
                                    // `pen` feeds both `push_annotations` and `push_notebooks`.
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
                                            let rl_t0 = Instant::now();
                                            // `archive_once` runs before `push_reading_log` reads the archive.
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
                                            {
                                                // `fresh` carries a reconvert made during this run; `books` is the fallback.
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
                        // `Tap::Open` and `Tap::Clear` are the only arms reaching here.
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

                // With `armed` empty, `pager::hit` resolves the Up to a strip action.
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
                            // `filtermenu::run` blocks until its overlay closes.
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
                            // `filtermenu::run` painted over the grid.
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
                            // Toggle `source` between the LAN library and `dedrm` books.
                            log("source-button tap");
                            let before = source;
                            match source {
                                Source::Library => {
                                    // `dedrm::available` and a non-empty `drm_books` both gate the switch.
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
                                            // `lib_stash` holds `all_books` for the switch back.
                                            lib_stash = std::mem::take(&mut all_books);
                                            all_books =
                                                drm_books.iter().map(|d| d.book.clone()).collect();
                                            source = Source::Drm;
                                            log(format!("→ DRM source: {} books", all_books.len()));
                                        }
                                    }
                                }
                                Source::Drm => {
                                    all_books = std::mem::take(&mut lib_stash);
                                    source = Source::Library;
                                    log(format!("→ Library source: {} books", all_books.len()));
                                }
                            }
                            // `sort` carries across the switch; `query` and `filters` reset.
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
                            // A toast or the switch painted over the grid.
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
                // Clearing `armed` and `down_pos` makes the suppressed lift inert.
                armed = None;
                down_pos = None;
                match eink::screenshot::capture(&mut fb) {
                    Ok(p) => log(format!("screenshot saved: {}", p.display())),
                    Err(e) => log(format!("screenshot failed: {e:#}")),
                }
            }
            InputEvent::Page(pb) => {
                log(format!("page button: {pb:?}"));
                // `pb` carries the orientation standing before this read.
                if input.follow_orientation_now() {
                    current_orient = input.orientation();
                    continue;
                }
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
                // `fb.raw_fd` wakes this poll, and a full queue wakes it again at once.
                let pump = fb.pump_events();
                // `input.retake` reclaims the devices a covering window took.
                if let Some(covered) = pump.covered {
                    log(format!("x11: covered={covered}"));
                    input.set_covered(covered);
                    down_pos = None;
                    armed = None;
                }
                input.retake();
                // `fb.var` carries the size `pump.resized` reports.
                if pump.resized.is_some() {
                    layout = grid::Layout::compute(
                        fb.var.xres,
                        fb.var.yres,
                        top_margin(fb.var.xres),
                        pager::strip_h(fb.var.xres),
                    );
                    total_pages = pager::n_pages(cells.len(), layout.page_size());
                    page = page.min(total_pages.saturating_sub(1));
                    log(format!(
                        "grid: {}x{} cells ({} per page, {total_pages} pages)",
                        layout.cols,
                        layout.rows,
                        layout.page_size()
                    ));
                }
                let arm_ready = match armed.as_ref() {
                    Some(a) => {
                        matches!(cells.get(a.cell_idx).map(|c| &c.kind), Some(CellKind::Book))
                            && a.down_at.elapsed() >= ARM_THRESHOLD
                    }
                    None => false,
                };
                if arm_ready {
                    let a = armed.take().unwrap();
                    // A touch past `ARM_SLOP_PX` from `down_pos` cancels the arm.
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
                        // `draw_arm_cue` gets `ARM_DWELL` on screen ahead of the action overlay.
                        let cell_pos = a.cell_idx.saturating_sub(page * layout.page_size());
                        let (cx, cy) = layout.cell_xy(cell_pos);
                        if cx >= 0 && cy >= 0 {
                            grid::draw_arm_cue(&mut fb, cx, cy, layout.cell_h);
                            fb.send_update(
                                MxcfbRect {
                                    top: cy as u32,
                                    left: cx as u32,
                                    width: grid::cell_w(fb.var.xres),
                                    height: layout.cell_h,
                                },
                                WAVEFORM_MODE_DU,
                            )?;
                            thread::sleep(ARM_DWELL);
                        }
                        let book = &cells[a.cell_idx].cover_book;
                        // `dl_id` outlives the `cells` borrow the rebuild below reassigns.
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
                        // `source` picks the banner `banner_msg` is drawn into.
                        let dirty = match source {
                            Source::Drm => toast::draw(&mut fb, &mut renderer, &banner_msg),
                            Source::Library => {
                                toast::draw_download_done(&mut fb, &mut renderer, &banner_msg)
                            }
                        };
                        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                        thread::sleep(TOAST_LINGER);
                        // Drop `dl_id` from `all_books` and re-derive `entries` and `cells`.
                        if saved {
                            all_books.retain(|b| b.id != dl_id);
                            entries = series::group_by_series(rebuild_view(
                                &all_books, &filters, sort, &query,
                            ));
                            let drilled = series_view.clone();
                            cells = match drilled {
                                Some(name) => match series::members_of(&entries, &name) {
                                    Some(members) => series::cells_for_series(members),
                                    // `dl_id` was the series' last member.
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
                        // An empty `down_pos` keeps the holding finger's lift out of `classify_swipe`.
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
                    let turned = input.follow_orientation();
                    current_orient = input.orientation();
                    // `fb.covered` holds the repaint back while a window is in front.
                    let damaged =
                        pump.repaint || pump.resized.is_some() || pump.covered == Some(false);
                    if !fb.covered() && (turned || damaged) {
                        if !turned {
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
                // `armed` below `ARM_THRESHOLD` falls through and keeps polling.
            }
        }
    }
    Ok(())
}

/// `all_books` narrowed by `query` and `filters`, ordered by `sort`.
fn rebuild_view(
    all_books: &[api::Book],
    filters: &Filters,
    sort: SortState,
    query: &str,
) -> Vec<api::Book> {
    // `query` and `filters` are ANDed; `search_key` carries the series romaji.

    let mut view: Vec<api::Book> = all_books
        .iter()
        .filter(|b| filter::matches(b, filters, None) && search::matches(b, query))
        .cloned()
        .collect();
    sort.apply(&mut view);
    view
}

/// Draw one page of `cells`, the header and the strip, then one full GC16 refresh.
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

    // `drilled` drops the `searchbar` row, leaving the header alone.
    let hbaseline = if drilled {
        layout.top * 60 / 100
    } else {
        searchbar::draw(fb, renderer, query, true);
        searchbar::draw_buttons(fb, drm_active);
        searchbar::below(fb.var.xres) as i32 + renderer.line_height() as i32
    };
    // `wrap_and_clamp` holds `header` to one line.
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
        // `cover_book.language` picks the script for both cell kinds.
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
    // `pager::draw` carries Exit, and draws prev/next above one page.
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

/// Draw the current page of `cells`, then fill its `covers`.
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

/// Fetch every `covers[start..end]` entry at `None` and paint it as it arrives.
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
        // `cover_book` is the cell's own book or its series' lead member.
        // Under `drm`, `book.id` indexes `drm_books`.
        let book = &cells[idx].cover_book;
        let img = match drm {
            Some(drm_books) => drm_books
                .get(book.id as usize)
                .and_then(|d| d.cover_path.as_deref())
                .and_then(|path| dedrm_cover(path, fb.var.xres)),
            None => load_cover(agent, cfg, cache_dir, book, fb.var.xres),
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
                        width: grid::cell_w(fb.var.xres),
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

/// Decode the device thumbnail at `path`, or `None` when missing or undecodable.
fn dedrm_cover(path: &Path, xres: u32) -> Option<DynamicImage> {
    let bytes = std::fs::read(path).ok()?;
    grid::decode_resize(&bytes, xres).ok()
}

/// Decode `book`'s cover from `cache_dir`, else fetch it and write it through.
fn load_cover(
    agent: &ureq::Agent,
    cfg: &config::ServerConfig,
    cache_dir: &Path,
    book: &api::Book,
    xres: u32,
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
    match grid::decode_resize(&bytes, xres) {
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

/// The `asin` of every book in `books`, keying `handwriting::scan`.
fn library_asins(books: &[api::Book]) -> std::collections::HashSet<String> {
    books.iter().filter_map(|b| b.asin.clone()).collect()
}

/// `api::push_book` every path in `decrypted`, reported as one banner line.
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
                // `cleanup_synced` runs once the desktop confirms the push.
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

/// Handle one `ev` arriving while [`decrypt_flow`] owns the screen.
fn decrypt_input_event(fb: &mut Framebuffer, ev: InputEvent) {
    log(format!("decrypt input: {ev:?}"));
    if ev == InputEvent::Touch(TouchEvent::Screenshot) {
        match eink::screenshot::capture(fb) {
            Ok(p) => log(format!("screenshot saved: {}", p.display())),
            Err(e) => log(format!("screenshot failed: {e:#}")),
        }
    }
}

/// Drain every queued input event through [`decrypt_input_event`].
fn drain_decrypt_input(input: &mut Input, fb: &mut Framebuffer) -> anyhow::Result<()> {
    while let Some(ev) = input.poll_now()? {
        decrypt_input_event(fb, ev);
    }
    Ok(())
}

/// Run the `dedrm` engine over one purchase, reported as one banner line.
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

    // `read_line` blocks; `child` stderr is inherited by the log.
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

    if !out_path.exists() {
        return Ok(("Decrypted (tap Sync to send)".to_string(), true));
    }
    let dirty = toast::draw(fb, renderer, &format!("Syncing {short}…"));
    fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
    let msg = match api::push_book(agent, cfg, out_path) {
        Ok(outcome) => {
            // `cleanup_synced` runs once the desktop confirms the push.
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
    // Take the gestures queued behind the blocking `push_book`.
    drain_decrypt_input(input, fb)?;
    Ok((msg, true))
}

/// Run [`decrypt_flow`] over every `books` entry, interruptible between books.
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
    // `stop_armed` carries the Down half of a Stop tap between events.
    let (mut stopping, mut stop_armed) = (false, false);
    for (i, book) in books.iter().enumerate() {
        // `i` counts books finished; `short` names the one in flight.
        let short = truncate_title(&book.book.title, 28);
        let (rect, stop_rect) =
            toast::draw_progress_stop(fb, renderer, &format!("Decrypting {short}…"), i, total);
        fb.send_update(rect, WAVEFORM_MODE_GC16)?;

        // `try_wait` keeps `input` polled through the child's run.
        let status = match Command::new(&exe).arg("dedrm").arg(&book.path).spawn() {
            Ok(mut child) => loop {
                match child.try_wait() {
                    Ok(Some(s)) => break Ok(s),
                    Ok(None) => {}
                    Err(e) => break Err(e),
                }
                match input.next_deadline(Some(Instant::now() + DEDRM_WAIT_POLL))? {
                    InputEvent::Tick => {}
                    // With `stopping` set, `stop_rect` is off screen.
                    ev if stopping => decrypt_input_event(fb, ev),
                    ev => {
                        if decrypt_all_stop_tap(fb, ev, &stop_rect, &mut stop_armed) {
                            stopping = true;
                            log("decrypt-all: stop requested");
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
        // Both arms fall through to the one `stopping` check below.
        if matches!(status, Ok(ref s) if s.success()) {
            decrypted += 1;

            // `token_bad` skips every push past the first 401.
            if out_path.exists() && !token_bad {
                match api::push_book(agent, cfg, out_path) {
                    Ok(api::BookPush::Imported | api::BookPush::Duplicate) => {
                        synced += 1;
                        // `cleanup_synced` runs once the desktop confirms the push.
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
                // Take the gestures queued behind the blocking `push_book`, Stop included.
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
    // `ran` settles the bar at what the batch covered.
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

/// `true` for a completed tap on `stop_rect`; every other event reaches
/// [`decrypt_input_event`].
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

/// The summary [`decrypt_all_flow`] ends on. `left` counts the books a stop skipped.
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

/// Stream `book` into [`DOWNLOAD_DIR`] behind a live progress overlay.
fn download_flow(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
    input: &mut Input,
    agent: &ureq::Agent,
    cfg: &config::ServerConfig,
    book: &api::Book,
) -> anyhow::Result<(String, bool)> {
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
    // `file_name` drops any path component in `dl.filename`.
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
    // `cancel_armed` carries the Down half of a Cancel tap between events.
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

        // One `reader.read` can queue both contacts of a `TouchEvent::Screenshot`.
        while let Some(ev) = input.poll_now()? {
            log(format!("dl input (chunk {chunks}): {ev:?}"));
            match ev {
                InputEvent::Touch(TouchEvent::Screenshot) => {
                    match eink::screenshot::capture(fb) {
                        Ok(p) => log(format!("screenshot saved: {}", p.display())),
                        Err(e) => log(format!("screenshot failed: {e:#}")),
                    }
                    // `capture` restored the toast with a GC16 refresh.
                    last_draw = Instant::now();
                }
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
    // `expected` is what separates a clean EOF from a whole transfer.
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
    // `record_download` baselines `kfx_rev` for `updates::pull_updates`.
    updates::record_download(Path::new(SYNCED_REVS_PATH), safe_name, book.kfx_rev);
    log(format!("downloaded {written} bytes to {}", path.display()));
    // `pull_sidecar` writes beside `path` ahead of any reader opening it.
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

/// Close `file` and delete `part`.
fn cleanup_part(file: std::fs::File, part: &Path) {
    drop(file);
    let _ = std::fs::remove_file(part);
}

/// `written` against `total`, e.g. `"12.3 MB / 305.7 MB  (4%)"`.
/// `written` alone where `total` is `None`.
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
    // `log_path` takes `line`; stderr reaches the same file by redirect.
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Append `line` to [`UPDATE_LOG_PATH`].
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
        // `left` at 0 drops the stop clause.
        assert_eq!(
            decrypt_all_summary(3, 3, 0, 0, false),
            "Decrypted 3, synced 3, 0 failed"
        );
        // A non-zero `left` adds it.
        assert_eq!(
            decrypt_all_summary(2, 2, 1, 5, false),
            "Decrypted 2, synced 2, 1 failed\nStopped, 5 left"
        );
        // `token_bad` replaces the sync count.
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
        // A zero `total` takes the same fallback.
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
