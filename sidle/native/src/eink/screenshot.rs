//! The native Kindle two-corner screenshot gesture, reimplemented for Sidle.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use super::fb::{Framebuffer, MxcfbRect, WAVEFORM_MODE_GC16};

/// Where screenshots land — the same directory stock Kindle screenshots use,
/// so they show up where expected when the Kindle is plugged in over USB.
const SCREENSHOT_DIR: &str = "/mnt/us/screenshots";

/// Hold the white flash long enough to read as a deliberate flash rather than
/// a refresh glitch, short enough not to feel like a hang.
const FLASH_MS: u64 = 120;

/// Save the current screen to a timestamped PNG, flash white as feedback, then
pub fn capture(fb: &mut Framebuffer) -> Result<PathBuf> {
    let dir = Path::new(SCREENSHOT_DIR);
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("screenshot_{secs}.png"));

    // Snapshot before anything touches the backing: `capture_png` reads the
    // live screen, and we restore this exact buffer after the flash.
    let snap = fb.backing_snapshot();
    let cap = fb.capture_png(&path);

    // White flash → brief hold → restore. send_update widens to full rows, so a
    // full-screen rect repaints everything; GC16 is the clean full refresh.
    let (w, h) = (fb.var.xres, fb.var.yres);
    let full = MxcfbRect {
        top: 0,
        left: 0,
        width: w,
        height: h,
    };
    fb.fill_rect(0, 0, w, h, 0xFF);
    let _ = fb.send_update(full, WAVEFORM_MODE_GC16);
    std::thread::sleep(Duration::from_millis(FLASH_MS));
    fb.restore_backing(snap);
    let _ = fb.send_update(full, WAVEFORM_MODE_GC16);

    cap.map(|()| path)
}
