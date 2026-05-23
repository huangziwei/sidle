//! Proof-of-concept for the X11-window rewrite.
//!
//! The one risky unknown before porting Sidle's renderer off raw `/dev/fb0`:
//! will the lab126 window manager *manage* a plain Sidle-created X11 window —
//! show it fullscreen and recomposite the screen when it's torn down — the way
//! it does for kterm? If yes, the windowless-exit bug (dead home status bar)
//! goes away for free, because the compositor repaints underneath us on exit.
//!
//! Run via `sidle --x11-poc`: maps a fullscreen window with an unmistakable
//! test pattern, waits for a tap, then destroys it. Success criteria:
//!   1. the test pattern actually appears (WM shows our window), and
//!   2. tapping exits to a cleanly-repainted home/KUAL screen — no stuck frame,
//!      status bar back — proving the WM recomposited on teardown.

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt, CreateGCAux, CreateWindowAux, EventMask, PropMode, Rectangle,
    WindowClass,
};
// `change_property8` lives in a separate `ConnectionExt` (the wrapper trait).
use x11rb::wrapper::ConnectionExt as _;

pub fn run(log: impl Fn(String)) -> Result<()> {
    let (conn, screen_num) = x11rb::connect(None).context("connect to X ($DISPLAY)")?;
    let screen = conn.setup().roots[screen_num].clone();
    let w = screen.width_in_pixels;
    let h = screen.height_in_pixels;
    let depth = screen.root_depth;
    log(format!(
        "x11poc: connected, screen {w}x{h} depth={depth} root_visual={} white={} black={}",
        screen.root_visual, screen.white_pixel, screen.black_pixel
    ));

    let win = conn.generate_id().context("generate_id window")?;
    conn.create_window(
        depth,
        win,
        screen.root,
        0,
        0,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.white_pixel)
            .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS),
    )
    .context("create_window")?;

    // The lab126 WM reads the window name as a layout spec. Mimic an
    // Application-layer, no-chrome, fullscreen window (the shape KUAL/booklets
    // use). This is a guess; if a plain name works the POC will still show.
    let name = b"L:A_N:application_ID:com.sidle.x11poc_PC:N_O:U";
    conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, name)
        .context("set WM_NAME")?;

    conn.map_window(win).context("map_window")?;

    let gc = conn.generate_id().context("generate_id gc")?;
    conn.create_gc(
        gc,
        win,
        &CreateGCAux::new()
            .foreground(screen.black_pixel)
            .background(screen.white_pixel),
    )
    .context("create_gc")?;

    let bars = [
        Rectangle { x: 0, y: 0, width: w, height: 10 },
        Rectangle { x: 0, y: (h - 10) as i16, width: w, height: 10 },
        Rectangle { x: 0, y: 0, width: 10, height: h },
        Rectangle { x: (w - 10) as i16, y: 0, width: 10, height: h },
        Rectangle { x: 120, y: 120, width: w.saturating_sub(240), height: 80 },
        Rectangle { x: 120, y: 320, width: w.saturating_sub(240), height: 80 },
    ];
    conn.poly_fill_rectangle(win, gc, &bars).context("poly_fill_rectangle")?;
    conn.flush().context("flush")?;
    log("x11poc: mapped + drew test pattern — tap to exit".to_string());

    loop {
        match conn.wait_for_event().context("wait_for_event")? {
            Event::Expose(_) => {
                let _ = conn.poly_fill_rectangle(win, gc, &bars);
                let _ = conn.flush();
            }
            Event::ButtonPress(_) => break,
            _ => {}
        }
    }

    conn.destroy_window(win).context("destroy_window")?;
    conn.flush().context("final flush")?;
    log("x11poc: destroyed window, exiting".to_string());
    Ok(())
}
