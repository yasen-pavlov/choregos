//! Loopback-only acceptance tests of the production scheduling loop and adapters.
use super::{Adapters, Runtime};
use crate::{
    cec::Frame,
    config::{Config, WakeConfig},
    engine::Engine,
    state::{AppState, Command, FrameDirection, Mode, RolePower, Snapshot},
};
use futures_util::{SinkExt, StreamExt};
use rumqttc::{AsyncClient, Event as MqttEvent, Incoming, MqttOptions, Publish, QoS};
use serde_json::{Value, json};
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::{
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};

const WAIT: Duration = Duration::from_secs(10);
const DUMMY_SECRET: &str = "runtime-test-only-dummy-secret-value";

// Broker::start creates unowned server threads and has no shutdown API. Hosting
// the same Router/Server in an owned runtime lets tests really sever every socket.
struct Broker {
    address: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Broker {
    fn start(address: SocketAddr) -> Self {
        let (stop, stopped) = oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let router = rumqttd::Router::new(
                0,
                rumqttd::RouterConfig {
                    max_connections: 16,
                    max_outgoing_packet_count: 500,
                    max_segment_size: 1024 * 1024,
                    max_segment_count: 10,
                    ..Default::default()
                },
            );
            let mut server = rumqttd::Server::new(
                rumqttd::ServerSettings {
                    name: "runtime-loop-test".into(),
                    listen: address,
                    tls: None,
                    next_connection_delay_ms: 0,
                    connections: rumqttd::ConnectionSettings {
                        connection_timeout_ms: 5000,
                        max_payload_size: 1024 * 1024,
                        max_inflight_count: 100,
                        auth: None,
                        external_auth: None,
                        dynamic_filters: true,
                    },
                },
                router.spawn(),
                rumqttd::protocol::v4::V4,
            );
            runtime.block_on(async {
                tokio::select! {
                    result = server.start(rumqttd::LinkType::Remote) => result.unwrap(),
                    _ = stopped => {},
                }
            });
            // Dropping the runtime also drops Server's spawned connection tasks.
        });
        Self {
            address,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
    async fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            tokio::task::spawn_blocking(move || thread.join().unwrap())
                .await
                .unwrap();
        }
    }
    fn restart(&mut self) {
        *self = Self::start(self.address);
    }
}
impl Drop for Broker {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

#[derive(Clone, Default)]
struct MqttView {
    connected: bool,
    subscriptions: u64,
    publications: Vec<Publish>,
}
struct MqttPeer {
    client: AsyncClient,
    port: u16,
    view: watch::Receiver<MqttView>,
    task: JoinHandle<()>,
}
impl MqttPeer {
    fn start(port: u16) -> Self {
        let options = MqttOptions::new(
            format!("runtime-observer-{}", uuid::Uuid::new_v4()),
            "127.0.0.1",
            port,
        );
        let (client, mut events) = AsyncClient::new(options, 256);
        let polling_client = client.clone();
        let (observed, view) = watch::channel(MqttView::default());
        let task = tokio::spawn(async move {
            loop {
                match events.poll().await {
                    Ok(MqttEvent::Incoming(Incoming::ConnAck(_))) => {
                        polling_client.try_subscribe("#", QoS::AtLeastOnce).unwrap();
                        observed.send_modify(|view| view.connected = true);
                    }
                    Ok(MqttEvent::Incoming(Incoming::SubAck(_))) => {
                        observed.send_modify(|view| view.subscriptions += 1);
                    }
                    Ok(MqttEvent::Incoming(Incoming::Publish(publication))) => {
                        observed.send_modify(|view| view.publications.push(publication));
                    }
                    Ok(_) => {}
                    Err(_) => {
                        observed.send_modify(|view| view.connected = false);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
        });
        Self {
            client,
            port,
            view,
            task,
        }
    }
    async fn wait_for(&self, predicate: impl Fn(&MqttView) -> bool) -> MqttView {
        let mut view = self.view.clone();
        tokio::time::timeout(WAIT, async {
            loop {
                if predicate(&view.borrow()) {
                    return view.borrow().clone();
                }
                view.changed().await.expect("MQTT observer stopped");
            }
        })
        .await
        .expect("MQTT observation timed out")
    }
    async fn publish(&self, topic: &str, retained: bool, payload: impl Into<Vec<u8>>) {
        self.client
            .publish(topic, QoS::AtLeastOnce, retained, payload)
            .await
            .unwrap();
    }
    async fn wait_state(&self, name: &str, value: &str) {
        let topic = format!("choregos/state/{name}");
        self.wait_for(|view| {
            view.publications
                .iter()
                .rev()
                .find(|p| p.topic == topic)
                .is_some_and(|p| p.payload.as_ref() == value.as_bytes())
        })
        .await;
    }
    async fn retained(&self, topic: &str) -> Publish {
        let options = MqttOptions::new(
            format!("retained-check-{}", uuid::Uuid::new_v4()),
            "127.0.0.1",
            self.port,
        );
        let (client, mut events) = AsyncClient::new(options, 4);
        tokio::time::timeout(WAIT, async {
            loop {
                match events.poll().await.unwrap() {
                    MqttEvent::Incoming(Incoming::ConnAck(_)) => {
                        client.try_subscribe(topic, QoS::AtLeastOnce).unwrap()
                    }
                    MqttEvent::Incoming(Incoming::Publish(p)) => {
                        assert!(p.retain);
                        return p;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("retained publication missing")
    }
}
impl Drop for MqttPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone, Default)]
struct HaView {
    connections: usize,
    calls: Vec<Value>,
}
struct HaFake {
    url: String,
    view: watch::Receiver<HaView>,
    task: Option<JoinHandle<()>>,
}
impl HaFake {
    async fn start(config: &Config) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let mut initial = vec![
            json!({"entity_id":config.tv.entity,"state":"off","attributes":{}}),
            json!({"entity_id":config.receiver.entity,"state":"off","attributes":{"source":"Other"}}),
            json!({"entity_id":config.sonos.entity,"state":"paused","attributes":{}}),
        ];
        initial.extend(
            config
                .device
                .iter()
                .filter_map(|d| d.media_player.as_ref())
                .map(|entity| json!({"entity_id":entity,"state":"on","attributes":{}})),
        );
        let (observed, view) = watch::channel(HaView::default());
        let task = tokio::spawn(async move {
            let mut sessions = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let observed = observed.clone(); let initial = initial.clone();
                        sessions.spawn(async move { ha_session(socket, observed, initial).await; });
                    }
                    result = sessions.join_next(), if !sessions.is_empty() => { result.unwrap().unwrap(); }
                }
            }
        });
        Self {
            url,
            view,
            task: Some(task),
        }
    }
    async fn wait_for(&self, predicate: impl Fn(&HaView) -> bool) -> HaView {
        let mut view = self.view.clone();
        tokio::time::timeout(WAIT, async {
            loop {
                if predicate(&view.borrow()) {
                    return view.borrow().clone();
                }
                view.changed().await.expect("HA fake stopped");
            }
        })
        .await
        .expect("HA observation timed out")
    }
    async fn shutdown(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}
impl Drop for HaFake {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
async fn send_ha(ws: &mut WebSocketStream<TcpStream>, value: Value) {
    ws.send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}
async fn ha_session(socket: TcpStream, observed: watch::Sender<HaView>, initial: Vec<Value>) {
    let mut ws = accept_async(socket).await.unwrap();
    send_ha(&mut ws, json!({"type":"auth_required"})).await;
    let auth: Value =
        serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(auth["type"], "auth");
    assert_eq!(auth["access_token"], DUMMY_SECRET);
    send_ha(&mut ws, json!({"type":"auth_ok"})).await;
    observed.send_modify(|view| view.connections += 1);
    while let Some(Ok(message)) = ws.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let request: Value = serde_json::from_str(&text).unwrap();
        match request["type"].as_str().unwrap() {
            "subscribe_events" => {
                send_ha(
                    &mut ws,
                    json!({"id":request["id"],"type":"result","success":true,"result":null}),
                )
                .await
            }
            "get_states" => {
                send_ha(
                    &mut ws,
                    json!({"id":request["id"],"type":"result","success":true,"result":initial}),
                )
                .await
            }
            "call_service" => {
                observed.send_modify(|view| view.calls.push(request.clone()));
                send_ha(
                    &mut ws,
                    json!({"id":request["id"],"type":"result","success":true}),
                )
                .await;
            }
            "ping" => send_ha(&mut ws, json!({"id":request["id"],"type":"pong"})).await,
            other => panic!("unexpected HA request type: {other}"),
        }
    }
}

struct Harness {
    state: AppState,
    config: Config,
    path: PathBuf,
    peer: MqttPeer,
    ha: HaFake,
    broker: Broker,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<anyhow::Result<()>>>,
    _directory: tempfile::TempDir,
    _wake: UdpSocket,
}
impl Harness {
    async fn start() -> Self {
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);
        let broker = Broker::start(address);
        let peer = MqttPeer::start(address.port());
        peer.wait_for(|view| view.connected && view.subscriptions > 0)
            .await;
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        let ha = HaFake::start(&config).await;
        config.mqtt.host = "127.0.0.1".into();
        config.mqtt.port = address.port();
        config.home_assistant.url = ha.url.clone();
        let wake = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        config.tv.wake = WakeConfig::Wol {
            mac: "02:11:22:33:44:55".into(),
            broadcast: Ipv4Addr::LOCALHOST,
            port: wake.local_addr().unwrap().port(),
        };
        config.timing.takeover_settle_ms = 25;
        config
            .resolve_secrets(|_| Some(DUMMY_SECRET.into()))
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("choregos.toml");
        let text = toml::to_string_pretty(&config).unwrap().replacen(
            "mode = \"shadow\"",
            "mode = \"shadow\" # preserve this operator comment",
            1,
        );
        tokio::fs::write(&path, &text).await.unwrap();
        let state = AppState::new(config.clone(), text);
        let (tx, mut rx) = mpsc::channel(256);
        state.attach(tx.clone());
        let engine = Engine::new(config.clone(), Instant::now());
        let adapters = Adapters::start(&config, tx.clone(), &[]);
        let mut runtime = Runtime::new(state.clone(), path.clone(), tx, engine, adapters);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = runtime
                .run_loop(&mut rx, async {
                    let _ = stopped.await;
                })
                .await;
            runtime.adapters.take().unwrap().stop().await;
            result
        });
        let harness = Self {
            state,
            config,
            path,
            peer,
            ha,
            broker,
            stop: Some(stop),
            task: Some(task),
            _directory: directory,
            _wake: wake,
        };
        harness
            .peer
            .publish(
                &format!("tele/{}/LWT", harness.config.bridge.topic),
                true,
                "Online",
            )
            .await;
        wait_snapshot(&harness.state, |snapshot| {
            snapshot.connections.mqtt_connected
                && snapshot.connections.ha_connected
                && snapshot.bridge_online
                && snapshot.ha.tv.state != "unknown"
                && snapshot.ha.receiver.state != "unknown"
                && snapshot.ha.sonos.state != "unknown"
                && snapshot
                    .ha
                    .media_players
                    .iter()
                    .all(|entity| entity.state != "unknown")
        })
        .await;
        harness
    }
    async fn publish_wake(&self) {
        let before = self.state.snapshot().recent_frames.len();
        for line in include_str!("../../fixtures/capture-streamer-wake.jsonl").lines() {
            let record: Value = serde_json::from_str(line).unwrap();
            let payload = serde_json::to_vec(&record["payload"]).unwrap();
            let Ok(frame) = Frame::parse(&payload) else {
                continue;
            };
            if frame.from != 4 || !matches!(frame.opcode, Some(0x84 | 0x87 | 0x90 | 0x04 | 0x82)) {
                continue;
            }
            self.peer
                .publish(
                    &format!("tele/{}/SENSOR", self.config.bridge.topic),
                    false,
                    payload,
                )
                .await;
            if frame.opcode == Some(0x82) {
                break;
            }
        }
        wait_snapshot(&self.state, |snapshot| {
            role_on(snapshot)
                && !snapshot.warming
                && snapshot.recent_frames.iter().skip(before).any(|frame| {
                    frame.direction == FrameDirection::Received
                        && frame.from == 4
                        && frame.opcode == Some(0x82)
                })
        })
        .await;
    }
    async fn command(&self, command: Command) {
        tokio::time::timeout(WAIT, self.state.command(command))
            .await
            .unwrap()
            .unwrap();
    }
    async fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            tokio::time::timeout(WAIT, task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        self.ha.shutdown().await;
        self.broker.shutdown().await;
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
fn role_on(snapshot: &Snapshot) -> bool {
    snapshot
        .roles
        .iter()
        .any(|role| role.role == "streamer" && role.power == RolePower::On)
}
async fn wait_snapshot(state: &AppState, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
    tokio::time::timeout(WAIT, async {
        loop {
            let snapshot = state.snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("runtime snapshot did not converge: {:?}", state.snapshot()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shadow_fixture_wake_updates_snapshots_discovery_and_retained_power_without_ha_effects() {
    let mut harness = Harness::start().await;
    harness.publish_wake().await;
    let snapshot = wait_snapshot(&harness.state, |snapshot| {
        snapshot.decisions.iter().any(|decision| {
            decision.role.as_deref() == Some("streamer") && decision.to == Some(RolePower::On)
        })
    })
    .await;
    assert_eq!(snapshot.mode, Mode::Shadow);
    assert!(snapshot.decisions.iter().any(|decision| {
        decision.would
            && decision
                .actions
                .iter()
                .any(|action| action.starts_with("would: "))
    }));
    assert!(
        snapshot
            .recent_frames
            .iter()
            .any(|frame| frame.direction == FrameDirection::Received)
    );
    assert!(
        snapshot
            .recent_frames
            .iter()
            .any(|frame| frame.direction == FrameDirection::Sent && frame.data == "85")
    );
    harness.peer.wait_state("streamer", "ON").await;
    harness
        .peer
        .wait_for(|view| {
            view.publications
                .iter()
                .filter(|p| p.topic.starts_with("homeassistant/"))
                .count()
                >= harness.config.device.len() + 5
        })
        .await;
    let retained = harness.peer.retained("choregos/state/streamer").await;
    assert_eq!(retained.payload.as_ref(), b"ON");
    let discovery = harness
        .peer
        .retained("homeassistant/binary_sensor/choregos/streamer/config")
        .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&discovery.payload).unwrap()["default_entity_id"],
        "binary_sensor.choregos_streamer"
    );
    harness.shutdown().await;
    assert!(harness.ha.view.borrow().calls.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_commands_persist_mode_execute_takeover_and_reload_configuration_through_the_loop() {
    let mut harness = Harness::start().await;
    harness.publish_wake().await;
    harness.command(Command::Standby("streamer".into())).await;
    wait_snapshot(&harness.state, |snapshot| !role_on(snapshot)).await;
    let original = tokio::fs::read_to_string(&harness.path).await.unwrap();
    assert!(original.contains("# preserve this operator comment"));
    assert!(!original.contains("# Environment secrets"));
    assert_eq!(harness.state.config_text(), original);
    harness.command(Command::SetMode(Mode::Active)).await;
    wait_snapshot(&harness.state, |snapshot| snapshot.mode == Mode::Active).await;
    assert_eq!(
        tokio::fs::read_to_string(&harness.path).await.unwrap(),
        original.replacen("mode = \"shadow\"", "mode = \"active\"", 1)
    );
    assert_eq!(
        harness.state.config_text(),
        original.replacen("mode = \"shadow\"", "mode = \"active\"", 1)
    );
    harness.peer.wait_state("mode", "active").await;
    assert!(matches!(
        harness
            .state
            .command(Command::Takeover("missing".into()))
            .await,
        Err(crate::state::CommandError::Invalid(_))
    ));
    harness.command(Command::Takeover("streamer".into())).await;
    harness
        .ha
        .wait_for(|view| {
            view.calls.iter().any(|call| {
                call["type"] == "call_service"
                    && call["service"] == "turn_on"
                    && call["target"]["entity_id"] == harness.config.receiver.entity
            })
        })
        .await;
    wait_snapshot(&harness.state, |snapshot| {
        snapshot
            .decisions
            .iter()
            .any(|decision| decision.reason == "takeover_settled" && !decision.would)
    })
    .await;

    let connections = harness.ha.view.borrow().connections;
    let mut changed = Config::parse(&harness.state.config_text()).unwrap();
    changed.timing.off_delay_s = 0;
    changed.device.retain(|device| device.role != "console_b");
    let submitted = format!(
        "# preserve this operator comment after save\n{}",
        toml::to_string_pretty(&changed).unwrap()
    );
    tokio::time::timeout(WAIT, harness.state.save_config(&submitted))
        .await
        .unwrap()
        .unwrap();
    wait_snapshot(&harness.state, |snapshot| {
        snapshot.connections.mqtt_connected
            && snapshot.connections.ha_connected
            && snapshot.bridge_online
            && snapshot.roles.len() == changed.device.len()
    })
    .await;
    harness
        .ha
        .wait_for(|view| view.connections > connections)
        .await;
    assert_eq!(
        Config::parse(&harness.state.config_text())
            .unwrap()
            .timing
            .off_delay_s,
        0
    );
    assert_eq!(
        Config::parse(&tokio::fs::read_to_string(&harness.path).await.unwrap())
            .unwrap()
            .timing
            .off_delay_s,
        0
    );
    assert_eq!(harness.state.config_text(), submitted);
    assert_eq!(
        tokio::fs::read_to_string(&harness.path).await.unwrap(),
        submitted
    );
    harness.command(Command::SetMode(Mode::Shadow)).await;
    wait_snapshot(&harness.state, |snapshot| snapshot.mode == Mode::Shadow).await;
    let switched = submitted.replacen("mode = \"active\"", "mode = \"shadow\"", 1);
    assert_eq!(harness.state.config_text(), switched);
    assert_eq!(
        tokio::fs::read_to_string(&harness.path).await.unwrap(),
        switched
    );
    harness
        .peer
        .wait_for(|view| {
            view.publications.iter().any(|p| {
                p.topic == "homeassistant/binary_sensor/choregos/console_b/config"
                    && p.payload.is_empty()
            })
        })
        .await;
    harness.publish_wake().await; // a real reply ends reload warming
    harness.command(Command::Standby("streamer".into())).await;
    harness.command(Command::Identify(4)).await; // barrier: the previous input has been snapshotted
    assert!(
        harness
            .state
            .snapshot()
            .decisions
            .iter()
            .any(|decision| decision.reason == "off_timer_expired")
    );
    assert!(!role_on(&harness.state.snapshot()));

    let saved = tokio::fs::read_to_string(&harness.path).await.unwrap();
    changed.web.bind.set_port(9000);
    let error = harness
        .state
        .save_config(&changed.redacted_text().unwrap())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("restart"));
    assert_eq!(
        tokio::fs::read_to_string(&harness.path).await.unwrap(),
        saved
    );
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_restart_reinterrogates_the_bridge_and_ha_disconnect_invalidates_tracked_states() {
    let mut harness = Harness::start().await;
    harness.publish_wake().await;
    harness.broker.shutdown().await;
    let disconnected = wait_snapshot(&harness.state, |snapshot| {
        !snapshot.connections.mqtt_connected && !snapshot.bridge_online
    })
    .await;
    assert!(role_on(&disconnected));
    let generation = harness.peer.view.borrow().subscriptions;
    let offset = harness.peer.view.borrow().publications.len();
    harness.broker.restart();
    harness
        .peer
        .wait_for(|view| view.connected && view.subscriptions > generation)
        .await;
    harness
        .peer
        .publish(
            &format!("tele/{}/LWT", harness.config.bridge.topic),
            true,
            "Online",
        )
        .await;
    wait_snapshot(&harness.state, |snapshot| {
        snapshot.connections.mqtt_connected && snapshot.bridge_online
    })
    .await;
    let topic = format!("cmnd/{}/HdmiSend", harness.config.bridge.topic);
    harness
        .peer
        .wait_for(|view| {
            ["85", "8F"].iter().all(|opcode| {
                view.publications.iter().skip(offset).any(|p| {
                    p.topic == topic
                        && serde_json::from_slice::<Value>(&p.payload)
                            .is_ok_and(|value| value["Data"] == *opcode)
                })
            })
        })
        .await;
    harness.ha.shutdown().await;
    let snapshot = wait_snapshot(&harness.state, |snapshot| {
        !snapshot.connections.ha_connected
            && snapshot.ha.tv.state == "unknown"
            && snapshot.ha.receiver.state == "unknown"
            && snapshot.ha.sonos.state == "unknown"
            && snapshot
                .ha
                .media_players
                .iter()
                .all(|entity| entity.state == "unknown")
    })
    .await;
    assert!(snapshot.connections.mqtt_connected);
    harness.shutdown().await;
}
