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
    use energybrain_view::history::{Detail, day_range, day_view, parse_date};

    let st = state()?;
    let site = st.site.clone();
    let today = site.calendar.local_date(st.clock.now());
    let date = match date.filter(|d| !d.is_empty()) {
        Some(d) => parse_date(&d).map_err(ServerFnError::new)?,
        None => today,
    };
    let range = day_range(&site.calendar, date).map_err(ServerFnError::new)?;
    let history = read(&st, range, Detail::Full).await?;
    Ok(energybrain_view::day::day_page(
        &site,
        &day_view(&site, date, range, today, history),
    ))
}

/// A month (`YYYY-MM`; this month when empty).
#[server(prefix = "/fn")]
pub async fn load_month(month: Option<String>) -> Result<MonthPage, ServerFnError> {
    use energybrain_view::history::{Detail, month_range, month_view, parse_month};

    let st = state()?;
    let site = st.site.clone();
    let today = site.calendar.local_date(st.clock.now());
    let day = match month.filter(|m| !m.is_empty()) {
        Some(m) => parse_month(&m).map_err(ServerFnError::new)?,
        None => today,
    };
    let range = month_range(&site.calendar, day).map_err(ServerFnError::new)?;
    let history = read(&st, range, Detail::Hours).await?;
    Ok(energybrain_view::month::month_page(
        &site,
        &month_view(&site, day, range, today, &history),
    ))
}

/// Reads the store on a blocking thread.
#[cfg(feature = "ssr")]
async fn read(
    st: &energybrain_http::AppState,
    (from, to): (jiff::Timestamp, jiff::Timestamp),
    detail: energybrain_view::history::Detail,
) -> Result<energybrain_view::history::History, ServerFnError> {
    let query = st.history.clone();
    tokio::task::spawn_blocking(move || query(from, to, detail))
        .await
        .map_err(ServerFnError::new)?
        .map_err(ServerFnError::new)
}

#[cfg(feature = "ssr")]
fn state() -> Result<energybrain_http::AppState, ServerFnError> {
    use_context::<energybrain_http::AppState>()
        .ok_or_else(|| ServerFnError::new("the app state is not provided"))
}
