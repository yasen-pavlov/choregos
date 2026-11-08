//! Presentation helpers shared by templates: chrome, cockpit derivations and value formatting.
//!
//! Colour and tone helpers return class names only; templates never branch on raw strings
//! to pick a colour.

pub(crate) mod devices;
pub(crate) mod recorder;
pub(crate) mod window;

use chrono::{DateTime, Utc};

use super::auth::{Ctx, SessionUser};
use crate::{
    action::{DriftDetail, Expectation, Field, Outcome, ServiceKind, Trigger, Verdict},
    state::{
        BridgeTelemetry, BusEntryView, BusPower, DecisionView, DirectPower, FrameDirection,
        HaEntityView, Health, Mode, RolePower, RoleView, RoomPower, Snapshot,
    },
};

/// Bridge Wi-Fi signal below this (dBm) is flagged in the bridge drawer.
pub(crate) const WEAK_SIGNAL_DBM: i32 = -75;

/// Chrome rendered by `base.html` around every authenticated page.
pub(crate) struct Layout {
    /// Navigation key: `cockpit`, `recorder`, `devices` or `config`.
    pub(crate) page: &'static str,
    pub(crate) csrf: String,
    pub(crate) user: SessionUser,
    pub(crate) status: Status,
}

impl Layout {
    pub(crate) fn new(ctx: Ctx, page: &'static str, snapshot: &Snapshot) -> Self {
        Self {
            page,
            csrf: ctx.csrf,
            user: ctx.user,
            status: Status::new(snapshot, false),
        }
    }
}

/// The top-bar `#status` block (mode switch, consistency badge, connection pills) and the
/// `#bands` under the bar; polled on every page, carried out of band by action responses.
pub(crate) struct Status {
    pub(crate) mode: Mode,
    pub(crate) health: Health,
    pub(crate) consistency: Consistency,
    /// Bridge pill tooltip suffix (`−50 dBm`) once telemetry exists.
    pub(crate) bridge_signal: Option<String>,
    pub(crate) last_frame_at: Option<DateTime<Utc>>,
    /// Render with `hx-swap-oob` (action responses) instead of for `hx-select`.
    pub(crate) oob: bool,
}

impl Status {
    pub(crate) fn new(snapshot: &Snapshot, oob: bool) -> Self {
        Self {
            mode: snapshot.mode,
            health: snapshot.connections.clone(),
            consistency: consistency(snapshot),
            bridge_signal: snapshot
                .bridge_telemetry
                .as_ref()
                .map(|telemetry| dbm(&telemetry.signal_dbm)),
            last_frame_at: snapshot.last_frame_at,
            oob,
        }
    }

    pub(crate) fn is_shadow(&self) -> bool {
        self.mode == Mode::Shadow
    }
}

/// Whether the room matches what Choregos intends, in the words of the HA `drift` sensor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Consistency {
    Warming,
    NotChecked,
    Unknown(&'static str),
    /// Every open drift; the badge names the first, the tooltip lists all.
    Drift(Vec<DriftDetail>),
    Consistent,
}

/// The consistency rule, first match wins: warming, shadow, no intent, open drift, TV
/// unreachable while the room is on, else consistent.
pub(crate) fn consistency(snapshot: &Snapshot) -> Consistency {
    if snapshot.warming {
        Consistency::Warming
    } else if snapshot.mode == Mode::Shadow {
        Consistency::NotChecked
    } else if snapshot.intent.power == RoomPower::Unknown {
        Consistency::Unknown("no intent yet")
    } else if !snapshot.drift.is_empty() {
        Consistency::Drift(snapshot.drift.clone())
    } else if snapshot.intent.power == RoomPower::On
        && matches!(snapshot.tv_direct, Some((DirectPower::Unreachable, _)))
    {
        Consistency::Unknown("TV unreachable")
    } else {
        Consistency::Consistent
    }
}

impl Consistency {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Warming => "warming".to_owned(),
            Self::NotChecked => "not checked (shadow)".to_owned(),
            Self::Unknown(why) => format!("unknown: {why}"),
            Self::Drift(open) => open.first().map_or_else(
                || "drift".to_owned(),
                |drift| format!("drift: {}.{}", drift.subject, drift.field),
            ),
            Self::Consistent => "consistent".to_owned(),
        }
    }

    pub(crate) fn tone(&self) -> &'static str {
        match self {
            Self::Warming => "pend",
            Self::NotChecked => "shadow",
            Self::Unknown(_) => "dim",
            Self::Drift(_) => "warn",
            Self::Consistent => "ok",
        }
    }

    /// One plain sentence for the room strip.
    pub(crate) fn sentence(&self) -> String {
        match self {
            Self::Warming => {
                "Waiting for the bus to answer the resync; actions are refused meanwhile."
                    .to_owned()
            }
            Self::NotChecked => {
                "Drift is not checked in shadow, and outputs are read from Home Assistant only."
                    .to_owned()
            }
            Self::Unknown("no intent yet") => {
                "Nothing has woken or turned off the room since start, so there is nothing to compare."
                    .to_owned()
            }
            Self::Unknown(_) => {
                "The TV does not answer on the network, so its power cannot be checked.".to_owned()
            }
            Self::Drift(open) => open.first().map_or_else(String::new, |drift| {
                format!(
                    "{} {} should be {} but reads {} via {} — for {}. Reporting only.",
                    drift.subject,
                    drift.field,
                    drift.intended,
                    drift.observed,
                    drift.via,
                    open_for(&drift.since)
                )
            }),
            Self::Consistent => "TV and receiver agree with the intent.".to_owned(),
        }
    }

    /// Tooltip: every open drift, else the sentence.
    pub(crate) fn title(&self) -> String {
        match self {
            Self::Drift(open) => open
                .iter()
                .map(|drift| {
                    format!(
                        "{}.{}: intended {}, observed {} via {} since {}",
                        drift.subject,
                        drift.field,
                        drift.intended,
                        drift.observed,
                        drift.via,
                        iso(&drift.since)
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
            other => other.sentence(),
        }
    }
}

/// What kind of event a decision reason describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReasonGroup {
    Wake,
    Standby,
    Takeover,
    Timer,
    Identity,
    Drift,
    Command,
    Service,
    Other,
}

impl ReasonGroup {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Wake => "wake",
            Self::Standby => "standby",
            Self::Takeover => "takeover",
            Self::Timer => "timer",
            Self::Identity => "identity",
            Self::Drift => "drift",
            Self::Command => "command",
            Self::Service => "service",
            Self::Other => "other",
        }
    }
}

/// Every reason literal the engine records (and the three pre-v0.3.0 standby-verification
/// literals still found in restored rings), its group and one sentence in plain words.
const REASONS: &[(&str, ReasonGroup, &str)] = &[
    (
        "ImageViewOn",
        ReasonGroup::Wake,
        "The device announced itself with Image View On; Choregos treats it as a wake.",
    ),
    (
        "TextViewOn",
        ReasonGroup::Wake,
        "The device announced itself with Text View On; Choregos treats it as a wake.",
    ),
    (
        "ActiveSource",
        ReasonGroup::Wake,
        "The device declared itself the active source; Choregos treats it as a wake.",
    ),
    (
        "PowerOn",
        ReasonGroup::Wake,
        "The device reported that it is on; Choregos treats it as a wake.",
    ),
    (
        "ReportPhysicalAddress",
        ReasonGroup::Wake,
        "The device reported its physical address, which its profile treats as a wake.",
    ),
    (
        "manual_takeover",
        ReasonGroup::Wake,
        "An operator asked this role to take over the room.",
    ),
    (
        "resync_complete",
        ReasonGroup::Wake,
        "A signal held back during warm-up was applied once the bus model settled.",
    ),
    (
        "console_start",
        ReasonGroup::Wake,
        "The console's rest-mode probe saw it start before it spoke on the bus.",
    ),
    (
        "Standby",
        ReasonGroup::Standby,
        "The device sent Standby; its role is now off.",
    ),
    (
        "InactiveSource",
        ReasonGroup::Standby,
        "The device gave up the active source; its role is now off.",
    ),
    (
        "PowerStandby",
        ReasonGroup::Standby,
        "The device reported that it is in standby; its role is now off.",
    ),
    (
        "manual_standby",
        ReasonGroup::Standby,
        "An operator asked to put this role in standby; its stop actions, if any, were issued and the role is now off.",
    ),
    (
        "all_off",
        ReasonGroup::Standby,
        "All off: this role's stop actions, if any, were issued with the rest of the room and the role is now off.",
    ),
    (
        "presence_timeout",
        ReasonGroup::Standby,
        "A polled device stopped answering presence polls; it is treated as having left the bus and its role is now off.",
    ),
    (
        "taken_over",
        ReasonGroup::Standby,
        "Another role took over the room; this role's stop actions, if any, were issued and it is now treated as off.",
    ),
    (
        "late_announcement",
        ReasonGroup::Standby,
        "This console started before the current one but announced itself later; its stop actions, if any, were issued and the current role kept the room.",
    ),
    (
        "console_start_expired",
        ReasonGroup::Standby,
        "The console's start was not confirmed on the bus within its boot window, or Choregos switched to shadow first; the early wake is withdrawn.",
    ),
    (
        "console_start_retracted",
        ReasonGroup::Standby,
        "The console went back to rest mode before it spoke on the bus; the early wake is withdrawn.",
    ),
    (
        "standby_retry",
        ReasonGroup::Standby,
        "Before v0.3.0: Standby was not confirmed in time and was sent again.",
    ),
    (
        "standby_unconfirmed",
        ReasonGroup::Standby,
        "Before v0.3.0: the device never confirmed Standby.",
    ),
    (
        "standby_target_absent",
        ReasonGroup::Standby,
        "Before v0.3.0: no Standby send was acknowledged; the device was treated as absent from the bus.",
    ),
    (
        "takeover_settled",
        ReasonGroup::Takeover,
        "The takeover settle delay elapsed; any needed source selection and preset were issued.",
    ),
    (
        "off_timer_expired",
        ReasonGroup::Timer,
        "The off timer ran out with no role on, so TV off and receiver off were issued.",
    ),
    (
        "identity_lost",
        ReasonGroup::Identity,
        "The bus entry bound to this role stopped matching it or was forgotten; the role is now off.",
    ),
    (
        "conflict",
        ReasonGroup::Identity,
        "Two devices match one role, or one device matches two roles; those roles are blocked.",
    ),
    (
        "conflict_resolved",
        ReasonGroup::Identity,
        "Every identity conflict cleared; no role is blocked by one any more.",
    ),
    (
        "command_identify",
        ReasonGroup::Identity,
        "An operator asked a device to identify itself.",
    ),
    (
        "command_forget",
        ReasonGroup::Identity,
        "An operator made Choregos forget a learned device.",
    ),
    (
        "drift",
        ReasonGroup::Drift,
        "The room has disagreed with its intended state for longer than the grace period; reporting only.",
    ),
    (
        "drift_resolved",
        ReasonGroup::Drift,
        "The drift was closed by agreement, a changed intent, or leaving active monitoring.",
    ),
    (
        "command_all_off",
        ReasonGroup::Command,
        "An operator asked for all off.",
    ),
    (
        "command_set_mode",
        ReasonGroup::Command,
        "An operator switched the operating mode.",
    ),
    (
        "command_send_raw",
        ReasonGroup::Command,
        "An operator sent a raw CEC frame.",
    ),
    (
        "command_takeover",
        ReasonGroup::Command,
        "An operator's take-over command; this row carries the actions it issued.",
    ),
    (
        "command_standby",
        ReasonGroup::Command,
        "An operator's standby command; this row carries its stop actions.",
    ),
    (
        "service_started",
        ReasonGroup::Service,
        "Choregos started; rows before this were made by the previous run.",
    ),
    (
        "startup_source_ignored",
        ReasonGroup::Service,
        "A wake-like signal heard during the startup resync was not treated as a wake: the receiver was not known to be on, or the resync restarted.",
    ),
    (
        "sonos_volume",
        ReasonGroup::Other,
        "A volume or mute key from this role's remote was forwarded to the speakers.",
    ),
];

/// The group of a reason literal; unknown literals are `Other`.
pub(crate) fn reason_group(reason: &str) -> ReasonGroup {
    REASONS
        .iter()
        .find(|(literal, ..)| *literal == reason)
        .map_or(ReasonGroup::Other, |(_, group, _)| *group)
}

/// The glossary sentence of a reason literal.
pub(crate) fn reason_sentence(reason: &str) -> Option<&'static str> {
    REASONS
        .iter()
        .find(|(literal, ..)| *literal == reason)
        .map(|(.., sentence)| *sentence)
}

/// Tooltip for a reason literal: its group and glossary sentence.
pub(crate) fn reason_title(reason: &str) -> String {
    let group = reason_group(reason).label();
    reason_sentence(reason).map_or_else(
        || format!("{group} — not in the glossary"),
        |sentence| format!("{group} — {sentence}"),
    )
}

/// What a decision sent, by destination. Identify, resync and presence polls are
/// housekeeping and not counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ActionSummary {
    pub(crate) tv: bool,
    pub(crate) ha: u8,
    pub(crate) cec: u8,
}

impl ActionSummary {
    /// Parses the ring's action strings (optional `would: ` prefix + the action's JSON);
    /// anything unparsable is ignored.
    pub(crate) fn from_actions(actions: &[String]) -> Self {
        let mut summary = Self::default();
        for action in actions {
            let json = action.strip_prefix("would: ").unwrap_or(action);
            let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
                continue;
            };
            match value.get("kind").and_then(serde_json::Value::as_str) {
                Some("wake_tv" | "tv_off") => summary.tv = true,
                Some("ha_call") => summary.ha = summary.ha.saturating_add(1),
                Some("cec_send") => summary.cec = summary.cec.saturating_add(1),
                _ => {}
            }
        }
        summary
    }

    /// `TV · 1 HA · 3 CEC`, or `—` when nothing was sent.
    pub(crate) fn text(&self) -> String {
        let mut parts = Vec::new();
        if self.tv {
            parts.push("TV".to_owned());
        }
        if self.ha > 0 {
            parts.push(format!("{} HA", self.ha));
        }
        if self.cec > 0 {
            parts.push(format!("{} CEC", self.cec));
        }
        if parts.is_empty() {
            "—".to_owned()
        } else {
            parts.join(" · ")
        }
    }
}

/// The slowest met expectation of a decision (what the HA `last_decision` sensor reports).
pub(crate) fn max_latency(decision: &DecisionView) -> Option<u64> {
    decision
        .expectations
        .iter()
        .filter_map(|expectation| match expectation.outcome {
            Outcome::Met { latency_ms, .. } => Some(latency_ms),
            _ => None,
        })
        .max()
}

fn is_tv_on(expectation: &Expectation) -> bool {
    expectation.subject == "tv" && expectation.field == Field::Power && expectation.expect == "on"
}

/// Wake to picture: the met `tv.power = on` latency. Zero means the TV was already on when
/// the wake was decided, which is not a measurement.
pub(crate) fn wake_latency(decision: &DecisionView) -> Option<u64> {
    decision
        .expectations
        .iter()
        .filter(|expectation| is_tv_on(expectation))
        .find_map(|expectation| match expectation.outcome {
            Outcome::Met { latency_ms, .. } if latency_ms > 0 => Some(latency_ms),
            _ => None,
        })
}

/// A role's display name; the id itself for a role no longer configured.
pub(crate) fn role_name(snapshot: &Snapshot, role: &str) -> String {
    snapshot
        .roles
        .iter()
        .find(|view| view.role == role)
        .map_or_else(|| role.to_owned(), |view| view.name.clone())
}

/// The newest ring row about `role`.
pub(crate) fn last_decision_for<'a>(
    snapshot: &'a Snapshot,
    role: &str,
) -> Option<&'a DecisionView> {
    snapshot
        .decisions
        .iter()
        .rev()
        .find(|decision| decision.role.as_deref() == Some(role))
}

/// Rows still being verified, newest first.
pub(crate) fn pending(snapshot: &Snapshot) -> Vec<&DecisionView> {
    snapshot
        .decisions
        .iter()
        .rev()
        .filter(|decision| decision.verdict == Verdict::Pending)
        .collect()
}

/// The role's configured Home Assistant media player, if any.
pub(crate) fn ha_entity_for<'a>(
    snapshot: &'a Snapshot,
    role: &RoleView,
) -> Option<&'a HaEntityView> {
    let entity = role.media_player.as_deref()?;
    snapshot
        .ha
        .media_players
        .iter()
        .find(|view| view.entity == entity)
}

/// Rows imported from a pre-v0.3.0 state file: plain history with no trigger or verdict.
pub(crate) fn is_migrated(decision: &DecisionView) -> bool {
    matches!(
        decision.trigger,
        Trigger::Service {
            name: ServiceKind::Migrated
        }
    )
}

/// The wake row behind `decision`: itself for a wake, its parent for `takeover_settled`.
pub(crate) fn wake_row<'a>(
    snapshot: &'a Snapshot,
    decision: &'a DecisionView,
) -> Option<&'a DecisionView> {
    if decision.to == Some(RolePower::On) {
        return Some(decision);
    }
    let parent = decision
        .parent
        .filter(|_| decision.reason == "takeover_settled")?;
    snapshot.decisions.iter().rev().find(|row| row.id == parent)
}

/// One line on the wake of a wake row: `Wake → picture 7.4 s`, or why there is no number.
pub(crate) fn wake_line(decision: &DecisionView) -> Option<String> {
    let tv = decision.expectations.iter().find(|e| is_tv_on(e))?;
    if let Some(millis) = wake_latency(decision) {
        return Some(format!("Wake → picture {}", latency(&millis)));
    }
    Some(match &tv.outcome {
        Outcome::Pending => format!("Wake → picture: measuring (#{} pending)", decision.id),
        Outcome::Met { .. } => "Wake → picture: the TV was already on".to_owned(),
        Outcome::Missed { observed } => format!("Wake → picture: missed (read {observed})"),
        Outcome::Unknown { .. } => "Wake → picture: could not tell".to_owned(),
        Outcome::CecOnly { .. } => "Wake → picture: CEC only".to_owned(),
    })
}

/// One line on a role's standby verification, from the `role:<role>` expectation of
/// `decision`.
pub(crate) fn standby_line(decision: &DecisionView, role: &str) -> Option<String> {
    let subject = format!("role:{role}");
    let expectation = decision
        .expectations
        .iter()
        .find(|e| e.subject == subject && e.field == Field::Power)?;
    let line = match &expectation.outcome {
        Outcome::Pending => "Standby: confirming…".to_owned(),
        Outcome::Met {
            via, latency_ms, ..
        } => format!("Standby confirmed via {via} in {}", latency(latency_ms)),
        Outcome::Missed { observed } => format!("Standby missed: still {observed}"),
        Outcome::Unknown { observed } if observed.as_deref() == Some("absent") => {
            "Standby unconfirmed: never acknowledged on the bus".to_owned()
        }
        Outcome::Unknown { .. } => "Standby unconfirmed".to_owned(),
        Outcome::CecOnly { .. } => "Standby confirmed on CEC only".to_owned(),
    };
    Some(match expectation.attempts {
        0 => line,
        1 => format!("{line} · 1 retry"),
        n => format!("{line} · {n} retries"),
    })
}

/// Chip tone of a verdict.
pub(crate) fn verdict_tone(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Achieved => "ok",
        Verdict::Partial => "warn",
        Verdict::Missed => "bad",
        Verdict::Pending => "pend",
        Verdict::Unknown => "dim",
        Verdict::None => "none",
    }
}

/// Card edge class for a verdict; no edge without a verdict to show.
pub(crate) fn verdict_edge(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Achieved => "edge-ok",
        Verdict::Partial => "edge-warn",
        Verdict::Missed => "edge-bad",
        Verdict::Pending => "edge-pend",
        Verdict::Unknown | Verdict::None => "",
    }
}

/// Home Assistant power words normalised the way the engine reads them: playing, paused,
/// idle and buffering are on; standby is off; anything else (unknown, unavailable) is not
/// evidence.
pub(crate) fn ha_power(state: &str) -> Option<bool> {
    match state {
        "on" | "playing" | "paused" | "idle" | "buffering" => Some(true),
        "off" | "standby" => Some(false),
        _ => None,
    }
}

/// A truth source's value in a provenance stack.
pub(crate) struct Segment {
    pub(crate) source: &'static str,
    pub(crate) value: String,
    pub(crate) at: Option<DateTime<Utc>>,
    /// The value Choregos uses.
    pub(crate) used: bool,
    /// Not configured or not sampled: rendered dim.
    pub(crate) absent: bool,
    /// Disagrees with the intent (an open drift names this source).
    pub(crate) disagrees: bool,
}

impl Segment {
    pub(crate) fn new(
        source: &'static str,
        value: impl Into<String>,
        at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            source,
            value: value.into(),
            at,
            used: false,
            absent: false,
            disagrees: false,
        }
    }

    pub(crate) fn absent(source: &'static str, value: impl Into<String>) -> Self {
        Self {
            absent: true,
            ..Self::new(source, value, None)
        }
    }

    pub(crate) fn class(&self) -> String {
        let mut classes = Vec::new();
        if self.used {
            classes.push("use");
        }
        if self.absent {
            classes.push("na");
        }
        if self.disagrees {
            classes.push("dis");
        }
        classes.join(" ")
    }

    pub(crate) fn age(&self) -> Option<String> {
        self.at.as_ref().map(age)
    }

    pub(crate) fn title(&self) -> Option<String> {
        self.at.as_ref().map(iso)
    }
}

/// A role's power by truth source: CEC (its bus entry), Home Assistant (its media player,
/// when configured) and the probe. The one Choregos used is marked: the probe when its
/// sample is newer than the last CEC word, else CEC.
pub(crate) struct RoleSources {
    pub(crate) cec: Segment,
    pub(crate) ha: Segment,
    pub(crate) probe: Segment,
}

/// The bus entry a role is bound to.
pub(crate) fn bound_entry<'a>(snapshot: &'a Snapshot, role: &RoleView) -> Option<&'a BusEntryView> {
    role.logical_address.and_then(|la| {
        snapshot
            .bus
            .iter()
            .find(|entry| entry.logical_address == la)
    })
}

/// Unbound, waiting for an entry restored from the state file to be confirmed live.
pub(crate) fn is_restored(role: &RoleView) -> bool {
    role.restored && role.logical_address.is_none()
}

/// A role's identity chip, `(label, tone)`: conflict, restored, bound or unbound. Bound
/// entries are verified (identity binds nothing else); a restored role is unbound and only
/// matches an entry the state file brought back.
pub(crate) fn role_identity(role: &RoleView) -> (&'static str, &'static str) {
    if role.conflicted {
        ("conflict", "bad")
    } else if is_restored(role) {
        ("restored", "pend")
    } else if role.logical_address.is_some() {
        ("bound", "ok")
    } else {
        ("unbound", "dim")
    }
}

pub(crate) fn role_sources(snapshot: &Snapshot, role: &RoleView) -> RoleSources {
    let shadow = snapshot.mode == Mode::Shadow;
    let entry = bound_entry(snapshot, role);
    let mut cec = match entry {
        Some(entry) => Segment::new("cec", bus_power(&entry.power), role.last_seen),
        None if is_restored(role) => Segment::absent("cec", "silent since restore"),
        None => Segment::absent("cec", "—"),
    };
    let ha = match (role.media_player.is_some(), ha_entity_for(snapshot, role)) {
        (true, Some(entity)) => Segment::new("ha", entity.state.clone(), entity.last_changed),
        (true, None) => Segment::absent("ha", "unknown"),
        (false, _) => Segment::absent("ha", "not configured"),
    };
    let mut probe = match (role.probe_configured, role.direct) {
        (false, _) => Segment::absent("probe", "—"),
        (true, _) if shadow => Segment::absent("probe", "not sampled (shadow)"),
        (true, Some((state, at))) => Segment::new("probe", state.to_string(), Some(at)),
        (true, None) => Segment::absent("probe", "not sampled"),
    };
    // Power follows CEC; a probe sample newer than the last CEC word is what was used last.
    let probe_newer = !shadow
        && role
            .direct
            .is_some_and(|(_, at)| role.last_seen.is_none_or(|seen| at > seen));
    if role.probe_configured && probe_newer {
        probe.used = true;
    } else if entry.is_some() {
        cec.used = true;
    }
    RoleSources { cec, ha, probe }
}

// Askama passes call arguments by reference, hence the `&` parameters below.

pub(crate) fn clock(timestamp: &DateTime<Utc>) -> String {
    timestamp.format("%H:%M:%S%.3f").to_string()
}

/// [`clock`] split for the bus window, whose phone layout drops the hours and minutes:
/// `17:08:` …
pub(crate) fn clock_hm(timestamp: &DateTime<Utc>) -> String {
    timestamp.format("%H:%M:").to_string()
}

/// … and `40.865`.
pub(crate) fn clock_s(timestamp: &DateTime<Utc>) -> String {
    timestamp.format("%S%.3f").to_string()
}

pub(crate) fn hms(timestamp: &DateTime<Utc>) -> String {
    timestamp.format("%H:%M:%S").to_string()
}

/// Wall time, with its day unless it is today: `13:57:02`, `yesterday 13:57:02`,
/// `11-04 13:57:02`.
pub(crate) fn when(timestamp: &DateTime<Utc>) -> String {
    when_at(*timestamp, Utc::now(), "%H:%M:%S")
}

/// [`when`] with milliseconds, for decision rows.
pub(crate) fn when_ms(timestamp: &DateTime<Utc>) -> String {
    when_at(*timestamp, Utc::now(), "%H:%M:%S%.3f")
}

fn when_at(timestamp: DateTime<Utc>, now: DateTime<Utc>, format: &str) -> String {
    let time = timestamp.format(format);
    match (now.date_naive() - timestamp.date_naive()).num_days() {
        0 => time.to_string(),
        1 => format!("yesterday {time}"),
        _ => format!("{} {time}", timestamp.format("%m-%d")),
    }
}

pub(crate) fn iso(timestamp: &DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(crate) fn ago(timestamp: &Option<DateTime<Utc>>) -> String {
    timestamp.as_ref().map_or_else(|| "never".to_owned(), since)
}

pub(crate) fn since(timestamp: &DateTime<Utc>) -> String {
    ago_at(*timestamp, Utc::now())
}

/// Compact age for provenance segments: `8 s`, `41 m`, `2 h`, `3 d`.
pub(crate) fn age(timestamp: &DateTime<Utc>) -> String {
    let (value, unit) = bucket(*timestamp, Utc::now());
    format!("{value} {unit}")
}

fn bucket(timestamp: DateTime<Utc>, now: DateTime<Utc>) -> (i64, &'static str) {
    let seconds = (now - timestamp).num_seconds().max(0);
    match seconds {
        0..60 => (seconds, "s"),
        60..3600 => (seconds / 60, "m"),
        3600..86_400 => (seconds / 3600, "h"),
        _ => (seconds / 86_400, "d"),
    }
}

fn ago_at(timestamp: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let (value, unit) = bucket(timestamp, now);
    format!("{value}{unit} ago")
}

/// Compact uptime: the two most significant units (`3d 6h`, `6h 25m`), else one (`25m`, `48s`).
pub(crate) fn uptime(seconds: &u64) -> String {
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    match (days, hours, minutes) {
        (1.., ..) => format!("{days}d {hours}h"),
        (0, 1.., _) => format!("{hours}h {minutes}m"),
        (0, 0, 1..) => format!("{minutes}m"),
        (0, 0, 0) => format!("{seconds}s"),
    }
}

/// `49 ms` under a second, else tenths of a second (`7.4 s`, truncated).
pub(crate) fn latency(millis: &u64) -> String {
    if *millis < 1000 {
        format!("{millis} ms")
    } else {
        format!("{}.{} s", millis / 1000, millis % 1000 / 100)
    }
}

/// Signal strength with a typographic minus (`−50 dBm`).
pub(crate) fn dbm(signal: &i32) -> String {
    let magnitude = signal.unsigned_abs();
    if *signal < 0 {
        format!("−{magnitude} dBm")
    } else {
        format!("{magnitude} dBm")
    }
}

pub(crate) fn links(count: &u32) -> String {
    if *count == 1 {
        "1 link".to_owned()
    } else {
        format!("{count} links")
    }
}

/// Whether an online bridge's link looks degraded: weak signal or telemetry gone quiet.
pub(crate) fn bridge_degraded(snapshot: &Snapshot) -> bool {
    snapshot.bridge_online
        && snapshot
            .bridge_telemetry
            .as_ref()
            .is_some_and(|telemetry| telemetry_degraded_at(telemetry, Utc::now()))
}

/// Telemetry older than its adapter's `stale_after_s` (three missed periods) means the
/// telemetry, or the link, is stuck. `stale_after_s == 0` means periodic telemetry is
/// disabled, so only the signal is judged.
fn telemetry_degraded_at(telemetry: &BridgeTelemetry, now: DateTime<Utc>) -> bool {
    let weak_signal = telemetry.signal_dbm < WEAK_SIGNAL_DBM;
    let stale = telemetry.stale_after_s != 0
        && (now - telemetry.at).num_seconds()
            > i64::try_from(telemetry.stale_after_s).unwrap_or(i64::MAX);
    weak_signal || stale
}

/// The bridge's bus-line health worth flagging: `stuck_low`, `no_pullup`; nothing when `ok`,
/// unknown, or the bridge is offline (the last line state is stale then).
pub(crate) fn bus_fault(snapshot: &Snapshot) -> Option<&str> {
    snapshot
        .bridge_bus
        .as_deref()
        .filter(|bus| snapshot.bridge_online && *bus != "ok")
}

/// What a bus-line fault means for the room.
pub(crate) fn bus_fault_sentence(fault: &str) -> &'static str {
    match fault {
        "stuck_low" => {
            "The bridge sees the CEC line held low, so no device can talk. Usually a cable or a device mid-reboot."
        }
        "no_pullup" => {
            "The bridge read the CEC line low at boot: no pull-up, so nothing it sends can reach the devices."
        }
        _ => "The bridge reports a CEC line fault.",
    }
}

pub(crate) fn opt<T: std::fmt::Display>(value: &Option<T>) -> String {
    value
        .as_ref()
        .map_or_else(|| "—".to_owned(), ToString::to_string)
}

pub(crate) fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn opcode(opcode: &Option<u8>) -> String {
    opcode.map_or_else(|| "··".to_owned(), |opcode| format!("{opcode:02X}"))
}

pub(crate) fn direction(direction: &FrameDirection) -> &'static str {
    match direction {
        FrameDirection::Received => "rx",
        FrameDirection::Sent => "tx",
        FrameDirection::Local => "local",
    }
}

pub(crate) fn role_power(power: &RolePower) -> &'static str {
    match power {
        RolePower::On => "on",
        RolePower::Off => "off",
    }
}

pub(crate) fn transition(from: &Option<RolePower>, to: &Option<RolePower>) -> String {
    match (from, to) {
        (None, None) => String::new(),
        (from, to) => format!(
            "{} → {}",
            from.as_ref().map_or("?", role_power),
            to.as_ref().map_or("?", role_power)
        ),
    }
}

pub(crate) fn bus_power(power: &BusPower) -> &'static str {
    match power {
        BusPower::Unknown => "unknown",
        BusPower::On => "on",
        BusPower::Standby => "standby",
        BusPower::Transition => "transition",
    }
}

/// CEC device type names (UI only).
pub(crate) fn device_type(device_type: &Option<u8>) -> String {
    match *device_type {
        None => "—".to_owned(),
        Some(0) => "tv".to_owned(),
        Some(1) => "recording".to_owned(),
        Some(3) => "tuner".to_owned(),
        Some(4) => "playback".to_owned(),
        Some(5) => "audio".to_owned(),
        Some(6) => "switch".to_owned(),
        Some(7) => "processor".to_owned(),
        Some(other) => format!("type {other}"),
    }
}

/// A Home Assistant attribute as plain text (strings unquoted).
pub(crate) fn ha_attribute(entity: &HaEntityView, key: &str) -> Option<String> {
    entity.attributes.get(key).map(|value| match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    })
}

/// The drift detail of a `drift` / `drift_resolved` row; `None` for any other reason, so a
/// stray detail never replaces an ordinary row's reason.
pub(crate) fn drift_row(decision: &DecisionView) -> Option<&DriftDetail> {
    decision
        .drift
        .as_ref()
        .filter(|_| matches!(decision.reason.as_str(), "drift" | "drift_resolved"))
}

/// How long a resolved drift lasted (row time minus onset); `None` for an onset row.
pub(crate) fn drift_lasted(decision: &DecisionView) -> Option<String> {
    decision
        .drift
        .as_ref()
        .filter(|_| decision.reason == "drift_resolved")
        .map(|drift| duration(decision.timestamp - drift.since))
}

/// How long an open drift has held so far.
pub(crate) fn open_for(since: &DateTime<Utc>) -> String {
    duration(Utc::now() - *since)
}

/// Tenths of a second under a minute (like expectation latency, truncated so it never reads
/// `60.0 s`), then minutes and seconds, then the two most significant units. Negative spans
/// clamp to zero.
pub(crate) fn duration(span: chrono::TimeDelta) -> String {
    let millis = u64::try_from(span.num_milliseconds()).unwrap_or(0);
    let seconds = millis / 1000;
    match seconds {
        0..60 => format!("{seconds}.{} s", millis % 1000 / 100),
        60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => uptime(&seconds),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::{
        action::{Action, Via},
        state::AppState,
    };

    #[test]
    fn when_names_the_day_unless_it_is_today() {
        let now = Utc.with_ymd_and_hms(2026, 11, 7, 0, 30, 0).unwrap();
        let at = |day, hour| Utc.with_ymd_and_hms(2026, 11, day, hour, 5, 9).unwrap();
        assert_eq!(when_at(at(7, 0), now, "%H:%M:%S"), "00:05:09");
        // An hour ago, but across midnight.
        assert_eq!(when_at(at(6, 23), now, "%H:%M:%S"), "yesterday 23:05:09");
        assert_eq!(when_at(at(5, 23), now, "%H:%M:%S"), "11-05 23:05:09");
    }

    #[test]
    fn ago_buckets_and_clamps_future_timestamps() {
        let now = Utc.with_ymd_and_hms(2026, 10, 25, 12, 0, 0).unwrap();
        let at = |seconds| now - chrono::Duration::seconds(seconds);
        assert_eq!(ago_at(at(12), now), "12s ago");
        assert_eq!(ago_at(at(59), now), "59s ago");
        assert_eq!(ago_at(at(60), now), "1m ago");
        assert_eq!(ago_at(at(120), now), "2m ago");
        assert_eq!(ago_at(at(3_599), now), "59m ago");
        assert_eq!(ago_at(at(3_600), now), "1h ago");
        assert_eq!(ago_at(at(86_400 * 3), now), "3d ago");
        assert_eq!(ago_at(at(-5), now), "0s ago");
    }

    #[test]
    fn uptime_keeps_the_two_most_significant_units() {
        assert_eq!(uptime(&0), "0s");
        assert_eq!(uptime(&48), "48s");
        assert_eq!(uptime(&60), "1m");
        assert_eq!(uptime(&(25 * 60 + 59)), "25m");
        assert_eq!(uptime(&(6 * 3600 + 25 * 60 + 7)), "6h 25m");
        assert_eq!(uptime(&3600), "1h 0m");
        assert_eq!(uptime(&282_348), "3d 6h");
    }

    #[test]
    fn latency_switches_to_truncated_tenths_at_one_second() {
        assert_eq!(latency(&0), "0 ms");
        assert_eq!(latency(&999), "999 ms");
        assert_eq!(latency(&1_000), "1.0 s");
        assert_eq!(latency(&7_471), "7.4 s");
        assert_eq!(latency(&59_999), "59.9 s");
    }

    /// Whether telemetry `age_s` old with `signal_dbm`, under the adapter's `stale_after_s`,
    /// is degraded on a fixed clock.
    fn degraded(signal_dbm: i32, age_s: i64, stale_after_s: u64) -> bool {
        let now = Utc.with_ymd_and_hms(2026, 10, 30, 12, 0, 0).unwrap();
        let telemetry = BridgeTelemetry {
            signal_dbm,
            link_count: 1,
            uptime_s: 60,
            at: now - chrono::Duration::seconds(age_s),
            stale_after_s,
        };
        telemetry_degraded_at(&telemetry, now)
    }

    #[test]
    fn telemetry_is_degraded_by_weak_signal_or_age_past_its_stale_after() {
        // The age threshold is the adapter's, not a constant of the page.
        for stale_after in [30, 900] {
            let age = i64::try_from(stale_after).unwrap();
            assert!(!degraded(-75, age - 1, stale_after));
            assert!(!degraded(-75, age, stale_after));
            assert!(degraded(-50, age + 1, stale_after));
            assert!(degraded(-76, 0, stale_after));
        }
        // The same age is stale under one adapter's threshold and fresh under another's.
        assert!(degraded(-75, 31, 30));
        assert!(!degraded(-75, 31, 900));
        // A threshold beyond any representable age never flags (and never panics).
        assert!(!degraded(-75, 86_400 * 365, u64::MAX));
    }

    #[test]
    fn zero_stale_after_disables_the_age_check_but_not_the_signal_check() {
        let week = 7 * 86_400;
        assert!(!degraded(-50, week, 0));
        assert!(degraded(-80, week, 0));
    }

    #[test]
    fn duration_switches_units_at_the_minute_and_hour() {
        let ms = chrono::TimeDelta::milliseconds;
        assert_eq!(duration(ms(0)), "0.0 s");
        assert_eq!(duration(ms(3_480)), "3.4 s");
        assert_eq!(duration(ms(59_999)), "59.9 s");
        assert_eq!(duration(ms(60_000)), "1m 0s");
        assert_eq!(duration(ms(3_599_999)), "59m 59s");
        assert_eq!(duration(ms(3_600_000)), "1h 0m");
        assert_eq!(duration(ms(90_061_000)), "1d 1h");
        assert_eq!(duration(ms(-5_000)), "0.0 s");
    }

    /// The snapshot of a freshly started service: warming, shadow, nothing known.
    pub(super) fn snapshot() -> Snapshot {
        let text = r#"
mode = "shadow"
[mqtt]
host = "mqtt.test"
port = 1883
username = "choregos"
client_id = "choregos-test"
[bridge]
kind = "aether"
node = "cec-bridge"
logical_address = 0
[home_assistant]
url = "http://ha.test:8123"
[tv]
entity = "media_player.test_tv"
wake = { kind = "wol", mac = "02:00:00:00:00:01", broadcast = "192.0.2.255", port = 9 }
[receiver]
entity = "media_player.test_receiver"
select_source = true
[presets]
timezone = "UTC"
[[presets.slot]]
from = "00:00"
to = "24:00"
script = "script.test_preset"
[sonos]
entity = "media_player.test_speakers"
mute_switch = "switch.test_mute"
volume_from = []
[timing]
off_delay_s = 3
takeover_settle_ms = 2000
presence_poll_s = 5
presence_misses = 2
identify_retry_s = 10
[fake_tv]
power_status = "always_on"
cec_version = "1.4"
osd_name = "Choregos"
[[device]]
role = "streamer"
name = "Streamer"
kind = "playback"
match = { osd = "Streamer" }
source = "Streamer"
on = ["active_source"]
off = ["standby"]
takeover = "cec_standby"
presence = "events"
media_player = "media_player.test_streamer"
[[device]]
role = "console"
name = "Console"
kind = "playback"
match = { osd = "Console" }
source = "Console"
on = ["active_source"]
off = ["standby"]
takeover = "none"
presence = "events"
[web]
bind = "127.0.0.1:0"
public_url = "https://choregos.example.com/"
[web.oidc]
issuer = "http://127.0.0.1:9/"
client_id = "choregos-test"
[web.local_login]
username = "admin"
"#;
        let mut config = crate::config::Config::parse(text).expect("valid test config");
        config
            .resolve_secrets(|name| Some(format!("test-only-{name}-0123456789abcdef")))
            .expect("secrets resolve");
        AppState::new(config, text.to_owned()).snapshot()
    }

    /// The room is on, active, settled and consistent.
    fn settled() -> Snapshot {
        let mut snapshot = snapshot();
        snapshot.warming = false;
        snapshot.mode = Mode::Active;
        snapshot.intent.power = RoomPower::On;
        snapshot
    }

    fn drift(subject: &str) -> DriftDetail {
        DriftDetail {
            subject: subject.to_owned(),
            field: Field::Power,
            intended: "on".to_owned(),
            observed: "off".to_owned(),
            via: Via::Ha,
            since: Utc::now(),
        }
    }

    #[test]
    fn consistency_rule_takes_the_first_matching_state() {
        let fresh = snapshot();
        assert!(fresh.warming && fresh.mode == Mode::Shadow);
        assert_eq!(consistency(&fresh), Consistency::Warming);

        let mut shadow = settled();
        shadow.mode = Mode::Shadow;
        shadow.drift = vec![drift("tv")];
        assert_eq!(consistency(&shadow), Consistency::NotChecked);

        let mut no_intent = settled();
        no_intent.intent.power = RoomPower::Unknown;
        no_intent.drift = vec![drift("tv")];
        assert_eq!(
            consistency(&no_intent),
            Consistency::Unknown("no intent yet")
        );

        // Drift outranks an unreachable TV.
        let mut drifting = settled();
        drifting.drift = vec![drift("receiver"), drift("tv")];
        drifting.tv_direct = Some((DirectPower::Unreachable, Utc::now()));
        let state = consistency(&drifting);
        assert_eq!(state.label(), "drift: receiver.power");
        assert_eq!(state.tone(), "warn");
        let title = state.title();
        assert!(
            title.contains("receiver.power") && title.contains("tv.power"),
            "{title}"
        );

        let mut unreachable = settled();
        unreachable.tv_direct = Some((DirectPower::Unreachable, Utc::now()));
        assert_eq!(
            consistency(&unreachable),
            Consistency::Unknown("TV unreachable")
        );
        // An unreachable TV is expected while the room is off.
        unreachable.intent.power = RoomPower::Off;
        assert_eq!(consistency(&unreachable), Consistency::Consistent);

        assert_eq!(consistency(&settled()).label(), "consistent");
    }

    #[test]
    fn every_reason_literal_has_a_group_and_a_sentence() {
        use ReasonGroup::*;
        // The contract: every reason the engine records (data-model §2.3) plus the three
        // pre-v0.3.0 standby-verification literals that restored rings still carry.
        let literals = [
            ("ImageViewOn", Wake),
            ("TextViewOn", Wake),
            ("ActiveSource", Wake),
            ("PowerOn", Wake),
            ("ReportPhysicalAddress", Wake),
            ("manual_takeover", Wake),
            ("resync_complete", Wake),
            ("console_start", Wake),
            ("Standby", Standby),
            ("InactiveSource", Standby),
            ("PowerStandby", Standby),
            ("manual_standby", Standby),
            ("all_off", Standby),
            ("presence_timeout", Standby),
            ("taken_over", Standby),
            ("late_announcement", Standby),
            ("console_start_expired", Standby),
            ("console_start_retracted", Standby),
            ("standby_retry", Standby),
            ("standby_unconfirmed", Standby),
            ("standby_target_absent", Standby),
            ("takeover_settled", Takeover),
            ("off_timer_expired", Timer),
            ("identity_lost", Identity),
            ("conflict", Identity),
            ("conflict_resolved", Identity),
            ("command_identify", Identity),
            ("command_forget", Identity),
            ("drift", Drift),
            ("drift_resolved", Drift),
            ("command_all_off", Command),
            ("command_set_mode", Command),
            ("command_send_raw", Command),
            ("command_takeover", Command),
            ("command_standby", Command),
            ("service_started", Service),
            ("startup_source_ignored", Service),
            ("sonos_volume", Other),
        ];
        assert_eq!(literals.len(), 38);
        for (literal, group) in literals {
            assert_eq!(reason_group(literal), group, "{literal}");
            let sentence = reason_sentence(literal).unwrap_or_else(|| panic!("{literal}"));
            assert!(sentence.ends_with('.'), "{literal}: {sentence}");
        }
        assert_eq!(
            REASONS.len(),
            literals.len(),
            "no entry outside the contract"
        );
        assert_eq!(reason_group("something_new"), Other);
        assert_eq!(reason_sentence("something_new"), None);
    }

    #[test]
    fn action_summary_counts_by_destination_and_ignores_housekeeping() {
        let json = |action: &Action| serde_json::to_string(action).unwrap();
        let actions = [
            json(&Action::WakeTv),
            format!("would: {}", json(&Action::TvOff)),
            json(&Action::ha(
                "media_player",
                "turn_on",
                "media_player.test_receiver",
            )),
            format!(
                "would: {}",
                json(&Action::ha("script", "turn_on", "script.test"))
            ),
            json(&Action::CecSend {
                to: 4,
                data: vec![0x36],
            }),
            json(&Action::Identify {
                la: 4,
                queries: crate::action::IdentifyQueries::ALL,
            }),
            json(&Action::Resync),
            json(&Action::PresencePoll { la: 4 }),
            "not json".to_owned(),
        ];
        let summary = ActionSummary::from_actions(&actions);
        assert_eq!(
            summary,
            ActionSummary {
                tv: true,
                ha: 2,
                cec: 1
            }
        );
        assert_eq!(summary.text(), "TV · 2 HA · 1 CEC");
        assert_eq!(
            ActionSummary::from_actions(&[json(&Action::Resync)]).text(),
            "—"
        );
    }

    fn expectation(subject: &str, expect: &str, outcome: Outcome) -> Expectation {
        Expectation {
            subject: subject.to_owned(),
            field: Field::Power,
            expect: expect.to_owned(),
            via: vec![Via::TvDirect],
            deadline_ms: 45_000,
            attempts: 0,
            outcome,
        }
    }

    fn met(latency_ms: u64) -> Outcome {
        Outcome::Met {
            via: Via::TvDirect,
            observed: "on".to_owned(),
            latency_ms,
        }
    }

    pub(super) fn row(id: u64, role: Option<&str>, expectations: Vec<Expectation>) -> DecisionView {
        DecisionView {
            id,
            trigger: Trigger::Service {
                name: ServiceKind::Started,
            },
            parent: None,
            timestamp: Utc::now(),
            role: role.map(str::to_owned),
            from: Some(RolePower::Off),
            to: Some(RolePower::On),
            reason: "PowerOn".to_owned(),
            actions: vec![],
            would: false,
            expectations,
            verdict: Verdict::Achieved,
            drift: None,
        }
    }

    #[test]
    fn latencies_take_met_outcomes_and_skip_already_satisfied_wakes() {
        let wake = row(
            1,
            Some("streamer"),
            vec![
                expectation("tv", "on", met(7_471)),
                expectation("receiver", "on", met(9_000)),
                expectation(
                    "role:console",
                    "standby",
                    Outcome::Unknown { observed: None },
                ),
            ],
        );
        assert_eq!(max_latency(&wake), Some(9_000));
        assert_eq!(wake_latency(&wake), Some(7_471));
        assert_eq!(wake_line(&wake).as_deref(), Some("Wake → picture 7.4 s"));

        let already = row(2, None, vec![expectation("tv", "on", met(0))]);
        assert_eq!(wake_latency(&already), None);
        assert_eq!(max_latency(&already), Some(0));

        let turning_off = row(3, None, vec![expectation("tv", "off", met(6_000))]);
        assert_eq!(wake_latency(&turning_off), None);
        assert_eq!(wake_line(&turning_off), None);

        let measuring = row(4, None, vec![expectation("tv", "on", Outcome::Pending)]);
        assert_eq!(max_latency(&measuring), None);
        assert_eq!(
            wake_line(&measuring).as_deref(),
            Some("Wake → picture: measuring (#4 pending)")
        );
    }

    #[test]
    fn standby_line_reads_the_role_expectation_and_its_retries() {
        let mut absent = expectation(
            "role:console",
            "standby",
            Outcome::Unknown {
                observed: Some("absent".to_owned()),
            },
        );
        absent.attempts = 2;
        let wake = row(5, Some("streamer"), vec![absent]);
        assert_eq!(
            standby_line(&wake, "console").as_deref(),
            Some("Standby unconfirmed: never acknowledged on the bus · 2 retries")
        );
        assert_eq!(standby_line(&wake, "streamer"), None);
    }

    #[test]
    fn ring_lookups_find_the_newest_row_pending_rows_and_the_wake_behind_a_settle() {
        let mut snapshot = settled();
        let mut pending_wake = row(10, Some("streamer"), vec![]);
        pending_wake.verdict = Verdict::Pending;
        let mut settle = row(11, Some("streamer"), vec![]);
        settle.to = None;
        settle.from = None;
        settle.reason = "takeover_settled".to_owned();
        settle.parent = Some(10);
        let mut other_pending = row(12, Some("console"), vec![]);
        other_pending.verdict = Verdict::Pending;
        snapshot.decisions.extend([
            row(9, Some("streamer"), vec![]),
            pending_wake,
            settle,
            other_pending,
        ]);

        assert_eq!(
            last_decision_for(&snapshot, "streamer").map(|d| d.id),
            Some(11)
        );
        assert_eq!(last_decision_for(&snapshot, "nobody"), None);
        let ids: Vec<u64> = pending(&snapshot).iter().map(|d| d.id).collect();
        assert_eq!(ids, [12, 10]);
        let settle = last_decision_for(&snapshot, "streamer").unwrap();
        assert_eq!(wake_row(&snapshot, settle).map(|d| d.id), Some(10));
    }

    #[test]
    fn ha_entity_follows_the_roles_media_player() {
        let mut snapshot = settled();
        snapshot.ha.media_players[0].state = "playing".to_owned();
        let streamer = &snapshot.roles[0];
        assert_eq!(
            ha_entity_for(&snapshot, streamer).map(|e| e.state.as_str()),
            Some("playing")
        );
        let console = &snapshot.roles[1];
        assert!(console.media_player.is_none());
        assert!(ha_entity_for(&snapshot, console).is_none());
    }

    #[test]
    fn ha_power_reads_media_states_like_the_engine() {
        for state in ["on", "playing", "paused", "idle", "buffering"] {
            assert_eq!(ha_power(state), Some(true), "{state}");
        }
        for state in ["off", "standby"] {
            assert_eq!(ha_power(state), Some(false), "{state}");
        }
        for state in ["unknown", "unavailable"] {
            assert_eq!(ha_power(state), None, "{state}");
        }
    }
}
