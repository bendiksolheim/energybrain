//! Pure domain for energybrain: units, clock, time, tariff, energy accounting, peaks, zones,
//! the EV charger, policies, arbiter, actuator and engine.
//!
//! No async, no I/O, and no reading of the system clock: time always comes in as an argument.

// Tests compare exactly constructed floats on purpose.
#![cfg_attr(test, allow(clippy::float_cmp))]

pub mod actuator;
pub mod arbiter;
pub mod charger;
pub mod clock;
pub mod energy;
pub mod engine;
pub mod guard;
pub mod ids;
pub mod io;
pub mod mode;
pub mod peaks;
pub mod policy;
#[cfg(test)]
mod scenarios;
pub mod tariff;
pub mod time;
pub mod units;
pub mod zone;
