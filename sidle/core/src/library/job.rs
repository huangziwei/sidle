//! How a long-running library job reports progress and learns it should stop.

use std::ops::ControlFlow;

/// Where a job has got to.
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
