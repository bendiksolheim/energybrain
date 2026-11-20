//! Client and link against the fake Easee.

use std::sync::Arc;
use std::time::Duration;

use energybrain_core::charger::{ChargerMode, ChargerWrite};
use energybrain_core::clock::{Clock, SimClock};
use energybrain_core::io::Input;
use energybrain_core::units::{Ampere, Kw};
use jiff::{SignedDuration, Timestamp};
use serde_json::json;
use tokio::sync::mpsc;

use crate::api::CommandOutcome;
use crate::fake::{self, Fail};
use crate::{Client, Error, Link, Timing};

fn at() -> Timestamp {
    "2026-11-20T17:00:00Z".parse().unwrap()
}

fn nine_amps() -> ChargerWrite {
    ChargerWrite {
        current: Ampere::new(9.0),
        ttl: SignedDuration::from_mins(10),
    }
}

#[tokio::test]
async fn logs_in_once_reads_and_writes() {
    let (cfg, fake) = fake::start().await;
    let mut c = Client::new(cfg).unwrap();
    let r = c.read(at()).await.unwrap();
    assert_eq!(r.at, at());
    assert_eq!(r.mode, ChargerMode::Charging);
    assert_eq!(r.power, Kw::new(2.3));
    assert_eq!(c.write(nine_amps()).await.unwrap(), CommandOutcome::Sent);
    c.read(at()).await.unwrap();
    let f = fake.lock().unwrap();
    assert_eq!((f.logins, f.refreshes, f.polls), (1, 0, 2));
    assert_eq!(f.commands, [json!({"amps": 9, "minutes": 10})]);
    assert!(!format!("{c:?}").contains(fake::PASSWORD));
}

#[tokio::test]
async fn refreshes_tokens_when_due_and_logs_in_again_when_refused() {
    let (cfg, fake) = fake::start().await;
    fake.lock().unwrap().expires_in = 0;
    let mut c = Client::new(cfg).unwrap();
    c.read(at()).await.unwrap();
    c.read(at()).await.unwrap();
    assert_eq!(fake.lock().unwrap().refreshes, 1, "refreshed when due");
    fake.lock().unwrap().refuse_refresh = true;
    c.read(at()).await.unwrap();
    let f = fake.lock().unwrap();
    assert_eq!((f.logins, f.refreshes), (2, 1), "refused refresh → login");
}

#[tokio::test]
async fn a_revoked_token_is_refreshed_and_the_call_retried_once() {
    let (cfg, fake) = fake::start().await;
    let mut c = Client::new(cfg).unwrap();
    c.read(at()).await.unwrap();
    fake.lock().unwrap().revoke_access = true;
    c.read(at()).await.unwrap();
    let f = fake.lock().unwrap();
    assert_eq!((f.logins, f.refreshes, f.polls), (1, 1, 2));
}

#[tokio::test]
async fn errors_are_typed() {
    let (cfg, fake) = fake::start().await;
    let mut c = Client::new(cfg.clone()).unwrap();
    fake.lock()
        .unwrap()
        .poll_failures
        .extend([Fail(429, Some("120")), Fail(500, None)]);
    let e = c.read(at()).await.unwrap_err();
    assert_eq!(e.retry_after(), Some(Duration::from_secs(120)));
    assert_eq!(
        e.to_string(),
        "easee rate limit hit (retry after Some(120s))"
    );
    let e = c.read(at()).await.unwrap_err();
    assert_eq!(e.to_string(), "easee HTTP 500: scripted failure");
    assert_eq!(e.retry_after(), None);

    fake.lock().unwrap().observations = json!({"observations": []});
    let e = c.read(at()).await.unwrap_err();
    assert!(matches!(e, Error::Parse(_)), "{e}");

    // Another charger's serial.
    let mut other = Client::new(energybrain_config::EaseeConfig {
        serial: "EH999999".to_owned(),
        ..cfg.clone()
    })
    .unwrap();
    assert!(matches!(
        other.read(at()).await,
        Err(Error::Status { status: 404, .. })
    ));
    assert!(matches!(
        other.write(nine_amps()).await,
        Err(Error::Status { status: 401, .. })
    ));

    // Wrong password, and a rate-limited login.
    let mut wrong = Client::new(energybrain_config::EaseeConfig {
        password: "nope".to_owned(),
        ..cfg.clone()
    })
    .unwrap();
    assert_eq!(
        wrong.read(at()).await.unwrap_err().to_string(),
        "easee login refused: HTTP 400"
    );
    fake.lock()
        .unwrap()
        .login_failures
        .push_back(Fail(429, None));
    assert!(matches!(
        Client::new(cfg.clone()).unwrap().read(at()).await,
        Err(Error::RateLimited { retry_after: None })
    ));

    // A login response without tokens.
    fake.lock()
        .unwrap()
        .login_failures
        .push_back(Fail(204, None));
    assert!(matches!(
        Client::new(cfg).unwrap().read(at()).await,
        Err(Error::Parse(_))
    ));
}

#[tokio::test]
async fn an_unreachable_server_is_a_network_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let (cfg, _fake) = fake::start().await;
    let mut c = Client::new(energybrain_config::EaseeConfig {
        api_url: format!("http://{addr}"),
        ..cfg
    })
    .unwrap();
    let e = c.read(at()).await.unwrap_err();
    assert!(e.to_string().starts_with("easee unreachable"), "{e}");
    assert_eq!(e.retry_after(), None);
}

fn fast() -> Timing {
    Timing {
        poll_interval: Duration::from_millis(20),
        backoff_min: Duration::from_millis(10),
        backoff_max: Duration::from_millis(40),
    }
}

/// Waits until the fake has seen `n` command attempts.
async fn attempts(fake: &fake::Shared, n: u32) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while fake.lock().unwrap().command_attempts < n {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn the_link_polls_backs_off_writes_and_stops_with_the_engine() {
    let (cfg, fake) = fake::start().await;
    assert_eq!(Timing::of(&cfg).poll_interval, Duration::from_secs(30));
    assert!(Link::new(cfg.clone()).is_ok());
    {
        let mut f = fake.lock().unwrap();
        f.poll_failures.push_back(Fail(500, None));
        f.command_response = json!({"status": 3});
    }
    let clock: Arc<dyn Clock> = Arc::new(SimClock::new(at()));
    let (in_tx, mut in_rx) = mpsc::channel(8);
    let (w_tx, w_rx) = mpsc::channel(8);
    // One poll that fails, a retry after the backoff, then nothing for an hour, so the
    // writes below are the only traffic.
    let timing = Timing {
        poll_interval: Duration::from_secs(3600),
        ..fast()
    };
    let link = Link::with_timing(cfg, timing).unwrap();
    let task = tokio::spawn(link.run(clock, in_tx, w_rx));
    let Some(Input::Charger(r)) = in_rx.recv().await else {
        panic!("expected a reading")
    };
    assert_eq!(r.at, at());
    assert_eq!(fake.lock().unwrap().polls, 2);

    // Rejected, executed, then failing (token revoked, refresh refused, login down).
    w_tx.send(nine_amps()).await.unwrap();
    attempts(&fake, 1).await;
    fake.lock().unwrap().command_response = json!({"status": 2});
    w_tx.send(nine_amps()).await.unwrap();
    attempts(&fake, 2).await;
    {
        let mut f = fake.lock().unwrap();
        f.revoke_access = true;
        f.refuse_refresh = true;
        f.login_failures.push_back(Fail(500, None));
    }
    w_tx.send(nine_amps()).await.unwrap();
    attempts(&fake, 3).await;

    // Closing the write channel stops the link.
    drop(w_tx);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fake.lock().unwrap().commands.len(), 2);
}

#[tokio::test]
async fn the_link_stops_when_nobody_listens() {
    let (cfg, _fake) = fake::start().await;
    let clock: Arc<dyn Clock> = Arc::new(SimClock::new(at()));
    let (in_tx, in_rx) = mpsc::channel(1);
    let (_w_tx, w_rx) = mpsc::channel(1);
    drop(in_rx);
    let link = Link::with_timing(cfg, fast()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), link.run(clock, in_tx, w_rx))
        .await
        .unwrap();
}
