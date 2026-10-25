//! Stable, serializable boundary between the core and web slices.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

pub use crate::config::ConfigError;
use crate::config::{Config, Secret};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Shadow,
    Active,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Shadow => "shadow",
            Self::Active => "active",
        })
    }
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    snapshot: std::sync::RwLock<Snapshot>,
    events: broadcast::Sender<UiEvent>,
    config_text: std::sync::RwLock<String>,
    runtime: std::sync::RwLock<Option<tokio::sync::mpsc::Sender<crate::runtime::Input>>>,
    auth: AuthConfig,
}

impl AppState {
    /// Build an initially disconnected, warming state without any network I/O.
    pub fn new(config: Config, config_text: String) -> Self {
        let connections = Health {
            mqtt_connected: false,
            ha_connected: false,
            bridge_online: false,
            mode: config.mode,
        };
        let snapshot = Snapshot {
            mode: config.mode,
            warming: true,
            bridge_online: false,
            roles: config
                .device
                .iter()
                .map(|device| RoleView {
                    role: device.role.clone(),
                    name: device.name.clone(),
                    source: device.source.clone(),
                    power: RolePower::Off,
                    logical_address: None,
                    physical_address: None,
                    vendor: None,
                    osd: None,
                    last_event: None,
                    last_seen: None,
                    conflicted: false,
                })
                .collect(),
            bus: Vec::new(),
            ha: HaView {
                tv: HaEntityView::unknown(&config.tv.entity),
                receiver: HaEntityView::unknown(&config.receiver.entity),
                sonos: HaEntityView::unknown(&config.sonos.entity),
                media_players: config
                    .device
                    .iter()
                    .filter_map(|device| device.media_player.as_deref())
                    .map(HaEntityView::unknown)
                    .collect(),
            },
            active_role: None,
            conflicts: Vec::new(),
            recent_frames: VecDeque::new(),
            decisions: VecDeque::new(),
            connections,
        };
        let auth = AuthConfig {
            public_url: config.web.public_url.clone(),
            session_secret: config.secrets.session_secret.clone(),
            oidc: OidcAuthConfig {
                issuer: config.web.oidc.issuer.clone(),
                client_id: config.web.oidc.client_id.clone(),
                client_secret: config.secrets.oidc_client_secret.clone(),
            },
            local_login: LocalAuthConfig {
                username: config.web.local_login.username.clone(),
                password: config.secrets.local_password.clone(),
            },
        };
        let (events, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                snapshot: std::sync::RwLock::new(snapshot),
                events,
                config_text: std::sync::RwLock::new(config_text),
                runtime: std::sync::RwLock::new(None),
                auth,
            }),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        self.inner.events.subscribe()
    }

    pub async fn command(&self, cmd: Command) -> Result<(), CommandError> {
        let sender = self
            .inner
            .runtime
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(CommandError::Unavailable)?;
        let (reply, result) = tokio::sync::oneshot::channel();
        sender
            .send(crate::runtime::Input::Request(
                crate::runtime::Request::Command(cmd, Some(reply)),
            ))
            .await
            .map_err(|_| CommandError::Unavailable)?;
        result.await.map_err(|_| CommandError::Unavailable)?
    }

    pub fn config_text(&self) -> String {
        self.inner
            .config_text
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub async fn save_config(&self, text: &str) -> Result<(), ConfigError> {
        let sender = self
            .inner
            .runtime
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or(ConfigError::Unavailable)?;
        let (reply, result) = tokio::sync::oneshot::channel();
        sender
            .send(crate::runtime::Input::Request(
                crate::runtime::Request::SaveConfig(text.into(), reply),
            ))
            .await
            .map_err(|_| ConfigError::Unavailable)?;
        result.await.map_err(|_| ConfigError::Unavailable)?
    }

    pub(crate) fn attach(&self, sender: tokio::sync::mpsc::Sender<crate::runtime::Input>) {
        *self
            .inner
            .runtime
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sender);
    }

    pub(crate) fn publish(&self, snapshot: Snapshot, events: impl IntoIterator<Item = UiEvent>) {
        *self
            .inner
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
        for event in events {
            let _ = self.inner.events.send(event);
        }
    }

    pub(crate) fn replace_config_text(&self, text: String) {
        *self
            .inner
            .config_text
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = text;
    }

    pub fn health(&self) -> Health {
        self.inner
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connections
            .clone()
    }

    pub fn auth(&self) -> &AuthConfig {
        &self.inner.auth
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub mode: Mode,
    pub warming: bool,
    pub bridge_online: bool,
    pub roles: Vec<RoleView>,
    pub bus: Vec<BusEntryView>,
    pub ha: HaView,
    pub active_role: Option<String>,
    pub conflicts: Vec<Conflict>,
    pub recent_frames: VecDeque<FrameView>,
    pub decisions: VecDeque<DecisionView>,
    pub connections: Health,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum UiEvent {
    Frame(FrameView),
    Decision(DecisionView),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Command {
    AllOff,
    SetMode(Mode),
    Identify(u8),
    SendRaw { to: u8, data: Vec<u8> },
    Takeover(String),
    Standby(String),
}

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("runtime is not available")]
    Unavailable,
    #[error("invalid command: {0}")]
    Invalid(String),
    #[error("configuration error: {0}")]
    Config(#[from] ConfigError),
}

#[derive(Clone, Debug, Serialize)]
pub struct Health {
    pub mqtt_connected: bool,
    pub ha_connected: bool,
    pub bridge_online: bool,
    pub mode: Mode,
}

/// Resolved authentication inputs. Never serialize these or display secrets.
#[derive(Clone, Debug)]
pub struct AuthConfig {
    pub public_url: String,
    pub session_secret: Secret,
    pub oidc: OidcAuthConfig,
    pub local_login: LocalAuthConfig,
}

#[derive(Clone, Debug)]
pub struct OidcAuthConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Secret,
}

#[derive(Clone, Debug)]
pub struct LocalAuthConfig {
    pub username: String,
    /// Hash in memory at web startup; never put the original in a session.
    pub password: Secret,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RolePower {
    Off,
    On,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BusPower {
    Unknown,
    On,
    Standby,
    Transition,
}

#[derive(Clone, Debug, Serialize)]
pub struct RoleView {
    pub role: String,
    pub name: String,
    pub source: String,
    pub power: RolePower,
    pub logical_address: Option<u8>,
    pub physical_address: Option<String>,
    pub vendor: Option<String>,
    pub osd: Option<String>,
    pub last_event: Option<String>,
    pub last_seen: Option<DateTime<Utc>>,
    pub conflicted: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct BusEntryView {
    pub logical_address: u8,
    pub physical_address: Option<String>,
    pub device_type: Option<u8>,
    pub vendor: Option<String>,
    pub osd: Option<String>,
    pub power: BusPower,
    pub last_seen: Option<DateTime<Utc>>,
    pub identified: bool,
    pub matched_role: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HaView {
    pub tv: HaEntityView,
    pub receiver: HaEntityView,
    pub sonos: HaEntityView,
    /// Includes configured Kodi/media-stop entities without installation-specific names.
    pub media_players: Vec<HaEntityView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HaEntityView {
    pub entity: String,
    pub state: String,
    pub attributes: BTreeMap<String, serde_json::Value>,
    pub last_changed: Option<DateTime<Utc>>,
}

impl HaEntityView {
    fn unknown(entity: &str) -> Self {
        Self {
            entity: entity.to_owned(),
            state: "unknown".to_owned(),
            attributes: BTreeMap::new(),
            last_changed: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FrameView {
    pub timestamp: DateTime<Utc>,
    pub direction: FrameDirection,
    pub from: u8,
    pub to: u8,
    pub from_name: String,
    pub to_name: String,
    pub opcode: Option<u8>,
    pub opcode_name: String,
    pub params: Vec<u8>,
    pub data: String,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FrameDirection {
    Received,
    Sent,
}

#[derive(Clone, Debug, Serialize)]
pub struct DecisionView {
    pub timestamp: DateTime<Utc>,
    pub role: Option<String>,
    pub from: Option<RolePower>,
    pub to: Option<RolePower>,
    pub reason: String,
    /// Full, already-redacted action descriptions; `would` marks suppressed effects.
    pub actions: Vec<String>,
    pub would: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Conflict {
    pub roles: Vec<String>,
    pub logical_addresses: Vec<u8>,
    pub reason: String,
}
