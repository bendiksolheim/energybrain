//! Time source abstraction. The engine never reads the clock itself; the runtime asks a
//! [`Clock`] and passes `now` into every call. `SystemClock` lives in the app crate.

use jiff::Timestamp;

/// A source of the current instant.
pub trait Clock: Send + Sync {
    /// The current instant (UTC).
    fn now(&self) -> Timestamp;
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
