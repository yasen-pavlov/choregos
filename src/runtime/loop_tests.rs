//! Loopback-only acceptance tests of the production scheduling loop and adapters.
use super::{Adapters, Runtime};
use crate::{
    cec::Frame,
    config::{Config, WakeConfig},
    state::{AppState, Command, FrameDirection, Mode, RolePower, Snapshot},
};
use futures_util::{SinkExt, StreamExt};
use rumqttc::{AsyncClient, Event as MqttEvent, Incoming, MqttOptions, Publish, QoS};
use serde_json::{Value, json};
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
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
    task: Option<JoinHandle<anyhow::Result<crate::engine::Engine>>>,
    _directory: tempfile::TempDir,
    _wake: UdpSocket,
}
impl Harness {
    async fn start() -> Self {
        Self::start_with_bridge(false).await
    }
    async fn start_with_bridge(native: bool) -> Self {
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);
        let broker = Broker::start(address);
        let peer = MqttPeer::start(address.port());
        peer.wait_for(|view| view.connected && view.subscriptions > 0)
            .await;
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        if native {
            config.bridge = crate::config::BridgeConfig::Aether {
                node: "cec-bridge".into(),
                logical_address: 0,
            };
            config.fake_tv.replies = false;
            config.mode = Mode::Active;
            // Publish before the runtime subscribes: only retained redelivery can start resync.
            peer.publish(
                "aether/cec-bridge/status",
                true,
                r#"{"online":true,"v":1,"boot":1,"bus":"ok"}"#,
            )
            .await;
            peer.wait_for(|view| {
                view.publications
                    .iter()
                    .any(|p| p.topic == "aether/cec-bridge/status")
            })
            .await;
            peer.retained("aether/cec-bridge/status").await;
        }
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
        // Native outcome verification is ticked explicitly after taking back the
        // stopped loop's engine, not by waiting for a wall-clock settle timer.
        config.timing.takeover_settle_ms = if native { 60_000 } else { 25 };
        if native {
            config.timing.standby_retries = 0;
        }
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
        let (state, stop, task) = Self::start_runtime(&config, &path, text).await;
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
        if !native {
            harness
                .peer
                .publish(
                    &format!("tele/{}/LWT", harness.config.bridge.name()),
                    true,
                    "Online",
                )
                .await;
        }
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
    async fn start_runtime(
        config: &Config,
        path: &std::path::Path,
        text: String,
    ) -> (
        AppState,
        oneshot::Sender<()>,
        JoinHandle<anyhow::Result<crate::engine::Engine>>,
    ) {
        let state = AppState::new(config.clone(), text);
        let (tx, mut rx) = mpsc::channel(256);
        state.attach(tx.clone());
        let adapters = Adapters::start(config, tx.clone(), &[]);
        let mut runtime = Runtime::new(
            state.clone(),
            path.to_path_buf(),
            tx,
            config.clone(),
            adapters,
        )
        .await;
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = runtime
                .run_loop(&mut rx, async {
                    let _ = stopped.await;
                })
                .await;
            runtime.adapters.take().unwrap().stop().await;
            result?;
            Ok(runtime.engine)
        });
        (state, stop, task)
    }

    async fn restart_runtime(&mut self) {
        self.stop_runtime().await;
        let text = tokio::fs::read_to_string(&self.path).await.unwrap();
        let (state, stop, task) = Self::start_runtime(&self.config, &self.path, text).await;
        self.state = state;
        self.stop = Some(stop);
        self.task = Some(task);
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
                    &format!("tele/{}/SENSOR", self.config.bridge.name()),
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
    async fn stop_runtime(&mut self) {
        self.take_engine().await;
    }
    async fn take_engine(&mut self) -> Option<crate::engine::Engine> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            return Some(
                tokio::time::timeout(WAIT, task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
            );
        }
        None
    }
    async fn shutdown(&mut self) {
        self.stop_runtime().await;
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
                >= harness.config.device.len() + 8
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
            &format!("tele/{}/LWT", harness.config.bridge.name()),
            true,
            "Online",
        )
        .await;
    wait_snapshot(&harness.state, |snapshot| {
        snapshot.connections.mqtt_connected && snapshot.bridge_online
    })
    .await;
    let topic = format!("cmnd/{}/HdmiSend", harness.config.bridge.name());
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
    use super::trace::{self, TraceEntry};
    let text = std::fs::read_to_string(
        harness
            .path
            .with_file_name(format!("cec-bus.{}.jsonl", chrono::Utc::now().date_naive())),
    )
    .unwrap();
    let entries: Vec<_> = text
        .lines()
        .filter_map(trace::parse)
        .map(|(_, entry)| entry)
        .collect();
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry, TraceEntry::Connection { mqtt: false }))
    );
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry, TraceEntry::Connection { mqtt: true }))
    );
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, TraceEntry::Lwt { online: false, .. }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_restores_history_but_waits_for_physical_verification_to_match_roles() {
    let mut harness = Harness::start().await;
    harness.publish_wake().await;
    let previous = wait_snapshot(&harness.state, |snapshot| {
        snapshot
            .decisions
            .iter()
            .any(|decision| decision.reason == "takeover_settled")
    })
    .await;
    harness.restart_runtime().await;
    let restored = harness.state.snapshot();
    let streamer = restored
        .roles
        .iter()
        .find(|role| role.role == "streamer")
        .unwrap();
    assert_eq!(streamer.logical_address, None);
    let entry = restored
        .bus
        .iter()
        .find(|entry| entry.logical_address == 4)
        .unwrap();
    assert!(!entry.verified);
    assert_eq!(entry.last_seen, None);
    assert_eq!(streamer.power, RolePower::Off);
    assert_eq!(
        restored
            .bus
            .iter()
            .find(|entry| entry.logical_address == 4)
            .unwrap()
            .power,
        crate::state::BusPower::Unknown
    );
    assert!(restored.active_role.is_none());
    assert!(
        !restored
            .recent_frames
            .iter()
            .any(|frame| frame.direction == FrameDirection::Received)
    );
    assert_eq!(restored.decisions.len(), previous.decisions.len() + 1);
    assert!(
        restored
            .decisions
            .iter()
            .take(previous.decisions.len())
            .eq(&previous.decisions)
    );
    let boundary = restored.decisions.back().unwrap();
    assert_eq!(boundary.reason, "service_started");
    assert!(!boundary.would);
    assert!(boundary.actions.is_empty());
    wait_snapshot(&harness.state, |snapshot| {
        snapshot.recent_frames.iter().any(|frame| {
            frame.direction == FrameDirection::Sent && frame.data == "83" && frame.to == 4
        })
    })
    .await;
    harness
        .peer
        .publish(
            &format!("tele/{}/SENSOR", harness.config.bridge.name()),
            false,
            serde_json::to_vec(&json!({"HdmiReceived":{"From":4,"To":15,"Data":"84210004"}}))
                .unwrap(),
        )
        .await;
    wait_snapshot(&harness.state, |snapshot| {
        snapshot
            .roles
            .iter()
            .any(|role| role.role == "streamer" && role.logical_address == Some(4))
    })
    .await;
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_or_unknown_state_starts_empty_without_failing_runtime() {
    let mut harness = Harness::start().await;
    harness.publish_wake().await;
    for invalid in [
        b"not JSON".as_slice(),
        br#"{"version":2,"identities":[],"decisions":[]}"#.as_slice(),
    ] {
        harness.stop_runtime().await;
        tokio::fs::write(harness.path.with_file_name("choregos.state.json"), invalid)
            .await
            .unwrap();
        harness.restart_runtime().await;
        let snapshot = harness.state.snapshot();
        assert!(snapshot.bus.is_empty());
        assert!(
            snapshot
                .roles
                .iter()
                .all(|role| role.logical_address.is_none())
        );
        assert_eq!(snapshot.decisions.len(), 1);
        assert_eq!(snapshot.decisions[0].reason, "service_started");
    }
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_writes_ignore_ordinary_frames_and_shutdown_flushes_latest_decision() {
    use std::os::unix::fs::MetadataExt;
    let mut harness = Harness::start().await;
    harness.publish_wake().await;
    wait_snapshot(&harness.state, |snapshot| {
        snapshot
            .decisions
            .iter()
            .any(|decision| decision.reason == "takeover_settled")
    })
    .await;
    let path = harness.path.with_file_name("choregos.state.json");
    wait_persisted(&path, |saved| {
        saved
            .decisions
            .last()
            .is_some_and(|decision| decision.reason == "takeover_settled")
    })
    .await;
    let inode = tokio::fs::metadata(&path).await.unwrap().ino();
    let publish_poll = || async {
        let before = harness.state.snapshot().recent_frames.len();
        harness
            .peer
            .publish(
                &format!("tele/{}/SENSOR", harness.config.bridge.name()),
                false,
                serde_json::to_vec(&json!({"HdmiReceived":{"From":4,"To":0,"Data":""}})).unwrap(),
            )
            .await;
        wait_snapshot(&harness.state, |snapshot| {
            snapshot.recent_frames.len() > before
        })
        .await;
    };
    publish_poll().await;
    assert_eq!(tokio::fs::metadata(&path).await.unwrap().ino(), inode);
    harness.command(Command::Identify(4)).await;
    wait_snapshot(&harness.state, |snapshot| {
        snapshot
            .decisions
            .back()
            .is_some_and(|decision| decision.reason == "command_identify")
    })
    .await;
    harness.stop_runtime().await;
    let saved: super::PersistedState =
        serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
    assert_eq!(saved.decisions.last().unwrap().reason, "command_identify");
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restored_decision_ring_keeps_newest_entries_and_restart_boundary() {
    let mut harness = Harness::start().await;
    harness.stop_runtime().await;
    let mut decision = harness.state.snapshot().decisions[0].clone();
    let decisions = (0..2005)
        .map(|index| {
            decision.reason = format!("decision_{index}");
            decision.clone()
        })
        .collect();
    let saved = super::PersistedState {
        version: 1,
        identities: vec![],
        decisions,
    };
    tokio::fs::write(
        harness.path.with_file_name("choregos.state.json"),
        serde_json::to_vec(&saved).unwrap(),
    )
    .await
    .unwrap();
    harness.restart_runtime().await;
    let snapshot = harness.state.snapshot();
    assert_eq!(snapshot.decisions.len(), 2000);
    assert_eq!(snapshot.decisions.front().unwrap().reason, "decision_6");
    assert_eq!(snapshot.decisions[1998].reason, "decision_2004");
    assert_eq!(snapshot.decisions.back().unwrap().reason, "service_started");
    harness.shutdown().await;
}

async fn wait_persisted(
    path: &std::path::Path,
    predicate: impl Fn(&super::PersistedState) -> bool,
) {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(bytes) = tokio::fs::read(path).await
                && let Ok(saved) = serde_json::from_slice(&bytes)
                && predicate(&saved)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("state file did not converge");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_captures_replay_inputs_and_publishes_retained_bridge_telemetry() {
    use super::trace::{self, TraceEntry};
    let mut harness = Harness::start().await;
    assert!(harness.state.snapshot().bridge_telemetry.is_none());
    assert!(
        !harness
            .peer
            .view
            .borrow()
            .publications
            .iter()
            .any(|p| p.topic == "choregos/state/bridge_signal")
    );
    harness.publish_wake().await;
    harness.command(Command::Identify(4)).await;
    harness
        .peer
        .publish(
            &format!("stat/{}/RESULT", harness.config.bridge.name()),
            false,
            r#"{"HdmiSend":{"Ack":false}}"#,
        )
        .await;
    let before = chrono::Utc::now();
    harness
        .peer
        .publish(
            &format!("tele/{}/STATE", harness.config.bridge.name()),
            false,
            r#"{"UptimeSec":282348.0,"Wifi":{"RSSI":100,"Signal":-50.0,"LinkCount":2.0}}"#,
        )
        .await;
    let snapshot = wait_snapshot(&harness.state, |s| s.bridge_telemetry.is_some()).await;
    let telemetry = snapshot.bridge_telemetry.unwrap();
    assert_eq!(
        (
            telemetry.signal_dbm,
            telemetry.link_count,
            telemetry.uptime_s
        ),
        (-50, 2, 282348)
    );
    assert!(telemetry.at >= before);
    assert!(snapshot.last_frame_at.is_some());
    for (name, value) in [
        ("bridge_signal", "-50"),
        ("bridge_link_count", "2"),
        ("bridge_uptime", "282348"),
    ] {
        harness.peer.wait_state(name, value).await;
        let retained = harness
            .peer
            .retained(&format!("choregos/state/{name}"))
            .await;
        assert_eq!(retained.payload.as_ref(), value.as_bytes());
        let discovery = harness
            .peer
            .retained(&format!("homeassistant/sensor/choregos/{name}/config"))
            .await;
        let payload: Value = serde_json::from_slice(&discovery.payload).unwrap();
        assert_eq!(payload["entity_category"], "diagnostic");
        match name {
            "bridge_signal" => {
                assert_eq!(payload["unit_of_measurement"], "dBm");
                assert_eq!(payload["device_class"], "signal_strength");
                assert_eq!(payload["state_class"], "measurement");
            }
            "bridge_link_count" => assert_eq!(payload["state_class"], "total_increasing"),
            _ => {
                assert_eq!(payload["unit_of_measurement"], "s");
                assert_eq!(payload["device_class"], "duration");
            }
        }
    }
    harness.shutdown().await;
    let mut entries = vec![];
    for file in std::fs::read_dir(harness.path.parent().unwrap()).unwrap() {
        let file = file.unwrap();
        if file.file_name().to_string_lossy().starts_with("cec-bus.") {
            let text = std::fs::read_to_string(file.path()).unwrap();
            assert!(matches!(
                trace::parse(text.lines().next().unwrap()).unwrap().1,
                TraceEntry::Identities(_)
            ));
            entries.extend(text.lines().map(|line| trace::parse(line).unwrap().1));
        }
    }
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry, TraceEntry::Received { .. }))
    );
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry, TraceEntry::Lwt { online: true, .. }))
    );
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry, TraceEntry::Sent { .. }))
    );
    assert!(entries.iter().any(|entry| matches!(entry, TraceEntry::Result { payload, .. } if payload["HdmiSend"]["Ack"] == false)));
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry, TraceEntry::HaState { .. }))
    );
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry, TraceEntry::Command(Command::Identify(4))))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telemetry_after_offline_does_not_mark_the_bridge_online() {
    let mut harness = Harness::start().await;
    harness
        .peer
        .publish(
            &format!("tele/{}/LWT", harness.config.bridge.name()),
            true,
            "Offline",
        )
        .await;
    let offline = wait_snapshot(&harness.state, |snapshot| !snapshot.bridge_online).await;
    assert!(offline.bridge_telemetry.is_none());
    assert!(!offline.connections.bridge_online);
    harness
        .peer
        .publish(
            &format!("tele/{}/STATE", harness.config.bridge.name()),
            false,
            r#"{"UptimeSec":60,"Wifi":{"Signal":-80,"LinkCount":3}}"#,
        )
        .await;
    let snapshot = wait_snapshot(&harness.state, |snapshot| {
        snapshot.bridge_telemetry.is_some()
    })
    .await;
    assert!(!snapshot.bridge_online);
    assert!(!snapshot.connections.bridge_online);
    let telemetry = snapshot.bridge_telemetry.unwrap();
    assert_eq!(
        (
            telemetry.signal_dbm,
            telemetry.link_count,
            telemetry.uptime_s
        ),
        (-80, 3, 60)
    );
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_retained_status_resync_and_id_correlated_outcomes_reach_the_engine() {
    let mut harness = Harness::start_with_bridge(true).await;
    assert_eq!(harness.state.snapshot().bridge_bus.as_deref(), Some("ok"));
    let topic = "aether/cec-bridge/tx";
    let observed = harness
        .peer
        .wait_for(|view| {
            view.publications
                .iter()
                .filter(|p| p.topic == topic)
                .count()
                >= 5
        })
        .await;
    let resync: Vec<Value> = observed
        .publications
        .iter()
        .filter(|p| p.topic == topic)
        .map(|p| serde_json::from_slice(&p.payload).unwrap())
        .collect();
    let mut previous = 0;
    for send in &resync {
        let number = send["id"]
            .as_str()
            .unwrap()
            .strip_prefix("s-")
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(number > previous);
        previous = number;
        assert!(send.get("retries").is_none());
    }
    assert_eq!(resync[0]["to"], 15);
    assert_eq!(resync[0]["data"], "85");
    let mut seq = 0_u32;
    for (from, to, data) in [
        (0, 1, "8F"), // Promiscuous own-send echo must not create a bus entry.
        (4, 15, "84210004"),
        (4, 15, "870010FA"),
        (4, 0, "4753747265616D6572"),
        (4, 15, "822100"),
    ] {
        seq += 1;
        harness.peer.publish("aether/cec-bridge/bus", false, json!({"ev":"rx","seq":seq,"t_us":seq*1000,"from":from,"to":to,"data":data,"ack":true}).to_string()).await;
    }
    let snapshot = wait_snapshot(&harness.state, role_on).await;
    assert!(!snapshot.bus.iter().any(|entry| entry.logical_address == 0));
    assert!(
        snapshot
            .recent_frames
            .iter()
            .any(|f| f.from == 0 && f.direction == FrameDirection::Received && f.ack == Some(true))
    );
    seq += 1;
    harness.peer.publish("aether/cec-bridge/bus",false,json!({"ev":"tx","seq":seq,"t_us":6000,"origin":"local","from":0,"to":4,"data":"9000","status":"ack","attempts":1}).to_string()).await;
    wait_snapshot(&harness.state, |s| {
        s.recent_frames
            .iter()
            .any(|f| f.direction == FrameDirection::Local && f.ack == Some(true))
    })
    .await;
    seq += 1;
    harness.peer.publish("aether/cec-bridge/bus", false, json!({"ev":"rx","seq":seq,"t_us":seq*1000,"from":4,"to":0,"data":"475374","ack":false}).to_string()).await;
    let snapshot = wait_snapshot(&harness.state, |s| {
        s.recent_frames
            .iter()
            .any(|f| f.direction == FrameDirection::Received && f.data == "475374")
    })
    .await;
    assert!(role_on(&snapshot));
    assert!(snapshot.bus.iter().any(|entry| entry.logical_address == 4
        && entry.verified
        && entry.osd.as_deref() == Some("Streamer")));
    let before = harness.peer.view.borrow().publications.len();
    harness.command(Command::Standby("streamer".into())).await;
    let observed = harness
        .peer
        .wait_for(|view| {
            view.publications
                .iter()
                .skip(before)
                .filter(|p| p.topic == topic)
                .count()
                >= 2
        })
        .await;
    let sends: Vec<Value> = observed
        .publications
        .iter()
        .skip(before)
        .filter(|p| p.topic == topic)
        .map(|p| serde_json::from_slice(&p.payload).unwrap())
        .collect();
    for send in sends
        .iter()
        .filter(|send| send["to"] == 4 && (send["data"] == "36" || send["data"] == "8F"))
    {
        seq += 1;
        harness.peer.publish("aether/cec-bridge/bus",false,json!({"ev":"tx","seq":seq,"t_us":seq*1000,"origin":"host","from":0,"to":4,"data":send["data"],"id":send["id"],"status":"nack","attempts":2}).to_string()).await;
    }
    // The line marker is on the same ordered stream, so seeing it proves that
    // both outcomes have already reached the engine before we take ownership.
    seq += 1;
    harness
        .peer
        .publish(
            "aether/cec-bridge/bus",
            false,
            json!({"ev":"line","seq":seq,"bus":"stuck_low"}).to_string(),
        )
        .await;
    wait_snapshot(&harness.state, |s| {
        s.bridge_bus.as_deref() == Some("stuck_low")
    })
    .await;
    let mut engine = harness.take_engine().await.unwrap();
    let now = std::time::Instant::now() + Duration::from_secs(60);
    engine.handle(crate::engine::Event::Tick, now, chrono::Utc::now());
    assert!(
        engine
            .decisions
            .iter()
            .any(|d| d.reason == "standby_target_absent")
    );
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_bridge_kind_switch_subscribes_to_native_status_and_bus_and_resyncs() {
    let mut harness = Harness::start().await;
    assert_eq!(harness.state.bridge_max_raw_bytes(), 14);
    harness
        .peer
        .publish(
            "aether/cec-bridge/status",
            true,
            r#"{"online":true,"v":1,"boot":7,"t_us":1000,"bus":"ok"}"#,
        )
        .await;
    harness
        .peer
        .wait_for(|view| {
            view.publications
                .iter()
                .any(|p| p.topic == "aether/cec-bridge/status")
        })
        .await;
    // Observer delivery proves the publish was processed before this retained read.
    harness.peer.retained("aether/cec-bridge/status").await;
    let before = harness.peer.view.borrow().publications.len();
    let mut changed = harness.config.clone();
    changed.bridge = crate::config::BridgeConfig::Aether {
        node: "cec-bridge".into(),
        logical_address: 0,
    };
    changed.fake_tv.replies = false;
    let text = toml::to_string_pretty(&changed).unwrap();
    harness.state.save_config(&text).await.unwrap();
    let snapshot = wait_snapshot(&harness.state, |s| {
        s.bridge_online && s.bridge_bus.as_deref() == Some("ok") && s.bridge_max_raw_bytes == 15
    })
    .await;
    assert!(snapshot.connections.mqtt_connected);
    assert_eq!(harness.state.bridge_max_raw_bytes(), 15);
    let observed = harness
        .peer
        .wait_for(|view| {
            view.publications
                .iter()
                .skip(before)
                .filter(|p| p.topic == "aether/cec-bridge/tx")
                .count()
                >= 5
        })
        .await;
    let sends: Vec<Value> = observed
        .publications
        .iter()
        .skip(before)
        .filter(|p| p.topic == "aether/cec-bridge/tx")
        .map(|p| serde_json::from_slice(&p.payload).unwrap())
        .collect();
    assert_eq!(sends[0]["to"], 15);
    assert_eq!(sends[0]["data"], "85");
    assert!(
        sends
            .iter()
            .all(|send| send["id"].as_str().unwrap().starts_with("s-"))
    );

    // Old bridge topics must no longer feed the model; the new bus subscription
    // must decode identity and activation through the replacement adapter.
    harness
        .peer
        .publish(
            &format!("tele/{}/SENSOR", harness.config.bridge.name()),
            false,
            r#"{"HdmiReceived":{"From":8,"To":0,"Data":"04"}}"#,
        )
        .await;
    for (seq, to, data) in [
        (1, 15, "84210004"),
        (2, 15, "870010FA"),
        (3, 0, "4753747265616D6572"),
        (4, 15, "822100"),
    ] {
        harness.peer.publish("aether/cec-bridge/bus", false,
            json!({"ev":"rx","seq":seq,"from":4,"to":to,"data":data,"ack":true,"t_us":seq*1000}).to_string()).await;
    }
    let snapshot = wait_snapshot(&harness.state, role_on).await;
    assert!(
        snapshot
            .bus
            .iter()
            .any(|entry| entry.logical_address == 4 && entry.identified)
    );
    assert!(!snapshot.bus.iter().any(|entry| entry.logical_address == 8));
    assert_eq!(harness.state.config_text(), text);
    assert_eq!(
        tokio::fs::read_to_string(&harness.path).await.unwrap(),
        text
    );
    harness
        .peer
        .publish("aether/cec-bridge/status", true, r#"{"online":false}"#)
        .await;
    let offline = wait_snapshot(&harness.state, |s| !s.bridge_online).await;
    assert!(offline.bridge_bus.is_none());
    harness.shutdown().await;
}
