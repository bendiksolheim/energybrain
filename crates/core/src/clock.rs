//! Time source abstraction. The engine never reads the clock itself; the runtime asks a
//! [`Clock`] and passes `now` into every call. `SystemClock` lives in the app crate.

use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;

/// A source of the current instant.
pub trait Clock: Send + Sync {
    /// The current instant (UTC).
    fn now(&self) -> Timestamp;
}

/// A clock that runs `factor` times faster than its inner clock, starting at `start`: for
/// running against a simulated house in development (one hour in ten minutes at 6×).
///
/// It reads only its inner clock, so it is as pure as that clock. Every engine duration
/// (settle time, staleness, overrides, …) is measured with `now()` and so speeds up with
/// it; only real-time timers outside the engine, such as the tick interval, need
/// [`ScaledClock::real`].
pub struct ScaledClock {
    inner: Arc<dyn Clock>,
    factor: NonZeroU16,
    real_anchor: Timestamp,
    start: Timestamp,
}

impl ScaledClock {
    /// A clock that reads `start` now and then runs `factor` times faster than `inner`.
    pub fn new(inner: Arc<dyn Clock>, start: Timestamp, factor: NonZeroU16) -> Self {
        let real_anchor = inner.now();
        Self {
            inner,
            factor,
            real_anchor,
            start,
        }
    }

    /// How many times faster than the inner clock this clock runs.
    pub fn factor(&self) -> NonZeroU16 {
        self.factor
    }

    /// The real (inner-clock) duration that takes `simulated` on this clock.
    pub fn real(&self, simulated: Duration) -> Duration {
        simulated / u32::from(self.factor.get())
    }
}

impl Clock for ScaledClock {
    /// `start + (inner now − inner at construction) × factor`, saturating at the ends of
    /// the representable range.
    fn now(&self) -> Timestamp {
        let elapsed = self.inner.now().duration_since(self.real_anchor);
        let scaled = elapsed.saturating_mul(i32::from(self.factor.get()));
        let limit = if scaled.is_negative() {
            Timestamp::MIN
        } else {
            Timestamp::MAX
        };
        self.start.checked_add(scaled).unwrap_or(limit)
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use sim::SimClock;

#[cfg(any(test, feature = "test-support"))]
mod sim {
    use std::sync::{Mutex, PoisonError};

    use jiff::{SignedDuration, Timestamp};

    use super::Clock;

    /// A manually driven clock for tests and replays.
    #[derive(Debug)]
    pub struct SimClock {
        pub(super) now: Mutex<Timestamp>,
    }

    impl SimClock {
        /// A clock frozen at `at` until moved.
        pub fn new(at: Timestamp) -> Self {
            Self {
                now: Mutex::new(at),
            }
        }

        /// Jumps to `at`.
        pub fn set(&self, at: Timestamp) {
            *self.now.lock().unwrap_or_else(PoisonError::into_inner) = at;
        }

        /// Moves forward (or backward, for a negative duration) and returns the new instant.
        pub fn advance(&self, by: SignedDuration) -> Timestamp {
            let mut now = self.now.lock().unwrap_or_else(PoisonError::into_inner);
            *now += by;
            *now
        }
    }

    impl Clock for SimClock {
        fn now(&self) -> Timestamp {
            *self.now.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;

    use super::*;

    #[test]
    fn sim_clock_moves_only_when_told() {
        let start: Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        let clock = SimClock::new(start);
        assert_eq!(clock.now(), start);
        let later = clock.advance(SignedDuration::from_secs(10));
        assert_eq!(later, "2026-01-01T00:00:10Z".parse().unwrap());
        assert_eq!(clock.now(), later);
        clock.set(start);
        assert_eq!(clock.now(), start);
    }

    fn scaled(factor: u16) -> (Arc<SimClock>, ScaledClock) {
        let real = Arc::new(SimClock::new("2026-11-17T09:00:00Z".parse().unwrap()));
        let start = "2026-12-01T15:00:00Z".parse().unwrap();
        let clock = ScaledClock::new(real.clone(), start, NonZeroU16::new(factor).unwrap());
        (real, clock)
    }

    #[test]
    fn scaled_clock_starts_at_start_and_runs_faster() {
        let (real, clock) = scaled(6);
        assert_eq!(clock.factor().get(), 6);
        assert_eq!(clock.now(), "2026-12-01T15:00:00Z".parse().unwrap());
        // Ten real minutes are one simulated hour.
        real.advance(SignedDuration::from_mins(10));
        assert_eq!(clock.now(), "2026-12-01T16:00:00Z".parse().unwrap());
        real.advance(SignedDuration::from_millis(500));
        assert_eq!(clock.now(), "2026-12-01T16:00:03Z".parse().unwrap());
        // An inner clock stepping back moves this clock back too, scaled.
        real.advance(SignedDuration::from_secs(-1));
        assert_eq!(clock.now(), "2026-12-01T15:59:57Z".parse().unwrap());
    }

    #[test]
    fn scaled_clock_at_factor_one_follows_its_inner_clock() {
        let (real, clock) = scaled(1);
        real.advance(SignedDuration::from_hours(3));
        assert_eq!(clock.now(), "2026-12-01T18:00:00Z".parse().unwrap());
    }

    #[test]
    fn scaled_clock_is_linear_across_a_dst_change() {
        // Oslo springs forward at 2026-03-29T01:00Z; UTC (and so this clock) does not jump.
        let real = Arc::new(SimClock::new("2026-03-29T00:30:00Z".parse().unwrap()));
        let clock = ScaledClock::new(
            real.clone(),
            "2026-03-29T00:30:00Z".parse().unwrap(),
            NonZeroU16::new(60).unwrap(),
        );
        real.advance(SignedDuration::from_mins(1));
        assert_eq!(clock.now(), "2026-03-29T01:30:00Z".parse().unwrap());
    }

    #[test]
    fn scaled_clock_saturates() {
        let (real, clock) = scaled(u16::MAX);
        real.set(Timestamp::MAX);
        assert_eq!(clock.now(), Timestamp::MAX);
        real.set(Timestamp::MIN);
        assert_eq!(clock.now(), Timestamp::MIN);
    }

    #[test]
    fn real_durations_shrink_by_the_factor() {
        let (_, clock) = scaled(6);
        assert_eq!(
            clock.real(Duration::from_secs(10)),
            Duration::from_millis(1666) + Duration::from_nanos(666_666)
        );
        assert_eq!(clock.real(Duration::from_mins(60)), Duration::from_mins(10));
    }

    #[test]
    fn sim_clock_survives_a_poisoned_lock() {
        let start: Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        let clock = std::sync::Arc::new(SimClock::new(start));
        let c = clock.clone();
        let _ = std::thread::spawn(move || {
            let _guard = c.now.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert_eq!(clock.now(), start);
        clock.set(start);
        assert_eq!(clock.advance(SignedDuration::ZERO), start);
    }
}
