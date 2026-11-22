//! The web app (PLAN.md §10): Leptos components, routes and server functions, rendered on
//! the server (`ssr`) and hydrated in the browser (`hydrate`, wasm32).
//!
//! This is the one crate outside the coverage gate, so it holds no logic worth testing:
//! everything it shows is computed by `energybrain-view` (gated, shared with the browser),
//! and the components only put it into markup.

pub mod api;
pub mod app;
mod header;
mod pages;
#[cfg(feature = "ssr")]
mod server;

#[cfg(feature = "ssr")]
pub use server::router;

/// The browser's entry point, called by the script that loads the WASM.
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    leptos::mount::hydrate_body(app::App);
}
