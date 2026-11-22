//! Server functions: the browser calls them over HTTP (`/fn/...`), the server runs the body.
//! Each reads the shared [`energybrain_http::AppState`] and returns `energybrain-view` types.

// `#[server]` functions must be async, even when the body has nothing to await.
#![allow(clippy::unused_async, clippy::unused_async_trait_impl)]

use energybrain_view::day::DayPage;
use energybrain_view::month::MonthPage;
use energybrain_view::status::StatusBar;
use leptos::prelude::*;

/// The latest status, as the header shows it (`None` before the engine's first status).
#[server(prefix = "/fn")]
pub async fn latest_status() -> Result<Option<StatusBar>, ServerFnError> {
    let st = state()?;
    let status = st.status.borrow().clone();
    Ok(status.map(|s| energybrain_view::status::status_bar(&s, st.site.calendar.time_zone())))
}

/// A day (`YYYY-MM-DD`; today when empty).
#[server(prefix = "/fn")]
pub async fn load_day(date: Option<String>) -> Result<DayPage, ServerFnError> {
    energybrain_http::day_page(&state()?, date.as_deref())
        .await
        .map_err(ServerFnError::new)
}

/// A month (`YYYY-MM`; this month when empty).
#[server(prefix = "/fn")]
pub async fn load_month(month: Option<String>) -> Result<MonthPage, ServerFnError> {
    energybrain_http::month_page(&state()?, month.as_deref())
        .await
        .map_err(ServerFnError::new)
}

#[cfg(feature = "ssr")]
fn state() -> Result<energybrain_http::AppState, ServerFnError> {
    use_context::<energybrain_http::AppState>()
        .ok_or_else(|| ServerFnError::new("the app state is not provided"))
}
