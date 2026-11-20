//! The running link: polls the charger every `poll_interval` and sends the engine's writes.
//!
//! Readings go to the engine as [`Input::Charger`]. A failed poll is retried with
//! exponential backoff (30 s up to 10 min, at least the server's `Retry-After`); the engine
//! sees the charger go stale meanwhile and sheds only floors. A failed or rejected write is
//! only logged: the engine's actuator retries a write that a later poll doesn't confirm.

use std::sync::Arc;
use std::time::Duration;

use energybrain_config::EaseeConfig;
use energybrain_core::charger::ChargerWrite;
use energybrain_core::clock::Clock;
use energybrain_core::io::Input;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};

use crate::api::{Backoff, CommandOutcome};
use crate::client::{Client, Error};

/// Timing of the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Between successful polls.
    pub poll_interval: Duration,
    /// First wait after a failed poll.
    pub backoff_min: Duration,
    /// Longest wait after repeated failures (unless the server asks for longer).
    pub backoff_max: Duration,
}

impl Timing {
    /// The production timing for `cfg`.
    pub fn of(cfg: &EaseeConfig) -> Self {
        Self {
            poll_interval: cfg.poll_interval.unsigned_abs(),
            backoff_min: Duration::from_secs(30),
            backoff_max: Duration::from_secs(600),
        }
    }
}

/// The charger link.
#[derive(Debug)]
pub struct Link {
    client: Client,
    timing: Timing,
}

impl Link {
    /// A link for `cfg` with the production timing.
    pub fn new(cfg: EaseeConfig) -> Result<Self, Error> {
        let timing = Timing::of(&cfg);
        Self::with_timing(cfg, timing)
    }

    /// A link with explicit timing (tests, the simulator).
    pub fn with_timing(cfg: EaseeConfig, timing: Timing) -> Result<Self, Error> {
        Ok(Self {
            client: Client::new(cfg)?,
            timing,
        })
    }

    /// Runs until the engine side goes away: `writes` is closed (the app's stop signal) or
    /// `inputs` has no receiver. Polls at once, then every `poll_interval`.
    pub async fn run(
        mut self,
        clock: Arc<dyn Clock>,
        inputs: mpsc::Sender<Input>,
        mut writes: mpsc::Receiver<ChargerWrite>,
    ) {
        let mut backoff = Backoff::new(self.timing.backoff_min, self.timing.backoff_max);
        let mut next_poll = Instant::now();
        loop {
            tokio::select! {
                () = sleep_until(next_poll) => {
                    match self.client.read(clock.now()).await {
                        Ok(reading) => {
                            backoff.reset();
                            next_poll = Instant::now() + self.timing.poll_interval;
                            if inputs.send(Input::Charger(reading)).await.is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            let wait = backoff.failure(e.retry_after());
                            tracing::warn!(error = %e, retry_in = ?wait, "easee poll failed");
                            next_poll = Instant::now() + wait;
                        }
                    }
                }
                w = writes.recv() => {
                    let Some(w) = w else { return };
                    self.write(w).await;
                }
            }
        }
    }

    async fn write(&mut self, w: ChargerWrite) {
        match self.client.write(w).await {
            Ok(outcome @ (CommandOutcome::Rejected | CommandOutcome::Expired)) => {
                tracing::warn!(current = %w.current, ?outcome, "easee did not apply the charger limit");
            }
            Ok(outcome) => {
                tracing::info!(current = %w.current, ttl = ?w.ttl, ?outcome, "charger limit sent");
            }
            Err(e) => tracing::warn!(current = %w.current, error = %e, "charger limit failed"),
        }
    }
}
