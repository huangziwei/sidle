//! A one-line progress bar for a sweep over many books.
//!
//! A sweep prints one line per finished book, which reads well in a log and
//! badly on a terminal running two thousand of them. [`Bar`] replaces those
//! lines with a single line that rewrites itself: how many are done, how fast,
//! how long is left. A failure goes through [`Bar::note`], which puts it on
//! stderr and redraws the bar under it.
//!
//! The bar draws only when stdout is a terminal. Redirected to a file, or
//! under `--json`, [`Bar::enabled`] is false: the counts add up, nothing is
//! painted, and the caller prints its own per-book lines.

use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Redraws closer together than this are dropped.
const FRAME: Duration = Duration::from_millis(80);

/// Terminal width when `ioctl` names none.
const FALLBACK_COLUMNS: usize = 80;

/// The eighths of a full cell, for a bar edge between two columns.
const PARTIALS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

/// A sweep's progress on one rewriting line.
pub struct Bar {
    label: String,
    total: usize,
    enabled: bool,
    started: Instant,
    state: Mutex<State>,
}

struct State {
    /// Items finished, whatever the outcome.
    done: usize,
    failed: usize,
    /// Per-worker fraction of the item that worker holds, so the bar moves
    /// between completions on a sweep whose items take seconds each.
    in_flight: Vec<f32>,
    /// What the most recent worker to start an item is working on.
    current: String,
    painted: Instant,
}

impl Bar {
    /// A bar over `total` items worked by `slots` threads. `enabled` false —
    /// a redirected stdout, or `--json` — makes every method a no-op.
    pub fn new(label: &str, total: usize, slots: usize, enabled: bool) -> Self {
        Self {
            label: label.to_string(),
            total,
            enabled: enabled && total > 0,
            started: Instant::now(),
            state: Mutex::new(State {
                done: 0,
                failed: 0,
                in_flight: vec![0.0; slots.max(1)],
                current: String::new(),
                painted: Instant::now() - FRAME,
            }),
        }
    }

    /// Whether this bar draws. False means the caller owns the output.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// `slot` has taken up `title`, at zero progress.
    pub fn start(&self, slot: usize, title: &str) {
        self.with(|s| {
            if let Some(f) = s.in_flight.get_mut(slot) {
                *f = 0.0;
            }
            s.current = title.to_string();
        });
    }

    /// `slot` is `fraction` of the way through its item, 0.0 to 1.0.
    pub fn tick(&self, slot: usize, fraction: f32) {
        self.with(|s| {
            if let Some(f) = s.in_flight.get_mut(slot) {
                *f = fraction.clamp(0.0, 1.0);
            }
        });
    }

    /// `slot` finished its item.
    pub fn finish_item(&self, slot: usize, ok: bool) {
        self.with(|s| {
            if let Some(f) = s.in_flight.get_mut(slot) {
                *f = 0.0;
            }
            s.done += 1;
            if !ok {
                s.failed += 1;
            }
        });
    }

    /// Put `msg` on stderr, clearing the bar's line first and drawing it again
    /// underneath.
    pub fn note(&self, msg: &str) {
        if !self.enabled {
            eprintln!("{msg}");
            return;
        }
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "\r\x1b[K");
        let _ = out.flush();
        eprintln!("{msg}");
        let line = self.render(&s);
        let _ = write!(out, "{line}");
        let _ = out.flush();
        s.painted = Instant::now();
    }

    /// Draw the bar one last time and leave the cursor on a fresh line.
    pub fn finish(&self) {
        if !self.enabled {
            return;
        }
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let line = self.render(&s);
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "\r\x1b[K{line}");
        let _ = out.flush();
    }

    /// Apply `edit` to the counts, then repaint if the frame budget allows.
    /// A disabled bar keeps its counts and paints nothing.
    fn with(&self, edit: impl FnOnce(&mut State)) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        edit(&mut s);
        if !self.enabled || (s.painted.elapsed() < FRAME && s.done < self.total) {
            return;
        }
        let line = self.render(&s);
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "\r\x1b[K{line}");
        let _ = out.flush();
        s.painted = Instant::now();
    }

    /// The whole line, sized to the terminal.
    fn render(&self, s: &State) -> String {
        self.render_within(s, terminal_columns())
    }

    /// The line drawn for a terminal `columns` wide. The result stays one
    /// column short of that: a line that wraps leaves the `\r` rewrite
    /// scribbling over the row above it.
    fn render_within(&self, s: &State, columns: usize) -> String {
        let position = s.done as f32 + s.in_flight.iter().sum::<f32>();
        let fraction = (position / self.total as f32).clamp(0.0, 1.0);
        let elapsed = self.started.elapsed().as_secs_f32();

        let counts = format!("{}/{}", s.done, self.total);
        let percent = format!("{:>3.0}%", fraction * 100.0);
        let rate = rate_of(s.done, elapsed);
        let eta = match s.done {
            0 => "--:--".to_string(),
            done => clock(elapsed / done as f32 * (self.total - done) as f32),
        };
        let failed = match s.failed {
            0 => String::new(),
            n => format!("  {n} failed"),
        };
        let stats = format!(
            "{counts}  {percent}  {rate}  {}<{eta}{failed}",
            clock(elapsed)
        );

        // The bar takes what the label, the stats and the title leave. Under
        // eight columns it is noise, and the line carries the numbers alone.
        let fixed = self.label.chars().count() + stats.chars().count() + 4;
        let bar_width = columns.saturating_sub(fixed + 24).clamp(0, 40);
        let bar = if bar_width >= 8 {
            format!("{} ", meter(fraction, bar_width))
        } else {
            String::new()
        };

        let head = format!("{} {bar}{stats}", self.label);
        let room = columns.saturating_sub(head.chars().count() + 3);
        let tail = if room >= 8 && !s.current.is_empty() {
            format!("  {}", ellipsize(&s.current, room))
        } else {
            String::new()
        };
        ellipsize(&format!("{head}{tail}"), columns.saturating_sub(1))
    }
}

/// `done` items over `elapsed` seconds, in whichever unit reads above 1.
/// Under a second of it, a rate says more about the clock than the sweep.
fn rate_of(done: usize, elapsed: f32) -> String {
    if done == 0 || elapsed < 1.0 {
        return "--".to_string();
    }
    let per_second = done as f32 / elapsed;
    if per_second >= 1.0 {
        format!("{per_second:.1}/s")
    } else {
        format!("{:.1}s each", 1.0 / per_second)
    }
}

/// `seconds` as `m:ss`, or `h:mm:ss` past an hour.
fn clock(seconds: f32) -> String {
    let total = seconds.max(0.0) as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A `width`-column meter filled to `fraction`, the last cell an eighth-block.
fn meter(fraction: f32, width: usize) -> String {
    let eighths = (fraction * width as f32 * 8.0).round() as usize;
    let full = (eighths / 8).min(width);
    let rest = eighths % 8;
    let mut out = String::from("│");
    out.push_str(&"█".repeat(full));
    let mut used = full;
    if used < width && rest > 0 {
        out.push_str(PARTIALS[rest]);
        used += 1;
    }
    out.push_str(&" ".repeat(width - used));
    out.push('│');
    out
}

/// `text` inside `width` columns, tail-trimmed to `…` when it overruns.
fn ellipsize(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// The terminal's column count, or [`FALLBACK_COLUMNS`].
fn terminal_columns() -> usize {
    #[cfg(unix)]
    {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
        if rc == 0 && size.ws_col > 0 {
            return size.ws_col as usize;
        }
    }
    FALLBACK_COLUMNS
}

/// Whether stdout is a terminal, which is what makes a rewriting line legible.
pub fn stdout_is_terminal() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_meter_fills_left_to_right() {
        assert_eq!(meter(0.0, 4), "│    │");
        assert_eq!(meter(1.0, 4), "│████│");
        assert_eq!(meter(0.5, 4), "│██  │");
    }

    #[test]
    fn a_meter_never_overruns_its_width() {
        for width in 1..40 {
            for step in 0..=100 {
                let drawn = meter(step as f32 / 100.0, width);
                assert_eq!(
                    drawn.chars().count(),
                    width + 2,
                    "width {width} at {step}%: {drawn}"
                );
            }
        }
    }

    #[test]
    fn a_partial_cell_shows_an_eighth_block() {
        // An eighth of one cell is the narrowest movement a meter can draw.
        assert_eq!(meter(0.125, 1), "│▏│");
    }

    #[test]
    fn a_clock_grows_a_field_past_an_hour() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(59.6), "0:59");
        assert_eq!(clock(75.0), "1:15");
        assert_eq!(clock(3661.0), "1:01:01");
    }

    #[test]
    fn a_rate_below_one_per_second_reads_as_seconds_each() {
        assert_eq!(rate_of(0, 1.0), "--");
        assert_eq!(rate_of(10, 2.0), "5.0/s");
        assert_eq!(rate_of(1, 6.0), "6.0s each");
        // A sweep a fraction of a second old divides by nearly nothing.
        assert_eq!(rate_of(25, 0.00004), "--");
    }

    #[test]
    fn ellipsize_keeps_the_head() {
        assert_eq!(ellipsize("abc", 5), "abc");
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        // Counting characters, not bytes, keeps a Japanese title inside its
        // column budget.
        assert_eq!(ellipsize("吾輩は猫である", 4), "吾輩は…");
    }

    #[test]
    fn a_disabled_bar_prints_nothing_and_panics_never() {
        let bar = Bar::new("converting", 10, 4, false);
        assert!(!bar.enabled());
        bar.start(0, "a book");
        bar.tick(0, 0.5);
        bar.finish_item(0, true);
        bar.finish();
    }

    #[test]
    fn a_slot_past_the_worker_count_is_ignored() {
        let bar = Bar::new("converting", 10, 2, false);
        bar.tick(99, 1.0);
        bar.finish_item(99, false);
        let state = bar.state.lock().unwrap();
        assert_eq!(state.done, 1);
        assert_eq!(state.failed, 1);
    }

    #[test]
    fn a_line_never_reaches_the_last_column() {
        let bar = Bar::new("converting", 1841, 8, false);
        bar.start(0, "吾輩は猫である、そして名前はまだ無い、長い題名の本");
        bar.tick(0, 0.4);
        for _ in 0..900 {
            bar.finish_item(1, true);
        }
        let s = bar.state.lock().unwrap();
        for columns in 1..200 {
            let line = bar.render_within(&s, columns);
            assert!(
                line.chars().count() < columns.max(2),
                "{columns} columns drew {} chars: {line}",
                line.chars().count()
            );
        }
    }

    #[test]
    fn a_wide_line_carries_the_counts_the_bar_and_the_title() {
        let bar = Bar::new("converting", 100, 4, false);
        bar.start(0, "a book");
        for _ in 0..25 {
            bar.finish_item(1, true);
        }
        let s = bar.state.lock().unwrap();
        let line = bar.render_within(&s, 120);
        assert!(line.contains("converting"), "{line}");
        assert!(line.contains("25/100"), "{line}");
        assert!(line.contains("25%"), "{line}");
        assert!(line.contains("--"), "{line}");
        assert!(line.contains('█'), "{line}");
        assert!(line.contains("a book"), "{line}");
    }

    #[test]
    fn position_counts_finished_items_plus_the_ones_in_hand() {
        let bar = Bar::new("converting", 10, 2, false);
        bar.finish_item(0, true);
        bar.tick(0, 0.5);
        bar.tick(1, 0.25);
        let state = bar.state.lock().unwrap();
        let position = state.done as f32 + state.in_flight.iter().sum::<f32>();
        assert!((position - 1.75).abs() < 1e-6, "{position}");
    }
}
