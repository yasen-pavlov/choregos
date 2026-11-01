use super::{
    Input,
    bridge::{self, Correlation},
};
#[cfg(test)]
use crate::engine;
use crate::{
    config::{BridgeConfig, Config},
    state::{Command, Mode, Snapshot},
};
use rumqttc::{AsyncClient, Event, Incoming, LastWill, MqttOptions, QoS};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

static SEND_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct Mqtt {
    client: AsyncClient,
    bridge: BridgeConfig,
    connected: Arc<AtomicBool>,
    ingress: Arc<Mutex<Correlation>>,
}
enum Setup {
    Subscribe(String),
    Publish(String, String),
}
impl Mqtt {
    #[cfg(test)]
    pub(crate) fn test_transport(connected: bool, capacity: usize) -> (Self, rumqttc::EventLoop) {
        let (client, eventloop) =
            AsyncClient::new(MqttOptions::new("fake", "127.0.0.1", 1883), capacity);
        (
            Self {
                client,
                bridge: BridgeConfig::Tasmota {
                    topic: "fake_bridge".into(),
                    logical_address: 0,
                },
                connected: Arc::new(AtomicBool::new(connected)),
                ingress: Arc::new(Mutex::new(Correlation::default())),
            },
            eventloop,
        )
    }
    pub(crate) fn start(
        config: &Config,
        tx: mpsc::Sender<Input>,
        removed_roles: &[String],
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let mut options =
            MqttOptions::new(&config.mqtt.client_id, &config.mqtt.host, config.mqtt.port);
        options.set_keep_alive(Duration::from_secs(20));
        options.set_credentials(&config.mqtt.username, config.secrets.mqtt_password.expose());
        options.set_last_will(LastWill::new(
            "choregos/status",
            "offline",
            QoS::AtLeastOnce,
            true,
        ));
        let (client, mut eventloop) = AsyncClient::new(options, 128);
        let adapter = Self {
            client,
            bridge: config.bridge.clone(),
            connected: Arc::new(AtomicBool::new(false)),
            ingress: Arc::new(Mutex::new(Correlation::default())),
        };
        let transport = adapter.clone();
        let config = config.clone();
        let removed_roles = removed_roles.to_vec();
        let task = tokio::spawn(async move {
            let mut backoff = 1;
            let mut last_error = String::new();
            let mut setup = VecDeque::new();
            loop {
                flush_setup(&transport.client, &mut setup);
                match eventloop.poll().await {
                    Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                        setup = setup_messages(&config, &removed_roles);
                        flush_setup(&transport.client, &mut setup);
                        {
                            let mut outstanding = transport
                                .ingress
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            outstanding.clear();
                            transport.connected.store(true, Ordering::Release);
                        }
                        backoff = 1;
                        last_error.clear();
                        if tx
                            .send(Input::Connection {
                                mqtt: true,
                                connected: true,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(Event::Incoming(Incoming::Publish(p))) => {
                        let input = {
                            let mut outstanding = transport
                                .ingress
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            decode(&config, &p.topic, &p.payload, &mut outstanding)
                        };
                        if let Some(input) = input
                            && tx.send(input).await.is_err()
                        {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        {
                            let mut outstanding = transport
                                .ingress
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            transport.connected.store(false, Ordering::Release);
                            outstanding.clear();
                            // Discard queued commands rather than replaying stale standby/volume effects.
                            eventloop.clean();
                            eventloop.pending.clear();
                        }
                        setup.clear();
                        let error = error.to_string();
                        if error != last_error {
                            tracing::warn!(error = %error, "MQTT disconnected");
                            last_error = error;
                        }
                        if tx
                            .send(Input::Connection {
                                mqtt: true,
                                connected: false,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(backoff)).await;
                        backoff = (backoff * 2).min(60);
                    }
                }
            }
            transport.connected.store(false, Ordering::Release);
        });
        (adapter, task)
    }
    fn publish(&self, topic: String, retain: bool, payload: String) -> bool {
        let _guard = self
            .ingress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.publish_locked(topic, retain, payload)
    }

    // Caller holds ingress, so publishing and recording a send are atomic with decoding.
    fn publish_locked(&self, topic: String, retain: bool, payload: String) -> bool {
        if !self.connected.load(Ordering::Acquire) {
            tracing::warn!(topic, "dropping MQTT publish: disconnected");
            return false;
        }
        if self
            .client
            .try_publish(
                &topic,
                if retain || matches!(self.bridge, BridgeConfig::Aether { .. }) {
                    QoS::AtLeastOnce
                } else {
                    QoS::AtMostOnce
                },
                retain,
                payload,
            )
            .is_err()
        {
            tracing::warn!(topic, "dropping MQTT publish: queue full");
            return false;
        }
        true
    }
    pub fn send(&self, to: u8, data: &[u8]) -> Option<Option<String>> {
        let mut correlation = self
            .ingress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let counter = SEND_COUNTER.fetch_add(1, Ordering::Relaxed);
        let (topic, payload, id) = bridge::publish_send(&self.bridge, counter, to, data);
        if !self.publish_locked(topic, false, payload) {
            return None;
        }
        correlation.record_send(id.clone(), to, data.to_vec(), Instant::now());
        Some(id)
    }
    pub fn states(&self, snapshot: &Snapshot) {
        let mut states = vec![
            (
                "active_source".to_owned(),
                snapshot
                    .active_role
                    .clone()
                    .unwrap_or_else(|| "none".into()),
            ),
            ("mode".into(), snapshot.mode.to_string()),
            (
                "bridge".into(),
                if snapshot.bridge_online { "ON" } else { "OFF" }.into(),
            ),
            ("conflicts".into(), snapshot.conflicts.len().to_string()),
        ];
        states.extend(snapshot.roles.iter().map(|r| {
            (
                r.role.clone(),
                if r.power == crate::state::RolePower::On {
                    "ON"
                } else {
                    "OFF"
                }
                .into(),
            )
        }));
        if let Some(telemetry) = &snapshot.bridge_telemetry {
            states.extend([
                ("bridge_signal".into(), telemetry.signal_dbm.to_string()),
                ("bridge_link_count".into(), telemetry.link_count.to_string()),
                ("bridge_uptime".into(), telemetry.uptime_s.to_string()),
            ]);
        }
        for (name, value) in states {
            self.publish(format!("choregos/state/{name}"), true, value);
        }
    }
    pub fn shutdown(&self) {
        self.publish("choregos/status".into(), true, "offline".into());
        if self.client.try_disconnect().is_err() {
            tracing::warn!("MQTT disconnect queue full");
        }
    }
}
fn flush_setup(client: &AsyncClient, setup: &mut VecDeque<Setup>) {
    while let Some(request) = setup.front() {
        let result = match request {
            Setup::Subscribe(topic) => client.try_subscribe(topic, QoS::AtLeastOnce),
            Setup::Publish(topic, payload) => {
                client.try_publish(topic, QoS::AtLeastOnce, true, payload.as_bytes())
            }
        };
        if result.is_err() {
            break;
        }
        setup.pop_front();
    }
}
fn setup_messages(config: &Config, removed_roles: &[String]) -> VecDeque<Setup> {
    let mut messages: VecDeque<_> = bridge::subscriptions(&config.bridge)
        .into_iter()
        .chain(std::iter::once("choregos/cmd/+".into()))
        .map(Setup::Subscribe)
        .collect();
    for role in removed_roles {
        messages.push_back(Setup::Publish(
            format!("homeassistant/binary_sensor/choregos/{role}/config"),
            String::new(),
        ));
        messages.push_back(Setup::Publish(
            format!("choregos/state/{role}"),
            String::new(),
        ));
    }
    messages.extend(
        discovery(config)
            .into_iter()
            .map(|(topic, payload)| Setup::Publish(topic, payload.to_string())),
    );
    messages.push_back(Setup::Publish("choregos/status".into(), "online".into()));
    messages
}
fn decode(
    config: &Config,
    topic: &str,
    payload: &[u8],
    correlation: &mut Correlation,
) -> Option<Input> {
    if let Some(incoming) = bridge::decode(&config.bridge, topic, payload) {
        return match incoming {
            bridge::Incoming::Trace(entry) => {
                let decoded = correlation.apply(&entry, Instant::now());
                Some(Input::Bridge { entry, decoded })
            }
            bridge::Incoming::Telemetry(mut telemetry) => {
                if matches!(config.bridge, BridgeConfig::Aether { .. }) {
                    telemetry.stale_after_s = correlation.telemetry_stale_after_s();
                }
                Some(Input::BridgeTelemetry(telemetry))
            }
            bridge::Incoming::Cfg(payload) => {
                correlation.configure(&payload, &config.bridge, config.fake_tv.replies);
                None
            }
        };
    }
    let command = match topic.strip_prefix("choregos/cmd/")? {
        "mode" => match payload {
            b"active" => Command::SetMode(Mode::Active),
            b"shadow" => Command::SetMode(Mode::Shadow),
            _ => return None,
        },
        "all_off" => Command::AllOff,
        _ => return None,
    };
    Some(Input::Request(super::Request::Command(command, None)))
}

pub fn discovery(config: &Config) -> Vec<(String, Value)> {
    let mut entities: Vec<(String, String, String)> = config
        .device
        .iter()
        .map(|d| ("binary_sensor".into(), d.role.clone(), d.name.clone()))
        .collect();
    entities.extend(
        [
            ("sensor", "active_source", "Active source"),
            ("select", "mode", "Mode"),
            ("button", "all_off", "All off"),
            ("binary_sensor", "bridge", "Bridge"),
            ("sensor", "conflicts", "Conflicts"),
            ("sensor", "bridge_signal", "Bridge signal"),
            ("sensor", "bridge_link_count", "Bridge link count"),
            ("sensor", "bridge_uptime", "Bridge uptime"),
        ]
        .map(|(a, b, c)| (a.into(), b.into(), c.into())),
    );
    entities.into_iter().map(|(component, name, label)| {
        let mut payload = json!({"name":label,"unique_id":format!("choregos_{name}"),"default_entity_id":format!("{component}.choregos_{name}"),"availability_topic":"choregos/status","device":{"identifiers":["choregos"],"name":"Choregos","manufacturer":"Choregos"}});
        if component != "button" { payload["state_topic"] = json!(format!("choregos/state/{name}")); }
        if component == "select" { payload["options"] = json!(["shadow","active"]); }
        if component == "select" || component == "button" { payload["command_topic"] = json!(format!("choregos/cmd/{name}")); }
        match name.as_str() {
            "bridge_signal" => {
                payload["unit_of_measurement"] = json!("dBm");
                payload["device_class"] = json!("signal_strength");
                payload["state_class"] = json!("measurement");
            }
            "bridge_link_count" => payload["state_class"] = json!("total_increasing"),
            "bridge_uptime" => {
                payload["unit_of_measurement"] = json!("s");
                payload["device_class"] = json!("duration");
            }
            _ => {}
        }
        if name.starts_with("bridge_") { payload["entity_category"] = json!("diagnostic"); }
        (format!("homeassistant/{component}/choregos/{name}/config"), payload)
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_broker() -> std::net::SocketAddr {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        drop(socket);
        let server = rumqttd::ServerSettings {
            name: "test".into(),
            listen: address,
            tls: None,
            next_connection_delay_ms: 1,
            connections: rumqttd::ConnectionSettings {
                connection_timeout_ms: 5000,
                max_payload_size: 1024 * 1024,
                max_inflight_count: 100,
                auth: None,
                external_auth: None,
                dynamic_filters: true,
            },
        };
        let broker_config = rumqttd::Config {
            router: rumqttd::RouterConfig {
                max_connections: 10,
                max_outgoing_packet_count: 200,
                max_segment_size: 1024 * 1024,
                max_segment_count: 10,
                ..Default::default()
            },
            v4: Some([("test".into(), server)].into_iter().collect()),
            ..Default::default()
        };
        std::thread::spawn(move || {
            rumqttd::Broker::new(broker_config).start().unwrap();
        });
        address
    }

    fn spawn_observer(
        port: u16,
    ) -> (
        AsyncClient,
        mpsc::Receiver<rumqttc::Publish>,
        tokio::task::JoinHandle<()>,
    ) {
        let mut options = MqttOptions::new("observer", "127.0.0.1", port);
        options.set_keep_alive(Duration::from_secs(5));
        let (observer, mut events) = AsyncClient::new(options, 32);
        let (publications, received) = mpsc::channel(64);
        let poll = tokio::spawn(async move {
            loop {
                match events.poll().await {
                    Ok(Event::Incoming(Incoming::Publish(p))) => {
                        if publications.send(p).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        });
        (observer, received, poll)
    }

    async fn next_publication(
        received: &mut mpsc::Receiver<rumqttc::Publish>,
        mut select: impl FnMut(&rumqttc::Publish) -> bool,
    ) -> rumqttc::Publish {
        loop {
            let publish = received.recv().await.unwrap();
            if select(&publish) {
                return publish;
            }
        }
    }

    async fn assert_retained_discovery(rx: &mut mpsc::Receiver<rumqttc::Publish>, count: usize) {
        let mut retained = 0;
        while retained < count {
            let p = rx.recv().await.unwrap();
            if p.topic.starts_with("homeassistant/") {
                assert!(p.retain);
                retained += 1;
            }
        }
    }

    async fn next_input<T>(
        rx: &mut mpsc::Receiver<Input>,
        mut select: impl FnMut(Input) -> Option<T>,
    ) -> T {
        loop {
            if let Some(value) = select(rx.recv().await.unwrap()) {
                return value;
            }
        }
    }

    async fn wait_for_connack(events: &mut rumqttc::EventLoop) {
        loop {
            if matches!(
                events.poll().await.unwrap(),
                Event::Incoming(Incoming::ConnAck(_))
            ) {
                return;
            }
        }
    }

    async fn wait_for_reconnect(rx: &mut mpsc::Receiver<Input>) {
        let mut disconnected = false;
        loop {
            if let Input::Connection {
                mqtt: true,
                connected,
            } = rx.recv().await.unwrap()
            {
                if !connected {
                    disconnected = true;
                } else if disconnected {
                    return;
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn broker_receives_frames_sends_commands_and_retains_discovery() {
        let address = start_broker();
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config.mqtt.host = "127.0.0.1".into();
        config.mqtt.port = address.port();
        config.bridge = BridgeConfig::Tasmota {
            topic: "test_bridge".into(),
            logical_address: 0,
        };
        let (tx, mut rx) = mpsc::channel(32);
        let (mqtt, adapter_task) = Mqtt::start(&config, tx, &[]);
        let (observer, mut received, poll) = spawn_observer(address.port());
        let test = async {
            // Subscribe after the adapter has advertised online, then subscribe to
            // discovery: receiving retain=true proves the broker retained it.
            observer
                .subscribe("choregos/status", QoS::AtLeastOnce)
                .await
                .unwrap();
            next_publication(&mut received, |p| p.payload.as_ref() == b"online").await;
            observer
                .subscribe("homeassistant/+/choregos/+/config", QoS::AtLeastOnce)
                .await
                .unwrap();
            observer
                .subscribe("cmnd/test_bridge/HdmiSend", QoS::AtLeastOnce)
                .await
                .unwrap();
            assert_retained_discovery(&mut received, config.device.len() + 8).await;
            observer
                .publish(
                    "tele/test_bridge/SENSOR",
                    QoS::AtLeastOnce,
                    false,
                    r#"{"HdmiReceived":{"From":4,"To":0,"Data":"04"}}"#,
                )
                .await
                .unwrap();
            let frame = next_input(&mut rx, |input| match input {
                Input::Bridge { decoded, .. } => {
                    decoded.events.into_iter().find_map(|event| match event {
                        engine::Event::Frame(frame) => Some(frame),
                        _ => None,
                    })
                }
                _ => None,
            })
            .await;
            assert_eq!(frame.opcode, Some(4));
            assert!(mqtt.send(4, &[0x36]).is_some());
            let p =
                next_publication(&mut received, |p| p.topic == "cmnd/test_bridge/HdmiSend").await;
            assert_eq!(
                serde_json::from_slice::<Value>(&p.payload).unwrap(),
                json!({"To":4,"Data":"36"})
            );
            observer
                .publish(
                    "stat/test_bridge/RESULT",
                    QoS::AtLeastOnce,
                    false,
                    r#"{"HdmiSend":{"Ack":false}}"#,
                )
                .await
                .unwrap();
            let (to, data, status) = next_input(&mut rx, |input| match input {
                Input::Bridge { decoded, .. } => {
                    decoded.events.into_iter().find_map(|event| match event {
                        engine::Event::SendResult {
                            to, data, status, ..
                        } => Some((to, data, status)),
                        _ => None,
                    })
                }
                _ => None,
            })
            .await;
            assert_eq!(
                (to, data, status),
                (4, vec![0x36], engine::SendStatus::Nack)
            );
            observer
                .publish("choregos/cmd/mode", QoS::AtLeastOnce, false, "active")
                .await
                .unwrap();
            let mode = next_input(&mut rx, |input| match input {
                Input::Request(super::super::Request::Command(Command::SetMode(mode), _)) => {
                    Some(mode)
                }
                _ => None,
            })
            .await;
            assert_eq!(mode, Mode::Active);

            assert!(mqtt.send(8, &[0x8f]).is_some());
            // A second connection with the same client ID makes the broker drop the
            // adapter's socket, exercising its real disconnect/reconnect path.
            let (replacement, mut replacement_events) = AsyncClient::new(
                MqttOptions::new(&config.mqtt.client_id, "127.0.0.1", address.port()),
                8,
            );
            wait_for_connack(&mut replacement_events).await;
            drop(replacement_events);
            drop(replacement);
            wait_for_reconnect(&mut rx).await;
            // Setup publishes online after subscribing, providing a broker-side
            // barrier before sending the stale result and subsequent frame marker.
            next_publication(&mut received, |p| {
                p.topic == "choregos/status" && p.payload.as_ref() == b"online"
            })
            .await;
            observer
                .publish(
                    "stat/test_bridge/RESULT",
                    QoS::AtLeastOnce,
                    false,
                    r#"{"HdmiSend":{"Ack":false}}"#,
                )
                .await
                .unwrap();
            observer
                .publish(
                    "tele/test_bridge/SENSOR",
                    QoS::AtLeastOnce,
                    false,
                    r#"{"HdmiReceived":{"From":8,"To":0,"Data":"04"}}"#,
                )
                .await
                .unwrap();
            loop {
                if let Input::Bridge { decoded, .. } = rx.recv().await.unwrap() {
                    assert!(
                        !decoded
                            .events
                            .iter()
                            .any(|event| matches!(event, engine::Event::SendResult { .. })),
                        "stale pre-reconnect send result was correlated"
                    );
                    if decoded.events.iter().any(
                        |event| matches!(event, engine::Event::Frame(frame) if frame.from == 8),
                    ) {
                        break;
                    }
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), test)
            .await
            .unwrap();
        adapter_task.abort();
        poll.abort();
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;

    #[test]
    fn replacement_adapters_never_reuse_send_ids_or_accept_old_outcomes() {
        let now = Instant::now();
        let bridge = BridgeConfig::Aether {
            node: "cec-bridge".into(),
            logical_address: 0,
        };
        let (mut old, _old_events) = Mqtt::test_transport(true, 8);
        old.bridge = bridge.clone();
        let old_id = old.send(4, &[0x36]).unwrap().unwrap();
        drop(old);
        let (mut replacement, _new_events) = Mqtt::test_transport(true, 8);
        replacement.bridge = bridge;
        let new_id = replacement.send(8, &[0x8f]).unwrap().unwrap();
        let next_id = replacement.clone().send(4, &[0x36]).unwrap().unwrap();
        let number = |id: &str| id.strip_prefix("s-").unwrap().parse::<u64>().unwrap();
        assert!(number(&old_id) < number(&new_id));
        assert!(number(&new_id) < number(&next_id));
        let mut correlation = replacement.ingress.lock().unwrap();
        let outcome = |seq, id| super::super::trace::TraceEntry::AetherBus {
            node: "cec-bridge".into(),
            payload: json!({"ev":"tx","seq":seq,"id":id,"origin":"host","status":"nack","attempts":1}),
        };
        assert!(
            correlation
                .apply(&outcome(1, old_id), now)
                .events
                .is_empty()
        );
        assert!(
            matches!(&correlation.apply(&outcome(2, new_id), now).events[..],
            [engine::Event::SendResult { to: 8, data, status: engine::SendStatus::Nack, .. }] if data == &[0x8f])
        );
    }
    #[test]
    fn fast_lane_drops_disconnected_and_full_publishes_without_enqueuing_stale_work() {
        let (mqtt, mut eventloop) = Mqtt::test_transport(false, 1);
        assert!(mqtt.send(4, &[0x36]).is_none());
        eventloop.clean();
        assert!(eventloop.pending.is_empty());
        mqtt.connected.store(true, Ordering::Release);
        assert!(mqtt.send(4, &[0x8f]).is_some());
        assert!(mqtt.send(4, &[0x36]).is_none());
        let entry = super::super::trace::TraceEntry::Result {
            bridge: "fake_bridge".into(),
            payload: json!({"HdmiSend":{"Ack":false}}),
        };
        let mut correlation = mqtt.ingress.lock().unwrap();
        let decoded = correlation.apply(&entry, Instant::now());
        assert!(
            matches!(&decoded.events[..], [engine::Event::SendResult { to: 4, data, status: engine::SendStatus::Nack, .. }] if data == &[0x8f])
        );
        assert!(correlation.apply(&entry, Instant::now()).events.is_empty());
        eventloop.clean();
        assert_eq!(eventloop.pending.len(), 1);
        assert!(
            matches!(&eventloop.pending[0], rumqttc::Request::Publish(p) if p.payload.as_ref() == br#"{"Data":"8F","To":4}"#)
        );
    }
    #[test]
    fn removed_roles_are_tombstoned_before_new_discovery_and_ids_are_explicit() {
        let config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        let messages = setup_messages(&config, &["retired".into()]);
        assert!(messages.iter().any(|m| matches!(m, Setup::Publish(topic, payload) if topic == "homeassistant/binary_sensor/choregos/retired/config" && payload.is_empty())));
        assert!(messages.iter().any(|m| matches!(m, Setup::Publish(topic, payload) if topic == "choregos/state/retired" && payload.is_empty())));
        for (topic, payload) in discovery(&config) {
            let component = topic.split('/').nth(1).unwrap();
            let name = topic.split('/').nth(3).unwrap();
            assert_eq!(
                payload["default_entity_id"],
                format!("{component}.choregos_{name}")
            );
            assert!(payload.get("object_id").is_none());
        }
    }
}
