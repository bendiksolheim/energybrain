//! Process plumbing only (excluded from coverage; keep it branch-free).

use std::sync::Arc;
use std::time::Duration;

use energybrain::{Runtime, SystemClock, run, sqlite_events};
use energybrain_store::SqliteStore;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let logs = tracing_subscriber::fmt().with_env_filter(filter);
    match std::env::var("ENERGYBRAIN_LOG_FORMAT").as_deref() {
        Ok("json") => logs.json().init(),
        _ => logs.init(),
    }

    let path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("ENERGYBRAIN_CONFIG").ok())
        .unwrap_or_else(|| "config/example.toml".to_owned());
    let config = energybrain_config::load(path.as_ref(), &|k| std::env::var(k).ok())?;
    let store = SqliteStore::open(&config.store_path)?;
    let events = sqlite_events(config.store_path.clone());
    let listener = tokio::net::TcpListener::bind(config.http_listen).await?;
    let mut terminate = signal(SignalKind::terminate())?;
    let shutdown = async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    };

    let runtime = Runtime {
        config,
        clock: Arc::new(SystemClock),
        store: Box::new(store),
        events,
        listener,
        tick: Duration::from_secs(10),
        retry_delay: Duration::from_secs(5),
    };
    run(runtime, shutdown).await?;
    Ok(())
}
