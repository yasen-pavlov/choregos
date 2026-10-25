pub mod cli;
pub mod executor;
pub mod ha;
pub mod mqtt;
pub mod wol;

use crate::{
    action::Action,
    cec::{Frame, frame::physical_address, opcode},
    config::{Config, ConfigError},
    engine::{Engine, Event},
    state::{
        AppState, BusEntryView, Command, CommandError, DecisionView, FrameDirection, FrameView,
        HaEntityView, HaView, Mode, RoleView, Snapshot, UiEvent,
    },
};
use chrono::{DateTime, Utc};
use std::{
    collections::{BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, watch};

pub(crate) enum Request {
    Command(Command, Option<oneshot::Sender<Result<(), CommandError>>>),
    SaveConfig(String, oneshot::Sender<Result<(), ConfigError>>),
}
pub(crate) enum Input {
    Event(Event),
    Connection { mqtt: bool, connected: bool },
    Request(Request),
}
struct Adapters {
    executor: Arc<executor::Executor>,
    mqtt_task: tokio::task::JoinHandle<()>,
    ha_task: tokio::task::JoinHandle<()>,
    effects: mpsc::Sender<(Arc<Config>, Vec<Action>)>,
    worker: tokio::task::JoinHandle<()>,
}
impl Adapters {
    fn start(config: &Config, tx: mpsc::Sender<Input>, removed_roles: &[String]) -> Self {
        let (mqtt, mqtt_task) = mqtt::Mqtt::start(config, tx.clone(), removed_roles);
        let (ha, ha_task) = ha::Ha::start(config, tx);
        let executor = Arc::new(executor::Executor {
            mqtt,
            ha,
            active: Arc::new(AtomicBool::new(config.mode == Mode::Active)),
        });
        let worker_executor = executor.clone();
        let (effects, mut rx) = mpsc::channel::<(Arc<Config>, Vec<Action>)>(64);
        let worker = tokio::spawn(async move {
            while let Some((config, actions)) = rx.recv().await {
                for action in actions {
                    if let Err(error) = worker_executor.execute(&action, &config).await {
                        tracing::error!(error = %error, "action failed");
                    }
                }
            }
        });
        Self {
            executor,
            mqtt_task,
            ha_task,
            effects,
            worker,
        }
    }
    fn dispatch(&self, actions: Vec<Action>, engine: &Engine) -> Vec<Frame> {
        let known_las: Vec<_> = engine.identity.roles.values().copied().collect();
        let mut ordered = vec![];
        let mut sent = vec![];
        for action in actions {
            if let Some(frames) = self.executor.fast_lane(&action, &engine.config, &known_las) {
                sent.extend(frames);
            } else {
                ordered.push(action);
            }
        }
        if !ordered.is_empty()
            && self
                .effects
                .try_send((engine.config.clone(), ordered))
                .is_err()
        {
            tracing::warn!("dropping ordered effects: worker queue full or stopped");
        }
        sent
    }
    async fn stop(self) {
        drop(self.effects);
        let mut worker = self.worker;
        if tokio::time::timeout(Duration::from_secs(3), &mut worker)
            .await
            .is_err()
        {
            tracing::warn!("ordered worker drain timed out; cancelling remaining effects");
            worker.abort();
        }
        self.executor.mqtt.shutdown();
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.ha_task.abort();
        self.mqtt_task.abort();
    }
}

struct Runtime {
    state: AppState,
    path: PathBuf,
    tx: mpsc::Sender<Input>,
    engine: Engine,
    adapters: Option<Adapters>,
    snapshot: Snapshot,
    last_config: Arc<Config>,
    last_states: String,
    removed_roles: BTreeSet<String>,
}

impl Runtime {
    fn new(
        state: AppState,
        path: PathBuf,
        tx: mpsc::Sender<Input>,
        engine: Engine,
        adapters: Adapters,
    ) -> Self {
        Self {
            snapshot: state.snapshot(),
            last_config: engine.config.clone(),
            state,
            path,
            tx,
            engine,
            adapters: Some(adapters),
            last_states: String::new(),
            removed_roles: BTreeSet::new(),
        }
    }

    fn adapters(&self) -> &Adapters {
        self.adapters
            .as_ref()
            .expect("adapters running outside reload")
    }

    async fn handle_input(
        &mut self,
        input: Input,
        now: Instant,
        wall: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let mut ui_events = vec![];
        let events = match input {
            Input::Event(event) => {
                if let Event::Frame(frame) = &event {
                    let view = frame_view(frame, wall, FrameDirection::Received, &self.engine);
                    push_ring(&mut self.snapshot.recent_frames, view.clone(), 2000);
                    ui_events.push(UiEvent::Frame(view));
                }
                vec![event]
            }
            Input::Connection { mqtt, connected } => self.handle_connection(mqtt, connected),
            Input::Request(Request::Command(command, reply)) => {
                self.handle_command(command, reply).await
            }
            Input::Request(Request::SaveConfig(text, reply)) => {
                self.handle_config_save(text, reply).await
            }
        };
        self.apply_events(events, now, wall, &mut ui_events);
        self.publish_snapshot(now, wall, ui_events)
    }

    fn handle_connection(&mut self, mqtt: bool, connected: bool) -> Vec<Event> {
        if mqtt {
            self.snapshot.connections.mqtt_connected = connected;
        } else {
            self.snapshot.connections.ha_connected = connected;
        }
        if mqtt && connected {
            self.last_states.clear();
        }
        disconnected_events(mqtt, connected, &self.engine.config)
    }

    async fn handle_command(
        &mut self,
        command: Command,
        reply: Option<oneshot::Sender<Result<(), CommandError>>>,
    ) -> Vec<Event> {
        let result = match validate_command(&command, &self.engine) {
            Ok(()) => {
                if let Command::SetMode(mode) = command {
                    persist_mode(mode, &self.path)
                        .await
                        .map_err(CommandError::from)
                } else {
                    Ok(())
                }
            }
            Err(error) => Err(error),
        };
        let success = result.is_ok();
        if success && let Command::SetMode(mode) = command {
            self.adapters()
                .executor
                .active
                .store(mode == Mode::Active, Ordering::Release);
        }
        if let Some(reply) = reply {
            let _ = reply.send(result);
        } else if let Err(error) = result {
            tracing::warn!(error = %error, "MQTT command rejected");
        }
        if success {
            vec![Event::Command(command)]
        } else {
            vec![]
        }
    }

    async fn handle_config_save(
        &mut self,
        text: String,
        reply: oneshot::Sender<Result<(), ConfigError>>,
    ) -> Vec<Event> {
        match reload(&text, &self.engine.config, &self.path).await {
            Ok(config) => {
                self.restart_adapters(&config).await;
                let _ = reply.send(Ok(()));
                // ConfigReloaded updates the model; the retained Online LWT performs resync.
                vec![
                    Event::BridgeOffline,
                    Event::ConfigReloaded(Arc::new(config)),
                ]
            }
            Err(error) => {
                let _ = reply.send(Err(error));
                vec![]
            }
        }
    }

    async fn restart_adapters(&mut self, config: &Config) {
        self.removed_roles.extend(
            self.engine
                .config
                .device
                .iter()
                .filter(|device| !config.device.iter().any(|new| new.role == device.role))
                .map(|device| device.role.clone()),
        );
        self.removed_roles
            .retain(|role| !config.device.iter().any(|device| &device.role == role));
        self.adapters()
            .executor
            .active
            .store(config.mode == Mode::Active, Ordering::Release);
        self.adapters
            .take()
            .expect("adapters running before reload")
            .stop()
            .await;
        self.adapters = Some(Adapters::start(
            config,
            self.tx.clone(),
            &self.removed_roles.iter().cloned().collect::<Vec<_>>(),
        ));
        self.snapshot.connections.mqtt_connected = false;
        self.snapshot.connections.ha_connected = false;
        self.last_states.clear();
    }

    fn apply_events(
        &mut self,
        events: Vec<Event>,
        now: Instant,
        wall: DateTime<Utc>,
        ui_events: &mut Vec<UiEvent>,
    ) {
        for event in events {
            let actions = self.engine.handle(event, now, wall);
            self.record_decisions(wall, ui_events);
            for frame in self.adapters().dispatch(actions, &self.engine) {
                let view = frame_view(&frame, wall, FrameDirection::Sent, &self.engine);
                push_ring(&mut self.snapshot.recent_frames, view.clone(), 2000);
                ui_events.push(UiEvent::Frame(view));
            }
        }
    }

    fn record_decisions(&mut self, wall: DateTime<Utc>, ui_events: &mut Vec<UiEvent>) {
        for decision in &self.engine.decisions {
            tracing::info!(event = "decision", role = ?decision.role, from = ?decision.from, to = ?decision.to, reason = %decision.reason);
            let view = DecisionView {
                timestamp: wall,
                role: decision.role.clone(),
                from: decision.from,
                to: decision.to,
                reason: decision.reason.clone(),
                actions: decision
                    .actions
                    .iter()
                    .map(|action| {
                        format!(
                            "{}{}",
                            if executor::suppressed(self.engine.mode(), action) {
                                "would: "
                            } else {
                                ""
                            },
                            serde_json::to_string(action).expect("serializable action"),
                        )
                    })
                    .collect(),
                would: self.engine.mode() == Mode::Shadow,
            };
            push_ring(&mut self.snapshot.decisions, view.clone(), 2000);
            ui_events.push(UiEvent::Decision(view));
        }
    }

    fn publish_snapshot(
        &mut self,
        now: Instant,
        wall: DateTime<Utc>,
        ui_events: Vec<UiEvent>,
    ) -> anyhow::Result<()> {
        update_snapshot(&mut self.snapshot, &self.engine, now, wall);
        if !Arc::ptr_eq(&self.last_config, &self.engine.config) {
            self.state
                .replace_config_text(self.engine.config.redacted_text()?);
            self.last_config = self.engine.config.clone();
        }
        let entity_states = format!(
            "{:?}{:?}{:?}{:?}{:?}",
            self.snapshot.mode,
            self.snapshot.bridge_online,
            self.snapshot.active_role,
            self.snapshot.conflicts.len(),
            self.snapshot
                .roles
                .iter()
                .map(|role| role.power)
                .collect::<Vec<_>>(),
        );
        if entity_states != self.last_states {
            self.adapters().executor.mqtt.states(&self.snapshot);
            self.last_states = entity_states;
        }
        self.state.publish(self.snapshot.clone(), ui_events);
        Ok(())
    }
}

pub async fn run(config: Config, path: PathBuf) -> anyhow::Result<()> {
    let state = AppState::new(config.clone(), &path)?;
    let listener = tokio::net::TcpListener::bind(config.web.bind).await?;
    let (shutdown, mut stopped) = watch::channel(false);
    let mut server = tokio::spawn({
        let router = crate::web::router(state.clone());
        async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = stopped.wait_for(|value| *value).await;
                })
                .await
        }
    });
    let (tx, mut rx) = mpsc::channel(256);
    state.attach(tx.clone());
    let engine = Engine::new(config, Instant::now());
    let adapters = Adapters::start(&engine.config, tx.clone(), &[]);
    // The retained bridge LWT initiates the single startup resync.
    let mut runtime = Runtime::new(state, path, tx, engine, adapters);
    let shutdown_signal = shutdown_signal();
    tokio::pin!(shutdown_signal);
    loop {
        let deadline = runtime
            .engine
            .next_deadline()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(60));
        let input = tokio::select! {
            _ = &mut shutdown_signal => break,
            _ = tokio::time::sleep_until(deadline.into()) => Input::Event(Event::Tick),
            input = rx.recv() => match input { Some(input) => input, None => break },
        };
        runtime
            .handle_input(input, Instant::now(), Utc::now())
            .await?;
    }
    let _ = shutdown.send(true);
    runtime
        .adapters
        .take()
        .expect("adapters running at shutdown")
        .stop()
        .await;
    match tokio::time::timeout(Duration::from_secs(2), &mut server).await {
        Ok(result) => result??,
        Err(_) => {
            tracing::warn!("HTTP drain timed out; closing remaining streams");
            server.abort();
        }
    }
    Ok(())
}
async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
}
fn disconnected_events(mqtt: bool, connected: bool, config: &Config) -> Vec<Event> {
    if connected {
        return vec![];
    }
    if mqtt {
        return vec![Event::BridgeOffline];
    }
    let mut entities: BTreeSet<_> = [
        &config.tv.entity,
        &config.receiver.entity,
        &config.sonos.entity,
    ]
    .into_iter()
    .cloned()
    .collect();
    entities.extend(config.device.iter().filter_map(|d| d.media_player.clone()));
    entities
        .into_iter()
        .map(|entity| Event::HaState {
            entity,
            state: "unknown".into(),
            attrs: Default::default(),
        })
        .collect()
}
fn push_ring<T>(items: &mut VecDeque<T>, item: T, max: usize) {
    if items.len() == max {
        items.pop_front();
    }
    items.push_back(item);
}
fn validate_command(command: &Command, engine: &Engine) -> Result<(), CommandError> {
    let error = match command {
        Command::Identify(la) if *la >= 15 => Some("identify address must be 0–14"),
        Command::SendRaw { to, data } if *to > 15 || data.is_empty() || data.len() > 15 => {
            Some("invalid CEC destination or payload length")
        }
        Command::Takeover(role) | Command::Standby(role) if !engine.roles.contains_key(role) => {
            Some("unknown role")
        }
        Command::Takeover(role) | Command::Standby(role)
            if engine.identity.blocked.contains(role) =>
        {
            Some("role has an identity conflict")
        }
        Command::Takeover(_) | Command::Standby(_) | Command::AllOff if engine.warming => {
            Some("bridge is warming")
        }
        _ => None,
    };
    error.map_or(Ok(()), |error| Err(CommandError::Invalid(error.into())))
}
fn validate_reload(config: &Config, current: &Config) -> Result<(), ConfigError> {
    for (name, changed) in [
        (
            "web/auth",
            toml::to_string(&config.web)? != toml::to_string(&current.web)?,
        ),
        (
            "mqtt",
            toml::to_string(&config.mqtt)? != toml::to_string(&current.mqtt)?,
        ),
        (
            "home_assistant",
            toml::to_string(&config.home_assistant)? != toml::to_string(&current.home_assistant)?,
        ),
        (
            "tv.wake",
            toml::to_string(&config.tv.wake)? != toml::to_string(&current.tv.wake)?,
        ),
    ] {
        if changed {
            return Err(ConfigError::Validation(format!(
                "{name} changes require a restart; edit the mounted config while stopped"
            )));
        }
    }
    Ok(())
}
async fn reload(text: &str, current: &Config, path: &Path) -> Result<Config, ConfigError> {
    let mut config = Config::parse(text)?;
    validate_reload(&config, current)?;
    config.secrets = current.secrets.clone();
    persist_text(text, path).await?;
    Ok(config)
}
#[cfg(test)]
pub async fn persist(config: &Config, path: &Path) -> Result<(), ConfigError> {
    persist_text(&config.redacted_text()?, path).await
}
async fn persist_mode(mode: Mode, path: &Path) -> Result<(), ConfigError> {
    #[derive(serde::Deserialize)]
    struct ModeSpan {
        mode: toml::Spanned<Mode>,
    }
    let mut text = tokio::fs::read_to_string(path).await?;
    Config::parse(&text)?;
    let span: ModeSpan = toml::from_str(&text).map_err(|_| ConfigError::Parse)?;
    text.replace_range(span.mode.span(), &format!("\"{mode}\""));
    persist_text(&text, path).await
}
async fn persist_text(text: &str, path: &Path) -> Result<(), ConfigError> {
    use tokio::io::AsyncWriteExt;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".choregos-{}.tmp", uuid::Uuid::new_v4()));
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .await?;
        file.write_all(text.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, path).await?;
        tokio::fs::File::open(parent).await?.sync_all().await?;
        Ok::<(), std::io::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result.map_err(ConfigError::from)
}
fn update_snapshot(snapshot: &mut Snapshot, engine: &Engine, now: Instant, wall: DateTime<Utc>) {
    snapshot.mode = engine.mode();
    snapshot.warming = engine.warming;
    snapshot.bridge_online = engine.bridge_online;
    snapshot.active_role = engine.active_role.clone();
    snapshot.conflicts = engine.identity.conflicts.clone();
    snapshot.connections.bridge_online = engine.bridge_online;
    snapshot.connections.mode = engine.mode();
    let timestamp = |seen: Instant| {
        wall - chrono::Duration::from_std(now.saturating_duration_since(seen)).unwrap_or_default()
    };
    snapshot.roles = engine
        .config
        .device
        .iter()
        .map(|device| {
            let la = engine.identity.roles.get(&device.role).copied();
            let entry = la.and_then(|la| engine.bus.entries.get(&la));
            RoleView {
                role: device.role.clone(),
                name: device.name.clone(),
                source: device.source.clone(),
                power: engine.roles[&device.role].power,
                logical_address: la,
                physical_address: entry.and_then(|e| e.physical).map(physical_address),
                vendor: entry.and_then(|e| e.vendor).map(|v| format!("{v:06X}")),
                osd: entry.and_then(|e| e.osd.clone()),
                last_event: engine.roles[&device.role].last_event.clone(),
                last_seen: entry.map(|e| timestamp(e.last_seen)),
                conflicted: engine.identity.blocked.contains(&device.role),
            }
        })
        .collect();
    snapshot.bus = engine
        .bus
        .entries
        .iter()
        .map(|(&la, entry)| BusEntryView {
            logical_address: la,
            physical_address: entry.physical.map(physical_address),
            device_type: entry.device_type,
            vendor: entry.vendor.map(|v| format!("{v:06X}")),
            osd: entry.osd.clone(),
            power: entry.power,
            last_seen: Some(timestamp(entry.last_seen)),
            identified: entry.identified,
            matched_role: engine
                .identity
                .roles
                .iter()
                .find(|(_, addr)| **addr == la)
                .map(|(r, _)| r.clone()),
        })
        .collect();
    let entity = |id: &str| {
        let value = engine.ha.get(id);
        HaEntityView {
            entity: id.into(),
            state: value
                .map(|v| v.state.clone())
                .unwrap_or_else(|| "unknown".into()),
            attributes: value.map(|v| v.attrs.clone()).unwrap_or_default(),
            last_changed: None,
        }
    };
    snapshot.ha = HaView {
        tv: entity(&engine.config.tv.entity),
        receiver: entity(&engine.config.receiver.entity),
        sonos: entity(&engine.config.sonos.entity),
        media_players: engine
            .config
            .device
            .iter()
            .filter_map(|d| d.media_player.as_deref())
            .map(entity)
            .collect(),
    };
}
fn frame_view(
    frame: &Frame,
    timestamp: DateTime<Utc>,
    direction: FrameDirection,
    engine: &Engine,
) -> FrameView {
    let name = |la| {
        engine
            .identity
            .roles
            .iter()
            .find(|(_, addr)| **addr == la)
            .map(|(r, _)| r.clone())
            .unwrap_or_else(|| {
                if la == 15 {
                    "broadcast".into()
                } else if la == engine.bus.bridge {
                    "bridge".into()
                } else {
                    format!("LA {la}")
                }
            })
    };
    FrameView {
        timestamp,
        direction,
        from: frame.from,
        to: frame.to,
        from_name: name(frame.from),
        to_name: name(frame.to),
        opcode: frame.opcode,
        opcode_name: frame.opcode.map(opcode::name).unwrap_or("Poll").into(),
        params: frame.params.clone(),
        data: frame.data(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn atomic_persistence_roundtrips_and_failed_edits_preserve_original() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("choregos.toml");
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        persist(&config, &path).await.unwrap();
        let original = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(reload("not valid = [", &config, &path).await.is_err());
        let mut changed_web = config.clone();
        changed_web.web.bind.set_port(9000);
        let error = reload(&changed_web.redacted_text().unwrap(), &config, &path)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("restart"));
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), original);
        config.mode = Mode::Active;
        persist(&config, &path).await.unwrap();
        assert_eq!(
            Config::parse(&tokio::fs::read_to_string(&path).await.unwrap())
                .unwrap()
                .mode,
            Mode::Active
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn rejects_unsafe_manual_commands_before_acknowledging() {
        let config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        let engine = Engine::new(config, Instant::now());
        for command in [
            Command::Identify(15),
            Command::SendRaw {
                to: 16,
                data: vec![0x36],
            },
            Command::SendRaw {
                to: 4,
                data: vec![],
            },
            Command::Takeover("missing".into()),
            Command::AllOff,
        ] {
            assert!(validate_command(&command, &engine).is_err());
        }
        assert!(validate_command(&Command::SetMode(Mode::Shadow), &engine).is_ok());
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    fn config() -> Config {
        Config::parse(include_str!("../../choregos.example.toml")).unwrap()
    }

    #[tokio::test]
    async fn secret_destination_edits_require_restart_before_any_write_or_resolution() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("choregos.toml");
        let current = config();
        persist(&current, &path).await.unwrap();
        let original = tokio::fs::read_to_string(&path).await.unwrap();
        for kind in 0..5 {
            let mut changed = current.clone();
            match kind {
                0 => changed.web.public_url = "https://other.example.com".into(),
                1 => changed.mqtt.host = "mqtt.other.example.com".into(),
                2 => changed.home_assistant.url = "https://ha.other.example.com".into(),
                3 => {
                    changed.tv.wake = crate::config::WakeConfig::Http {
                        url: "https://wake.other.example.com".into(),
                        header: "Authorization".into(),
                        token_env: "CHOREGOS_LOCAL_PASSWORD".into(),
                    }
                }
                _ => changed.mqtt.username = "other".into(),
            }
            let error = reload(&changed.redacted_text().unwrap(), &current, &path)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("restart"));
            assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), original);
        }
        let text = original.replace("off_delay_s = 3", "off_delay_s = 4");
        assert_eq!(
            reload(&text, &current, &path)
                .await
                .unwrap()
                .timing
                .off_delay_s,
            4
        );
    }

    #[tokio::test]
    async fn mode_persistence_preserves_comments_order_and_unrelated_manual_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("choregos.toml");
        let text = include_str!("../../choregos.example.toml")
            .replacen("mode = \"shadow\"", "mode = 'shadow'", 1)
            .replace("off_delay_s = 3", "off_delay_s = 9");
        tokio::fs::write(&path, &text).await.unwrap();
        persist_mode(Mode::Active, &path).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            text.replacen("'shadow'", "\"active\"", 1)
        );
        persist_mode(Mode::Shadow, &path).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            text.replacen("'shadow'", "\"shadow\"", 1)
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn connection_failures_become_model_events_and_invalidate_every_tracked_ha_state() {
        let config = config();
        let now = Instant::now();
        let wall = Utc::now();
        assert!(matches!(
            disconnected_events(true, false, &config).as_slice(),
            [Event::BridgeOffline]
        ));
        assert!(disconnected_events(true, true, &config).is_empty());
        let mut engine = Engine::new(config.clone(), now);
        engine.handle(
            Event::HaState {
                entity: config.tv.entity.clone(),
                state: "on".into(),
                attrs: Default::default(),
            },
            now,
            wall,
        );
        for event in disconnected_events(false, false, &config) {
            engine.handle(event, now, wall);
        }
        assert_eq!(engine.ha[&config.tv.entity].state, "unknown");
        assert_eq!(engine.ha[&config.receiver.entity].state, "unknown");
        assert_eq!(engine.ha[&config.sonos.entity].state, "unknown");
        assert!(
            config
                .device
                .iter()
                .filter_map(|d| d.media_player.as_ref())
                .all(|e| engine.ha[e].state == "unknown")
        );
    }

    #[test]
    fn bounded_rings_evict_oldest_without_reordering_survivors() {
        let mut ring = VecDeque::new();
        for value in 0..5 {
            push_ring(&mut ring, value, 3);
        }
        assert_eq!(ring, [2, 3, 4]);
    }

    #[tokio::test]
    async fn full_ordered_queue_cannot_delay_fast_cec_replies() {
        let mut config = config();
        config.mode = Mode::Active;
        let now = Instant::now();
        let engine = Engine::new(config, now);
        let (mqtt, mut eventloop) = mqtt::Mqtt::test_transport(true, 16);
        let (ha, _) = ha::Ha::fake();
        let executor = Arc::new(executor::Executor {
            mqtt,
            ha,
            active: Arc::new(AtomicBool::new(true)),
        });
        let (effects, _rx) = mpsc::channel(1);
        effects
            .try_send((engine.config.clone(), vec![Action::WakeTv]))
            .unwrap();
        let adapters = Adapters {
            executor,
            effects,
            worker: tokio::spawn(std::future::pending()),
            mqtt_task: tokio::spawn(std::future::pending()),
            ha_task: tokio::spawn(std::future::pending()),
        };
        let frames = adapters.dispatch(
            vec![
                Action::WakeTv,
                Action::CecSend {
                    to: 4,
                    data: vec![0x90, 0],
                },
            ],
            &engine,
        );
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data(), "9000");
        eventloop.clean();
        assert_eq!(eventloop.pending.len(), 1);
        adapters.worker.abort();
        adapters.mqtt_task.abort();
        adapters.ha_task.abort();
    }

    #[tokio::test]
    async fn adapter_shutdown_drains_ordered_work_before_stopping_transports() {
        let config = Arc::new(config());
        let (mqtt, _eventloop) = mqtt::Mqtt::test_transport(false, 16);
        let (ha, _) = ha::Ha::fake();
        let executor = Arc::new(executor::Executor {
            mqtt,
            ha,
            active: Arc::new(AtomicBool::new(false)),
        });
        let (effects, mut rx) = mpsc::channel(4);
        let (observed, mut received) = mpsc::unbounded_channel();
        let worker = tokio::spawn(async move {
            while let Some((_, actions)) = rx.recv().await {
                let _ = observed.send(actions);
            }
        });
        let adapters = Adapters {
            executor,
            effects,
            worker,
            mqtt_task: tokio::spawn(std::future::pending()),
            ha_task: tokio::spawn(std::future::pending()),
        };
        for _ in 0..3 {
            adapters
                .effects
                .try_send((config.clone(), vec![Action::WakeTv]))
                .unwrap();
        }
        adapters.stop().await;
        assert_eq!(received.try_recv().unwrap(), vec![Action::WakeTv]);
        assert_eq!(received.try_recv().unwrap(), vec![Action::WakeTv]);
        assert_eq!(received.try_recv().unwrap(), vec![Action::WakeTv]);
    }
}
