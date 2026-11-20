//! A fake Easee cloud for tests: login, refresh, observations and the dynamic-current
//! command, with scripted failures. Runs on a free local port.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use energybrain_config::EaseeConfig;
use jiff::SignedDuration;
use serde_json::{Value, json};

pub const SERIAL: &str = "EH123456";
pub const USER: &str = "me@example.com";
pub const PASSWORD: &str = "s3cret";

/// A scripted response: status plus optional `Retry-After`.
#[derive(Debug, Clone, Copy)]
pub struct Fail(pub u16, pub Option<&'static str>);

#[derive(Debug, Default)]
pub struct Fake {
    pub logins: u32,
    pub refreshes: u32,
    pub access: String,
    pub refresh: String,
    /// `expiresIn` handed out with new tokens.
    pub expires_in: u64,
    /// Refuse refresh tokens (as if the session was revoked).
    pub refuse_refresh: bool,
    /// Treat the next authenticated call as unauthorized (token revoked server-side).
    pub revoke_access: bool,
    /// Responses for the next observation polls, before the normal one.
    pub poll_failures: VecDeque<Fail>,
    /// Responses for the next logins.
    pub login_failures: VecDeque<Fail>,
    pub observations: Value,
    pub commands: Vec<Value>,
    pub command_response: Value,
    pub command_attempts: u32,
    pub polls: u32,
}

pub type Shared = Arc<Mutex<Fake>>;

/// A charging car at 10 A.
pub fn charging() -> Value {
    json!({"observations": [
        {"id": 109, "value": 3},
        {"id": 120, "value": 2.3},
        {"id": 48, "value": 10.0},
        {"id": 47, "value": 16.0},
        {"id": 114, "value": 10},
        {"id": 250, "value": true},
    ]})
}

fn failure(f: Fail) -> Response {
    let mut resp = (StatusCode::from_u16(f.0).unwrap(), "scripted failure").into_response();
    if let Some(ra) = f.1 {
        resp.headers_mut()
            .insert("retry-after", ra.parse().unwrap());
    }
    resp
}

fn issue(s: &mut Fake) -> Value {
    s.access = format!("access-{}-{}", s.logins, s.refreshes);
    s.refresh = format!("refresh-{}-{}", s.logins, s.refreshes);
    json!({"accessToken": s.access, "expiresIn": s.expires_in, "refreshToken": s.refresh, "tokenType": "Bearer"})
}

async fn login(State(st): State<Shared>, Json(body): Json<Value>) -> Response {
    let mut s = st.lock().unwrap();
    if let Some(f) = s.login_failures.pop_front() {
        return failure(f);
    }
    if body["userName"] != USER || body["password"] != PASSWORD {
        return (StatusCode::BAD_REQUEST, "wrong").into_response();
    }
    s.logins += 1;
    Json(issue(&mut s)).into_response()
}

async fn refresh(State(st): State<Shared>, Json(body): Json<Value>) -> Response {
    let mut s = st.lock().unwrap();
    if s.refuse_refresh || body["refreshToken"] != s.refresh.as_str() {
        return (StatusCode::BAD_REQUEST, "invalid refresh token").into_response();
    }
    s.refreshes += 1;
    Json(issue(&mut s)).into_response()
}

fn authorized(s: &mut Fake, headers: &HeaderMap) -> bool {
    if std::mem::take(&mut s.revoke_access) {
        return false;
    }
    headers.get("authorization").and_then(|h| h.to_str().ok())
        == Some(format!("Bearer {}", s.access).as_str())
}

async fn observations(
    State(st): State<Shared>,
    Path(serial): Path<String>,
    headers: HeaderMap,
) -> Response {
    let mut s = st.lock().unwrap();
    if !authorized(&mut s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if serial != SERIAL {
        return StatusCode::NOT_FOUND.into_response();
    }
    s.polls += 1;
    if let Some(f) = s.poll_failures.pop_front() {
        return failure(f);
    }
    Json(s.observations.clone()).into_response()
}

async fn command(
    State(st): State<Shared>,
    Path(serial): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let mut s = st.lock().unwrap();
    s.command_attempts += 1;
    if !authorized(&mut s, &headers) || serial != SERIAL {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    s.commands.push(body);
    Json(s.command_response.clone()).into_response()
}

/// Starts the fake and returns a config pointing at it.
pub async fn start() -> (EaseeConfig, Shared) {
    let state: Shared = Arc::new(Mutex::new(Fake {
        expires_in: 3600,
        observations: charging(),
        command_response: json!({"id": 1, "status": 0}),
        ..Fake::default()
    }));
    let app = Router::new()
        .route("/api/accounts/login", post(login))
        .route("/api/accounts/refresh_token", post(refresh))
        .route("/state/{serial}/observations", get(observations))
        .route(
            "/api/chargers/{serial}/commands/set_dynamic_charger_current",
            post(command),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg = EaseeConfig {
        serial: SERIAL.to_owned(),
        username: USER.to_owned(),
        password: PASSWORD.to_owned(),
        api_url: format!("http://{addr}"),
        poll_interval: SignedDuration::from_secs(30),
    };
    (cfg, state)
}
