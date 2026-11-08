//! The bus window of a decision: frames, observations and decision rows around it, from the
//! trace day files or from memory, tagged by how they relate to the decision.
//!
//! Trace entries are decoded with the bridge correlation that live ingress and replay share
//! (`runtime::bridge::Correlation`), so the window never re-implements a wire format.

use std::{collections::HashMap, time::Instant};

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;

use super::{hex_bytes, latency, recorder::la_name};
use crate::{
    action::{Field, Outcome, Trigger, Verdict, Via},
    cec::{Frame, frame::decode_osd, opcode},
    engine::{Event, SendStatus},
    runtime::{
        bridge::{Correlation, Decoded, SendKey},
        trace::TraceEntry,
    },
    state::{DecisionView, FrameDirection, Snapshot},
};

/// How close a frame must be to the decision to be its trigger.
const TRIGGER_SLACK: TimeDelta = TimeDelta::seconds(2);
/// How soon after one of ours a frame to the bridge counts as its reply.
const REPLY_WITHIN: TimeDelta = TimeDelta::seconds(1);
/// How close an observation must be to `timestamp + latency_ms` to have settled it.
const SETTLE_SLACK: TimeDelta = TimeDelta::milliseconds(1500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dir {
    Rx,
    Tx,
    Local,
}

impl Dir {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Rx => "rx",
            Self::Tx => "tx",
            Self::Local => "local",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tag {
    /// The frame that caused the decision.
    Trigger,
    /// Sent by Choregos or answered locally by the bridge.
    Ours,
    /// Addressed to the bridge shortly after one of ours.
    Reply,
    Other,
}

impl Tag {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Trigger => "trigger",
            Self::Ours => "ours",
            Self::Reply => "reply",
            Self::Other => "other",
        }
    }

    pub(crate) fn tone(self) -> &'static str {
        match self {
            Self::Trigger => "info",
            Self::Ours => "ours",
            Self::Reply => "ok",
            Self::Other => "dim",
        }
    }
}

pub(crate) struct WindowFrame {
    pub(crate) dir: Dir,
    pub(crate) from: u8,
    pub(crate) to: u8,
    pub(crate) from_name: String,
    pub(crate) to_name: String,
    /// Opcode and operands; empty for a header-only poll.
    pub(crate) bytes: Vec<u8>,
    /// Acknowledgement of a received or local frame, or the send outcome of ours.
    pub(crate) result: Option<String>,
    pub(crate) tag: Tag,
}

impl WindowFrame {
    fn new(dir: Dir, frame: &Frame, snapshot: &Snapshot) -> Self {
        let mut bytes: Vec<u8> = frame.opcode.into_iter().collect();
        bytes.extend(&frame.params);
        Self {
            dir,
            from: frame.from,
            to: frame.to,
            from_name: la_name(snapshot, frame.from),
            to_name: la_name(snapshot, frame.to),
            bytes,
            result: frame.ack.map(ack_word),
            tag: Tag::Other,
        }
    }

    pub(crate) fn is_poll(&self) -> bool {
        self.bytes.is_empty()
    }

    pub(crate) fn data(&self) -> String {
        if self.is_poll() {
            "—".to_owned()
        } else {
            hex_bytes(&self.bytes)
        }
    }

    /// The opcode name, `poll` for a header-only frame, with the OSD name or power status
    /// spelled out and local replies marked.
    pub(crate) fn meaning(&self) -> String {
        let Some((&code, params)) = self.bytes.split_first() else {
            return "poll".to_owned();
        };
        let mut meaning = opcode::name(code).to_owned();
        match (code, params.first()) {
            (opcode::SET_OSD_NAME, Some(_)) => {
                meaning.push_str(&format!(" \"{}\"", decode_osd(params)));
            }
            (opcode::REPORT_POWER_STATUS, Some(status)) => meaning.push_str(match status {
                0 => ": on",
                1 => ": standby",
                2 => ": turning on",
                3 => ": turning standby",
                _ => "",
            }),
            _ => {}
        }
        if self.dir == Dir::Local {
            meaning.push_str(" (bridge reply)");
        }
        meaning
    }
}

fn ack_word(ack: bool) -> String {
    if ack { "ack" } else { "nack" }.to_owned()
}

fn status_word(status: SendStatus) -> &'static str {
    match status {
        SendStatus::Ack => "ack",
        SendStatus::Nack => "nack",
        SendStatus::ArbLost => "arb_lost",
        SendStatus::LineError => "line_error",
        SendStatus::Busy => "busy",
    }
}

fn send_word(word: &str, attempts: Option<u8>) -> String {
    match attempts {
        Some(attempts) if attempts > 1 => format!("{word} · {attempts} attempts"),
        _ => word.to_owned(),
    }
}

/// A device observation from the trace (direct pollers or Home Assistant).
pub(crate) struct Observation {
    /// The trace topic: `tv/state`, `receiver/state`, `probe/state`, `ha/state`.
    pub(crate) topic: &'static str,
    /// The source an expectation names it by; `None` when it cannot settle one.
    pub(crate) via: Option<Via>,
    pub(crate) value: String,
    /// The receiver's previous sample in this window, when it differs.
    pub(crate) previous: Option<String>,
    /// Home Assistant entity and its `source` attribute.
    pub(crate) entity: Option<String>,
    pub(crate) source: Option<String>,
    /// The probed role.
    pub(crate) role: Option<String>,
    /// When the sample was taken, if the observation says (Home Assistant: `last_updated`).
    pub(crate) sample_at: Option<DateTime<Utc>>,
    /// Home Assistant's `last_changed`, the sample time of power and playback.
    pub(crate) changed: Option<DateTime<Utc>>,
    /// `— tv.power met (15.6 s)` when it settled one of the decision's expectations.
    pub(crate) note: Option<String>,
}

impl Observation {
    fn new(topic: &'static str, via: Option<Via>, value: impl Into<String>) -> Self {
        Self {
            topic,
            via,
            value: value.into(),
            previous: None,
            entity: None,
            source: None,
            role: None,
            sample_at: None,
            changed: None,
            note: None,
        }
    }

    /// `tv/state unreachable · tv_direct`, `receiver/state off → on · denon`,
    /// `probe/state standby · ps5`, `ha/state media_player.x on`.
    pub(crate) fn text(&self) -> String {
        let value = match &self.previous {
            Some(previous) => format!("{previous} → {}", self.value),
            None => self.value.clone(),
        };
        match (&self.entity, self.via) {
            (Some(entity), _) => match &self.source {
                Some(source) => format!("{} {entity} {value} · source {source}", self.topic),
                None => format!("{} {entity} {value}", self.topic),
            },
            (None, Some(via)) => format!("{} {value} · {via}", self.topic),
            (None, None) => format!("{} {value}", self.topic),
        }
    }
}

/// A decision row inside the window.
pub(crate) struct WindowDecision {
    pub(crate) id: u64,
    pub(crate) timestamp: DateTime<Utc>,
    pub(crate) role: Option<String>,
    pub(crate) reason: String,
    pub(crate) verdict: Verdict,
    pub(crate) would: bool,
    pub(crate) parent: Option<u64>,
    /// The decision the window is about.
    pub(crate) own: bool,
    /// Still in the ring (same id and time), so its detail page exists.
    pub(crate) linked: bool,
}

pub(crate) enum Item {
    Frame(WindowFrame),
    Obs(Observation),
    Decision(WindowDecision),
}

pub(crate) struct Entry {
    pub(crate) at: DateTime<Utc>,
    pub(crate) item: Item,
}

fn in_range(at: DateTime<Utc>, from: DateTime<Utc>, to: DateTime<Utc>) -> bool {
    from <= at && at <= to
}

fn ring_decision(decision: &DecisionView, own: &DecisionView) -> WindowDecision {
    WindowDecision {
        id: decision.id,
        timestamp: decision.timestamp,
        role: decision.role.clone(),
        reason: decision.reason.clone(),
        verdict: decision.verdict,
        would: decision.would,
        parent: decision.parent,
        own: decision.id == own.id && decision.timestamp == own.timestamp,
        linked: true,
    }
}

/// The window from the in-memory frames and ring; memory holds no observations.
pub(crate) fn from_memory(
    snapshot: &Snapshot,
    decision: &DecisionView,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<Entry> {
    let mut entries: Vec<Entry> = snapshot
        .recent_frames
        .iter()
        .filter(|frame| in_range(frame.timestamp, from, to))
        .map(|view| {
            let dir = match view.direction {
                FrameDirection::Received => Dir::Rx,
                FrameDirection::Sent => Dir::Tx,
                FrameDirection::Local => Dir::Local,
            };
            let mut bytes: Vec<u8> = view.opcode.into_iter().collect();
            bytes.extend(&view.params);
            Entry {
                at: view.timestamp,
                item: Item::Frame(WindowFrame {
                    dir,
                    from: view.from,
                    to: view.to,
                    from_name: la_name(snapshot, view.from),
                    to_name: la_name(snapshot, view.to),
                    bytes,
                    result: view.ack.map(ack_word),
                    tag: Tag::Other,
                }),
            }
        })
        .collect();
    entries.extend(
        snapshot
            .decisions
            .iter()
            .filter(|row| in_range(row.timestamp, from, to))
            .map(|row| Entry {
                at: row.timestamp,
                item: Item::Decision(ring_decision(row, decision)),
            }),
    );
    // Stable: at equal times a frame stays above the decision it caused.
    entries.sort_by_key(|entry| entry.at);
    entries
}

/// A `choregos/decision` trace payload as a window row.
fn trace_decision(at: DateTime<Utc>, payload: &Value) -> Option<WindowDecision> {
    Some(WindowDecision {
        id: payload.get("id")?.as_u64()?,
        timestamp: payload
            .get("timestamp")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or(at),
        role: payload
            .get("role")
            .and_then(Value::as_str)
            .map(str::to_owned),
        reason: payload.get("reason")?.as_str()?.to_owned(),
        verdict: payload
            .get("verdict")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default(),
        would: payload
            .get("would")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        parent: payload.get("parent").and_then(Value::as_u64),
        own: false,
        linked: false,
    })
}

/// The window from trace records (already bounded to it), in trace order. Bridge records
/// go through one fresh correlation, as in replay: a send outcome becomes the result of
/// the send the adapter matched it to (Tasmota FIFO position, aether id), directed NACKs
/// keep their full bytes, local replies are `local` rows and an aether host transmission
/// with no recorded send is still an `ours` row. The decision itself is always a row.
pub(crate) fn from_trace(
    records: Vec<(DateTime<Utc>, TraceEntry)>,
    snapshot: &Snapshot,
    decision: &DecisionView,
) -> Vec<Entry> {
    let start = Instant::now();
    let epoch = records.first().map(|(at, _)| *at);
    let mut correlation = Correlation::default();
    // The row of each recorded send, by the key the adapter answers it with.
    let mut sends: HashMap<SendKey, usize> = HashMap::new();
    let mut entries: Vec<Entry> = Vec::with_capacity(records.len() + 1);
    let mut receiver: Option<String> = None;
    for (at, record) in records {
        let now = start
            + epoch
                .and_then(|epoch| (at - epoch).to_std().ok())
                .unwrap_or_default();
        let observation = |observation| Entry {
            at,
            item: Item::Obs(observation),
        };
        match record {
            TraceEntry::Sent { to, data, id, .. } => {
                let sent = Frame {
                    from: snapshot.bridge_logical_address,
                    to,
                    opcode: data.first().copied(),
                    params: data.get(1..).unwrap_or_default().to_vec(),
                    ack: None,
                    t_us: None,
                };
                let aether = id.clone().map(SendKey::Id);
                let fifo = correlation
                    .record_send(id, to, data, now)
                    .map(SendKey::Fifo);
                if let Some(key) = aether.or(fifo) {
                    sends.insert(key, entries.len());
                }
                entries.push(Entry {
                    at,
                    item: Item::Frame(WindowFrame::new(Dir::Tx, &sent, snapshot)),
                });
            }
            TraceEntry::Received { .. }
            | TraceEntry::Lwt { .. }
            | TraceEntry::Result { .. }
            | TraceEntry::AetherStatus { .. }
            | TraceEntry::AetherBus { .. } => {
                let decoded = correlation.apply(&record, now);
                bridge_rows(&mut entries, &mut sends, at, decoded, snapshot);
            }
            TraceEntry::Connection { .. } => correlation.clear(),
            TraceEntry::HaState {
                entity,
                state,
                attrs,
                changed,
                updated,
            } => entries.push(observation(Observation {
                entity: Some(entity),
                source: attrs
                    .get("source")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                sample_at: updated,
                changed,
                ..Observation::new("ha/state", Some(Via::Ha), state)
            })),
            TraceEntry::TvPower { state, at: sample } => entries.push(observation(Observation {
                sample_at: Some(sample),
                ..Observation::new("tv/state", Some(Via::TvDirect), state.to_string())
            })),
            TraceEntry::TvDirectUnavailable => entries.push(observation(Observation::new(
                "tv/state",
                None,
                "unavailable · tv_direct (pairing required)",
            ))),
            TraceEntry::ReceiverPower {
                state, at: sample, ..
            } => {
                let value = state.to_string();
                let previous = receiver.replace(value.clone()).filter(|old| *old != value);
                entries.push(observation(Observation {
                    previous,
                    sample_at: Some(sample),
                    ..Observation::new("receiver/state", Some(Via::Denon), value)
                }));
            }
            TraceEntry::ConsolePower {
                role,
                state,
                at: sample,
            } => entries.push(observation(Observation {
                role: Some(role),
                sample_at: Some(sample),
                ..Observation::new("probe/state", Some(Via::Ps5), state.to_string())
            })),
            TraceEntry::Decision(payload) => {
                if let Some(row) = trace_decision(at, &payload) {
                    entries.push(Entry {
                        at,
                        item: Item::Decision(row),
                    });
                }
            }
            TraceEntry::Verdict(payload) => settle_trace_verdict(&mut entries, &payload),
            TraceEntry::Command(_) | TraceEntry::Identities(_) => {}
        }
    }
    link_ring_rows(&mut entries, snapshot, decision);
    entries
}

/// The rows of one decoded bridge record. Results attach by the adapter's send key, never
/// by searching the rows for similar bytes.
fn bridge_rows(
    entries: &mut Vec<Entry>,
    sends: &mut HashMap<SendKey, usize>,
    at: DateTime<Utc>,
    mut decoded: Decoded,
    snapshot: &Snapshot,
) {
    let row = |dir, frame: &Frame| Entry {
        at,
        item: Item::Frame(WindowFrame::new(dir, frame, snapshot)),
    };
    let answered = decoded.result_for.take().and_then(|key| sends.remove(&key));
    let set_result = |entries: &mut Vec<Entry>, result: String| {
        if let Some(Item::Frame(frame)) = answered.map(|index| &mut entries[index].item) {
            frame.result = Some(result);
        }
    };
    for local in &decoded.local {
        entries.push(row(Dir::Local, local));
    }
    for event in decoded.events {
        match event {
            Event::Frame(received) => {
                let full = decoded.raw_received.take().unwrap_or(received);
                entries.push(row(Dir::Rx, &full));
            }
            Event::SendResult {
                status, attempts, ..
            } => set_result(entries, send_word(status_word(status), attempts)),
            _ => {}
        }
    }
    if let Some(tx) = decoded.host_tx {
        let result = send_word(&tx.status, tx.attempts);
        if answered.is_some() {
            set_result(entries, result);
        } else {
            let mut unrecorded = row(Dir::Tx, &tx.frame);
            if let Item::Frame(frame) = &mut unrecorded.item {
                frame.result = Some(result);
            }
            entries.push(unrecorded);
        }
    }
}

/// A verdict line settles the newest row with its id recorded before it (ids repeat
/// across state lineages).
fn settle_trace_verdict(entries: &mut [Entry], payload: &Value) {
    let id = payload.get("id").and_then(Value::as_u64);
    let verdict = payload
        .get("verdict")
        .and_then(|value| serde_json::from_value(value.clone()).ok());
    let row = entries
        .iter_mut()
        .rev()
        .find_map(|entry| match &mut entry.item {
            Item::Decision(row) if Some(row.id) == id => Some(row),
            _ => None,
        });
    if let (Some(row), Some(verdict)) = (row, verdict) {
        row.verdict = verdict;
    }
}

/// A trace row is the ring row only when the time matches too; the ring's verdict is the
/// newest. The viewed decision is known from the ring, so it is a row exactly once even
/// when the trace lacks its creation line (capture began later, or the line was dropped).
fn link_ring_rows(entries: &mut Vec<Entry>, snapshot: &Snapshot, decision: &DecisionView) {
    let mut own = false;
    for entry in entries.iter_mut() {
        let Item::Decision(row) = &mut entry.item else {
            continue;
        };
        row.own = row.id == decision.id && row.timestamp == decision.timestamp;
        own |= row.own;
        if let Some(ring) = snapshot
            .decisions
            .iter()
            .find(|ring| ring.id == row.id && ring.timestamp == row.timestamp)
        {
            row.verdict = ring.verdict;
            row.linked = true;
        }
    }
    if !own {
        let index = entries
            .iter()
            .position(|entry| entry.at > decision.timestamp)
            .unwrap_or(entries.len());
        entries.insert(
            index,
            Entry {
                at: decision.timestamp,
                item: Item::Decision(ring_decision(decision, decision)),
            },
        );
    }
}

/// Tags every frame: the trigger (equal to the decision's trigger frame, closest within
/// ±2 s), ours (sent or answered locally), replies (to the bridge within 1 s after one of
/// ours) and the rest.
pub(crate) fn tag(entries: &mut [Entry], decision: &DecisionView, bridge: u8) {
    let mut last_ours: Option<DateTime<Utc>> = None;
    for entry in entries.iter_mut() {
        let Item::Frame(frame) = &mut entry.item else {
            continue;
        };
        frame.tag = match frame.dir {
            Dir::Tx | Dir::Local => {
                last_ours = Some(entry.at);
                Tag::Ours
            }
            Dir::Rx
                if frame.to == bridge
                    && last_ours.is_some_and(|ours| entry.at - ours <= REPLY_WITHIN) =>
            {
                Tag::Reply
            }
            Dir::Rx => Tag::Other,
        };
    }
    let Trigger::Frame { from, to, data } = &decision.trigger else {
        return;
    };
    let Ok(bytes) = hex::decode(data) else {
        return;
    };
    let distance = |at: DateTime<Utc>| (at - decision.timestamp).abs();
    let trigger = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| distance(entry.at) <= TRIGGER_SLACK)
        .filter(|(_, entry)| {
            matches!(&entry.item, Item::Frame(frame)
                if frame.dir == Dir::Rx && frame.from == *from && frame.to == *to && frame.bytes == bytes)
        })
        .min_by_key(|(_, entry)| distance(entry.at))
        .map(|(index, _)| index);
    if let Some(Item::Frame(frame)) = trigger.map(|index| &mut entries[index].item) {
        frame.tag = Tag::Trigger;
    }
}

/// The Home Assistant entity an expectation subject is read from, when it has one.
fn subject_entity<'a>(snapshot: &'a Snapshot, subject: &str) -> Option<&'a str> {
    match subject {
        "tv" => Some(&snapshot.ha.tv.entity),
        "receiver" => Some(&snapshot.ha.receiver.entity),
        "speakers" => Some(&snapshot.ha.sonos.entity),
        _ => {
            let role = subject.strip_prefix("role:")?;
            snapshot
                .roles
                .iter()
                .find(|view| view.role == role)?
                .media_player
                .as_deref()
        }
    }
}

/// The value a Home Assistant state gives `field`, read as the engine reads it
/// (`engine::expectations` `ha_sample`): power `on|playing|paused|idle|buffering` → `on`,
/// `off|standby` → `off`; playback `idle|paused|off|stopped|standby` → `stopped`; source
/// is the `source` attribute. `unknown`/`unavailable` are no evidence.
pub(crate) fn ha_value<'a>(
    field: Field,
    state: &'a str,
    source: Option<&'a str>,
) -> Option<&'a str> {
    if matches!(state, "unknown" | "unavailable") {
        return None;
    }
    Some(match field {
        Field::Source => source?,
        Field::Power => match state {
            "on" | "playing" | "paused" | "idle" | "buffering" => "on",
            "off" | "standby" => "off",
            other => other,
        },
        Field::Playback if matches!(state, "idle" | "paused" | "off" | "stopped" | "standby") => {
            "stopped"
        }
        Field::Playback => state,
    })
}

/// The Home Assistant timestamp the engine samples `field` at: `last_changed` for power
/// and playback, `last_updated` for source.
pub(crate) fn ha_sample_at(
    field: Field,
    changed: Option<DateTime<Utc>>,
    updated: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match field {
        Field::Source => updated,
        Field::Power | Field::Playback => changed,
    }
}

/// The value and sample time an observation offers an expectation on `field`, or `None`
/// when it is no evidence for it.
fn offered(observation: &Observation, field: Field) -> Option<(&str, Option<DateTime<Utc>>)> {
    if observation.via == Some(Via::Ha) {
        let value = ha_value(field, &observation.value, observation.source.as_deref())?;
        let at = ha_sample_at(field, observation.changed, observation.sample_at)?;
        return Some((value, Some(at)));
    }
    Some((&observation.value, observation.sample_at))
}

/// Whether an observation comes from the subject's device.
fn from_subject(observation: &Observation, via: Via, subject: &str, entity: Option<&str>) -> bool {
    match via {
        Via::TvDirect => subject == "tv",
        Via::Denon => subject == "receiver",
        Via::Ps5 => subject.strip_prefix("role:") == observation.role.as_deref(),
        Via::Ha => entity.is_none_or(|entity| observation.entity.as_deref() == Some(entity)),
        Via::Cec => false,
    }
}

/// Notes the observation that settled each met expectation: same source, the observed
/// value as the engine normalises it for the field, the subject's device, at
/// `timestamp + latency_ms` ±1.5 s.
pub(crate) fn settle(entries: &mut [Entry], decision: &DecisionView, snapshot: &Snapshot) {
    for expectation in &decision.expectations {
        let Outcome::Met {
            via,
            observed,
            latency_ms,
        } = &expectation.outcome
        else {
            continue;
        };
        if *latency_ms == 0 {
            continue;
        }
        let target = decision.timestamp
            + TimeDelta::milliseconds(i64::try_from(*latency_ms).unwrap_or(i64::MAX));
        let subject = expectation.subject.as_str();
        let entity = subject_entity(snapshot, subject);
        let distance = |entry: &Entry, sample: Option<DateTime<Utc>>| {
            let at = (entry.at - target).abs();
            sample.map_or(at, |sample| at.min((sample - target).abs()))
        };
        let settled = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let Item::Obs(observation) = &entry.item else {
                    return None;
                };
                if observation.via != Some(*via)
                    || observation.note.is_some()
                    || !from_subject(observation, *via, subject, entity)
                {
                    return None;
                }
                let (value, sample) = offered(observation, expectation.field)?;
                (value == observed.as_str()).then(|| (index, distance(entry, sample)))
            })
            .filter(|(_, distance)| *distance <= SETTLE_SLACK)
            .min_by_key(|(_, distance)| *distance)
            .map(|(index, _)| index);
        if let Some(Item::Obs(observation)) = settled.map(|index| &mut entries[index].item) {
            observation.note = Some(format!(
                "{}.{} met ({})",
                expectation.subject,
                expectation.field,
                latency(latency_ms)
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;
    use crate::{
        action::{Expectation, Field},
        state::{ConsoleState, DirectPower},
        web::view::tests::{row, snapshot},
    };

    fn power(state: DirectPower) -> String {
        state.to_string()
    }

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 7, 17, 8, 40).unwrap() + TimeDelta::milliseconds(860)
    }

    fn ms(offset: i64) -> DateTime<Utc> {
        t0() + TimeDelta::milliseconds(offset)
    }

    fn frame_entry(at: i64, dir: Dir, from: u8, to: u8, bytes: &[u8]) -> Entry {
        Entry {
            at: ms(at),
            item: Item::Frame(WindowFrame {
                dir,
                from,
                to,
                from_name: String::new(),
                to_name: String::new(),
                bytes: bytes.to_vec(),
                result: None,
                tag: Tag::Other,
            }),
        }
    }

    fn tags(entries: &[Entry]) -> Vec<&'static str> {
        entries
            .iter()
            .filter_map(|entry| match &entry.item {
                Item::Frame(frame) => Some(frame.tag.word()),
                _ => None,
            })
            .collect()
    }

    /// Console A's wake: it reports its physical address to everyone at t0.
    fn wake() -> DecisionView {
        DecisionView {
            timestamp: t0(),
            trigger: Trigger::Frame {
                from: 11,
                to: 15,
                data: "84230004".to_owned(),
            },
            reason: "ReportPhysicalAddress".to_owned(),
            ..row(844, Some("console"), vec![])
        }
    }

    #[test]
    fn tags_the_trigger_ours_replies_and_the_rest() {
        let mut entries = vec![
            // The same report three seconds early is outside the ±2 s slack.
            frame_entry(-3_000, Dir::Rx, 11, 15, &[0x84, 0x23, 0x00, 0x04]),
            frame_entry(-1_100, Dir::Rx, 4, 4, &[]),
            frame_entry(5, Dir::Rx, 11, 15, &[0x84, 0x23, 0x00, 0x04]),
            frame_entry(48, Dir::Rx, 5, 11, &[0x46]),
            frame_entry(2_000, Dir::Local, 0, 15, &[0x87, 0x01, 0x23, 0x45]),
            frame_entry(3_000, Dir::Tx, 0, 11, &[0x8F]),
            // Answered within a second, to the bridge: a reply.
            frame_entry(3_400, Dir::Rx, 11, 0, &[0x90, 0x00]),
            // Too late to be a reply.
            frame_entry(4_500, Dir::Rx, 11, 0, &[0x90, 0x00]),
            // In time, but to someone else.
            frame_entry(3_500, Dir::Rx, 11, 15, &[0x87, 0x00, 0x09, 0xBF]),
        ];
        entries.sort_by_key(|entry| entry.at);
        tag(&mut entries, &wake(), 0);
        assert_eq!(
            tags(&entries),
            [
                "other", "other", "trigger", "other", "ours", "ours", "reply", "other", "other"
            ]
        );

        // A non-frame trigger tags no frame as the trigger.
        let timer = DecisionView {
            trigger: Trigger::Timer {
                name: crate::action::TimerKind::RoomOff,
            },
            ..wake()
        };
        tag(&mut entries, &timer, 0);
        assert!(!tags(&entries).contains(&"trigger"));
    }

    fn obs(at: i64, observation: Observation) -> Entry {
        Entry {
            at: ms(at),
            item: Item::Obs(observation),
        }
    }

    fn notes(entries: &[Entry]) -> Vec<Option<&str>> {
        entries
            .iter()
            .filter_map(|entry| match &entry.item {
                Item::Obs(observation) => Some(observation.note.as_deref()),
                _ => None,
            })
            .collect()
    }

    fn met(subject: &str, via: Via, observed: &str, latency_ms: u64) -> Expectation {
        Expectation {
            subject: subject.to_owned(),
            field: Field::Power,
            expect: observed.to_owned(),
            via: vec![via],
            deadline_ms: 45_000,
            attempts: 0,
            outcome: Outcome::Met {
                via,
                observed: observed.to_owned(),
                latency_ms,
            },
        }
    }

    #[test]
    fn notes_the_observation_that_settled_each_expectation() {
        let snapshot = snapshot();
        let tv = |at, state: DirectPower| {
            obs(
                at,
                Observation::new("tv/state", Some(Via::TvDirect), power(state)),
            )
        };
        let mut source = met("receiver", Via::Ha, "Console", 2_100);
        source.field = Field::Source;
        let decision = DecisionView {
            expectations: vec![
                met("tv", Via::TvDirect, "on", 15_600),
                met("receiver", Via::Denon, "on", 1_060),
                source,
                met("role:streamer", Via::Ps5, "standby", 9_000),
                // Already satisfied: nothing in the window settled it.
                met("speakers", Via::Ha, "off", 0),
            ],
            ..wake()
        };
        let mut entries = vec![
            tv(10, DirectPower::Unreachable),
            obs(
                1_070,
                Observation {
                    previous: Some("off".to_owned()),
                    ..Observation::new("receiver/state", Some(Via::Denon), "on")
                },
            ),
            // Home Assistant's receiver gains the source; the TV entity says nothing useful.
            obs(
                2_000,
                Observation {
                    entity: Some("media_player.test_tv".to_owned()),
                    source: Some("Console".to_owned()),
                    sample_at: Some(ms(2_000)),
                    ..Observation::new("ha/state", Some(Via::Ha), "on")
                },
            ),
            obs(
                2_050,
                Observation {
                    entity: Some("media_player.test_receiver".to_owned()),
                    source: Some("Console".to_owned()),
                    sample_at: Some(ms(2_050)),
                    ..Observation::new("ha/state", Some(Via::Ha), "on")
                },
            ),
            // The probe of another role, then the right one; the sample time is what counts.
            obs(
                9_000,
                Observation {
                    role: Some("console".to_owned()),
                    ..Observation::new("probe/state", Some(Via::Ps5), "standby")
                },
            ),
            obs(
                11_000,
                Observation {
                    role: Some("streamer".to_owned()),
                    sample_at: Some(ms(9_100)),
                    ..Observation::new("probe/state", Some(Via::Ps5), "standby")
                },
            ),
            // An earlier "on" more than 1.5 s before the measured time is not it.
            tv(13_000, DirectPower::On),
            tv(15_500, DirectPower::On),
            obs(20_000, Observation::new("ha/state", Some(Via::Ha), "off")),
        ];
        settle(&mut entries, &decision, &snapshot);
        assert_eq!(
            notes(&entries),
            [
                None,
                Some("receiver.power met (1.0 s)"),
                None,
                Some("receiver.source met (2.1 s)"),
                None,
                Some("role:streamer.power met (9.0 s)"),
                None,
                Some("tv.power met (15.6 s)"),
                None,
            ]
        );
    }

    #[test]
    fn ha_values_follow_the_engine_normalisation() {
        // Pinned by example to `engine::expectations` `ha_sample`.
        for (field, state, expected) in [
            (Field::Power, "on", Some("on")),
            (Field::Power, "playing", Some("on")),
            (Field::Power, "paused", Some("on")),
            (Field::Power, "idle", Some("on")),
            (Field::Power, "buffering", Some("on")),
            (Field::Power, "off", Some("off")),
            (Field::Power, "standby", Some("off")),
            (Field::Power, "heat", Some("heat")),
            (Field::Playback, "idle", Some("stopped")),
            (Field::Playback, "paused", Some("stopped")),
            (Field::Playback, "off", Some("stopped")),
            (Field::Playback, "stopped", Some("stopped")),
            (Field::Playback, "standby", Some("stopped")),
            (Field::Playback, "playing", Some("playing")),
            (Field::Source, "on", Some("Console")),
            (Field::Power, "unknown", None),
            (Field::Playback, "unavailable", None),
            (Field::Source, "unavailable", None),
        ] {
            assert_eq!(
                ha_value(field, state, Some("Console")),
                expected,
                "{field} {state}"
            );
        }
        assert_eq!(ha_value(Field::Source, "on", None), None);
        let (changed, updated) = (Some(ms(1)), Some(ms(2)));
        assert_eq!(ha_sample_at(Field::Power, changed, updated), changed);
        assert_eq!(ha_sample_at(Field::Playback, changed, updated), changed);
        assert_eq!(ha_sample_at(Field::Source, changed, updated), updated);
    }

    #[test]
    fn ha_observations_settle_by_normalised_value_and_field_timestamp() {
        let snapshot = snapshot();
        let speakers = snapshot.ha.sonos.entity.clone();
        let tv = snapshot.ha.tv.entity.clone();
        let mut stopped = met("speakers", Via::Ha, "stopped", 500);
        stopped.field = Field::Playback;
        let decision = DecisionView {
            expectations: vec![stopped, met("tv", Via::Ha, "on", 3_000)],
            ..wake()
        };
        let ha = |entity: &str, state: &str, changed: i64, updated: i64| Observation {
            entity: Some(entity.to_owned()),
            changed: Some(ms(changed)),
            sample_at: Some(ms(updated)),
            ..Observation::new("ha/state", Some(Via::Ha), state)
        };
        let mut entries = vec![
            // `idle` settles playback `stopped`; arrival is late, `last_changed` is on time.
            obs(2_400, ha(&speakers, "idle", 500, 2_400)),
            // A `paused` TV is power `on`; its attribute refresh (`updated`) is not the
            // sample time of power, `changed` is.
            obs(9_000, ha(&tv, "paused", 3_000, 9_000)),
            // Also `on`, arriving 100 ms from the measured time, but its `changed` is old:
            // the exact `changed` sample above is the one that settled it.
            obs(3_100, ha(&tv, "playing", -20_000, 3_100)),
        ];
        entries.sort_by_key(|entry| entry.at);
        settle(&mut entries, &decision, &snapshot);
        let noted: Vec<_> = entries
            .iter()
            .filter_map(|entry| match &entry.item {
                Item::Obs(observation) => {
                    Some((observation.value.as_str(), observation.note.as_deref()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            noted,
            [
                ("idle", Some("speakers.playback met (500 ms)")),
                ("playing", None),
                ("paused", Some("tv.power met (3.0 s)")),
            ]
        );
    }

    #[test]
    fn trace_records_decode_through_the_shared_correlation() {
        let snapshot = snapshot();
        let bridge = "cec-bridge".to_owned();
        let aether = |seq: u64, payload: Value| {
            let mut payload = payload;
            payload["seq"] = json!(seq);
            TraceEntry::AetherBus {
                node: bridge.clone(),
                payload,
            }
        };
        let decision_payload = json!({
            "id": 844, "timestamp": t0(), "parent": null, "role": "console",
            "trigger": {"kind":"frame","from":11,"to":15,"data":"84230004"},
            "from": "off", "to": "on", "reason": "ReportPhysicalAddress", "actions": [],
            "expectations": [], "verdict": "pending", "would": false
        });
        let records = vec![
            // A row with the same id from an earlier state lineage: same id, other time.
            (
                ms(-500),
                TraceEntry::Decision(
                    json!({"id": 844, "timestamp": ms(-500), "reason": "Standby", "verdict": "none"}),
                ),
            ),
            (ms(0), TraceEntry::Decision(decision_payload.clone())),
            (
                ms(5),
                aether(
                    1,
                    json!({"ev":"rx","from":11,"to":15,"data":"84230004","ack":true}),
                ),
            ),
            (
                ms(200),
                TraceEntry::Sent {
                    bridge: bridge.clone(),
                    to: 11,
                    data: vec![0x8F],
                    id: Some("s-7".to_owned()),
                },
            ),
            (
                ms(300),
                aether(
                    2,
                    json!({"ev":"tx","origin":"host","id":"s-7","from":0,"to":11,"data":"8F","status":"ack","attempts":2}),
                ),
            ),
            (
                ms(400),
                aether(
                    3,
                    json!({"ev":"tx","origin":"local","from":0,"to":15,"data":"87012345","status":"ack"}),
                ),
            ),
            // A directed NACK keeps its full bytes in the window.
            (
                ms(500),
                aether(
                    4,
                    json!({"ev":"rx","from":11,"to":5,"data":"47436F6E","ack":false}),
                ),
            ),
            (
                ms(600),
                TraceEntry::ReceiverPower {
                    state: DirectPower::Off,
                    at: ms(600),
                    source_code: None,
                },
            ),
            (
                ms(700),
                TraceEntry::ReceiverPower {
                    state: DirectPower::On,
                    at: ms(700),
                    source_code: None,
                },
            ),
            (
                ms(800),
                TraceEntry::ConsolePower {
                    role: "streamer".to_owned(),
                    state: ConsoleState::Standby,
                    at: ms(800),
                },
            ),
            (
                ms(900),
                TraceEntry::Verdict(json!({"id": 844, "verdict": "achieved", "expectations": []})),
            ),
            // Tasmota: the RESULT acknowledges the preceding HdmiSend.
            (
                ms(1_000),
                TraceEntry::Sent {
                    bridge: "tasmota".to_owned(),
                    to: 4,
                    data: vec![0x36],
                    id: None,
                },
            ),
            (
                ms(1_100),
                TraceEntry::Result {
                    bridge: "tasmota".to_owned(),
                    payload: json!({"HdmiSend":{"Ack":false}}),
                },
            ),
        ];
        let decision = DecisionView {
            timestamp: t0(),
            ..wake()
        };
        let entries = from_trace(records, &snapshot, &decision);
        let shape = shape(&entries);
        assert_eq!(
            shape,
            [
                "#844 Standby none own=false linked=false",
                "#844 ReportPhysicalAddress achieved own=true linked=false",
                "rx 11→15 84 23 00 04 ack",
                "tx 0→11 8F ack · 2 attempts",
                "local 0→15 87 01 23 45 ack",
                "rx 11→5 47 43 6F 6E nack",
                "receiver/state off · denon",
                "receiver/state off → on · denon",
                "probe/state standby · ps5",
                "tx 0→4 36 nack",
            ]
        );
        let Item::Frame(nacked) = &entries[5].item else {
            panic!("frame expected");
        };
        assert_eq!(nacked.meaning(), "Set OSD Name \"Con\"");
    }

    /// One line per window row.
    fn shape(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match &entry.item {
                Item::Frame(frame) => format!(
                    "{} {}→{} {} {}",
                    frame.dir.word(),
                    frame.from,
                    frame.to,
                    frame.data(),
                    frame.result.as_deref().unwrap_or("-")
                ),
                Item::Obs(observation) => observation.text(),
                Item::Decision(row) => format!(
                    "#{} {} {} own={} linked={}",
                    row.id, row.reason, row.verdict, row.own, row.linked
                ),
            })
            .collect()
    }

    fn sent(id: Option<&str>) -> TraceEntry {
        TraceEntry::Sent {
            bridge: "cec-bridge".to_owned(),
            to: 4,
            data: vec![0x8F],
            id: id.map(str::to_owned),
        }
    }

    fn frames(entries: &[Entry]) -> Vec<String> {
        shape(entries)
            .into_iter()
            .filter(|line| !line.starts_with('#'))
            .collect()
    }

    #[test]
    fn results_attach_to_the_send_the_adapter_matched() {
        let snapshot = snapshot();
        let decision = wake();
        let result = |ack: Value| TraceEntry::Result {
            bridge: "tasmota".to_owned(),
            payload: json!({ "HdmiSend": ack }),
        };
        // Tasmota is a FIFO: a stock reply consumes the first send without classifying it,
        // then NACK and ACK answer the second and third identical sends in order.
        let tasmota = vec![
            (ms(0), sent(None)),
            (ms(10), sent(None)),
            (ms(20), sent(None)),
            (ms(30), result(json!("Done"))),
            (ms(40), result(json!({"Ack": false}))),
            (ms(50), result(json!({"Ack": true}))),
        ];
        assert_eq!(
            frames(&from_trace(tasmota, &snapshot, &decision)),
            ["tx 0→4 8F -", "tx 0→4 8F nack", "tx 0→4 8F ack"]
        );
        // aether answers by id, here out of order: the second send fails first.
        let aether = |seq: u64, id: &str, status: &str| TraceEntry::AetherBus {
            node: "cec-bridge".to_owned(),
            payload: json!({"ev":"tx","seq":seq,"origin":"host","id":id,"from":0,"to":4,"data":"8F","status":status}),
        };
        let records = vec![
            (
                ms(-10),
                TraceEntry::AetherStatus {
                    node: "cec-bridge".to_owned(),
                    payload: json!({"online":true,"v":1,"boot":1}),
                },
            ),
            (ms(0), sent(Some("s-1"))),
            (ms(10), sent(Some("s-2"))),
            (ms(20), aether(1, "s-2", "nack")),
            (ms(30), aether(2, "s-1", "ack")),
        ];
        assert_eq!(
            frames(&from_trace(records, &snapshot, &decision)),
            ["tx 0→4 8F ack", "tx 0→4 8F nack"]
        );
    }

    #[test]
    fn host_transmissions_without_a_recorded_send_are_ours() {
        let snapshot = snapshot();
        let decision = wake();
        let records = vec![
            (
                ms(-10),
                TraceEntry::AetherStatus {
                    node: "cec-bridge".to_owned(),
                    payload: json!({"online":true,"v":1,"boot":1}),
                },
            ),
            (ms(0), sent(Some("s-1"))),
            (
                ms(20),
                TraceEntry::AetherBus {
                    node: "cec-bridge".to_owned(),
                    payload: json!({"ev":"tx","seq":1,"origin":"host","id":"s-1","from":0,"to":4,"data":"8F","status":"ack"}),
                },
            ),
            // A manual MQTT send: its id was never recorded by Choregos.
            (
                ms(40),
                TraceEntry::AetherBus {
                    node: "cec-bridge".to_owned(),
                    payload: json!({"ev":"tx","seq":2,"origin":"host","id":"manual-1","from":0,"to":5,"data":"8F","status":"nack","attempts":3}),
                },
            ),
        ];
        let mut entries = from_trace(records, &snapshot, &decision);
        tag(&mut entries, &decision, snapshot.bridge_logical_address);
        assert_eq!(
            frames(&entries),
            ["tx 0→4 8F ack", "tx 0→5 8F nack · 3 attempts"]
        );
        assert_eq!(tags(&entries), ["ours", "ours"]);
    }

    #[test]
    fn the_viewed_decision_is_a_row_once_even_without_its_trace_line() {
        let snapshot = snapshot();
        let decision = wake();
        let records = vec![(ms(-100), sent(None)), (ms(5), sent(None))];
        let rows = shape(&from_trace(records, &snapshot, &decision));
        assert_eq!(
            rows,
            [
                "tx 0→4 8F -",
                "#844 ReportPhysicalAddress achieved own=true linked=true",
                "tx 0→4 8F -",
            ]
        );
        assert_eq!(
            shape(&from_trace(vec![], &snapshot, &decision)),
            ["#844 ReportPhysicalAddress achieved own=true linked=true"]
        );
        // With its creation line, it is not added again.
        let line = json!({"id": 844, "timestamp": t0(), "reason": "ReportPhysicalAddress"});
        let rows = shape(&from_trace(
            vec![(ms(0), TraceEntry::Decision(line))],
            &snapshot,
            &decision,
        ));
        assert_eq!(rows.len(), 1, "{rows:?}");
    }
}
