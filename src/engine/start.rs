//! Evidence-based boot ordering, independent of logical power and reporting caches.
use super::{
    Action, ConsoleState, DateTime, Decision, DecisionTrigger, DirectPower, Duration, Engine,
    Event, Frame, GIVE_DEVICE_POWER_STATUS, Instant, Mode, PLAY, PLAY_MODE_STILL, PowerEvent,
    RolePower, RoleState, RoomPower, STANDBY, StandbyVerification, StartEvidence, StartState,
    StartWake, Takeover, TimerKind, Utc, Value, Verdict, json,
};

const RESYNC_WINDOW: Duration = Duration::from_secs(5);

impl Engine {
    fn probe_policy(&self, role: &str) -> Option<(StartWake, Duration)> {
        let probe = self
            .config
            .device
            .iter()
            .find(|d| d.role == role)?
            .probe
            .as_ref()?;
        Some((probe.start_wake(), Duration::from_secs(probe.boot_s())))
    }

    pub(super) fn start_deadline(&self) -> Option<Instant> {
        self.roles
            .iter()
            .filter_map(|(role, state)| {
                let evidence = state.start?;
                if !evidence.state.unconfirmed() || self.identity.blocked.contains(role) {
                    return None;
                }
                let (_, boot) = self.probe_policy(role)?;
                let deadline = evidence.at + boot;
                Some(if self.warming {
                    deadline.max(self.warm_until)
                } else {
                    deadline
                })
            })
            .min()
    }

    pub(super) fn observe_console(
        &mut self,
        role: &str,
        sample: ConsoleState,
        at: DateTime<Utc>,
    ) -> Vec<Action> {
        let Some(state) = self.roles.get_mut(role) else {
            return vec![];
        };
        state.direct = Some((sample, at));
        if self.config.mode == Mode::Shadow {
            state.start = None;
            state.start_baseline = None;
            state.start_first = None;
            return vec![];
        }
        let Some((policy, boot)) = self.probe_policy(role) else {
            return vec![];
        };
        let sent_at = self
            .now
            .checked_sub((self.wall - at).to_std().unwrap_or_default())
            .unwrap_or(self.now);
        if self.roles[role]
            .start
            .is_some_and(|e| e.state.unconfirmed())
            && (self.warming || self.identity.blocked.contains(role))
        {
            return vec![];
        }
        if self.roles[role]
            .start
            .is_some_and(|e| e.state.unconfirmed() && self.now >= e.at + boot)
        {
            let trigger = self
                .trigger
                .replace(DecisionTrigger::Timer(TimerKind::ConsoleBoot));
            let actions = self.end_console_start(role, "console_start_expired", policy);
            self.trigger = trigger;
            // A deadline sample can establish the next baseline, not resurrect an old episode.
            let state = self.roles.get_mut(role).expect("configured role");
            state.start_baseline = (sample == ConsoleState::Standby).then_some(sent_at);
            state.start_first = None;
            return actions;
        }
        if sample == ConsoleState::Standby
            && self.roles[role]
                .start
                .is_some_and(|e| e.state.unconfirmed())
        {
            let actions = self.end_console_start(role, "console_start_retracted", policy);
            self.roles
                .get_mut(role)
                .expect("configured role")
                .start_baseline = Some(sent_at);
            return actions;
        }
        if self
            .roles
            .get_mut(role)
            .expect("configured role")
            .classify_start(sample, sent_at)
        {
            return self.create_console_start(role, policy);
        }
        vec![]
    }

    fn create_console_start(&mut self, role: &str, policy: StartWake) -> Vec<Action> {
        if policy == StartWake::Off {
            return vec![];
        }
        if !self.can_start_console(role) {
            self.roles
                .get_mut(role)
                .expect("configured role")
                .start
                .as_mut()
                .expect("new start")
                .state = StartState::Superseded;
            return vec![];
        }
        if policy == StartWake::Would {
            let actions = self.preview_wake(role);
            let id = self.record_would(role, "console_start", actions, None);
            self.roles
                .get_mut(role)
                .expect("configured role")
                .start
                .as_mut()
                .expect("new start")
                .state = StartState::Woke { decision: id };
            return vec![];
        }
        let actions = self.transition(role, RolePower::On, "console_start", self.now);
        let id = self.decisions.last().expect("eligible start transition").id;
        self.roles
            .get_mut(role)
            .expect("configured role")
            .start
            .as_mut()
            .expect("new start")
            .state = StartState::Woke { decision: id };
        actions
    }

    fn can_start_console(&self, role: &str) -> bool {
        !self.warming
            && !self.identity.blocked.contains(role)
            && self.roles[role].power == RolePower::Off
            && !self.older_than_active(role)
            && !self.preview_wake(role).is_empty()
    }

    fn preview_wake(&self, role: &str) -> Vec<Action> {
        let mut actions = vec![];
        if self.intent.power == RoomPower::Off || !self.power_is_on(&self.config.tv.entity) {
            actions.push(Action::WakeTv);
        }
        for (other, state) in &self.roles {
            if other != role && state.power == RolePower::On {
                actions.extend(self.stop_actions(other));
            }
        }
        if self.is_state(&self.config.sonos.entity, "playing") {
            actions.push(Action::ha(
                "media_player",
                "media_stop",
                &self.config.sonos.entity,
            ));
        }
        if self.intent.power == RoomPower::Off || !self.power_is_on(&self.config.receiver.entity) {
            actions.push(Action::ha(
                "media_player",
                "turn_on",
                &self.config.receiver.entity,
            ));
        }
        actions
    }

    pub(super) fn start_parent(&self, role: &str, target: RolePower) -> Option<u64> {
        let (policy, boot) = self.probe_policy(role)?;
        if target != RolePower::On || policy != StartWake::Would {
            return None;
        }
        let evidence = self.roles.get(role)?.start?;
        if self.now >= evidence.at + boot {
            return None;
        }
        evidence.state.decision()
    }

    fn older_than_active(&self, role: &str) -> bool {
        let Some(evidence) = self.roles[role].start else {
            return false;
        };
        self.intent
            .active_role
            .as_deref()
            .filter(|active| *active != role)
            .and_then(|active| self.roles.get(active)?.on_since)
            .is_some_and(|started| evidence.last_standby <= started)
    }

    pub(super) fn apply_role_signal(
        &mut self,
        role: &str,
        target: RolePower,
        reason: &str,
        now: Instant,
    ) -> Vec<Action> {
        if target != RolePower::On {
            return self.transition(role, target, reason, now);
        }
        let Some((policy, boot)) = self.probe_policy(role) else {
            return self.transition(role, target, reason, now);
        };
        let Some(evidence) = self.roles[role].start.filter(|e| now < e.at + boot) else {
            return self.transition(role, target, reason, now);
        };
        if matches!(evidence.state, StartState::Rejected) {
            return vec![];
        }
        self.roles
            .get_mut(role)
            .expect("configured role")
            .start
            .as_mut()
            .expect("start")
            .state = StartState::Confirmed {
            decision: evidence.state.decision(),
        };
        if self.older_than_active(role) {
            let actions = self.late_announcement(role, policy);
            if policy != StartWake::Would {
                return actions;
            }
        }
        self.transition(role, target, reason, now)
    }

    fn late_announcement(&mut self, role: &str, policy: StartWake) -> Vec<Action> {
        let mut actions = if policy == StartWake::Would {
            self.stop_actions(role)
        } else {
            self.stop_role(role, self.now)
        };
        self.reassert_active_source(&mut actions);
        if policy == StartWake::Would {
            self.record_would(
                role,
                "late_announcement",
                actions,
                self.start_parent(role, RolePower::On),
            );
            self.decisions.last_mut().expect("would reversal").from = Some(self.roles[role].power);
            return vec![];
        }
        let previous = self.mark_role_off(role, "late_announcement");
        let id = self.record(
            Some(role.into()),
            Some(previous),
            Some(RolePower::Off),
            "late_announcement",
            actions.clone(),
            None,
        );
        for pending in &mut self.standby_verifications {
            if pending.role == role && pending.parent.is_none() {
                pending.parent = Some(id);
            }
        }
        self.roles
            .get_mut(role)
            .expect("configured role")
            .start
            .as_mut()
            .expect("start")
            .state = StartState::Rejected;
        actions
    }

    fn reassert_active_source(&self, actions: &mut Vec<Action>) {
        let Some(active) = self.intent.active_role.as_deref() else {
            return;
        };
        if let Some(physical) = self
            .identity
            .roles
            .get(active)
            .and_then(|la| self.bus.entries.get(la))
            .filter(|entry| entry.verified)
            .and_then(|entry| entry.physical)
        {
            let [hi, lo] = physical.to_be_bytes();
            actions.push(Action::CecSend {
                to: 15,
                data: vec![crate::cec::opcode::SET_STREAM_PATH, hi, lo],
            });
        }
        if self.config.receiver.select_source
            && !self
                .settle
                .as_ref()
                .is_some_and(|(_, role, _, _)| role == active)
            && let Some(device) = self.config.device.iter().find(|d| d.role == active)
            && self
                .ha
                .get(&self.config.receiver.entity)
                .and_then(|state| state.attrs.get("source"))
                .and_then(Value::as_str)
                != Some(device.source.as_str())
        {
            actions.push(Action::HaCall {
                domain: "media_player".into(),
                service: "select_source".into(),
                entity: self.config.receiver.entity.clone(),
                data: json!({"source": device.source}),
            });
        }
    }

    pub(super) fn expire_console_starts(&mut self) -> Vec<Action> {
        if self.warming {
            return vec![];
        }
        let expired: Vec<_> = self
            .roles
            .iter()
            .filter_map(|(role, state)| {
                let evidence = state.start?;
                let (policy, boot) = self.probe_policy(role)?;
                (!self.identity.blocked.contains(role)
                    && evidence.state.unconfirmed()
                    && self.now >= evidence.at + boot)
                    .then(|| (role.clone(), policy))
            })
            .collect();
        let trigger = self.trigger.take();
        self.trigger = Some(DecisionTrigger::Timer(TimerKind::ConsoleBoot));
        let mut actions = vec![];
        for (role, policy) in expired {
            actions.extend(self.end_console_start(&role, "console_start_expired", policy));
        }
        self.trigger = trigger;
        actions
    }

    fn end_console_start(&mut self, role: &str, reason: &str, policy: StartWake) -> Vec<Action> {
        let state = self.roles.get_mut(role).expect("configured role");
        let evidence = state.start.take().expect("start");
        state.start_first = None;
        state.start_baseline = None;
        if policy == StartWake::Would && matches!(evidence.state, StartState::Woke { .. }) {
            self.record_would(role, reason, vec![], evidence.state.decision());
            vec![]
        } else if policy == StartWake::Act && matches!(evidence.state, StartState::Woke { .. }) {
            if self.roles[role].power == RolePower::Off {
                self.record(
                    Some(role.into()),
                    Some(RolePower::Off),
                    Some(RolePower::Off),
                    reason,
                    vec![],
                    None,
                );
                vec![]
            } else {
                self.transition(role, RolePower::Off, reason, self.now)
            }
        } else {
            vec![]
        }
    }

    pub(super) fn release_unconfirmed_starts(&mut self) {
        let roles: Vec<_> = self
            .roles
            .iter()
            .filter(|(_, state)| state.unconfirmed_start_wake())
            .map(|(role, _)| role.clone())
            .collect();
        let previous = self
            .trigger
            .replace(DecisionTrigger::Timer(TimerKind::ConsoleBoot));
        for role in roles {
            if self
                .probe_policy(&role)
                .is_some_and(|(policy, _)| policy == StartWake::Would)
            {
                let parent = self.roles[&role]
                    .start
                    .and_then(|evidence| evidence.state.decision());
                self.record_would(&role, "console_start_expired", vec![], parent);
            } else if self.roles[&role].power == RolePower::On {
                self.transition_role(&role, RolePower::Off, "console_start_expired", self.now);
            }
        }
        self.trigger = previous;
    }

    fn record_would(
        &mut self,
        role: &str,
        reason: &str,
        actions: Vec<Action>,
        parent: Option<u64>,
    ) -> u64 {
        let id = self.next_decision_id;
        self.next_decision_id = id.checked_add(1).expect("decision id exhausted");
        self.decisions.push(Decision {
            id,
            parent,
            trigger: self.trigger.as_ref().expect("decision trigger").recorded(),
            role: Some(role.into()),
            from: Some(RolePower::Off),
            to: Some(RolePower::Off),
            reason: reason.into(),
            actions,
            would: true,
            expectations: vec![],
            verdict: Verdict::None,
            drift: None,
        });
        id
    }

    pub(super) fn resync_deadline(&self) -> Option<Instant> {
        if self.startup_held.is_empty() {
            return None;
        }
        self.resync_at
            .map(|at| at + RESYNC_WINDOW)
            .filter(|deadline| *deadline > self.now)
    }

    pub(super) fn hold_startup_signal(
        &mut self,
        frame: &Frame,
        signal: Option<PowerEvent>,
    ) -> bool {
        let guarded = self.intent.power == RoomPower::Unknown
            && self
                .resync_at
                .is_some_and(|at| self.now < at + RESYNC_WINDOW)
            && matches!(
                signal,
                Some(
                    PowerEvent::ActiveSource
                        | PowerEvent::PowerOn
                        | PowerEvent::ReportPhysicalAddress
                )
            );
        if guarded && signal.is_some_and(|signal| self.startup_signal_relevant(frame.from, signal))
        {
            self.startup_held.insert(
                (frame.from, frame.opcode.expect("guarded opcode")),
                frame.clone(),
            );
        }
        guarded
    }

    fn startup_signal_relevant(&self, la: u8, signal: PowerEvent) -> bool {
        let role = self.identity.roles.iter().find(|(_, bound)| **bound == la);
        role.and_then(|(role, _)| self.config.device.iter().find(|d| d.role == *role))
            .is_none_or(|device| device.on.contains(&signal))
    }

    fn receiver_startup_power(&self) -> Option<bool> {
        if let Some((power, _)) = self.receiver_direct {
            match power {
                DirectPower::On => return Some(true),
                DirectPower::Off => return Some(false),
                DirectPower::Unreachable => {}
            }
        }
        match self.ha.get(&self.config.receiver.entity)?.state.as_str() {
            "on" | "playing" | "paused" | "idle" | "buffering" => Some(true),
            "off" | "standby" => Some(false),
            _ => None,
        }
    }

    pub(super) fn resolve_startup_signal(&mut self) -> Vec<Action> {
        if self.intent.power != RoomPower::Unknown {
            self.startup_held.clear();
            return vec![];
        }
        if self.startup_held.is_empty() {
            return vec![];
        }
        let power = self.receiver_startup_power();
        let expired = self
            .resync_at
            .is_some_and(|at| self.now >= at + RESYNC_WINDOW);
        if power.is_none() && !expired {
            return vec![];
        }
        let ready: Vec<_> = self
            .startup_held
            .keys()
            .copied()
            .filter(|(la, _)| expired || self.identity.roles.values().any(|bound| bound == la))
            .collect();
        let mut actions = vec![];
        for la in ready {
            let frame = self.startup_held.remove(&la).expect("held frame");
            actions.extend(self.resolve_startup_frame(frame, power));
        }
        actions
    }

    fn resolve_startup_frame(&mut self, frame: Frame, power: Option<bool>) -> Vec<Action> {
        let Some(role) = self
            .identity
            .roles
            .iter()
            .find(|(_, la)| **la == frame.from)
            .map(|(role, _)| role.clone())
        else {
            if power == Some(true) {
                if let Some(signal) = self.signal(&frame) {
                    let signals = self.signals.entry(frame.from).or_default();
                    signals.retain(|previous| *previous != signal);
                    signals.push(signal);
                }
            } else {
                self.discard_startup_frame(frame, None);
            }
            return vec![];
        };
        let signal = self.signal(&frame).expect("held role signal");
        let device = self
            .config
            .device
            .iter()
            .find(|d| d.role == role)
            .expect("configured role");
        if !device.on.contains(&signal) {
            return vec![];
        }
        let previous = self.trigger.take();
        self.trigger = DecisionTrigger::from_event(&Event::Frame(frame));
        let actions = if power == Some(true) {
            self.apply_role_signal(&role, RolePower::On, &format!("{signal:?}"), self.now)
        } else {
            self.record_startup_ignored(Some(role));
            vec![]
        };
        self.trigger = previous;
        actions
    }

    pub(super) fn discard_startup_holds(&mut self) {
        for (_, frame) in std::mem::take(&mut self.startup_held) {
            let role = self
                .identity
                .roles
                .iter()
                .find(|(_, la)| **la == frame.from)
                .map(|(role, _)| role.clone());
            self.discard_startup_frame(frame, role);
        }
    }

    fn discard_startup_frame(&mut self, frame: Frame, role: Option<String>) {
        let previous = self.trigger.take();
        self.trigger = DecisionTrigger::from_event(&Event::Frame(frame));
        self.record_startup_ignored(role);
        self.trigger = previous;
    }

    fn record_startup_ignored(&mut self, role: Option<String>) {
        if !self.startup_ignored {
            self.record(
                role,
                Some(RolePower::Off),
                Some(RolePower::Off),
                "startup_source_ignored",
                vec![],
                None,
            );
            self.startup_ignored = true;
        }
    }

    pub(super) fn stop_actions(&self, role: &str) -> Vec<Action> {
        if self.identity.blocked.contains(role) {
            return vec![];
        }
        let Some(device) = self.config.device.iter().find(|d| d.role == role) else {
            return vec![];
        };
        match device.takeover {
            Takeover::CecStandby | Takeover::CecPauseStandby => self
                .identity
                .roles
                .get(role)
                .map(|&la| StandbyVerification::actions(la, Self::stop_frames(device.takeover)))
                .unwrap_or_default(),
            Takeover::HaMediaStop => device
                .media_player
                .as_ref()
                .map(|entity| vec![Action::ha("media_player", "media_stop", entity)])
                .unwrap_or_default(),
            Takeover::None => vec![],
        }
    }

    pub(super) fn stop_frames(takeover: Takeover) -> &'static [&'static [u8]] {
        if takeover == Takeover::CecPauseStandby {
            &[
                &[PLAY, PLAY_MODE_STILL],
                &[STANDBY],
                &[GIVE_DEVICE_POWER_STATUS],
            ]
        } else {
            &[&[STANDBY], &[GIVE_DEVICE_POWER_STATUS]]
        }
    }
}

impl StartState {
    fn unconfirmed(self) -> bool {
        matches!(self, Self::Pending | Self::Woke { .. } | Self::Superseded)
    }

    fn decision(self) -> Option<u64> {
        match self {
            Self::Woke { decision } => Some(decision),
            Self::Confirmed { decision } => decision,
            _ => None,
        }
    }
}

impl RoleState {
    pub(super) fn unconfirmed_start_wake(&self) -> bool {
        self.start
            .is_some_and(|e| matches!(e.state, StartState::Woke { .. }))
    }

    fn classify_start(&mut self, sample: ConsoleState, sent_at: Instant) -> bool {
        match sample {
            ConsoleState::Standby => {
                self.start_baseline = Some(sent_at);
                self.start_first = None;
                self.start = None;
            }
            ConsoleState::Awake => {
                self.start_baseline = None;
                self.start_first = None;
            }
            ConsoleState::Unreachable if self.start.is_none() && self.start_baseline.is_some() => {
                if let Some(first) = self.start_first.take() {
                    self.start = Some(StartEvidence {
                        at: first,
                        last_standby: self.start_baseline.take().expect("standby baseline"),
                        state: StartState::Pending,
                    });
                    return true;
                }
                self.start_first = Some(sent_at);
            }
            ConsoleState::Unreachable => {}
        }
        false
    }
}
