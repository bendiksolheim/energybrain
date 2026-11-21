//! A fake Easee cloud backed by the simulated charger, so energybrain's real Easee adapter
//! runs unchanged in `just dev` (PLAN.md §9, E5).
//!
//! The HTTP side ([`router`]) only translates: each poll or command becomes a [`Request`]
//! for the driver, which owns the house, answers it and replies. Logins and refreshes always
//! succeed. [`observations`] produces the JSON that Easee's observations endpoint returns.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use energybrain_core::units::Ampere;
use jiff::SignedDuration;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::loads::ChargerState;

/// Something the fake Easee needs from the house.
#[derive(Debug)]
pub enum Request {
    /// A poll: reply with the observations.
    Read(oneshot::Sender<Value>),
    /// `set_dynamic_charger_current`.
    Write {
        /// The limit.
        current: Ampere,
        /// How long it lasts (simulated time).
        ttl: SignedDuration,
    },
}

/// The charger's operating mode code, as Easee reports it (observation 109).
pub fn op_mode(c: &ChargerState) -> u8 {
    if !c.plugged {
        1 // disconnected
    } else if c.power.value() > 0.0 {
        3 // charging
    } else if c.full {
        4 // completed
    } else if c.offered.value() < 6.0 {
        2 // awaiting start (no current offered)
    } else {
        6 // ready to charge: offered, the car hasn't started drawing yet
    }
}

/// The observations endpoint's JSON for the charger as it is now.
pub fn observations(c: &ChargerState) -> Value {
    let mut list = vec![
        json!({"id": 109, "value": op_mode(c)}),
        json!({"id": 120, "value": c.power.value()}),
        json!({"id": 121, "value": c.session.value()}),
        json!({"id": 47, "value": c.limit.value()}),
        json!({"id": 114, "value": c.offered.value()}),
        json!({"id": 250, "value": true}),
    ];
    if let Some(d) = c.dynamic {
        list.push(json!({"id": 48, "value": d.value()}));
    }
    json!({ "observations": list })
}

#[derive(Deserialize)]
struct Command {
    amps: f64,
    minutes: i64,
}

async fn tokens() -> Json<Value> {
    Json(json!({"accessToken": "sim", "expiresIn": 86_400, "refreshToken": "sim"}))
}

async fn read(State(requests): State<mpsc::Sender<Request>>) -> Response {
    let (reply, answer) = oneshot::channel();
    if requests.send(Request::Read(reply)).await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    match answer.await {
        Ok(v) => Json(v).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn write(State(requests): State<mpsc::Sender<Request>>, Json(c): Json<Command>) -> Response {
    let request = Request::Write {
        current: Ampere::new(c.amps),
        ttl: SignedDuration::from_mins(c.minutes),
    };
    if requests.send(request).await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    Json(json!({"status": 2})).into_response()
}

/// The fake Easee API, forwarding to `requests`.
pub fn router(requests: mpsc::Sender<Request>) -> Router {
    Router::new()
        .route("/api/accounts/login", post(tokens))
        .route("/api/accounts/refresh_token", post(tokens))
        .route("/state/{serial}/observations", get(read))
        .route(
            "/api/chargers/{serial}/commands/set_dynamic_charger_current",
            post(write),
        )
        .with_state(requests)
}

#[cfg(test)]
mod tests {
    use energybrain_core::charger::ChargerMode;
    use energybrain_core::units::{Kw, Kwh};
    use jiff::Timestamp;

    use super::*;

    fn state() -> ChargerState {
        ChargerState {
            plugged: true,
            full: false,
            power: Kw::new(3.68),
            session: Kwh::new(1.5),
            dynamic: Some(Ampere::new(16.0)),
            limit: Ampere::new(16.0),
            offered: Ampere::new(16.0),
        }
    }

    /// The wire format goes through energybrain's own parser, so the two can't drift.
    #[test]
    fn energybrain_reads_what_the_fake_reports() {
        let at: Timestamp = "2026-12-01T17:00:00Z".parse().unwrap();
        let body = serde_json::to_vec(&observations(&state())).unwrap();
        let r = energybrain_easee::api::reading(&body, at).unwrap();
        assert_eq!(r.mode, ChargerMode::Charging);
        assert_eq!(r.power, Kw::new(3.68));
        assert_eq!(r.session, Kwh::new(1.5));
        assert_eq!(r.dynamic_current, Some(Ampere::new(16.0)));
        assert_eq!(r.max_current, Some(Ampere::new(16.0)));
        assert_eq!(r.offered, Ampere::new(16.0));
        assert!(r.connected);
        let unlimited = ChargerState {
            dynamic: None,
            ..state()
        };
        let body = serde_json::to_vec(&observations(&unlimited)).unwrap();
        assert_eq!(
            energybrain_easee::api::reading(&body, at)
                .unwrap()
                .dynamic_current,
            None
        );
    }

    #[test]
    fn op_modes_follow_the_car() {
        let modes: Vec<u8> = [
            ChargerState {
                plugged: false,
                ..state()
            },
            state(),
            ChargerState {
                power: Kw::ZERO,
                full: true,
                ..state()
            },
            ChargerState {
                power: Kw::ZERO,
                offered: Ampere::ZERO,
                ..state()
            },
            ChargerState {
                power: Kw::ZERO,
                ..state()
            },
        ]
        .iter()
        .map(op_mode)
        .collect();
        assert_eq!(modes, [1, 3, 4, 2, 6]);
    }

    async fn serve(requests: mpsc::Sender<Request>) -> energybrain_easee::Client {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router(requests)).await.unwrap() });
        energybrain_easee::Client::new(energybrain_config::EaseeConfig {
            serial: "EH-SIM".to_owned(),
            username: "sim".to_owned(),
            password: "sim".to_owned(),
            api_url: url,
            poll_interval: SignedDuration::from_secs(30),
        })
        .unwrap()
    }

    /// energybrain's real client against the fake: polls and commands reach the driver.
    #[tokio::test]
    async fn the_real_client_talks_to_the_fake() {
        use energybrain_core::charger::ChargerWrite;
        let (tx, mut rx) = mpsc::channel(4);
        let mut client = serve(tx).await;
        let (seen_tx, mut seen) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            // The test makes exactly one poll and one command.
            for _ in 0..2 {
                match rx.recv().await.unwrap() {
                    Request::Read(reply) => reply.send(observations(&state())).unwrap(),
                    Request::Write { current, ttl } => seen_tx.send((current, ttl)).unwrap(),
                }
            }
        });
        let at: Timestamp = "2026-12-01T17:00:00Z".parse().unwrap();
        assert_eq!(client.read(at).await.unwrap().power, Kw::new(3.68));
        let outcome = client
            .write(ChargerWrite {
                current: Ampere::new(9.0),
                ttl: SignedDuration::from_mins(10),
            })
            .await
            .unwrap();
        assert_eq!(outcome, energybrain_easee::api::CommandOutcome::Executed);
        assert_eq!(
            seen.recv().await,
            Some((Ampere::new(9.0), SignedDuration::from_mins(10)))
        );
    }

    #[tokio::test]
    async fn without_a_driver_the_fake_is_unavailable() {
        use energybrain_core::charger::ChargerWrite;
        let at: Timestamp = "2026-12-01T17:00:00Z".parse().unwrap();
        // The driver is gone.
        let (tx, rx) = mpsc::channel(4);
        drop(rx);
        let mut client = serve(tx).await;
        let unavailable = |e: energybrain_easee::Error| {
            matches!(e, energybrain_easee::Error::Status { status: 503, .. })
        };
        assert!(unavailable(client.read(at).await.unwrap_err()));
        let w = ChargerWrite {
            current: Ampere::new(9.0),
            ttl: SignedDuration::from_mins(10),
        };
        assert!(unavailable(client.write(w).await.unwrap_err()));
        // The driver takes the request but never answers.
        let (tx, mut rx) = mpsc::channel(4);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let mut client = serve(tx).await;
        assert!(unavailable(client.read(at).await.unwrap_err()));
    }
}
