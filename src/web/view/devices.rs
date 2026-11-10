//! Devices: registry rows, statistics over the decision ring, identity rows and events,
//! quirk sentences from a device profile, and agreement between truth sources.
//!
//! Statistics count only rows that did something: `would` rows (nothing was sent) and
//! migrated history (no verdict) are excluded everywhere. Latencies of 0 ms mean the state
//! was already there when the decision was made, so they are not measurements.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Utc};

use super::{
    age, bound_entry, bus_power, drift_row, duration, ha_power, is_migrated, is_restored, latency,
    reason_sentence, recorder::outcome_word, role_identity, role_sources, transition,
};
use crate::{
    action::{Field, Outcome, Verdict, Via},
    config::{
        Config, DeviceConfig, DeviceKind, MatchConfig, PowerEvent, Presence, ProbeConfig,
        ReceiverObserveConfig, StartWake, Takeover, TvOffConfig, WakeConfig, parse_physical,
    },
    state::{
        BusEntryView, BusPower, ConsoleState, DecisionView, DirectPower, Mode, RolePower, RoleView,
        Snapshot,
    },
};

/// Restored identities unheard this long are dropped from the state file. The runtime's
/// identity lifetime, restated so the UI does not reach into runtime internals.
pub(crate) const IDENTITY_LIFETIME_DAYS: u32 = 30;

/// p95 is hidden below this many samples.
pub(crate) const P95_MIN_N: usize = 5;

/// Rows in "Decisions involving …".
pub(crate) const INVOLVED_ROWS: usize = 10;

// ---- statistics ---------------------------------------------------------------------------

/// Nearest-rank percentile of an ascending sample: `sorted[ceil(p·n/100) − 1]`; `None` when
/// the sample is empty.
pub(crate) fn percentile(sorted: &[u64], p: usize) -> Option<u64> {
    let n = sorted.len();
    let rank = (p * n).div_ceil(100).clamp(1, n.max(1));
    sorted.get(rank - 1).copied()
}

/// A latency distribution with the sources its samples came from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub(crate) n: usize,
    pub(crate) p50: Option<u64>,
    /// `None` under [`P95_MIN_N`] samples.
    pub(crate) p95: Option<u64>,
    pub(crate) via: Vec<Via>,
}

impl Stats {
    /// Latency samples, restricted to those via `prefer` when any came that way.
    pub(crate) fn from_samples(samples: &[(u64, Via)], prefer: Option<Via>) -> Self {
        let prefer = prefer.filter(|wanted| samples.iter().any(|(_, via)| via == wanted));
        let mut values = Vec::with_capacity(samples.len());
        let mut vias = Vec::new();
        for &(millis, via) in samples {
            if prefer.is_some_and(|wanted| wanted != via) {
                continue;
            }
            values.push(millis);
            if !vias.contains(&via) {
                vias.push(via);
            }
        }
        values.sort_unstable();
        Self {
            n: values.len(),
            p50: percentile(&values, 50),
            p95: if values.len() >= P95_MIN_N {
                percentile(&values, 95)
            } else {
                None
            },
            via: vias,
        }
    }

    /// `p50 2.0 s`, or `—` without samples.
    pub(crate) fn p50_text(&self) -> String {
        self.p50
            .map_or_else(|| "—".to_owned(), |p50| format!("p50 {}", latency(&p50)))
    }

    /// `p50 18.4 s · p95 21.0 s`, `p50 18.3 s · p95 hidden`, or `—`.
    pub(crate) fn text(&self) -> String {
        match (self.p50, self.p95) {
            (None, _) => "—".to_owned(),
            (Some(p50), None) => format!("p50 {} · p95 hidden", latency(&p50)),
            (Some(p50), Some(p95)) => format!("p50 {} · p95 {}", latency(&p50), latency(&p95)),
        }
    }

    pub(crate) fn via_text(&self) -> String {
        via_list(&self.via)
    }
}

fn via_list(vias: &[Via]) -> String {
    if vias.is_empty() {
        "—".to_owned()
    } else {
        vias.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// Rows that count towards statistics: neither `would` nor migrated history.
fn counted(decision: &DecisionView) -> bool {
    !decision.would && !is_migrated(decision)
}

/// Met latencies (> 0 ms) of the expectations on `subject.field` (= `expect`, when given).
fn collect_met(
    samples: &mut Vec<(u64, Via)>,
    decision: &DecisionView,
    subject: &str,
    field: Field,
    expect: Option<&str>,
) {
    samples.extend(
        decision
            .expectations
            .iter()
            .filter(|e| {
                e.subject == subject && e.field == field && expect.is_none_or(|v| e.expect == v)
            })
            .filter_map(|e| match e.outcome {
                Outcome::Met {
                    via, latency_ms, ..
                } if latency_ms > 0 => Some((latency_ms, via)),
                _ => None,
            }),
    );
}

fn met_stats(
    ring: &VecDeque<DecisionView>,
    subject: &str,
    field: Field,
    expect: Option<&str>,
    prefer: Option<Via>,
) -> Stats {
    let mut samples = Vec::new();
    for decision in ring.iter().filter(|decision| counted(decision)) {
        collect_met(&mut samples, decision, subject, field, expect);
    }
    Stats::from_samples(&samples, prefer)
}

/// Standby verifications of one role (`role:<r>` power expectations), settled ones only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StandbyStats {
    pub(crate) total: usize,
    /// Met or CEC only.
    pub(crate) confirmed: usize,
    pub(crate) missed: usize,
    pub(crate) unknown: usize,
    /// Every source the verifications were allowed to use.
    pub(crate) via: Vec<Via>,
}

/// How one role behaves, over the ring.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RoleStats {
    /// Settled wakes: rows of the role turning on with a verdict other than none or pending.
    pub(crate) wakes: usize,
    pub(crate) achieved: usize,
    /// `tv.power = on` met on those wakes, `tv_direct` samples preferred.
    pub(crate) picture: Stats,
    /// `receiver.power = on` met on those wakes.
    pub(crate) receiver: Stats,
    /// `receiver.source` met on the wakes' `takeover_settled` children.
    pub(crate) source: Stats,
    pub(crate) standby: StandbyStats,
    /// `presence_timeout` rows, for a polled role.
    pub(crate) left_bus: Option<usize>,
}

pub(crate) fn role_stats(ring: &VecDeque<DecisionView>, role: &str, polled: bool) -> RoleStats {
    let rows = || ring.iter().filter(|decision| counted(decision));
    let mine = |decision: &DecisionView| decision.role.as_deref() == Some(role);
    let wakes: Vec<&DecisionView> = rows()
        .filter(|decision| {
            mine(decision)
                && decision.to == Some(RolePower::On)
                && !matches!(decision.verdict, Verdict::None | Verdict::Pending)
        })
        .collect();
    let ids: BTreeSet<u64> = wakes.iter().map(|wake| wake.id).collect();
    let (mut picture, mut receiver, mut source) = (Vec::new(), Vec::new(), Vec::new());
    for wake in &wakes {
        collect_met(&mut picture, wake, "tv", Field::Power, Some("on"));
        collect_met(&mut receiver, wake, "receiver", Field::Power, Some("on"));
    }
    for child in rows().filter(|decision| {
        decision.reason == "takeover_settled" && decision.parent.is_some_and(|p| ids.contains(&p))
    }) {
        collect_met(&mut source, child, "receiver", Field::Source, None);
    }

    let subject = format!("role:{role}");
    let mut standby = StandbyStats::default();
    for expectation in rows()
        .flat_map(|decision| &decision.expectations)
        .filter(|e| e.subject == subject && e.field == Field::Power)
    {
        match expectation.outcome {
            Outcome::Pending => continue,
            Outcome::Met { .. } | Outcome::CecOnly { .. } => standby.confirmed += 1,
            Outcome::Missed { .. } => standby.missed += 1,
            Outcome::Unknown { .. } => standby.unknown += 1,
        }
        standby.total += 1;
        for via in &expectation.via {
            if !standby.via.contains(via) {
                standby.via.push(*via);
            }
        }
    }

    RoleStats {
        wakes: wakes.len(),
        achieved: wakes
            .iter()
            .filter(|wake| wake.verdict == Verdict::Achieved)
            .count(),
        picture: Stats::from_samples(&picture, Some(Via::TvDirect)),
        receiver: Stats::from_samples(&receiver, None),
        source: Stats::from_samples(&source, None),
        standby,
        left_bus: polled.then(|| {
            rows()
                .filter(|decision| mine(decision) && decision.reason == "presence_timeout")
                .count()
        }),
    }
}

/// One row of a statistics table.
pub(crate) struct Measure {
    pub(crate) name: String,
    pub(crate) value: String,
    /// `(tone, text)` beside the value.
    pub(crate) chip: Option<(&'static str, String)>,
    pub(crate) n: usize,
    pub(crate) source: String,
    pub(crate) read_as: String,
}

impl Measure {
    fn latency(name: impl Into<String>, value: String, stats: &Stats, read_as: &str) -> Self {
        Self {
            name: name.into(),
            value,
            chip: None,
            n: stats.n,
            source: stats.via_text(),
            read_as: read_as.to_owned(),
        }
    }
}

/// The "How it behaves" table of a role; `source` is its receiver input.
pub(crate) fn role_measures(stats: &RoleStats, source: &str) -> Vec<Measure> {
    let standby = &stats.standby;
    let mut measures = vec![
        Measure {
            name: "Wakes".to_owned(),
            value: if stats.wakes == 0 {
                "—".to_owned()
            } else {
                format!("{} / {} achieved", stats.achieved, stats.wakes)
            },
            chip: None,
            n: stats.wakes,
            source: "verdicts".to_owned(),
            read_as: "Settled wakes that reached every expected state.".to_owned(),
        },
        Measure::latency(
            "Wake → picture",
            stats.picture.text(),
            &stats.picture,
            "Time from the trigger until the TV reported on.",
        ),
        Measure::latency(
            "Receiver ready",
            stats.receiver.p50_text(),
            &stats.receiver,
            "Time from the trigger until the receiver reported on.",
        ),
        Measure::latency(
            "Source switch",
            stats.source.p50_text(),
            &stats.source,
            &format!("Time from the settle until the receiver input read \"{source}\"."),
        ),
        Measure {
            name: "Standby confirmed".to_owned(),
            value: match (standby.total, standby.unknown) {
                (0, _) => "—".to_owned(),
                (total, 0) => format!("{} of {total}", standby.confirmed),
                (total, unknown) => format!("{} of {total} · {unknown} unconfirmed", standby.confirmed),
            },
            chip: (standby.missed > 0).then(|| ("bad", format!("{} missed", standby.missed))),
            n: standby.total,
            source: via_list(&standby.via),
            read_as: "After a takeover or standby, how often it was confirmed asleep (met or CEC only); missed means a source still saw it awake.".to_owned(),
        },
    ];
    if let Some(left) = stats.left_bus {
        measures.push(Measure {
            name: "Left the bus".to_owned(),
            value: format!("{left}×"),
            chip: None,
            n: left,
            source: "cec".to_owned(),
            read_as: "Times its presence polls went unanswered and it was marked off.".to_owned(),
        });
    }
    measures
}

/// The outputs Choregos drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputKind {
    Tv,
    Receiver,
    Speakers,
}

impl OutputKind {
    pub(crate) fn from_path(segment: &str) -> Option<Self> {
        match segment {
            "tv" => Some(Self::Tv),
            "receiver" => Some(Self::Receiver),
            "speakers" => Some(Self::Speakers),
            _ => None,
        }
    }

    /// The expectation and drift subject.
    pub(crate) fn subject(self) -> &'static str {
        match self {
            Self::Tv => "tv",
            Self::Receiver => "receiver",
            Self::Speakers => "speakers",
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Tv => "TV",
            Self::Receiver => "Receiver",
            Self::Speakers => "Speakers",
        }
    }
}

/// The timings table of an output.
pub(crate) fn output_measures(snapshot: &Snapshot, kind: OutputKind) -> Vec<Measure> {
    let ring = &snapshot.decisions;
    match kind {
        OutputKind::Tv => {
            let off = met_stats(ring, "tv", Field::Power, Some("off"), Some(Via::TvDirect));
            let mut measures = vec![Measure::latency(
                "Off → confirmed off",
                off.p50_text(),
                &off,
                "",
            )];
            measures.extend(snapshot.roles.iter().map(|role| {
                let picture = role_stats(ring, &role.role, false).picture;
                Measure::latency(
                    format!("Wake → picture, {}", role.name),
                    picture.text(),
                    &picture,
                    "",
                )
            }));
            measures
        }
        OutputKind::Receiver => {
            let on = met_stats(ring, "receiver", Field::Power, Some("on"), None);
            let off = met_stats(ring, "receiver", Field::Power, Some("off"), None);
            let source = met_stats(ring, "receiver", Field::Source, None, None);
            vec![
                Measure::latency("Turn on → on", on.p50_text(), &on, ""),
                Measure::latency("Turn off → off", off.text(), &off, ""),
                Measure::latency("Source switch", source.p50_text(), &source, ""),
            ]
        }
        OutputKind::Speakers => {
            let stopped = met_stats(ring, "speakers", Field::Playback, Some("stopped"), None);
            vec![Measure::latency(
                "Playback stopped",
                stopped.p50_text(),
                &stopped,
                "",
            )]
        }
    }
}

// ---- agreement ----------------------------------------------------------------------------

/// Whether the truth sources with evidence say the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Agreement {
    Agree,
    Disagree,
    /// Fewer than two sources have evidence.
    Alone,
}

impl Agreement {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Agree => "agree",
            Self::Disagree => "disagree",
            Self::Alone => "one source",
        }
    }

    pub(crate) fn tone(self) -> &'static str {
        match self {
            Self::Agree => "ok",
            Self::Disagree => "warn",
            Self::Alone => "dim",
        }
    }
}

/// Normalised power readings (`Some(on)`), `None` for a source without evidence.
pub(crate) fn agreement(readings: &[Option<bool>]) -> Agreement {
    let mut known = readings.iter().flatten();
    let Some(first) = known.next() else {
        return Agreement::Alone;
    };
    let mut compared = false;
    for reading in known {
        if reading != first {
            return Agreement::Disagree;
        }
        compared = true;
    }
    if compared {
        Agreement::Agree
    } else {
        Agreement::Alone
    }
}

/// CEC power: on, standby; a transition or nothing heard is not evidence.
pub(crate) fn bus_reading(power: BusPower) -> Option<bool> {
    match power {
        BusPower::On => Some(true),
        BusPower::Standby => Some(false),
        BusPower::Unknown | BusPower::Transition => None,
    }
}

/// Probe: awake, standby; unreachable is not evidence.
pub(crate) fn probe_reading(state: ConsoleState) -> Option<bool> {
    match state {
        ConsoleState::Awake => Some(true),
        ConsoleState::Standby => Some(false),
        ConsoleState::Unreachable => None,
    }
}

/// The TV leaves the network in standby: unreachable (webOS) and unavailable or unknown
/// (Home Assistant) read as off, as on the cockpit.
pub(crate) fn tv_direct_reading(state: DirectPower) -> Option<bool> {
    Some(state == DirectPower::On)
}

pub(crate) fn tv_ha_reading(state: &str) -> Option<bool> {
    match state {
        "unavailable" | "unknown" => Some(false),
        other => ha_power(other),
    }
}

/// Receiver over HTTP: unreachable is not evidence.
pub(crate) fn receiver_direct_reading(state: DirectPower) -> Option<bool> {
    match state {
        DirectPower::On => Some(true),
        DirectPower::Off => Some(false),
        DirectPower::Unreachable => None,
    }
}

/// The TV's power: webOS when sampled (never in shadow), else Home Assistant. `(on, used
/// source, its value)`.
pub(crate) fn tv_power(snapshot: &Snapshot) -> (Option<bool>, &'static str, String) {
    match snapshot.tv_direct {
        Some((state, _)) if snapshot.mode != Mode::Shadow => {
            (tv_direct_reading(state), "webos", state.to_string())
        }
        _ => (
            tv_ha_reading(&snapshot.ha.tv.state),
            "ha",
            snapshot.ha.tv.state.clone(),
        ),
    }
}

/// The receiver's power: Denon when sampled and reachable (never in shadow), else Home
/// Assistant.
pub(crate) fn receiver_power(snapshot: &Snapshot) -> (Option<bool>, &'static str, String) {
    match snapshot.receiver_direct {
        Some((state, _)) if snapshot.mode != Mode::Shadow && state != DirectPower::Unreachable => {
            (receiver_direct_reading(state), "denon", state.to_string())
        }
        _ => (
            ha_power(&snapshot.ha.receiver.state),
            "ha",
            snapshot.ha.receiver.state.clone(),
        ),
    }
}

/// The receiver's CEC entry: the first audio-system device on the bus.
pub(crate) fn audio_entry(snapshot: &Snapshot) -> Option<&BusEntryView> {
    snapshot
        .bus
        .iter()
        .find(|entry| entry.device_type == Some(5) && entry.matched_role.is_none())
}

// ---- identity -----------------------------------------------------------------------------

/// The name CEC gives a logical address.
pub(crate) fn la_label(la: u8) -> &'static str {
    match la {
        0 => "TV",
        1 => "recording device 1",
        2 => "recording device 2",
        3 => "tuner 1",
        4 => "playback device 1",
        5 => "audio system",
        6 => "tuner 2",
        7 => "tuner 3",
        8 => "playback device 2",
        9 => "recording device 3",
        10 => "tuner 4",
        11 => "playback device 3",
        12 | 13 => "reserved",
        14 => "specific use",
        _ => "broadcast",
    }
}

/// How one identity key compares with the role's match rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    Matches,
    NotRequired,
    Mismatch,
    /// The rule needs the key and the device has not reported it.
    Unreported,
}

impl Check {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Matches => "matches rule",
            Self::NotRequired => "not required by the rule",
            Self::Mismatch => "mismatch",
            Self::Unreported => "not reported",
        }
    }

    pub(crate) fn tone(self) -> &'static str {
        match self {
            Self::Matches => "ok",
            Self::NotRequired => "",
            Self::Mismatch => "bad",
            Self::Unreported => "dim",
        }
    }
}

/// One row of the identity panel.
pub(crate) struct IdentityRow {
    pub(crate) key: &'static str,
    pub(crate) value: String,
    pub(crate) check: Option<Check>,
}

/// The identity keys a role is known by (its bound or restored entry).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Keys {
    pub(crate) la: Option<u8>,
    pub(crate) physical: Option<String>,
    pub(crate) vendor: Option<String>,
    pub(crate) osd: Option<String>,
}

impl Keys {
    pub(crate) fn of(entry: &BusEntryView) -> Self {
        Self {
            la: Some(entry.logical_address),
            physical: entry.physical_address.clone(),
            vendor: entry.vendor.clone(),
            osd: entry.osd.clone(),
        }
    }

    pub(crate) fn of_role(role: &RoleView) -> Self {
        Self {
            la: role.logical_address,
            physical: role.physical_address.clone(),
            vendor: role.vendor.clone(),
            osd: role.osd.clone(),
        }
    }

    /// `LA 1 · 2.2.0.0 · 001582 · "Kodi"`.
    pub(crate) fn line(&self) -> String {
        format!(
            "LA {} · {} · {} · {}",
            self.la.map_or_else(|| "—".to_owned(), |la| la.to_string()),
            self.physical.as_deref().unwrap_or("—"),
            self.vendor.as_deref().unwrap_or("—"),
            self.osd
                .as_deref()
                .map_or_else(|| "—".to_owned(), |osd| format!("\"{osd}\""))
        )
    }
}

fn same_physical(rule: &str, value: &str) -> bool {
    match (parse_physical(rule), parse_physical(value)) {
        (Ok(rule), Ok(value)) => rule == value,
        _ => rule.trim() == value.trim(),
    }
}

fn same_vendor(rule: &str, value: &str) -> bool {
    match (
        u32::from_str_radix(rule.trim(), 16),
        u32::from_str_radix(value.trim(), 16),
    ) {
        (Ok(rule), Ok(value)) => rule == value,
        _ => rule.trim().eq_ignore_ascii_case(value.trim()),
    }
}

fn same_osd(rule: &str, value: &str) -> bool {
    rule.trim() == value.trim()
}

fn check(rule: Option<&str>, value: Option<&str>, same: fn(&str, &str) -> bool) -> Check {
    match (rule, value) {
        (None, _) => Check::NotRequired,
        (Some(_), None) => Check::Unreported,
        (Some(rule), Some(value)) if same(rule, value) => Check::Matches,
        (Some(_), Some(_)) => Check::Mismatch,
    }
}

/// Logical, physical, OSD and vendor, each compared with the match rule.
pub(crate) fn identity_rows(rule: &MatchConfig, keys: &Keys) -> Vec<IdentityRow> {
    let shown = |value: &Option<String>| value.clone().unwrap_or_else(|| "—".to_owned());
    vec![
        IdentityRow {
            key: "Logical",
            value: keys
                .la
                .map_or_else(|| "—".to_owned(), |la| format!("{la} · {}", la_label(la))),
            check: None,
        },
        IdentityRow {
            key: "Physical",
            value: shown(&keys.physical),
            check: Some(check(
                rule.physical.as_deref(),
                keys.physical.as_deref(),
                same_physical,
            )),
        },
        IdentityRow {
            key: "OSD name",
            value: keys
                .osd
                .as_deref()
                .map_or_else(|| "—".to_owned(), |osd| format!("\"{osd}\"")),
            check: Some(check(rule.osd.as_deref(), keys.osd.as_deref(), same_osd)),
        },
        IdentityRow {
            key: "Vendor",
            value: shown(&keys.vendor),
            check: Some(check(
                rule.vendor.as_deref(),
                keys.vendor.as_deref(),
                same_vendor,
            )),
        },
    ]
}

/// Whether `entry` carries every key the rule configures, with the same value.
pub(crate) fn rule_matches(rule: &MatchConfig, entry: &BusEntryView) -> bool {
    let keys = Keys::of(entry);
    identity_rows(rule, &keys)
        .iter()
        .filter_map(|row| row.check)
        .all(|check| matches!(check, Check::Matches | Check::NotRequired))
}

/// The unverified entry a restored role waits on.
pub(crate) fn restored_entry<'a>(
    snapshot: &'a Snapshot,
    device: &DeviceConfig,
) -> Option<&'a BusEntryView> {
    snapshot.bus.iter().find(|entry| {
        !entry.verified
            && entry.logical_address != snapshot.bridge_logical_address
            && rule_matches(&device.matches, entry)
    })
}

/// The match rule as its TOML inline table: `match = { osd = "Kodi", physical = "2.2.0.0" }`.
pub(crate) fn match_rule(rule: &MatchConfig) -> String {
    let quoted = |value: &str| toml::Value::String(value.to_owned()).to_string();
    let keys: Vec<String> = [
        ("osd", &rule.osd),
        ("vendor", &rule.vendor),
        ("physical", &rule.physical),
    ]
    .into_iter()
    .filter_map(|(key, value)| {
        value
            .as_deref()
            .map(|value| format!("{key} = {}", quoted(value)))
    })
    .collect();
    format!("match = {{ {} }}", keys.join(", "))
}

/// One line of a role's identity history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IdentityEvent {
    pub(crate) at: DateTime<Utc>,
    pub(crate) word: String,
    pub(crate) tone: &'static str,
    pub(crate) sentence: String,
    /// The ring row, for recorded events (synthetic lines have none).
    pub(crate) id: Option<u64>,
}

/// The newest `service_started` row's time: when the running service restored its state.
pub(crate) fn service_started(snapshot: &Snapshot) -> Option<DateTime<Utc>> {
    snapshot
        .decisions
        .iter()
        .rev()
        .find(|decision| decision.reason == "service_started")
        .map(|decision| decision.timestamp)
}

/// Identity history of a role, newest first. Recorded: its `identity_lost` rows, plus
/// `conflict`, `conflict_resolved`, `command_identify` and `command_forget` rows (they carry
/// no role) whose trigger names an address of the role: its current or last one, its restored
/// entry's, one of a conflict it is part of, or one a loss row names. Synthetic: `verified`
/// at the bound entry's confirmation, `restored` at the service start when the identity came
/// from the state file. Address moves leave no row.
pub(crate) fn identity_events(
    snapshot: &Snapshot,
    role: &RoleView,
    entry: Option<&BusEntryView>,
) -> Vec<IdentityEvent> {
    let rows = || {
        snapshot
            .decisions
            .iter()
            .filter(|decision| !is_migrated(decision))
    };
    let lost = |decision: &DecisionView| {
        decision.reason == "identity_lost" && decision.role.as_deref() == Some(role.role.as_str())
    };
    let mut addresses: BTreeSet<u8> = role
        .logical_address
        .into_iter()
        .chain(role.last_logical_address)
        .chain(entry.map(|entry| entry.logical_address))
        .collect();
    addresses.extend(
        snapshot
            .conflicts
            .iter()
            .filter(|conflict| conflict.roles.contains(&role.role))
            .flat_map(|conflict| conflict.logical_addresses.iter().copied()),
    );
    addresses.extend(
        rows()
            .filter(|decision| lost(decision))
            .filter_map(DecisionView::trigger_logical_address),
    );

    let mut events: Vec<IdentityEvent> = rows()
        .filter(|decision| {
            lost(decision)
                || (matches!(
                    decision.reason.as_str(),
                    "conflict" | "conflict_resolved" | "command_identify" | "command_forget"
                ) && decision
                    .trigger_logical_address()
                    .is_some_and(|la| addresses.contains(&la)))
        })
        .map(|decision| {
            let sentence = reason_sentence(&decision.reason).unwrap_or_default();
            IdentityEvent {
                at: decision.timestamp,
                word: decision.reason.replacen("command_", "command ", 1),
                tone: match decision.reason.as_str() {
                    "identity_lost" => "warn",
                    "conflict" => "bad",
                    "conflict_resolved" => "ok",
                    _ => "dim",
                },
                sentence: match decision.trigger_logical_address() {
                    Some(la) => format!("LA {la}: {sentence}"),
                    None => sentence.to_owned(),
                },
                id: Some(decision.id),
            }
        })
        .collect();

    let started = service_started(snapshot);
    let restored_at = |at: DateTime<Utc>, sentence: String| IdentityEvent {
        at,
        word: "restored".to_owned(),
        tone: "pend",
        sentence,
        id: None,
    };
    match entry
        .filter(|entry| entry.verified)
        .and_then(|e| e.confirmed_at)
    {
        // Confirmed before the running service started: the state file brought it back and
        // vendor plus OSD re-confirmed it since (only a live 84 refreshes the time).
        Some(confirmed) if started.is_some_and(|started| confirmed < started) => {
            if let Some(started) = started {
                events.push(restored_at(
                    started,
                    "Read from the state file at start-up; re-confirmed by vendor and OSD since."
                        .to_owned(),
                ));
            }
        }
        Some(confirmed) => events.push(IdentityEvent {
            at: confirmed,
            word: "verified".to_owned(),
            tone: "ok",
            sentence: "A live Report Physical Address confirmed it; now bound.".to_owned(),
            id: None,
        }),
        None => {}
    }
    if is_restored(role)
        && let Some(started) = started
    {
        events.push(restored_at(
            started,
            format!(
                "Read from the state file at start-up; waiting for {} to talk.",
                role.name
            ),
        ));
    }
    events.sort_by_key(|event| std::cmp::Reverse(event.at));
    events
}

// ---- profile sentences --------------------------------------------------------------------

/// A piece of a sentence: plain text, or a configuration key or value set in code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    Text(String),
    Code(String),
}

/// A sentence made of [`Part`]s.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Sentence(pub(crate) Vec<Part>);

impl Sentence {
    fn text(mut self, text: impl Into<String>) -> Self {
        self.0.push(Part::Text(text.into()));
        self
    }

    fn code(mut self, code: impl Into<String>) -> Self {
        self.0.push(Part::Code(code.into()));
        self
    }

    /// `a`, `a and b`, `a, b and c`, each in code.
    fn codes(mut self, items: &[&str]) -> Self {
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                self = self.text(if index + 1 == items.len() {
                    " and "
                } else {
                    ", "
                });
            }
            self = self.code(*item);
        }
        self
    }

    /// Plain text with code in backticks.
    #[cfg(test)]
    pub(crate) fn plain(&self) -> String {
        self.0
            .iter()
            .map(|part| match part {
                Part::Text(text) => text.clone(),
                Part::Code(code) => format!("`{code}`"),
            })
            .collect()
    }
}

pub(crate) fn power_event_word(event: PowerEvent) -> &'static str {
    match event {
        PowerEvent::ImageViewOn => "image_view_on",
        PowerEvent::TextViewOn => "text_view_on",
        PowerEvent::ActiveSource => "active_source",
        PowerEvent::PowerOn => "power_on",
        PowerEvent::Standby => "standby",
        PowerEvent::InactiveSource => "inactive_source",
        PowerEvent::PowerStandby => "power_standby",
        PowerEvent::ReportPhysicalAddress => "report_physical_address",
    }
}

fn takeover_word(takeover: Takeover) -> &'static str {
    match takeover {
        Takeover::CecStandby => "cec_standby",
        Takeover::CecPauseStandby => "cec_pause_standby",
        Takeover::HaMediaStop => "ha_media_stop",
        Takeover::None => "none",
    }
}

/// What a device profile makes Choregos do, one sentence per setting group: power events,
/// takeover and standby verification, presence polling, the probe, the media player.
pub(crate) fn quirk_sentences(device: &DeviceConfig, config: &Config) -> Vec<Sentence> {
    let name = &device.name;
    let words = |events: &[PowerEvent]| -> Vec<&str> {
        events
            .iter()
            .map(|event| power_event_word(*event))
            .collect()
    };
    let mut sentences = Vec::new();

    let mut power = Sentence::default()
        .text(format!(
            "Choregos learns that {name} is on from its own CEC announcements: "
        ))
        .codes(&words(&device.on))
        .text(". ");
    power = if device.off.is_empty() {
        power.text("No CEC announcement turns it off.")
    } else {
        power
            .text("Off comes from ")
            .codes(&words(&device.off))
            .text(".")
    };
    sentences.push(power);

    let cec_takeover = matches!(
        device.takeover,
        Takeover::CecStandby | Takeover::CecPauseStandby
    );
    let takeover = Sentence::default().text(format!("When another player takes over, {name} "));
    let takeover = match device.takeover {
        Takeover::CecStandby => takeover.text("is sent CEC Standby ("),
        Takeover::CecPauseStandby => takeover.text("is sent Pause, then CEC Standby ("),
        Takeover::HaMediaStop => takeover.text("is stopped through Home Assistant ("),
        Takeover::None => takeover.text("is left alone ("),
    }
    .code(format!("takeover = {}", takeover_word(device.takeover)))
    .text(")");
    let by_cec = device.verify_standby && cec_takeover;
    let takeover = match (by_cec, device.probe.is_some()) {
        (true, true) => takeover
            .text(" and its CEC power status and the probe confirm it went to sleep (")
            .code("verify_standby")
            .text(")."),
        (true, false) => takeover
            .text(" and its CEC power status confirms it went to sleep (")
            .code("verify_standby")
            .text(")."),
        (false, true) => takeover.text(" and the probe confirms it went to sleep."),
        (false, false) if cec_takeover => takeover
            .text("; its sleep is not verified (")
            .code("verify_standby = false")
            .text(")."),
        (false, false) => takeover.text("; its sleep is not verified."),
    };
    sentences.push(takeover);

    if device.presence == Presence::Poll {
        sentences.push(
            Sentence::default()
                .text(format!(
                    "It leaves the bus in standby, so while it is on Choregos asks its power every {} s and marks it off after {} unanswered polls (",
                    config.timing.presence_poll_s, config.timing.presence_misses
                ))
                .code("presence = poll")
                .text(")."),
        );
    }

    if let Some(probe) = &device.probe {
        sentences.extend(probe_sentences(probe));
    }

    if let Some(player) = &device.media_player {
        let sentence = Sentence::default()
            .text("Home Assistant ")
            .code(player.clone());
        sentences.push(if device.takeover == Takeover::HaMediaStop {
            sentence.text(" is how it is stopped on takeover; its state is shown for comparison and never decides its power.")
        } else {
            sentence.text(" is shown for comparison only; it never decides its power.")
        });
    }
    sentences
}

fn probe_sentences(probe: &ProbeConfig) -> Vec<Sentence> {
    let start = match probe.start_wake() {
        StartWake::Off => Sentence::default()
            .text("No early wake; a start the probe saw before the active device is stopped when it announces late (")
            .code("start_wake = off")
            .text(")."),
        StartWake::Would => Sentence::default()
            .text("If the probe sees it wake without any CEC announcement, Choregos would wake the room but only logs it (")
            .code("start_wake = would")
            .text(")."),
        StartWake::Act => Sentence::default()
            .text("If the probe sees it wake without any CEC announcement, Choregos wakes the room (")
            .code("start_wake = act")
            .text(")."),
    };
    vec![
        start,
        Sentence::default()
            .text(format!(
                "After a wake it allows {} s to boot before judging (",
                probe.boot_s()
            ))
            .code(format!("boot_s = {}", probe.boot_s()))
            .text(format!(
                "); while idle the probe asks every {} s (",
                probe.idle_poll_s()
            ))
            .code(format!("idle_poll_s = {}", probe.idle_poll_s()))
            .text(")."),
    ]
}

/// How Choregos drives an output, from its configuration.
pub(crate) fn drive_sentences(kind: OutputKind, config: &Config) -> Vec<Sentence> {
    match kind {
        OutputKind::Tv => {
            let wake = match &config.tv.wake {
                WakeConfig::Wol { .. } => Sentence::default().text("Wakes it with Wake-on-LAN ("),
                WakeConfig::Http { token_env, .. } => Sentence::default()
                    .text("Wakes it with an HTTP request (token from env ")
                    .code(token_env.clone())
                    .text(", "),
            }
            .code("tv.wake")
            .text(")");
            let off = match &config.tv.off {
                TvOffConfig::Ha => wake
                    .text(" and turns it off through Home Assistant (")
                    .code("tv.off")
                    .text(")."),
                TvOffConfig::Webos { key_env, .. } => wake
                    .text(" and turns it off over webOS (")
                    .code("tv.off")
                    .text(", key from env ")
                    .code(key_env.clone())
                    .text(")."),
            };
            vec![
                off,
                Sentence::default()
                    .text("Both are read at start-up; change them in Config and restart."),
            ]
        }
        OutputKind::Receiver => {
            let source = if config.receiver.select_source {
                Sentence::default()
                    .text("Switches its input to the role's source once a wake settles (")
                    .code("select_source = true")
                    .text(").")
            } else {
                Sentence::default()
                    .text("Leaves the input to CEC active source (")
                    .code("select_source = false")
                    .text(").")
            };
            let observe = match &config.receiver.observe {
                Some(ReceiverObserveConfig::Denon { .. }) => Sentence::default()
                    .text("Power is read directly from its Denon HTTP status (")
                    .code("observe = denon")
                    .text("), Home Assistant as the fallback."),
                None => Sentence::default()
                    .text("Power is read from Home Assistant only (no ")
                    .code("receiver.observe")
                    .text(")."),
            };
            vec![
                Sentence::default()
                    .text("Turned on with a wake and off by the off timer through Home Assistant."),
                source,
                observe,
            ]
        }
        OutputKind::Speakers => {
            let names = &config.sonos.volume_from;
            let volume = if names.is_empty() {
                Sentence::default()
                    .text("No role forwards its volume keys (")
                    .code("volume_from = []")
                    .text(").")
            } else {
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                Sentence::default()
                    .text("Volume keys from ")
                    .codes(&names)
                    .text(" are forwarded; mute toggles ")
                    .code(config.sonos.mute_switch.clone())
                    .text(".")
            };
            vec![
                Sentence::default()
                    .text("Playback is stopped (")
                    .code("media_stop")
                    .text(") when a role wakes the room."),
                volume,
            ]
        }
    }
}

/// `player · input "Console B" · polled · probe`.
pub(crate) fn kind_label(device: &DeviceConfig) -> String {
    let mut parts = vec![
        match device.kind {
            DeviceKind::Playback => "player",
            DeviceKind::Recording => "recorder",
        }
        .to_owned(),
        format!("input \"{}\"", device.source),
    ];
    if device.presence == Presence::Poll {
        parts.push("polled".to_owned());
    }
    if device.probe.is_some() {
        parts.push("probe".to_owned());
    }
    parts.join(" · ")
}

// ---- decisions and drift ------------------------------------------------------------------

/// A row of "Decisions involving …".
pub(crate) struct Involved {
    pub(crate) decision: DecisionView,
    pub(crate) part: String,
    /// The standby verification outcome `(word, tone)`.
    pub(crate) outcome: Option<(String, &'static str)>,
}

/// The newest rows about `role` or verifying its standby, migrated history excluded.
pub(crate) fn involved(snapshot: &Snapshot, role: &str, limit: usize) -> Vec<Involved> {
    let subject = format!("role:{role}");
    snapshot
        .decisions
        .iter()
        .rev()
        .filter(|decision| !is_migrated(decision))
        .filter_map(|decision| {
            let mine = decision.role.as_deref() == Some(role);
            let standby = decision
                .expectations
                .iter()
                .find(|e| e.subject == subject && e.field == Field::Power);
            let change = transition(&decision.from, &decision.to);
            let (part, outcome) = if mine && decision.reason == "takeover_settled" {
                ("settled source".to_owned(), None)
            } else if mine && decision.to == Some(RolePower::On) {
                (format!("triggered: {change}"), None)
            } else if let Some(standby) = standby {
                let via = standby
                    .via
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("+");
                let who = if mine { "stopped" } else { "taken over" };
                (
                    format!("{who}; standby via {via}"),
                    Some(outcome_word(&standby.outcome)),
                )
            } else if mine && !change.is_empty() {
                (format!("turned off: {change}"), None)
            } else if mine {
                ("—".to_owned(), None)
            } else {
                return None;
            };
            Some(Involved {
                decision: decision.clone(),
                part,
                outcome,
            })
        })
        .take(limit)
        .collect()
}

/// One drift of an output: its onset row, its resolution row, or both.
pub(crate) struct DriftPair {
    pub(crate) field: Field,
    pub(crate) intended: String,
    /// What disagreed, `off via ha`, when the onset row is still in memory.
    pub(crate) observed: Option<String>,
    pub(crate) since: DateTime<Utc>,
    pub(crate) opened: Option<(u64, DateTime<Utc>)>,
    /// `(id, at, "on via tv_direct")`.
    pub(crate) resolved: Option<(u64, DateTime<Utc>, String)>,
    pub(crate) lasted: String,
}

/// `drift` / `drift_resolved` rows about `subject`, paired by field and onset, newest
/// onset first. An unpaired onset is open while the snapshot still holds it.
pub(crate) fn drift_history(
    snapshot: &Snapshot,
    subject: &str,
    now: DateTime<Utc>,
) -> Vec<DriftPair> {
    let mut pairs: BTreeMap<(DateTime<Utc>, String), DriftPair> = BTreeMap::new();
    for decision in snapshot.decisions.iter().filter(|d| !is_migrated(d)) {
        let Some(drift) = drift_row(decision).filter(|drift| drift.subject == subject) else {
            continue;
        };
        let pair = pairs
            .entry((drift.since, drift.field.to_string()))
            .or_insert_with(|| DriftPair {
                field: drift.field,
                intended: drift.intended.clone(),
                observed: None,
                since: drift.since,
                opened: None,
                resolved: None,
                lasted: String::new(),
            });
        if decision.reason == "drift" {
            pair.opened = Some((decision.id, decision.timestamp));
            pair.observed = Some(format!("{} via {}", drift.observed, drift.via));
        } else {
            pair.resolved = Some((
                decision.id,
                decision.timestamp,
                format!("{} via {}", drift.observed, drift.via),
            ));
        }
    }
    let mut history: Vec<DriftPair> = pairs
        .into_values()
        .map(|mut pair| {
            pair.lasted = match &pair.resolved {
                Some((_, at, _)) => duration(*at - pair.since),
                None if snapshot.drift.iter().any(|open| {
                    open.subject == subject && open.field == pair.field && open.since == pair.since
                }) =>
                {
                    format!("open · {}", duration(now - pair.since))
                }
                None => "no resolution recorded".to_owned(),
            };
            pair
        })
        .collect();
    history.reverse();
    history
}

// ---- registry -----------------------------------------------------------------------------

/// One row of the registry.
pub(crate) struct RegRow {
    pub(crate) href: String,
    pub(crate) name: String,
    /// Beside the name: the role id, adapter kind or the address's CEC name.
    pub(crate) tag: Option<String>,
    pub(crate) kind: String,
    pub(crate) la_pa: String,
    pub(crate) vendor_osd: String,
    /// `(label, tone)`.
    pub(crate) identity: Option<(&'static str, &'static str)>,
    /// Under the identity chip: when the next identification query is due.
    pub(crate) identity_note: Option<String>,
    /// `(tone, label)`.
    pub(crate) power: Option<(&'static str, String)>,
    pub(crate) power_note: String,
    pub(crate) sources: String,
    pub(crate) last: Option<DecisionView>,
    pub(crate) seen: String,
}

/// A footer explanation of a row state.
pub(crate) struct RegNote {
    pub(crate) label: &'static str,
    pub(crate) tone: &'static str,
    pub(crate) text: String,
}

pub(crate) struct Registry {
    pub(crate) roles: Vec<RegRow>,
    pub(crate) outputs: Vec<RegRow>,
    pub(crate) bridge: RegRow,
    pub(crate) unbound: Vec<RegRow>,
    pub(crate) notes: Vec<RegNote>,
}

/// `in 4 m`, `in 12 s`, or `now` when due.
pub(crate) fn until(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (at - now).num_seconds();
    match seconds {
        ..=0 => "now".to_owned(),
        1..60 => format!("in {seconds} s"),
        60..3600 => format!("in {} m", seconds / 60),
        _ => format!("in {} h", seconds / 3600),
    }
}

fn la_pa(entry: Option<&BusEntryView>) -> String {
    entry.map_or_else(
        || "—".to_owned(),
        |entry| {
            format!(
                "{} · {}",
                entry.logical_address,
                entry.physical_address.as_deref().unwrap_or("?")
            )
        },
    )
}

fn vendor_osd(entry: Option<&BusEntryView>) -> String {
    entry.map_or_else(
        || "—".to_owned(),
        |entry| {
            format!(
                "{} · {}",
                entry.vendor.as_deref().unwrap_or("—"),
                entry
                    .osd
                    .as_deref()
                    .map_or_else(|| "—".to_owned(), |osd| format!("\"{osd}\""))
            )
        },
    )
}

fn power_chip(on: Option<bool>, label: impl Into<String>) -> (&'static str, String) {
    (
        match on {
            Some(true) => "ok",
            Some(false) => "dim",
            None => "none",
        },
        label.into(),
    )
}

/// The newest non-migrated row matching `wanted`.
fn newest(snapshot: &Snapshot, wanted: impl Fn(&DecisionView) -> bool) -> Option<DecisionView> {
    snapshot
        .decisions
        .iter()
        .rev()
        .find(|decision| !is_migrated(decision) && wanted(decision))
        .cloned()
}

fn newest_time(times: impl IntoIterator<Item = Option<DateTime<Utc>>>) -> String {
    times
        .into_iter()
        .flatten()
        .max()
        .as_ref()
        .map_or_else(|| "never".to_owned(), age)
}

fn role_row(snapshot: &Snapshot, role: &RoleView, device: Option<&DeviceConfig>) -> RegRow {
    let restored = is_restored(role);
    let entry = bound_entry(snapshot, role).or_else(|| {
        device
            .filter(|_| restored)
            .and_then(|d| restored_entry(snapshot, d))
    });
    let sources = role_sources(snapshot, role);
    let polled = device.is_some_and(|device| device.presence == Presence::Poll);
    let power_note =
        if polled && bound_entry(snapshot, role).is_some_and(|e| e.power == BusPower::Unknown) {
            "left the bus".to_owned()
        } else if let Some(used) = [&sources.cec, &sources.probe].into_iter().find(|s| s.used) {
            format!("{} {}", used.source, used.value)
        } else {
            "cec —".to_owned()
        };
    let mut words = vec!["cec"];
    if role.media_player.is_some() {
        words.push("ha");
    }
    if role.probe_configured {
        words.push("probe");
    }
    RegRow {
        href: format!("/devices/{}", path_segment(&role.role)),
        name: role.name.clone(),
        tag: Some(role.role.clone()),
        kind: device.map_or_else(String::new, kind_label),
        la_pa: la_pa(entry),
        vendor_osd: vendor_osd(entry),
        identity: Some(role_identity(role)),
        identity_note: None,
        power: Some(power_chip(
            Some(role.power == RolePower::On),
            if role.power == RolePower::On {
                "on"
            } else {
                "off"
            },
        )),
        power_note,
        sources: words.join(" · "),
        last: newest(snapshot, |d| d.role.as_deref() == Some(role.role.as_str())),
        seen: if restored {
            "before restart".to_owned()
        } else {
            role.last_seen
                .as_ref()
                .map_or_else(|| "never".to_owned(), age)
        },
    }
}

/// A path segment with everything but unreserved characters percent-encoded.
pub(crate) fn path_segment(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn about(subject: &'static str) -> impl Fn(&DecisionView) -> bool {
    move |decision| {
        decision.expectations.iter().any(|e| e.subject == subject)
            || drift_row(decision).is_some_and(|drift| drift.subject == subject)
    }
}

fn output_rows(snapshot: &Snapshot, config: Option<&Config>) -> Vec<RegRow> {
    let shadow = snapshot.mode == Mode::Shadow;
    let webos = config.is_some_and(|c| matches!(c.tv.off, TvOffConfig::Webos { .. }));
    let denon = config.is_some_and(|c| c.receiver.observe.is_some());
    let (tv_on, tv_source, tv_value) = tv_power(snapshot);
    let (receiver_on, receiver_source, receiver_value) = receiver_power(snapshot);
    let audio = audio_entry(snapshot);
    let speakers = &snapshot.ha.sonos;
    let direct_at =
        |sample: Option<(DirectPower, DateTime<Utc>)>| sample.filter(|_| !shadow).map(|(_, at)| at);
    vec![
        RegRow {
            href: "/devices/out/tv".to_owned(),
            name: "TV".to_owned(),
            tag: None,
            kind: if webos {
                "webOS · not on the bus"
            } else {
                "Home Assistant · not on the bus"
            }
            .to_owned(),
            la_pa: "—".to_owned(),
            vendor_osd: "—".to_owned(),
            identity: None,
            identity_note: None,
            power: Some(power_chip(
                tv_on,
                match tv_on {
                    Some(true) => "on",
                    Some(false) => "off",
                    None => "unknown",
                },
            )),
            power_note: format!("{tv_source} {tv_value}"),
            sources: if webos { "webos · ha" } else { "ha" }.to_owned(),
            last: newest(snapshot, about("tv")),
            seen: newest_time([direct_at(snapshot.tv_direct), snapshot.ha.tv.last_changed]),
        },
        RegRow {
            href: "/devices/out/receiver".to_owned(),
            name: "Receiver".to_owned(),
            tag: None,
            kind: if denon {
                "Denon HTTP status · audio system"
            } else {
                "Home Assistant · audio system"
            }
            .to_owned(),
            la_pa: audio.map_or_else(|| "— · —".to_owned(), |entry| la_pa(Some(entry))),
            vendor_osd: vendor_osd(audio),
            identity: None,
            identity_note: None,
            power: Some(power_chip(
                receiver_on,
                match receiver_on {
                    Some(true) => "on",
                    Some(false) => "off",
                    None => "unknown",
                },
            )),
            power_note: format!("{receiver_source} {receiver_value}"),
            sources: if denon { "denon · ha" } else { "ha" }.to_owned(),
            last: newest(snapshot, about("receiver")),
            seen: newest_time([
                direct_at(snapshot.receiver_direct),
                snapshot.ha.receiver.last_changed,
                audio.and_then(|entry| entry.last_seen),
            ]),
        },
        RegRow {
            href: "/devices/out/speakers".to_owned(),
            name: "Speakers".to_owned(),
            tag: None,
            kind: "Sonos via Home Assistant".to_owned(),
            la_pa: "—".to_owned(),
            vendor_osd: "—".to_owned(),
            identity: None,
            identity_note: None,
            power: Some(power_chip(
                ha_power(&speakers.state),
                speakers.state.clone(),
            )),
            power_note: String::new(),
            sources: "ha".to_owned(),
            last: newest(snapshot, about("speakers")),
            seen: newest_time([speakers.last_changed]),
        },
    ]
}

fn bridge_row(snapshot: &Snapshot, config: Option<&Config>) -> RegRow {
    let la = snapshot.bridge_logical_address;
    let entry = snapshot
        .bus
        .iter()
        .find(|entry| entry.logical_address == la);
    let adapter = config.map(|config| match config.bridge {
        crate::config::BridgeConfig::Tasmota { .. } => "tasmota",
        crate::config::BridgeConfig::Aether { .. } => "aether",
    });
    RegRow {
        href: "/devices/out/bridge".to_owned(),
        name: "Bridge".to_owned(),
        tag: adapter.map(str::to_owned),
        kind: format!(
            "{} · speaks as LA {la}",
            adapter.map_or_else(|| "adapter".to_owned(), |a| format!("{a} adapter"))
        ),
        la_pa: entry.map_or_else(|| format!("{la} · —"), |entry| la_pa(Some(entry))),
        vendor_osd: vendor_osd(entry),
        identity: Some(if snapshot.bridge_online {
            ("online", "ok")
        } else {
            ("offline", "bad")
        }),
        identity_note: None,
        power: None,
        power_note: String::new(),
        sources: if snapshot.bridge_telemetry.is_some() {
            "mqtt · telemetry"
        } else {
            "mqtt"
        }
        .to_owned(),
        last: None,
        seen: snapshot
            .last_frame_at
            .as_ref()
            .map_or_else(|| "never".to_owned(), age),
    }
}

/// Bus entries no role is bound to, without the bridge, the receiver's audio-system entry
/// and the entries restored roles wait on.
pub(crate) fn unbound_entries<'a>(
    snapshot: &'a Snapshot,
    config: Option<&Config>,
) -> Vec<&'a BusEntryView> {
    let audio = audio_entry(snapshot).map(|entry| entry.logical_address);
    let waiting: BTreeSet<u8> = config
        .map(|config| {
            snapshot
                .roles
                .iter()
                .filter(|role| is_restored(role))
                .filter_map(|role| config.device.iter().find(|d| d.role == role.role))
                .filter_map(|device| restored_entry(snapshot, device))
                .map(|entry| entry.logical_address)
                .collect()
        })
        .unwrap_or_default();
    snapshot
        .bus
        .iter()
        .filter(|entry| {
            entry.matched_role.is_none()
                && entry.logical_address != snapshot.bridge_logical_address
                && Some(entry.logical_address) != audio
                && !waiting.contains(&entry.logical_address)
        })
        .collect()
}

/// An unbound entry's chip: conflict, unidentified or unmatched.
pub(crate) fn entry_identity(
    snapshot: &Snapshot,
    entry: &BusEntryView,
) -> (&'static str, &'static str) {
    if snapshot
        .conflicts
        .iter()
        .any(|conflict| conflict.logical_addresses.contains(&entry.logical_address))
    {
        ("conflict", "bad")
    } else if entry.identified {
        ("unmatched", "dim")
    } else {
        ("unidentified", "none")
    }
}

/// `next query in 4 m`, or why none is scheduled.
pub(crate) fn next_query(entry: &BusEntryView, now: DateTime<Utc>) -> String {
    match entry.next_identify_at {
        Some(at) => format!("next query {}", until(at, now)),
        None => "no query scheduled (bridge offline)".to_owned(),
    }
}

fn entry_row(snapshot: &Snapshot, entry: &BusEntryView, now: DateTime<Utc>) -> RegRow {
    let la = entry.logical_address;
    let identity = entry_identity(snapshot, entry);
    let power = bus_power(&entry.power);
    RegRow {
        href: format!("/devices/la/{la}"),
        name: format!("LA {la}"),
        tag: Some(la_label(la).to_owned()),
        kind: format!(
            "{} · not a role",
            entry.device_type.map_or_else(
                || "type unknown".to_owned(),
                |_| super::device_type(&entry.device_type)
            )
        ),
        la_pa: la_pa(Some(entry)),
        vendor_osd: vendor_osd(Some(entry)),
        identity: Some(identity),
        identity_note: (!entry.identified).then(|| next_query(entry, now)),
        power: Some(power_chip(bus_reading(entry.power), power)),
        power_note: String::new(),
        sources: "cec".to_owned(),
        last: None,
        seen: if entry.verified {
            entry
                .last_seen
                .as_ref()
                .map_or_else(|| "never".to_owned(), age)
        } else {
            "before restart".to_owned()
        },
    }
}

/// Every device Choregos knows about, by section, with footer notes for the states that
/// need one.
pub(crate) fn registry(
    snapshot: &Snapshot,
    config: Option<&Config>,
    now: DateTime<Utc>,
) -> Registry {
    let device = |role: &str| config.and_then(|c| c.device.iter().find(|d| d.role == role));
    let restart = service_started(snapshot)
        .map_or_else(|| "last".to_owned(), |at| at.format("%H:%M").to_string());
    let mut notes = Vec::new();
    for role in snapshot.roles.iter().filter(|role| is_restored(role)) {
        notes.push(RegNote {
            label: "restored",
            tone: "pend",
            text: format!(
                "{name}'s identity came from the state file after the {restart} restart. It cannot trigger actions until {name} reports the same physical address live.",
                name = role.name
            ),
        });
    }
    let unbound = unbound_entries(snapshot, config);
    for entry in &unbound {
        let la = entry.logical_address;
        match entry_identity(snapshot, entry).0 {
            "unidentified" => notes.push(RegNote {
                label: "unidentified",
                tone: "none",
                text: match entry.next_identify_at {
                    Some(at) => format!(
                        "LA {la} has talked but not yet answered every identification query. Next query {} (backing off; at most one query per second on the bus).",
                        until(at, now)
                    ),
                    None => format!(
                        "LA {la} has talked but not yet answered every identification query. No query is sent while the bridge is offline."
                    ),
                },
            }),
            "unmatched" => notes.push(RegNote {
                label: "unmatched",
                tone: "dim",
                text: format!(
                    "LA {la} is fully identified but no role's match rule fits it. Add a role in Config if it should take part."
                ),
            }),
            _ => {}
        }
    }
    Registry {
        roles: snapshot
            .roles
            .iter()
            .map(|role| role_row(snapshot, role, device(&role.role)))
            .collect(),
        outputs: output_rows(snapshot, config),
        bridge: bridge_row(snapshot, config),
        unbound: unbound
            .iter()
            .map(|entry| entry_row(snapshot, entry, now))
            .collect(),
        notes,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;
    use crate::{
        action::{Expectation, ServiceKind, Trigger},
        state::{Command, Conflict},
        web::view::tests::snapshot,
    };

    fn met(via: Via, latency_ms: u64) -> Outcome {
        Outcome::Met {
            via,
            observed: "on".to_owned(),
            latency_ms,
        }
    }

    fn expect(
        subject: &str,
        field: Field,
        value: &str,
        via: &[Via],
        outcome: Outcome,
    ) -> Expectation {
        Expectation {
            subject: subject.to_owned(),
            field,
            expect: value.to_owned(),
            via: via.to_vec(),
            deadline_ms: 45_000,
            attempts: 0,
            outcome,
        }
    }

    fn tv_on(outcome: Outcome) -> Expectation {
        expect("tv", Field::Power, "on", &[Via::TvDirect], outcome)
    }

    fn standby(role: &str, via: &[Via], outcome: Outcome) -> Expectation {
        expect(
            &format!("role:{role}"),
            Field::Power,
            "standby",
            via,
            outcome,
        )
    }

    fn row(id: u64, role: Option<&str>, reason: &str) -> DecisionView {
        DecisionView {
            id,
            trigger: Trigger::Frame {
                from: 4,
                to: 15,
                data: "82".to_owned(),
            },
            parent: None,
            timestamp: Utc::now() - TimeDelta::hours(1)
                + TimeDelta::minutes(i64::try_from(id).unwrap()),
            role: role.map(str::to_owned),
            from: None,
            to: None,
            reason: reason.to_owned(),
            actions: vec![],
            would: false,
            expectations: vec![],
            verdict: Verdict::None,
            drift: None,
        }
    }

    fn wake(id: u64, verdict: Verdict, expectations: Vec<Expectation>) -> DecisionView {
        DecisionView {
            from: Some(RolePower::Off),
            to: Some(RolePower::On),
            verdict,
            expectations,
            ..row(id, Some("streamer"), "ActiveSource")
        }
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let five = [15, 20, 35, 40, 50];
        for (p, expected) in [(5, 15), (30, 20), (40, 20), (50, 35), (95, 50), (100, 50)] {
            assert_eq!(percentile(&five, p), Some(expected), "p{p}");
        }
        let twenty: Vec<u64> = (1..=20).collect();
        assert_eq!(percentile(&twenty, 50), Some(10));
        assert_eq!(percentile(&twenty, 95), Some(19));
        assert_eq!(percentile(&[7], 95), Some(7));
        assert_eq!(percentile(&[], 50), None);
    }

    #[test]
    fn stats_prefer_a_source_and_hide_p95_under_five_samples() {
        let four: Vec<(u64, Via)> = (1..=4).map(|s| (s * 1000, Via::TvDirect)).collect();
        let four = Stats::from_samples(&four, None);
        assert_eq!((four.n, four.p50, four.p95), (4, Some(2000), None));
        assert_eq!(four.text(), "p50 2.0 s · p95 hidden");
        let five: Vec<(u64, Via)> = (1..=5).map(|s| (s * 1000, Via::TvDirect)).collect();
        let five = Stats::from_samples(&five, None);
        assert_eq!((five.p50, five.p95), (Some(3000), Some(5000)));
        assert_eq!(five.text(), "p50 3.0 s · p95 5.0 s");

        let mixed = [(100, Via::Ha), (200, Via::TvDirect)];
        let preferred = Stats::from_samples(&mixed, Some(Via::TvDirect));
        assert_eq!(
            (preferred.n, preferred.via_text()),
            (1, "tv_direct".to_owned())
        );
        let fallback = Stats::from_samples(&mixed[..1], Some(Via::TvDirect));
        assert_eq!((fallback.n, fallback.via_text()), (1, "ha".to_owned()));
        let both = Stats::from_samples(&mixed, None);
        assert_eq!((both.n, both.via_text()), (2, "ha · tv_direct".to_owned()));
        assert_eq!(Stats::default().text(), "—");
    }

    /// Wakes 1–4 count; would, migrated, pending and verdict-less rows do not.
    fn ring() -> VecDeque<DecisionView> {
        let receiver_on = |outcome| expect("receiver", Field::Power, "on", &[Via::Denon], outcome);
        let mut would = wake(
            5,
            Verdict::Achieved,
            vec![tv_on(met(Via::TvDirect, 99_000))],
        );
        would.would = true;
        let mut migrated = wake(
            6,
            Verdict::Achieved,
            vec![tv_on(met(Via::TvDirect, 98_000))],
        );
        migrated.trigger = Trigger::Service {
            name: ServiceKind::Migrated,
        };
        let settled = |id, parent, latency| DecisionView {
            parent: Some(parent),
            expectations: vec![expect(
                "receiver",
                Field::Source,
                "Streamer",
                &[Via::Ha],
                met(Via::Ha, latency),
            )],
            ..row(id, Some("streamer"), "takeover_settled")
        };
        let console = DecisionView {
            role: Some("console".to_owned()),
            ..wake(
                11,
                Verdict::Achieved,
                vec![standby("streamer", &[Via::Cec], met(Via::Cec, 4000))],
            )
        };
        let rows = [
            wake(
                1,
                Verdict::Achieved,
                vec![
                    tv_on(met(Via::TvDirect, 10_000)),
                    receiver_on(met(Via::Denon, 2000)),
                ],
            ),
            wake(
                2,
                Verdict::Partial,
                vec![
                    tv_on(met(Via::TvDirect, 20_000)),
                    receiver_on(Outcome::Missed {
                        observed: "off".to_owned(),
                    }),
                ],
            ),
            wake(3, Verdict::Missed, vec![tv_on(met(Via::Ha, 3000))]),
            wake(4, Verdict::Achieved, vec![tv_on(met(Via::TvDirect, 0))]),
            would,
            migrated,
            wake(7, Verdict::Pending, vec![tv_on(Outcome::Pending)]),
            wake(8, Verdict::None, vec![]),
            settled(9, 1, 500),
            settled(10, 5, 900),
            console,
            DecisionView {
                expectations: vec![standby(
                    "streamer",
                    &[Via::Ps5],
                    Outcome::Missed {
                        observed: "awake".to_owned(),
                    },
                )],
                ..row(12, Some("streamer"), "manual_standby")
            },
            DecisionView {
                expectations: vec![standby(
                    "streamer",
                    &[Via::Cec],
                    Outcome::Unknown {
                        observed: Some("absent".to_owned()),
                    },
                )],
                ..row(13, None, "command_all_off")
            },
            DecisionView {
                expectations: vec![standby("streamer", &[Via::Cec], Outcome::Pending)],
                ..row(14, Some("kodi"), "ActiveSource")
            },
            row(15, Some("streamer"), "presence_timeout"),
        ];
        rows.into()
    }

    #[test]
    fn role_stats_count_settled_wakes_and_skip_would_migrated_and_zero_latency() {
        let stats = role_stats(&ring(), "streamer", true);
        assert_eq!((stats.wakes, stats.achieved), (4, 2));
        // 10 s and 20 s via webOS; the HA sample, the 0 ms wake, would and migrated rows drop.
        assert_eq!(stats.picture.n, 2);
        assert_eq!(stats.picture.via, vec![Via::TvDirect]);
        assert_eq!((stats.picture.p50, stats.picture.p95), (Some(10_000), None));
        assert_eq!((stats.receiver.n, stats.receiver.p50), (1, Some(2000)));
        // Only the child of a counted wake.
        assert_eq!((stats.source.n, stats.source.p50), (1, Some(500)));
        assert_eq!(
            stats.standby,
            StandbyStats {
                total: 3,
                confirmed: 1,
                missed: 1,
                unknown: 1,
                via: vec![Via::Cec, Via::Ps5],
            }
        );
        assert_eq!(stats.left_bus, Some(1));
        assert_eq!(role_stats(&ring(), "streamer", false).left_bus, None);

        let measures = role_measures(&stats, "Streamer");
        let by = |name: &str| measures.iter().find(|m| m.name == name).expect(name);
        assert_eq!(by("Wakes").value, "2 / 4 achieved");
        assert_eq!(by("Wake → picture").value, "p50 10.0 s · p95 hidden");
        assert_eq!(by("Wake → picture").source, "tv_direct");
        let standby = by("Standby confirmed");
        assert_eq!(standby.value, "1 of 3 · 1 unconfirmed");
        assert_eq!(standby.chip, Some(("bad", "1 missed".to_owned())));
        assert_eq!((standby.n, standby.source.as_str()), (3, "cec · ps5"));
        assert_eq!(by("Left the bus").value, "1×");
    }

    #[test]
    fn identity_rows_compare_each_key_with_the_rule() {
        let rule = MatchConfig {
            osd: Some("Console B".to_owned()),
            vendor: None,
            physical: Some("2.4.0.0".to_owned()),
        };
        let keys = Keys {
            la: Some(8),
            physical: Some("2.4.0.0".to_owned()),
            vendor: Some("08001F".to_owned()),
            osd: Some(" Console B".to_owned()),
        };
        let checks = |keys: &Keys| -> Vec<(String, Option<Check>)> {
            identity_rows(&rule, keys)
                .into_iter()
                .map(|row| (row.value, row.check))
                .collect()
        };
        assert_eq!(
            checks(&keys),
            [
                ("8 · playback device 2".to_owned(), None),
                ("2.4.0.0".to_owned(), Some(Check::Matches)),
                ("\" Console B\"".to_owned(), Some(Check::Matches)),
                ("08001F".to_owned(), Some(Check::NotRequired)),
            ]
        );
        let moved = Keys {
            physical: Some("2.3.0.0".to_owned()),
            osd: None,
            ..keys
        };
        let moved = checks(&moved);
        assert_eq!(moved[1].1, Some(Check::Mismatch));
        assert_eq!(moved[2].1, Some(Check::Unreported));
        assert_eq!(
            match_rule(&rule),
            r#"match = { osd = "Console B", physical = "2.4.0.0" }"#
        );

        let vendor = MatchConfig {
            osd: None,
            vendor: Some("08001f".to_owned()),
            physical: None,
        };
        let mut entry = BusEntryView {
            logical_address: 8,
            physical_address: Some("2.4.0.0".to_owned()),
            device_type: Some(4),
            vendor: Some("08001F".to_owned()),
            osd: None,
            power: BusPower::Unknown,
            last_seen: None,
            identified: true,
            verified: false,
            matched_role: None,
            confirmed_at: None,
            next_identify_at: None,
        };
        assert!(rule_matches(&vendor, &entry), "vendor compares as hex");
        assert!(!rule_matches(&rule, &entry), "a rule key the entry lacks");
        entry.osd = Some("Console B".to_owned());
        assert!(rule_matches(&rule, &entry));
    }

    fn device(text: &str) -> DeviceConfig {
        toml::from_str(text).expect("test device")
    }

    fn example() -> Config {
        Config::parse(include_str!("../../../choregos.example.toml")).expect("valid example")
    }

    #[test]
    fn quirk_sentences_follow_the_device_profile() {
        let config = example();
        let probed = device(
            r#"
role = "console_b"
name = "Console B"
kind = "playback"
match = { physical = "2.4.0.0" }
source = "Console B"
on = ["image_view_on", "text_view_on", "active_source"]
off = ["standby", "power_standby"]
takeover = "cec_standby"
presence = "events"
probe = { kind = "ps5", host = "console.test", start_wake = "would", boot_s = 40, idle_poll_s = 3 }
"#,
        );
        let text: Vec<String> = quirk_sentences(&probed, &config)
            .iter()
            .map(Sentence::plain)
            .collect();
        assert_eq!(
            text[0],
            "Choregos learns that Console B is on from its own CEC announcements: `image_view_on`, `text_view_on` and `active_source`. Off comes from `standby` and `power_standby`."
        );
        assert!(text[1].contains("is sent CEC Standby (`takeover = cec_standby`) and its CEC power status and the probe confirm it went to sleep (`verify_standby`)."), "{}", text[1]);
        assert!(text[2].contains("would wake the room but only logs it (`start_wake = would`)"));
        assert!(text[3].contains("allows 40 s to boot") && text[3].contains("`idle_poll_s = 3`"));
        assert_eq!(text.len(), 4, "no presence or media-player sentence");
        assert_eq!(kind_label(&probed), "player · input \"Console B\" · probe");

        let polled = device(
            r#"
role = "console_a"
name = "Console A"
kind = "playback"
match = { osd = "Console A" }
source = "Console A"
on = ["active_source"]
off = []
takeover = "cec_pause_standby"
verify_standby = false
presence = "poll"
"#,
        );
        let text: Vec<String> = quirk_sentences(&polled, &config)
            .iter()
            .map(Sentence::plain)
            .collect();
        assert!(text[0].ends_with("No CEC announcement turns it off."));
        assert!(text[1].contains("is sent Pause, then CEC Standby (`takeover = cec_pause_standby`); its sleep is not verified (`verify_standby = false`)."), "{}", text[1]);
        assert!(text[2].contains(&format!(
            "every {} s and marks it off after {} unanswered polls (`presence = poll`)",
            config.timing.presence_poll_s, config.timing.presence_misses
        )));
        assert_eq!(kind_label(&polled), "player · input \"Console A\" · polled");

        let stopped = device(
            r#"
role = "kodi"
name = "Kodi"
kind = "playback"
match = { osd = "Kodi" }
source = "Kodi"
on = ["active_source"]
off = ["standby"]
takeover = "ha_media_stop"
media_player = "media_player.kodi"
presence = "events"
"#,
        );
        let text: Vec<String> = quirk_sentences(&stopped, &config)
            .iter()
            .map(Sentence::plain)
            .collect();
        assert!(text[1].contains("is stopped through Home Assistant (`takeover = ha_media_stop`); its sleep is not verified."));
        assert!(
            text[2]
                .starts_with("Home Assistant `media_player.kodi` is how it is stopped on takeover")
        );
    }

    #[test]
    fn agreement_compares_normalised_readings() {
        assert_eq!(agreement(&[Some(true), Some(true), None]), Agreement::Agree);
        assert_eq!(
            agreement(&[Some(true), Some(false), None]),
            Agreement::Disagree
        );
        assert_eq!(agreement(&[Some(false), None, None]), Agreement::Alone);
        assert_eq!(agreement(&[None, None]), Agreement::Alone);
        // A role: CEC standby, HA standby, probe standby agree; HA playing disagrees.
        for state in ["playing", "paused", "idle", "buffering"] {
            assert_eq!(ha_power(state), Some(true), "{state}");
        }
        let off = [
            bus_reading(BusPower::Standby),
            ha_power("standby"),
            probe_reading(ConsoleState::Standby),
        ];
        assert_eq!(agreement(&off), Agreement::Agree);
        assert_eq!(
            agreement(&[
                bus_reading(BusPower::On),
                ha_power("playing"),
                probe_reading(ConsoleState::Awake)
            ]),
            Agreement::Agree
        );
        assert_eq!(
            agreement(&[bus_reading(BusPower::Standby), ha_power("playing")]),
            Agreement::Disagree
        );
        // Not evidence: a CEC transition, HA unavailable, an unreachable probe or receiver.
        assert_eq!(
            agreement(&[
                bus_reading(BusPower::Transition),
                ha_power("unavailable"),
                probe_reading(ConsoleState::Unreachable),
                receiver_direct_reading(DirectPower::Unreachable),
            ]),
            Agreement::Alone
        );
        // The TV leaves the network in standby: HA unavailable reads as off, so it disagrees
        // with webOS reporting on (Home Assistant's reconnect after a wake).
        assert_eq!(
            agreement(&[
                tv_direct_reading(DirectPower::On),
                tv_ha_reading("unavailable")
            ]),
            Agreement::Disagree
        );
        assert_eq!(
            agreement(&[
                tv_direct_reading(DirectPower::Unreachable),
                tv_ha_reading("off")
            ]),
            Agreement::Agree
        );
    }

    fn entry(la: u8, confirmed_at: Option<DateTime<Utc>>) -> BusEntryView {
        BusEntryView {
            logical_address: la,
            physical_address: Some("2.1.0.0".to_owned()),
            device_type: Some(4),
            vendor: None,
            osd: Some("Streamer".to_owned()),
            power: BusPower::Standby,
            last_seen: None,
            identified: true,
            verified: true,
            matched_role: Some("streamer".to_owned()),
            confirmed_at,
            next_identify_at: None,
        }
    }

    fn command(id: u64, command: Command, reason: &str) -> DecisionView {
        DecisionView {
            trigger: Trigger::Command { command },
            ..row(id, None, reason)
        }
    }

    #[test]
    fn identity_events_follow_the_role_across_its_addresses() {
        let mut snapshot = snapshot();
        snapshot.roles[0].logical_address = Some(4);
        let lost_at_6 = DecisionView {
            trigger: Trigger::Frame {
                from: 6,
                to: 0,
                data: "8700001F".to_owned(),
            },
            ..row(2, Some("streamer"), "identity_lost")
        };
        let conflict_from = |id, la, reason| DecisionView {
            trigger: Trigger::Frame {
                from: la,
                to: 15,
                data: "84".to_owned(),
            },
            ..row(id, None, reason)
        };
        snapshot.decisions = [
            DecisionView {
                trigger: Trigger::Service {
                    name: ServiceKind::Started,
                },
                ..row(1, None, "service_started")
            },
            lost_at_6,
            command(3, Command::Identify(4), "command_identify"),
            command(4, Command::Identify(9), "command_identify"),
            conflict_from(5, 4, "conflict"),
            conflict_from(6, 7, "conflict_resolved"),
            row(7, Some("console"), "identity_lost"),
            command(8, Command::Forget(6), "command_forget"),
        ]
        .into();
        let started = snapshot.decisions[0].timestamp;
        let confirmed = started + TimeDelta::seconds(90);
        let bound = entry(4, Some(confirmed));
        let events = identity_events(&snapshot, &snapshot.roles[0], Some(&bound));
        let mut expected = vec![
            (Some(8), "command forget".to_owned()),
            (Some(5), "conflict".to_owned()),
            (Some(3), "command identify".to_owned()),
            (Some(2), "identity_lost".to_owned()),
        ];
        expected.push((None, "verified".to_owned()));
        let mut seen: Vec<(Option<u64>, String)> =
            events.iter().map(|e| (e.id, e.word.clone())).collect();
        // The verified line sits at its own time; compare as sets of (id, word) in time order.
        assert!(
            events.windows(2).all(|pair| pair[0].at >= pair[1].at),
            "newest first"
        );
        seen.sort();
        expected.sort();
        assert_eq!(seen, expected);
        assert!(events.iter().any(|e| e.sentence.starts_with("LA 6: ")));

        // Confirmed before the service started: restored from the state file, re-confirmed
        // by vendor and OSD since (no live 84).
        let stored = entry(4, Some(started - TimeDelta::days(2)));
        let words: Vec<String> = identity_events(&snapshot, &snapshot.roles[0], Some(&stored))
            .into_iter()
            .filter(|e| e.id.is_none())
            .map(|e| e.word)
            .collect();
        assert_eq!(words, ["restored"]);

        // A restored role waiting for its device.
        let mut waiting = snapshot.roles[0].clone();
        waiting.logical_address = None;
        waiting.restored = true;
        let mut unverified = entry(1, Some(started - TimeDelta::days(2)));
        unverified.verified = false;
        unverified.matched_role = None;
        let events = identity_events(&snapshot, &waiting, Some(&unverified));
        let restored = events
            .iter()
            .find(|e| e.word == "restored")
            .expect("restored line");
        assert_eq!(restored.at, started);
        assert!(restored.sentence.contains("waiting for Streamer"));
    }

    #[test]
    fn a_newly_conflicted_role_shows_its_conflict_without_any_loss_history() {
        let mut snapshot = snapshot();
        // Blocked: the engine dropped the binding and records no identity_lost for it.
        let mut role = snapshot.roles[0].clone();
        role.logical_address = None;
        role.last_logical_address = None;
        role.conflicted = true;
        let conflict_from = |id, la| DecisionView {
            trigger: Trigger::Frame {
                from: la,
                to: 15,
                data: "84220004".to_owned(),
            },
            ..row(id, None, "conflict")
        };
        snapshot.decisions = [conflict_from(1, 9), conflict_from(2, 12)].into();
        let ids = |snapshot: &Snapshot| -> Vec<Option<u64>> {
            identity_events(snapshot, &role, None)
                .iter()
                .map(|e| e.id)
                .collect()
        };
        assert_eq!(ids(&snapshot), Vec::<Option<u64>>::new());
        snapshot.conflicts = vec![Conflict {
            roles: vec![role.role.clone()],
            logical_addresses: vec![4, 9],
            reason: "role matches multiple logical addresses".to_owned(),
        }];
        assert_eq!(ids(&snapshot), [Some(1)]);
        // Another role's conflict does not leak in.
        snapshot.conflicts[0].roles = vec!["kodi".to_owned()];
        assert_eq!(ids(&snapshot), Vec::<Option<u64>>::new());
    }

    #[test]
    fn forgetting_an_off_role_shows_the_forget_at_its_last_address() {
        let mut snapshot = snapshot();
        // Already Off, so the forget made no identity_lost row; only the last address ties it.
        let mut role = snapshot.roles[0].clone();
        role.logical_address = None;
        role.last_logical_address = Some(4);
        role.power = RolePower::Off;
        snapshot.decisions = [
            command(1, Command::Forget(4), "command_forget"),
            command(2, Command::Forget(7), "command_forget"),
        ]
        .into();
        let events = identity_events(&snapshot, &role, None);
        let seen: Vec<_> = events.iter().map(|e| (e.id, e.word.as_str())).collect();
        assert_eq!(seen, [(Some(1), "command forget")]);
        assert!(events[0].sentence.starts_with("LA 4: "));
        role.last_logical_address = None;
        assert!(identity_events(&snapshot, &role, None).is_empty());
    }

    #[test]
    fn its_part_names_the_trigger_the_takeover_and_the_settle() {
        let mut snapshot = snapshot();
        let console_wake = DecisionView {
            role: Some("console".to_owned()),
            ..wake(
                3,
                Verdict::Missed,
                vec![standby(
                    "streamer",
                    &[Via::Ps5],
                    Outcome::Missed {
                        observed: "awake".to_owned(),
                    },
                )],
            )
        };
        snapshot.decisions = [
            wake(1, Verdict::Achieved, vec![]),
            DecisionView {
                parent: Some(1),
                ..row(2, Some("streamer"), "takeover_settled")
            },
            console_wake,
            row(4, Some("console"), "Standby"),
        ]
        .into();
        let parts: Vec<(u64, String, Option<String>)> = involved(&snapshot, "streamer", 10)
            .into_iter()
            .map(|row| (row.decision.id, row.part, row.outcome.map(|(word, _)| word)))
            .collect();
        assert_eq!(
            parts,
            [
                (
                    3,
                    "taken over; standby via ps5".to_owned(),
                    Some("missed · read awake".to_owned())
                ),
                (2, "settled source".to_owned(), None),
                (1, "triggered: off → on".to_owned(), None),
            ]
        );
        assert_eq!(involved(&snapshot, "streamer", 1).len(), 1);
    }
}
