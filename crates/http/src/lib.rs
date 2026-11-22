//! HTTP: the old history page at `/legacy` (until the web app replaces it), `/healthz`, `/metrics`, `/api/state`, `/api/events`, and the
//! history views `/api/day` and `/api/month` behind the page.
//!
//! The router only reads shared state that the runtime loop keeps up to date: the latest
//! [`Status`] (a `watch` channel), whether MQTT is connected, the [`Metrics`] counters, and
//! functions that query the store.

mod metrics;
mod page;

use std::sync::{Arc, Mutex, PoisonError};

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use energybrain_core::clock::Clock;
use energybrain_core::energy::Freshness;
use energybrain_core::io::Status;
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use energybrain_view::history;
pub use energybrain_view::history::{
    DayView, Detail, History, HistoryQuery, MAX_DAY_EVENTS, MonthView, Site, day_view, month_view,
};
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
        .route("/legacy", get(page::index))
        .route("/assets/{name}", get(page::asset))
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/api/state", get(api_state))
        .route("/api/events", get(api_events))
        .route("/api/day", get(api_day))
        .route("/api/month", get(api_month))
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
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
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

fn error(code: StatusCode, message: impl Into<String>) -> Response {
    (code, Json(json!({ "error": message.into() }))).into_response()
}

/// Query string of `/api/day`.
#[derive(Debug, Deserialize)]
struct DayQuery {
    date: Option<String>,
}

async fn api_day(State(st): State<AppState>, Query(q): Query<DayQuery>) -> Response {
    let site = st.site.clone();
    let today = site.calendar.local_date(st.clock.now());
    let date = match q.date.as_deref().map(history::parse_date).transpose() {
        Ok(date) => date.unwrap_or(today),
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let (from, to) = match history::day_range(&site.calendar, date) {
        Ok(range) => range,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let query = st.history.clone();
    match blocking(move || query(from, to, Detail::Full)).await {
        Ok(h) => Json(day_view(&site, date, (from, to), today, h)).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// Query string of `/api/month`.
#[derive(Debug, Deserialize)]
struct MonthQuery {
    month: Option<String>,
}

async fn api_month(State(st): State<AppState>, Query(q): Query<MonthQuery>) -> Response {
    let site = st.site.clone();
    let today = site.calendar.local_date(st.clock.now());
    let day = match q.month.as_deref().map(history::parse_month).transpose() {
        Ok(day) => day.unwrap_or(today),
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let (from, to) = match history::month_range(&site.calendar, day) {
        Ok(range) => range,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let query = st.history.clone();
    match blocking(move || query(from, to, Detail::Hours)).await {
        Ok(h) => Json(month_view(&site, day, (from, to), today, &h)).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
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
        assert_eq!(get(&r.state, "/api/day").await.0, StatusCode::OK);
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

    fn body(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn day_defaults_to_today_and_reads_the_whole_local_day() {
        let (query, calls) = recording(history::fixtures::day_history());
        let r = rig_with(no_events(), query);
        let (code, text) = get(&r.state, "/api/day").await;
        assert_eq!(code, StatusCode::OK);
        let v = body(&text);
        assert_eq!(v["date"], "2026-11-15");
        assert_eq!(v["today"], "2026-11-15");
        assert_eq!(v["time_zone"], "Europe/Oslo");
        assert_eq!(v["samples"]["power"]["unit"], "kW");
        let (code, text) = get(&r.state, "/api/day?date=2026-10-25").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body(&text)["hours"]["start"].as_array().unwrap().len(), 25);
        assert_eq!(
            *calls.lock().unwrap(),
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
        let (code, text) = get(&r.state, "/api/month").await;
        assert_eq!(code, StatusCode::OK);
        let v = body(&text);
        assert_eq!(v["month"], "2026-11");
        assert_eq!(v["days"]["date"].as_array().unwrap().len(), 30);
        assert_eq!(v["tier"]["index"], 0);
        let (code, text) = get(&r.state, "/api/month?month=2026-02").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body(&text)["days"]["date"].as_array().unwrap().len(), 28);
        assert_eq!(
            *calls.lock().unwrap(),
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
        for uri in [
            "/api/day?date=yesterday",
            "/api/day?date=9999-12-31",
            "/api/month?month=2026-13",
            "/api/month?month=9999-12",
        ] {
            let (code, text) = get(&r.state, uri).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "{uri}");
            assert!(body(&text)["error"].is_string(), "{uri}: {text}");
        }
        assert_eq!(calls.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn history_lookup_failures_are_server_errors() {
        let failing: HistoryQuery = Arc::new(|_, _, _| Err("disk full".to_owned()));
        let r = rig_with(no_events(), failing);
        for uri in ["/api/day", "/api/month"] {
            assert_eq!(
                get(&r.state, uri).await,
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    r#"{"error":"disk full"}"#.to_owned()
                )
            );
        }
        let panicking: HistoryQuery = Arc::new(|_, _, _| panic!("boom"));
        let r = rig_with(no_events(), panicking);
        // `blocking` is instantiated per handler, so each needs its panic path run.
        for uri in ["/api/day", "/api/month"] {
            let (code, text) = get(&r.state, uri).await;
            assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
            assert!(text.contains("panicked"), "{text}");
        }
    }

    #[tokio::test]
    async fn the_page_and_its_assets_are_served() {
        let r = rig(no_events());
        let res = router(r.state.clone())
            .oneshot(Request::get("/legacy").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("<title>energybrain history</title>"));
        for (uri, content_type) in [
            ("/assets/app.js", "text/javascript; charset=utf-8"),
            ("/assets/uplot.min.css", "text/css; charset=utf-8"),
            ("/assets/uplot-LICENSE", "text/plain; charset=utf-8"),
        ] {
            let res = router(r.state.clone())
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{uri}");
            assert_eq!(res.headers()[CONTENT_TYPE], content_type, "{uri}");
            assert_eq!(res.headers()[axum::http::header::CACHE_CONTROL], "no-cache");
        }
        assert_eq!(
            get(&r.state, "/assets/secret.toml").await,
            (
                StatusCode::NOT_FOUND,
                r#"{"error":"no asset \"secret.toml\""}"#.to_owned()
            )
        );
    }
}
