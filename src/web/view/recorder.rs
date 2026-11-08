//! Recorder derivations: list filters and pagination, day separators, and the detail page's
//! headline, trigger, actions, expectations and glossary.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeDelta, Timelike, Utc};
use openidconnect::url::form_urlencoded;
use serde::Serialize;

use super::{
    ActionSummary, REASONS, ReasonGroup, drift_lasted, drift_row, hex_bytes, is_migrated, latency,
    max_latency, reason_group, role_name, transition,
};
use crate::{
    action::{Expectation, Outcome, Trigger, Verdict},
    cec::{frame::physical_address, opcode},
    state::{Command, DecisionView, Snapshot},
};

/// Rows per list page.
pub(crate) const PAGE_ROWS: usize = 100;

/// Every reason group in glossary and filter order.
pub(crate) const GROUPS: [ReasonGroup; 9] = [
    ReasonGroup::Wake,
    ReasonGroup::Standby,
    ReasonGroup::Takeover,
    ReasonGroup::Timer,
    ReasonGroup::Identity,
    ReasonGroup::Drift,
    ReasonGroup::Command,
    ReasonGroup::Service,
    ReasonGroup::Other,
];

/// Every verdict in filter order.
pub(crate) const VERDICTS: [Verdict; 6] = [
    Verdict::Pending,
    Verdict::Achieved,
    Verdict::Partial,
    Verdict::Missed,
    Verdict::Unknown,
    Verdict::None,
];

/// A serde word (`room_off`, `tv_direct`) of a unit enum.
fn word(value: &impl Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WouldFilter {
    #[default]
    All,
    Only,
    Hide,
}

impl WouldFilter {
    pub(crate) const ALL: [Self; 3] = [Self::All, Self::Only, Self::Hide];

    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Only => "only",
            Self::Hide => "hide",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Only => "only would",
            Self::Hide => "hide would",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Range {
    Hour,
    SixHours,
    Today,
    /// The whole ring (it holds about two weeks); the default.
    #[default]
    TwoWeeks,
    Custom,
}

impl Range {
    pub(crate) const ALL: [Self; 5] = [
        Self::Hour,
        Self::SixHours,
        Self::Today,
        Self::TwoWeeks,
        Self::Custom,
    ];

    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::SixHours => "6h",
            Self::Today => "today",
            Self::TwoWeeks => "2w",
            Self::Custom => "custom",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Hour => "1 h",
            Self::SixHours => "6 h",
            Self::Today => "today",
            Self::TwoWeeks => "2 weeks",
            Self::Custom => "custom",
        }
    }
}

/// The list's query: every filter is a URL parameter, so a filtered view is a link.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ListFilter {
    /// A role id, or `room` for rows without a role.
    pub(crate) role: Option<String>,
    /// An exact reason literal.
    pub(crate) reason: Option<String>,
    pub(crate) groups: Vec<ReasonGroup>,
    pub(crate) verdicts: Vec<Verdict>,
    pub(crate) would: WouldFilter,
    pub(crate) range: Range,
    pub(crate) from: Option<DateTime<Utc>>,
    pub(crate) to: Option<DateTime<Utc>>,
    /// Show rows imported from a pre-v0.3.0 state file.
    pub(crate) migrated: bool,
    /// Page cursor: only rows with a smaller id.
    pub(crate) before: Option<u64>,
}

/// RFC3339, or a `datetime-local` value (`2026-11-07T17:08`, seconds and fraction optional)
/// read as UTC.
fn parse_time(text: &str) -> Result<Option<DateTime<Utc>>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    if let Ok(at) = DateTime::parse_from_rfc3339(text) {
        return Ok(Some(at.with_timezone(&Utc)));
    }
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(text, format).ok())
        .map(|at| Some(at.and_utc()))
        .ok_or_else(|| format!("{text:?} is not an RFC3339 time"))
}

impl ListFilter {
    /// Parses the query pairs. Lists accept commas and repeated keys; unknown keys are
    /// ignored.
    pub(crate) fn parse(pairs: &[(String, String)]) -> Result<Self, String> {
        let mut filter = Self::default();
        let nonempty = |value: &str| (!value.is_empty()).then(|| value.to_owned());
        for (key, value) in pairs {
            let value = value.trim();
            let parts = || {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
            };
            match key.as_str() {
                "role" => filter.role = nonempty(value),
                "reason" => filter.reason = nonempty(value),
                "group" => {
                    for part in parts() {
                        let group = GROUPS
                            .into_iter()
                            .find(|group| group.label() == part)
                            .ok_or_else(|| format!("unknown reason group {part:?}"))?;
                        if !filter.groups.contains(&group) {
                            filter.groups.push(group);
                        }
                    }
                }
                "verdict" => {
                    for part in parts() {
                        let verdict = VERDICTS
                            .into_iter()
                            .find(|verdict| verdict.to_string() == part)
                            .ok_or_else(|| format!("unknown verdict {part:?}"))?;
                        if !filter.verdicts.contains(&verdict) {
                            filter.verdicts.push(verdict);
                        }
                    }
                }
                "would" => {
                    filter.would = match value {
                        "" => WouldFilter::All,
                        word => WouldFilter::ALL
                            .into_iter()
                            .find(|would| would.word() == word)
                            .ok_or_else(|| {
                                format!("would must be all, only or hide, not {word:?}")
                            })?,
                    }
                }
                "range" => {
                    filter.range = match value {
                        "" => Range::TwoWeeks,
                        word => Range::ALL
                            .into_iter()
                            .find(|range| range.word() == word)
                            .ok_or_else(|| format!("unknown range {word:?}"))?,
                    }
                }
                "from" => filter.from = parse_time(value)?,
                "to" => filter.to = parse_time(value)?,
                "migrated" => filter.migrated = value == "show",
                "before" => {
                    filter.before = match value {
                        "" => None,
                        id => Some(
                            id.parse()
                                .map_err(|_| format!("before must be a decision id, not {id:?}"))?,
                        ),
                    }
                }
                _ => {}
            }
        }
        Ok(filter)
    }

    /// Whether `decision` passes every filter except the page cursor.
    pub(crate) fn matches(&self, decision: &DecisionView, now: DateTime<Utc>) -> bool {
        if !self.migrated && is_migrated(decision) {
            return false;
        }
        let role = match self.role.as_deref() {
            None => true,
            Some("room") => decision.role.is_none(),
            Some(role) => decision.role.as_deref() == Some(role),
        };
        let since = match self.range {
            Range::Hour => Some(now - TimeDelta::hours(1)),
            Range::SixHours => Some(now - TimeDelta::hours(6)),
            Range::Today => Some(now.date_naive().and_time(chrono::NaiveTime::MIN).and_utc()),
            Range::TwoWeeks => Some(now - TimeDelta::days(14)),
            Range::Custom => self.from,
        };
        let until = (self.range == Range::Custom).then_some(self.to).flatten();
        role && self
            .reason
            .as_ref()
            .is_none_or(|reason| *reason == decision.reason)
            && (self.groups.is_empty() || self.groups.contains(&reason_group(&decision.reason)))
            && (self.verdicts.is_empty() || self.verdicts.contains(&decision.verdict))
            && match self.would {
                WouldFilter::All => true,
                WouldFilter::Only => decision.would,
                WouldFilter::Hide => !decision.would,
            }
            && since.is_none_or(|since| decision.timestamp >= since)
            && until.is_none_or(|until| decision.timestamp <= until)
    }

    /// One page of the ring, newest first.
    pub(crate) fn page<'a>(
        &self,
        ring: impl DoubleEndedIterator<Item = &'a DecisionView>,
        now: DateTime<Utc>,
    ) -> Page<'a> {
        let mut total = 0;
        let mut rows = Vec::new();
        let mut more = false;
        for decision in ring.rev().filter(|decision| self.matches(decision, now)) {
            total += 1;
            if self.before.is_some_and(|before| decision.id >= before) {
                continue;
            }
            if rows.len() == PAGE_ROWS {
                more = true;
            } else {
                rows.push(decision);
            }
        }
        let older = more.then(|| rows.last().map(|row| row.id)).flatten();
        Page { total, rows, older }
    }

    /// The canonical query string without the page cursor (`role=room&group=wake,drift`).
    pub(crate) fn query(&self) -> String {
        let mut query = form_urlencoded::Serializer::new(String::new());
        if let Some(role) = &self.role {
            query.append_pair("role", role);
        }
        if let Some(reason) = &self.reason {
            query.append_pair("reason", reason);
        }
        if !self.groups.is_empty() {
            let groups: Vec<_> = self.groups.iter().map(|group| group.label()).collect();
            query.append_pair("group", &groups.join(","));
        }
        if !self.verdicts.is_empty() {
            let verdicts: Vec<_> = self.verdicts.iter().map(ToString::to_string).collect();
            query.append_pair("verdict", &verdicts.join(","));
        }
        if self.would != WouldFilter::All {
            query.append_pair("would", self.would.word());
        }
        if self.range != Range::TwoWeeks {
            query.append_pair("range", self.range.word());
        }
        if self.range == Range::Custom {
            // Lossless: the poll and pager must match exactly the rows the page matched.
            for (key, at) in [("from", self.from), ("to", self.to)] {
                if let Some(at) = at {
                    query.append_pair(
                        key,
                        &at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                    );
                }
            }
        }
        if self.migrated {
            query.append_pair("migrated", "show");
        }
        query.finish()
    }

    /// `/recorder` with this filter and, optionally, a page cursor.
    pub(crate) fn url(&self, before: Option<u64>) -> String {
        let mut query = self.query();
        if let Some(before) = before {
            if !query.is_empty() {
                query.push('&');
            }
            query.push_str(&format!("before={before}"));
        }
        if query.is_empty() {
            "/recorder".to_owned()
        } else {
            format!("/recorder?{query}")
        }
    }

    /// The poll URL of the rows partial (first page only).
    pub(crate) fn rows_url(&self) -> String {
        match self.query() {
            query if query.is_empty() => "/p/recorder".to_owned(),
            query => format!("/p/recorder?{query}"),
        }
    }

    pub(crate) fn is_filtered(&self) -> bool {
        Self {
            before: None,
            ..self.clone()
        } != Self::default()
    }

    pub(crate) fn has_group(&self, group: &ReasonGroup) -> bool {
        self.groups.contains(group)
    }

    pub(crate) fn has_verdict(&self, verdict: &Verdict) -> bool {
        self.verdicts.contains(verdict)
    }

    pub(crate) fn role_is(&self, role: &str) -> bool {
        self.role.as_deref() == Some(role)
    }

    /// The custom range bounds as `datetime-local` values (UTC), to the second or
    /// millisecond when the bound has them.
    pub(crate) fn custom_from(&self) -> String {
        local_input(self.from)
    }

    pub(crate) fn custom_to(&self) -> String {
        local_input(self.to)
    }

    /// Whether a custom bound is finer than a minute, so the inputs must accept seconds.
    pub(crate) fn custom_seconds(&self) -> bool {
        [self.from, self.to]
            .into_iter()
            .flatten()
            .any(|at| at.second() != 0 || at.nanosecond() != 0)
    }
}

fn local_input(at: Option<DateTime<Utc>>) -> String {
    let Some(at) = at else {
        return String::new();
    };
    let format = if at.nanosecond() != 0 {
        "%Y-%m-%dT%H:%M:%S%.3f"
    } else if at.second() != 0 {
        "%Y-%m-%dT%H:%M:%S"
    } else {
        "%Y-%m-%dT%H:%M"
    };
    at.format(format).to_string()
}

/// One list page.
pub(crate) struct Page<'a> {
    /// Rows matching the filters across the whole ring.
    pub(crate) total: usize,
    pub(crate) rows: Vec<&'a DecisionView>,
    /// The cursor of the next (older) page when there is one.
    pub(crate) older: Option<u64>,
}

/// `Today · Sat 7 Nov`, `Yesterday · Fri 6 Nov`, else `Thu 5 Nov` (with the year when it
/// is not this year's). Days are UTC days.
pub(crate) fn day_label(date: NaiveDate, today: NaiveDate) -> String {
    let format = if date.format("%Y").to_string() == today.format("%Y").to_string() {
        "%a %-d %b"
    } else {
        "%a %-d %b %Y"
    };
    let day = date.format(format);
    match (today - date).num_days() {
        0 => format!("Today · {day}"),
        1 => format!("Yesterday · {day}"),
        _ => day.to_string(),
    }
}

/// A list row with its derived columns.
pub(crate) struct ListRow {
    pub(crate) decision: DecisionView,
    pub(crate) role_name: Option<String>,
    pub(crate) change: String,
    pub(crate) group: &'static str,
    pub(crate) trigger: String,
    pub(crate) actions: String,
    pub(crate) latency: Option<String>,
    /// How long a resolved drift held.
    pub(crate) lasted: Option<String>,
}

pub(crate) enum ListItem {
    /// A UTC day boundary.
    Day(String),
    /// A `service_started` row: the restart separator.
    Restart(Box<DecisionView>),
    Row(Box<ListRow>),
}

/// The page's rows with a day separator wherever the UTC date changes (and before the
/// first row), and restart rows as separators.
pub(crate) fn list_items(
    rows: &[&DecisionView],
    snapshot: &Snapshot,
    today: NaiveDate,
) -> Vec<ListItem> {
    let mut items = Vec::with_capacity(rows.len() + 4);
    let mut day = None;
    for decision in rows {
        let date = decision.timestamp.date_naive();
        if day != Some(date) {
            day = Some(date);
            items.push(ListItem::Day(day_label(date, today)));
        }
        if decision.reason == "service_started" {
            items.push(ListItem::Restart(Box::new((*decision).clone())));
            continue;
        }
        items.push(ListItem::Row(Box::new(ListRow {
            role_name: decision
                .role
                .as_deref()
                .map(|role| role_name(snapshot, role)),
            change: match (decision.from, decision.to) {
                (None, None) => "—".to_owned(),
                (from, to) => transition(&from, &to),
            },
            group: reason_group(&decision.reason).label(),
            trigger: trigger_compact(&decision.trigger),
            actions: ActionSummary::from_actions(&decision.actions).text(),
            latency: (decision.verdict != Verdict::Pending)
                .then(|| max_latency(decision).map(|ms| latency(&ms)))
                .flatten(),
            lasted: drift_lasted(decision),
            decision: (*decision).clone(),
        })));
    }
    items
}

/// `command takeover streamer`, `set_mode shadow` …: the command and its argument.
fn command_text(command: &Command) -> String {
    match command {
        Command::AllOff => "all_off".to_owned(),
        Command::SetMode(mode) => format!("set_mode {mode}"),
        Command::Identify(la) => format!("identify {la}"),
        Command::Forget(la) => format!("forget {la}"),
        Command::SendRaw { to, data } => format!("send_raw {to} {}", hex_bytes(data)),
        Command::Takeover(role) => format!("takeover {role}"),
        Command::Standby(role) => format!("standby {role}"),
    }
}

/// The frame of a trigger as spaced hex (`84 23 00 04`); undecodable data as given.
fn trigger_bytes(data: &str) -> Vec<u8> {
    hex::decode(data).unwrap_or_default()
}

/// The list's trigger column: `timer room_off`, `11 → 15 84 23 00 04`,
/// `command takeover streamer`, `obs ps5`, `service started`.
pub(crate) fn trigger_compact(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Frame { from, to, data } => {
            let bytes = trigger_bytes(data);
            if bytes.is_empty() {
                format!("{from} → {to}")
            } else {
                format!("{from} → {to} {}", hex_bytes(&bytes))
            }
        }
        Trigger::Command { command } => format!("command {}", command_text(command)),
        Trigger::Timer { name } => format!("timer {}", word(name)),
        Trigger::Service { name } => format!("service {}", word(name)),
        Trigger::Observation { source } => format!("obs {source}"),
    }
}

/// A logical address by name: the role bound to it, else the bus table's role or OSD name,
/// `broadcast` for 15, the TV for 0, the bridge for its own address, else `LA n`.
pub(crate) fn la_name(snapshot: &Snapshot, la: u8) -> String {
    if la == 15 {
        return "broadcast".to_owned();
    }
    if la == snapshot.bridge_logical_address {
        return if la == 0 { "TV" } else { "bridge" }.to_owned();
    }
    if let Some(role) = snapshot
        .roles
        .iter()
        .find(|role| role.logical_address == Some(la))
    {
        return role.name.clone();
    }
    let entry = snapshot
        .bus
        .iter()
        .find(|entry| entry.logical_address == la);
    if let Some(role) = entry.and_then(|entry| entry.matched_role.as_deref()) {
        return role_name(snapshot, role);
    }
    if let Some(osd) = entry.and_then(|entry| entry.osd.clone()) {
        return osd;
    }
    if la == 0 {
        "TV".to_owned()
    } else {
        format!("LA {la}")
    }
}

/// Operands decoded where the meaning is fixed: a physical address (and device type for
/// Report Physical Address); else `None`.
pub(crate) fn decode_params(opcode: u8, params: &[u8]) -> Option<String> {
    let address = |bytes: &[u8]| {
        bytes
            .get(..2)
            .map(|pa| physical_address(u16::from_be_bytes([pa[0], pa[1]])))
    };
    match opcode {
        0x84 => {
            let pa = address(params)?;
            Some(match params.get(2) {
                Some(kind) => format!(
                    "PA {pa} · type {kind:02X} {}",
                    super::device_type(&Some(*kind))
                ),
                None => format!("PA {pa}"),
            })
        }
        0x82 | 0x86 => address(params).map(|pa| format!("PA {pa}")),
        0x47 => Some(format!("\"{}\"", crate::cec::frame::decode_osd(params))),
        _ => None,
    }
}

/// A trigger frame, rendered.
pub(crate) struct FrameTrigger {
    pub(crate) from: u8,
    pub(crate) to: u8,
    pub(crate) from_name: String,
    pub(crate) to_name: String,
    /// Opcode as hex, absent for a header-only poll.
    pub(crate) opcode: Option<String>,
    pub(crate) opcode_name: String,
    /// Operands: decoded where known, else hex; `None` when there are none.
    pub(crate) params: Option<String>,
    pub(crate) operands: String,
}

/// What caused a decision, rendered by kind.
pub(crate) struct TriggerView {
    /// `frame`, `timer`, `command`, `observation` or `service`.
    pub(crate) kind: &'static str,
    pub(crate) frame: Option<FrameTrigger>,
    /// The non-frame trigger in words (`room_off`, the command JSON, `tv_direct`).
    pub(crate) text: String,
}

pub(crate) fn trigger_view(trigger: &Trigger, snapshot: &Snapshot) -> TriggerView {
    match trigger {
        Trigger::Frame { from, to, data } => {
            let bytes = trigger_bytes(data);
            let opcode_byte = bytes.first().copied();
            let params = bytes.get(1..).unwrap_or_default();
            TriggerView {
                kind: "frame",
                frame: Some(FrameTrigger {
                    from: *from,
                    to: *to,
                    from_name: la_name(snapshot, *from),
                    to_name: la_name(snapshot, *to),
                    opcode: opcode_byte.map(|byte| format!("{byte:02X}")),
                    opcode_name: opcode_byte.map_or("poll", opcode::name).to_owned(),
                    params: (!params.is_empty()).then(|| {
                        opcode_byte
                            .and_then(|byte| decode_params(byte, params))
                            .unwrap_or_else(|| hex_bytes(params))
                    }),
                    operands: hex_bytes(params),
                }),
                text: String::new(),
            }
        }
        Trigger::Command { command } => TriggerView {
            kind: "command",
            frame: None,
            text: serde_json::to_string(command).unwrap_or_default(),
        },
        Trigger::Timer { name } => TriggerView {
            kind: "timer",
            frame: None,
            text: word(name),
        },
        Trigger::Service { name } => TriggerView {
            kind: "service",
            frame: None,
            text: word(name),
        },
        Trigger::Observation { source } => TriggerView {
            kind: "observation",
            frame: None,
            text: source.to_string(),
        },
    }
}

/// The roles a wake displaced: its `role:<r>` standby expectations.
fn displaced(decision: &DecisionView, snapshot: &Snapshot) -> Vec<String> {
    decision
        .expectations
        .iter()
        .filter(|expectation| expectation.expect == "standby")
        .filter_map(|expectation| expectation.subject.strip_prefix("role:"))
        .filter(|role| decision.role.as_deref() != Some(*role))
        .map(|role| role_name(snapshot, role))
        .collect()
}

fn command_headline(decision: &DecisionView, snapshot: &Snapshot) -> String {
    let Trigger::Command { command } = &decision.trigger else {
        return format!("Operator command ({})", decision.reason);
    };
    match command {
        Command::AllOff => "All off requested".to_owned(),
        Command::SetMode(mode) => format!("Mode set to {mode}"),
        Command::Identify(la) => format!("Identify LA {la} requested"),
        Command::Forget(la) => format!("LA {la} forgotten"),
        Command::SendRaw { to, data } => format!("Raw frame {} sent to LA {to}", hex_bytes(data)),
        Command::Takeover(role) => format!("{} take-over requested", role_name(snapshot, role)),
        Command::Standby(role) => format!("{} standby requested", role_name(snapshot, role)),
    }
}

/// The detail page's title sentence: what happened, then the outcome when it is not the
/// plain one (`Console A woke — room on`, `Console B woke; Streamer taken over — partial`).
pub(crate) fn headline(decision: &DecisionView, snapshot: &Snapshot) -> String {
    let who = decision
        .role
        .as_deref()
        .map_or_else(|| "Room".to_owned(), |role| role_name(snapshot, role));
    let group = reason_group(&decision.reason);
    let reason = decision.reason.as_str();
    let main = match (group, reason) {
        (_, "console_start") => format!("{who} started, seen by its probe"),
        (ReasonGroup::Wake, _) => match displaced(decision, snapshot) {
            others if others.is_empty() => format!("{who} woke"),
            others => format!("{who} woke; {} taken over", others.join(", ")),
        },
        (ReasonGroup::Standby, "taken_over") => format!("{who} taken over"),
        (ReasonGroup::Standby, "all_off") => format!("{who} off with all off"),
        (ReasonGroup::Standby, "presence_timeout") => {
            format!("{who} stopped answering presence polls")
        }
        (ReasonGroup::Standby, "manual_standby") => format!("{who} put in standby"),
        (ReasonGroup::Standby, "late_announcement") => {
            format!("{who} stopped: it announced itself late")
        }
        (ReasonGroup::Standby, "console_start_expired" | "console_start_retracted") => {
            format!("{who}'s early start withdrawn")
        }
        (ReasonGroup::Standby, _) => format!("{who} went to standby"),
        (ReasonGroup::Takeover, _) => format!("{who} take-over settled"),
        (ReasonGroup::Timer, _) => "Off timer expired".to_owned(),
        (ReasonGroup::Identity, "identity_lost") => format!("{who} lost its device"),
        (ReasonGroup::Identity, "conflict") => "Identity conflict".to_owned(),
        (ReasonGroup::Identity, "conflict_resolved") => "Identity conflict resolved".to_owned(),
        (ReasonGroup::Identity, "command_identify") => "Identify requested".to_owned(),
        (ReasonGroup::Identity, _) => "Device forgotten".to_owned(),
        (ReasonGroup::Drift, _) => match drift_row(decision) {
            Some(drift) if reason == "drift" => format!(
                "Drift: {}.{} reads {} instead of {}",
                drift.subject, drift.field, drift.observed, drift.intended
            ),
            Some(drift) => format!("Drift resolved: {}.{}", drift.subject, drift.field),
            None => format!("Drift ({reason})"),
        },
        (ReasonGroup::Command, _) => command_headline(decision, snapshot),
        (ReasonGroup::Service, "service_started") => "Choregos started".to_owned(),
        (ReasonGroup::Service, _) => format!("{who}: startup signal ignored"),
        (ReasonGroup::Other, "sonos_volume") => format!("{who} volume key forwarded"),
        (ReasonGroup::Other, _) => format!("{who}: {reason}"),
    };
    let outcome = match decision.verdict {
        Verdict::Partial | Verdict::Missed | Verdict::Unknown => Some(decision.verdict.to_string()),
        _ => match (group, reason) {
            (_, "console_start") => None,
            (ReasonGroup::Wake, _) => Some("room on".to_owned()),
            (ReasonGroup::Timer, _) | (_, "command_all_off") => Some("room off".to_owned()),
            _ => None,
        },
    };
    match outcome {
        Some(outcome) => format!("{main} — {outcome}"),
        None => main,
    }
}

/// The rule a group of reasons applies, in one static sentence (no config numbers).
pub(crate) fn rule_sentence(group: ReasonGroup) -> &'static str {
    match group {
        ReasonGroup::Wake => {
            "A role turned on while the room was off → wake the TV, turn the receiver on, then switch the receiver to the role's source once the wake settles (the follow-up row is linked below)."
        }
        ReasonGroup::Standby => {
            "The role turned off; if it was the active one the off timer starts."
        }
        ReasonGroup::Takeover => {
            "The settle delay elapsed; source and preset were issued if needed."
        }
        ReasonGroup::Timer => {
            "No role came back on before the off timer ran out → turn the TV and the receiver off."
        }
        ReasonGroup::Identity => {
            "Identity rows track which bus device plays which role; a role without its device, or with a contested one, cannot drive the room."
        }
        ReasonGroup::Drift => {
            "Drift is reporting only: the disagreement and its end are recorded, nothing is sent."
        }
        ReasonGroup::Command => {
            "An operator command goes through the same rules as a device signal; this row carries what it issued."
        }
        ReasonGroup::Service => {
            "Service rows mark a start of Choregos; rows before a start were made by the previous run."
        }
        ReasonGroup::Other => {
            "A remote key was forwarded to another device; room state is unchanged."
        }
    }
}

/// The glossary: every reason literal with its sentence, by group.
pub(crate) fn glossary() -> Vec<(ReasonGroup, Vec<(&'static str, &'static str)>)> {
    GROUPS
        .into_iter()
        .map(|group| {
            let reasons = REASONS
                .iter()
                .filter(|(_, of, _)| *of == group)
                .map(|(literal, _, sentence)| (*literal, *sentence))
                .collect();
            (group, reasons)
        })
        .collect()
}

/// One action of a decision, decoded for the actions table.
pub(crate) struct ActionRow {
    pub(crate) kind: String,
    pub(crate) target: String,
    pub(crate) detail: String,
    /// Identify, resync and presence polls: bus housekeeping, rendered dim.
    pub(crate) housekeeping: bool,
    /// Recorded with the runtime's `would: ` prefix: suppressed, not sent.
    pub(crate) would: bool,
}

impl ActionRow {
    pub(crate) fn status(&self) -> &'static str {
        if self.would { "would" } else { "sent" }
    }
}

/// Decodes the ring's action strings (optional `would: ` prefix + the action's JSON);
/// anything unparsable is shown verbatim.
pub(crate) fn action_rows(decision: &DecisionView, snapshot: &Snapshot) -> Vec<ActionRow> {
    let verifies_standby = decision
        .expectations
        .iter()
        .any(|expectation| expectation.subject.starts_with("role:"));
    let la = |value: &serde_json::Value| {
        value
            .as_u64()
            .and_then(|la| u8::try_from(la).ok())
            .map(|la| (la, format!("LA {la} · {}", la_name(snapshot, la))))
    };
    decision
        .actions
        .iter()
        .map(|action| {
            let (would, json) = match action.strip_prefix("would: ") {
                Some(json) => (true, json),
                None => (false, action.as_str()),
            };
            let raw = || ActionRow {
                kind: "—".to_owned(),
                target: String::new(),
                detail: json.to_owned(),
                housekeeping: false,
                would,
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
                return raw();
            };
            let field = |key: &str| value.get(key).and_then(serde_json::Value::as_str);
            let row = |kind: &str, target: String, detail: String, housekeeping| ActionRow {
                kind: kind.to_owned(),
                target,
                detail,
                housekeeping,
                would,
            };
            match field("kind") {
                Some("wake_tv") => row(
                    "wake_tv",
                    "TV".into(),
                    "via configured tv.wake".into(),
                    false,
                ),
                Some("tv_off") => row("tv_off", "TV".into(), "via configured tv.off".into(), false),
                Some("cec_send") => {
                    let Some((_, target)) = value.get("to").and_then(la) else {
                        return raw();
                    };
                    let data: Vec<u8> = value
                        .get("data")
                        .and_then(|data| serde_json::from_value(data.clone()).ok())
                        .unwrap_or_default();
                    let mut detail = match data.first() {
                        Some(byte) => format!("{} · {}", hex_bytes(&data), opcode::name(*byte)),
                        None => "poll".to_owned(),
                    };
                    if data == [opcode::GIVE_DEVICE_POWER_STATUS] && verifies_standby {
                        detail.push_str(" — standby verification");
                    }
                    row("cec_send", target, detail, false)
                }
                Some("ha_call") => {
                    let mut detail = format!(
                        "{}.{}",
                        field("domain").unwrap_or("?"),
                        field("service").unwrap_or("?")
                    );
                    if let Some(data) = value
                        .get("data")
                        .filter(|data| data.as_object().is_some_and(|map| !map.is_empty()))
                    {
                        detail.push(' ');
                        detail.push_str(&data.to_string());
                    }
                    row(
                        "ha_call",
                        field("entity").unwrap_or("").to_owned(),
                        detail,
                        false,
                    )
                }
                Some("identify") => {
                    let target = value
                        .get("la")
                        .and_then(la)
                        .map(|(_, t)| t)
                        .unwrap_or_default();
                    let queries: Vec<&str> = ["physical", "vendor", "osd"]
                        .into_iter()
                        .filter(|query| {
                            value
                                .pointer(&format!("/queries/{query}"))
                                .and_then(serde_json::Value::as_bool)
                                == Some(true)
                        })
                        .collect();
                    row(
                        "identify",
                        target,
                        format!("asks {}", queries.join(", ")),
                        true,
                    )
                }
                Some("resync") => row(
                    "resync",
                    "bus".into(),
                    "asks for the active source and every power status".into(),
                    true,
                ),
                Some("presence_poll") => {
                    let target = value
                        .get("la")
                        .and_then(la)
                        .map(|(_, t)| t)
                        .unwrap_or_default();
                    row(
                        "presence_poll",
                        target,
                        "8F · Give Device Power Status".into(),
                        true,
                    )
                }
                _ => raw(),
            }
        })
        .collect()
}

/// One expectation, rendered.
pub(crate) struct ExpectationRow {
    pub(crate) name: String,
    pub(crate) expect: String,
    /// Each allowed source; `true` for the one that settled it.
    pub(crate) via: Vec<(String, bool)>,
    pub(crate) deadline: String,
    pub(crate) attempts: String,
    pub(crate) outcome: String,
    pub(crate) tone: &'static str,
    pub(crate) latency: Option<String>,
    /// Why a role's standby could not be confirmed (03c), when it applies.
    pub(crate) explain: Option<String>,
}

/// Outcome words: `met via x`, `already` (satisfied before the decision), `missed`,
/// `unknown (absent)`, `unknown`, `cec_only`, `pending`; with the chip tone.
pub(crate) fn outcome_word(outcome: &Outcome) -> (String, &'static str) {
    match outcome {
        Outcome::Pending => ("pending".to_owned(), "pend"),
        Outcome::Met { latency_ms: 0, .. } => ("already".to_owned(), "ok"),
        Outcome::Met { via, .. } => (format!("met via {via}"), "ok"),
        Outcome::Missed { observed } => (format!("missed · read {observed}"), "bad"),
        Outcome::Unknown { observed } if observed.as_deref() == Some("absent") => {
            ("unknown (absent)".to_owned(), "dim")
        }
        Outcome::Unknown { .. } => ("unknown".to_owned(), "dim"),
        Outcome::CecOnly { .. } => ("cec_only".to_owned(), "ok"),
    }
}

/// A deadline in seconds (`45 s`, `2.5 s`).
fn seconds(millis: u64) -> String {
    if millis.is_multiple_of(1000) {
        format!("{} s", millis / 1000)
    } else {
        format!("{}.{} s", millis / 1000, millis % 1000 / 100)
    }
}

pub(crate) fn expectation_rows(
    decision: &DecisionView,
    snapshot: &Snapshot,
) -> Vec<ExpectationRow> {
    decision
        .expectations
        .iter()
        .map(|expectation| {
            let used = match &expectation.outcome {
                Outcome::Met { via, .. } => Some(*via),
                _ => None,
            };
            let (outcome, tone) = outcome_word(&expectation.outcome);
            ExpectationRow {
                name: format!("{}.{}", expectation.subject, expectation.field),
                expect: expectation.expect.clone(),
                via: expectation
                    .via
                    .iter()
                    .map(|via| (via.to_string(), used == Some(*via)))
                    .collect(),
                deadline: seconds(expectation.deadline_ms),
                attempts: match expectation.attempts {
                    0 => "—".to_owned(),
                    n => n.to_string(),
                },
                outcome,
                tone,
                latency: match &expectation.outcome {
                    Outcome::Met { latency_ms, .. } if *latency_ms > 0 => Some(latency(latency_ms)),
                    _ => None,
                },
                explain: unconfirmed_standby(expectation, snapshot),
            }
        })
        .collect()
}

/// The 03c explainer for a role whose standby was asked about and never answered.
fn unconfirmed_standby(expectation: &Expectation, snapshot: &Snapshot) -> Option<String> {
    let role = expectation.subject.strip_prefix("role:")?;
    if expectation.attempts == 0 || !matches!(expectation.outcome, Outcome::Unknown { .. }) {
        return None;
    }
    let resends = match expectation.attempts {
        1 => "1 resend".to_owned(),
        n => format!("{n} resends"),
    };
    let absent = matches!(&expectation.outcome, Outcome::Unknown { observed } if observed.as_deref() == Some("absent"));
    let answer = if absent {
        "never acknowledged a frame, so it is treated as absent from the bus"
    } else {
        "did not report its power status before the deadline, so Choregos cannot tell whether it is asleep or still playing"
    };
    Some(format!(
        "{} was asked for its power status {} times within {} (the first ask and {resends}) and {answer}. \
         Attempts counts CEC resends of the query. unknown: the device was asked but never answered in time; \
         unknown (absent): it never acknowledged the frame at all; cec_only: the only evidence is the device's own CEC report.",
        role_name(snapshot, role),
        u32::from(expectation.attempts) + 1,
        seconds(expectation.deadline_ms),
    ))
}

/// `2 of 2 expectations met · slowest 15.6 s` (`cec_only` counts as met).
pub(crate) fn met_summary(decision: &DecisionView) -> Option<String> {
    let total = decision.expectations.len();
    if total == 0 {
        return None;
    }
    let met = decision
        .expectations
        .iter()
        .filter(|expectation| {
            matches!(
                expectation.outcome,
                Outcome::Met { .. } | Outcome::CecOnly { .. }
            )
        })
        .count();
    let mut summary = format!("{met} of {total} expectations met");
    if let Some(slowest) = max_latency(decision).filter(|ms| *ms > 0) {
        summary.push_str(&format!(" · slowest {}", latency(&slowest)));
    }
    Some(summary)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::{
        action::{DriftDetail, Field, ServiceKind, TimerKind, Via},
        state::{Mode, RolePower},
        web::view::tests::{row, snapshot},
    };

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, day, hour, 0, 0).unwrap()
    }

    fn pairs(query: &str) -> Vec<(String, String)> {
        form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect()
    }

    fn filter(query: &str) -> ListFilter {
        ListFilter::parse(&pairs(query)).unwrap()
    }

    /// A ring of eight rows over two days covering every filter dimension.
    fn ring() -> Vec<DecisionView> {
        let at_row = |id, role: Option<&str>, reason: &str, when| DecisionView {
            timestamp: when,
            reason: reason.to_owned(),
            ..row(id, role, vec![])
        };
        let mut migrated = at_row(1, Some("streamer"), "ActiveSource", at(5, 10));
        migrated.trigger = Trigger::Service {
            name: ServiceKind::Migrated,
        };
        let mut drift = at_row(2, None, "drift", at(6, 9));
        drift.verdict = Verdict::None;
        let mut would = at_row(3, Some("console"), "Standby", at(6, 20));
        would.would = true;
        would.verdict = Verdict::None;
        let mut missed = at_row(4, Some("streamer"), "PowerOn", at(7, 8));
        missed.verdict = Verdict::Missed;
        let mut timer = at_row(5, None, "off_timer_expired", at(7, 11));
        timer.verdict = Verdict::Partial;
        vec![
            migrated,
            drift,
            would,
            missed,
            timer,
            at_row(
                6,
                Some("console"),
                "ActiveSource",
                at(7, 11) + TimeDelta::minutes(30),
            ),
        ]
    }

    fn ids(filter: &ListFilter, ring: &[DecisionView], now: DateTime<Utc>) -> Vec<u64> {
        filter
            .page(ring.iter(), now)
            .rows
            .iter()
            .map(|d| d.id)
            .collect()
    }

    #[test]
    fn each_list_filter_alone_and_combined() {
        let ring = ring();
        let now = at(7, 12);
        let ids = |query: &str| ids(&filter(query), &ring, now);
        // Migrated history is hidden unless asked for; newest first.
        assert_eq!(ids(""), [6, 5, 4, 3, 2]);
        assert_eq!(ids("migrated=show"), [6, 5, 4, 3, 2, 1]);
        assert_eq!(ids("role=room"), [5, 2]);
        assert_eq!(ids("role=streamer"), [4]);
        assert_eq!(ids("group=drift"), [2]);
        assert_eq!(ids("group=wake,timer"), [6, 5, 4]);
        assert_eq!(ids("group=wake&group=timer"), [6, 5, 4]);
        assert_eq!(ids("verdict=none"), [3, 2]);
        assert_eq!(ids("verdict=missed,partial"), [5, 4]);
        assert_eq!(ids("would=only"), [3]);
        assert_eq!(ids("would=hide"), [6, 5, 4, 2]);
        assert_eq!(ids("reason=ActiveSource"), [6]);
        assert_eq!(ids("reason=ActiveSource&migrated=show"), [6, 1]);
        assert_eq!(ids("range=1h"), [6, 5]);
        assert_eq!(ids("range=today"), [6, 5, 4]);
        assert_eq!(ids("range=6h"), [6, 5, 4]);
        assert_eq!(
            ids("range=custom&from=2026-11-06T00:00:00Z&to=2026-11-06T23:59"),
            [3, 2]
        );
        // Combined: every filter must pass.
        assert_eq!(
            ids("role=room&group=timer&verdict=partial&range=today"),
            [5]
        );
        assert_eq!(ids("role=console&would=hide"), [6]);
        assert!(ids("role=room&group=wake").is_empty());

        for bad in [
            "group=nap",
            "verdict=great",
            "would=maybe",
            "range=year",
            "before=x",
            "from=yesterday",
        ] {
            assert!(ListFilter::parse(&pairs(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn query_round_trips_and_omits_defaults_and_the_cursor() {
        let query = "role=room&group=wake,drift&verdict=partial&would=hide&range=custom&from=2026-11-06T00:00:00Z&migrated=show&before=40";
        let parsed = filter(query);
        assert_eq!(
            parsed.query(),
            "role=room&group=wake%2Cdrift&verdict=partial&would=hide&range=custom&from=2026-11-06T00%3A00%3A00Z&migrated=show"
        );
        assert_eq!(
            filter(&parsed.query()),
            ListFilter {
                before: None,
                ..parsed.clone()
            }
        );
        assert_eq!(filter("range=2w&would=all").query(), "");
        assert!(!filter("before=9").is_filtered());
        assert!(parsed.is_filtered());
        assert_eq!(filter("").url(Some(9)), "/recorder?before=9");
        assert_eq!(filter("role=tv").rows_url(), "/p/recorder?role=tv");
    }

    #[test]
    fn fractional_custom_bounds_survive_the_canonical_url_and_form() {
        let parsed =
            filter("range=custom&from=2026-11-07T23:54:02.000Z&to=2026-11-07T23:54:03.999123Z");
        assert_eq!(
            parsed.rows_url(),
            "/p/recorder?range=custom&from=2026-11-07T23%3A54%3A02Z&to=2026-11-07T23%3A54%3A03.999123Z"
        );
        assert_eq!(filter(&parsed.query()), parsed, "lossless");
        assert!(parsed.custom_seconds());
        assert_eq!(parsed.custom_from(), "2026-11-07T23:54:02");
        assert_eq!(parsed.custom_to(), "2026-11-07T23:54:03.999");
        // The form's own values parse back; whole minutes stay minute-precise.
        let form = filter("range=custom&from=2026-11-07T23:54:03.999&to=2026-11-07T23:55");
        assert_eq!(form.to.unwrap().nanosecond(), 0);
        assert_eq!(form.from.unwrap().nanosecond(), 999_000_000);
        assert_eq!(form.custom_to(), "2026-11-07T23:55");
    }

    #[test]
    fn pages_hold_a_hundred_rows_and_the_cursor_is_exclusive() {
        let now = at(7, 12);
        let ring: Vec<_> = (1..=250)
            .map(|id| DecisionView {
                timestamp: now - TimeDelta::seconds(300 - i64::try_from(id).unwrap()),
                ..row(id, None, vec![])
            })
            .collect();
        let first = filter("").page(ring.iter(), now);
        assert_eq!(first.total, 250);
        assert_eq!(first.rows.len(), PAGE_ROWS);
        assert_eq!((first.rows[0].id, first.rows[99].id), (250, 151));
        assert_eq!(first.older, Some(151));
        let second = filter("before=151").page(ring.iter(), now);
        assert_eq!((second.rows[0].id, second.rows[99].id), (150, 51));
        assert_eq!(second.older, Some(51));
        // The last page is short and offers nothing older.
        let last = filter("before=51").page(ring.iter(), now);
        assert_eq!(last.rows.len(), 50);
        assert_eq!(last.older, None);
        // Exactly one page: full, but nothing older.
        let exact = filter("before=101").page(ring.iter(), now);
        assert_eq!(exact.rows.len(), 100);
        assert_eq!(exact.older, None);
    }

    #[test]
    fn day_separators_name_today_yesterday_then_the_date() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 7).unwrap();
        let day = |d| NaiveDate::from_ymd_opt(2026, 11, d).unwrap();
        assert_eq!(day_label(today, today), "Today · Sat 7 Nov");
        assert_eq!(day_label(day(6), today), "Yesterday · Fri 6 Nov");
        assert_eq!(day_label(day(5), today), "Thu 5 Nov");
        let last_year = NaiveDate::from_ymd_opt(2025, 12, 31).unwrap();
        assert_eq!(day_label(last_year, today), "Wed 31 Dec 2025");

        let mut ring = ring();
        let mut started = ring[2].clone();
        started.id = 10;
        started.reason = "service_started".to_owned();
        started.timestamp = at(6, 22);
        ring.insert(3, started);
        let rows: Vec<_> = ring.iter().rev().collect();
        let shape: Vec<String> = list_items(&rows, &snapshot(), today)
            .iter()
            .map(|item| match item {
                ListItem::Day(label) => label.clone(),
                ListItem::Restart(decision) => format!("restart #{}", decision.id),
                ListItem::Row(row) => format!("#{}", row.decision.id),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "Today · Sat 7 Nov",
                "#6",
                "#5",
                "#4",
                "Yesterday · Fri 6 Nov",
                "restart #10",
                "#3",
                "#2",
                "Thu 5 Nov",
                "#1",
            ]
        );
    }

    /// The view-test snapshot with the streamer bound at LA 4 and the console at LA 11.
    fn bound() -> Snapshot {
        let mut snapshot = snapshot();
        snapshot.roles[0].logical_address = Some(4);
        snapshot.roles[1].logical_address = Some(11);
        snapshot
    }

    fn met(latency_ms: u64) -> Outcome {
        Outcome::Met {
            via: Via::TvDirect,
            observed: "on".to_owned(),
            latency_ms,
        }
    }

    fn expect(subject: &str, value: &str, outcome: Outcome) -> Expectation {
        Expectation {
            subject: subject.to_owned(),
            field: Field::Power,
            expect: value.to_owned(),
            via: vec![Via::TvDirect, Via::Ha],
            deadline_ms: 6_000,
            attempts: 0,
            outcome,
        }
    }

    #[test]
    fn headline_per_reason_group() {
        let snapshot = bound();
        let headline = |decision: &DecisionView| headline(decision, &snapshot);
        let wake = DecisionView {
            reason: "ActiveSource".to_owned(),
            ..row(1, Some("streamer"), vec![expect("tv", "on", met(7_000))])
        };
        assert_eq!(headline(&wake), "Streamer woke — room on");
        let takeover = DecisionView {
            reason: "TextViewOn".to_owned(),
            verdict: Verdict::Partial,
            ..row(
                2,
                Some("console"),
                vec![
                    expect("tv", "on", met(7_000)),
                    expect(
                        "role:streamer",
                        "standby",
                        Outcome::Unknown { observed: None },
                    ),
                ],
            )
        };
        assert_eq!(
            headline(&takeover),
            "Console woke; Streamer taken over — partial"
        );
        let standby = DecisionView {
            reason: "Standby".to_owned(),
            from: Some(RolePower::On),
            to: Some(RolePower::Off),
            verdict: Verdict::None,
            ..row(3, Some("streamer"), vec![])
        };
        assert_eq!(headline(&standby), "Streamer went to standby");
        let taken = DecisionView {
            reason: "taken_over".to_owned(),
            ..standby.clone()
        };
        assert_eq!(headline(&taken), "Streamer taken over");
        let settled = DecisionView {
            reason: "takeover_settled".to_owned(),
            from: None,
            to: None,
            ..row(4, Some("console"), vec![])
        };
        assert_eq!(headline(&settled), "Console take-over settled");
        let timer = DecisionView {
            reason: "off_timer_expired".to_owned(),
            trigger: Trigger::Timer {
                name: TimerKind::RoomOff,
            },
            ..row(5, None, vec![])
        };
        assert_eq!(headline(&timer), "Off timer expired — room off");
        let missed_timer = DecisionView {
            verdict: Verdict::Missed,
            ..timer
        };
        assert_eq!(headline(&missed_timer), "Off timer expired — missed");
        let drift = DriftDetail {
            subject: "receiver".to_owned(),
            field: Field::Power,
            intended: "on".to_owned(),
            observed: "off".to_owned(),
            via: Via::Ha,
            since: Utc::now(),
        };
        let onset = DecisionView {
            reason: "drift".to_owned(),
            verdict: Verdict::None,
            drift: Some(drift),
            ..row(6, None, vec![])
        };
        assert_eq!(
            headline(&onset),
            "Drift: receiver.power reads off instead of on"
        );
        let resolved = DecisionView {
            reason: "drift_resolved".to_owned(),
            ..onset
        };
        assert_eq!(headline(&resolved), "Drift resolved: receiver.power");
        let command = |command, reason: &str| DecisionView {
            reason: reason.to_owned(),
            trigger: Trigger::Command { command },
            ..row(7, None, vec![])
        };
        assert_eq!(
            headline(&command(Command::SetMode(Mode::Shadow), "command_set_mode")),
            "Mode set to shadow"
        );
        assert_eq!(
            headline(&command(Command::AllOff, "command_all_off")),
            "All off requested — room off"
        );
        let started = DecisionView {
            reason: "service_started".to_owned(),
            verdict: Verdict::None,
            ..row(8, None, vec![])
        };
        assert_eq!(headline(&started), "Choregos started");
    }

    #[test]
    fn trigger_renders_per_kind_with_names_and_decoded_operands() {
        let snapshot = bound();
        let frame = |from, to, data: &str| Trigger::Frame {
            from,
            to,
            data: data.to_owned(),
        };
        let report = frame(11, 15, "84230004");
        assert_eq!(trigger_compact(&report), "11 → 15 84 23 00 04");
        let view = trigger_view(&report, &snapshot);
        assert_eq!(view.kind, "frame");
        let rendered = view.frame.unwrap();
        assert_eq!(
            (rendered.from_name.as_str(), rendered.to_name.as_str()),
            ("Console", "broadcast")
        );
        assert_eq!(rendered.opcode.as_deref(), Some("84"));
        assert_eq!(rendered.opcode_name, "Report Physical Address");
        assert_eq!(
            rendered.params.as_deref(),
            Some("PA 2.3.0.0 · type 04 playback")
        );

        let active = trigger_view(&frame(4, 15, "822100"), &snapshot)
            .frame
            .unwrap();
        assert_eq!(
            (active.from_name.as_str(), active.params.as_deref()),
            ("Streamer", Some("PA 2.1.0.0"))
        );
        let text_view_on = trigger_view(&frame(11, 0, "0D"), &snapshot).frame.unwrap();
        assert_eq!(
            (text_view_on.to_name.as_str(), text_view_on.params),
            ("TV", None)
        );
        // Unknown device and operands without a fixed meaning: address and hex.
        let other = trigger_view(&frame(8, 4, "449D"), &snapshot).frame.unwrap();
        assert_eq!(
            (other.from_name.as_str(), other.params.as_deref()),
            ("LA 8", Some("9D"))
        );
        let poll = trigger_view(&frame(4, 4, ""), &snapshot).frame.unwrap();
        assert_eq!((poll.opcode, poll.opcode_name.as_str()), (None, "poll"));
        assert_eq!(trigger_compact(&frame(4, 4, "")), "4 → 4");

        let timer = Trigger::Timer {
            name: TimerKind::RoomOff,
        };
        assert_eq!(trigger_compact(&timer), "timer room_off");
        assert_eq!(trigger_view(&timer, &snapshot).text, "room_off");
        let command = Trigger::Command {
            command: Command::Takeover("streamer".to_owned()),
        };
        assert_eq!(trigger_compact(&command), "command takeover streamer");
        assert_eq!(
            trigger_view(&command, &snapshot).text,
            r#"{"kind":"takeover","data":"streamer"}"#
        );
        let observation = Trigger::Observation { source: Via::Ps5 };
        assert_eq!(trigger_compact(&observation), "obs ps5");
        assert_eq!(trigger_view(&observation, &snapshot).kind, "observation");
        let service = Trigger::Service {
            name: ServiceKind::Started,
        };
        assert_eq!(trigger_compact(&service), "service started");
    }

    #[test]
    fn action_table_decodes_kinds_housekeeping_and_the_would_prefix() {
        use crate::action::{Action, IdentifyQueries};
        let json = |action: &Action| serde_json::to_string(action).unwrap();
        let snapshot = bound();
        let decision = DecisionView {
            actions: vec![
                json(&Action::WakeTv),
                format!(
                    "would: {}",
                    json(&Action::HaCall {
                        domain: "media_player".to_owned(),
                        service: "select_source".to_owned(),
                        entity: "media_player.test_receiver".to_owned(),
                        data: serde_json::json!({"source": "Console"}),
                    })
                ),
                json(&Action::CecSend {
                    to: 4,
                    data: vec![0x41, 0x25],
                }),
                json(&Action::CecSend {
                    to: 4,
                    data: vec![0x8F],
                }),
                json(&Action::Identify {
                    la: 11,
                    queries: IdentifyQueries {
                        physical: false,
                        vendor: true,
                        osd: true,
                    },
                }),
                json(&Action::Resync),
                json(&Action::PresencePoll { la: 11 }),
                "not json".to_owned(),
            ],
            ..row(
                1,
                Some("console"),
                vec![expect("role:streamer", "standby", Outcome::Pending)],
            )
        };
        let rows = action_rows(&decision, &snapshot);
        let table: Vec<(&str, &str, &str, bool, &str)> = rows
            .iter()
            .map(|row| {
                (
                    row.kind.as_str(),
                    row.target.as_str(),
                    row.detail.as_str(),
                    row.housekeeping,
                    row.status(),
                )
            })
            .collect();
        assert_eq!(
            table,
            [
                ("wake_tv", "TV", "via configured tv.wake", false, "sent"),
                (
                    "ha_call",
                    "media_player.test_receiver",
                    r#"media_player.select_source {"source":"Console"}"#,
                    false,
                    "would"
                ),
                ("cec_send", "LA 4 · Streamer", "41 25 · Play", false, "sent"),
                (
                    "cec_send",
                    "LA 4 · Streamer",
                    "8F · Give Device Power Status — standby verification",
                    false,
                    "sent"
                ),
                (
                    "identify",
                    "LA 11 · Console",
                    "asks vendor, osd",
                    true,
                    "sent"
                ),
                (
                    "resync",
                    "bus",
                    "asks for the active source and every power status",
                    true,
                    "sent"
                ),
                (
                    "presence_poll",
                    "LA 11 · Console",
                    "8F · Give Device Power Status",
                    true,
                    "sent"
                ),
                ("—", "", "not json", false, "sent"),
            ]
        );
        // Without a role expectation an 8F is just a query.
        let plain = DecisionView {
            expectations: vec![],
            ..decision
        };
        assert_eq!(
            action_rows(&plain, &snapshot)[3].detail,
            "8F · Give Device Power Status"
        );
    }

    #[test]
    fn expectation_rows_word_outcomes_and_explain_unanswered_standby() {
        let snapshot = bound();
        let mut unanswered = expect(
            "role:streamer",
            "standby",
            Outcome::Unknown { observed: None },
        );
        unanswered.attempts = 2;
        unanswered.via = vec![Via::Cec];
        let mut absent = unanswered.clone();
        absent.outcome = Outcome::Unknown {
            observed: Some("absent".to_owned()),
        };
        let decision = row(
            1,
            Some("console"),
            vec![
                expect("tv", "on", met(15_600)),
                expect("receiver", "on", met(0)),
                unanswered,
                absent,
                expect(
                    "tv",
                    "off",
                    Outcome::Missed {
                        observed: "on".to_owned(),
                    },
                ),
                expect(
                    "role:console",
                    "standby",
                    Outcome::CecOnly {
                        observed: "standby".to_owned(),
                    },
                ),
            ],
        );
        let rows = expectation_rows(&decision, &snapshot);
        let words: Vec<&str> = rows.iter().map(|row| row.outcome.as_str()).collect();
        assert_eq!(
            words,
            [
                "met via tv_direct",
                "already",
                "unknown",
                "unknown (absent)",
                "missed · read on",
                "cec_only"
            ]
        );
        assert_eq!(
            rows[0].via,
            [("tv_direct".to_owned(), true), ("ha".to_owned(), false)]
        );
        assert_eq!(rows[0].latency.as_deref(), Some("15.6 s"));
        assert_eq!(rows[1].latency, None, "already satisfied is not a latency");
        assert_eq!(
            (rows[2].attempts.as_str(), rows[2].deadline.as_str()),
            ("2", "6 s")
        );
        let explain = rows[2].explain.as_deref().unwrap();
        assert!(explain.starts_with("Streamer was asked for its power status 3 times within 6 s (the first ask and 2 resends)"), "{explain}");
        assert!(
            explain.contains("cannot tell whether it is asleep"),
            "{explain}"
        );
        assert!(
            rows[3]
                .explain
                .as_deref()
                .unwrap()
                .contains("treated as absent")
        );
        assert!(rows.iter().filter(|row| row.explain.is_some()).count() == 2);
        assert_eq!(
            met_summary(&decision).as_deref(),
            Some("3 of 6 expectations met · slowest 15.6 s")
        );
    }

    #[test]
    fn glossary_lists_every_literal_once_by_group() {
        let glossary = glossary();
        assert_eq!(glossary.len(), GROUPS.len());
        let count: usize = glossary.iter().map(|(_, reasons)| reasons.len()).sum();
        assert_eq!(count, REASONS.len());
        assert_eq!(
            glossary[3],
            (
                ReasonGroup::Timer,
                vec![(
                    "off_timer_expired",
                    super::super::reason_sentence("off_timer_expired").unwrap()
                )]
            )
        );
    }
}
