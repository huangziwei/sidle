//! Env-gated phase timer for ad-hoc pipeline tracing.

/// Monotonic stopwatch wrapping `std::time::Instant`.
#[derive(Debug, Clone, Copy)]
pub struct Stopwatch {
    start: std::time::Instant,
}

impl Stopwatch {
    #[inline]
    pub fn start() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }

    #[inline]
    pub fn elapsed(&self) -> std::time::Duration {
        self.start.elapsed()
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
