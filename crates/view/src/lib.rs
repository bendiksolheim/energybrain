//! Pure presentation for the web app: no I/O, no clock, no async. Compiles to native (server)
//! and `wasm32-unknown-unknown` (browser), so the browser and the server share these types
//! and functions, and they are tested here under the coverage gate (PLAN.md §10).

pub mod day;
pub mod fmt;
pub mod history;
pub mod month;
pub mod status;
