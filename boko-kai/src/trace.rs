//! Env-gated phase timer for ad-hoc pipeline tracing.
//!
//! Each [`Trace::new`] call takes a pipeline name (printed on every line) and
//! the env var that gates output. Marks print the *cumulative* wall time
//! since the trace was created, so the table reads top-to-bottom as a
//! Gantt-style timeline. Output goes to stderr.
//!
//! Existing gates:
//!  - `BOKO_MERGE_TRACE=1` — `.kfx-zip` → `.kfx` merge (mechanical + fast)
//!  - `BOKO_KFX2EPUB_TRACE=1` — `.kfx` → `.epub` mechanical port

use std::time::Instant;

pub struct Trace {
    name: &'static str,
    start: Instant,
    enabled: bool,
}

impl Trace {
    /// Create a tracer. Output is enabled iff `env_var` is set to any value
    /// in the environment when this is called.
    pub fn new(name: &'static str, env_var: &str) -> Self {
        Self {
            name,
            start: Instant::now(),
            enabled: std::env::var(env_var).is_ok(),
        }
    }

    pub fn mark(&self, label: &str) {
        if self.enabled {
            eprintln!(
                "[{}] {:>10.3} ms  {}",
                self.name,
                self.start.elapsed().as_secs_f64() * 1e3,
                label
            );
        }
    }
}
