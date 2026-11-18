//! Diagnostics on control/owner threads only, never on an audio or HAL callback.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub(crate) struct Timing {
    operation: &'static str,
    phase: &'static str,
    id: u64,
    started: Instant,
}

impl Timing {
    pub fn start(operation: &'static str, phase: &'static str, id: u64) -> Self {
        tracing::debug!(operation,
         phase,
         operation_id = id,
         thread_id = ?std::thread::current().id(),
         "macOS output begin");

        Self {
            operation,
            phase,
            id,
            started: Instant::now(),
        }
    }
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for Timing {
    fn drop(&mut self) {
        tracing::debug!(
            operation = self.operation,
            phase = self.phase,
            operation_id = self.id,
            elapsed_ms = self.started.elapsed().as_secs_f64() * 1000.0,
            unwinding = std::thread::panicking(),
            "macOS output end"
        );
    }
}
