//! Env-gated phase timer used by both merge paths.
//!
//! Enable via `BOKO_MERGE_TRACE=1`. Each [`Trace::mark`] prints the cumulative
//! wall time since the trace was created. Output goes to stderr.

use std::time::Instant;

pub struct Trace {
    name: &'static str,
    start: Instant,
    enabled: bool,
}

impl Trace {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            start: Instant::now(),
            enabled: std::env::var("BOKO_MERGE_TRACE").is_ok(),
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
