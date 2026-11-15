//! Pure domain for energybrain: units, clock, time, tariff, energy accounting, peaks, zones,
//! policies, arbiter, actuator and engine.
//!
//! No async, no I/O, and no reading of the system clock: time always comes in as an argument.

// Tests compare exactly constructed floats on purpose.
#![cfg_attr(test, allow(clippy::float_cmp))]

pub mod actuator;
pub mod arbiter;
pub mod clock;
pub mod energy;
pub mod guard;
pub mod ids;
pub mod mode;
pub mod peaks;
pub mod policy;
pub mod tariff;
pub mod time;
pub mod units;
pub mod zone;
