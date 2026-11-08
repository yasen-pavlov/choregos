//! The Recorder timeline: one window of the bus (`window::from_trace` / `from_memory`) laid
//! out as lanes, every position a percentage of the window so the page is complete without
//! JavaScript. The room band, decision markers, one lane per logical address and the
//! device-observation lanes are all derived here; templates only print them.

use std::collections::BTreeMap;

use chrono::{DateTime, TimeDelta, Timelike, Utc};
use serde::Serialize;

use super::{
    clock, hms,
    recorder::{headline, la_name},
    verdict_tone,
    window::{self, Dir, Entry, Item, Tag},
};
use crate::state::{DecisionView, RolePower, Snapshot};

/// The widest window the timeline (and its JSON) serves.
pub(crate) const MAX_SPAN: TimeDelta = TimeDelta::hours(1);
/// Decision markers carry their id as a label up to this many; beyond it the labels would
/// overlap, and the id stays in the marker's title.
const LABELLED_MARKERS: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Zoom {
    M1,
    M5,
    M15,
    H1,
}

impl Zoom {
    pub(crate) const ALL: [Self; 4] = [Self::M1, Self::M5, Self::M15, Self::H1];

    pub(crate) fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|zoom| zoom.word() == word)
    }

    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::H1 => "1h",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::M1 => "1 m",
            Self::M5 => "5 m",
            Self::M15 => "15 m",
            Self::H1 => "1 h",
        }
    }

    pub(crate) fn span(self) -> TimeDelta {
        TimeDelta::seconds(match self {
            Self::M1 => 60,
            Self::M5 => 300,
            Self::M15 => 900,
            Self::H1 => 3600,
        })
    }

    /// The pan step: a tenth of the window.
    pub(crate) fn step(self) -> TimeDelta {
        self.span() / 10
    }

    /// `6 s`, `30 s`, `90 s`, `6 m`.
    pub(crate) fn step_label(self) -> String {
        let seconds = self.step().num_seconds();
        if seconds < 120 {
            format!("{seconds} s")
        } else {
            format!("{} m", seconds / 60)
        }
    }
}

/// The instants a timeline shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) from: DateTime<Utc>,
    pub(crate) to: DateTime<Utc>,
}

impl Span {
    /// The window of `zoom` centred on `at`.
    pub(crate) fn around(at: DateTime<Utc>, zoom: Zoom) -> Self {
        let half = zoom.span() / 2;
        Self {
            from: at - half,
            to: at + half,
        }
    }

    /// Where `at` falls, in percent of the window, clamped to [0, 100].
    pub(crate) fn x(&self, at: DateTime<Utc>) -> f64 {
        let total = (self.to - self.from).num_milliseconds();
        if total <= 0 {
            return 0.0;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a window is at most an hour of ms"
        )]
        let x = (at - self.from).num_milliseconds() as f64 * 100.0 / total as f64;
        x.clamp(0.0, 100.0)
    }

    /// `Sat 7 Nov · 17:08:30 → 17:09:30`.
    pub(crate) fn line(&self) -> String {
        format!(
            "{} · {} → {}",
            self.from.format("%a %-d %b"),
            hms(&self.from),
            hms(&self.to)
        )
    }
}

/// A percentage for an SVG coordinate: `18.10`.
pub(crate) fn pct(x: &f64) -> String {
    format!("{x:.2}")
}

#[derive(Debug, Serialize)]
pub(crate) struct AxisLabel {
    pub(crate) at: DateTime<Utc>,
    pub(crate) x: f64,
    pub(crate) label: String,
}

/// A stretch of one value: a room state or an observed device value.
#[derive(Debug, Serialize)]
pub(crate) struct Band {
    pub(crate) start: DateTime<Utc>,
    pub(crate) end: DateTime<Utc>,
    pub(crate) x: f64,
    pub(crate) width: f64,
    pub(crate) label: String,
    /// `on`, `off`, `unknown` (room); `on`, `off`, `na` (observations).
    pub(crate) class: &'static str,
    /// The last band: it still holds at its end, so live tail stretches it.
    pub(crate) open: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct Marker {
    pub(crate) id: u64,
    pub(crate) at: DateTime<Utc>,
    pub(crate) x: f64,
    /// Verdict tone, or `would`.
    pub(crate) class: &'static str,
    pub(crate) headline: String,
    pub(crate) title: String,
    /// In the ring, so its detail page exists.
    pub(crate) linked: bool,
    pub(crate) selected: bool,
    pub(crate) show_id: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct Tick {
    /// Position of the frame in the window's entries.
    pub(crate) seq: usize,
    pub(crate) at: DateTime<Utc>,
    pub(crate) x: f64,
    /// `trigger`, `nack`, `ours` or `other`.
    pub(crate) kind: &'static str,
    /// A header-only presence poll (drawn short).
    pub(crate) poll: bool,
    /// `time · from→to · data · meaning`, with the result and the decision it triggered.
    pub(crate) title: String,
    /// `84 trigger` under a trigger tick.
    pub(crate) note: Option<String>,
    /// `17:08:40.865 11→15 84 Report Physical Address · trigger of #844`: the frame in the
    /// lane's text alternative.
    pub(crate) line: String,
}

impl Tick {
    /// Top, height and width in the 32 px lane: triggers tall, polls short.
    pub(crate) fn y(&self) -> u8 {
        match (self.kind, self.poll) {
            ("trigger", _) => 3,
            (_, true) => 12,
            _ => 8,
        }
    }

    pub(crate) fn height(&self) -> u8 {
        match (self.kind, self.poll) {
            ("trigger", _) => 22,
            (_, true) => 8,
            _ => 16,
        }
    }

    pub(crate) fn width(&self) -> u8 {
        if matches!(self.kind, "trigger" | "ours") {
            3
        } else {
            2
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct AddressLane {
    pub(crate) la: u8,
    pub(crate) name: String,
    pub(crate) ticks: Vec<Tick>,
    /// `silent`, `silent (probe: standby)` when the lane has no frames.
    pub(crate) empty: Option<String>,
    /// The selected decision's trigger came from this address.
    pub(crate) focus: bool,
    /// `LA 11 Console · 4 frames · trigger at :40.8`: the track's accessible name.
    pub(crate) summary: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ObservationLane {
    /// `tv`, `receiver`, `probe:<role>`.
    pub(crate) key: String,
    /// `tv · webos`, `receiver · denon`, `probe · <role>`.
    pub(crate) label: String,
    pub(crate) runs: Vec<Band>,
    pub(crate) empty: Option<String>,
    /// `tv · webos: unreachable → on at :56.4`: the track's accessible name.
    pub(crate) summary: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Lanes {
    pub(crate) axis: Vec<AxisLabel>,
    pub(crate) room: Vec<Band>,
    pub(crate) decisions: Vec<Marker>,
    pub(crate) addresses: Vec<AddressLane>,
    pub(crate) observations: Vec<ObservationLane>,
}

/// `/p/timeline/window.json`: the window the partial renders.
#[derive(Serialize)]
pub(crate) struct WindowJson {
    pub(crate) from: DateTime<Utc>,
    pub(crate) to: DateTime<Utc>,
    /// `memory`, `trace files`.
    pub(crate) source: String,
    pub(crate) truncated: bool,
    pub(crate) entries: Vec<Entry>,
    pub(crate) lanes: Lanes,
}

/// Tags the window's frames (ours / reply / other, then the trigger of every ring decision
/// in it) and notes the observations that settled their expectations. Returns the trigger
/// frames as (entry index, decision id).
pub(crate) fn tag_and_settle(entries: &mut [Entry], snapshot: &Snapshot) -> Vec<(usize, u64)> {
    window::tag_frames(entries, snapshot.bridge_logical_address);
    let decisions: Vec<&DecisionView> = entries
        .iter()
        .filter_map(|entry| match &entry.item {
            Item::Decision(row) => snapshot
                .decisions
                .iter()
                .find(|ring| ring.id == row.id && ring.timestamp == row.timestamp),
            _ => None,
        })
        .collect();
    let mut triggers = Vec::new();
    for decision in decisions {
        if let Some(index) = window::mark_trigger(entries, decision) {
            triggers.push((index, decision.id));
        }
        window::settle(entries, decision, snapshot);
    }
    triggers
}

/// Builds every lane of the window. `now` bounds open-ended bands (nothing is drawn in the
/// future); `memory` says the entries came from memory, which holds no observations.
pub(crate) fn lanes(
    snapshot: &Snapshot,
    entries: &[Entry],
    triggers: &[(usize, u64)],
    span: Span,
    now: DateTime<Utc>,
    selected: Option<&DecisionView>,
    memory: bool,
) -> Lanes {
    Lanes {
        axis: axis(span),
        room: room(snapshot, span, now),
        decisions: markers(snapshot, entries, span, selected),
        addresses: addresses(snapshot, entries, triggers, span, selected),
        observations: observations(snapshot, entries, span, now, memory),
    }
}

/// Six labels, one every sixth of the window: the full time first and whenever the minute
/// changes, else `:SS`.
pub(crate) fn axis(span: Span) -> Vec<AxisLabel> {
    let step = (span.to - span.from) / 6;
    let mut previous: Option<(u32, u32)> = None;
    (0..6)
        .map(|k| {
            let at = span.from + step * k;
            let minute = (at.hour(), at.minute());
            let label = if previous == Some(minute) {
                format!(":{:02}", at.second())
            } else {
                hms(&at)
            };
            previous = Some(minute);
            AxisLabel {
                at,
                x: span.x(at),
                label,
            }
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Room {
    Unknown,
    Off,
    On {
        role: Option<String>,
        /// The receiver input a recorded `select_source` call asked for, and when.
        input: Option<(String, DateTime<Utc>)>,
    },
}

/// A row the engine records for what a `would` start policy would have done
/// (`record_would`): it never changes the intent. Late-announcement reversals are always
/// skipped (a real one stops a role that is not the active one, so the room is unchanged);
/// the console-start reasons only in `record_would`'s Off → Off shape, because a real
/// transition always changes power and shadow mode marks real transitions `would` too.
fn counterfactual(row: &DecisionView) -> bool {
    row.would
        && match row.reason.as_str() {
            "late_announcement" => true,
            "console_start" | "console_start_retracted" | "console_start_expired" => {
                row.from == row.to
            }
            _ => false,
        }
}

/// The source of the row's recorded `media_player.select_source` call (sent or `would`).
fn selected_source(row: &DecisionView) -> Option<String> {
    row.actions.iter().find_map(|action| {
        let json = action.strip_prefix("would: ").unwrap_or(action);
        let value: serde_json::Value = serde_json::from_str(json).ok()?;
        (value.get("kind")?.as_str()? == "ha_call"
            && value.get("domain")?.as_str()? == "media_player"
            && value.get("service")?.as_str()? == "select_source")
            .then(|| {
                value
                    .get("data")?
                    .get("source")?
                    .as_str()
                    .map(str::to_owned)
            })
            .flatten()
    })
}

/// The room's intent over the window, replayed from the ring the way the engine sets it: a
/// role turning on turns the room on with it, the active role turning off leaves the room
/// on with no role (the off timer runs), the off timer turns it off, a restart forgets it.
/// Counterfactual rows are skipped. The receiver input is named only from a recorded
/// `select_source` call; naming the first input of the role already on does not start a
/// new band.
fn room(snapshot: &Snapshot, span: Span, now: DateTime<Utc>) -> Vec<Band> {
    let mut rows: Vec<&DecisionView> = snapshot
        .decisions
        .iter()
        .filter(|row| row.timestamp <= span.to && !counterfactual(row))
        .collect();
    rows.sort_by_key(|row| row.timestamp);
    let mut changes: Vec<(DateTime<Utc>, Room)> = vec![(DateTime::<Utc>::MIN_UTC, Room::Unknown)];
    for row in rows {
        let current = &changes.last().expect("starts with Unknown").1;
        let mut next = match (row.reason.as_str(), row.role.as_deref(), row.to) {
            ("service_started", ..) => Room::Unknown,
            ("off_timer_expired", ..) => Room::Off,
            (_, Some(role), Some(RolePower::On)) => Room::On {
                role: Some(role.to_owned()),
                input: None,
            },
            (_, Some(role), Some(RolePower::Off)) if matches!(current, Room::On { role: Some(active), .. } if active == role) => {
                Room::On {
                    role: None,
                    input: None,
                }
            }
            _ => current.clone(),
        };
        if let Room::On {
            role: Some(_),
            input,
        } = &mut next
            && let Some(source) = selected_source(row)
            && input.as_ref().is_none_or(|(held, _)| *held != source)
        {
            *input = Some((source, row.timestamp));
        }
        if *current == next {
            continue;
        }
        let names_first_input = matches!(
            (current, &next),
            (Room::On { role: held, input: None }, Room::On { role, .. }) if held == role
        );
        if names_first_input {
            changes.last_mut().expect("starts with Unknown").1 = next;
        } else {
            changes.push((row.timestamp, next));
        }
    }
    let end = span.to.min(now);
    let mut bands = Vec::new();
    for (index, (start, state)) in changes.iter().enumerate() {
        let until = changes.get(index + 1).map_or(end, |(next, _)| *next);
        let (from, to) = ((*start).max(span.from), until.min(end));
        if to <= from {
            continue;
        }
        let (label, class) = match state {
            Room::Unknown => ("unknown".to_owned(), "unknown"),
            Room::Off => ("off".to_owned(), "off"),
            Room::On { role: None, .. } => ("on · no active role".to_owned(), "on"),
            Room::On {
                role: Some(role),
                input,
            } => {
                let mut label = format!("on · {role}");
                if let Some((source, at)) = input {
                    label.push_str(&format!(" · input {source} from {}", short_time(*at, span)));
                }
                (label, "on")
            }
        };
        bands.push(Band {
            start: from,
            end: to,
            x: span.x(from),
            width: span.x(to) - span.x(from),
            label,
            class,
            open: index + 1 == changes.len(),
        });
    }
    bands
}

/// `:42.9` inside the window, else `17:01:12`.
fn short_time(at: DateTime<Utc>, span: Span) -> String {
    if span.from <= at && at <= span.to {
        format!(":{:02}.{}", at.second(), at.timestamp_subsec_millis() / 100)
    } else {
        hms(&at)
    }
}

fn markers(
    snapshot: &Snapshot,
    entries: &[Entry],
    span: Span,
    selected: Option<&DecisionView>,
) -> Vec<Marker> {
    let mut markers: Vec<Marker> = entries
        .iter()
        .filter_map(|entry| {
            let Item::Decision(row) = &entry.item else {
                return None;
            };
            let ring = snapshot
                .decisions
                .iter()
                .find(|ring| ring.id == row.id && ring.timestamp == row.timestamp);
            let headline = ring.map_or_else(
                || {
                    format!(
                        "#{} {} · {}",
                        row.id,
                        row.role.as_deref().unwrap_or("room"),
                        row.reason
                    )
                },
                |ring| headline(ring, snapshot),
            );
            let verdict = if row.would {
                "would".to_owned()
            } else {
                row.verdict.to_string()
            };
            Some(Marker {
                id: row.id,
                at: entry.at,
                x: span.x(entry.at),
                class: if row.would {
                    "would"
                } else {
                    verdict_tone(&row.verdict)
                },
                title: format!(
                    "#{} · {} · {headline} · {verdict}",
                    row.id,
                    clock(&entry.at)
                ),
                headline,
                linked: row.linked,
                selected: selected.is_some_and(|selected| {
                    selected.id == row.id && selected.timestamp == row.timestamp
                }),
                show_id: false,
            })
        })
        .collect();
    let labelled = markers.len() <= LABELLED_MARKERS;
    for marker in &mut markers {
        marker.show_id = labelled;
    }
    markers
}

/// The lane a frame is drawn in: its sender, except a presence poll from the bridge, which
/// is about (and drawn in the lane of) the address it polls.
fn lane_of(dir: Dir, from: u8, to: u8, poll: bool, bridge: u8) -> u8 {
    if poll && from == bridge && dir != Dir::Rx {
        to
    } else {
        from
    }
}

fn addresses(
    snapshot: &Snapshot,
    entries: &[Entry],
    triggers: &[(usize, u64)],
    span: Span,
    selected: Option<&DecisionView>,
) -> Vec<AddressLane> {
    let bridge = snapshot.bridge_logical_address;
    let mut lanes: BTreeMap<u8, Vec<Tick>> = BTreeMap::new();
    for (seq, entry) in entries.iter().enumerate() {
        let Item::Frame(frame) = &entry.item else {
            continue;
        };
        let la = lane_of(frame.dir, frame.from, frame.to, frame.is_poll(), bridge);
        if la == 15 {
            continue;
        }
        let triggered = triggers
            .iter()
            .find(|(index, _)| *index == seq)
            .map(|(_, id)| *id);
        let nack = frame
            .result
            .as_deref()
            .is_some_and(|result| result.starts_with("nack"));
        let kind = match frame.tag {
            Tag::Trigger => "trigger",
            _ if nack => "nack",
            Tag::Ours => "ours",
            Tag::Reply | Tag::Other => "other",
        };
        let mut title = format!(
            "{} · {}→{} · {} · {}",
            clock(&entry.at),
            frame.from,
            frame.to,
            frame.data(),
            frame.meaning()
        );
        if let Some(result) = &frame.result {
            title.push_str(&format!(" · {result}"));
        }
        if let Some(id) = triggered {
            title.push_str(&format!(" · trigger of #{id}"));
        }
        let note = triggered.map(|_| match frame.bytes.first() {
            Some(opcode) => format!("{opcode:02X} trigger"),
            None => "poll trigger".to_owned(),
        });
        let mut line = format!(
            "{} {}→{} {}",
            clock(&entry.at),
            frame.from,
            frame.to,
            match frame.bytes.first() {
                Some(opcode) => format!("{opcode:02X} {}", frame.meaning()),
                None => "poll".to_owned(),
            }
        );
        if nack {
            line.push_str(" · nack");
        }
        if let Some(id) = triggered {
            line.push_str(&format!(" · trigger of #{id}"));
        }
        lanes.entry(la).or_default().push(Tick {
            seq,
            at: entry.at,
            x: span.x(entry.at),
            kind,
            poll: frame.is_poll(),
            title,
            note,
            line,
        });
    }
    for role in &snapshot.roles {
        if let Some(la) = role.logical_address {
            lanes.entry(la).or_default();
        }
    }
    let focus = selected.and_then(DecisionView::trigger_logical_address);
    lanes
        .into_iter()
        .map(|(la, ticks)| {
            let empty = ticks.is_empty().then(|| {
                snapshot
                    .roles
                    .iter()
                    .find(|role| role.logical_address == Some(la) && role.probe_configured)
                    .and_then(|role| role.direct)
                    .map_or_else(
                        || "silent".to_owned(),
                        |(state, _)| format!("silent (probe: {state})"),
                    )
            });
            let name = if la == bridge {
                "bridge".to_owned()
            } else {
                la_name(snapshot, la)
            };
            AddressLane {
                summary: address_summary(la, &name, &ticks, empty.as_deref(), span),
                la,
                name,
                ticks,
                empty,
                focus: focus == Some(la),
            }
        })
        .collect()
}

/// `LA 11 Console · 4 frames · trigger at :40.8`, `LA 4 Streamer · silent`.
fn address_summary(la: u8, name: &str, ticks: &[Tick], empty: Option<&str>, span: Span) -> String {
    let mut summary = if name == format!("LA {la}") {
        name.to_owned()
    } else {
        format!("LA {la} {name}")
    };
    if let Some(empty) = empty {
        summary.push_str(&format!(" · {empty}"));
        return summary;
    }
    let plural = if ticks.len() == 1 { "" } else { "s" };
    summary.push_str(&format!(" · {} frame{plural}", ticks.len()));
    let triggers: Vec<String> = ticks
        .iter()
        .filter(|tick| tick.kind == "trigger")
        .map(|tick| short_time(tick.at, span))
        .collect();
    if !triggers.is_empty() {
        let plural = if triggers.len() == 1 { "" } else { "s" };
        summary.push_str(&format!(" · trigger{plural} at {}", triggers.join(", ")));
    }
    summary
}

/// One observation lane while it is collected.
struct Collecting<'a> {
    key: String,
    label: String,
    /// (time, value, settle note) in window order.
    samples: Vec<(DateTime<Utc>, &'a str, Option<&'a str>)>,
    /// The device's current sample and when it was taken.
    current: Option<(String, DateTime<Utc>)>,
}

/// The index of the lane `key`, added when it is new.
fn lane_index<'a>(
    lanes: &mut Vec<Collecting<'a>>,
    key: String,
    label: String,
    current: Option<(String, DateTime<Utc>)>,
) -> usize {
    if let Some(index) = lanes.iter().position(|lane| lane.key == key) {
        return index;
    }
    lanes.push(Collecting {
        key,
        label,
        samples: vec![],
        current,
    });
    lanes.len() - 1
}

fn observation_class(value: &str) -> &'static str {
    match value {
        "on" | "awake" => "on",
        "unreachable" | "unavailable" => "na",
        _ => "off",
    }
}

/// Device-observation lanes: TV (webOS), receiver (Denon) and each probed role. Equal
/// consecutive samples merge into one run, which lasts until the next differing sample (or
/// the window's end, never past now); a run carries the note of the observation that
/// settled an expectation. A lane with no sample in the window shows the device's current
/// sample when it was taken before the window ended (it held from then on).
fn observations(
    snapshot: &Snapshot,
    entries: &[Entry],
    span: Span,
    now: DateTime<Utc>,
    memory: bool,
) -> Vec<ObservationLane> {
    let mut lanes: Vec<Collecting<'_>> = vec![];
    if snapshot.tv_direct.is_some() || has_topic(entries, "tv/state") {
        let current = snapshot
            .tv_direct
            .map(|(power, at)| (power.to_string(), at));
        lane_index(
            &mut lanes,
            "tv".to_owned(),
            "tv · webos".to_owned(),
            current,
        );
    }
    if snapshot.receiver_direct.is_some() || has_topic(entries, "receiver/state") {
        let current = snapshot
            .receiver_direct
            .map(|(power, at)| (power.to_string(), at));
        let label = "receiver · denon".to_owned();
        lane_index(&mut lanes, "receiver".to_owned(), label, current);
    }
    for role in snapshot.roles.iter().filter(|role| role.probe_configured) {
        let current = role.direct.map(|(state, at)| (state.to_string(), at));
        let (key, label) = (
            format!("probe:{}", role.role),
            format!("probe · {}", role.role),
        );
        lane_index(&mut lanes, key, label, current);
    }
    for entry in entries {
        let Item::Obs(observation) = &entry.item else {
            continue;
        };
        let (key, label) = match (observation.topic, &observation.role) {
            ("tv/state", _) => ("tv".to_owned(), "tv · webos".to_owned()),
            ("receiver/state", _) => ("receiver".to_owned(), "receiver · denon".to_owned()),
            ("probe/state", Some(role)) => (format!("probe:{role}"), format!("probe · {role}")),
            _ => continue,
        };
        // `unavailable · tv_direct (pairing required)` reads `unavailable`.
        let value = observation
            .value
            .split(" ·")
            .next()
            .unwrap_or(&observation.value);
        let index = lane_index(&mut lanes, key, label, None);
        lanes[index]
            .samples
            .push((entry.at, value, observation.note.as_deref()));
    }
    let end = span.to.min(now);
    lanes
        .into_iter()
        .map(|lane| {
            // (start, value, note) of each run of equal values.
            let mut runs: Vec<(DateTime<Utc>, String, Option<&str>)> = vec![];
            for (at, value, note) in &lane.samples {
                match runs.last_mut() {
                    Some((_, current, held)) if current == value => {
                        *held = held.or(*note);
                    }
                    _ => runs.push((*at, (*value).to_owned(), *note)),
                }
            }
            if lane.samples.is_empty()
                && let Some((value, at)) = lane.current
                && at <= end
            {
                runs.push((at, value, None));
            }
            let starts: Vec<DateTime<Utc>> = runs.iter().map(|(start, ..)| *start).collect();
            let runs: Vec<Band> = runs
                .into_iter()
                .enumerate()
                .filter_map(|(index, (start, value, note))| {
                    let until = starts.get(index + 1).copied().unwrap_or(end).min(end);
                    let start = start.max(span.from);
                    (until > start).then(|| Band {
                        start,
                        end: until,
                        x: span.x(start),
                        width: span.x(until) - span.x(start),
                        class: observation_class(&value),
                        label: match note {
                            Some(note) => format!("{value} — {note}"),
                            None => value,
                        },
                        open: index + 1 == starts.len(),
                    })
                })
                .collect();
            let empty = runs.is_empty().then(|| {
                if memory {
                    "not in memory: observations are read from the trace files".to_owned()
                } else {
                    "no samples in this window".to_owned()
                }
            });
            ObservationLane {
                summary: observation_summary(&lane.label, &runs, empty.as_deref(), span),
                key: lane.key,
                label: lane.label,
                runs,
                empty,
            }
        })
        .collect()
}

/// `tv · webos: unreachable → on at :56.4`, `tv · webos: no samples in this window`.
fn observation_summary(label: &str, runs: &[Band], empty: Option<&str>, span: Span) -> String {
    let mut summary = format!("{label}: ");
    if let Some(empty) = empty {
        summary.push_str(empty);
    }
    for (index, run) in runs.iter().enumerate() {
        if index == 0 {
            summary.push_str(&run.label);
        } else {
            summary.push_str(&format!(
                " → {} at {}",
                run.label,
                short_time(run.start, span)
            ));
        }
    }
    summary
}

fn has_topic(entries: &[Entry], topic: &str) -> bool {
    entries
        .iter()
        .any(|entry| matches!(&entry.item, Item::Obs(observation) if observation.topic == topic))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::{
        action::{Action, Trigger, Verdict, Via},
        state::{ConsoleState, DirectPower},
        web::view::{
            tests::{row, snapshot},
            window::{Observation, WindowDecision, WindowFrame},
        },
    };

    /// 17:08:40.860, the wake every test is built around.
    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 7, 17, 8, 40).unwrap() + TimeDelta::milliseconds(860)
    }

    fn ms(offset: i64) -> DateTime<Utc> {
        t0() + TimeDelta::milliseconds(offset)
    }

    /// The minute 17:08:30 → 17:09:30.
    fn minute() -> Span {
        Span {
            from: Utc.with_ymd_and_hms(2026, 11, 7, 17, 8, 30).unwrap(),
            to: Utc.with_ymd_and_hms(2026, 11, 7, 17, 9, 30).unwrap(),
        }
    }

    fn later() -> DateTime<Utc> {
        t0() + TimeDelta::hours(1)
    }

    fn frame(at: i64, dir: Dir, (from, to): (u8, u8), bytes: &[u8], result: Option<&str>) -> Entry {
        Entry {
            at: ms(at),
            item: Item::Frame(WindowFrame {
                dir,
                from,
                to,
                from_name: String::new(),
                to_name: String::new(),
                bytes: bytes.to_vec(),
                result: result.map(str::to_owned),
                tag: Tag::Other,
            }),
        }
    }

    fn observed(at: i64, topic: &'static str, value: &str, role: Option<&str>) -> Entry {
        Entry {
            at: ms(at),
            item: Item::Obs(Observation {
                topic,
                via: Some(Via::TvDirect),
                value: value.to_owned(),
                previous: None,
                entity: None,
                source: None,
                role: role.map(str::to_owned),
                sample_at: Some(ms(at)),
                changed: None,
                note: None,
            }),
        }
    }

    fn decision_entry(decision: &DecisionView) -> Entry {
        Entry {
            at: decision.timestamp,
            item: Item::Decision(WindowDecision {
                id: decision.id,
                timestamp: decision.timestamp,
                role: decision.role.clone(),
                reason: decision.reason.clone(),
                verdict: decision.verdict,
                would: decision.would,
                parent: decision.parent,
                own: false,
                linked: true,
            }),
        }
    }

    /// Console woke at t0 by reporting its physical address from LA 11.
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
    fn positions_are_percentages_clamped_to_the_window() {
        let span = minute();
        assert!((span.x(span.from) - 0.0).abs() < f64::EPSILON);
        assert!((span.x(span.to) - 100.0).abs() < f64::EPSILON);
        assert_eq!(pct(&span.x(t0())), "18.10");
        assert!((span.x(span.from - TimeDelta::minutes(5)) - 0.0).abs() < f64::EPSILON);
        assert!((span.x(span.to + TimeDelta::minutes(5)) - 100.0).abs() < f64::EPSILON);
        let empty = Span {
            from: span.to,
            to: span.to,
        };
        assert!((empty.x(t0()) - 0.0).abs() < f64::EPSILON);
        assert_eq!(span.line(), "Sat 7 Nov · 17:08:30 → 17:09:30");
    }

    #[test]
    fn axis_labels_every_sixth_spell_the_minute_only_when_it_changes() {
        let labels = |zoom| -> Vec<String> {
            axis(Span::around(minute().from + TimeDelta::seconds(30), zoom))
                .into_iter()
                .map(|label| label.label)
                .collect()
        };
        assert_eq!(
            labels(Zoom::M1),
            ["17:08:30", ":40", ":50", "17:09:00", ":10", ":20"]
        );
        assert_eq!(
            labels(Zoom::M5),
            [
                "17:06:30", "17:07:20", "17:08:10", "17:09:00", ":50", "17:10:40"
            ]
        );
        assert_eq!(
            labels(Zoom::M15),
            [
                "17:01:30", "17:04:00", "17:06:30", "17:09:00", "17:11:30", "17:14:00"
            ]
        );
        assert_eq!(
            labels(Zoom::H1),
            [
                "16:39:00", "16:49:00", "16:59:00", "17:09:00", "17:19:00", "17:29:00"
            ]
        );
        let xs: Vec<String> = axis(minute()).iter().map(|label| pct(&label.x)).collect();
        assert_eq!(xs, ["0.00", "16.67", "33.33", "50.00", "66.67", "83.33"]);
    }

    #[test]
    fn zoom_words_steps_and_bounds() {
        assert_eq!(Zoom::parse("15m"), Some(Zoom::M15));
        assert_eq!(Zoom::parse("2h"), None);
        let steps: Vec<String> = Zoom::ALL.iter().map(|zoom| zoom.step_label()).collect();
        assert_eq!(steps, ["6 s", "30 s", "90 s", "6 m"]);
        assert!(Zoom::ALL.iter().all(|zoom| zoom.span() <= MAX_SPAN));
    }

    #[test]
    fn address_lanes_hold_frames_by_sender_polls_by_target_and_silent_bound_roles() {
        let mut snapshot = snapshot();
        snapshot.roles[0].logical_address = Some(4);
        snapshot.roles[1].logical_address = Some(8);
        snapshot.roles[1].probe_configured = true;
        snapshot.roles[1].direct = Some((ConsoleState::Standby, ms(-60_000)));
        snapshot.decisions.push_back(wake());
        let mut entries = vec![
            frame(-2_000, Dir::Tx, (0, 11), &[], Some("nack")),
            frame(5, Dir::Rx, (11, 15), &[0x84, 0x23, 0x00, 0x04], Some("ack")),
            frame(500, Dir::Rx, (5, 11), &[0x46], Some("ack")),
            frame(1_000, Dir::Tx, (0, 11), &[0x8F], Some("ack · 2 attempts")),
            frame(1_200, Dir::Rx, (11, 0), &[0x90, 0x00], Some("ack")),
            decision_entry(&wake()),
        ];
        let triggers = tag_and_settle(&mut entries, &snapshot);
        assert_eq!(triggers, [(1, 844)]);
        let lanes = addresses(&snapshot, &entries, &triggers, minute(), Some(&wake()));
        type Lane<'a> = (u8, &'a str, Vec<&'a str>, Option<&'a str>, bool);
        let shape: Vec<Lane<'_>> = lanes
            .iter()
            .map(|lane| {
                (
                    lane.la,
                    lane.name.as_str(),
                    lane.ticks.iter().map(|tick| tick.kind).collect(),
                    lane.empty.as_deref(),
                    lane.focus,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (0, "bridge", vec!["ours"], None, false),
                (4, "Streamer", vec![], Some("silent"), false),
                (5, "LA 5", vec!["other"], None, false),
                (8, "Console", vec![], Some("silent (probe: standby)"), false),
                // The nacked poll of 11 sits in 11's lane; the reply to the bridge is other.
                (11, "LA 11", vec!["nack", "trigger", "other"], None, true),
            ]
        );
        let trigger = &lanes[4].ticks[1];
        assert_eq!(trigger.seq, 1);
        assert_eq!(trigger.note.as_deref(), Some("84 trigger"));
        assert_eq!(
            trigger.title,
            "17:08:40.865 · 11→15 · 84 23 00 04 · Report Physical Address · ack · trigger of #844"
        );
        assert!(lanes[4].ticks[0].poll && lanes[4].ticks[0].height() < trigger.height());
        // The text alternative: a name per track and a line per frame.
        let summaries: Vec<&str> = lanes.iter().map(|lane| lane.summary.as_str()).collect();
        assert_eq!(
            summaries,
            [
                "LA 0 bridge · 1 frame",
                "LA 4 Streamer · silent",
                "LA 5 · 1 frame",
                "LA 8 Console · silent (probe: standby)",
                "LA 11 · 3 frames · trigger at :40.8",
            ]
        );
        let lines: Vec<&str> = lanes[4]
            .ticks
            .iter()
            .map(|tick| tick.line.as_str())
            .collect();
        assert_eq!(
            lines,
            [
                "17:08:38.860 0→11 poll · nack",
                "17:08:40.865 11→15 84 Report Physical Address · trigger of #844",
                "17:08:42.060 11→0 90 Report Power Status: on",
            ]
        );
    }

    #[test]
    fn room_bands_replay_intent_across_the_window_edge() {
        let mut snapshot = snapshot();
        let at = |seconds: i64| minute().from + TimeDelta::seconds(seconds);
        let decision = |id, seconds, role: Option<&str>, to, reason: &str| DecisionView {
            timestamp: at(seconds),
            to,
            reason: reason.to_owned(),
            ..row(id, role, vec![])
        };
        snapshot.decisions.extend([
            // Before the window: the room went off, then the streamer woke it.
            decision(1, -600, None, None, "off_timer_expired"),
            decision(
                2,
                -20,
                Some("streamer"),
                Some(RolePower::On),
                "ActiveSource",
            ),
            decision(3, -18, Some("streamer"), None, "takeover_settled"),
            // In it: the console takes over (input settles 2.3 s later), then goes off.
            decision(
                4,
                10,
                Some("console"),
                Some(RolePower::On),
                "ReportPhysicalAddress",
            ),
            decision(5, 10, Some("streamer"), Some(RolePower::Off), "taken_over"),
            decision(6, 12, Some("console"), None, "takeover_settled"),
            decision(
                7,
                40,
                Some("console"),
                Some(RolePower::Off),
                "presence_timeout",
            ),
            decision(8, 50, None, None, "off_timer_expired"),
            // After the window: ignored.
            decision(9, 90, Some("streamer"), Some(RolePower::On), "ActiveSource"),
        ]);
        snapshot.decisions[5].timestamp += TimeDelta::milliseconds(300);
        // The settles' recorded select_source calls name the inputs (not today's config).
        snapshot.decisions[2].actions = vec![select_source("HDMI 1", false)];
        snapshot.decisions[5].actions = vec![select_source("Game", true)];
        let bands = room(&snapshot, minute(), later());
        let shape: Vec<(String, &str, String, String, bool)> = bands
            .iter()
            .map(|band| {
                (
                    band.label.clone(),
                    band.class,
                    pct(&band.x),
                    pct(&band.width),
                    band.open,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (
                    "on · streamer · input HDMI 1 from 17:08:12".to_owned(),
                    "on",
                    "0.00".to_owned(),
                    "16.67".to_owned(),
                    false
                ),
                (
                    "on · console · input Game from :42.3".to_owned(),
                    "on",
                    "16.67".to_owned(),
                    "50.00".to_owned(),
                    false
                ),
                (
                    "on · no active role".to_owned(),
                    "on",
                    "66.67".to_owned(),
                    "16.67".to_owned(),
                    false
                ),
                (
                    "off".to_owned(),
                    "off",
                    "83.33".to_owned(),
                    "16.67".to_owned(),
                    true
                ),
            ]
        );
        // Nothing is drawn after now, and a restart forgets the intent.
        let now = at(55);
        assert_eq!(room(&snapshot, minute(), now).last().unwrap().end, now);
        snapshot
            .decisions
            .push_back(decision(10, 52, None, None, "service_started"));
        let after_restart = room(&snapshot, minute(), later());
        assert_eq!(after_restart.last().unwrap().class, "unknown");
        // No decision before or in the window: unknown throughout.
        let empty = room(&super::super::tests::snapshot(), minute(), later());
        assert_eq!(empty.len(), 1);
        assert_eq!(
            (empty[0].class, pct(&empty[0].width)),
            ("unknown", "100.00".to_owned())
        );
    }

    /// A recorded `media_player.select_source` action, as the ring stores it.
    fn select_source(source: &str, would: bool) -> String {
        let json = serde_json::to_string(&Action::HaCall {
            domain: "media_player".to_owned(),
            service: "select_source".to_owned(),
            entity: "media_player.test_receiver".to_owned(),
            data: serde_json::json!({ "source": source }),
        })
        .unwrap();
        if would {
            format!("would: {json}")
        } else {
            json
        }
    }

    #[test]
    fn room_skips_counterfactual_rows_and_names_only_a_selected_input() {
        let mut snapshot = snapshot();
        let at = |seconds: i64| minute().from + TimeDelta::seconds(seconds);
        let labels = |snapshot: &Snapshot| -> Vec<String> {
            room(snapshot, minute(), later())
                .into_iter()
                .map(|band| band.label)
                .collect()
        };
        snapshot.decisions.extend([
            // console_b wakes; its settle selects nothing (`select_source = false`).
            DecisionView {
                timestamp: at(5),
                reason: "ReportPhysicalAddress".to_owned(),
                ..row(1, Some("console_b"), vec![])
            },
            DecisionView {
                timestamp: at(7),
                from: None,
                to: None,
                reason: "takeover_settled".to_owned(),
                actions: vec![r#"{"kind":"wake_tv"}"#.to_owned()],
                ..row(2, Some("console_b"), vec![])
            },
            // A repeated older-start announcement records the reversal a `would` policy
            // would make; the engine keeps console_b on and active.
            DecisionView {
                timestamp: at(20),
                from: Some(RolePower::On),
                to: Some(RolePower::Off),
                would: true,
                reason: "late_announcement".to_owned(),
                actions: vec![select_source("Console A", true)],
                ..row(3, Some("console_b"), vec![])
            },
            DecisionView {
                timestamp: at(25),
                from: Some(RolePower::Off),
                to: Some(RolePower::Off),
                would: true,
                reason: "console_start_expired".to_owned(),
                ..row(4, Some("console_b"), vec![])
            },
        ]);
        assert_eq!(labels(&snapshot), ["unknown", "on · console_b"]);
        // Shadow mode marks every row would: a real start (power changes) still counts.
        snapshot.decisions.push_back(DecisionView {
            timestamp: at(30),
            would: true,
            reason: "console_start".to_owned(),
            ..row(5, Some("console_a"), vec![])
        });
        assert_eq!(
            labels(&snapshot),
            ["unknown", "on · console_b", "on · console_a"]
        );
    }

    #[test]
    fn observation_runs_merge_equal_samples_and_keep_the_settle_note() {
        let mut snapshot = snapshot();
        let mut entries = vec![
            observed(-1_000, "tv/state", "unreachable", None),
            observed(4_000, "tv/state", "unreachable", None),
            observed(15_600, "tv/state", "on", None),
            observed(20_000, "tv/state", "on", None),
            observed(0, "probe/state", "standby", Some("console")),
        ];
        entries.sort_by_key(|entry| entry.at);
        if let Item::Obs(observation) = &mut entries[3].item {
            observation.note = Some("tv.power met (15.6 s)".to_owned());
        }
        let lanes = observations(&snapshot, &entries, minute(), later(), false);
        type Runs<'a> = (&'a str, Vec<(String, &'a str, bool)>);
        let shape: Vec<Runs<'_>> = lanes
            .iter()
            .map(|lane| {
                (
                    lane.label.as_str(),
                    lane.runs
                        .iter()
                        .map(|run| (run.label.clone(), run.class, run.open))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (
                    "tv · webos",
                    vec![
                        ("unreachable".to_owned(), "na", false),
                        ("on — tv.power met (15.6 s)".to_owned(), "on", true),
                    ]
                ),
                ("probe · console", vec![("standby".to_owned(), "off", true)]),
            ]
        );
        let tv = &lanes[0].runs;
        assert_eq!(
            tv[0].end, tv[1].start,
            "a run lasts until the next one starts"
        );
        assert_eq!(tv[1].end, minute().to);
        let summaries: Vec<&str> = lanes.iter().map(|lane| lane.summary.as_str()).collect();
        assert_eq!(
            summaries,
            [
                "tv · webos: unreachable → on — tv.power met (15.6 s) at :56.4",
                "probe · console: standby",
            ]
        );

        // Memory has no samples: the current one stands in from when it was taken, and a
        // probed role without one says where observations live.
        snapshot.tv_direct = Some((DirectPower::On, ms(10_000)));
        snapshot.roles[1].probe_configured = true;
        let lanes = observations(&snapshot, &[], minute(), later(), true);
        assert_eq!(lanes[0].runs.len(), 1);
        assert_eq!(lanes[0].runs[0].start, ms(10_000));
        assert_eq!(lanes[0].runs[0].label, "on");
        assert_eq!(
            lanes[1].empty.as_deref(),
            Some("not in memory: observations are read from the trace files")
        );
        // A current sample newer than the window says nothing about it.
        snapshot.tv_direct = Some((DirectPower::On, later()));
        let lanes = observations(&snapshot, &[], minute(), later(), true);
        assert!(lanes[0].runs.is_empty());
        assert_eq!(
            lanes[0].summary,
            "tv · webos: not in memory: observations are read from the trace files"
        );
    }

    #[test]
    fn markers_are_coloured_by_verdict_and_mark_the_selection() {
        let mut snapshot = snapshot();
        let partial = DecisionView {
            verdict: Verdict::Partial,
            ..wake()
        };
        let would = DecisionView {
            id: 845,
            would: true,
            timestamp: ms(2_000),
            ..wake()
        };
        let missed = DecisionView {
            id: 846,
            verdict: Verdict::Missed,
            timestamp: ms(3_000),
            ..wake()
        };
        snapshot
            .decisions
            .extend([partial.clone(), would.clone(), missed.clone()]);
        let mut entries: Vec<Entry> = [&partial, &would, &missed]
            .into_iter()
            .map(decision_entry)
            .collect();
        // A trace row no longer in the ring: not a link, named by its reason.
        let mut gone = decision_entry(&DecisionView {
            id: 700,
            timestamp: ms(4_000),
            verdict: Verdict::Achieved,
            ..wake()
        });
        if let Item::Decision(row) = &mut gone.item {
            row.linked = false;
        }
        entries.push(gone);
        let markers = markers(&snapshot, &entries, minute(), Some(&missed));
        let shape: Vec<(u64, &str, bool, bool)> = markers
            .iter()
            .map(|marker| (marker.id, marker.class, marker.linked, marker.selected))
            .collect();
        assert_eq!(
            shape,
            [
                (844, "warn", true, false),
                (845, "would", true, false),
                (846, "bad", true, true),
                (700, "ok", false, false),
            ]
        );
        assert_eq!(markers[3].headline, "#700 console · ReportPhysicalAddress");
        assert!(markers[0].title.starts_with("#844 · 17:08:40.860 · "));
        assert!(markers.iter().all(|marker| marker.show_id));
    }

    #[test]
    fn the_window_json_serialises_entries_and_the_lanes_that_draw_them() {
        let mut snapshot = snapshot();
        snapshot.decisions.push_back(wake());
        let mut entries = vec![
            observed(-100, "tv/state", "on", None),
            frame(5, Dir::Rx, (11, 15), &[0x84, 0x23, 0x00, 0x04], None),
            decision_entry(&wake()),
        ];
        let triggers = tag_and_settle(&mut entries, &snapshot);
        let lanes = lanes(
            &snapshot,
            &entries,
            &triggers,
            minute(),
            later(),
            None,
            false,
        );
        let json = serde_json::to_value(WindowJson {
            from: minute().from,
            to: minute().to,
            source: "memory".to_owned(),
            truncated: false,
            entries,
            lanes,
        })
        .unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["entries", "from", "lanes", "source", "to", "truncated"]
        );
        assert_eq!(json["from"], "2026-11-07T17:08:30Z");
        assert_eq!(json["truncated"], false);
        let kinds: Vec<&str> = json["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["obs", "frame", "decision"]);
        // A tick's seq is the index of the frame it draws, tagged as the wake's trigger.
        let tick = &json["lanes"]["addresses"][0]["ticks"][0];
        let seq = usize::try_from(tick["seq"].as_u64().unwrap()).unwrap();
        let drawn = &json["entries"][seq];
        assert_eq!(
            (
                &drawn["kind"],
                &drawn["tag"],
                &drawn["from"],
                &drawn["bytes"]
            ),
            (
                &serde_json::json!("frame"),
                &serde_json::json!("trigger"),
                &serde_json::json!(11),
                &serde_json::json!([0x84, 0x23, 0x00, 0x04])
            )
        );
        assert_eq!(tick["kind"], "trigger");
        assert_eq!(tick["at"], drawn["at"]);
        assert_eq!(json["lanes"]["decisions"][0]["id"], 844);
        assert_eq!(json["lanes"]["observations"][0]["key"], "tv");
        assert_eq!(json["lanes"]["observations"][0]["runs"][0]["label"], "on");
    }
}
