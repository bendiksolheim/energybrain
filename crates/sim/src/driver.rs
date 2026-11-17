//! The async shell around [`House`]: energybrain's own MQTT transport in and out, and a
//! real-time interval that steps the house to the shared clock's `now`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use energybrain_core::clock::Clock;
use energybrain_mqtt::{Link, Publisher, TransportEvent};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::Publish;
use crate::house::{House, STATE_TOPIC};

/// Runs `house` until `shutdown` completes. Every `every` (real time) the house is stepped
/// to `clock.now()`; on every connect the thermostats' current values are published
/// (retained). Each `sim/state` also logs [`House::summary`], so the house shows up next
/// to energybrain's log.
///
/// While disconnected, the house keeps running but what it publishes is dropped: rumqttc
/// only drains its request queue while connected, and a full queue would block this loop
/// (and Ctrl-C) until the broker came back.
pub async fn run(
    mut house: House,
    link: Link,
    publisher: Publisher,
    clock: Arc<dyn Clock>,
    every: Duration,
    shutdown: impl Future<Output = ()>,
) {
    // `Link::next` is not cancel-safe, so it runs in its own task, as in the app.
    let (tx, mut events) = mpsc::unbounded_channel();
    let mut link = link;
    let link_task = tokio::spawn(async move { while tx.send(link.next().await).is_ok() {} });
    let mut connected = false;
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(shutdown);
    loop {
        let out = tokio::select! {
            () = &mut shutdown => break,
            _ = ticker.tick() => house.step(clock.now()),
            Some(event) = events.recv() => on_event(event, &mut house, &mut connected, clock.as_ref()),
        };
        for p in out {
            if p.topic == STATE_TOPIC {
                info!("{}", house.summary());
            }
            if connected {
                // Publishing only fails once the event loop is gone, i.e. while shutting down.
                let _ = publisher.publish_bytes(&p.topic, p.payload, p.retain).await;
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), publisher.disconnect()).await;
    link_task.abort();
}

fn on_event(
    event: TransportEvent,
    house: &mut House,
    connected: &mut bool,
    clock: &dyn Clock,
) -> Vec<Publish> {
    match event {
        TransportEvent::Connected => {
            info!("simulated house connected");
            *connected = true;
            house.retained()
        }
        TransportEvent::Disconnected(reason) => {
            *connected = false;
            warn!(%reason, "simulated house disconnected; retrying");
            Vec::new()
        }
        TransportEvent::Message { topic, payload } => {
            let mut out = house.step(clock.now());
            let text = String::from_utf8_lossy(&payload).into_owned();
            match house.on_message(&topic, &payload) {
                Ok(more) => {
                    if topic.starts_with("sim/") {
                        info!(%topic, payload = %text, "sim control applied");
                    }
                    out.extend(more);
                }
                Err(reason) => warn!(%topic, payload = %text, %reason, "ignoring sim control"),
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use energybrain_config::MqttConfig;
    use energybrain_core::clock::SimClock;
    use energybrain_mqtt::{connect, translate};
    use jiff::SignedDuration;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;
    use crate::config::tests::{MINIMAL, eb};

    const BATHROOM_SETPOINT: &str = "zwave/Bathroom/Floor_bathroom/67/1/setpoint/1";

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// An embedded broker without authentication, like the dev mosquitto.
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
            "#
        );
        let config: rumqttd::Config = toml::from_str(&toml).unwrap();
        std::thread::spawn(move || {
            let _ = rumqttd::Broker::new(config).start();
        });
        port
    }

    fn mqtt(port: u16, id: &str) -> MqttConfig {
        MqttConfig {
            host: "127.0.0.1".to_owned(),
            port,
            client_id: id.to_owned(),
            keep_alive: SignedDuration::from_secs(10),
            username: None,
            password: None,
        }
    }

    /// The house wired to `port`, on a clock that a background task moves 1 s every 20 ms.
    fn start(port: u16) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let sim = crate::parse(MINIMAL, &eb()).unwrap();
        let clock = Arc::new(SimClock::new(sim.start));
        let mover = clock.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
                mover.advance(SignedDuration::from_secs(1));
            }
        });
        let (house, _) = House::new(&sim);
        let (link, publisher) = connect(
            &mqtt(port, "sim"),
            house.subscriptions(),
            Duration::from_millis(50),
        );
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(
            house,
            link,
            publisher,
            clock,
            Duration::from_millis(10),
            async {
                let _ = stopped.await;
            },
        ));
        (stop, task)
    }

    /// Waits for a message on `topic` whose payload satisfies `want`.
    async fn expect(
        events: &mut mpsc::UnboundedReceiver<TransportEvent>,
        topic: &str,
        want: impl Fn(&[u8]) -> bool,
    ) {
        let found = timeout(Duration::from_secs(10), async {
            loop {
                if let Some(TransportEvent::Message { topic: t, payload }) = events.recv().await
                    && t == topic
                    && want(&payload)
                {
                    return;
                }
            }
        })
        .await;
        assert!(found.is_ok(), "nothing on {topic}");
    }

    fn text(topic: &str, payload: &str) -> translate::Publish {
        translate::Publish {
            topic: topic.to_owned(),
            payload: payload.to_owned(),
            retain: false,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn publishes_echoes_and_survives_bad_controls() {
        let port = broker();
        let (link, observer) = connect(
            &mqtt(port, "observer"),
            vec!["#".to_owned()],
            Duration::from_millis(50),
        );
        let (tx, mut seen) = mpsc::unbounded_channel();
        let mut link = link;
        tokio::spawn(async move { while tx.send(link.next().await).is_ok() {} });
        let (stop, task) = start(port);

        // The thermostats' startup values, then live meter frames.
        expect(&mut seen, BATHROOM_SETPOINT, |p| p == b"22").await;
        expect(&mut seen, "tibber", |p| p.first() == Some(&0x7e)).await;
        // A setpoint write is echoed.
        observer
            .publish(&text(&format!("{BATHROOM_SETPOINT}/set"), "21"))
            .await
            .unwrap();
        expect(&mut seen, BATHROOM_SETPOINT, |p| p == b"21").await;
        // A bad control is logged and survived; a good one takes effect.
        observer.publish(&text("sim/ev/set", "fast")).await.unwrap();
        observer.publish(&text("sim/ev/set", "7.4")).await.unwrap();
        expect(&mut seen, STATE_TOPIC, |p| {
            serde_json::from_slice::<serde_json::Value>(p).unwrap()["loads"]["ev"]["value"] == 7.4
        })
        .await;

        stop.send(()).unwrap();
        timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_a_broker_it_keeps_retrying_until_stopped() {
        let (stop, task) = start(free_port());
        tokio::time::sleep(Duration::from_millis(300)).await;
        stop.send(()).unwrap();
        timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }
}
