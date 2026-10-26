use super::Input;
use crate::{
    cec::Frame,
    config::Config,
    engine,
    state::{Command, Mode, Snapshot},
};
use rumqttc::{AsyncClient, Event, Incoming, LastWill, MqttOptions, QoS};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

type OutstandingSends = VecDeque<(u8, Vec<u8>, Instant)>;

#[derive(Clone)]
pub struct Mqtt {
    client: AsyncClient,
    topic: String,
    connected: Arc<AtomicBool>,
    ingress: Arc<Mutex<OutstandingSends>>,
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
                topic: "fake_bridge".into(),
                connected: Arc::new(AtomicBool::new(connected)),
                ingress: Arc::new(Mutex::new(VecDeque::new())),
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
            topic: config.bridge.topic.clone(),
            connected: Arc::new(AtomicBool::new(false)),
            ingress: Arc::new(Mutex::new(VecDeque::new())),
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
                if retain {
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
    pub fn send(&self, to: u8, data: &[u8]) -> bool {
        let mut outstanding = self
            .ingress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.publish_locked(
            format!("cmnd/{}/HdmiSend", self.topic),
            false,
            json!({"To":to,"Data":hex::encode_upper(data)}).to_string(),
        ) {
            return false;
        }
        if outstanding.len() == 64 {
            outstanding.pop_front();
        }
        outstanding.push_back((to, data.to_vec(), Instant::now()));
        true
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
    let mut messages: VecDeque<_> = [
        format!("tele/{}/SENSOR", config.bridge.topic),
        format!("tele/{}/LWT", config.bridge.topic),
        format!("stat/{}/RESULT", config.bridge.topic),
        "choregos/cmd/+".into(),
    ]
    .into_iter()
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
    outstanding: &mut OutstandingSends,
) -> Option<Input> {
    if topic == format!("tele/{}/SENSOR", config.bridge.topic) {
        return Frame::parse(payload)
            .ok()
            .map(|f| Input::Event(engine::Event::Frame(f)));
    }
    if topic == format!("tele/{}/LWT", config.bridge.topic) {
        return match payload {
            b"Online" => {
                outstanding.clear();
                Some(Input::Event(engine::Event::BridgeOnline))
            }
            b"Offline" => {
                outstanding.clear();
                Some(Input::Event(engine::Event::BridgeOffline))
            }
            _ => None,
        };
    }
    if topic == format!("stat/{}/RESULT", config.bridge.topic) {
        let result = serde_json::from_slice::<Value>(payload).ok()?;
        let hdmi_send = result.get("HdmiSend")?;
        while outstanding
            .front()
            .is_some_and(|(_, _, sent_at)| sent_at.elapsed() > Duration::from_secs(5))
        {
            outstanding.pop_front();
        }
        let (to, data, _) = outstanding.pop_front()?;
        let ack = hdmi_send.get("Ack").and_then(Value::as_bool);
        tracing::debug!(to, opcode = ?data.first(), ack, result = %hdmi_send, "bridge HdmiSend result");
        let ack = ack?;
        return Some(Input::Event(engine::Event::SendResult { to, data, ack }));
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
        ]
        .map(|(a, b, c)| (a.into(), b.into(), c.into())),
    );
    entities.into_iter().map(|(component, name, label)| {
        let mut payload = json!({"name":label,"unique_id":format!("choregos_{name}"),"default_entity_id":format!("{component}.choregos_{name}"),"availability_topic":"choregos/status","device":{"identifiers":["choregos"],"name":"Choregos","manufacturer":"Choregos"}});
        if component != "button" { payload["state_topic"] = json!(format!("choregos/state/{name}")); }
        if component == "select" { payload["options"] = json!(["shadow","active"]); }
        if component == "select" || component == "button" { payload["command_topic"] = json!(format!("choregos/cmd/{name}")); }
        (format!("homeassistant/{component}/choregos/{name}/config"), payload)
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_patched_acks_emit_results_but_all_hdmisend_replies_consume_fifo_entries() {
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        let (mqtt, _events) = Mqtt::test_transport(true, 16);
        config.bridge.topic.clone_from(&mqtt.topic);
        let result_topic = format!("stat/{}/RESULT", mqtt.topic);
        let cases = [
            (
                4,
                vec![0x36],
                json!({"HdmiSend":{"Ack":false}}),
                Some(false),
            ),
            (8, vec![0x8f], json!({"HdmiSend":"Done"}), None),
            (11, vec![0x8f], json!({"HdmiSend":"Sending failed"}), None),
            (4, vec![0x83], json!({"HdmiSend":"Invalid JSON"}), None),
            (8, vec![0x83], json!({"HdmiSend":"Buffer empty"}), None),
            (11, vec![0x83], json!({"HdmiSend":"Buffer too large"}), None),
            (4, vec![0x8f], json!({"HdmiSend":"unknown"}), None),
            (8, vec![0x36], json!({"HdmiSend":{"Ack":true}}), Some(true)),
        ];
        for (to, data, _, _) in &cases {
            assert!(mqtt.send(*to, data));
        }
        let mut outstanding = mqtt.ingress.lock().unwrap();
        assert!(
            decode(
                &config,
                &result_topic,
                br#"{"Other":"Done"}"#,
                &mut outstanding,
            )
            .is_none()
        );
        for (expected_to, expected_data, payload, expected_ack) in cases {
            let input = decode(
                &config,
                &result_topic,
                &serde_json::to_vec(&payload).unwrap(),
                &mut outstanding,
            );
            if let Some(expected_ack) = expected_ack {
                let Some(Input::Event(engine::Event::SendResult { to, data, ack })) = input else {
                    panic!("expected correlated send result");
                };
                assert_eq!((to, data, ack), (expected_to, expected_data, expected_ack));
            } else {
                assert!(input.is_none());
            }
        }
        assert!(
            decode(
                &config,
                &result_topic,
                br#"{"HdmiSend":"Done"}"#,
                &mut outstanding,
            )
            .is_none()
        );
    }

    #[test]
    fn send_fifo_drops_oldest_at_capacity() {
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        let (mqtt, _events) = Mqtt::test_transport(true, 128);
        config.bridge.topic.clone_from(&mqtt.topic);
        for opcode in 0..65 {
            assert!(mqtt.send(4, &[opcode]));
        }
        let mut outstanding = mqtt.ingress.lock().unwrap();
        let Some(Input::Event(engine::Event::SendResult { to, data, ack })) = decode(
            &config,
            &format!("stat/{}/RESULT", mqtt.topic),
            br#"{"HdmiSend":{"Ack":true}}"#,
            &mut outstanding,
        ) else {
            panic!("expected the oldest retained send");
        };
        assert_eq!((to, data, ack), (4, vec![1], true));
    }

    #[test]
    fn bridge_offline_and_online_clear_outstanding_sends() {
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        let (mqtt, _events) = Mqtt::test_transport(true, 8);
        config.bridge.topic.clone_from(&mqtt.topic);
        for lwt in [b"Offline".as_slice(), b"Online".as_slice()] {
            assert!(mqtt.send(4, &[0x36]));
            let mut outstanding = mqtt.ingress.lock().unwrap();
            assert!(
                decode(
                    &config,
                    &format!("tele/{}/LWT", mqtt.topic),
                    lwt,
                    &mut outstanding,
                )
                .is_some()
            );
            assert!(
                decode(
                    &config,
                    &format!("stat/{}/RESULT", mqtt.topic),
                    br#"{"HdmiSend":{"Ack":false}}"#,
                    &mut outstanding,
                )
                .is_none()
            );
        }
    }

    #[test]
    fn send_results_discard_entries_older_than_five_seconds() {
        let config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        let now = Instant::now();
        let mut outstanding = VecDeque::from([
            (4, vec![0x36], now - Duration::from_secs(6)),
            (8, vec![0x8f], now),
        ]);
        let Some(Input::Event(engine::Event::SendResult { to, data, ack })) = decode(
            &config,
            &format!("stat/{}/RESULT", config.bridge.topic),
            br#"{"HdmiSend":{"Ack":false}}"#,
            &mut outstanding,
        ) else {
            panic!("expected the non-expired send");
        };
        assert_eq!((to, data, ack), (8, vec![0x8f], false));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn broker_receives_frames_sends_commands_and_retains_discovery() {
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
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config.mqtt.host = "127.0.0.1".into();
        config.mqtt.port = address.port();
        config.bridge.topic = "test_bridge".into();
        let (tx, mut rx) = mpsc::channel(32);
        let (mqtt, adapter_task) = Mqtt::start(&config, tx, &[]);
        let mut options = MqttOptions::new("observer", "127.0.0.1", address.port());
        options.set_keep_alive(Duration::from_secs(5));
        let (observer, mut events) = AsyncClient::new(options, 32);
        let (publications, mut received) = mpsc::channel(64);
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
        let test = async {
            // Subscribe after the adapter has advertised online, then subscribe to
            // discovery: receiving retain=true proves the broker retained it.
            observer
                .subscribe("choregos/status", QoS::AtLeastOnce)
                .await
                .unwrap();
            loop {
                if received.recv().await.unwrap().payload.as_ref() == b"online" {
                    break;
                }
            }
            observer
                .subscribe("homeassistant/+/choregos/+/config", QoS::AtLeastOnce)
                .await
                .unwrap();
            observer
                .subscribe("cmnd/test_bridge/HdmiSend", QoS::AtLeastOnce)
                .await
                .unwrap();
            let mut retained = 0;
            while retained < config.device.len() + 5 {
                let p = received.recv().await.unwrap();
                if p.topic.starts_with("homeassistant/") {
                    assert!(p.retain);
                    retained += 1;
                }
            }
            observer
                .publish(
                    "tele/test_bridge/SENSOR",
                    QoS::AtLeastOnce,
                    false,
                    r#"{"HdmiReceived":{"From":4,"To":0,"Data":"04"}}"#,
                )
                .await
                .unwrap();
            loop {
                if let Input::Event(engine::Event::Frame(frame)) = rx.recv().await.unwrap() {
                    assert_eq!(frame.opcode, Some(4));
                    break;
                }
            }
            assert!(mqtt.send(4, &[0x36]));
            loop {
                let p = received.recv().await.unwrap();
                if p.topic == "cmnd/test_bridge/HdmiSend" {
                    assert_eq!(
                        serde_json::from_slice::<Value>(&p.payload).unwrap(),
                        json!({"To":4,"Data":"36"})
                    );
                    break;
                }
            }
            observer
                .publish(
                    "stat/test_bridge/RESULT",
                    QoS::AtLeastOnce,
                    false,
                    r#"{"HdmiSend":{"Ack":false}}"#,
                )
                .await
                .unwrap();
            loop {
                if let Input::Event(engine::Event::SendResult { to, data, ack }) =
                    rx.recv().await.unwrap()
                {
                    assert_eq!((to, data, ack), (4, vec![0x36], false));
                    break;
                }
            }
            observer
                .publish("choregos/cmd/mode", QoS::AtLeastOnce, false, "active")
                .await
                .unwrap();
            loop {
                if let Input::Request(super::super::Request::Command(Command::SetMode(mode), _)) =
                    rx.recv().await.unwrap()
                {
                    assert_eq!(mode, Mode::Active);
                    break;
                }
            }

            assert!(mqtt.send(8, &[0x8f]));
            // A second connection with the same client ID makes the broker drop the
            // adapter's socket, exercising its real disconnect/reconnect path.
            let (replacement, mut replacement_events) = AsyncClient::new(
                MqttOptions::new(&config.mqtt.client_id, "127.0.0.1", address.port()),
                8,
            );
            loop {
                if matches!(
                    replacement_events.poll().await.unwrap(),
                    Event::Incoming(Incoming::ConnAck(_))
                ) {
                    break;
                }
            }
            drop(replacement_events);
            drop(replacement);
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
                        break;
                    }
                }
            }
            // Setup publishes online after subscribing, providing a broker-side
            // barrier before sending the stale result and subsequent frame marker.
            loop {
                let publish = received.recv().await.unwrap();
                if publish.topic == "choregos/status" && publish.payload.as_ref() == b"online" {
                    break;
                }
            }
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
                match rx.recv().await.unwrap() {
                    Input::Event(engine::Event::SendResult { .. }) => {
                        panic!("stale pre-reconnect send result was correlated");
                    }
                    Input::Event(engine::Event::Frame(frame)) if frame.from == 8 => break,
                    _ => {}
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
    fn fast_lane_drops_disconnected_and_full_publishes_without_enqueuing_stale_work() {
        let (mqtt, mut eventloop) = Mqtt::test_transport(false, 1);
        assert!(!mqtt.send(4, &[0x36]));
        assert!(mqtt.ingress.lock().unwrap().is_empty());
        eventloop.clean();
        assert!(eventloop.pending.is_empty());
        mqtt.connected.store(true, Ordering::Release);
        assert!(mqtt.send(4, &[0x8f]));
        assert!(!mqtt.send(4, &[0x36]));
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config.bridge.topic.clone_from(&mqtt.topic);
        let mut outstanding = mqtt.ingress.lock().unwrap();
        let Some(Input::Event(engine::Event::SendResult { to, data, ack })) = decode(
            &config,
            &format!("stat/{}/RESULT", mqtt.topic),
            br#"{"HdmiSend":{"Ack":false}}"#,
            &mut outstanding,
        ) else {
            panic!("expected only the successful publish in the FIFO");
        };
        assert_eq!((to, data, ack), (4, vec![0x8f], false));
        assert!(outstanding.is_empty());
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
