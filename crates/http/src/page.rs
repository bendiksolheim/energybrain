//! The history page: static files built into the binary.
//!
//! `GET /` is the page, `GET /assets/<name>` its script, stylesheet and the vendored uPlot
//! 1.6.32 (MIT, licence served as `uplot-LICENSE`). All URLs in the page are relative, so it
//! also works behind a reverse proxy under a path prefix.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};

const INDEX: &str = include_str!("../assets/index.html");

/// `(name, content type, body)`.
const ASSETS: &[(&str, &str, &str)] = &[
    (
        "app.js",
        "text/javascript; charset=utf-8",
        include_str!("../assets/app.js"),
    ),
    (
        "app.css",
        "text/css; charset=utf-8",
        include_str!("../assets/app.css"),
    ),
    (
        "uplot.min.js",
        "text/javascript; charset=utf-8",
        include_str!("../assets/uplot.min.js"),
    ),
    (
        "uplot.min.css",
        "text/css; charset=utf-8",
        include_str!("../assets/uplot.min.css"),
    ),
    (
        "uplot-LICENSE",
        "text/plain; charset=utf-8",
        include_str!("../assets/uplot-LICENSE"),
    ),
];

/// Browsers revalidate on every load, so a new release's page is picked up at once.
const NO_CACHE: &str = "no-cache";

pub(crate) async fn index() -> Response {
    (
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CACHE_CONTROL, NO_CACHE),
        ],
        INDEX,
    )
        .into_response()
}

pub(crate) async fn asset(Path(name): Path<String>) -> Response {
    match ASSETS.iter().find(|(n, _, _)| *n == name) {
        Some((_, content_type, body)) => (
            [(CONTENT_TYPE, *content_type), (CACHE_CONTROL, NO_CACHE)],
            *body,
        )
            .into_response(),
        None => crate::error(StatusCode::NOT_FOUND, format!("no asset {name:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_references_only_assets_that_exist() {
        for (name, _, body) in ASSETS {
            assert!(!body.is_empty(), "{name}");
        }
        for referenced in [
            "assets/app.js",
            "assets/app.css",
            "assets/uplot.min.js",
            "assets/uplot.min.css",
        ] {
            assert!(INDEX.contains(referenced), "{referenced}");
            let name = referenced.trim_start_matches("assets/");
            assert!(ASSETS.iter().any(|(n, _, _)| *n == name), "{name}");
        }
        assert!(
            ASSETS[2]
                .2
                .starts_with("/*! https://github.com/leeoniya/uPlot (v1.6.32) */")
        );
    }
}
