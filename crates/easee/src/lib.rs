//! Easee cloud API adapter for the EV charger (PLAN.md §9).
//!
//! Split like the MQTT adapter: [`api`] is pure (request bodies, response parsing, token
//! lifetimes, backoff) and unit-tested exhaustively; [`client`] and [`link`] are the thin
//! `reqwest` transport, tested against a fake Easee server built on `axum`.

pub mod api;
pub mod client;
pub mod link;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

pub use client::{Client, Error};
pub use link::{Link, Timing};
