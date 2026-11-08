//! `/` and `/p/cockpit`: room strip, verification, alerts, role cards, outputs and the last
//! decisions, all derived from one snapshot.

use askama::Template;
use axum::{extract::State, response::Html};
use chrono::{DateTime, Utc};

use crate::{
    action::{Field, Outcome, Verdict},
    state::{DecisionView, DirectPower, Mode, RolePower, RoleView, RoomPower, Snapshot},
    web::{
        WebError, WebState,
        auth::Ctx,
        render,
        view::{self, ActionSummary, Consistency, Layout, Segment, Status},
    },
};

/// Rows shown in the last-decisions table (service-start separators do not count).
const RECENT_ROWS: usize = 5;

#[derive(Template)]
#[template(path = "cockpit.html")]
struct CockpitPage {
    layout: Option<Layout>,
    cockpit: Cockpit,
}

/// The polled partial: `main#cockpit` plus the chrome's `#status` and `#bands`.
#[derive(Template)]
#[template(path = "partials/cockpit.html")]
struct CockpitPartial {
    cockpit: Cockpit,
    status: Status,
}

pub(crate) async fn page(State(web): State<WebState>, ctx: Ctx) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    render(&CockpitPage {
        layout: Some(Layout::new(ctx, "cockpit", &snapshot)),
        cockpit: Cockpit::new(&snapshot, Utc::now()),
    })
}

pub(crate) async fn partial(State(web): State<WebState>) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    render(&CockpitPartial {
        cockpit: Cockpit::new(&snapshot, Utc::now()),
        status: Status::new(&snapshot, false),
    })
}

pub(crate) struct Cockpit {
    shadow: bool,
    warming: bool,
    consistency: Consistency,
    room: Room,
    verifying: Vec<Verifying>,
    alerts: Vec<Alert>,
    cards: Vec<RoleCard>,
    outputs: [Output; 3],
    recent: Vec<RecentRow>,
}

struct Room {
    power: &'static str,
    /// The wake (or off) row that set the current intent.
    since: Option<(u64, DateTime<Utc>)>,
    active: Option<ActiveSource>,
    receiver_source: Option<Segment>,
    receiver_value: String,
    /// The intended receiver input when the observed one differs.
    intended_source: Option<String>,
    off_value: String,
    off_detail: String,
    off_running: bool,
}

struct ActiveSource {
    name: String,
    role: String,
    logical_address: Option<u8>,
    since: Option<DateTime<Utc>>,
}

struct Verifying {
    id: u64,
    items: Vec<VerifyItem>,
    elapsed_ms: u64,
    max_ms: u64,
}

impl Verifying {
    fn progress(&self) -> String {
        format!(
            "{} s / {} s",
            self.elapsed_ms.min(self.max_ms) / 1000,
            self.max_ms / 1000
        )
    }

    fn value(&self) -> u64 {
        self.elapsed_ms.min(self.max_ms)
    }
}

struct VerifyItem {
    name: String,
    expect: String,
    /// Chip for a settled expectation: (tone, label).
    chip: Option<(&'static str, &'static str)>,
    detail: String,
}

struct Alert {
    tone: &'static str,
    title: String,
    detail: String,
    bridge_button: bool,
    devices_link: bool,
}

struct RoleCard {
    name: String,
    role: String,
    logical_address: Option<u8>,
    physical_address: Option<String>,
    identity: (&'static str, &'static str),
    on: bool,
    active: bool,
    power: Vec<Segment>,
    /// Bound to a bus entry restored from the state file and not yet re-confirmed.
    restored: bool,
    restored_at: Option<DateTime<Utc>>,
    last: Option<DecisionView>,
    line: Option<String>,
    edge: &'static str,
    show_takeover: bool,
    show_standby: bool,
    /// Why the power actions are disabled, if they are.
    disabled: Option<&'static str>,
}

struct Output {
    name: &'static str,
    chip: (&'static str, String),
    lines: Vec<OutputLine>,
    note: Option<String>,
}

struct OutputLine {
    key: &'static str,
    segments: Vec<Segment>,
    extra: Option<String>,
}

struct RecentRow {
    decision: DecisionView,
    separator: bool,
    role_name: Option<String>,
    actions: String,
    latency: Option<String>,
}

impl Cockpit {
    fn new(snapshot: &Snapshot, now: DateTime<Utc>) -> Self {
        let shadow = snapshot.mode == Mode::Shadow;
        let pending = view::pending(snapshot);
        Self {
            shadow,
            warming: snapshot.warming,
            consistency: view::consistency(snapshot),
            room: room(snapshot, now),
            verifying: pending
                .iter()
                .map(|decision| verifying(decision, now))
                .collect(),
            alerts: alerts(snapshot),
            cards: snapshot
                .roles
                .iter()
                .map(|role| role_card(snapshot, role))
                .collect(),
            outputs: [
                tv(snapshot, &pending),
                receiver(snapshot),
                speakers(snapshot),
            ],
            recent: recent(snapshot),
        }
    }
}

/// Wake rows (the role came on) and room-off rows (timer or all off).
fn sets_intent(decision: &DecisionView, power: RoomPower) -> bool {
    match power {
        RoomPower::On => decision.to == Some(RolePower::On),
        RoomPower::Off => matches!(
            decision.reason.as_str(),
            "off_timer_expired" | "all_off" | "command_all_off"
        ),
        RoomPower::Unknown => false,
    }
}

fn room(snapshot: &Snapshot, now: DateTime<Utc>) -> Room {
    let intent = &snapshot.intent;
    let since = snapshot
        .decisions
        .iter()
        .rev()
        .find(|decision| !view::is_migrated(decision) && sets_intent(decision, intent.power))
        .map(|decision| (decision.id, decision.timestamp));
    let active = snapshot.active_role.as_ref().map(|role| ActiveSource {
        name: view::role_name(snapshot, role),
        role: role.clone(),
        logical_address: snapshot
            .roles
            .iter()
            .find(|view| &view.role == role)
            .and_then(|view| view.logical_address),
        since: snapshot
            .decisions
            .iter()
            .rev()
            .find(|decision| {
                decision.role.as_deref() == Some(role) && decision.to == Some(RolePower::On)
            })
            .map(|decision| decision.timestamp),
    });
    let receiver = &snapshot.ha.receiver;
    let observed_source = view::ha_attribute(receiver, "source");
    let receiver_source = observed_source
        .as_ref()
        .map(|source| Segment::new("ha", source.clone(), receiver.last_changed))
        .map(|segment| Segment {
            used: true,
            ..segment
        });
    let intended_source = intent
        .receiver_source
        .clone()
        .filter(|intended| observed_source.as_ref() != Some(intended));
    let (off_value, off_detail, off_running) = off_timer(snapshot, now);
    Room {
        power: match intent.power {
            RoomPower::On => "On",
            RoomPower::Off => "Off",
            RoomPower::Unknown => "Unknown",
        },
        since,
        active,
        receiver_value: observed_source.unwrap_or_else(|| "—".to_owned()),
        receiver_source,
        intended_source,
        off_value,
        off_detail,
        off_running,
    }
}

/// The off timer, shown only from the deadline the snapshot carries. `intent.off_at` is
/// when the room last went off (history, not a deadline), so it is never read here.
fn off_timer(snapshot: &Snapshot, now: DateTime<Utc>) -> (String, String, bool) {
    match snapshot.off_deadline {
        Some(at) => {
            let seconds = (at - now).num_seconds().max(0);
            (
                format!("off in {seconds} s"),
                format!("off at {} (in {seconds} s)", view::hms(&at)),
                true,
            )
        }
        None => (
            "—".to_owned(),
            "Starts when the last role turns off.".to_owned(),
            false,
        ),
    }
}

fn verifying(decision: &DecisionView, now: DateTime<Utc>) -> Verifying {
    let items = decision
        .expectations
        .iter()
        .map(|expectation| {
            let (chip, detail) = match &expectation.outcome {
                Outcome::Pending => (
                    None,
                    format!(
                        "waiting for {}",
                        expectation
                            .via
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(" / ")
                    ),
                ),
                Outcome::Met {
                    via, latency_ms, ..
                } => (
                    Some(("ok", "met")),
                    format!("via {via} {}", view::latency(latency_ms)),
                ),
                Outcome::Missed { observed } => {
                    (Some(("bad", "missed")), format!("observed {observed}"))
                }
                Outcome::Unknown { .. } => (Some(("dim", "unknown")), String::new()),
                Outcome::CecOnly { .. } => (Some(("ok", "cec only")), String::new()),
            };
            VerifyItem {
                name: format!("{}.{}", expectation.subject, expectation.field),
                expect: expectation.expect.clone(),
                chip,
                detail,
            }
        })
        .collect();
    Verifying {
        id: decision.id,
        items,
        elapsed_ms: u64::try_from((now - decision.timestamp).num_milliseconds()).unwrap_or(0),
        max_ms: decision
            .expectations
            .iter()
            .map(|expectation| expectation.deadline_ms)
            .max()
            .unwrap_or(0),
    }
}

/// Most severe first: bridge offline, bus fault, identity conflicts, warming.
fn alerts(snapshot: &Snapshot) -> Vec<Alert> {
    let mut alerts = Vec::new();
    if !snapshot.bridge_online {
        alerts.push(Alert {
            tone: "bad",
            title: "Bridge offline".to_owned(),
            detail: snapshot.last_frame_at.map_or_else(
                || "No frame has arrived yet; Choregos cannot see or reach the bus.".to_owned(),
                |at| {
                    format!(
                        "No frames since {}; bus and power values below are as of then.",
                        view::when(&at)
                    )
                },
            ),
            bridge_button: true,
            devices_link: false,
        });
    }
    if let Some(fault) = view::bus_fault(snapshot) {
        alerts.push(Alert {
            tone: "bad",
            title: format!("Bus fault: {fault}"),
            detail: format!(
                "{} Choregos keeps its last known state until the line recovers.",
                view::bus_fault_sentence(fault)
            ),
            bridge_button: true,
            devices_link: false,
        });
    }
    for conflict in &snapshot.conflicts {
        let addresses = conflict
            .logical_addresses
            .iter()
            .map(|la| format!("LA {la}"))
            .collect::<Vec<_>>()
            .join(", ");
        alerts.push(Alert {
            tone: "bad",
            title: format!("Identity conflict: {}", conflict.roles.join(", ")),
            detail: format!(
                "{addresses}: {}. These roles cannot trigger power actions until a device is forgotten or the match rule is narrowed.",
                conflict.reason
            ),
            bridge_button: false,
            devices_link: true,
        });
    }
    if snapshot.warming {
        alerts.push(Alert {
            tone: "pend",
            title: "Warming up".to_owned(),
            detail: "For a few seconds after a start, resync or reload Choregos refuses actions so the bus model can settle; this clears on the next refresh.".to_owned(),
            bridge_button: false,
            devices_link: false,
        });
    }
    alerts
}

fn role_card(snapshot: &Snapshot, role: &RoleView) -> RoleCard {
    let restored = view::is_restored(role);
    let identity = view::role_identity(role);
    let view::RoleSources { cec, ha, probe } = view::role_sources(snapshot, role);

    let last = view::last_decision_for(snapshot, &role.role);
    let line = last.and_then(|decision| match view::wake_row(snapshot, decision) {
        Some(wake) => view::wake_line(wake),
        None => standby_line(snapshot, decision, &role.role),
    });
    let restored_at = restored
        .then(|| {
            snapshot
                .decisions
                .iter()
                .rev()
                .find(|decision| decision.reason == "service_started")
                .map(|decision| decision.timestamp)
        })
        .flatten();
    let active = snapshot.active_role.as_deref() == Some(role.role.as_str());
    let disabled = if role.logical_address.is_none() {
        Some("not verified yet")
    } else if snapshot.warming {
        Some("warming: actions are refused until the bus model settles")
    } else {
        None
    };
    RoleCard {
        name: role.name.clone(),
        role: role.role.clone(),
        logical_address: role.logical_address,
        physical_address: role.physical_address.clone(),
        identity,
        on: role.power == RolePower::On,
        active,
        power: vec![cec, ha, probe],
        restored,
        restored_at,
        edge: last.map_or("", |decision| view::verdict_edge(&decision.verdict)),
        line,
        last: last.cloned(),
        show_takeover: !active,
        show_standby: active || role.power == RolePower::On,
        disabled,
    }
}

/// The newest standby verification of `role` recorded with or after `decision` (a role put
/// in standby by a take-over is verified on the waking role's row).
fn standby_line(snapshot: &Snapshot, decision: &DecisionView, role: &str) -> Option<String> {
    snapshot
        .decisions
        .iter()
        .rev()
        .take_while(|row| row.timestamp >= decision.timestamp)
        .find_map(|row| view::standby_line(row, role))
}

fn power_chip(on: Option<bool>) -> (&'static str, String) {
    match on {
        Some(true) => ("ok", "on".to_owned()),
        Some(false) => ("dim", "off".to_owned()),
        None => ("none", "unknown".to_owned()),
    }
}

fn drifting<'a>(snapshot: &'a Snapshot, subject: &str) -> Vec<&'a crate::action::DriftDetail> {
    snapshot
        .drift
        .iter()
        .filter(|drift| drift.subject == subject)
        .collect()
}

fn tv(snapshot: &Snapshot, pending: &[&DecisionView]) -> Output {
    let shadow = snapshot.mode == Mode::Shadow;
    let ha = &snapshot.ha.tv;
    // The TV leaves the network in standby, so Home Assistant reports it unavailable or
    // unknown whenever the room is off: that reads as off, not as a fault.
    let ha_unreachable = matches!(ha.state.as_str(), "unavailable" | "unknown");
    let mut ha_segment = Segment::new(
        "ha",
        if ha_unreachable {
            "off (unreachable)".to_owned()
        } else {
            ha.state.clone()
        },
        ha.last_changed,
    );
    let mut direct = match snapshot.tv_direct {
        Some((state, at)) if !shadow => Segment::new("webos", state.to_string(), Some(at)),
        _ => Segment::absent("webos", "—"),
    };
    let drifts = drifting(snapshot, "tv");
    for drift in &drifts {
        match drift.via {
            crate::action::Via::TvDirect => direct.disagrees = true,
            crate::action::Via::Ha => ha_segment.disagrees = true,
            _ => {}
        }
    }
    let on = if direct.absent {
        ha_segment.used = true;
        if ha_unreachable {
            Some(false)
        } else {
            view::ha_power(&ha.state)
        }
    } else {
        direct.used = true;
        snapshot
            .tv_direct
            .map(|(state, _)| state == DirectPower::On)
    };
    let waking = pending.iter().find_map(|decision| {
        decision
            .expectations
            .iter()
            .find(|e| {
                e.subject == "tv"
                    && e.field == crate::action::Field::Power
                    && e.expect == "on"
                    && e.outcome == Outcome::Pending
            })
            .map(|e| {
                decision.timestamp
                    + chrono::TimeDelta::milliseconds(
                        i64::try_from(e.deadline_ms).unwrap_or(i64::MAX),
                    )
            })
    });
    let chip = if waking.is_some() {
        ("pend", "waking".to_owned())
    } else if !drifts.is_empty() {
        ("warn", "drift".to_owned())
    } else {
        power_chip(on)
    };
    let intent = match (snapshot.intent.power, waking) {
        (RoomPower::On, Some(by)) => format!("on · expected by {}", view::hms(&by)),
        (power, _) => power.to_string(),
    };
    let note = if shadow {
        Some("In shadow there are no direct samples: the webOS segment stays empty instead of showing a stale value.".to_owned())
    } else if matches!(snapshot.tv_direct, Some((DirectPower::Unreachable, _))) {
        Some(
            "The TV drops off the network in standby and while it boots; unreachable reads as off."
                .to_owned(),
        )
    } else {
        None
    };
    Output {
        name: "TV",
        chip,
        lines: vec![
            OutputLine {
                key: "power",
                segments: vec![direct, ha_segment],
                extra: None,
            },
            OutputLine {
                key: "intent",
                segments: vec![],
                extra: Some(intent),
            },
        ],
        note,
    }
}

fn receiver(snapshot: &Snapshot) -> Output {
    let shadow = snapshot.mode == Mode::Shadow;
    let ha = &snapshot.ha.receiver;
    let mut ha_power = Segment::new("ha", ha.state.clone(), ha.last_changed);
    let mut direct = match snapshot.receiver_direct {
        Some((state, at)) if !shadow => Segment::new("denon", state.to_string(), Some(at)),
        _ => Segment::absent("denon", "—"),
    };
    let drifts = drifting(snapshot, "receiver");
    let source_drift = drifts.iter().any(|drift| drift.field == Field::Source);
    for drift in drifts.iter().filter(|drift| drift.field == Field::Power) {
        match drift.via {
            crate::action::Via::Denon => direct.disagrees = true,
            crate::action::Via::Ha => ha_power.disagrees = true,
            _ => {}
        }
    }
    let on = match snapshot.receiver_direct {
        Some((state, _)) if !direct.absent && state != DirectPower::Unreachable => {
            direct.used = true;
            Some(state == DirectPower::On)
        }
        _ => {
            ha_power.used = true;
            view::ha_power(&ha.state)
        }
    };
    let attribute_line = |key: &'static str, attribute: &str| {
        view::ha_attribute(ha, attribute).map(|value| {
            let mut segment = Segment::new("ha", value, ha.last_changed);
            segment.used = true;
            OutputLine {
                key,
                segments: vec![segment],
                extra: None,
            }
        })
    };
    let mut lines = vec![OutputLine {
        key: "power",
        segments: vec![direct, ha_power],
        extra: None,
    }];
    lines.extend(attribute_line("source", "source").map(|mut line| {
        // Receiver source drift is judged on Home Assistant's source only.
        for segment in &mut line.segments {
            segment.disagrees = source_drift;
        }
        line
    }));
    let preset = preset(snapshot);
    match attribute_line("sound", "sound_mode") {
        Some(mut sound) => {
            sound.extra = preset;
            lines.push(sound);
        }
        None => lines.extend(preset.map(|preset| OutputLine {
            key: "sound",
            segments: vec![],
            extra: Some(preset),
        })),
    }
    Output {
        name: "Receiver",
        chip: if drifts.is_empty() {
            power_chip(on)
        } else {
            ("warn", "drift".to_owned())
        },
        lines,
        note: shadow.then(|| {
            "In shadow there are no direct samples: the receiver segment stays empty.".to_owned()
        }),
    }
}

/// The preset script the newest settled take-over ran, by its short name.
fn preset(snapshot: &Snapshot) -> Option<String> {
    let settled = snapshot
        .decisions
        .iter()
        .rev()
        .find(|decision| decision.reason == "takeover_settled")?;
    settled.actions.iter().find_map(|action| {
        let json = action.strip_prefix("would: ").unwrap_or(action);
        let value: serde_json::Value = serde_json::from_str(json).ok()?;
        (value.get("kind")?.as_str()? == "ha_call"
            && value.get("domain")?.as_str()? == "script"
            && value.get("service")?.as_str()? == "turn_on")
            .then(|| value.get("entity")?.as_str())
            .flatten()
            .map(|entity| {
                format!(
                    "preset {}",
                    entity.strip_prefix("script.").unwrap_or(entity)
                )
            })
    })
}

fn speakers(snapshot: &Snapshot) -> Output {
    let ha = &snapshot.ha.sonos;
    let mut state = Segment::new("ha", ha.state.clone(), ha.last_changed);
    state.used = true;
    let volume = ha
        .attributes
        .get("volume_level")
        .and_then(serde_json::Value::as_f64)
        .map(|level| format!("{:.0}", level * 100.0));
    let muted = ha
        .attributes
        .get("is_volume_muted")
        .and_then(serde_json::Value::as_bool)
        .map(|muted| if muted { "muted" } else { "not muted" });
    let mut lines = vec![OutputLine {
        key: "state",
        segments: vec![state],
        extra: None,
    }];
    if volume.is_some() || muted.is_some() {
        lines.push(OutputLine {
            key: "volume",
            segments: vec![],
            extra: Some(
                [volume.as_deref(), muted]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · "),
            ),
        });
    }
    Output {
        name: "Speakers",
        chip: power_chip(view::ha_power(&ha.state)),
        lines,
        note: Some("Speakers have no direct probe; Home Assistant is the only source.".to_owned()),
    }
}

/// The newest rows, migrated history excluded; a service start becomes a separator that
/// does not count toward the limit.
fn recent(snapshot: &Snapshot) -> Vec<RecentRow> {
    let mut rows = Vec::new();
    let mut counted = 0;
    for decision in snapshot.decisions.iter().rev() {
        if counted == RECENT_ROWS {
            break;
        }
        if view::is_migrated(decision) {
            continue;
        }
        let separator = decision.reason == "service_started";
        if !separator {
            counted += 1;
        }
        rows.push(RecentRow {
            separator,
            role_name: decision
                .role
                .as_deref()
                .map(|role| view::role_name(snapshot, role)),
            actions: ActionSummary::from_actions(&decision.actions).text(),
            latency: if decision.verdict == Verdict::Pending {
                None
            } else {
                view::max_latency(decision).map(|ms| view::latency(&ms))
            },
            decision: decision.clone(),
        });
    }
    rows
}
