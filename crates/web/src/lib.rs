//! The web app (PLAN.md §10): Leptos components, routes and server functions, rendered on
//! the server (`ssr`) and hydrated in the browser (`hydrate`, wasm32).
//!
//! This is the one crate outside the coverage gate, so it holds no logic worth testing:
//! everything it shows is computed by `energybrain-view` (gated, shared with the browser),
//! and the components only put it into markup.

pub mod api;
pub mod app;
mod chart;
mod header;
mod pages;
#[cfg(feature = "ssr")]
mod server;

#[cfg(feature = "ssr")]
pub use server::router;

/// Runs `f` once the page is live in the browser (an effect); never on the server.
///
/// Gated on `wasm32` as well as `hydrate`: `cargo test --all-features` builds the server with
/// `hydrate` too, and an effect created while the server renders would spawn a local task
/// outside a `LocalSet` and panic.
pub fn in_browser(f: impl FnOnce() + 'static) {
    #[cfg(all(feature = "hydrate", target_arch = "wasm32"))]
    {
        let mut f = Some(f);
        leptos::prelude::Effect::new(move |_| {
            if let Some(f) = f.take() {
                f();
            }
        });
    }
    #[cfg(not(all(feature = "hydrate", target_arch = "wasm32")))]
    let _ = f;
}

/// The browser's entry point, called by the script that loads the WASM.
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    leptos::mount::hydrate_body(app::App);
}
