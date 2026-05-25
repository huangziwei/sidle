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

/// Monotonic stopwatch that is wasm-safe.
///
/// On native it wraps `std::time::Instant`. On `wasm32` there is no monotonic
/// clock and `Instant::now()` panics at runtime, so it degrades to a no-op that
/// always reports `Duration::ZERO`. The only consumers of these timings are the
/// env-gated traces (`BOKO_*_TRACE`), which never run in a browser, so zero
/// durations on wasm are harmless.
#[derive(Debug, Clone, Copy)]
pub struct Stopwatch {
    #[cfg(not(target_arch = "wasm32"))]
    start: std::time::Instant,
}

impl Stopwatch {
    #[inline]
    pub fn start() -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            start: std::time::Instant::now(),
        }
    }

    #[inline]
    pub fn elapsed(&self) -> std::time::Duration {
        #[cfg(not(target_arch = "wasm32"))]
        let elapsed = self.start.elapsed();
        #[cfg(target_arch = "wasm32")]
        let elapsed = std::time::Duration::ZERO;
        elapsed
    }
}

pub struct Trace {
    name: &'static str,
    start: Stopwatch,
    enabled: bool,
}

impl Trace {
    /// Create a tracer. Output is enabled iff `env_var` is set to any value
    /// in the environment when this is called.
    pub fn new(name: &'static str, env_var: &str) -> Self {
        Self {
            name,
            start: Stopwatch::start(),
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
