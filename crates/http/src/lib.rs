//! HTTP: `/healthz`, `/metrics`, `/api/state` and `/api/events`.
//!
//! The router only reads shared state that the runtime loop keeps up to date: the latest
//! [`Status`] (a `watch` channel), whether MQTT is connected, the [`Metrics`] counters, and
//! a function that queries stored events.

mod metrics;

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
    /// A status older than this means the engine loop has stalled.
    pub max_status_age: SignedDuration,
}

/// The HTTP routes.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/api/state", get(api_state))
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
    let result = tokio::task::spawn_blocking(move || query(since, limit))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    match result {
        Ok(events) => Json(events).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
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
        let (status, status_rx) = watch::channel(None);
        let (mqtt, mqtt_rx) = watch::channel(false);
        let state = AppState {
            status: status_rx,
            mqtt_connected: mqtt_rx,
            clock: Arc::new(SimClock::new(ts(NOW))),
            metrics: Arc::new(Mutex::new(Metrics::default())),
            events,
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
        assert!(health(Some(&s), true, now, age).is_empty());
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
        assert_eq!(v["zones"][0]["id"], "bad");
        assert_eq!(
            get(&r.state, "/api/events").await,
            (StatusCode::OK, "[]".to_owned())
        );
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
}
