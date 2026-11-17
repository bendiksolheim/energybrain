//! The energybrain service: the runtime loop wiring the engine to MQTT, SQLite and HTTP.
//!
//! [`run`] owns the [`Engine`]. It multiplexes MQTT events (from a forwarding task, so
//! `select!` stays cancellation-safe), a tick interval and the shutdown signal, and fans
//! the engine's outputs out to MQTT, the store thread, the HTTP state and the logs.

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use energybrain_config::Config;
use energybrain_core::clock::Clock;
use energybrain_core::engine::Engine;
use energybrain_core::io::{Input, Output, Status};
use energybrain_http::{
    AppState, Detail, EventQuery, History, HistoryQuery, MAX_DAY_EVENTS, Metrics, Site, router,
};
use energybrain_mqtt::{Decoded, Link, Publisher, Translator, TransportEvent, connect};
use energybrain_store::{SqliteStore, Store, StoreError};
use jiff::{SignedDuration, Timestamp};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{error, info, warn};

/// Crate version, as reported at startup.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    // The one place allowed to read the system clock; everything else gets `now` passed in.
    #[allow(clippy::disallowed_methods)]
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// Stored events from the `SQLite` file at `path`, for `/api/events`. Opens its own
/// connection per query so it never contends with the writer thread's connection.
pub fn sqlite_events(path: PathBuf) -> EventQuery {
    Arc::new(move |since, limit| {
        SqliteStore::open(&path)
            .and_then(|s| s.events(since, limit))
            .map_err(|e| e.to_string())
    })
}

/// The history behind `/api/day` and `/api/month`, from the `SQLite` file at `path` (one
/// connection per query, like [`sqlite_events`]).
pub fn sqlite_history(path: PathBuf) -> HistoryQuery {
    Arc::new(move |from, to, detail| {
        SqliteStore::open(&path)
            .and_then(|s| read_history(&s, from, to, detail))
            .map_err(|e| e.to_string())
    })
}

fn read_history(
    store: &SqliteStore,
    from: Timestamp,
    to: Timestamp,
    detail: Detail,
) -> Result<History, StoreError> {
    let hours = store.hours(from, to)?;
    Ok(match detail {
        Detail::Hours => History {
            hours,
            ..History::default()
        },
        Detail::Full => History {
            hours,
            samples: store.samples(from, to)?,
            zone_hours: store.zone_hours(from, to)?,
            events: store.events_between(from, to, MAX_DAY_EVENTS)?,
        },
    })
}

/// What the history views need from the configuration.
fn site(config: &Config) -> Site {
    let e = &config.engine;
    Site {
        calendar: e.calendar.clone(),
        tariff: e.tariff.clone(),
        target: e.target,
        zones: e
            .zones
            .iter()
            .map(|z| (z.id.clone(), z.name.clone()))
            .collect(),
    }
}

/// Everything [`run`] needs.
pub struct Runtime {
    /// Validated configuration.
    pub config: Config,
    /// Time source.
    pub clock: Arc<dyn Clock>,
    /// Persistence (moved to its own thread).
    pub store: Box<dyn Store>,
    /// Stored-event lookup for HTTP.
    pub events: EventQuery,
    /// History lookup for HTTP.
    pub history: HistoryQuery,
    /// Bound HTTP listener.
    pub listener: TcpListener,
    /// Evaluation interval (10 s in production).
    pub tick: Duration,
    /// Pause between MQTT reconnect attempts.
    pub retry_delay: Duration,
}

/// Hours older than this are not loaded; the ledger only needs the current month.
const HISTORY: SignedDuration = SignedDuration::from_hours(32 * 24);

/// Runs the service until `shutdown` completes.
pub async fn run(rt: Runtime, shutdown: impl Future<Output = ()>) -> Result<(), StoreError> {
    let Runtime {
        config,
        clock,
        store,
        events,
        history,
        listener,
        tick,
        retry_delay,
    } = rt;
    let now = clock.now();
    let persisted = store.load(now - HISTORY)?;
    let mut engine = Engine::new(config.engine.clone(), persisted, now);
    info!(version = version(), mode = %engine.mode(), zones = config.engine.zones.len(), "starting");

    let translator = Translator::new(&config.topics, config.engine.calendar.time_zone().clone());
    let (link, publisher) = connect(&config.mqtt, translator.subscriptions(), retry_delay);
    let (mut mqtt_events, link_task) = spawn_link(link);

    let (status_tx, status_rx) = watch::channel(None);
    let (mqtt_tx, mqtt_rx) = watch::channel(false);
    let metrics = Arc::new(Mutex::new(Metrics::default()));
    let app = router(AppState {
        status: status_rx,
        mqtt_connected: mqtt_rx,
        clock: clock.clone(),
        metrics: metrics.clone(),
        events,
        history,
        site: Arc::new(site(&config)),
        max_status_age: SignedDuration::from_secs(30)
            .max(SignedDuration::try_from(tick * 3).unwrap_or_default()),
    });
    let (http_stop, http_stopped) = oneshot::channel::<()>();
    let http = tokio::spawn(async move {
        // `serve` only returns once shutdown is requested; accept errors are retried inside.
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = http_stopped.await;
            })
            .await;
    });

    let (store_tx, store_thread) = spawn_store(store);
    let mut sink = Sink {
        translator,
        publisher,
        store_tx,
        status_tx,
        metrics,
    };

    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            _ = ticker.tick() => {
                let out = engine.handle(Input::Tick { at: clock.now() });
                sink.emit(out).await;
            }
            Some(event) = mqtt_events.recv() => {
                on_mqtt(event, &mut engine, &mut sink, &mqtt_tx, clock.as_ref()).await;
            }
        }
    }

    info!("shutting down");
    let out = engine.shutdown(clock.now());
    sink.emit(out).await;
    finish(sink, mqtt_events, link_task, store_thread, http_stop, http).await;
    info!("stopped");
    Ok(())
}

/// Handles one MQTT transport event.
async fn on_mqtt(
    event: TransportEvent,
    engine: &mut Engine,
    sink: &mut Sink,
    mqtt_tx: &watch::Sender<bool>,
    clock: &dyn Clock,
) {
    match event {
        TransportEvent::Connected => {
            info!("mqtt connected");
            mqtt_tx.send_replace(true);
            sink.republish().await;
        }
        TransportEvent::Disconnected(reason) => {
            warn!(%reason, "mqtt disconnected; retrying");
            mqtt_tx.send_replace(false);
        }
        TransportEvent::Message { topic, payload } => {
            match sink.translator.decode(&topic, &payload, clock.now()) {
                Decoded::Inputs(inputs) => {
                    for input in inputs {
                        let out = engine.handle(input);
                        sink.emit(out).await;
                    }
                }
                Decoded::Ignored => {}
                Decoded::Invalid { topic, reason } => {
                    warn!(%topic, %reason, "ignoring malformed message");
                }
            }
        }
    }
}

/// Waits until the link reports a disconnect (or ends).
async fn until_disconnected(events: &mut mpsc::UnboundedReceiver<TransportEvent>) {
    let mut next = events.recv().await;
    while next
        .as_ref()
        .is_some_and(|e| !matches!(e, TransportEvent::Disconnected(_)))
    {
        next = events.recv().await;
    }
}

/// Flushes MQTT, then stops the link, the store thread and the HTTP server.
async fn finish(
    sink: Sink,
    mut mqtt_events: mpsc::UnboundedReceiver<TransportEvent>,
    link_task: tokio::task::JoinHandle<()>,
    store_thread: std::thread::JoinHandle<()>,
    http_stop: oneshot::Sender<()>,
    http: tokio::task::JoinHandle<()>,
) {
    // Let the event loop flush what is queued, then stop it.
    let _ = sink.publisher.disconnect().await;
    let _ =
        tokio::time::timeout(Duration::from_secs(2), until_disconnected(&mut mqtt_events)).await;
    link_task.abort();
    drop(sink);
    let _ = tokio::task::spawn_blocking(move || store_thread.join()).await;
    let _ = http_stop.send(());
    let _ = http.await;
}

/// Runs the MQTT link in its own task and forwards its events.
fn spawn_link(
    mut link: Link,
) -> (
    mpsc::UnboundedReceiver<TransportEvent>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move { while tx.send(link.next().await).is_ok() {} });
    (rx, task)
}

/// Runs the store on its own thread; batches of outputs arrive over a channel.
fn spawn_store(
    mut store: Box<dyn Store>,
) -> (
    std::sync::mpsc::Sender<Vec<Output>>,
    std::thread::JoinHandle<()>,
) {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<Output>>();
    let thread = std::thread::spawn(move || {
        for batch in rx {
            if let Err(e) = store.apply(&batch) {
                error!(error = %e, "persisting failed");
            }
        }
    });
    (tx, thread)
}

/// Where engine outputs go.
struct Sink {
    translator: Translator,
    publisher: Publisher,
    store_tx: std::sync::mpsc::Sender<Vec<Output>>,
    status_tx: watch::Sender<Option<Arc<Status>>>,
    metrics: Arc<Mutex<Metrics>>,
}

impl Sink {
    async fn emit(&mut self, outputs: Vec<Output>) {
        for o in &outputs {
            self.metrics
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .record(o);
            log(o);
            if let Output::PublishStatus(s) = o {
                self.status_tx
                    .send_replace(Some(Arc::new(s.as_ref().clone())));
            }
            for p in self.translator.encode(o) {
                // Fails only once the link task is gone, i.e. during shutdown.
                let _ = self.publisher.publish(&p).await;
            }
        }
        // Fails only if the store thread is gone, i.e. during shutdown.
        let _ = self.store_tx.send(outputs);
    }

    async fn republish(&mut self) {
        for p in self.translator.retained() {
            let _ = self.publisher.publish(&p).await;
        }
    }
}

/// Logs setpoint decisions (with their reasons) and events.
fn log(o: &Output) {
    match o {
        Output::WriteSetpoint {
            zone,
            value,
            reasons,
        } => {
            let reasons: Vec<String> = reasons.iter().map(ToString::to_string).collect();
            info!(%zone, %value, reasons = reasons.join("; "), "write setpoint");
        }
        Output::WouldWrite {
            zone,
            value,
            reasons,
        } => {
            let reasons: Vec<String> = reasons.iter().map(ToString::to_string).collect();
            info!(%zone, %value, reasons = reasons.join("; "), "would write setpoint (observe)");
        }
        Output::PersistEvent(e) => {
            // Serializing plain data structs to a String cannot fail.
            #[allow(clippy::expect_used)]
            let json = serde_json::to_string(e).expect("event serializes");
            info!(event = json, "event");
        }
        Output::PublishStatus(_)
        | Output::PersistHour(_)
        | Output::PersistZoneWrite { .. }
        | Output::PersistZoneBase { .. }
        | Output::PersistOverride { .. }
        | Output::PersistZoneHour { .. }
        | Output::PersistZoneStats { .. }
        | Output::PersistMode(_)
        | Output::PersistShedAllowed { .. } => {}
    }
}

#[cfg(test)]
mod tests;
