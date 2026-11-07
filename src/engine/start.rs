//! Evidence-based boot ordering, independent of logical power and reporting caches.
use super::{
    Action, ConsoleState, DateTime, Decision, DecisionTrigger, DirectPower, Duration, Engine,
    Event, Frame, GIVE_DEVICE_POWER_STATUS, Instant, Mode, PLAY, PLAY_MODE_STILL, PowerEvent,
    RolePower, RoleState, RoomPower, STANDBY, StandbyVerification, StartEvidence, StartState,
    StartWake, Takeover, TimerKind, Utc, Value, Verdict, json,
};

const RESYNC_WINDOW: Duration = Duration::from_secs(5);
const LATE_REPEATS: u8 = 3;

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
                let evidence = state.start.as_ref()?;
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
        let sent_at = self
            .now
            .checked_sub((self.wall - at).to_std().unwrap_or_default())
            .unwrap_or(self.now);
        let Some(state) = self.roles.get_mut(role) else {
            return vec![];
        };
        state.direct = Some((sample, at));
        state.observe_probe_run(sample, sent_at);
        if self.config.mode == Mode::Shadow {
            state.start = None;
            state.start_baseline = None;
            state.start_first = None;
            return vec![];
        }
        let Some((policy, boot)) = self.probe_policy(role) else {
            return vec![];
        };
        self.clear_expired_rejection(role, boot);
        if self.roles[role]
            .start
            .as_ref()
            .is_some_and(|e| e.state.unconfirmed())
            && (self.warming || self.identity.blocked.contains(role))
        {
            return vec![];
        }
        if let Some(actions) = self.observe_start_end(role, sample, sent_at, policy, boot) {
            return actions;
        }
        let rest = self
            .config
            .device
            .iter()
            .find(|d| d.role == role)
            .and_then(|d| d.probe.as_ref())
            .map(|p| Duration::from_secs(p.rest_s()))
            .expect("probe policy");
        if self
            .roles
            .get_mut(role)
            .expect("configured role")
            .classify_start(sample, sent_at, rest)
        {
            return self.create_console_start(role, policy);
        }
        vec![]
    }

    fn observe_start_end(
        &mut self,
        role: &str,
        sample: ConsoleState,
        sent_at: Instant,
        policy: StartWake,
        boot: Duration,
    ) -> Option<Vec<Action>> {
        let evidence = self.roles[role]
            .start
            .as_ref()
            .filter(|e| e.state.unconfirmed())?;
        let reason = if self.now >= evidence.at + boot {
            "console_start_expired"
        } else if sample == ConsoleState::Standby {
            "console_start_retracted"
        } else {
            return None;
        };
        let expired = reason == "console_start_expired";
        let previous = expired.then(|| {
            self.trigger
                .replace(DecisionTrigger::Timer(TimerKind::ConsoleBoot))
        });
        let actions = self.end_console_start(role, reason, policy);
        if let Some(previous) = previous {
            self.trigger = previous;
        }
        let state = self.roles.get_mut(role).expect("configured role");
        state.start_baseline = state.standby_baseline(sent_at);
        state.start_first = None;
        Some(actions)
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
        let evidence = self.roles.get(role)?.start.as_ref()?;
        if self.now >= evidence.at + boot {
            return None;
        }
        evidence.state.decision()
    }

    fn older_than_active(&self, role: &str) -> bool {
        let Some(evidence) = self.roles[role].start.as_ref() else {
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
        self.roles.get_mut(role).expect("configured role").awake_at = Some(now);
        let Some((policy, boot)) = self.probe_policy(role) else {
            return self.transition(role, target, reason, now);
        };
        self.clear_expired_rejection(role, boot);
        let Some(evidence) = self.roles[role]
            .start
            .as_ref()
            .filter(|e| now < e.at + boot)
        else {
            return self.transition(role, target, reason, now);
        };
        if matches!(evidence.state, StartState::Rejected { .. }) {
            return self.repeat_late_announcement(role, policy, reason, now);
        }
        let decision = evidence.state.decision();
        self.roles
            .get_mut(role)
            .expect("configured role")
            .start
            .as_mut()
            .expect("start")
            .state = StartState::Confirmed { decision };
        if self.older_than_active(role) {
            let winner = self.intent.active_role.clone().expect("older active role");
            let actions = self.late_announcement(role, policy, &winner);
            if policy != StartWake::Would {
                return actions;
            }
        }
        self.transition(role, target, reason, now)
    }

    fn repeat_late_announcement(
        &mut self,
        role: &str,
        policy: StartWake,
        reason: &str,
        now: Instant,
    ) -> Vec<Action> {
        let Some(winner) = self.repeat_winner(role, policy) else {
            self.roles.get_mut(role).expect("configured role").start = None;
            return self.transition(role, RolePower::On, reason, now);
        };
        let evidence = self.roles[role].start.as_ref().expect("rejected episode");
        let settle = Duration::from_millis(self.config.timing.takeover_settle_ms);
        if evidence.late_repeats >= LATE_REPEATS
            || evidence.last_late.is_some_and(|last| now < last + settle)
        {
            return if policy == StartWake::Would {
                self.transition(role, RolePower::On, reason, now)
            } else {
                vec![]
            };
        }
        self.roles
            .get_mut(role)
            .expect("configured role")
            .start
            .as_mut()
            .expect("rejected episode")
            .late_repeats += 1;
        let actions = self.late_announcement(role, policy, &winner);
        if policy == StartWake::Would {
            self.transition(role, RolePower::On, reason, now)
        } else {
            actions
        }
    }

    fn repeat_winner(&self, role: &str, policy: StartWake) -> Option<String> {
        let active = self.intent.active_role.as_deref()?;
        if active != role {
            return Some(active.into());
        }
        if policy == StartWake::Would
            && let StartState::Rejected { winner, .. } = &self.roles[role].start.as_ref()?.state
        {
            return Some(winner.clone());
        }
        None
    }

    fn clear_expired_rejection(&mut self, role: &str, boot: Duration) {
        let state = self.roles.get_mut(role).expect("configured role");
        if state.start.as_ref().is_some_and(|evidence| {
            matches!(evidence.state, StartState::Rejected { .. }) && self.now >= evidence.at + boot
        }) {
            state.start = None;
            state.start_first = None;
        }
    }

    fn late_announcement(&mut self, role: &str, policy: StartWake, winner: &str) -> Vec<Action> {
        let evidence = self
            .roles
            .get_mut(role)
            .expect("configured role")
            .start
            .as_mut()
            .expect("start");
        evidence.last_late = Some(self.now);
        if !matches!(&evidence.state, StartState::Rejected { winner: previous, .. } if previous == winner)
        {
            evidence.state = StartState::Rejected {
                decision: evidence.state.decision(),
                winner: winner.into(),
            };
        }
        let mut actions = if policy == StartWake::Would {
            self.stop_actions(role)
        } else {
            self.stop_role(role, self.now)
        };
        self.reassert_source(winner, &mut actions);
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
        actions
    }

    fn reassert_source(&self, active: &str, actions: &mut Vec<Action>) {
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
                let evidence = state.start.as_ref()?;
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
                    .as_ref()
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
    fn unconfirmed(&self) -> bool {
        matches!(self, Self::Pending | Self::Woke { .. } | Self::Superseded)
    }

    fn decision(&self) -> Option<u64> {
        match self {
            Self::Woke { decision } => Some(*decision),
            Self::Confirmed { decision } | Self::Rejected { decision, .. } => *decision,
            _ => None,
        }
    }
}

impl RoleState {
    pub(super) fn unconfirmed_start_wake(&self) -> bool {
        self.start
            .as_ref()
            .is_some_and(|e| matches!(e.state, StartState::Woke { .. }))
    }

    fn observe_probe_run(&mut self, sample: ConsoleState, sent_at: Instant) {
        if self
            .probe_run
            .is_none_or(|(previous, _)| previous != sample)
        {
            self.probe_run = Some((sample, sent_at));
        }
    }

    pub(super) fn probe_standby_trusted(&self) -> bool {
        self.probe_run.is_some_and(|(sample, start)| {
            sample == ConsoleState::Standby && self.latest_awake().is_none_or(|awake| start > awake)
        })
    }

    fn latest_awake(&self) -> Option<Instant> {
        self.on_since.max(self.awake_at)
    }

    fn standby_baseline(&self, sent_at: Instant) -> Option<(Instant, Instant)> {
        self.probe_run
            .filter(|_| self.probe_standby_trusted())
            .map(|(_, start)| (sent_at, start))
    }

    fn classify_start(&mut self, sample: ConsoleState, sent_at: Instant, rest: Duration) -> bool {
        match sample {
            ConsoleState::Standby => {
                self.start_baseline = self.standby_baseline(sent_at);
                self.start_first = None;
                let settled = self
                    .start_baseline
                    .is_some_and(|(_, start)| sent_at.saturating_duration_since(start) >= rest);
                if settled
                    || !self
                        .start
                        .as_ref()
                        .is_some_and(|e| matches!(e.state, StartState::Rejected { .. }))
                {
                    self.start = None;
                }
            }
            ConsoleState::Awake => {
                self.start_baseline = None;
                self.start_first = None;
            }
            ConsoleState::Unreachable if self.start.is_none() => {
                return self.classify_unreachable(sent_at, rest);
            }
            ConsoleState::Unreachable => {}
        }
        false
    }

    fn classify_unreachable(&mut self, sent_at: Instant, rest: Duration) -> bool {
        let Some((last, start)) = self.start_baseline else {
            return false;
        };
        if self.latest_awake().is_some_and(|awake| start <= awake)
            || sent_at.saturating_duration_since(start) < rest
        {
            self.start_baseline = None;
            self.start_first = None;
            return false;
        }
        if let Some(first) = self.start_first.take() {
            self.start = Some(StartEvidence {
                at: first,
                last_standby: last,
                state: StartState::Pending,
                late_repeats: 0,
                last_late: None,
            });
            self.start_baseline = None;
            true
        } else {
            self.start_first = Some(sent_at);
            false
        }
    }
}
