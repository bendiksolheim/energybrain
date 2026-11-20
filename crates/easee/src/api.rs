//! The pure part of the Easee adapter: request bodies, response parsing, token lifetimes and
//! the backoff schedule. No I/O; time comes in as an argument.
//!
//! Endpoints (developer.easee.com, PLAN.md §9):
//! - `POST /api/accounts/login {userName, password}` and
//!   `POST /api/accounts/refresh_token {refreshToken}` → `{accessToken, expiresIn, refreshToken}`.
//!   A refresh token is single use: always keep the newest.
//! - `GET /state/{serial}/observations?ids=…` → `{observations: [{id, value, …}]}`
//!   (100 requests per 5 minutes).
//! - `POST /api/chargers/{serial}/commands/set_dynamic_charger_current {amps, minutes}` →
//!   a command with `status` 0 sent, 1 expired, 2 executed, 3 rejected.

use std::time::Duration;

use energybrain_core::charger::{ChargerMode, ChargerReading, ChargerWrite};
use energybrain_core::units::{Ampere, Kw, Kwh};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;

/// Observation ids we read.
mod id {
    pub const MAX_CURRENT: u32 = 47;
    pub const DYNAMIC_CURRENT: u32 = 48;
    pub const REASON_FOR_NO_CURRENT: u32 = 96;
    pub const OP_MODE: u32 = 109;
    pub const OFFERED: u32 = 114;
    pub const POWER: u32 = 120;
    pub const SESSION_ENERGY: u32 = 121;
    pub const CONNECTED_TO_CLOUD: u32 = 250;
}

/// The `ids` query for the observations endpoint.
pub const OBSERVATION_IDS: &str = "47,48,96,109,114,120,121,250";

/// A response that doesn't have the expected shape.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unexpected Easee response: {0}")]
pub struct ParseError(pub String);

/// Login request body.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Login<'a> {
    /// Phone number or e-mail.
    pub user_name: &'a str,
    /// Password.
    pub password: &'a str,
}

/// Refresh request body.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Refresh<'a> {
    /// The newest refresh token.
    pub refresh_token: &'a str,
}

/// What login and refresh return.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenResponse {
    /// Bearer token for API calls.
    pub access_token: String,
    /// Seconds the access token is valid.
    pub expires_in: u64,
    /// Single-use token for the next refresh.
    pub refresh_token: String,
}

/// The tokens in use. Lives in memory only: a restart logs in again.
#[derive(Clone, PartialEq, Eq)]
pub struct Tokens {
    /// Bearer token.
    pub access: String,
    /// Single-use refresh token.
    pub refresh: String,
    /// Refresh from this instant on (at 80 % of the lifetime).
    pub refresh_after: Instant,
}

impl std::fmt::Debug for Tokens {
    /// Never prints the tokens.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("refresh_after", &self.refresh_after)
            .finish_non_exhaustive()
    }
}

impl Tokens {
    /// Tokens from a login or refresh response received at `now`.
    pub fn new(r: TokenResponse, now: Instant) -> Self {
        Self {
            access: r.access_token,
            refresh: r.refresh_token,
            refresh_after: now + Duration::from_secs(r.expires_in) * 4 / 5,
        }
    }

    /// Whether to refresh before the next call.
    pub fn due(&self, now: Instant) -> bool {
        now >= self.refresh_after
    }
}

#[derive(Deserialize)]
struct Observations {
    observations: Vec<Observation>,
}

#[derive(Deserialize)]
struct Observation {
    id: u32,
    value: Value,
}

/// A number, whether Easee sent it as a number, a numeric string or a boolean.
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

/// A flag, whether sent as a boolean, a number or a string.
fn flag(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Some(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Some(false),
        other => number(other).map(|n| n != 0.0),
    }
}

/// Parses an observations response polled at `at` (receive time, like meter power).
///
/// The op mode (109) and power (120) are required. A missing dynamic or max current stays
/// `None`; missing session energy or offered current reads as 0; a missing cloud flag as
/// connected (the poll itself just went through the cloud).
pub fn reading(body: &[u8], at: Timestamp) -> Result<ChargerReading, ParseError> {
    let parsed: Observations =
        serde_json::from_slice(body).map_err(|e| ParseError(e.to_string()))?;
    let get = |id: u32| {
        parsed
            .observations
            .iter()
            .rev()
            .find(|o| o.id == id)
            .map(|o| &o.value)
    };
    let num = |id: u32| get(id).and_then(number);
    let mode = get(id::OP_MODE)
        .and_then(|v| match v {
            Value::String(s) => s.trim().parse::<u8>().ok(),
            other => other.as_u64().and_then(|n| u8::try_from(n).ok()),
        })
        .map(ChargerMode::from_code)
        .ok_or_else(|| ParseError(format!("no op mode ({})", id::OP_MODE)))?;
    let power = num(id::POWER).ok_or_else(|| ParseError(format!("no power ({})", id::POWER)))?;
    // The raw dynamic current shows how a cleared limit reads (PLAN.md §9, unchecked in E0).
    let (dynamic, max, reason) = (
        get(id::DYNAMIC_CURRENT),
        get(id::MAX_CURRENT),
        get(id::REASON_FOR_NO_CURRENT),
    );
    tracing::debug!(?dynamic, ?max, reason_for_no_current = ?reason, "easee observations");
    Ok(ChargerReading {
        at,
        mode,
        power: Kw::new(power),
        session: Kwh::new(num(id::SESSION_ENERGY).unwrap_or(0.0)),
        dynamic_current: num(id::DYNAMIC_CURRENT).map(Ampere::new),
        max_current: num(id::MAX_CURRENT).map(Ampere::new),
        offered: Ampere::new(num(id::OFFERED).unwrap_or(0.0)),
        connected: get(id::CONNECTED_TO_CLOUD).and_then(flag).unwrap_or(true),
    })
}

/// Body of `set_dynamic_charger_current`: whole amps and whole minutes (Easee wants
/// integers; a TTL is rounded up so a limit never ends early, and is at least a minute).
pub fn command_body(w: &ChargerWrite) -> Value {
    // Currents are 0–40 A and TTLs minutes to hours, far inside i64.
    #[allow(clippy::cast_possible_truncation)]
    let amps = w.current.value().round() as i64;
    let secs = w.ttl.as_secs().max(1);
    let minutes = (secs + 59) / 60;
    serde_json::json!({ "amps": amps, "minutes": minutes })
}

/// What Easee said about a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Sent to the charger (normal; it executes shortly).
    Sent,
    /// The charger didn't pick it up in time.
    Expired,
    /// The charger executed it.
    Executed,
    /// The charger rejected it.
    Rejected,
    /// Accepted, without a status we know.
    Accepted,
}

/// Parses a command response (lenient: anything without a known status is `Accepted`).
pub fn command_outcome(body: &[u8]) -> CommandOutcome {
    let status = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("status").and_then(Value::as_u64));
    match status {
        Some(0) => CommandOutcome::Sent,
        Some(1) => CommandOutcome::Expired,
        Some(2) => CommandOutcome::Executed,
        Some(3) => CommandOutcome::Rejected,
        Some(_) | None => CommandOutcome::Accepted,
    }
}

/// A `Retry-After` header in seconds (the HTTP-date form is ignored).
pub fn retry_after(header: Option<&str>) -> Option<Duration> {
    header
        .and_then(|h| h.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Exponential backoff for failed polls: `min`, doubling up to `max`, but never shorter than
/// a server's `Retry-After`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    /// A fresh schedule.
    pub const fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max,
            next: min,
        }
    }

    /// The wait after another failure.
    pub fn failure(&mut self, retry_after: Option<Duration>) -> Duration {
        let wait = self.next.max(retry_after.unwrap_or(Duration::ZERO));
        self.next = (self.next * 2).min(self.max);
        wait
    }

    /// A success: start over.
    pub fn reset(&mut self) {
        self.next = self.min;
    }
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;

    use super::*;

    fn at() -> Timestamp {
        "2026-11-20T17:00:00Z".parse().unwrap()
    }

    fn body(pairs: &[(u32, Value)]) -> Vec<u8> {
        let observations: Vec<Value> = pairs
            .iter()
            .map(|(id, v)| serde_json::json!({"id": id, "timestamp": "2026-11-20T16:59:00Z", "dataType": 3, "value": v}))
            .collect();
        serde_json::to_vec(&serde_json::json!({ "observations": observations })).unwrap()
    }

    #[test]
    fn parses_a_full_reading() {
        let b = body(&[
            (47, 16.0.into()),
            (48, 10.0.into()),
            (96, 0.into()),
            (109, 3.into()),
            (114, 10.into()),
            (120, 2.31.into()),
            (121, 4.5.into()),
            (250, true.into()),
        ]);
        assert_eq!(
            reading(&b, at()).unwrap(),
            ChargerReading {
                at: at(),
                mode: ChargerMode::Charging,
                power: Kw::new(2.31),
                session: Kwh::new(4.5),
                dynamic_current: Some(Ampere::new(10.0)),
                max_current: Some(Ampere::new(16.0)),
                offered: Ampere::new(10.0),
                connected: true,
            }
        );
    }

    #[test]
    fn values_may_come_as_strings_and_optional_ones_may_be_missing() {
        let b = body(&[
            (109, "1".into()),
            (120, " 0.0 ".into()),
            (250, "False".into()),
        ]);
        let r = reading(&b, at()).unwrap();
        assert_eq!(r.mode, ChargerMode::Disconnected);
        assert_eq!(r.power, Kw::ZERO);
        assert_eq!(r.session, Kwh::ZERO);
        assert_eq!(r.dynamic_current, None);
        assert_eq!(r.max_current, None);
        assert_eq!(r.offered, Ampere::ZERO);
        assert!(!r.connected);
        let flags: Vec<_> = [
            Value::from("TRUE"),
            Value::from(0),
            Value::from(false),
            Value::Null,
            Value::from("maybe"),
        ]
        .iter()
        .map(flag)
        .collect();
        assert_eq!(flags, [Some(true), Some(false), Some(false), None, None]);
        assert_eq!(number(&Value::from(true)), Some(1.0));
        assert_eq!(number(&serde_json::json!([1])), None);
        assert_eq!(number(&serde_json::json!({})), None);
        let defaults = reading(&body(&[(109, 9.into()), (120, 0.into())]), at()).unwrap();
        assert!(defaults.connected, "no cloud flag: the poll went through");
        assert_eq!(defaults.mode, ChargerMode::Unknown(9));
    }

    #[test]
    fn rejects_responses_without_the_essentials() {
        let err = |b: &[u8]| reading(b, at()).unwrap_err().0;
        assert!(err(b"not json").starts_with("expected"));
        assert_eq!(err(&body(&[(120, 1.into())])), "no op mode (109)");
        assert_eq!(
            err(&body(&[(109, 300.into()), (120, 1.into())])),
            "no op mode (109)"
        );
        assert_eq!(
            err(&body(&[(109, Value::Null), (120, 1.into())])),
            "no op mode (109)"
        );
        assert_eq!(err(&body(&[(109, 3.into())])), "no power (120)");
        assert_eq!(
            ParseError("x".to_owned()).to_string(),
            "unexpected Easee response: x"
        );
    }

    #[test]
    fn command_bodies_use_whole_amps_and_minutes() {
        let w = |amps: f64, ttl: SignedDuration| {
            command_body(&ChargerWrite {
                current: Ampere::new(amps),
                ttl,
            })
        };
        assert_eq!(
            w(9.0, SignedDuration::from_mins(10)),
            serde_json::json!({"amps": 9, "minutes": 10})
        );
        assert_eq!(
            w(0.0, SignedDuration::from_secs(61)),
            serde_json::json!({"amps": 0, "minutes": 2})
        );
        assert_eq!(
            w(32.0, SignedDuration::ZERO),
            serde_json::json!({"amps": 32, "minutes": 1})
        );
    }

    #[test]
    fn command_outcomes() {
        let o = |s: &str| command_outcome(s.as_bytes());
        assert_eq!(o(r#"{"status":0}"#), CommandOutcome::Sent);
        assert_eq!(o(r#"{"status":1}"#), CommandOutcome::Expired);
        assert_eq!(o(r#"{"status":2}"#), CommandOutcome::Executed);
        assert_eq!(o(r#"{"status":3}"#), CommandOutcome::Rejected);
        assert_eq!(o(r#"{"status":7}"#), CommandOutcome::Accepted);
        assert_eq!(o(r#"{"commandId":5}"#), CommandOutcome::Accepted);
        assert_eq!(o(""), CommandOutcome::Accepted);
    }

    #[test]
    fn retry_after_takes_seconds_only() {
        assert_eq!(retry_after(Some(" 30")), Some(Duration::from_secs(30)));
        assert_eq!(retry_after(Some("Wed, 21 Oct 2026 07:28:00 GMT")), None);
        assert_eq!(retry_after(None), None);
    }

    #[test]
    fn backoff_doubles_honours_retry_after_and_resets() {
        let s = Duration::from_secs;
        let mut b = Backoff::new(s(30), s(600));
        assert_eq!(b.failure(None), s(30));
        assert_eq!(b.failure(None), s(60));
        assert_eq!(b.failure(Some(s(900))), s(900));
        for _ in 0..5 {
            b.failure(None);
        }
        assert_eq!(b.failure(None), s(600));
        b.reset();
        assert_eq!(b.failure(Some(s(5))), s(30));
    }

    #[test]
    fn tokens_refresh_at_four_fifths_of_their_life_and_never_print() {
        let now = Instant::now();
        let t = Tokens::new(
            TokenResponse {
                access_token: "secret-access".to_owned(),
                expires_in: 100,
                refresh_token: "secret-refresh".to_owned(),
            },
            now,
        );
        assert!(!t.due(now + Duration::from_secs(79)));
        assert!(t.due(now + Duration::from_secs(80)));
        assert!(!format!("{t:?}").contains("secret"));
        let bodies = (
            serde_json::to_value(Login {
                user_name: "u",
                password: "p",
            })
            .unwrap(),
            serde_json::to_value(Refresh { refresh_token: "r" }).unwrap(),
        );
        assert_eq!(
            bodies,
            (
                serde_json::json!({"userName": "u", "password": "p"}),
                serde_json::json!({"refreshToken": "r"})
            )
        );
    }
}
