//! Injectable wall clock (catalog timestamps, purge deadline, maintenance schedule).
//!
//! Production uses [`SystemClock`]; tests use [`ManualClock`] to step over the 15-minute purge SLA
//! or the 6-hour optimize interval without waiting.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// Milliseconds since the Unix epoch.
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now_ms(&self) -> i64;
}

/// The real clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        sr_catalog::now_ms()
    }
}

/// A clock that only moves when told to (tests).
#[derive(Debug)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    /// Starts at the current real time, so catalog rows look normal.
    pub fn new() -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(sr_catalog::now_ms())))
    }
    pub fn advance_ms(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
    pub fn set_ms(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Shared clock handle.
pub type SharedClock = Arc<dyn Clock>;

/// The default clock.
pub fn system() -> SharedClock {
    Arc::new(SystemClock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves_only_when_told() {
        let c = ManualClock::new();
        let t0 = c.now_ms();
        assert_eq!(c.now_ms(), t0);
        c.advance_ms(15 * 60 * 1000);
        assert_eq!(c.now_ms(), t0 + 900_000);
        c.set_ms(5);
        assert_eq!(c.now_ms(), 5);
    }
}
