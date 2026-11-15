//! Thin rumqttc transport: connect, (re)subscribe on every `ConnAck`, surface messages, publish.
//!
//! Reconnecting is rumqttc's job: after an error the next `poll` reconnects. We only pace it
//! with `retry_delay` so a dead broker does not cause a busy loop.

use std::time::Duration;

use energybrain_config::MqttConfig;
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS, SubscribeFilter};

use crate::translate::Publish;

/// Something the transport saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEvent {
    /// Connected (or reconnected) and subscribed. Republish retained state now.
    Connected,
    /// The connection failed or dropped; rumqttc retries on the next call.
    Disconnected(String),
    /// An inbound message.
    Message {
        /// Topic.
        topic: String,
        /// Raw payload.
        payload: Vec<u8>,
    },
}

/// Publishing failed because the transport is gone.
#[derive(Debug, thiserror::Error)]
#[error("mqtt publish failed: {0}")]
pub struct PublishError(#[from] rumqttc::ClientError);

/// The receiving half: owns the rumqttc event loop.
pub struct Link {
    eventloop: EventLoop,
    client: AsyncClient,
    subscriptions: Vec<String>,
    retry_delay: Duration,
    /// The last call reported a disconnect: pause before the next attempt.
    backoff: bool,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("subscriptions", &self.subscriptions)
            .finish_non_exhaustive()
    }
}

/// The sending half: cheap to clone.
#[derive(Debug, Clone)]
pub struct Publisher(AsyncClient);

/// Creates the link and its publisher. Nothing happens until [`Link::next`] is polled.
pub fn connect(
    cfg: &MqttConfig,
    subscriptions: Vec<String>,
    retry_delay: Duration,
) -> (Link, Publisher) {
    let mut opts = MqttOptions::new(&cfg.client_id, &cfg.host, cfg.port);
    opts.set_keep_alive(Duration::from_secs(cfg.keep_alive.as_secs().unsigned_abs()));
    opts.set_max_packet_size(64 * 1024, 64 * 1024);
    if let Some(user) = &cfg.username {
        opts.set_credentials(user, cfg.password.clone().unwrap_or_default());
    }
    let (client, mut eventloop) = AsyncClient::new(opts, 256);
    eventloop.network_options.set_connection_timeout(5);
    let link = Link {
        eventloop,
        client: client.clone(),
        subscriptions,
        retry_delay,
        backoff: false,
    };
    (link, Publisher(client))
}

impl Link {
    /// Waits for the next connection change or message. A disconnect is reported at once;
    /// the pause before reconnecting happens on the following call.
    pub async fn next(&mut self) -> TransportEvent {
        if std::mem::take(&mut self.backoff) {
            tokio::time::sleep(self.retry_delay).await;
        }
        loop {
            match self.eventloop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    let filters = self
                        .subscriptions
                        .iter()
                        .map(|t| SubscribeFilter::new(t.clone(), QoS::AtLeastOnce));
                    return match self.client.subscribe_many(filters).await {
                        Ok(()) => TransportEvent::Connected,
                        Err(e) => TransportEvent::Disconnected(e.to_string()),
                    };
                }
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    return TransportEvent::Message {
                        topic: p.topic,
                        payload: p.payload.to_vec(),
                    };
                }
                Ok(_) => {}
                Err(e) => {
                    self.backoff = true;
                    return TransportEvent::Disconnected(e.to_string());
                }
            }
        }
    }
}

impl Publisher {
    /// Queues a clean disconnect after everything already queued.
    pub async fn disconnect(&self) -> Result<(), PublishError> {
        self.0.disconnect().await?;
        Ok(())
    }

    /// Queues a message for publishing.
    pub async fn publish(&self, p: &Publish) -> Result<(), PublishError> {
        self.0
            .publish(
                p.topic.clone(),
                QoS::AtLeastOnce,
                p.retain,
                p.payload.clone(),
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::TcpListener;
    use std::time::Duration;

    use energybrain_config::MqttConfig;
    use jiff::SignedDuration;
    use tokio::time::timeout;

    use super::*;

    /// Starts an embedded rumqttd broker on a free port requiring `user`/`pass`.
    pub(crate) fn broker() -> u16 {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
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
            let mut broker = rumqttd::Broker::new(config);
            let _ = broker.start();
        });
        port
    }

    pub(crate) fn cfg(port: u16, id: &str) -> MqttConfig {
        MqttConfig {
            host: "127.0.0.1".to_owned(),
            port,
            client_id: id.to_owned(),
            keep_alive: SignedDuration::from_secs(10),
            username: Some("user".to_owned()),
            password: Some("pass".to_owned()),
        }
    }

    /// Runs a link in the background, forwarding its events.
    pub(crate) fn pump(mut link: Link) -> tokio::sync::mpsc::UnboundedReceiver<TransportEvent> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move { while tx.send(link.next().await).is_ok() {} });
        rx
    }

    /// Waits for the first event matching `f`.
    pub(crate) async fn wait_for<T>(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<TransportEvent>,
        f: impl Fn(TransportEvent) -> Option<T>,
    ) -> T {
        timeout(Duration::from_secs(10), async {
            loop {
                if let Some(t) = rx.recv().await.and_then(&f) {
                    return t;
                }
            }
        })
        .await
        .unwrap()
    }

    #[allow(clippy::needless_pass_by_value)] // same shape as the other `wait_for` filters
    pub(crate) fn connected(e: TransportEvent) -> Option<()> {
        matches!(e, TransportEvent::Connected).then_some(())
    }

    pub(crate) fn message(e: TransportEvent) -> Option<(String, Vec<u8>)> {
        if let TransportEvent::Message { topic, payload } = e {
            Some((topic, payload))
        } else {
            None
        }
    }

    fn publish(topic: &str, payload: &str, retain: bool) -> Publish {
        Publish {
            topic: topic.to_owned(),
            payload: payload.to_owned(),
            retain,
        }
    }

    #[tokio::test]
    async fn subscribes_receives_and_publishes_through_a_real_broker() {
        let port = broker();
        let fast = Duration::from_millis(50);
        let (ours, our_pub) = connect(&cfg(port, "energybrain"), vec!["tibber".to_owned()], fast);
        let (theirs, their_pub) = connect(
            &cfg(port, "homekit"),
            vec!["energybrain/#".to_owned()],
            fast,
        );
        assert!(format!("{ours:?}").contains("tibber"));
        assert_eq!(message(TransportEvent::Connected), None);
        let mut ours = pump(ours);
        let mut theirs = pump(theirs);
        wait_for(&mut ours, connected).await;
        wait_for(&mut theirs, connected).await;

        their_pub
            .publish(&publish("tibber", "frame", false))
            .await
            .unwrap();
        let (topic, payload) = wait_for(&mut ours, message).await;
        assert_eq!(
            (topic.as_str(), payload.as_slice()),
            ("tibber", b"frame".as_slice())
        );

        our_pub
            .publish(&publish("energybrain/mode", "observe", true))
            .await
            .unwrap();
        let (topic, payload) = wait_for(&mut theirs, message).await;
        assert_eq!(
            (topic.as_str(), payload.as_slice()),
            ("energybrain/mode", b"observe".as_slice())
        );

        our_pub.disconnect().await.unwrap();
        wait_for(&mut ours, |e| {
            matches!(e, TransportEvent::Disconnected(_)).then_some(())
        })
        .await;
    }

    #[tokio::test]
    async fn wrong_password_and_dead_broker_are_disconnects() {
        let port = broker();
        let mut bad = cfg(port, "intruder");
        bad.password = Some("nope".to_owned());
        let (mut link, _) = connect(&bad, vec!["tibber".to_owned()], Duration::from_millis(10));
        let ev = timeout(Duration::from_secs(10), link.next()).await.unwrap();
        assert!(matches!(ev, TransportEvent::Disconnected(_)), "{ev:?}");

        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut anon = cfg(closed, "nobody");
        anon.username = None;
        let (mut link, _) = connect(&anon, vec!["tibber".to_owned()], Duration::from_millis(10));
        let ev = timeout(Duration::from_secs(10), link.next()).await.unwrap();
        assert!(matches!(ev, TransportEvent::Disconnected(_)), "{ev:?}");
    }

    #[tokio::test]
    async fn empty_subscription_list_is_reported() {
        let port = broker();
        let (link, _) = connect(&cfg(port, "lonely"), vec![], Duration::from_millis(10));
        let mut events = pump(link);
        let err = wait_for(&mut events, |e| match e {
            TransportEvent::Disconnected(e) if e.contains("Failed to send") => Some(e),
            TransportEvent::Connected
            | TransportEvent::Disconnected(_)
            | TransportEvent::Message { .. } => None,
        })
        .await;
        assert!(err.contains("eventloop"), "{err}");
    }

    #[tokio::test]
    async fn publishing_after_the_link_is_gone_fails() {
        let (link, publisher) = connect(&cfg(1, "gone"), vec![], Duration::ZERO);
        drop(link);
        let err = publisher
            .publish(&publish("t", "p", false))
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("mqtt publish failed"), "{err}");
        assert!(publisher.disconnect().await.is_err());
    }
}
