//! HTTP: `/healthz`, `/metrics`, `/api/state`, `/api/events` and the live status stream
//! `/api/stream`. The web app (`energybrain-web`) adds its pages and server functions on top.

mod metrics;

use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use energybrain_core::clock::Clock;
use energybrain_core::energy::Freshness;
use energybrain_core::io::Status;
use futures_util::Stream;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use energybrain_view::day::DayPage;
use energybrain_view::history;
pub use energybrain_view::history::{Detail, History, HistoryQuery, MAX_DAY_EVENTS, Site};
use energybrain_view::month::MonthPage;
pub use metrics::Metrics;

/// Looks up stored events (JSON), newest first: `(since, limit)`.
pub type EventQuery =
    Arc<dyn Fn(Option<Timestamp>, usize) -> Result<Vec<serde_json::Value>, String> + Send + Sync>;

/// Most events one request returns.
pub const MAX_EVENTS: usize = 1000;

/// Everything the handlers read.
#[derive(Clone)]
pub struct AppState {
    /// Latest status from the engine.
    pub status: watch::Receiver<Option<Arc<Status>>>,
    /// Whether MQTT is connected.
    pub mqtt_connected: watch::Receiver<bool>,
    /// Time source.
    pub clock: Arc<dyn Clock>,
    /// Counters and the Prometheus renderer.
    pub metrics: Arc<Mutex<Metrics>>,
    /// Stored-event lookup (runs on a blocking thread).
    pub events: EventQuery,
    /// History lookup for the day and month views (runs on a blocking thread).
    pub history: HistoryQuery,
    /// What the history views need to know about the installation.
    pub site: Arc<Site>,
    /// A status older than this means the engine loop has stalled.
    pub max_status_age: SignedDuration,
}

/// The HTTP routes.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/api/state", get(api_state))
        .route("/api/stream", get(api_stream))
        .route("/api/events", get(api_events))
        .with_state(state)
}

/// Why the service is unhealthy; empty when healthy.
pub fn health(
    status: Option<&Status>,
    mqtt_connected: bool,
    now: Timestamp,
    max_status_age: SignedDuration,
) -> Vec<String> {
    let mut reasons = Vec::new();
    match status {
        None => reasons.push("engine has not reported yet".to_owned()),
        Some(s) => {
            if now.duration_since(s.at) > max_status_age {
                reasons.push(format!("engine loop stalled: last status at {}", s.at));
            }
            match s.freshness {
                Freshness::Fresh => {}
                Freshness::Stale => reasons.push("meter stale".to_owned()),
                Freshness::NoData => reasons.push("no meter data yet".to_owned()),
            }
        }
    }
    if !mqtt_connected {
        reasons.push("mqtt disconnected".to_owned());
    }
    reasons
}

async fn healthz(State(st): State<AppState>) -> Response {
    let status = st.status.borrow().clone();
    let reasons = health(
        status.as_deref(),
        *st.mqtt_connected.borrow(),
        st.clock.now(),
        st.max_status_age,
    );
    if reasons.is_empty() {
        (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "unhealthy", "reasons": reasons })),
        )
            .into_response()
    }
}

async fn metrics(State(st): State<AppState>) -> Response {
    let status = st.status.borrow().clone();
    let connected = *st.mqtt_connected.borrow();
    let text = st
        .metrics
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .render(status.as_deref(), connected);
    ([(CONTENT_TYPE, "text/plain; version=0.0.4")], text).into_response()
}

async fn api_state(State(st): State<AppState>) -> Response {
    match st.status.borrow().clone() {
        Some(s) => Json(s.as_ref().clone()).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "no status yet" })),
        )
            .into_response(),
    }
}

/// Server-sent events: one `status` event per engine status (the web app's header), as
/// `energybrain_view::status::StatusBar` JSON.
async fn api_stream(
    State(st): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let tz = st.site.calendar.time_zone().clone();
    Sse::new(status_events(st.status.clone(), tz)).keep_alive(KeepAlive::default())
}

/// The latest status at once, then each new one. Ends when the engine stops (the sender is
/// dropped), so a graceful shutdown doesn't wait for open streams.
fn status_events(
    mut status: watch::Receiver<Option<Arc<Status>>>,
    tz: TimeZone,
) -> impl Stream<Item = Result<Event, Infallible>> {
    status.mark_changed();
    futures_util::stream::unfold(status, move |mut rx| {
        let tz = tz.clone();
        async move {
            loop {
                rx.changed().await.ok()?;
                let bar = rx
                    .borrow_and_update()
                    .as_deref()
                    .map(|s| energybrain_view::status::status_bar(s, &tz));
                if let Some(bar) = bar {
                    // Plain data: serializing it can't fail.
                    let json = serde_json::to_string(&bar).unwrap_or_default();
                    return Some((Ok(Event::default().event("status").data(json)), rx));
                }
            }
        }
    })
}

/// Query string of `/api/events`.
#[derive(Debug, Deserialize)]
struct EventsQuery {
    since: Option<String>,
    limit: Option<usize>,
}

async fn api_events(State(st): State<AppState>, Query(q): Query<EventsQuery>) -> Response {
    let since = match q.since.as_deref().map(str::parse::<Timestamp>).transpose() {
        Ok(since) => since,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("since: {e}") })),
            )
                .into_response();
        }
    };
    let limit = q.limit.unwrap_or(100).min(MAX_EVENTS);
    let query = st.events.clone();
    match blocking(move || query(since, limit)).await {
        Ok(events) => Json(events).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// Runs a store lookup on a blocking thread; a panic becomes an error message.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(e.to_string()))
}

/// The day page for `date` (`YYYY-MM-DD`; today when absent or empty), read from the store.
/// Errors are messages for the page: a bad date, or the store's failure.
pub async fn day_page(st: &AppState, date: Option<&str>) -> Result<DayPage, String> {
    let site = st.site.clone();
    let today = site.calendar.local_date(st.clock.now());
    let date = match date.filter(|d| !d.is_empty()) {
        Some(d) => history::parse_date(d).map_err(|e| e.to_string())?,
        None => today,
    };
    let range = history::day_range(&site.calendar, date).map_err(|e| e.to_string())?;
    let query = st.history.clone();
    let found = blocking(move || query(range.0, range.1, Detail::Full)).await?;
    let view = history::day_view(&site, date, range, today, found);
    Ok(energybrain_view::day::day_page(&site, &view))
}

/// The month page for `month` (`YYYY-MM`; this month when absent or empty).
pub async fn month_page(st: &AppState, month: Option<&str>) -> Result<MonthPage, String> {
    let site = st.site.clone();
    let today = site.calendar.local_date(st.clock.now());
    let day = match month.filter(|m| !m.is_empty()) {
        Some(m) => history::parse_month(m).map_err(|e| e.to_string())?,
        None => today,
    };
    let range = history::month_range(&site.calendar, day).map_err(|e| e.to_string())?;
    let query = st.history.clone();
    let found = blocking(move || query(range.0, range.1, Detail::Hours)).await?;
    let view = history::month_view(&site, day, range, today, &found);
    Ok(energybrain_view::month::month_page(&site, &view))
}

fn error(code: StatusCode, message: &str) -> Response {
    (code, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use energybrain_core::clock::SimClock;
    use tower::ServiceExt;

    use super::*;
    use crate::metrics::tests::status;

    const NOW: &str = "2026-11-15T17:20:00Z";

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    struct Rig {
        status: watch::Sender<Option<Arc<Status>>>,
        mqtt: watch::Sender<bool>,
        state: AppState,
    }

    fn rig(events: EventQuery) -> Rig {
        rig_with(events, Arc::new(|_, _, _| Ok(History::default())))
    }

    fn rig_with(events: EventQuery, history: HistoryQuery) -> Rig {
        let (status, status_rx) = watch::channel(None);
        let (mqtt, mqtt_rx) = watch::channel(false);
        let state = AppState {
            status: status_rx,
            mqtt_connected: mqtt_rx,
            clock: Arc::new(SimClock::new(ts(NOW))),
            metrics: Arc::new(Mutex::new(Metrics::default())),
            events,
            history,
            site: Arc::new(history::fixtures::site()),
            max_status_age: SignedDuration::from_secs(30),
        };
        Rig {
            status,
            mqtt,
            state,
        }
    }

    fn no_events() -> EventQuery {
        Arc::new(|_, _| Ok(vec![]))
    }

    async fn get(state: &AppState, uri: &str) -> (StatusCode, String) {
        let res = router(state.clone())
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let code = res.status();
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (code, String::from_utf8(body.to_vec()).unwrap())
    }

    #[test]
    fn health_reasons() {
        let age = SignedDuration::from_secs(30);
        let now = ts(NOW);
        assert_eq!(
            health(None, false, now, age),
            ["engine has not reported yet", "mqtt disconnected"]
        );
        let mut s = status();
        assert_eq!(health(Some(&s), true, now, age), Vec::<String>::new());
        s.freshness = Freshness::Stale;
        s.at = ts("2026-11-15T17:19:00Z");
        assert_eq!(
            health(Some(&s), true, now, age),
            [
                "engine loop stalled: last status at 2026-11-15T17:19:00Z",
                "meter stale"
            ]
        );
        s.freshness = Freshness::NoData;
        s.at = now;
        assert_eq!(health(Some(&s), true, now, age), ["no meter data yet"]);
    }

    #[tokio::test]
    async fn healthz_reports_ok_or_why_not() {
        let r = rig(no_events());
        let (code, body) = get(&r.state, "/healthz").await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            r#"{"reasons":["engine has not reported yet","mqtt disconnected"],"status":"unhealthy"}"#
        );
        r.status.send(Some(Arc::new(status()))).unwrap();
        r.mqtt.send(true).unwrap();
        assert_eq!(
            get(&r.state, "/healthz").await,
            (StatusCode::OK, r#"{"status":"ok"}"#.to_owned())
        );
    }

    #[tokio::test]
    async fn api_state_serves_the_latest_status() {
        let r = rig(no_events());
        assert_eq!(
            get(&r.state, "/api/state").await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                r#"{"error":"no status yet"}"#.to_owned()
            )
        );
        r.status.send(Some(Arc::new(status()))).unwrap();
        let (code, body) = get(&r.state, "/api/state").await;
        assert_eq!(code, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["mode"], "observe");
        assert_eq!(v["zones"][0]["id"], "bathroom");
        assert_eq!(
            get(&r.state, "/api/events").await,
            (StatusCode::OK, "[]".to_owned())
        );
        // The default rig has an empty history: today renders with no samples.
        assert_eq!(day_page(&r.state, None).await.unwrap().samples.at.len(), 0);
    }

    #[tokio::test]
    async fn the_stream_sends_each_status_and_ends_with_the_engine() {
        use futures_util::StreamExt;

        let r = rig(no_events());
        let res = router(r.state.clone())
            .oneshot(Request::get("/api/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.headers()[CONTENT_TYPE], "text/event-stream");
        let mut body = res.into_body().into_data_stream();
        let quiet = std::time::Duration::from_millis(50);
        // No status yet: nothing is sent until the first one.
        assert!(tokio::time::timeout(quiet, body.next()).await.is_err());
        r.status.send(Some(Arc::new(status()))).unwrap();
        let first = String::from_utf8(body.next().await.unwrap().unwrap().to_vec()).unwrap();
        assert!(first.starts_with("event: status\ndata: {"), "{first}");
        let json = first.trim_end().trim_start_matches("event: status\ndata: ");
        let bar: energybrain_view::status::StatusBar = serde_json::from_str(json).unwrap();
        assert_eq!(bar.mode, "observe");
        // Clearing the status sends nothing; the next one does.
        r.status.send(None).unwrap();
        assert!(tokio::time::timeout(quiet, body.next()).await.is_err());
        let mut s = status();
        s.mode = energybrain_core::mode::Mode::Active;
        r.status.send(Some(Arc::new(s))).unwrap();
        let next = String::from_utf8(body.next().await.unwrap().unwrap().to_vec()).unwrap();
        assert!(next.contains(r#""mode":"active""#), "{next}");
        drop(r.status);
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn metrics_are_prometheus_text() {
        let r = rig(no_events());
        r.status.send(Some(Arc::new(status()))).unwrap();
        let res = router(r.state.clone())
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.headers()[CONTENT_TYPE], "text/plain; version=0.0.4");
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("energybrain_mqtt_connected 0\n"), "{text}");
        assert!(text.contains("energybrain_hour_limit_kwh 4.7\n"), "{text}");
    }

    #[tokio::test]
    async fn events_are_paged_and_validated() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let events: EventQuery = Arc::new(move |since, limit| {
            log.lock().unwrap().push((since, limit));
            Ok(vec![json!({ "kind": { "type": "mode_changed" } })])
        });
        let r = rig(events);
        let (code, body) = get(&r.state, "/api/events").await;
        assert_eq!(
            (code, body.as_str()),
            (StatusCode::OK, r#"[{"kind":{"type":"mode_changed"}}]"#)
        );
        get(
            &r.state,
            "/api/events?since=2026-11-15T17:00:00Z&limit=5000",
        )
        .await;
        assert_eq!(
            *seen.lock().unwrap(),
            [(None, 100), (Some(ts("2026-11-15T17:00:00Z")), MAX_EVENTS)]
        );
        let (code, body) = get(&r.state, "/api/events?since=yesterday").await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(body.starts_with(r#"{"error":"since: "#), "{body}");
    }

    #[tokio::test]
    async fn event_lookup_failures_are_server_errors() {
        let failing: EventQuery = Arc::new(|_, _| Err("database is locked".to_owned()));
        let r = rig(failing);
        assert_eq!(
            get(&r.state, "/api/events").await,
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error":"database is locked"}"#.to_owned()
            )
        );
        let panicking: EventQuery = Arc::new(|_, _| panic!("boom"));
        let r = rig(panicking);
        let (code, body) = get(&r.state, "/api/events").await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("panicked"), "{body}");
    }

    #[tokio::test]
    async fn metrics_survive_a_poisoned_lock() {
        let r = rig(no_events());
        let m = r.state.metrics.clone();
        let _ = std::thread::spawn(move || {
            let _g = m.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert_eq!(get(&r.state, "/metrics").await.0, StatusCode::OK);
    }

    type Calls = Arc<Mutex<Vec<(Timestamp, Timestamp, Detail)>>>;

    fn recording(history: History) -> (HistoryQuery, Calls) {
        let calls: Calls = Arc::default();
        let log = calls.clone();
        let query: HistoryQuery = Arc::new(move |from, to, detail| {
            log.lock().unwrap().push((from, to, detail));
            Ok(history.clone())
        });
        (query, calls)
    }

    #[tokio::test]
    async fn day_defaults_to_today_and_reads_the_whole_local_day() {
        let (query, calls) = recording(history::fixtures::day_history());
        let r = rig_with(no_events(), query);
        let day = day_page(&r.state, None).await.unwrap();
        assert_eq!((day.date.as_str(), day.is_today), ("2026-11-15", true));
        assert_eq!(day.time_zone, "Europe/Oslo");
        assert_eq!(day.samples.at.len(), 5);
        let fall_back = day_page(&r.state, Some("2026-10-25")).await.unwrap();
        assert_eq!(fall_back.hours.len(), 25);
        assert_eq!(
            day_page(&r.state, Some("")).await.unwrap().date,
            "2026-11-15"
        );
        assert_eq!(
            calls.lock().unwrap()[..2],
            [
                (
                    ts("2026-11-14T23:00:00Z"),
                    ts("2026-11-15T23:00:00Z"),
                    Detail::Full
                ),
                (
                    ts("2026-10-24T22:00:00Z"),
                    ts("2026-10-25T23:00:00Z"),
                    Detail::Full
                )
            ]
        );
    }

    #[tokio::test]
    async fn month_defaults_to_this_month_and_reads_hours_only() {
        let (query, calls) = recording(History::default());
        let r = rig_with(no_events(), query);
        let month = month_page(&r.state, None).await.unwrap();
        assert_eq!((month.month.as_str(), month.days.len()), ("2026-11", 30));
        let feb = month_page(&r.state, Some("2026-02")).await.unwrap();
        assert_eq!(feb.days.len(), 28);
        assert_eq!(
            month_page(&r.state, Some("")).await.unwrap().month,
            "2026-11"
        );
        assert_eq!(
            calls.lock().unwrap()[..2],
            [
                (
                    ts("2026-10-31T23:00:00Z"),
                    ts("2026-11-30T23:00:00Z"),
                    Detail::Hours
                ),
                (
                    ts("2026-01-31T23:00:00Z"),
                    ts("2026-02-28T23:00:00Z"),
                    Detail::Hours
                )
            ]
        );
    }

    #[tokio::test]
    async fn bad_dates_are_rejected_before_reading_the_store() {
        let (query, calls) = recording(History::default());
        let r = rig_with(no_events(), query);
        for date in ["yesterday", "9999-12-31"] {
            assert!(day_page(&r.state, Some(date)).await.is_err(), "{date}");
        }
        for month in ["2026-13", "9999-12"] {
            assert!(month_page(&r.state, Some(month)).await.is_err(), "{month}");
        }
        assert_eq!(calls.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn history_lookup_failures_are_errors() {
        let failing: HistoryQuery = Arc::new(|_, _, _| Err("disk full".to_owned()));
        let r = rig_with(no_events(), failing);
        assert_eq!(day_page(&r.state, None).await.unwrap_err(), "disk full");
        assert_eq!(month_page(&r.state, None).await.unwrap_err(), "disk full");
        let panicking: HistoryQuery = Arc::new(|_, _, _| panic!("boom"));
        let r = rig_with(no_events(), panicking);
        // `blocking` is instantiated per caller, so each needs its panic path run.
        assert!(
            day_page(&r.state, None)
                .await
                .unwrap_err()
                .contains("panicked")
        );
        assert!(
            month_page(&r.state, None)
                .await
                .unwrap_err()
                .contains("panicked")
        );
    }
}
