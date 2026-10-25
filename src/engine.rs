use crate::{
    action::{Action, Decision},
    cec::{bus::BusModel, frame::Frame, opcode::*},
    config::{Config, PowerEvent, Presence, Takeover},
    identity::{self, Resolution},
    state::{Command, Mode, RolePower},
};
use chrono::{DateTime, Timelike, Utc};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub enum Event {
    Frame(Frame),
    BridgeOnline,
    BridgeOffline,
    HaState {
        entity: String,
        state: String,
        attrs: BTreeMap<String, Value>,
    },
    Tick,
    Command(Command),
    ConfigReloaded(Arc<Config>),
}
#[derive(Clone, Debug)]
pub struct RoleState {
    pub power: RolePower,
    pub last_event: Option<String>,
    poll_at: Option<Instant>,
    awaiting: bool,
    misses: u32,
}
impl Default for RoleState {
    fn default() -> Self {
        Self {
            power: RolePower::Off,
            last_event: None,
            poll_at: None,
            awaiting: false,
            misses: 0,
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct HaState {
    pub state: String,
    pub attrs: BTreeMap<String, Value>,
}

pub struct Engine {
    pub config: Arc<Config>,
    pub bus: BusModel,
    pub identity: Resolution,
    pub roles: BTreeMap<String, RoleState>,
    pub ha: BTreeMap<String, HaState>,
    pub active_role: Option<String>,
    pub bridge_online: bool,
    pub warming: bool,
    pub decisions: Vec<Decision>,
    warm_until: Instant,
    off_at: Option<Instant>,
    settle: Option<(Instant, String, bool)>,
    signals: BTreeMap<u8, Vec<PowerEvent>>,
}
impl Engine {
    pub fn new(config: Config, now: Instant) -> Self {
        let roles = config
            .device
            .iter()
            .map(|d| (d.role.clone(), RoleState::default()))
            .collect();
        Self {
            bus: BusModel::new(
                config.bridge.logical_address,
                config.timing.identify_retry_s,
            ),
            config: Arc::new(config),
            identity: Resolution::default(),
            roles,
            ha: BTreeMap::new(),
            active_role: None,
            bridge_online: false,
            warming: true,
            decisions: vec![],
            warm_until: now + Duration::from_secs(3),
            off_at: None,
            settle: None,
            signals: BTreeMap::new(),
        }
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.warming {
            return Some(self.warm_until);
        }
        self.roles
            .iter()
            .filter(|(role, _)| !self.identity.blocked.contains(*role))
            .filter_map(|(_, r)| r.poll_at)
            .chain(self.off_at)
            .chain(self.settle.as_ref().map(|s| s.0))
            .min()
    }
    pub fn handle(&mut self, event: Event, now: Instant, wall: DateTime<Utc>) -> Vec<Action> {
        self.decisions.clear();
        let mut actions = vec![];
        let was_warming = self.warming;
        if self.warming && now >= self.warm_until {
            self.warming = false;
        }
        let command_reason = match &event {
            Event::Command(command) => Some(match command {
                Command::AllOff => "command_all_off",
                Command::SetMode(_) => "command_set_mode",
                Command::Identify(_) => "command_identify",
                Command::SendRaw { .. } => "command_send_raw",
                Command::Takeover(_) => "command_takeover",
                Command::Standby(_) => "command_standby",
            }),
            _ => None,
        };
        match event {
            Event::BridgeOnline => {
                self.bridge_online = true;
                actions.extend(self.resync(now));
            }
            Event::BridgeOffline => {
                self.bridge_online = false;
            }
            Event::HaState {
                entity,
                state,
                attrs,
            } => {
                self.ha.insert(entity, HaState { state, attrs });
            }
            Event::ConfigReloaded(config) => {
                let bridge_changed = self.config.bridge.topic != config.bridge.topic
                    || self.config.bridge.logical_address != config.bridge.logical_address;
                self.config = config;
                self.roles
                    .retain(|role, _| self.config.device.iter().any(|d| &d.role == role));
                for device in &self.config.device {
                    let role = self.roles.entry(device.role.clone()).or_default();
                    role.poll_at = (role.power == RolePower::On
                        && device.presence == Presence::Poll)
                        .then_some(now + Duration::from_secs(self.config.timing.presence_poll_s));
                    role.awaiting = false;
                    role.misses = 0;
                }
                if bridge_changed {
                    self.bus = BusModel::new(
                        self.config.bridge.logical_address,
                        self.config.timing.identify_retry_s,
                    );
                    self.signals.clear();
                    self.active_role = None;
                    for role in self.roles.values_mut() {
                        *role = RoleState::default();
                    }
                } else {
                    self.bus
                        .set_identify_retry(self.config.timing.identify_retry_s);
                }
                self.identity = identity::resolve(&self.config.device, &self.bus);
                self.warming = true;
                self.warm_until = now + Duration::from_secs(3);
            }
            Event::Frame(frame) => {
                let previous_roles = self.identity.roles.clone();
                if frame.opcode == Some(REPORT_PHYSICAL_ADDRESS)
                    && self
                        .bus
                        .entries
                        .get(&frame.from)
                        .and_then(|e| e.physical)
                        .is_some_and(|pa| Some(pa) != frame.physical())
                {
                    self.signals.remove(&frame.from);
                }
                actions.extend(self.bus.handle(&frame, now));
                self.signals
                    .retain(|la, _| self.bus.entries.contains_key(la));
                if self.warming
                    && frame.from != self.bus.bridge
                    && matches!(
                        frame.opcode,
                        Some(
                            REPORT_POWER_STATUS
                                | REPORT_PHYSICAL_ADDRESS
                                | ACTIVE_SOURCE
                                | DEVICE_VENDOR_ID
                                | SET_OSD_NAME
                        )
                    )
                {
                    self.warming = false;
                }
                let resolution = identity::resolve(&self.config.device, &self.bus);
                let conflicts_changed = self.identity.conflicts != resolution.conflicts;
                self.identity = resolution;
                if conflicts_changed {
                    self.record(
                        None,
                        None,
                        None,
                        if self.identity.conflicts.is_empty() {
                            "conflict_resolved"
                        } else {
                            "conflict"
                        },
                        vec![],
                    );
                }
                if self
                    .settle
                    .as_ref()
                    .is_some_and(|(_, role, _)| self.identity.blocked.contains(role))
                {
                    self.settle = None;
                }
                for role in previous_roles.keys() {
                    if !self.identity.roles.contains_key(role)
                        && !self.identity.blocked.contains(role)
                    {
                        actions.extend(self.transition(role, RolePower::Off, "identity_lost", now));
                    }
                }
                let signal = self.signal(&frame);
                if let Some(signal) = signal {
                    let signals = self.signals.entry(frame.from).or_default();
                    signals.retain(|s| *s != signal);
                    signals.push(signal);
                }
                let mappings: Vec<_> = self
                    .identity
                    .roles
                    .iter()
                    .map(|(r, &la)| (r.clone(), la))
                    .collect();
                for (role, la) in mappings {
                    if frame.from == la {
                        let state = self.roles.get_mut(&role).expect("configured role");
                        state.awaiting = false;
                        state.misses = 0;
                    }
                    let device = self
                        .config
                        .device
                        .iter()
                        .find(|d| d.role == role)
                        .expect("configured role");
                    let signal = if previous_roles.get(&role) != Some(&la) {
                        self.signals
                            .get(&la)
                            .and_then(|s| {
                                s.iter()
                                    .rev()
                                    .find(|s| device.on.contains(s) || device.off.contains(s))
                            })
                            .copied()
                    } else if la == frame.from {
                        signal
                    } else {
                        None
                    };
                    if let Some(signal) = signal {
                        let target = if device.on.contains(&signal) {
                            Some(RolePower::On)
                        } else if device.off.contains(&signal) {
                            Some(RolePower::Off)
                        } else {
                            None
                        };
                        if let Some(target) = target {
                            actions.extend(self.transition(
                                &role,
                                target,
                                &format!("{signal:?}"),
                                now,
                            ));
                        }
                    }
                }
                actions.extend(self.fake_tv(&frame));
                if frame.opcode == Some(USER_CONTROL_PRESSED)
                    && [self.bus.bridge, 5].contains(&frame.to)
                    && self.is_state(&self.config.sonos.entity, "playing")
                    && let Some((role, _)) = self
                        .identity
                        .roles
                        .iter()
                        .find(|(_, la)| **la == frame.from)
                    && self.config.sonos.volume_from.contains(role)
                {
                    let action = match frame.params.first() {
                        Some(0x41) => Some(Action::ha(
                            "media_player",
                            "volume_up",
                            &self.config.sonos.entity,
                        )),
                        Some(0x42) => Some(Action::ha(
                            "media_player",
                            "volume_down",
                            &self.config.sonos.entity,
                        )),
                        Some(0x43) => Some(Action::ha(
                            "switch",
                            "toggle",
                            &self.config.sonos.mute_switch,
                        )),
                        _ => None,
                    };
                    if let Some(action) = action {
                        self.record(
                            Some(role.clone()),
                            None,
                            None,
                            "sonos_volume",
                            vec![action.clone()],
                        );
                        actions.push(action);
                    }
                }
            }
            Event::Command(command) => match command {
                Command::SetMode(mode) => {
                    Arc::make_mut(&mut self.config).mode = mode;
                    self.record(None, None, None, "command_set_mode", vec![]);
                }
                Command::Identify(la) => {
                    if la < 15 {
                        actions.push(Action::Identify { la });
                    }
                }
                Command::SendRaw { to, data } => {
                    if to <= 15 && !data.is_empty() && data.len() <= 15 {
                        actions.push(Action::CecSend { to, data });
                    }
                }
                Command::Takeover(role) => {
                    actions.extend(self.transition(&role, RolePower::On, "manual_takeover", now));
                }
                Command::Standby(role) => {
                    if !self.warming && !self.identity.blocked.contains(&role) {
                        actions.extend(self.stop_role(&role));
                        actions.extend(self.transition(
                            &role,
                            RolePower::Off,
                            "manual_standby",
                            now,
                        ));
                    }
                }
                Command::AllOff => {
                    if !self.warming {
                        for role in self.roles.keys().cloned().collect::<Vec<_>>() {
                            actions.extend(self.stop_role(&role));
                            actions.extend(self.transition(&role, RolePower::Off, "all_off", now));
                        }
                        self.off_at =
                            Some(now + Duration::from_secs(self.config.timing.off_delay_s));
                    }
                }
            },
            Event::Tick => {}
        }
        if was_warming && !self.warming {
            let pending: Vec<_> = self
                .identity
                .roles
                .iter()
                .filter_map(|(role, la)| {
                    let device = self.config.device.iter().find(|d| &d.role == role)?;
                    let signal = self
                        .signals
                        .get(la)?
                        .iter()
                        .rev()
                        .find(|s| device.on.contains(s) || device.off.contains(s))?;
                    Some((
                        role.clone(),
                        if device.on.contains(signal) {
                            RolePower::On
                        } else {
                            RolePower::Off
                        },
                    ))
                })
                .collect();
            for (role, power) in pending {
                actions.extend(self.transition(&role, power, "resync_complete", now));
            }
        }
        if let Some(reason) = command_reason
            && !actions.is_empty()
            && self.decisions.is_empty()
        {
            self.record(None, None, None, reason, actions.clone());
        }
        actions.extend(self.timers(now, wall));
        actions
    }
    fn resync(&mut self, now: Instant) -> Vec<Action> {
        self.warming = true;
        self.warm_until = now + Duration::from_secs(3);
        for role in self.roles.values_mut() {
            role.awaiting = false;
            role.misses = 0;
        }
        vec![Action::Resync]
    }
    fn signal(&self, frame: &Frame) -> Option<PowerEvent> {
        match frame.opcode {
            Some(IMAGE_VIEW_ON) if frame.to == self.bus.bridge => Some(PowerEvent::ImageViewOn),
            Some(TEXT_VIEW_ON) if frame.to == self.bus.bridge => Some(PowerEvent::TextViewOn),
            Some(ACTIVE_SOURCE) if frame.to == 15 && frame.params.len() == 2 => {
                Some(PowerEvent::ActiveSource)
            }
            Some(REPORT_PHYSICAL_ADDRESS) if frame.params.len() >= 3 => {
                Some(PowerEvent::ReportPhysicalAddress)
            }
            Some(STANDBY)
                if frame.from != self.bus.bridge && [self.bus.bridge, 15].contains(&frame.to) =>
            {
                Some(PowerEvent::Standby)
            }
            Some(INACTIVE_SOURCE) if frame.to == self.bus.bridge && frame.params.len() == 2 => {
                Some(PowerEvent::InactiveSource)
            }
            Some(REPORT_POWER_STATUS) => match frame.params.first() {
                Some(0) => Some(PowerEvent::PowerOn),
                Some(1) => Some(PowerEvent::PowerStandby),
                _ => None,
            },
            _ => None,
        }
    }
    fn transition(
        &mut self,
        role: &str,
        target: RolePower,
        reason: &str,
        now: Instant,
    ) -> Vec<Action> {
        if self.warming || self.identity.blocked.contains(role) || !self.roles.contains_key(role) {
            return vec![];
        }
        let state = self.roles.get_mut(role).expect("configured role");
        if state.power == target {
            return vec![];
        }
        let previous = state.power;
        state.power = target;
        state.last_event = Some(reason.into());
        state.misses = 0;
        state.awaiting = false;
        let poll = self
            .config
            .device
            .iter()
            .any(|d| d.role == role && d.presence == Presence::Poll);
        state.poll_at = (target == RolePower::On && poll)
            .then_some(now + Duration::from_secs(self.config.timing.presence_poll_s));
        let mut actions = vec![];
        if target == RolePower::On {
            self.off_at = None;
            if !self.is_state(&self.config.tv.entity, "on") {
                actions.push(Action::WakeTv);
            }
            for other in self
                .roles
                .keys()
                .filter(|r| r.as_str() != role)
                .cloned()
                .collect::<Vec<_>>()
            {
                if self.roles[&other].power == RolePower::On {
                    let stop = self.stop_role(&other);
                    if !stop.is_empty() {
                        let state = self.roles.get_mut(&other).expect("configured role");
                        state.power = RolePower::Off;
                        state.last_event = Some("taken_over".into());
                        state.poll_at = None;
                        state.awaiting = false;
                        state.misses = 0;
                        if let Some(la) = self.identity.roles.get(&other) {
                            self.signals.remove(la);
                        }
                        self.record(
                            Some(other.clone()),
                            Some(RolePower::On),
                            Some(RolePower::Off),
                            "taken_over",
                            vec![],
                        );
                        actions.extend(stop);
                    }
                }
            }
            if self.is_state(&self.config.sonos.entity, "playing") {
                actions.push(Action::ha(
                    "media_player",
                    "media_stop",
                    &self.config.sonos.entity,
                ));
            }
            let receiver_was_off = !self.is_state(&self.config.receiver.entity, "on");
            if receiver_was_off {
                actions.push(Action::ha(
                    "media_player",
                    "turn_on",
                    &self.config.receiver.entity,
                ));
            }
            let preset = receiver_was_off || self.active_role.as_deref() != Some(role);
            self.active_role = Some(role.into());
            self.settle = Some((
                now + Duration::from_millis(self.config.timing.takeover_settle_ms),
                role.into(),
                preset,
            ));
        } else {
            if self.active_role.as_deref() == Some(role) {
                self.active_role = None;
                self.settle = None;
            }
            self.off_at = Some(now + Duration::from_secs(self.config.timing.off_delay_s));
        }
        self.record(
            Some(role.into()),
            Some(previous),
            Some(target),
            reason,
            actions.clone(),
        );
        actions
    }
    fn stop_role(&self, role: &str) -> Vec<Action> {
        if self.identity.blocked.contains(role) {
            return vec![];
        }
        let Some(device) = self.config.device.iter().find(|d| d.role == role) else {
            return vec![];
        };
        match device.takeover {
            Takeover::CecStandby => self
                .identity
                .roles
                .get(role)
                .map(|&to| {
                    vec![Action::CecSend {
                        to,
                        data: vec![STANDBY],
                    }]
                })
                .unwrap_or_default(),
            Takeover::HaMediaStop => device
                .media_player
                .as_ref()
                .map(|entity| vec![Action::ha("media_player", "media_stop", entity)])
                .unwrap_or_default(),
            Takeover::None => vec![],
        }
    }
    fn timers(&mut self, now: Instant, wall: DateTime<Utc>) -> Vec<Action> {
        let mut actions = vec![];
        if self.warming {
            return actions;
        }
        if self.off_at.is_some_and(|t| t <= now) {
            self.off_at = None;
            if self.roles.iter().all(|(role, state)| {
                self.identity.blocked.contains(role) || state.power == RolePower::Off
            }) {
                let off = vec![
                    Action::ha("media_player", "turn_off", &self.config.tv.entity),
                    Action::ha("media_player", "turn_off", &self.config.receiver.entity),
                ];
                for entity in [&self.config.tv.entity, &self.config.receiver.entity] {
                    self.ha.entry(entity.clone()).or_default().state = "off".into();
                }
                self.record(None, None, None, "off_timer_expired", off.clone());
                actions.extend(off);
            }
        }
        if self.settle.as_ref().is_some_and(|s| s.0 <= now) {
            let (_, role, preset) = self.settle.take().expect("due settle");
            if self
                .roles
                .get(&role)
                .is_some_and(|r| r.power == RolePower::On)
            {
                let device = self
                    .config
                    .device
                    .iter()
                    .find(|d| d.role == role)
                    .expect("configured role");
                let mut settled = vec![];
                if self.config.receiver.select_source
                    && self
                        .ha
                        .get(&self.config.receiver.entity)
                        .and_then(|s| s.attrs.get("source"))
                        .and_then(Value::as_str)
                        != Some(&device.source)
                {
                    settled.push(Action::HaCall {
                        domain: "media_player".into(),
                        service: "select_source".into(),
                        entity: self.config.receiver.entity.clone(),
                        data: json!({"source": device.source}),
                    });
                }
                if preset && let Some(script) = self.preset(wall) {
                    settled.push(Action::ha("script", "turn_on", script));
                }
                self.record(Some(role), None, None, "takeover_settled", settled.clone());
                actions.extend(settled);
            }
        }
        for role in self.roles.keys().cloned().collect::<Vec<_>>() {
            if self.identity.blocked.contains(&role) {
                continue;
            }
            if !self.roles[&role].poll_at.is_some_and(|t| t <= now) {
                continue;
            }
            let Some(&la) = self.identity.roles.get(&role) else {
                self.roles.get_mut(&role).expect("role").poll_at = None;
                continue;
            };
            let state = self.roles.get_mut(&role).expect("role");
            if !self.bridge_online {
                state.poll_at = Some(now + Duration::from_secs(self.config.timing.presence_poll_s));
                continue;
            }
            if state.awaiting {
                state.misses += 1;
            }
            if state.misses >= self.config.timing.presence_misses {
                self.signals.remove(&la);
                actions.extend(self.transition(&role, RolePower::Off, "presence_timeout", now));
            } else {
                state.awaiting = true;
                state.poll_at = Some(now + Duration::from_secs(self.config.timing.presence_poll_s));
                actions.push(Action::PresencePoll { la });
            }
        }
        actions
    }
    fn preset(&self, wall: DateTime<Utc>) -> Option<&str> {
        let tz: chrono_tz::Tz = self.config.presets.timezone.parse().ok()?;
        let time = wall.with_timezone(&tz);
        let minute = time.hour() * 60 + time.minute();
        let parse = |s: &str| -> u32 {
            let (h, m) = s.split_once(':').expect("validated time");
            h.parse::<u32>().expect("hour") * 60 + m.parse::<u32>().expect("minute")
        };
        self.config
            .presets
            .slot
            .iter()
            .find(|s| minute >= parse(&s.from) && minute < parse(&s.to))
            .map(|s| s.script.as_str())
    }
    fn is_state(&self, entity: &str, state: &str) -> bool {
        self.ha.get(entity).is_some_and(|s| s.state == state)
    }
    fn fake_tv(&self, frame: &Frame) -> Vec<Action> {
        if frame.from == self.bus.bridge || frame.from >= 15 {
            return vec![];
        }
        let data = if self.config.fake_tv.replies && frame.to == self.bus.bridge {
            match frame.opcode {
                Some(GIVE_DEVICE_POWER_STATUS) => Some(vec![REPORT_POWER_STATUS, 0]),
                Some(GET_CEC_VERSION) => Some(vec![CEC_VERSION, 5]),
                Some(GIVE_OSD_NAME) => Some(
                    std::iter::once(SET_OSD_NAME)
                        .chain(self.config.fake_tv.osd_name.bytes())
                        .collect(),
                ),
                Some(GET_MENU_LANGUAGE) => Some(vec![SET_MENU_LANGUAGE, b'e', b'n', b'g']),
                _ => None,
            }
        } else {
            None
        };
        if let Some(data) = data {
            return vec![Action::CecSend {
                to: frame.from,
                data,
            }];
        }
        if frame.to == 15
            && frame.opcode == Some(REQUEST_ACTIVE_SOURCE)
            && let Some(pa) = self.bus.active_source
        {
            return vec![Action::CecSend {
                to: 15,
                data: vec![ACTIVE_SOURCE, (pa >> 8) as u8, pa as u8],
            }];
        }
        vec![]
    }
    fn record(
        &mut self,
        role: Option<String>,
        from: Option<RolePower>,
        to: Option<RolePower>,
        reason: &str,
        actions: Vec<Action>,
    ) {
        self.decisions.push(Decision {
            role,
            from,
            to,
            reason: reason.into(),
            actions,
        });
    }
    pub fn mode(&self) -> Mode {
        self.config.mode
    }
}
