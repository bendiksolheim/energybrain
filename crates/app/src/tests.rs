//! Runtime tests: an end-to-end run against an embedded broker, a temp SQLite file and the
//! real HTTP listener, plus failure paths.

use std::net::TcpListener as StdListener;
use std::path::Path;
use std::time::Duration;

use energybrain_core::clock::SimClock;
use energybrain_core::engine::PersistedState;
use energybrain_core::mode::Mode;
use energybrain_core::units::Celsius;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::*;

const NOW: &str = "2026-11-15T17:30:00Z";
const SETPOINT: &str = "zwave/Bathroom/Floor_bathroom/67/1/setpoint/1";

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn free_port() -> u16 {
    StdListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// An embedded rumqttd broker on a free port requiring `user`/`pass`.
fn broker() -> u16 {
    let port = free_port();
    let toml = format!(
        r#"
        id = 0
        [router]
        id = 0
        max_connections = 100
        max_outgoing_packet_count = 200
        max_segment_size = 1048576
        max_segment_count = 10
        [v4.1]
        name = "v4-1"
        listen = "127.0.0.1:{port}"
        next_connection_delay_ms = 1
        [v4.1.connections]
        connection_timeout_ms = 5000
        max_payload_size = 65536
        max_inflight_count = 100
        dynamic_filters = true
        [v4.1.connections.auth]
        user = "pass"
        "#
    );
    let config: rumqttd::Config = toml::from_str(&toml).unwrap();
    std::thread::spawn(move || {
        let _ = rumqttd::Broker::new(config).start();
    });
    port
}

fn config(mqtt_port: u16, store: &Path) -> Config {
    let text = format!(
        r#"
        [mqtt]
        host = "127.0.0.1"
        port = {mqtt_port}
        username = "user"
        password = "pass"
        [store]
        path = "{}"
        [meter]
        topic = "tibber"
        [tariff]
        tiers = [{{ to_kw = 5, price = 233 }}, {{ price = 397 }}]
        target_kw = 5
        [[zone]]
        id = "bathroom"
        name = "Bathroom"
        device = "zwave/Bathroom/Floor_bathroom"
        priority = 1
        "#,
        store.display()
    );
    energybrain_config::parse(&text, &|_| None).unwrap()
}

async fn runtime(cfg: Config, store: Box<dyn Store>, events: EventQuery) -> (Runtime, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let rt = Runtime {
        history: sqlite_history(cfg.store_path.clone()),
        config: cfg,
        clock: Arc::new(SimClock::new(ts(NOW))),
        store,
        events,
        listener,
        tick: Duration::from_millis(50),
        retry_delay: Duration::from_millis(50),
        charger: None,
        site_root: "target/site".into(),
    };
    (rt, port)
}

async fn http_get(port: u16, path: &str) -> String {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../../tests/fixtures/han/{name}.hex",
        env!("CARGO_MANIFEST_DIR")
    );
    let hex = std::fs::read_to_string(path).unwrap();
    let hex = hex.trim();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// A plain MQTT client standing in for the Pulse, the thermostats and HomeKit.
struct World {
    client: AsyncClient,
    seen: mpsc::UnboundedReceiver<(String, String)>,
}

impl World {
    async fn connect(port: u16) -> Self {
        let mut opts = MqttOptions::new("world", "127.0.0.1", port);
        opts.set_credentials("user", "pass");
        let (client, mut eventloop) = AsyncClient::new(opts, 64);
        client.subscribe("zwave/#", QoS::AtLeastOnce).await.unwrap();
        client
            .subscribe("energybrain/#", QoS::AtLeastOnce)
            .await
            .unwrap();
        let (tx, seen) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        let payload = String::from_utf8_lossy(&p.payload).into_owned();
                        let _ = tx.send((p.topic, payload));
                    }
                    Ok(_) => {}
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
        });
        Self { client, seen }
    }

    async fn send(&self, topic: &str, payload: impl Into<Vec<u8>>) {
        self.client
            .publish(topic, QoS::AtLeastOnce, false, payload.into())
            .await
            .unwrap();
    }

    async fn expect(&mut self, topic: &str, payload: &str) {
        timeout(Duration::from_secs(10), async {
            while let Some((t, p)) = self.seen.recv().await {
                if t == topic && p == payload {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("never saw {topic} = {payload}"));
    }
}

#[test]
fn version_is_the_package_version() {
    assert_eq!(version(), "0.1.0");
}

#[test]
fn system_clock_reads_the_real_time() {
    assert!(SystemClock.now() > ts("2026-01-01T00:00:00Z"));
}

#[test]
fn event_lookup_reports_store_errors() {
    let dir = tempfile::tempdir().unwrap();
    let query = sqlite_events(dir.path().to_owned());
    assert!(query(None, 10).unwrap_err().starts_with("sqlite: "));
}

#[test]
fn history_reads_what_each_view_needs() {
    use energybrain_core::energy::{HourEnergy, Quality};
    use energybrain_core::io::{Event, EventKind};
    use energybrain_core::time::HourStart;
    use energybrain_core::units::Kwh;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("h.db");
    let hour = HourStart::containing(ts("2026-11-15T17:00:00Z"));
    SqliteStore::open(&db)
        .unwrap()
        .apply(&[
            Output::PersistHour(HourEnergy {
                hour,
                kwh: Kwh::new(4.2),
                quality: Quality::Measured,
            }),
            Output::PersistZoneHour {
                zone: energybrain_core::ids::ZoneId::new("bathroom").unwrap(),
                hour,
                kwh: Kwh::new(1.1),
            },
            Output::PersistEvent(Event {
                at: ts("2026-11-15T17:20:00Z"),
                mode: Mode::Observe,
                kind: EventKind::ModeChanged {
                    from: Mode::Observe,
                    to: Mode::Active,
                },
            }),
        ])
        .unwrap();
    let query = sqlite_history(db);
    let (from, to) = (ts("2026-11-15T00:00:00Z"), ts("2026-11-16T00:00:00Z"));
    let full = query(from, to, Detail::Full).unwrap();
    assert_eq!(
        (full.hours.len(), full.zone_hours.len(), full.events.len()),
        (1, 1, 1)
    );
    assert_eq!(
        query(from, to, Detail::Hours).unwrap(),
        History {
            hours: full.hours,
            ..History::default()
        }
    );
    let broken = sqlite_history(dir.path().to_owned());
    assert!(
        broken(from, to, Detail::Hours)
            .unwrap_err()
            .starts_with("sqlite: ")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn end_to_end_shed_report_and_restore_on_shutdown() {
    let mqtt_port = broker();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("eb.db");
    let cfg = config(mqtt_port, &db);
    let store = Box::new(SqliteStore::open(&db).unwrap());
    let (rt, http_port) = runtime(cfg, store, sqlite_events(db.clone())).await;
    let (stop, stopped) = oneshot::channel::<()>();
    let service = tokio::spawn(run(rt, async {
        let _ = stopped.await;
    }));

    let mut world = World::connect(mqtt_port).await;
    // First start: observe mode, announced retained.
    world.expect("energybrain/mode", "observe").await;

    world.send(SETPOINT, "26.5").await;
    world
        .send("zwave/Bathroom/Floor_bathroom/50/1/value/66049", "1500")
        .await;
    world
        .send(
            "zwave/Bathroom/Floor_bathroom/50/1/value/65537",
            "not a number",
        )
        .await;
    world
        .send("tibber", br#"{"status":{"rssi":-57}}"#.to_vec())
        .await;
    // The meter reports 6.4 kW: far above the 4.7 kWh limit. In observe mode the guard
    // sheds Bathroom on paper only (status topics, no write).
    world.send("tibber", fixture("kaifa_ma304h3e_list1")).await;
    world.expect("energybrain/peakguard/active", "1").await;
    world.expect("energybrain/zone/bathroom/shed", "1").await;
    // Going active makes it real.
    // (The write comes before the status publish within one tick.)
    world.send("energybrain/mode/set", "active").await;
    world.expect(&format!("{SETPOINT}/set"), "22.5").await;
    world.expect("energybrain/mode", "active").await;
    world.send(SETPOINT, "22.5").await; // the thermostat's echo

    let health = http_get(http_port, "/healthz").await;
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    let events = http_get(http_port, "/api/events").await;
    assert!(events.contains(r#""type":"shed""#), "{events}");
    // The web app's pages render from the store. The clock stands still, so the sample log
    // holds the first status only (17:30Z, embedded in the page's resolved data).
    let day = http_get(http_port, "/").await;
    assert!(day.starts_with("HTTP/1.1 200"), "{day}");
    assert!(day.contains("Sunday 15 November 2026"), "{day}");
    assert!(day.contains("[1794763800]"), "{day}");
    let month = http_get(http_port, "/month").await;
    assert!(month.contains("November 2026"), "{month}");
    let metrics = http_get(http_port, "/metrics").await;
    assert!(
        metrics.contains("energybrain_zone_writes_total{zone=\"bathroom\"} 1"),
        "{metrics}"
    );
    assert!(
        metrics.contains("energybrain_zone_would_write_total{zone=\"bathroom\"} 1"),
        "{metrics}"
    );

    // Stopping restores Bathroom and keeps everything for the next start.
    stop.send(()).unwrap();
    world.expect(&format!("{SETPOINT}/set"), "26.5").await;
    timeout(Duration::from_secs(10), service)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let state = SqliteStore::open(&db)
        .unwrap()
        .load(ts("2026-11-01T00:00:00Z"))
        .unwrap();
    assert_eq!(state.mode, Some(Mode::Active));
    let bathroom = &state.zones[&energybrain_core::ids::ZoneId::new("bathroom").unwrap()];
    assert_eq!(bathroom.base, Some(Celsius::new(26.5)));
    assert_eq!(
        bathroom.last_write.map(|(v, _)| v),
        Some(Celsius::new(26.5))
    );
    assert_eq!(
        bathroom.heating_kw,
        Some(energybrain_core::units::Kw::new(1.5))
    );
}

/// A store that can fail on demand.
struct FailingStore {
    load_fails: bool,
}

impl Store for FailingStore {
    fn load(&self, _: Timestamp) -> Result<PersistedState, StoreError> {
        if self.load_fails {
            Err(StoreError::Corrupt("disk on fire".to_owned()))
        } else {
            Ok(PersistedState::default())
        }
    }

    fn apply(&mut self, _: &[Output]) -> Result<(), StoreError> {
        Err(StoreError::Corrupt("read-only".to_owned()))
    }
}

fn no_events() -> EventQuery {
    Arc::new(|_, _| Ok(vec![]))
}

#[tokio::test]
async fn a_failing_load_stops_startup() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(free_port(), &dir.path().join("x.db"));
    let (rt, _) = runtime(
        cfg,
        Box::new(FailingStore { load_fails: true }),
        no_events(),
    )
    .await;
    let err = run(rt, std::future::pending()).await.unwrap_err();
    assert_eq!(err.to_string(), "corrupt store: disk on fire");
    assert_eq!(
        no_events()(None, 1).unwrap(),
        Vec::<serde_json::Value>::new()
    );
}

#[tokio::test]
async fn dead_broker_and_failing_writes_keep_running_and_stop_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(free_port(), &dir.path().join("x.db"));
    let (rt, http_port) = runtime(
        cfg,
        Box::new(FailingStore { load_fails: false }),
        no_events(),
    )
    .await;
    let (stop, stopped) = oneshot::channel::<()>();
    let service = tokio::spawn(run(rt, async {
        let _ = stopped.await;
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let health = http_get(http_port, "/healthz").await;
    assert!(health.starts_with("HTTP/1.1 503"), "{health}");
    assert!(health.contains("mqtt disconnected"), "{health}");
    stop.send(()).unwrap();
    timeout(Duration::from_secs(10), service)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn flushing_skips_events_until_the_disconnect() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tx.send(TransportEvent::Message {
        topic: "t".to_owned(),
        payload: vec![],
    })
    .unwrap();
    tx.send(TransportEvent::Disconnected("bye".to_owned()))
        .unwrap();
    tx.send(TransportEvent::Connected).unwrap();
    until_disconnected(&mut rx).await;
    assert_eq!(rx.recv().await, Some(TransportEvent::Connected));
    drop(tx);
    until_disconnected(&mut rx).await;
}

/// docs/architecture/system.d2 is hand-written: every workspace crate must have a box there,
/// labelled with its package name (`"energybrain-core\n…` or `"energybrain (app)\n…`).
#[test]
fn architecture_diagram_names_every_crate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let diagram = std::fs::read_to_string(root.join("docs/architecture/system.d2")).unwrap();
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(root.join("crates")).unwrap() {
        let manifest = std::fs::read_to_string(entry.unwrap().path().join("Cargo.toml")).unwrap();
        let name = manifest
            .lines()
            .find_map(|l| l.strip_prefix("name = \""))
            .and_then(|l| l.strip_suffix('"'))
            .unwrap()
            .to_owned();
        if !diagram.contains(&format!("\"{name}\\n")) && !diagram.contains(&format!("\"{name} (")) {
            missing.push(name);
        }
    }
    assert_eq!(
        missing,
        Vec::<String>::new(),
        "add these crates to system.d2"
    );
}

/// A minimal Easee cloud: a car charging at 15 A, recording every command it gets.
async fn fake_easee() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    use axum::Json;
    use axum::routing::{get, post};
    use serde_json::{Value, json};

    let commands = Arc::new(Mutex::new(Vec::new()));
    let recorded = commands.clone();
    let app = axum::Router::new()
        .route(
            "/api/accounts/login",
            post(|| async {
                Json(json!({"accessToken": "a", "expiresIn": 3600, "refreshToken": "r"}))
            }),
        )
        .route(
            "/state/{serial}/observations",
            get(|| async {
                Json(json!({"observations": [
                    {"id": 109, "value": 3},
                    {"id": 120, "value": 3.45},
                    {"id": 47, "value": 16},
                    {"id": 114, "value": 15},
                ]}))
            }),
        )
        .route(
            "/api/chargers/{serial}/commands/set_dynamic_charger_current",
            post(move |Json(body): Json<Value>| async move {
                recorded.lock().unwrap().push(body);
                Json(json!({"status": 0}))
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, commands)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_charger_is_limited_through_easee_and_released_from_homekit() {
    let mqtt_port = broker();
    let (easee_url, commands) = fake_easee().await;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("eb.db");
    let mut cfg = config(mqtt_port, &db);
    // The same configuration with an [ev] section pointing at the fake.
    let ev = energybrain_config::parse(
        &format!(
            "{}\n[ev]\nserial = \"EH123456\"\nusername = \"u\"\npassword = \"p\"\napi_url = \"{easee_url}\"\n",
            std::fs::read_to_string(env!("CARGO_MANIFEST_DIR").to_owned() + "/../../config/example.toml").unwrap()
        ),
        &|_| None,
    )
    .unwrap();
    cfg.engine.charger = ev.engine.charger;
    cfg.topics.ev = true;
    let link = energybrain_easee::Link::with_timing(
        ev.easee.unwrap(),
        energybrain_easee::Timing {
            poll_interval: Duration::from_millis(100),
            backoff_min: Duration::from_millis(100),
            backoff_max: Duration::from_millis(100),
        },
    )
    .unwrap();
    let store = Box::new(SqliteStore::open(&db).unwrap());
    let (mut rt, http_port) = runtime(cfg, store, sqlite_events(db.clone())).await;
    rt.charger = Some(link);
    let (stop, stopped) = oneshot::channel::<()>();
    let service = tokio::spawn(run(rt, async {
        let _ = stopped.await;
    }));

    let mut world = World::connect(mqtt_port).await;
    world.expect("energybrain/mode", "observe").await;
    world.expect("energybrain/ev/shed_allowed", "1").await;
    // 6.4 kW from the meter with the car at 15 A: observe mode limits the charger on paper.
    world.send("tibber", fixture("kaifa_ma304h3e_list1")).await;
    world.expect("energybrain/ev/limited", "1").await;
    assert!(
        commands.lock().unwrap().is_empty(),
        "observe writes nothing"
    );
    // Active: the limit goes to Easee for real.
    world.send("energybrain/mode/set", "active").await;
    world.expect("energybrain/mode", "active").await;
    timeout(Duration::from_secs(10), async {
        while commands.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        commands.lock().unwrap()[0],
        serde_json::json!({"amps": 6, "minutes": 10})
    );
    let metrics = http_get(http_port, "/metrics").await;
    assert!(
        metrics.contains("energybrain_ev_writes_total 1"),
        "{metrics}"
    );
    assert!(
        metrics.contains("energybrain_ev_would_write_total 1"),
        "{metrics}"
    );

    // Switching limiting off in HomeKit releases it; stopping then writes nothing more.
    world.send("energybrain/ev/shed_allowed/set", "0").await;
    world.expect("energybrain/ev/shed_allowed", "0").await;
    stop.send(()).unwrap();
    timeout(Duration::from_secs(15), service)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let sent = commands.lock().unwrap().clone();
    assert_eq!(
        sent.last(),
        Some(&serde_json::json!({"amps": 32, "minutes": 1})),
        "{sent:?}"
    );
    let state = SqliteStore::open(&db)
        .unwrap()
        .load(ts("2026-11-01T00:00:00Z"))
        .unwrap();
    assert_eq!(state.charger.shed_allowed, Some(false));
    assert_eq!(state.charger.last_write, None);
}
