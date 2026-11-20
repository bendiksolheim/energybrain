//! The HTTP client: logs in, keeps the tokens fresh, and makes the two calls energybrain needs.
//!
//! Tokens are refreshed at 80 % of their lifetime; a failed refresh (refresh tokens are single
//! use and may be revoked) falls back to logging in with the password. A 401 refreshes once
//! and retries the call once. 429 becomes [`Error::RateLimited`] with the server's
//! `Retry-After`, for the caller's backoff.

use std::time::Duration;

use energybrain_config::EaseeConfig;
use energybrain_core::charger::{ChargerReading, ChargerWrite};
use jiff::Timestamp;
use reqwest::{RequestBuilder, Response, StatusCode};
use tokio::time::Instant;

use crate::api::{self, CommandOutcome, Login, Refresh, TokenResponse, Tokens};

/// Every request gives up after this.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Error bodies are cut to this many characters in messages.
const BODY_LIMIT: usize = 200;

/// Why a call failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The request didn't get a response (DNS, connect, TLS, timeout).
    #[error("easee unreachable: {0}")]
    Network(#[from] reqwest::Error),
    /// Too many requests; wait at least `retry_after` if given.
    #[error("easee rate limit hit (retry after {retry_after:?})")]
    RateLimited {
        /// The server's `Retry-After`.
        retry_after: Option<Duration>,
    },
    /// The account was refused (wrong user name or password).
    #[error("easee login refused: HTTP {0}")]
    Login(u16),
    /// Any other unsuccessful status.
    #[error("easee HTTP {status}: {body}")]
    Status {
        /// The status code.
        status: u16,
        /// The start of the response body.
        body: String,
    },
    /// The response didn't have the expected shape.
    #[error(transparent)]
    Parse(#[from] api::ParseError),
}

impl Error {
    /// How long the server asked us to wait, if it did.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            Self::Network(_) | Self::Login(_) | Self::Status { .. } | Self::Parse(_) => None,
        }
    }
}

/// An Easee API client for one charger.
pub struct Client {
    http: reqwest::Client,
    cfg: EaseeConfig,
    tokens: Option<Tokens>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("cfg", &self.cfg)
            .field("tokens", &self.tokens)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client that logs in on its first call.
    pub fn new(cfg: EaseeConfig) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("energybrain/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            cfg,
            tokens: None,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.cfg.api_url)
    }

    /// Polls the charger's observations; the reading is stamped `at`.
    pub async fn read(&mut self, at: Timestamp) -> Result<ChargerReading, Error> {
        let url = self.url(&format!(
            "/state/{}/observations?ids={}",
            self.cfg.serial,
            api::OBSERVATION_IDS
        ));
        let resp = self
            .send(|http, token| http.get(&url).bearer_auth(token))
            .await?;
        Ok(api::reading(&resp.bytes().await?, at)?)
    }

    /// Sets the charger's dynamic current.
    pub async fn write(&mut self, w: ChargerWrite) -> Result<CommandOutcome, Error> {
        let url = self.url(&format!(
            "/api/chargers/{}/commands/set_dynamic_charger_current",
            self.cfg.serial
        ));
        let body = api::command_body(&w);
        let resp = self
            .send(|http, token| http.post(&url).json(&body).bearer_auth(token))
            .await?;
        Ok(api::command_outcome(&resp.bytes().await?))
    }

    /// Sends an authenticated request; on 401 refreshes the token once and retries once.
    async fn send(
        &mut self,
        request: impl Fn(&reqwest::Client, &str) -> RequestBuilder,
    ) -> Result<Response, Error> {
        let token = self.token().await?;
        let resp = request(&self.http, &token).send().await?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            return check(resp).await;
        }
        let now = Instant::now();
        self.tokens.iter_mut().for_each(|t| t.refresh_after = now);
        let token = self.token().await?;
        check(request(&self.http, &token).send().await?).await
    }

    /// A usable access token: logs in, or refreshes when due (falling back to a login).
    async fn token(&mut self) -> Result<String, Error> {
        let now = Instant::now();
        let tokens = match self.tokens.take() {
            Some(t) if !t.due(now) => t,
            Some(t) => match self.refresh(&t.refresh).await {
                Ok(fresh) => fresh,
                Err(e) => {
                    tracing::info!(error = %e, "easee token refresh failed; logging in again");
                    self.login().await?
                }
            },
            None => self.login().await?,
        };
        let access = tokens.access.clone();
        self.tokens = Some(tokens);
        Ok(access)
    }

    async fn login(&self) -> Result<Tokens, Error> {
        let body = Login {
            user_name: &self.cfg.username,
            password: &self.cfg.password,
        };
        let resp = self
            .http
            .post(self.url("/api/accounts/login"))
            .json(&body)
            .send()
            .await?;
        match resp.status() {
            StatusCode::TOO_MANY_REQUESTS => Err(rate_limited(&resp)),
            s if s.is_success() => tokens(resp).await,
            s => Err(Error::Login(s.as_u16())),
        }
    }

    async fn refresh(&self, refresh_token: &str) -> Result<Tokens, Error> {
        let resp = self
            .http
            .post(self.url("/api/accounts/refresh_token"))
            .json(&Refresh { refresh_token })
            .send()
            .await?;
        tokens(check(resp).await?).await
    }
}

async fn tokens(resp: Response) -> Result<Tokens, Error> {
    let r: TokenResponse = serde_json::from_slice(&resp.bytes().await?)
        .map_err(|e| api::ParseError(format!("token response: {e}")))?;
    Ok(Tokens::new(r, Instant::now()))
}

fn rate_limited(resp: &Response) -> Error {
    let header = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok());
    Error::RateLimited {
        retry_after: api::retry_after(header),
    }
}

/// Turns an unsuccessful status into an error.
async fn check(resp: Response) -> Result<Response, Error> {
    let status = resp.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(rate_limited(&resp));
    }
    if status.is_success() {
        return Ok(resp);
    }
    let body: String = resp.text().await?.chars().take(BODY_LIMIT).collect();
    Err(Error::Status {
        status: status.as_u16(),
        body,
    })
}
