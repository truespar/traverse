//! Hybrid logical clock for monotonic transaction timestamps.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
// `web_time` is `std::time` natively and a Date/performance shim on wasm32, where
// `SystemTime::now()` panics
use web_time::{SystemTime, UNIX_EPOCH};

/// Hybrid logical timestamp: wall-clock ms plus a logical counter. Totally
/// ordered by `physical`, then `logical` (tie-break within a millisecond).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HybridTimestamp {
    /// Ms since the Unix epoch.
    pub physical: u64,
    /// Counter for ordering within one millisecond.
    pub logical: u32,
}

impl HybridTimestamp {
    /// Zero timestamp, before all others.
    pub const ZERO: Self = Self {
        physical: 0,
        logical: 0,
    };
}

/// Hybrid logical clock producing monotonic timestamps; lock-free (atomics), safe across threads in
/// one process.
pub struct HlcClock {
    last_physical: AtomicU64,
    last_logical: AtomicU32,
}

impl HlcClock {
    /// Clock seeded from wall-clock time.
    pub fn new() -> Self {
        let now = wall_clock_millis();
        Self {
            last_physical: AtomicU64::new(now),
            last_logical: AtomicU32::new(0),
        }
    }

    /// Monotonic timestamp: logical resets to 0 when the wall clock advances, else increments.
    pub fn now(&self) -> HybridTimestamp {
        let wall = wall_clock_millis();
        loop {
            let old_phys = self.last_physical.load(Ordering::Acquire);
            let old_log = self.last_logical.load(Ordering::Acquire);

            let (new_phys, new_log) = if wall > old_phys {
                // wall clock advanced: reset logical
                (wall, 0)
            } else {
                // same or backwards: bump logical
                (old_phys, old_log + 1)
            };

            // CAS physical first
            if self
                .last_physical
                .compare_exchange_weak(old_phys, new_phys, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.last_logical.store(new_log, Ordering::Release);
                return HybridTimestamp {
                    physical: new_phys,
                    logical: new_log,
                };
            }
            // lost a race: retry
        }
    }
}

impl Default for HlcClock {
    fn default() -> Self {
        Self::new()
    }
}

fn wall_clock_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_monotonic() {
        let clock = HlcClock::new();
        let mut prev = clock.now();
        for _ in 0..1000 {
            let ts = clock.now();
            assert!(ts > prev, "{ts:?} should be > {prev:?}");
            prev = ts;
        }
    }

    #[test]
    fn zero_is_smallest() {
        let clock = HlcClock::new();
        let ts = clock.now();
        assert!(ts > HybridTimestamp::ZERO);
    }

    #[test]
    fn ordering_physical_then_logical() {
        let a = HybridTimestamp {
            physical: 100,
            logical: 5,
        };
        let b = HybridTimestamp {
            physical: 100,
            logical: 6,
        };
        let c = HybridTimestamp {
            physical: 101,
            logical: 0,
        };
        assert!(a < b);
        assert!(b < c);
    }
}
