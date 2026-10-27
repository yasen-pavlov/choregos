pub mod cli;
pub mod executor;
pub mod ha;
pub mod mqtt;
pub mod webos;
pub mod wol;

use crate::{
    action::Action,
    cec::{Frame, bus::LearnedIdentity, frame::physical_address, opcode},
    config::{Config, ConfigError},
    engine::{Engine, Event},
    state::{
        AppState, BusEntryView, Command, CommandError, DecisionView, FrameDirection, FrameView,
        HaEntityView, HaView, Mode, RoleView, Snapshot, UiEvent,
    },
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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

const IDENTITY_LIFETIME_DAYS: i64 = 30;

#[derive(Default, Deserialize, Serialize)]
struct PersistedState {
    version: u32,
    identities: Vec<LearnedIdentity>,
    decisions: Vec<DecisionView>,
}

impl PersistedState {
    async fn load(path: &Path, wall: DateTime<Utc>) -> Option<Self> {
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                tracing::warn!(error = %error, "cannot read persisted state; starting empty");
                return None;
            }
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(mut state) if state.version == 1 => {
                state.identities.retain(|identity| {
                    wall.signed_duration_since(identity.confirmed_at)
                        <= chrono::Duration::days(IDENTITY_LIFETIME_DAYS)
                });
                Some(state)
            }
            Ok(_) => {
                tracing::warn!("unknown persisted state version; starting empty");
                None
            }
            Err(error) => {
                tracing::warn!(error = %error, "corrupt persisted state; starting empty");
                None
            }
        }
    }

    fn identities_match(identities: &[LearnedIdentity], engine: &Engine) -> bool {
        let mut current = engine.bus.entries.iter().filter(|(la, entry)| {
            **la != engine.bus.bridge
                && entry.confirmed_at.is_some()
                && (entry.physical.is_some() || entry.vendor.is_some() || entry.osd.is_some())
        });
        identities.iter().all(|saved| {
            current.next().is_some_and(|(&la, entry)| {
                saved.la == la
                    && saved.physical == entry.physical
                    && saved.vendor == entry.vendor
                    && saved.osd == entry.osd
                    && saved.device_type == entry.device_type
                    && Some(saved.confirmed_at) == entry.confirmed_at
            })
        }) && current.next().is_none()
    }

    fn capture(engine: &Engine, decisions: &VecDeque<DecisionView>) -> Self {
        Self {
            version: 1,
            identities: engine
                .bus
                .entries
                .iter()
                .filter(|(la, entry)| {
                    **la != engine.bus.bridge
                        && entry.confirmed_at.is_some()
                        && (entry.physical.is_some()
                            || entry.vendor.is_some()
                            || entry.osd.is_some())
                })
                .map(|(&la, entry)| LearnedIdentity {
                    la,
                    physical: entry.physical,
                    vendor: entry.vendor,
                    osd: entry.osd.clone(),
                    device_type: entry.device_type,
                    confirmed_at: entry.confirmed_at.expect("filtered confirmation time"),
                })
                .collect(),
            decisions: decisions.iter().cloned().collect(),
        }
    }
}

const STATE_WRITE_RETRY: Duration = Duration::from_secs(60);

struct StateWriter {
    updates: watch::Sender<Option<Arc<Vec<u8>>>>,
    task: tokio::task::JoinHandle<()>,
}

impl StateWriter {
    fn start(path: PathBuf) -> Self {
        let (updates, mut receiver) = watch::channel::<Option<Arc<Vec<u8>>>>(None);
        let task = tokio::spawn(async move {
            let mut pending = None;
            let mut retry_at = None;
            let mut failed = false;
            let mut closing = false;
            loop {
                if pending.is_none() {
                    if receiver.changed().await.is_err() {
                        break;
                    }
                    pending = receiver.borrow_and_update().clone();
                }
                if let Some(deadline) = retry_at.take() {
                    tokio::select! {
                        result = receiver.changed(), if !closing => {
                            if result.is_ok() {
                                pending = receiver.borrow_and_update().clone();
                            } else {
                                // Shutdown: make the final attempt now rather than after the backoff.
                                closing = true;
                            }
                        }
                        _ = tokio::time::sleep_until(deadline) => {}
                    }
                }
                let Some(bytes) = pending.as_ref() else {
                    continue;
                };
                match persist_bytes(&path, bytes).await {
                    Ok(()) => {
                        if failed {
                            tracing::info!("state persistence recovered");
                        }
                        failed = false;
                        pending = None;
                        if closing {
                            break;
                        }
                    }
                    Err(error) => {
                        if !failed {
                            tracing::warn!(error = %error, "cannot persist state; retrying after 60 seconds or on new state");
                        }
                        failed = true;
                        if closing {
                            break;
                        }
                        retry_at = Some(tokio::time::Instant::now() + STATE_WRITE_RETRY);
                    }
                }
            }
        });
        Self { updates, task }
    }

    fn submit(&self, bytes: Vec<u8>) {
        self.updates.send_replace(Some(Arc::new(bytes)));
    }

    async fn flush(self) {
        drop(self.updates);
        if let Err(error) = self.task.await {
            tracing::warn!(error = %error, "state writer stopped unexpectedly");
        }
    }
}

struct Runtime {
    state: AppState,
    path: PathBuf,
    tx: mpsc::Sender<Input>,
    engine: Engine,
    adapters: Option<Adapters>,
    snapshot: Snapshot,
    last_states: String,
    removed_roles: BTreeSet<String>,
    writer: Option<StateWriter>,
    last_identities: Option<Vec<LearnedIdentity>>,
    decisions_dirty: bool,
}

impl Runtime {
    async fn new(
        state: AppState,
        path: PathBuf,
        tx: mpsc::Sender<Input>,
        config: Config,
        adapters: Adapters,
    ) -> Self {
        let now = Instant::now();
        let wall = Utc::now();
        let state_path = path.with_file_name("choregos.state.json");
        let restored = PersistedState::load(&state_path, wall)
            .await
            .unwrap_or_default();
        let engine = Engine::new(config, now, restored.identities);
        let mut snapshot = state.snapshot();
        snapshot.decisions = restored.decisions.into_iter().rev().take(2000).collect();
        snapshot.decisions.make_contiguous().reverse();
        push_ring(
            &mut snapshot.decisions,
            DecisionView {
                timestamp: wall,
                role: None,
                from: None,
                to: None,
                reason: "service_started".into(),
                actions: vec![],
                would: false,
            },
            2000,
        );
        let mut runtime = Self {
            snapshot,
            state,
            path,
            tx,
            engine,
            adapters: Some(adapters),
            last_states: String::new(),
            removed_roles: BTreeSet::new(),
            writer: Some(StateWriter::start(state_path)),
            last_identities: None,
            decisions_dirty: true,
        };
        runtime.publish_snapshot(now, wall, vec![]).await;
        runtime
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
        self.publish_snapshot(now, wall, ui_events).await;
        Ok(())
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
                        .map(|text| self.state.replace_config_text(text))
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
                self.state.replace_config_text(text);
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
            self.decisions_dirty = true;
            ui_events.push(UiEvent::Decision(view));
        }
    }

    async fn publish_snapshot(
        &mut self,
        now: Instant,
        wall: DateTime<Utc>,
        ui_events: Vec<UiEvent>,
    ) {
        update_snapshot(&mut self.snapshot, &self.engine, now, wall);
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
        if self.snapshot.connections.mqtt_connected && entity_states != self.last_states {
            self.adapters().executor.mqtt.states(&self.snapshot);
            self.last_states = entity_states;
        }
        if self.decisions_dirty
            || self
                .last_identities
                .as_ref()
                .is_none_or(|saved| !PersistedState::identities_match(saved, &self.engine))
        {
            let saved = PersistedState::capture(&self.engine, &self.snapshot.decisions);
            match serde_json::to_vec(&saved) {
                Ok(bytes) => {
                    self.writer
                        .as_ref()
                        .expect("state writer running")
                        .submit(bytes);
                    self.last_identities = Some(saved.identities);
                    self.decisions_dirty = false;
                }
                Err(error) => {
                    tracing::warn!(error = %error, "cannot encode persisted state; continuing")
                }
            }
        }
        self.state.publish(self.snapshot.clone(), ui_events);
    }

    async fn run_loop(
        &mut self,
        rx: &mut mpsc::Receiver<Input>,
        shutdown: impl Future<Output = ()>,
    ) -> anyhow::Result<()> {
        tokio::pin!(shutdown);
        loop {
            let deadline = self
                .engine
                .next_deadline()
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(60));
            let input = tokio::select! {
                _ = &mut shutdown => break,
                _ = tokio::time::sleep_until(deadline.into()) => Input::Event(Event::Tick),
                input = rx.recv() => match input { Some(input) => input, None => break },
            };
            self.handle_input(input, Instant::now(), Utc::now()).await?;
        }
        if let Some(writer) = self.writer.take() {
            writer.flush().await;
        }
        Ok(())
    }
}

pub async fn run(config: Config, config_text: String, path: PathBuf) -> anyhow::Result<()> {
    let state = AppState::new(config.clone(), config_text);
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
    let adapters = Adapters::start(&config, tx.clone(), &[]);
    // The retained bridge LWT initiates the single startup resync.
    let mut runtime = Runtime::new(state, path, tx, config, adapters).await;
    runtime.run_loop(&mut rx, shutdown_signal()).await?;
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
            changed: None,
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
        Command::Forget(la) if *la >= 15 => Some("forget address must be 0–14"),
        Command::Forget(la) if *la == engine.bus.bridge => Some("cannot forget the bridge"),
        Command::Forget(la) if !engine.bus.entries.contains_key(la) => {
            Some("unknown logical address")
        }
        Command::SendRaw { to, data } if *to > 15 || data.is_empty() || data.len() > 14 => {
            Some("invalid CEC destination or payload length")
        }
        Command::Identify(la) if *la == engine.bus.bridge => Some("cannot identify the bridge"),
        Command::Takeover(role) | Command::Standby(role) if !engine.roles.contains_key(role) => {
            Some("unknown role")
        }
        Command::Takeover(role) | Command::Standby(role)
            if engine.identity.blocked.contains(role) =>
        {
            Some("role has an identity conflict")
        }
        Command::Takeover(_) | Command::Standby(_) | Command::AllOff | Command::Forget(_)
            if engine.warming =>
        {
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
        (
            "tv.off",
            toml::to_string(&config.tv.off)? != toml::to_string(&current.tv.off)?,
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
    persist_bytes(path, text.as_bytes()).await?;
    Ok(config)
}
#[cfg(test)]
pub async fn persist(config: &Config, path: &Path) -> Result<(), ConfigError> {
    persist_bytes(path, config.redacted_text()?.as_bytes()).await
}
async fn persist_mode(mode: Mode, path: &Path) -> Result<String, ConfigError> {
    #[derive(serde::Deserialize)]
    struct ModeSpan {
        mode: toml::Spanned<Mode>,
    }
    let mut text = tokio::fs::read_to_string(path).await?;
    Config::parse(&text)?;
    let span: ModeSpan = toml::from_str(&text).map_err(|_| ConfigError::Parse)?;
    text.replace_range(span.mode.span(), &format!("\"{mode}\""));
    persist_bytes(path, text.as_bytes()).await?;
    Ok(text)
}
async fn persist_bytes(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
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
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, path).await?;
        if let Err(error) = async {
            tokio::fs::File::open(parent).await?.sync_all().await
        }
        .await
        {
            tracing::warn!(error = %error, "file renamed but directory sync failed; crash durability is not guaranteed");
        }
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
    snapshot.bridge_logical_address = engine.bus.bridge;
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
                last_seen: entry.and_then(|e| e.last_seen).map(timestamp),
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
            last_seen: entry.last_seen.map(timestamp),
            identified: entry.identified,
            verified: entry.verified,
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
            last_changed: value.and_then(|v| v.changed),
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
mod loop_tests;
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
        let engine = Engine::new(config, Instant::now(), vec![]);
        for command in [
            Command::Identify(15),
            Command::Identify(engine.bus.bridge),
            Command::SendRaw {
                to: 16,
                data: vec![0x36],
            },
            Command::SendRaw {
                to: 4,
                data: vec![],
            },
            Command::SendRaw {
                to: 4,
                data: vec![0; 15],
            },
            Command::Takeover("missing".into()),
            Command::AllOff,
        ] {
            assert!(validate_command(&command, &engine).is_err());
        }
        assert!(validate_command(&Command::SetMode(Mode::Shadow), &engine).is_ok());
        assert!(
            validate_command(
                &Command::SendRaw {
                    to: 4,
                    data: vec![0; 14],
                },
                &engine,
            )
            .is_ok()
        );
    }

    #[test]
    fn forget_rejects_invalid_bridge_and_unknown_addresses_but_allows_restored_entries() {
        let config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        let now = Instant::now();
        let mut engine = Engine::new(config, now, vec![]);
        for (la, message) in [
            (15, "forget address must be 0–14"),
            (u8::MAX, "forget address must be 0–14"),
            (engine.bus.bridge, "cannot forget the bridge"),
            (3, "unknown logical address"),
        ] {
            let error = validate_command(&Command::Forget(la), &engine).unwrap_err();
            assert_eq!(error.to_string(), format!("invalid command: {message}"));
        }
        engine.bus.restore(
            vec![LearnedIdentity {
                la: 3,
                physical: Some(0x2500),
                vendor: Some(0x123456),
                osd: Some("Disconnected adapter".into()),
                device_type: Some(3),
                confirmed_at: Utc::now(),
            }],
            now,
        );
        assert!(engine.warming);
        assert_eq!(engine.mode(), Mode::Shadow);
        assert_eq!(
            validate_command(&Command::Forget(3), &engine)
                .unwrap_err()
                .to_string(),
            "invalid command: bridge is warming"
        );
        engine.handle(Event::Tick, now + Duration::from_secs(3), Utc::now());
        assert!(!engine.warming);
        assert!(validate_command(&Command::Forget(3), &engine).is_ok());
    }

    #[test]
    fn forgetting_an_identity_invalidates_and_removes_it_from_the_next_saved_state() {
        let now = Instant::now();
        let wall: DateTime<Utc> = "2026-10-27T12:00:00Z".parse().unwrap();
        let config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        let identities = [(3, 0x2500), (4, 0x2100)]
            .into_iter()
            .map(|(la, physical)| LearnedIdentity {
                la,
                physical: Some(physical),
                vendor: Some(0x123456),
                osd: Some("CEC adapter".into()),
                device_type: Some(3),
                confirmed_at: wall,
            })
            .collect();
        let mut engine = Engine::new(config, now, identities);
        let decisions = VecDeque::new();
        let saved = PersistedState::capture(&engine, &decisions);
        assert_eq!(saved.identities.len(), 2);
        assert!(PersistedState::identities_match(&saved.identities, &engine));

        engine.handle(Event::Command(Command::Forget(3)), now, wall);
        assert!(!PersistedState::identities_match(
            &saved.identities,
            &engine
        ));
        let next = PersistedState::capture(&engine, &decisions);
        assert!(PersistedState::identities_match(&next.identities, &engine));
        assert_eq!(
            next.identities
                .iter()
                .map(|identity| identity.la)
                .collect::<Vec<_>>(),
            [4]
        );
    }

    #[test]
    fn snapshots_expose_the_configured_bridge_address_and_refresh_it_after_reload() {
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config.bridge.logical_address = 14;
        let state = AppState::new(config.clone(), String::new());
        let mut snapshot = state.snapshot();
        assert_eq!(snapshot.bridge_logical_address, 14);

        let now = Instant::now();
        let mut engine = Engine::new(config.clone(), now, vec![]);
        config.bridge.logical_address = 0;
        engine.handle(Event::ConfigReloaded(Arc::new(config)), now, Utc::now());
        update_snapshot(&mut snapshot, &engine, now, Utc::now());
        assert_eq!(snapshot.bridge_logical_address, 0);
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
        for kind in 0..6 {
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
                4 => {
                    changed.tv.off = crate::config::TvOffConfig::Webos {
                        host: "tv.other.example.com".into(),
                        port: 3001,
                        key_env: "CHOREGOS_LOCAL_PASSWORD".into(),
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
        let mut engine = Engine::new(config.clone(), now, vec![]);
        engine.handle(
            Event::HaState {
                entity: config.tv.entity.clone(),
                state: "on".into(),
                attrs: Default::default(),
                changed: None,
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
        let engine = Engine::new(config, now, vec![]);
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

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct Logs(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Logs {
        fn count(&self, message: &str) -> usize {
            String::from_utf8_lossy(&self.0.lock().unwrap())
                .matches(message)
                .count()
        }
    }

    async fn eventually(predicate: impl Fn() -> bool) {
        let started = Instant::now();
        while !predicate() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "writer did not converge"
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn writer_limits_failure_warnings_and_retries_at_sixty_seconds_or_new_state() {
        let logs = Logs(Arc::new(Mutex::new(vec![])));
        let output = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || output.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("choregos.state.json");
        std::fs::create_dir(&path).unwrap();
        let writer = StateWriter::start(path.clone());
        writer.submit(b"first".to_vec());
        eventually(|| logs.count("cannot persist state") == 1).await;
        std::fs::remove_dir(&path).unwrap();
        tokio::time::advance(Duration::from_secs(59)).await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert!(!path.exists(), "writer retried before 60 seconds");
        assert_eq!(logs.count("cannot persist state"), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        eventually(|| std::fs::read(&path).is_ok_and(|bytes| bytes == b"first")).await;
        eventually(|| logs.count("state persistence recovered") == 1).await;

        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        writer.submit(b"second".to_vec());
        eventually(|| logs.count("cannot persist state") == 2).await;
        std::fs::remove_dir(&path).unwrap();
        writer.submit(b"superseded".to_vec());
        writer.submit(b"newest".to_vec());
        eventually(|| std::fs::read(&path).is_ok_and(|bytes| bytes == b"newest")).await;
        writer.flush().await;
        assert_eq!(logs.count("cannot persist state"), 2);
        assert_eq!(logs.count("state persistence recovered"), 2);
    }

    #[tokio::test]
    async fn identity_expiry_keeps_exactly_thirty_days_but_drops_older_confirmations() {
        let wall: DateTime<Utc> = "2026-10-25T12:00:00Z".parse().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("choregos.state.json");
        let identities = [0, 1]
            .into_iter()
            .map(|seconds| LearnedIdentity {
                la: 4 + seconds as u8,
                physical: Some(0x2100 + seconds as u16 * 0x100),
                vendor: Some(0x0010fa),
                osd: Some("Streamer".into()),
                device_type: Some(4),
                confirmed_at: wall
                    - chrono::Duration::days(30)
                    - chrono::Duration::seconds(seconds),
            })
            .collect();
        let saved = PersistedState {
            version: 1,
            identities,
            decisions: vec![],
        };
        tokio::fs::write(&path, serde_json::to_vec(&saved).unwrap())
            .await
            .unwrap();
        let restored = PersistedState::load(&path, wall).await.unwrap();
        assert_eq!(restored.identities.len(), 1);
        assert_eq!(restored.identities[0].la, 4);
    }
}
