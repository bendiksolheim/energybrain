//! The server half: the Leptos routes and server functions next to the HTTP routes.

use std::path::Path;

use axum::Router;
use energybrain_http::AppState;
use leptos::prelude::*;
use leptos_axum::{LeptosRoutes, file_and_error_handler, generate_route_list};

use crate::app::{App, shell};

/// Every route energybrain serves: `energybrain-http`'s, the app's pages and server functions,
/// and the browser bundle (`site_root/pkg`, built by `cargo leptos build`).
pub fn router(state: AppState, site_root: &Path) -> Router {
    let options = LeptosOptions::builder()
        .output_name("energybrain")
        .site_root(site_root.to_string_lossy().as_ref())
        .site_pkg_dir("pkg")
        .env(Env::PROD)
        .build();
    let routes = generate_route_list(App);
    let context = {
        let state = state.clone();
        move || provide_context(state.clone())
    };
    let app = Router::new()
        .leptos_routes_with_context(&options, routes, context, {
            let options = options.clone();
            move || shell(options.clone())
        })
        .fallback(file_and_error_handler(shell))
        .with_state(options);
    energybrain_http::router(state).merge(app)
}
