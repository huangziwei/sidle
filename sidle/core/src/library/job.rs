//! How a long-running library job reports progress and learns it should stop.
//!
//! Anything that can outlast a few seconds — indexing every book's position
//! axis, reading a month of device logs — has to be watchable and interruptible,
//! or the UI can only offer a frozen window. Both needs are one callback: the
//! job hands out a [`Report`], and what comes back says whether to keep going.
//!
//! Kept Tauri-free, like the rest of this crate. The desktop app turns a
//! [`Report`] into an event and a cancel flag into [`ControlFlow::Break`]; the
//! LAN server or a test can pass a closure that only counts.

use std::ops::ControlFlow;

/// Where a job has got to.
///
/// `total` is the best estimate available when the phase starts, and may be 0
/// when the size is not yet known — a bar should show itself indeterminate
/// rather than dividing by it.
#[derive(Debug, Clone, Copy)]
pub struct Report<'a> {
    /// Stable machine name for the step, e.g. `"index"` or `"read"`. The UI
    /// keys phase ordering off this, so it must not be a prose string.
    pub phase: &'a str,
    pub done: usize,
    pub total: usize,
    /// What this step is doing right now, for a human. A filename, a title.
    pub label: &'a str,
}

impl Report<'_> {
    /// Fraction of this phase completed, or `None` when the total is unknown.
    #[must_use]
    pub fn fraction(&self) -> Option<f32> {
        (self.total > 0).then(|| (self.done as f32 / self.total as f32).clamp(0.0, 1.0))
    }
}

/// What a job calls to report a step. Returning [`ControlFlow::Break`] asks it
/// to stop at the next safe point.
pub type Watch<'a> = &'a mut dyn FnMut(Report<'_>) -> ControlFlow<()>;

/// A watcher that never cancels and ignores every report — for callers with no
/// UI, and for tests.
pub fn ignore(_: Report<'_>) -> ControlFlow<()> {
    ControlFlow::Continue(())
}
