//! Reporting-only verification. Deadlines are injected monotonic time; evidence
//! is observed independently, never an optimistic command-side cache write.
use super::{DecisionTrigger, Engine, StandbyVerification, VerificationResult};
use crate::{
    action::{Action, Expectation, Field, Outcome, Verdict, VerdictUpdate, Via},
    cec::opcode::STANDBY,
    config::{Config, Takeover, TvOffConfig},
    state::{BusPower, Command, ConsoleState, Mode, RolePower, TvState},
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::time::{Duration, Instant};

pub(super) struct PendingDecision {
    pub at: Instant,
    pub wall: DateTime<Utc>,
    pub expectations: Vec<Expectation>,
    progress: Vec<ExpectationProgress>,
}

impl PendingDecision {
    pub fn new(at: Instant, wall: DateTime<Utc>, expectations: Vec<Expectation>) -> Self {
        let progress = (0..expectations.len())
            .map(|_| ExpectationProgress::default())
            .collect();
        Self {
            at,
            wall,
            expectations,
            progress,
        }
    }

    fn push(&mut self, expectation: Expectation) {
        self.expectations.push(expectation);
        self.progress.push(ExpectationProgress::default());
    }
    fn cancel(&mut self) {
        for (expectation, progress) in self.expectations.iter_mut().zip(&self.progress) {
            progress.cancel(expectation);
        }
    }

    pub fn deadlines(&self) -> impl Iterator<Item = Instant> + '_ {
        self.expectations
            .iter()
            .filter(|expectation| expectation.outcome == Outcome::Pending)
            .map(|expectation| self.at + Duration::from_millis(expectation.deadline_ms))
    }
}

#[derive(Clone, Copy)]
struct Observation<'a> {
    value: &'a str,
    latency_ms: u64,
}

#[derive(Default)]
struct ExpectationProgress {
    cec_disagreement_at: Option<u64>,
    cec_met: bool,
    absent: bool,
}

impl ExpectationProgress {
    fn cancel(&self, expectation: &mut Expectation) {
        if expectation.outcome == Outcome::Pending {
            expectation.outcome = if self.cec_met {
                Outcome::CecOnly {
                    observed: expectation.expect.clone(),
                }
            } else {
                Outcome::Unknown { observed: None }
            };
        }
    }
}

impl Engine {
    pub fn next_expectation_deadline(&self) -> Option<Instant> {
        self.pending_decisions
            .values()
            .flat_map(PendingDecision::deadlines)
            .min()
    }

    /// Read-only inspection of pending or newly settled expectations, used by replay tests.
    pub fn expectations(&self, id: u64) -> Option<&[Expectation]> {
        self.pending_decisions
            .get(&id)
            .map(|pending| pending.expectations.as_slice())
            .or_else(|| {
                self.verdicts
                    .iter()
                    .find(|update| update.id == id)
                    .map(|update| update.expectations.as_slice())
            })
    }

    /// Complete reporting deadlines without running any control, presence, or off timers.
    pub fn settle_expectations(&mut self, now: Instant) {
        self.decisions.clear();
        self.verdicts.clear();
        self.evaluate_expectations(now);
    }

    pub(super) fn standby_result(
        &mut self,
        verification: &StandbyVerification,
        result: VerificationResult,
    ) {
        let Some(pending) = verification
            .parent
            .and_then(|id| self.pending_decisions.get_mut(&id))
        else {
            return;
        };
        let Some(index) = pending.expectations.iter().position(|expectation| {
            expectation.subject.strip_prefix("role:") == Some(&verification.role)
        }) else {
            return;
        };
        if pending.expectations[index].outcome != Outcome::Pending {
            return;
        }
        let progress = &mut pending.progress[index];
        match result {
            VerificationResult::Cancelled => {
                progress.cancel(&mut pending.expectations[index]);
            }
            VerificationResult::Confirmed | VerificationResult::Exhausted => {
                let entry = self.bus.entries.get(&verification.la);
                let observed_at = entry.and_then(|entry| entry.power_at).filter(|at| {
                    *at > pending.at
                        && *at
                            <= pending.at
                                + Duration::from_millis(pending.expectations[index].deadline_ms)
                });
                if matches!(result, VerificationResult::Confirmed) {
                    progress.cec_met |= observed_at.is_some();
                } else {
                    progress.absent = verification.sent > 0
                        && !verification.acked
                        && verification.nacked >= verification.sent;
                    if let Some(at) = observed_at
                        && entry.is_some_and(|entry| entry.power == BusPower::On)
                    {
                        progress.cec_disagreement_at =
                            Some(at.duration_since(pending.at).as_millis() as u64);
                    }
                }
            }
        }
    }

    fn expectation(
        &self,
        subject: &str,
        field: Field,
        expect: &str,
        via: Vec<Via>,
        deadline_ms: u64,
    ) -> Expectation {
        let mut expectation = Expectation {
            subject: subject.into(),
            field,
            expect: expect.into(),
            via,
            deadline_ms,
            attempts: 0,
            outcome: Outcome::Pending,
        };
        self.settle_already_satisfied(&mut expectation);
        expectation
    }

    fn tv_expectation(&self, value: &str) -> Expectation {
        let via = if matches!(self.config.tv.off, TvOffConfig::Webos { .. }) {
            vec![Via::TvDirect, Via::Ha]
        } else {
            vec![Via::Ha]
        };
        self.expectation(
            "tv",
            Field::Power,
            value,
            via,
            self.config.verify.tv_s * 1000,
        )
    }

    fn role_expectation(&self, role: &str) -> Option<Expectation> {
        let device = self
            .config
            .device
            .iter()
            .find(|device| device.role == role)
            .expect("configured role");
        let cec = device.verify_standby
            && matches!(
                device.takeover,
                Takeover::CecStandby | Takeover::CecPauseStandby
            );
        let via = match (cec, device.probe.is_some()) {
            (false, false) => return None,
            (false, true) => vec![Via::Ps5],
            (true, true) => vec![Via::Ps5, Via::Cec],
            (true, false) => vec![Via::Cec],
        };
        let cec_deadline_ms = if cec {
            self.config.timing.takeover_settle_ms
                * (u64::from(self.config.timing.standby_retries) + 1)
        } else {
            0
        };
        let probe_deadline_ms = if device.probe.is_some() {
            self.config.verify.probe_window_s * 1000
        } else {
            0
        };
        Some(self.expectation(
            &format!("role:{role}"),
            Field::Power,
            "standby",
            via,
            cec_deadline_ms.max(probe_deadline_ms),
        ))
    }

    pub(super) fn decision_expectations(
        &self,
        role: Option<&str>,
        to: Option<RolePower>,
        reason: &str,
        actions: &[Action],
    ) -> Vec<Expectation> {
        if self.config.mode == Mode::Shadow {
            return vec![];
        }
        let mut expectations = vec![];
        let wake = to == Some(RolePower::On);
        if wake && actions.contains(&Action::WakeTv) {
            expectations.push(self.tv_expectation("on"));
        }
        if reason == "off_timer_expired" && actions.contains(&Action::TvOff) {
            expectations.push(self.tv_expectation("off"));
        }
        for action in actions {
            match action {
                Action::HaCall {
                    entity,
                    service,
                    data,
                    ..
                } if entity == &self.config.receiver.entity => {
                    let expectation = match service.as_str() {
                        "turn_on" if wake => Some(self.expectation(
                            "receiver",
                            Field::Power,
                            "on",
                            vec![Via::Ha],
                            self.config.verify.receiver_s * 1000,
                        )),
                        "turn_off" if reason == "off_timer_expired" => Some(self.expectation(
                            "receiver",
                            Field::Power,
                            "off",
                            vec![Via::Ha],
                            self.config.verify.receiver_off_s * 1000,
                        )),
                        "select_source" if reason == "takeover_settled" => {
                            data.get("source").and_then(Value::as_str).map(|source| {
                                self.expectation(
                                    "receiver",
                                    Field::Source,
                                    source,
                                    vec![Via::Ha],
                                    self.config.verify.source_s * 1000,
                                )
                            })
                        }
                        _ => None,
                    };
                    expectations.extend(expectation);
                }
                Action::HaCall {
                    entity, service, ..
                } if wake && service == "media_stop" && entity == &self.config.sonos.entity => {
                    expectations.push(self.expectation(
                        "speakers",
                        Field::Playback,
                        "stopped",
                        vec![Via::Ha],
                        self.config.verify.speakers_s * 1000,
                    ));
                }
                _ => {}
            }
        }
        if wake {
            for device in &self.config.device {
                if device.role == role.unwrap_or("") {
                    continue;
                }
                let stopped = actions.iter().any(|action| match action {
                    Action::CecSend { to, data } => {
                        data == &[STANDBY] && self.identity.roles.get(&device.role) == Some(to)
                    }
                    Action::HaCall {
                        entity, service, ..
                    } => service == "media_stop" && device.media_player.as_ref() == Some(entity),
                    _ => false,
                });
                if stopped {
                    expectations.extend(self.role_expectation(&device.role));
                }
            }
        } else if matches!(reason, "all_off" | "manual_standby")
            && let Some(role) = role
            && self.stopped_roles.iter().any(|stopped| stopped == role)
        {
            expectations.extend(self.role_expectation(role));
        }
        expectations
    }

    /// All-off can also dispatch to bound roles already Off. Attach every stop
    /// to a command decision, never to an unrelated warm-up transition.
    pub(super) fn attach_command_stops(&mut self, actions: &[Action]) {
        if self.config.mode == Mode::Shadow
            || !matches!(
                self.trigger,
                Some(DecisionTrigger::Command(
                    Command::AllOff | Command::Standby(_)
                ))
            )
        {
            return;
        }
        let expectations: Vec<_> = self
            .stopped_roles
            .iter()
            .filter(|role| {
                self.config
                    .device
                    .iter()
                    .find(|device| device.role == **role)
                    .is_some_and(|device| {
                        actions.iter().any(|action| match action {
                            Action::CecSend { to, data } => {
                                data == &[STANDBY] && self.identity.roles.get(*role) == Some(to)
                            }
                            Action::HaCall {
                                entity, service, ..
                            } => {
                                service == "media_stop"
                                    && device.media_player.as_ref() == Some(entity)
                            }
                            _ => false,
                        })
                    })
            })
            .filter(|role| {
                !self.decisions.iter().any(|decision| {
                    decision.expectations.iter().any(|expectation| {
                        expectation.subject.strip_prefix("role:") == Some(role.as_str())
                    })
                })
            })
            .filter_map(|role| {
                self.role_expectation(role)
                    .map(|expectation| (role.clone(), expectation))
            })
            .collect();
        if expectations.is_empty() {
            return;
        }
        let index = if let Some(index) = self.decisions.iter().position(|decision| {
            matches!(
                decision.reason.as_str(),
                "all_off" | "manual_standby" | "command_all_off" | "command_standby"
            )
        }) {
            index
        } else {
            let reason = match self.trigger {
                Some(DecisionTrigger::Command(Command::AllOff)) => "command_all_off",
                _ => "command_standby",
            };
            self.record(None, None, None, reason, actions.to_vec(), None);
            self.decisions.len() - 1
        };
        let id = self.decisions[index].id;
        for (role, expectation) in expectations {
            if !self.decisions[index]
                .expectations
                .iter()
                .any(|existing| existing.subject == expectation.subject)
            {
                self.decisions[index].expectations.push(expectation.clone());
                self.decisions[index].verdict = verdict(&self.decisions[index].expectations);
                if self.decisions[index].verdict == Verdict::Pending {
                    self.pending_decisions
                        .entry(id)
                        .and_modify(|pending| pending.push(expectation))
                        .or_insert_with(|| {
                            PendingDecision::new(
                                self.now,
                                self.wall,
                                self.decisions[index].expectations.clone(),
                            )
                        });
                }
            }
            for pending in &mut self.standby_verifications {
                if pending.role == role && pending.parent.is_none() {
                    pending.parent = Some(id);
                }
            }
        }
    }

    fn independent_sample<'a>(
        &'a self,
        subject: &str,
        field: Field,
        via: Via,
    ) -> Option<(&'a str, DateTime<Utc>)> {
        match via {
            Via::TvDirect => self.tv_direct.and_then(|(state, at)| match state {
                TvState::On => Some(("on", at)),
                TvState::Off => Some(("off", at)),
                TvState::Unreachable => None,
            }),
            Via::Ps5 => subject
                .strip_prefix("role:")
                .and_then(|role| self.roles.get(role))
                .and_then(|role| role.direct)
                .and_then(|(state, at)| match state {
                    ConsoleState::Awake => Some(("on", at)),
                    ConsoleState::Standby => Some(("standby", at)),
                    ConsoleState::Unreachable => None,
                }),
            Via::Ha => {
                let entity = match subject {
                    "tv" => Some(&self.config.tv.entity),
                    "receiver" => Some(&self.config.receiver.entity),
                    "speakers" => Some(&self.config.sonos.entity),
                    _ => None,
                };
                entity
                    .and_then(|entity| self.ha.get(entity))
                    .and_then(|state| {
                        if matches!(state.state.as_str(), "unknown" | "unavailable") {
                            return None;
                        }
                        let value = match field {
                            Field::Source => state.attrs.get("source")?.as_str()?,
                            Field::Power => match state.state.as_str() {
                                "on" | "playing" | "paused" | "idle" | "buffering" => "on",
                                "off" | "standby" => "off",
                                other => other,
                            },
                            Field::Playback
                                if matches!(
                                    state.state.as_str(),
                                    "idle" | "paused" | "off" | "stopped" | "standby"
                                ) =>
                            {
                                "stopped"
                            }
                            _ => &state.state,
                        };
                        Some((
                            value,
                            match field {
                                Field::Source => state.updated?,
                                Field::Power | Field::Playback => state.changed?,
                            },
                        ))
                    })
            }
            Via::Cec => None,
        }
    }

    /// Select by the sample's timestamp, not adapter preference or arrival time.
    /// The optional upper bound preserves verifier creation-time semantics.
    pub(super) fn newest_independent_sample<'a>(
        &'a self,
        subject: &str,
        field: Field,
        sources: &[Via],
        before: Option<DateTime<Utc>>,
    ) -> Option<(Via, &'a str, DateTime<Utc>)> {
        sources
            .iter()
            .filter_map(|&via| {
                self.independent_sample(subject, field, via)
                    .map(|(value, at)| (via, value, at))
            })
            .filter(|(_, _, at)| before.is_none_or(|before| *at < before))
            .max_by_key(|(_, _, at)| *at)
    }

    fn observation<'a>(
        &'a self,
        wall: DateTime<Utc>,
        expectation: &Expectation,
        via: Via,
    ) -> Option<Observation<'a>> {
        let (value, observed_at) =
            self.independent_sample(&expectation.subject, expectation.field, via)?;
        if observed_at <= wall
            || observed_at > wall + chrono::Duration::milliseconds(expectation.deadline_ms as i64)
        {
            return None;
        }
        Some(Observation {
            value,
            latency_ms: (observed_at - wall).num_milliseconds() as u64,
        })
    }

    pub(super) fn cancel_woken_role_expectations(&mut self, role: &str) {
        for pending in self.pending_decisions.values_mut() {
            for (expectation, progress) in pending.expectations.iter_mut().zip(&pending.progress) {
                if expectation.subject.strip_prefix("role:") == Some(role)
                    && expectation.field == Field::Power
                    && expectation.expect == "standby"
                {
                    progress.cancel(expectation);
                }
            }
        }
    }

    /// Expire independent evidence before an incoming late sample replaces it.
    /// Active CEC checks remain owned by the normal post-event timer pass.
    pub(super) fn settle_due_independent(&mut self, now: Instant) {
        if self
            .next_expectation_deadline()
            .is_some_and(|deadline| deadline <= now)
        {
            self.evaluate_expectations_inner(now, true);
        }
    }

    pub(super) fn evaluate_expectations(&mut self, now: Instant) {
        self.evaluate_expectations_inner(now, false);
    }

    fn evaluate_expectations_inner(&mut self, now: Instant, before_event: bool) {
        let mut decisions = std::mem::take(&mut self.pending_decisions);
        decisions.retain(|&id, pending| {
            for (index, expectation) in pending.expectations.iter_mut().enumerate() {
                if expectation.outcome != Outcome::Pending {
                    continue;
                }
                if before_event
                    && expectation.via.contains(&Via::Cec)
                    && self.standby_verifications.iter().any(|verification| {
                        verification.parent == Some(id)
                            && expectation.subject.strip_prefix("role:") == Some(&verification.role)
                    })
                {
                    continue;
                }
                let expired = now >= pending.at + Duration::from_millis(expectation.deadline_ms);
                self.evaluate_expectation(
                    pending.wall,
                    expectation,
                    &pending.progress[index],
                    expired,
                );
            }
            if pending
                .expectations
                .iter()
                .any(|expectation| expectation.outcome == Outcome::Pending)
            {
                return true;
            }
            self.verdicts.push(VerdictUpdate {
                id,
                verdict: verdict(&pending.expectations),
                expectations: std::mem::take(&mut pending.expectations),
            });
            false
        });
        self.pending_decisions = decisions;
    }

    /// Creation alone may use a recent matching independent observation.
    /// CEC still requires its fresh, due verification check.
    fn settle_already_satisfied(&self, expectation: &mut Expectation) {
        let newest = self.newest_independent_sample(
            &expectation.subject,
            expectation.field,
            &expectation.via,
            Some(self.wall),
        );
        let Some((via, value, at)) = newest else {
            return;
        };
        let oldest = self.wall - chrono::Duration::milliseconds(expectation.deadline_ms as i64);
        if value != expectation.expect
            || at < oldest
            || self.pending_decisions.values().any(|pending| {
                pending.wall >= at
                    && pending.expectations.iter().any(|previous| {
                        previous.subject == expectation.subject
                            && previous.field == expectation.field
                    })
            })
        {
            return;
        }
        expectation.outcome = Outcome::Met {
            via,
            observed: value.into(),
            latency_ms: 0,
        };
    }

    fn evaluate_expectation(
        &self,
        wall: DateTime<Utc>,
        expectation: &mut Expectation,
        progress: &ExpectationProgress,
        expired: bool,
    ) {
        // Only current independent samples count: an unreachable sample
        // replaces that source's earlier disagreement, not its truth.
        let mut disagreement = progress.cec_disagreement_at.map(|latency_ms| Observation {
            value: "on",
            latency_ms,
        });
        for &via in &expectation.via {
            let Some(observation) = self.observation(wall, expectation, via) else {
                continue;
            };
            if observation.value == expectation.expect {
                expectation.outcome = Outcome::Met {
                    via,
                    observed: observation.value.into(),
                    latency_ms: observation.latency_ms,
                };
                return;
            } else if disagreement
                .is_none_or(|previous| observation.latency_ms >= previous.latency_ms)
            {
                disagreement = Some(observation);
            }
        }
        if progress.cec_met && (!expectation.via.contains(&Via::Ps5) || expired) {
            expectation.outcome = Outcome::CecOnly {
                observed: expectation.expect.clone(),
            };
        } else if expired {
            expectation.outcome = match disagreement {
                Some(observation) => Outcome::Missed {
                    observed: observation.value.into(),
                },
                None => Outcome::Unknown {
                    observed: progress.absent.then(|| "absent".into()),
                },
            };
        }
    }

    pub(super) fn cancel_cec_expectations(&mut self) {
        for pending in self.pending_decisions.values_mut() {
            for (expectation, progress) in pending.expectations.iter_mut().zip(&pending.progress) {
                if expectation.outcome == Outcome::Pending
                    && expectation.via.as_slice() == [Via::Cec]
                {
                    progress.cancel(expectation);
                }
            }
        }
    }

    pub(super) fn cancel_reconfigured_expectations(&mut self, config: &Config) {
        for pending in self.pending_decisions.values_mut() {
            for (expectation, progress) in pending.expectations.iter_mut().zip(&pending.progress) {
                if expectation.outcome == Outcome::Pending
                    && expectation_config_changed(&self.config, config, expectation)
                {
                    progress.cancel(expectation);
                }
            }
        }
    }

    pub(super) fn cancel_expectations(&mut self) {
        for (id, mut pending) in std::mem::take(&mut self.pending_decisions) {
            pending.cancel();
            self.cancel_interrupted(id, std::mem::take(&mut pending.expectations));
        }
    }

    /// Restarts cannot resume an old monotonic deadline or its dispatched work.
    /// Preserve completed evidence, but never leave an abandoned row pending.
    pub(crate) fn cancel_interrupted(&mut self, id: u64, mut expectations: Vec<Expectation>) {
        if let Some(mut pending) = self.pending_decisions.remove(&id) {
            pending.cancel();
            expectations = pending.expectations;
        }
        for expectation in &mut expectations {
            if expectation.outcome == Outcome::Pending {
                expectation.outcome = Outcome::Unknown { observed: None };
            }
        }
        self.verdicts.push(VerdictUpdate {
            id,
            verdict: verdict(&expectations),
            expectations,
        });
    }
}

fn expectation_config_changed(old: &Config, new: &Config, expectation: &Expectation) -> bool {
    if new.mode == Mode::Shadow
        || expectation.via.contains(&Via::Ha) && old.home_assistant.url != new.home_assistant.url
    {
        return true;
    }
    match expectation.subject.as_str() {
        "tv" => {
            old.tv.entity != new.tv.entity
                || old.tv.off != new.tv.off
                || old.tv.wake != new.tv.wake
                || old.verify.tv_s != new.verify.tv_s
        }
        "receiver" if expectation.field == Field::Source => {
            old.receiver.entity != new.receiver.entity
                || old.receiver.select_source != new.receiver.select_source
                || old.verify.source_s != new.verify.source_s
                || old
                    .device
                    .iter()
                    .find(|device| device.source == expectation.expect)
                    .is_some_and(|device| {
                        !new.device
                            .iter()
                            .any(|next| next.role == device.role && next.source == device.source)
                    })
        }
        "receiver" => {
            let deadline_changed = if expectation.expect == "off" {
                old.verify.receiver_off_s != new.verify.receiver_off_s
            } else {
                old.verify.receiver_s != new.verify.receiver_s
            };
            old.receiver.entity != new.receiver.entity || deadline_changed
        }
        "speakers" => {
            old.sonos.entity != new.sonos.entity || old.verify.speakers_s != new.verify.speakers_s
        }
        subject => {
            let Some(role) = subject.strip_prefix("role:") else {
                return false;
            };
            let (Some(before), Some(after)) = (
                old.device.iter().find(|device| device.role == role),
                new.device.iter().find(|device| device.role == role),
            ) else {
                return true;
            };
            expectation.via.contains(&Via::Cec)
                && (old.timing.takeover_settle_ms != new.timing.takeover_settle_ms
                    || old.timing.standby_retries != new.timing.standby_retries
                    || old.bridge != new.bridge)
                || expectation.via.contains(&Via::Ps5)
                    && old.verify.probe_window_s != new.verify.probe_window_s
                || before.probe != after.probe
                || before.takeover != after.takeover
                || before.verify_standby != after.verify_standby
                || before.matches.vendor != after.matches.vendor
                || before.matches.osd != after.matches.osd
                || before.matches.physical != after.matches.physical
        }
    }
}

pub(super) fn verdict(expectations: &[Expectation]) -> Verdict {
    if expectations.is_empty() {
        return Verdict::None;
    }
    if expectations
        .iter()
        .any(|expectation| expectation.outcome == Outcome::Pending)
    {
        return Verdict::Pending;
    }
    let met = expectations
        .iter()
        .filter(|expectation| {
            matches!(
                expectation.outcome,
                Outcome::Met { .. } | Outcome::CecOnly { .. }
            )
        })
        .count();
    if met == expectations.len() {
        Verdict::Achieved
    } else if met > 0 {
        Verdict::Partial
    } else if expectations
        .iter()
        .all(|expectation| matches!(expectation.outcome, Outcome::Unknown { .. }))
    {
        Verdict::Unknown
    } else {
        Verdict::Missed
    }
}
