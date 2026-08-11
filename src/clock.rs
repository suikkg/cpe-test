//! Monotonic time sources used by long-running measurements.
//!
//! Production code uses [`SystemClock`], which delegates to
//! [`std::time::Instant::now`].  Tests can use [`ManualClock`] to advance time
//! without sleeping, while retaining `Instant`'s monotonic subtraction API.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A source of monotonic timestamps.
pub trait MonotonicClock: Send + Sync {
    fn now(&self) -> Instant;

    /// Wait for a monotonic duration. Production sleeps; deterministic test
    /// clocks advance immediately so retry and lease tests stay zero-wait.
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// The normal operating-system monotonic clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl MonotonicClock for SystemClock {
    #[inline]
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A deterministic monotonic clock for tests and simulations.
///
/// The clock starts at a real `Instant` only to give callers a familiar
/// timestamp type.  Its offset is controlled exclusively through
/// [`ManualClock::advance`], so tests never need to sleep to exercise timeout
/// or sampling calculations.
#[derive(Clone, Debug)]
pub struct ManualClock {
    origin: Instant,
    elapsed: Arc<Mutex<Duration>>,
}

impl ManualClock {
    /// Create a clock whose initial timestamp is approximately now.
    pub fn new() -> Self {
        Self::with_origin(Instant::now())
    }

    /// Create a clock with an explicit timestamp origin.
    pub fn with_origin(origin: Instant) -> Self {
        Self {
            origin,
            elapsed: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }

    /// Advance the clock by a non-negative duration.
    pub fn advance(&self, amount: Duration) {
        let mut elapsed = self.elapsed.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(next) = elapsed
            .checked_add(amount)
            .filter(|candidate| self.origin.checked_add(*candidate).is_some())
        {
            *elapsed = next;
        }
    }

    /// Return the elapsed amount since this clock was created.
    pub fn elapsed(&self) -> Duration {
        *self.elapsed.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock for ManualClock {
    #[inline]
    fn now(&self) -> Instant {
        let elapsed = self.elapsed();
        self.origin.checked_add(elapsed).unwrap_or(self.origin)
    }

    fn sleep(&self, duration: Duration) {
        self.advance(duration);
    }
}

#[cfg(test)]
mod tests {
    use super::{ManualClock, MonotonicClock};
    use std::time::Duration;

    #[test]
    fn manual_clock_advances_without_sleeping() {
        let clock = ManualClock::new();
        let start = clock.now();
        clock.advance(Duration::from_secs(3));
        assert_eq!(clock.now().duration_since(start), Duration::from_secs(3));
        assert_eq!(clock.elapsed(), Duration::from_secs(3));
    }

    #[test]
    fn cloned_manual_clocks_share_the_same_timeline() {
        let clock = ManualClock::new();
        let clone = clock.clone();
        clock.advance(Duration::from_millis(250));
        assert_eq!(clock.elapsed(), Duration::from_millis(250));
        assert_eq!(clone.elapsed(), clock.elapsed());
    }
}
