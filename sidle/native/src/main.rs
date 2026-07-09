//! Sidle native — Milestone 9 paginated cover grid + download flow.
//!
//! 3×3 grid per page, prev/next bottom-strip controls when the library
//! overflows one page. Tap a cover → overlay "Downloading…" → stream
//! `.kfx` to `/mnt/us/documents/Sidle/<filename>` → `touch
//! /mnt/us/system/.cleanindex` → overlay "Downloaded" → restore gallery.

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
mod eink;
mod orientation;
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
/// USB-drive root — the base for the misc backup scan: screenshots live in
/// `screenshots/` (and the root itself on KOA2 stock firmware), KUAL logs at the
/// root. See [`api::push_misc`].
const MNT_US: &str = "/mnt/us";
/// On-device cover thumbnail cache, under the extension dir (not documents/,
/// so the stock indexer never sees it). See [`cover_cache`].
const COVER_CACHE_DIR: &str = "/mnt/us/extensions/sidle/cache/covers";
/// Records the KFX revision (`Book::kfx_rev`) last written for each on-device
/// file, so the Sync tap can re-pull a book the desktop reconverted — in place,
/// under its frozen filename. Under the extension dir, never in documents/.
/// See [`updates`].
const SYNCED_REVS_PATH: &str = "/mnt/us/extensions/sidle/cache/synced_revs.json";
const CLEANINDEX: &str = "/mnt/us/system/.cleanindex";
const TOAST_LINGER: Duration = Duration::from_millis(1200);
/// How long a finger must rest on a book cover — without drifting more than
/// [`ARM_SLOP_PX`] — before the tile "arms" and its action (download / decrypt)
/// auto-fires. The arm is signalled on the cell itself (see
/// [`grid::draw_arm_cue`]) at this instant and the action starts immediately, so
/// the user watches for the flip instead of timing a release: a hold that's "too
/// long" no longer wastes time (it fired the moment it armed) and one that's "too
/// short" is a visible non-event, not a silent misclick. Long enough to keep an
/// accidental brush from downloading; tune on hardware. Auto-fire removes the
/// over-hold cost, so this can drop toward the stock ~500ms if it feels sluggish.
const ARM_THRESHOLD: Duration = Duration::from_millis(1000);
/// Max drift (either axis, user-visible px) from the finger's landing point that
/// still counts as a hold. Past this the stroke is a drag / page-flip swipe in
/// progress, so the arm is cancelled and the eventual `Up` classifies the swipe.
const ARM_SLOP_PX: u32 = 40;
/// Dwell between painting the armed cue and letting the action overlay paint over
/// it — long enough that the slow e-ink panel actually presents the "armed" frame
/// (a partial refresh lands in a few hundred ms). Purely for perception; the
/// action is correct with it at 0.
const ARM_DWELL: Duration = Duration::from_millis(250);

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

/// Which library the picker is showing. `Library` is the LAN server library (the
/// default, download-a-book source); `Drm` is on-device purchased KFX books that
/// a tap decrypts via kfxdedrm (see [`dedrm`]). The bottom-strip Source button
/// toggles between them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Library,
    Drm,
}

/// The DRM view-models when the DRM source is active, else `None` — the value
/// threaded into `repaint_page`/`fetch_and_paint_page` so the cover seam loads
/// local thumbnails (and the strip labels the toggle) only in DRM mode. Keyed by
/// `book.id` = index in `drm_books` (see [`dedrm::DrmBook`]).
fn drm_slice(source: Source, drm: &[dedrm::DrmBook]) -> Option<&[dedrm::DrmBook]> {
    matches!(source, Source::Drm).then_some(drm)
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
    // `iter().cloned()` (not `into_iter`) so the full library survives as `books`
    // — the Sync tap's in-place update pass (`updates::pull_updates`) needs every
    // row's `kfx_rev` + `device_filename` to spot books the desktop reconverted.
    let mut all_books: Vec<api::Book> = books
        .iter()
        .filter(|b| match b.kfx_sha256.as_deref() {
            Some(sha) if sha.len() >= 8 => !downloaded.contains(&sha[..8]),
            _ => true,
        })
        .cloned()
        .collect();

    // Which source is showing (LAN library vs on-device DRM books) + the DRM
    // scan when active. `lib_stash` parks the library master while in DRM mode so
    // the toggle restores it — with any mid-session download-hides — instead of
    // re-listing the server. See the `PagerHit::Source` handler.
    let mut source = Source::Library;
    let mut drm_books: Vec<dedrm::DrmBook> = Vec::new();
    let mut lib_stash: Vec<api::Book> = Vec::new();

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
        drm_slice(source, &drm_books),
    )?;

    let mut armed: Option<Armed> = None;
    // Where the current touch landed (set on Down, cleared on Up) — lets the Up
    // handler tell a tap from a horizontal page-flip swipe. Independent of
    // `armed`, since a swipe can start anywhere, not just on a cover cell.
    let mut down_pos: Option<(u32, u32)> = None;
    loop {
        // While a *book* cell is held, wake the loop at the arm threshold so the
        // tile can flip to the armed cue and its action auto-fire — a `Tick`
        // otherwise never arrives during a hold (finger micro-jitter keeps `poll`
        // busy). Series cells have no threshold (they drill on release), so no
        // deadline is set for them.
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
                            drm_slice(source, &drm_books),
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
                                drm_slice(source, &drm_books),
                            )?;
                        }
                        continue;
                    }
                    // Book cell released. A hold long enough to act already
                    // auto-fired from the `Tick` arm (which took `armed` and
                    // cleared the state), so reaching here means the finger lifted
                    // *before* the arm threshold — a short tap. Show the discovery
                    // hint; without it a tap-trained user keeps tapping and wonders
                    // why nothing happens.
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
                        grid_left,
                        grid_top,
                        sort,
                        filters.active_facets(),
                        series_view.as_deref(),
                        &query,
                        drm_slice(source, &drm_books),
                    )?;
                    continue;
                }

                // Search bar (top level only — the bar isn't drawn when drilled).
                // Tap the field → on-screen keyboard; tap the `clear` zone → drop
                // the query. Either way re-filter (only if the query changed) and
                // repaint, since the keyboard overwrote the screen.
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
                                drm_slice(source, &drm_books),
                            )?;
                            continue;
                        }
                        ui::searchbar::Tap::DecryptAll => {
                            // DRM view's right disc (the library view's Update slot,
                            // useless while browsing DRM): decrypt every on-device
                            // purchase and push each to the desktop, behind an
                            // `n/total` progress bar. Then re-scan — each decrypted
                            // book now has a `.kfx-zip`, so `dedrm::scan` drops it and
                            // the DRM view collapses to just the ones left (or empties).
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
                                total_pages = pager::n_pages(cells.len());
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
                                grid_left,
                                grid_top,
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
                            // Either way the grid is unchanged — toast the report
                            // and repaint the page underneath.
                            log("sync-button tap");
                            let banner_msg = match source {
                                Source::Drm => {
                                    let zips = dedrm::decrypted_books();
                                    if zips.is_empty() {
                                        "No decrypted books to sync".to_string()
                                    } else {
                                        let dirty = toast::draw(
                                            &mut fb,
                                            &mut renderer,
                                            "Syncing decrypted books…",
                                        );
                                        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                                        sync_decrypted(&agent, &cfg, &zips)
                                    }
                                }
                                Source::Library => {
                                    let dirty = toast::draw(&mut fb, &mut renderer, "Syncing…");
                                    fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                                    let sync_t0 = Instant::now();
                                    match api::push_annotations(
                                        &agent,
                                        &cfg,
                                        std::path::Path::new(DOWNLOAD_DIR),
                                    ) {
                                        Ok(report) => {
                                            let mut summary = report.summary();
                                            log(format!(
                                                "annotation sync ok in {:?}: {summary}",
                                                sync_t0.elapsed()
                                            ));
                                            // Same Sync tap also backs up screenshots + KUAL
                                            // logs over WiFi. Best-effort: annotations already
                                            // landed, so a misc failure only adds a note — it
                                            // never turns the sync into a failure.
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
                                            // Same Sync tap pulls any book the desktop
                                            // reconverted since last time — in place, under
                                            // its frozen filename so the Kindle keeps the
                                            // book's `.sdr` (highlights + position). Automatic
                                            // upkeep; a failed update only adds a note.
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
                                            "Token mismatch.\nPlug Kindle into sidle and click Update KUAL."
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
                                grid_left,
                                grid_top,
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
                                drm_slice(source, &drm_books),
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
                                    grid_left,
                                    grid_top,
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
                                    grid_left,
                                    grid_top,
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
                                    // cheap dir check; the decrypt action re-probes
                                    // for a *working* ABI binary), then on ≥1
                                    // purchase present. Any miss toasts and stays.
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
                            // failed switch (not installed / empty) leaves the
                            // current view untouched. Fresh query + facets for the
                            // new set; the sort key carries over.
                            if source != before {
                                query.clear();
                                filters = Filters::default();
                                series_view = None;
                                entries = series::group_by_series(rebuild_view(
                                    &all_books, &filters, sort, &query,
                                ));
                                cells = series::cells_for_top(&entries);
                                total_pages = pager::n_pages(cells.len());
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
                                grid_left,
                                grid_top,
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
                        drm_slice(source, &drm_books),
                    )?;
                }
            }
            InputEvent::Tick => {
                // A Tick means one of two things now:
                //  (a) the arm deadline fired while a book cell was held past
                //      ARM_THRESHOLD — flip the tile to the armed cue and auto-fire
                //      its action, so the user never has to time a release; or
                //  (b) an ordinary idle poll with nothing armed — re-check the
                //      framework orientation.
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
                    // mid-drag (a slow page-flip swipe), not a hold — cancel the arm
                    // and clear its outline, keeping `down_pos` so the eventual Up
                    // can still classify the swipe out of the full stroke.
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
                            grid_left,
                            grid_top,
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
                        let cell_pos = a.cell_idx.saturating_sub(page * PAGE_SIZE);
                        let (cx, cy) = grid::cell_xy(grid_left, grid_top, cell_pos);
                        if cx >= 0 && cy >= 0 {
                            grid::draw_arm_cue(&mut fb, cx, cy);
                            fb.send_update(
                                MxcfbRect {
                                    top: cy as u32,
                                    left: cx as u32,
                                    width: grid::CELL_W,
                                    height: grid::CELL_H,
                                },
                                WAVEFORM_MODE_DU,
                            )?;
                            thread::sleep(ARM_DWELL);
                        }
                        // Auto-fire: act on the book — download it (library) or
                        // decrypt it in place (DRM). The finger is still down; its
                        // eventual lift is inert (`down_pos` is cleared below, and a
                        // lift on the grid maps to no Up action), and a lift landing
                        // in the download overlay's Cancel is ignored there — Cancel
                        // now needs a fresh tap (Down+Up in the button).
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
                                Some(drm_book) => {
                                    decrypt_flow(&mut fb, &mut renderer, &agent, &cfg, drm_book)
                                        .unwrap_or_else(|err| {
                                            log(format!("decrypt flow error: {err:#}"));
                                            (format!("Failed: {err}"), false)
                                        })
                                }
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
                        // toast, so a plain toast overwrites it cleanly. The library
                        // download's overlay is the taller `draw_download`
                        // (title/progress/Cancel), so its result reuses that
                        // footprint — one banner replacing the live one in place.
                        let dirty = match source {
                            Source::Drm => toast::draw(&mut fb, &mut renderer, &banner_msg),
                            Source::Library => {
                                toast::draw_download_done(&mut fb, &mut renderer, &banner_msg)
                            }
                        };
                        fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
                        thread::sleep(TOAST_LINGER);
                        // Hide the just-acted book: on the device now, and the
                        // picker's rule is "on device → not shown". Drop it from the
                        // master set and re-derive the current view (top level, or
                        // the drilled-in series' members) so the tile vanishes in the
                        // repaint below. Keep the user on their page, clamped if its
                        // last tile just left.
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
                            grid_left,
                            grid_top,
                            sort,
                            filters.active_facets(),
                            series_view.as_deref(),
                            &query,
                            drm_slice(source, &drm_books),
                        )?;
                    }
                } else if armed.is_none() {
                    // Idle poll. The X server rotates our window to the framework
                    // orientation but leaves it blank until we repaint, and raw
                    // touch/buttons don't follow the rotation. So on a detected
                    // flip: re-orient input, then repaint the current page (the X
                    // server rotates the repaint correctly, clearing the blank).
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
    drm_active: bool,
) -> anyhow::Result<()> {
    fb.fill_rect(0, 0, fb.var.xres, fb.var.yres, 0xFF);

    // Top chrome. At the top level: the search bar (the shared widget — same in
    // the keyboard overlay), then the sort header just below it. Drilled into a
    // series: no bar (search is a top-level action), just the series-name header.
    let hbaseline = if drilled {
        grid_top * 60 / 100
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
        grid_left,
        grid_top,
        &header,
        filter_count,
        drilled,
        query,
        drm.is_some(),
    )?;
    fetch_and_paint_page(
        fb, renderer, agent, cfg, cache_dir, cells, covers, page, grid_left, grid_top, drm,
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
    drm: Option<&[dedrm::DrmBook]>,
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
        // lead member — one fetch per collection, not one per member. In DRM mode
        // it's the book's local device thumbnail (keyed by `book.id` = drm index);
        // in library mode a LAN fetch + disk cache.
        let book = &cells[idx].cover_book;
        let img = match drm {
            Some(drm_books) => drm_books
                .get(book.id as usize)
                .and_then(|d| d.cover_path.as_deref())
                .and_then(dedrm_cover),
            None => load_cover(agent, cfg, cache_dir, book),
        };

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

/// Decode a DRM book's local device thumbnail into a grid image, or `None` if
/// it's missing/undecodable (cell stays a placeholder). The DRM cover seam's
/// local twin of [`load_cover`]'s LAN fetch — no network, no cache, since the
/// thumbnail is already a small file on the device.
fn dedrm_cover(path: &Path) -> Option<DynamicImage> {
    let bytes = std::fs::read(path).ok()?;
    grid::decode_resize(&bytes).ok()
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
/// Push every decrypted `.kfx-zip` to the desktop over the LAN, returning a
/// one-line toast summary. The manual DRM-mode Sync; server dedupe makes re-runs
/// safe (a re-push of an already-imported book is a `duplicate` no-op). A token
/// mismatch aborts early with the re-provision breadcrumb — every push would hit
/// the same wall.
fn sync_decrypted(
    agent: &ureq::Agent,
    cfg: &config::ServerConfig,
    zips: &[std::path::PathBuf],
) -> String {
    let (mut imported, mut dup, mut failed) = (0u32, 0u32, 0u32);
    for zip in zips {
        match api::push_book(agent, cfg, zip) {
            Ok(api::BookPush::Imported) => imported += 1,
            Ok(api::BookPush::Duplicate) => dup += 1,
            Err(api::SidleError::TokenMismatch) => {
                return "Token mismatch.\nPlug Kindle into sidle and click Update KUAL."
                    .to_string();
            }
            Err(api::SidleError::Other(err)) => {
                log(format!("sync {}: {err:#}", zip.display()));
                failed += 1;
            }
        }
    }
    format!("Synced: {imported} new, {dup} already, {failed} failed")
}

/// Decrypt one on-device DRM purchase in place via the kfxdedrm engine — the DRM
/// twin of [`download_flow`]. Probe the working ABI binary, spawn `<exe> dedrm
/// <kfx>`, stream its stdout to the toast, and report exit status. Returns the
/// toast message **and** whether it succeeded (`true` hides the tile, mirroring a
/// completed download).
///
/// No cancel — a single small book decrypts in seconds — and stderr is inherited
/// (it lands in `sidle.sh`'s log). The engine writes `<stem>.kfx-zip` under
/// [`dedrm::OUT_DIR`]; whether the file materialized is logged as a breadcrumb
/// (to confirm the assumed output name on-device) but success is the exit code,
/// so a divergent output name still reads as success rather than a false failure.
fn decrypt_flow(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
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
        .arg(&book.kfx_path)
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
    let out_path = dedrm::out_path(&book.kfx_path);
    log(format!(
        "kfxdedrm exit={status:?}; {} exists={}",
        out_path.display(),
        out_path.exists()
    ));
    if !matches!(status, Ok(ref s) if s.success()) {
        return Ok(("Decrypt failed — see log".to_string(), false));
    }

    // Decrypt done → auto-push the fresh .kfx-zip to the desktop over the LAN
    // (best effort). The decrypt already succeeded, so the tile hides either way;
    // a failed push is caught by the DRM-mode Sync button, which re-pushes every
    // dedrm/*.kfx-zip. If the assumed output name isn't on disk (a divergent
    // kfxdedrm name), don't guess which file is the fresh one — leave it for Sync.
    if !out_path.exists() {
        return Ok(("Decrypted (tap Sync to send)".to_string(), true));
    }
    let dirty = toast::draw(fb, renderer, &format!("Syncing {short}…"));
    fb.send_update(dirty, WAVEFORM_MODE_GC16)?;
    let msg = match api::push_book(agent, cfg, &out_path) {
        Ok(api::BookPush::Imported) => "Decrypted → synced to library".to_string(),
        Ok(api::BookPush::Duplicate) => "Decrypted → already in library".to_string(),
        Err(err) => {
            log(format!("auto-push failed: {err}"));
            "Decrypted (tap Sync to send)".to_string()
        }
    };
    Ok((msg, true))
}

/// Decrypt every on-device DRM purchase in `books` and push each result to the
/// desktop — the batch twin of [`decrypt_flow`], run from the DRM view's right
/// action button (the slot the library view gives to self-update). Steps through
/// the list behind a [`toast::draw_progress`] `n / total` bar that advances one
/// book at a time. The ABI binary is probed once up front. Each book's decrypt
/// is a blocking `<exe> dedrm <kfx>` with stdio inherited (it lands in
/// `sidle.sh`'s log — no pipe to drain, unlike the single-book streaming flow),
/// then the resulting `.kfx-zip` is pushed over the LAN. Push is best-effort: an
/// `Other` error is logged and the book still counts as decrypted; a token
/// mismatch stops further pushes (every one would hit the same wall) but
/// decryption continues — a decrypted book is still useful and re-pushable via
/// the DRM Sync button. Returns the summary toast; the caller re-scans to drop
/// the now-decrypted tiles.
fn decrypt_all_flow(
    fb: &mut Framebuffer,
    renderer: &mut TextRenderer,
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
    for (i, book) in books.iter().enumerate() {
        // Draw progress before the book so the bar reflects completed work while
        // the title names the one now in flight.
        let short = truncate_title(&book.book.title, 28);
        let rect = toast::draw_progress(fb, renderer, &format!("Decrypting {short}…"), i, total);
        fb.send_update(rect, WAVEFORM_MODE_GC16)?;

        // Blocking decrypt with inherited stdio — no pipe to drain (the single
        // `decrypt_flow` pipes stdout only to stream it to the toast).
        let status = Command::new(&exe).arg("dedrm").arg(&book.kfx_path).status();
        let out_path = dedrm::out_path(&book.kfx_path);
        log(format!(
            "decrypt-all {}/{}: {} exit={status:?} out_exists={}",
            i + 1,
            total,
            book.book.title,
            out_path.exists()
        ));
        if !matches!(status, Ok(ref s) if s.success()) {
            failed += 1;
            continue;
        }
        decrypted += 1;

        // Push the fresh output (best effort; skipped once the token is known bad).
        if out_path.exists() && !token_bad {
            match api::push_book(agent, cfg, &out_path) {
                Ok(api::BookPush::Imported | api::BookPush::Duplicate) => synced += 1,
                Err(api::SidleError::TokenMismatch) => {
                    log("decrypt-all: token rejected — pausing pushes, still decrypting");
                    token_bad = true;
                }
                Err(api::SidleError::Other(err)) => {
                    log(format!("decrypt-all push {}: {err:#}", out_path.display()));
                }
            }
        }
    }
    // Settle the bar at 100% so the batch visibly completes before the caller's
    // summary panel replaces it.
    let rect = toast::draw_progress(fb, renderer, "Done", total, total);
    fb.send_update(rect, WAVEFORM_MODE_GC16)?;
    log(format!(
        "decrypt-all done in {:?}: {decrypted} decrypted, {synced} synced, {failed} failed, token_bad={token_bad}",
        t0.elapsed()
    ));

    // A token mismatch means the decrypts landed but nothing synced — point the
    // user at the re-provision step (the toast is one line, so no `\n` here: the
    // renderer draws it as a stray glyph, not a break). The books are on disk;
    // the DRM Sync button re-pushes them once the token is refreshed.
    if token_bad {
        return Ok(format!(
            "Decrypted {decrypted}; sync blocked — plug into sidle, Update KUAL"
        ));
    }
    Ok(format!(
        "Decrypted {decrypted}, synced {synced}, {failed} failed"
    ))
}

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
    // Cancel needs a *full tap* (Down then Up in the button), not just an Up:
    // with the arm-flip auto-fire the finger that started the download is still
    // down when the overlay appears, so its release is an Up with no matching
    // Down here — that must not trip Cancel just because it lands on the button.
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
