//! Reporting-only verification. Deadlines are injected monotonic time; evidence
//! is observed independently, never an optimistic command-side cache write.
use super::{DecisionTrigger, Engine, StandbyVerification, VerificationResult};
use crate::{
    action::{Action, Expectation, Field, Outcome, Verdict, VerdictUpdate, Via},
    cec::opcode::STANDBY,
    config::{Config, Takeover, TvOffConfig},
    state::{BusPower, Command, ConsoleState, DirectPower, Mode, RolePower},
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
    pub fn new(
        at: Instant,
        wall: DateTime<Utc>,
        expectations: Vec<Expectation>,
        contested: impl Fn(&Expectation) -> bool,
    ) -> Self {
        let progress = expectations
            .iter()
            .map(|expectation| ExpectationProgress {
                contested: contested(expectation),
                ..ExpectationProgress::default()
            })
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

#[derive(Clone, Copy)]
enum EvidenceMode {
    Current,
    Run,
    Retained,
}

#[derive(Default)]
struct ExpectationProgress {
    cec_disagreement_at: Option<u64>,
    cec_met: bool,
    absent: bool,
    contested: bool,
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

pub(super) fn tv_sources(config: &Config, direct_available: bool) -> &[Via] {
    if direct_available && matches!(config.tv.off, TvOffConfig::Webos { .. }) {
        &[Via::TvDirect]
    } else {
        &[Via::Ha]
    }
}

pub(super) fn receiver_power_sources(config: &Config) -> &[Via] {
    if config.receiver.observe.is_some() {
        &[Via::Denon]
    } else {
        &[Via::Ha]
    }
}

pub(super) fn sample_agrees(via: Via, observed: &str, expected: &str) -> bool {
    observed == expected || (via == Via::TvDirect && observed == "unreachable" && expected == "off")
}

fn direct_sample(
    cache: Option<(DirectPower, DateTime<Utc>)>,
    unreachable_is_evidence: bool,
) -> Option<(&'static str, DateTime<Utc>)> {
    let (state, at) = cache?;
    let value = match state {
        DirectPower::On => "on",
        DirectPower::Off => "off",
        DirectPower::Unreachable if unreachable_is_evidence => "unreachable",
        DirectPower::Unreachable => return None,
    };
    Some((value, at))
}
impl Engine {
    pub(super) fn drift_closure_sample(
        &self,
        subject: &str,
        field: Field,
        sources: &[Via],
    ) -> Option<(Via, &str, DateTime<Utc>)> {
        self.newest_independent_sample(subject, field, sources, None)
            .or_else(|| {
                sources.iter().find_map(|&via| {
                    let cache = match via {
                        Via::TvDirect => self.tv_last_usable,
                        Via::Denon => self.receiver_last_usable,
                        _ => None,
                    };
                    direct_sample(cache, via == Via::TvDirect).map(|(value, at)| (via, value, at))
                })
            })
    }
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
        Expectation {
            subject: subject.into(),
            field,
            expect: expect.into(),
            via,
            deadline_ms,
            attempts: 0,
            outcome: Outcome::Pending,
        }
    }

    fn tv_expectation(&self, value: &str) -> Expectation {
        let via = tv_sources(&self.config, !self.tv_direct_unavailable).to_vec();
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
                            receiver_power_sources(&self.config).to_vec(),
                            self.config.verify.receiver_s * 1000,
                        )),
                        "turn_off" if reason == "off_timer_expired" => Some(self.expectation(
                            "receiver",
                            Field::Power,
                            "off",
                            receiver_power_sources(&self.config).to_vec(),
                            self.config.verify.receiver_off_s * 1000,
                        )),
                        "select_source"
                            if matches!(reason, "takeover_settled" | "late_announcement") =>
                        {
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
        } else if matches!(reason, "all_off" | "manual_standby" | "late_announcement")
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
            .filter(|role| self.command_stop_dispatched(role, actions))
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
        let index = self.command_stop_row(actions);
        let id = self.decisions[index].id;
        for (role, mut expectation) in expectations {
            let _ = self.prepare_expectations(std::slice::from_mut(&mut expectation));
            self.attach_role_expectation(index, expectation);
            for pending in &mut self.standby_verifications {
                if pending.role == role && pending.parent.is_none() {
                    pending.parent = Some(id);
                }
            }
        }
    }

    fn command_stop_dispatched(&self, role: &str, actions: &[Action]) -> bool {
        let device = self
            .config
            .device
            .iter()
            .find(|device| device.role == role)
            .expect("configured role");
        actions.iter().any(|action| match action {
            Action::CecSend { to, data } => {
                data == &[STANDBY] && self.identity.roles.get(role) == Some(to)
            }
            Action::HaCall {
                entity, service, ..
            } => service == "media_stop" && device.media_player.as_ref() == Some(entity),
            _ => false,
        })
    }

    fn command_stop_row(&mut self, actions: &[Action]) -> usize {
        if let Some(index) = self.decisions.iter().position(|decision| {
            matches!(
                decision.reason.as_str(),
                "all_off" | "manual_standby" | "command_all_off" | "command_standby"
            )
        }) {
            return index;
        }
        let reason = match self.trigger {
            Some(DecisionTrigger::Command(Command::AllOff)) => "command_all_off",
            _ => "command_standby",
        };
        self.record(None, None, None, reason, actions.to_vec(), None);
        self.decisions.len() - 1
    }

    fn attach_role_expectation(&mut self, index: usize, expectation: Expectation) {
        if self.decisions[index]
            .expectations
            .iter()
            .any(|existing| existing.subject == expectation.subject)
        {
            return;
        }
        self.decisions[index].expectations.push(expectation.clone());
        self.decisions[index].verdict = verdict(&self.decisions[index].expectations);
        if self.decisions[index].verdict == Verdict::Pending {
            let id = self.decisions[index].id;
            self.pending_decisions
                .entry(id)
                .and_modify(|pending| pending.push(expectation))
                .or_insert_with(|| {
                    PendingDecision::new(
                        self.now,
                        self.wall,
                        self.decisions[index].expectations.clone(),
                        |_| false,
                    )
                });
        }
    }

    fn independent_sample<'a>(
        &'a self,
        subject: &str,
        field: Field,
        via: Via,
    ) -> Option<(&'a str, DateTime<Utc>)> {
        match via {
            Via::TvDirect => direct_sample(self.tv_direct, self.tv_unreachable_streak >= 2),
            Via::Denon => direct_sample(self.receiver_direct, false),
            Via::Ps5 => {
                let role = self.roles.get(subject.strip_prefix("role:")?)?;
                let (state, at) = role.direct?;
                match state {
                    ConsoleState::Awake => Some(("on", at)),
                    ConsoleState::Standby if role.probe_standby_trusted() => Some(("standby", at)),
                    _ => None,
                }
            }
            Via::Ha => self.ha_sample(subject, field),
            Via::Cec => None,
        }
    }

    fn ha_sample(&self, subject: &str, field: Field) -> Option<(&str, DateTime<Utc>)> {
        let entity = match subject {
            "tv" => &self.config.tv.entity,
            "receiver" => &self.config.receiver.entity,
            "speakers" => &self.config.sonos.entity,
            _ => return None,
        };
        let state = self.ha.get(entity)?;
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
        let at = match field {
            Field::Source => state.updated?,
            Field::Power | Field::Playback => state.changed?,
        };
        Some((value, at))
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
        mode: EvidenceMode,
    ) -> Option<Observation<'a>> {
        let direct = matches!(via, Via::TvDirect | Via::Denon);
        let sample = match mode {
            EvidenceMode::Run if direct => self.direct_run_sample(via, wall),
            EvidenceMode::Retained if direct => self.retained_direct_sample(via, wall),
            _ => self.independent_sample(&expectation.subject, expectation.field, via),
        };
        let (value, observed_at) = sample?;
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

    fn retained_direct_sample(
        &self,
        via: Via,
        wall: DateTime<Utc>,
    ) -> Option<(&'static str, DateTime<Utc>)> {
        self.direct_run_sample(via, wall)
            .filter(|(_, at)| *at > wall)
            .or_else(|| {
                let cache = match via {
                    Via::TvDirect => self.tv_last_usable,
                    Via::Denon => self.receiver_last_usable,
                    _ => None,
                };
                direct_sample(cache, via == Via::TvDirect)
            })
    }

    fn direct_run_sample(
        &self,
        via: Via,
        wall: DateTime<Utc>,
    ) -> Option<(&'static str, DateTime<Utc>)> {
        let run = match via {
            Via::TvDirect => self.tv_run,
            Via::Denon => self.receiver_run,
            _ => None,
        }?;
        if !run.sustained(via == Via::TvDirect)
            || run.state == DirectPower::Unreachable && run.previous.is_none_or(|at| at <= wall)
        {
            return None;
        }
        direct_sample(Some((run.state, run.evidence_at())), via == Via::TvDirect)
    }

    /// Close before pre-settlement so no new cycle inherits pending ownership.
    pub(super) fn prepare_expectations(
        &mut self,
        expectations: &mut [Expectation],
    ) -> Vec<(String, Field)> {
        let mut contested = vec![];
        let mut pending = std::mem::take(&mut self.pending_decisions);
        for old in pending.values_mut() {
            self.close_matching_fields(old, expectations, &mut contested);
        }
        self.pending_decisions = pending;
        self.publish_completed_expectations();
        for expectation in expectations {
            if !contested.iter().any(|(subject, field)| {
                subject == &expectation.subject && *field == expectation.field
            }) {
                self.settle_already_satisfied(expectation);
            }
        }
        contested
    }

    fn close_matching_fields(
        &self,
        old: &mut PendingDecision,
        expectations: &[Expectation],
        contested: &mut Vec<(String, Field)>,
    ) {
        for (expectation, progress) in old.expectations.iter_mut().zip(&old.progress) {
            if expectation.outcome != Outcome::Pending {
                continue;
            }
            let Some(new) = expectations
                .iter()
                .find(|new| new.subject == expectation.subject && new.field == expectation.field)
            else {
                continue;
            };
            self.close_expectation(old.wall, expectation, progress);
            if self.direct_field(new)
                && new.expect != expectation.expect
                && matches!(expectation.outcome, Outcome::Unknown { .. })
            {
                contested.push((new.subject.clone(), new.field));
            }
        }
    }

    fn close_expectation(
        &self,
        wall: DateTime<Utc>,
        expectation: &mut Expectation,
        progress: &ExpectationProgress,
    ) {
        if expectation
            .via
            .iter()
            .any(|via| matches!(via, Via::Cec | Via::Ps5))
        {
            progress.cancel(expectation);
            return;
        }
        let newest = expectation
            .via
            .iter()
            .filter_map(|&via| {
                self.observation(wall, expectation, via, EvidenceMode::Run)
                    .map(|sample| (via, sample))
            })
            .max_by_key(|(_, sample)| sample.latency_ms);
        expectation.outcome = match newest {
            Some((via, sample)) if sample_agrees(via, sample.value, &expectation.expect) => {
                Outcome::Met {
                    via,
                    observed: sample.value.into(),
                    latency_ms: sample.latency_ms,
                }
            }
            Some((_, sample)) => Outcome::Missed {
                observed: sample.value.into(),
            },
            None => Outcome::Unknown { observed: None },
        };
    }

    fn publish_completed_expectations(&mut self) {
        self.pending_decisions.retain(|&id, pending| {
            if pending
                .expectations
                .iter()
                .any(|e| e.outcome == Outcome::Pending)
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
        if !sample_agrees(via, value, &expectation.expect) || at < oldest {
            return;
        }
        expectation.outcome = Outcome::Met {
            via,
            observed: value.into(),
            latency_ms: 0,
        };
    }

    fn direct_field(&self, expectation: &Expectation) -> bool {
        expectation
            .via
            .iter()
            .any(|via| matches!(via, Via::TvDirect | Via::Denon))
    }

    pub(super) fn refresh_tv_sources(&mut self) {
        let sources = tv_sources(&self.config, !self.tv_direct_unavailable);
        for pending in self.pending_decisions.values_mut() {
            for expectation in &mut pending.expectations {
                if expectation.subject == "tv" && expectation.outcome == Outcome::Pending {
                    expectation.via.clear();
                    expectation.via.extend_from_slice(sources);
                }
            }
        }
    }

    fn evaluate_expectation(
        &self,
        wall: DateTime<Utc>,
        expectation: &mut Expectation,
        progress: &ExpectationProgress,
        expired: bool,
    ) {
        let mode = match (progress.contested, expired) {
            (true, false) => EvidenceMode::Run,
            (true, true) => EvidenceMode::Retained,
            (false, _) => EvidenceMode::Current,
        };
        let (matched, mut disagreement) = self.independent_outcome(wall, expectation, mode);
        if let Some(outcome) = matched {
            expectation.outcome = outcome;
            return;
        }
        if let Some(latency_ms) = progress.cec_disagreement_at
            && disagreement.is_none_or(|sample| latency_ms >= sample.latency_ms)
        {
            disagreement = Some(Observation {
                value: "on",
                latency_ms,
            });
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

    fn independent_outcome<'a>(
        &'a self,
        wall: DateTime<Utc>,
        expectation: &Expectation,
        mode: EvidenceMode,
    ) -> (Option<Outcome>, Option<Observation<'a>>) {
        let mut disagreement: Option<Observation<'a>> = None;
        for &via in &expectation.via {
            let Some(observation) = self.observation(wall, expectation, via, mode) else {
                continue;
            };
            if sample_agrees(via, observation.value, &expectation.expect) {
                return (
                    Some(Outcome::Met {
                        via,
                        observed: observation.value.into(),
                        latency_ms: observation.latency_ms,
                    }),
                    disagreement,
                );
            }
            if disagreement.is_none_or(|previous| observation.latency_ms >= previous.latency_ms) {
                disagreement = Some(observation);
            }
        }
        (None, disagreement)
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
            old.receiver.entity != new.receiver.entity
                || old.receiver.observe != new.receiver.observe
                || deadline_changed
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
