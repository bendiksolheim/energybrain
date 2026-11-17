//! `energybrain-sim`: energybrain and a simulated house in one process, on an accelerated
//! clock. Process plumbing only (excluded from coverage; keep it branch-free).
//!
//!     energybrain-sim [config/dev.toml] [config/sim.toml]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use energybrain::{Runtime, SystemClock, run, sqlite_events, sqlite_history};
use energybrain_core::clock::{Clock, ScaledClock};
use energybrain_core::io::Output;
use energybrain_sim::{House, driver};
use energybrain_store::{SqliteStore, Store};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let args: Vec<String> = std::env::args().collect();
    let eb_path = args.get(1).map_or("config/dev.toml", String::as_str);
    let sim_path = args.get(2).map_or("config/sim.toml", String::as_str);
    let config = energybrain_config::load(eb_path.as_ref(), &|k| std::env::var(k).ok())?;
    let sim = energybrain_sim::load(sim_path.as_ref(), &config)?;

    let clock = Arc::new(ScaledClock::new(
        Arc::new(SystemClock),
        sim.start,
        sim.factor,
    ));
    tracing::info!(start = %sim.start, factor = sim.factor.get(), "simulated time");

    // The dev database is fresh each run (`just dev` deletes it); seed the starting mode.
    std::fs::create_dir_all(config.store_path.parent().unwrap_or(Path::new(".")))?;
    let mut store =
        SqliteStore::open(&config.store_path)?.with_sample_retention(config.sample_retention);
    let seed: Vec<Output> = sim
        .initial_mode
        .map(Output::PersistMode)
        .into_iter()
        .collect();
    store.apply(&seed)?;

    let (house, _) = House::new(&sim);
    let mut house_mqtt = config.mqtt.clone();
    house_mqtt.client_id = format!("{}-sim", config.mqtt.client_id);
    let (link, publisher) =
        energybrain_mqtt::connect(&house_mqtt, house.subscriptions(), Duration::from_secs(5));

    let (ctrl_c, stopped) = watch::channel(false);
    let mut house_stopped = stopped.clone();
    let mut eb_stopped = stopped;
    let house_step = clock
        .real(Duration::from_millis(500))
        .max(Duration::from_millis(10));
    let house_task = tokio::spawn(driver::run(
        house,
        link,
        publisher,
        clock.clone() as Arc<dyn Clock>,
        house_step,
        async move {
            let _ = house_stopped.changed().await;
        },
    ));
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = ctrl_c.send(true);
    });

    let events = sqlite_events(config.store_path.clone());
    let history = sqlite_history(config.store_path.clone());
    let listener = tokio::net::TcpListener::bind(config.http_listen).await?;
    let runtime = Runtime {
        tick: clock.real(Duration::from_secs(10)),
        config,
        clock,
        store: Box::new(store),
        events,
        history,
        listener,
        retry_delay: Duration::from_secs(5),
    };
    run(runtime, async move {
        let _ = eb_stopped.changed().await;
    })
    .await?;
    house_task.await?;
    Ok(())
}
