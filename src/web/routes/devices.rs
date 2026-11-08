//! `/devices`: the registry (roles, outputs, the bridge, unbound bus entries) and one page
//! per role, output, the bridge and unbound bus entry. Device profiles come from the
//! configuration text, parsed per request.

use askama::Template;
use axum::{
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{DateTime, Utc};

use crate::{
    config::{Config, DeviceConfig, Presence, ProbeConfig, ReceiverObserveConfig, TvOffConfig},
    state::{FrameView, Mode, RolePower, RoleView, Snapshot},
    web::{
        WebError, WebState,
        auth::Ctx,
        render,
        view::{
            self, Layout,
            devices::{
                self as dv, Agreement, DriftPair, IdentityEvent, IdentityRow, Involved, Keys,
                Measure, OutputKind, Part, Registry, Sentence,
            },
        },
    },
};

/// Frames listed on an unbound entry's page.
const ENTRY_FRAMES: usize = 20;

fn config(web: &WebState) -> Option<Config> {
    Config::parse(&web.app.config_text()).ok()
}

// ---- registry -----------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "devices.html")]
struct RegistryPage {
    layout: Option<Layout>,
    registry: Registry,
}

/// The polled `#registry` block alone.
#[derive(Template)]
#[template(path = "partials/registry.html")]
struct RegistryPartial {
    registry: Registry,
}

pub(crate) async fn page(State(web): State<WebState>, ctx: Ctx) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    render(&RegistryPage {
        layout: Some(Layout::new(ctx, "devices", &snapshot)),
        registry: dv::registry(&snapshot, config(&web).as_ref(), Utc::now()),
    })
}

pub(crate) async fn partial(State(web): State<WebState>) -> Result<Html<String>, WebError> {
    render(&RegistryPartial {
        registry: dv::registry(&web.app.snapshot(), config(&web).as_ref(), Utc::now()),
    })
}

// ---- detail pages -------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "device.html")]
struct DevicePage {
    layout: Option<Layout>,
    title: String,
    body: Body,
}

enum Body {
    Role(Box<RolePage>),
    Output(Box<OutputPage>),
    Bridge(Box<BridgePage>),
    Entry(Box<EntryPage>),
}

/// One truth source's column in a "Power, by source" panel.
struct SourceCol {
    title: &'static str,
    value: String,
    lines: Vec<String>,
    used: bool,
    absent: bool,
    disagrees: bool,
}

impl SourceCol {
    fn new(title: &'static str, value: impl Into<String>, lines: Vec<String>) -> Self {
        Self {
            title,
            value: value.into(),
            lines,
            used: false,
            absent: false,
            disagrees: false,
        }
    }

    fn absent(title: &'static str, lines: Vec<String>) -> Self {
        Self {
            absent: true,
            ..Self::new(title, "—", lines)
        }
    }

    fn class(&self) -> &'static str {
        match (self.used, self.disagrees, self.absent) {
            (_, true, _) => "src-col dis",
            (true, _, _) => "src-col use",
            (_, _, true) => "src-col na",
            _ => "src-col",
        }
    }
}

/// Marks the sources whose reading differs from the used one's.
fn mark_disagreeing(columns: &mut [SourceCol], readings: &[Option<bool>]) {
    let used = columns
        .iter()
        .zip(readings)
        .find_map(|(column, reading)| column.used.then_some(*reading).flatten());
    if let Some(used) = used {
        for (column, reading) in columns.iter_mut().zip(readings) {
            column.disagrees = reading.is_some_and(|reading| reading != used);
        }
    }
}

struct RolePage {
    name: String,
    role: String,
    identity: (&'static str, &'static str),
    on: bool,
    active: bool,
    sub: String,
    shadow: bool,
    warming: bool,
    /// The bound address (Identify, Forget).
    la: Option<u8>,
    show_takeover: bool,
    show_standby: bool,
    disabled: Option<&'static str>,
    restored: Option<Restored>,
    sources: Vec<SourceCol>,
    agreement: Agreement,
    sources_note: &'static str,
    keys: Vec<IdentityRow>,
    confirmed: String,
    rule: Option<String>,
    events: Vec<IdentityEvent>,
    measures: Vec<Measure>,
    quirks: Vec<Sentence>,
    involved: Vec<Involved>,
}

/// The empty state of a role restored from the state file and not heard since.
struct Restored {
    la: Option<u8>,
    stored: Option<String>,
    at: Option<DateTime<Utc>>,
    heard: String,
    ha: Option<String>,
}

fn frame_line(frame: &FrameView) -> String {
    let bytes: Vec<u8> = frame
        .opcode
        .into_iter()
        .chain(frame.params.iter().copied())
        .collect();
    format!(
        "{} → {} · {} {}",
        frame.from,
        frame.to,
        view::hex_bytes(&bytes),
        frame.opcode_name
    )
}

/// The newest frame `la` sent, from the memory ring, after `since` when given.
fn last_frame_from(
    snapshot: &Snapshot,
    la: u8,
    since: Option<DateTime<Utc>>,
) -> Option<&FrameView> {
    snapshot
        .recent_frames
        .iter()
        .rev()
        .find(|frame| frame.from == la && since.is_none_or(|since| frame.timestamp > since))
}

fn probe_host(probe: &ProbeConfig) -> &str {
    match probe {
        ProbeConfig::Ps5 { host, .. } => host,
    }
}

/// The role's "Power, by source" columns (CEC, Home Assistant, probe), their agreement and
/// which one Choregos uses.
fn power_by_source(
    snapshot: &Snapshot,
    role: &RoleView,
    device: Option<&DeviceConfig>,
) -> (Vec<SourceCol>, Agreement, &'static str) {
    let shadow = snapshot.mode == Mode::Shadow;
    let bound = view::bound_entry(snapshot, role);
    let segments = view::role_sources(snapshot, role);
    let cec = match bound {
        Some(entry) => {
            let mut lines = Vec::new();
            if let Some(seen) = &role.last_seen {
                lines.push(format!(
                    "{} · LA {}",
                    view::since(seen),
                    entry.logical_address
                ));
            }
            if let Some(frame) = last_frame_from(snapshot, entry.logical_address, None) {
                lines.push(format!("last frame {}", frame_line(frame)));
            }
            SourceCol::new("CEC · bus", view::bus_power(&entry.power), lines)
        }
        None if view::is_restored(role) => {
            SourceCol::absent("CEC · bus", vec!["silent since restore".to_owned()])
        }
        None => SourceCol::absent("CEC · bus", vec!["not bound to a bus entry".to_owned()]),
    };
    let ha = match (&role.media_player, view::ha_entity_for(snapshot, role)) {
        (Some(player), Some(entity)) => SourceCol::new(
            "Home Assistant",
            entity.state.clone(),
            vec![player.clone(), view::ago(&entity.last_changed)],
        ),
        (Some(player), None) => SourceCol::new("Home Assistant", "unknown", vec![player.clone()]),
        (None, _) => SourceCol::absent(
            "Home Assistant",
            vec!["not configured — optional for this role".to_owned()],
        ),
    };
    let probe = match device.and_then(|device| device.probe.as_ref()) {
        Some(config) if role.probe_configured => {
            let idle = format!("polled every {} s while idle", config.idle_poll_s());
            match role.direct {
                _ if shadow => SourceCol::absent("Probe", vec!["not sampled in shadow".to_owned()]),
                Some((state, at)) => {
                    SourceCol::new("Probe", state.to_string(), vec![view::since(&at), idle])
                }
                None => SourceCol::absent("Probe", vec!["not sampled yet".to_owned(), idle]),
            }
        }
        _ if role.probe_configured => SourceCol::absent("Probe", vec![]),
        _ => SourceCol::absent("Probe", vec!["no probe".to_owned()]),
    };
    let mut sources = vec![cec, ha, probe];
    sources[0].used = segments.cec.used;
    sources[2].used = segments.probe.used;
    let readings = [
        bound.and_then(|entry| dv::bus_reading(entry.power)),
        role.media_player
            .as_ref()
            .and_then(|_| view::ha_entity_for(snapshot, role))
            .and_then(|entity| view::ha_power(&entity.state)),
        role.direct
            .filter(|_| !shadow)
            .and_then(|(state, _)| dv::probe_reading(state)),
    ];
    let agreement = dv::agreement(&readings);
    mark_disagreeing(&mut sources, &readings);
    let note = if segments.probe.used {
        "Choregos uses the probe; CEC is the bus view"
    } else if segments.cec.used {
        "Choregos uses the device's own CEC events"
    } else {
        "no source has reported yet"
    };
    (sources, agreement, note)
}

/// When and how the identity was last confirmed.
fn confirmed_line(
    role: &RoleView,
    bound_confirmed: Option<DateTime<Utc>>,
    physical: Option<&str>,
    started: Option<DateTime<Utc>>,
    has_entry: bool,
) -> String {
    match bound_confirmed {
        Some(at) if started.is_some_and(|started| at < started) => format!(
            "{} · stored before the restart; vendor and OSD re-confirmed it since",
            view::when_ms(&at)
        ),
        Some(at) => format!(
            "{} · live Report Physical Address matched the stored {}",
            view::when_ms(&at),
            physical.unwrap_or("address")
        ),
        None if has_entry && view::is_restored(role) => match started {
            Some(at) => format!("restored {} · from state file", view::when_ms(&at)),
            None => "restored · from state file".to_owned(),
        },
        None => "not yet".to_owned(),
    }
}

/// The empty state of a role waiting on `waiting`, the entry the state file brought back.
fn restored_view(
    snapshot: &Snapshot,
    role: &RoleView,
    waiting: Option<&crate::state::BusEntryView>,
    started: Option<DateTime<Utc>>,
) -> Restored {
    Restored {
        la: waiting.map(|entry| entry.logical_address),
        stored: waiting.map(|entry| Keys::of(entry).line()),
        at: started,
        heard: waiting
            .and_then(|entry| last_frame_from(snapshot, entry.logical_address, started))
            .map_or_else(
                || "nothing on the bus".to_owned(),
                |frame| format!("{} · {}", frame_line(frame), view::since(&frame.timestamp)),
            ),
        ha: role.media_player.as_ref().map(|player| {
            view::ha_entity_for(snapshot, role).map_or_else(
                || format!("{player} unknown"),
                |entity| {
                    let age = entity
                        .last_changed
                        .as_ref()
                        .map(|at| format!(" · {}", view::age(at)))
                        .unwrap_or_default();
                    format!("{player} {}{age}", entity.state)
                },
            )
        }),
    }
}

fn role_page(snapshot: &Snapshot, role: &RoleView, config: Option<&Config>) -> RolePage {
    let device: Option<&DeviceConfig> =
        config.and_then(|config| config.device.iter().find(|d| d.role == role.role));
    let shadow = snapshot.mode == Mode::Shadow;
    let restored = view::is_restored(role);
    let bound = view::bound_entry(snapshot, role);
    let waiting = device
        .filter(|_| restored)
        .and_then(|device| dv::restored_entry(snapshot, device));
    let entry = bound.or(waiting);
    let keys = entry.map_or_else(|| Keys::of_role(role), Keys::of);
    let started = dv::service_started(snapshot);
    let (sources, agreement, sources_note) = power_by_source(snapshot, role, device);

    // Identity.
    let keys_rows = match device {
        Some(device) => dv::identity_rows(&device.matches, &keys),
        None => dv::identity_rows(
            &crate::config::MatchConfig {
                osd: None,
                vendor: None,
                physical: None,
            },
            &keys,
        )
        .into_iter()
        .map(|row| IdentityRow { check: None, ..row })
        .collect(),
    };
    let confirmed = confirmed_line(
        role,
        bound.and_then(|entry| entry.confirmed_at),
        keys.physical.as_deref(),
        started,
        entry.is_some(),
    );

    let polled = device.is_some_and(|device| device.presence == Presence::Poll);
    let active = snapshot.active_role.as_deref() == Some(role.role.as_str());
    let mut sub = Vec::new();
    if let Some(la) = keys.la {
        sub.push(format!("LA {la}"));
    }
    if let Some(pa) = &keys.physical {
        sub.push(format!("PA {pa}"));
    }
    sub.push(format!("receiver input \"{}\"", role.source));
    if let Some(probe) = device.and_then(|device| device.probe.as_ref()) {
        sub.push(format!("probe at {}", probe_host(probe)));
    }

    RolePage {
        name: role.name.clone(),
        role: role.role.clone(),
        identity: view::role_identity(role),
        on: role.power == RolePower::On,
        active,
        sub: sub.join(" · "),
        shadow,
        warming: snapshot.warming,
        la: role.logical_address,
        show_takeover: !active,
        show_standby: active || role.power == RolePower::On,
        disabled: if role.logical_address.is_none() {
            Some("not verified yet")
        } else if snapshot.warming {
            Some("warming: actions are refused until the bus model settles")
        } else {
            None
        },
        restored: restored.then(|| restored_view(snapshot, role, waiting, started)),
        sources,
        agreement,
        sources_note,
        keys: keys_rows,
        confirmed,
        rule: device.map(|device| dv::match_rule(&device.matches)),
        events: dv::identity_events(snapshot, role, entry),
        measures: dv::role_measures(
            &dv::role_stats(&snapshot.decisions, &role.role, polled),
            &role.source,
        ),
        quirks: match (device, config) {
            (Some(device), Some(config)) => dv::quirk_sentences(device, config),
            _ => Vec::new(),
        },
        involved: dv::involved(snapshot, &role.role, dv::INVOLVED_ROWS),
    }
}

pub(crate) async fn role(
    State(web): State<WebState>,
    ctx: Ctx,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    let role = snapshot
        .roles
        .iter()
        .find(|role| role.role == id)
        .ok_or(WebError::NotFound)?;
    let page = role_page(&snapshot, role, config(&web).as_ref());
    render(&DevicePage {
        layout: Some(Layout::new(ctx, "devices", &snapshot)),
        title: page.name.clone(),
        body: Body::Role(Box::new(page)),
    })
}

struct OutputPage {
    kind: OutputKind,
    chip: (&'static str, String),
    sub: String,
    sources: Vec<SourceCol>,
    agreement: Option<Agreement>,
    agreement_note: &'static str,
    explain: Option<&'static str>,
    /// Further Home Assistant attributes: receiver source and sound mode, speaker volume.
    attributes: Vec<(&'static str, String)>,
    /// The receiver's CEC entry, as rows; `Some(empty)` when none has talked.
    cec: Option<Vec<(&'static str, String)>>,
    /// The receiver's audio-system address, for Identify and Forget on its CEC entry.
    cec_la: Option<u8>,
    warming: bool,
    measures: Vec<Measure>,
    drifts: Option<Vec<DriftPair>>,
    drift_grace_s: Option<u64>,
    drive: Vec<Sentence>,
}

impl OutputPage {
    fn name(&self) -> &'static str {
        self.kind.name()
    }
}

fn power_chip(on: Option<bool>) -> (&'static str, String) {
    match on {
        Some(true) => ("ok", "on".to_owned()),
        Some(false) => ("dim", "off".to_owned()),
        None => ("none", "unknown".to_owned()),
    }
}

fn ha_col(entity: &crate::state::HaEntityView) -> SourceCol {
    SourceCol::new(
        "Home Assistant",
        entity.state.clone(),
        vec![entity.entity.clone(), view::ago(&entity.last_changed)],
    )
}

fn tv_page(snapshot: &Snapshot, config: Option<&Config>) -> OutputPage {
    let shadow = snapshot.mode == Mode::Shadow;
    let webos = config.and_then(|config| match &config.tv.off {
        TvOffConfig::Webos { host, .. } => Some(host.clone()),
        TvOffConfig::Ha => None,
    });
    let ha = &snapshot.ha.tv;
    let mut direct = match (&webos, snapshot.tv_direct) {
        (None, _) => SourceCol::absent(
            "webOS · direct",
            vec!["not configured (tv.off = ha)".to_owned()],
        ),
        (Some(_), _) if shadow => {
            SourceCol::absent("webOS · direct", vec!["not sampled in shadow".to_owned()])
        }
        (Some(_), Some((state, at))) => {
            SourceCol::new("webOS · direct", state.to_string(), vec![view::since(&at)])
        }
        (Some(_), None) => SourceCol::absent("webOS · direct", vec!["not sampled yet".to_owned()]),
    };
    direct.used = !direct.absent;
    let mut ha_column = ha_col(ha);
    ha_column.used = !direct.used;
    let direct_reading = snapshot
        .tv_direct
        .filter(|_| !direct.absent)
        .and_then(|(state, _)| dv::tv_direct_reading(state));
    let readings = [direct_reading, dv::tv_ha_reading(&ha.state)];
    let mut sources = vec![direct, ha_column];
    mark_disagreeing(&mut sources, &readings);
    let reconnecting =
        direct_reading == Some(true) && matches!(ha.state.as_str(), "unavailable" | "unknown");
    let mut sub = Vec::new();
    if let Some(host) = &webos {
        sub.push(format!("webOS at {host}"));
    }
    sub.push(format!(
        "not on the CEC bus (the bridge speaks as LA {})",
        snapshot.bridge_logical_address
    ));
    sub.push(format!("Home Assistant {}", ha.entity));
    OutputPage {
        kind: OutputKind::Tv,
        chip: power_chip(dv::tv_power(snapshot).0),
        sub: sub.join(" · "),
        sources,
        agreement: Some(dv::agreement(&readings)),
        agreement_note: "direct wins; Home Assistant is the fallback",
        explain: reconnecting.then_some(
            "Home Assistant reconnects to the TV about 30 s after a wake. Expected; it is not drift because the direct source agrees with the intent.",
        ),
        attributes: Vec::new(),
        cec: None,
        cec_la: None,
        warming: snapshot.warming,
        measures: dv::output_measures(snapshot, OutputKind::Tv),
        drifts: Some(dv::drift_history(snapshot, "tv", Utc::now())),
        drift_grace_s: config.map(|config| config.verify.drift_grace_s),
        drive: config.map_or_else(Vec::new, |config| dv::drive_sentences(OutputKind::Tv, config)),
    }
}

fn receiver_page(snapshot: &Snapshot, config: Option<&Config>) -> OutputPage {
    let shadow = snapshot.mode == Mode::Shadow;
    let denon = config.and_then(|config| {
        config
            .receiver
            .observe
            .as_ref()
            .map(|ReceiverObserveConfig::Denon { host, .. }| host.clone())
    });
    let ha = &snapshot.ha.receiver;
    let mut direct = match (&denon, snapshot.receiver_direct) {
        (None, _) => SourceCol::absent(
            "Denon · direct",
            vec!["not configured (no receiver.observe)".to_owned()],
        ),
        (Some(_), _) if shadow => {
            SourceCol::absent("Denon · direct", vec!["not sampled in shadow".to_owned()])
        }
        (Some(_), Some((state, at))) => {
            SourceCol::new("Denon · direct", state.to_string(), vec![view::since(&at)])
        }
        (Some(_), None) => SourceCol::absent("Denon · direct", vec!["not sampled yet".to_owned()]),
    };
    let direct_reading = snapshot
        .receiver_direct
        .filter(|_| !direct.absent)
        .and_then(|(state, _)| dv::receiver_direct_reading(state));
    direct.used = direct_reading.is_some();
    let mut ha_column = ha_col(ha);
    ha_column.used = !direct.used;
    let readings = [direct_reading, view::ha_power(&ha.state)];
    let mut sources = vec![direct, ha_column];
    mark_disagreeing(&mut sources, &readings);
    let attributes = [("source", "source"), ("sound mode", "sound_mode")]
        .into_iter()
        .filter_map(|(key, attribute)| {
            view::ha_attribute(ha, attribute).map(|value| (key, format!("{value} · ha")))
        })
        .collect();
    let audio = dv::audio_entry(snapshot);
    let cec = audio.map_or_else(Vec::new, |entry| {
        vec![
            (
                "Logical",
                format!(
                    "{} · {}",
                    entry.logical_address,
                    dv::la_label(entry.logical_address)
                ),
            ),
            ("Physical", view::opt(&entry.physical_address)),
            ("Vendor", view::opt(&entry.vendor)),
            ("OSD name", view::opt(&entry.osd)),
            ("Power", view::bus_power(&entry.power).to_owned()),
            ("Last seen", view::ago(&entry.last_seen)),
        ]
    });
    let mut sub = Vec::new();
    if let Some(host) = &denon {
        sub.push(format!("Denon at {host}"));
    }
    if let Some(entry) = audio {
        sub.push(format!("CEC LA {}", entry.logical_address));
    }
    sub.push(format!("Home Assistant {}", ha.entity));
    OutputPage {
        kind: OutputKind::Receiver,
        chip: power_chip(dv::receiver_power(snapshot).0),
        sub: sub.join(" · "),
        sources,
        agreement: Some(dv::agreement(&readings)),
        agreement_note: "direct wins; Home Assistant is the fallback",
        explain: None,
        attributes,
        cec: Some(cec),
        cec_la: audio.map(|entry| entry.logical_address),
        warming: snapshot.warming,
        measures: dv::output_measures(snapshot, OutputKind::Receiver),
        drifts: Some(dv::drift_history(snapshot, "receiver", Utc::now())),
        drift_grace_s: config.map(|config| config.verify.drift_grace_s),
        drive: config.map_or_else(Vec::new, |config| {
            dv::drive_sentences(OutputKind::Receiver, config)
        }),
    }
}

fn speakers_page(snapshot: &Snapshot, config: Option<&Config>) -> OutputPage {
    let ha = &snapshot.ha.sonos;
    let mut column = ha_col(ha);
    column.used = true;
    let mut attributes = Vec::new();
    if let Some(level) = ha
        .attributes
        .get("volume_level")
        .and_then(serde_json::Value::as_f64)
    {
        attributes.push(("volume", format!("{:.0}", level * 100.0)));
    }
    if let Some(muted) = ha
        .attributes
        .get("is_volume_muted")
        .and_then(serde_json::Value::as_bool)
    {
        attributes.push(("mute", if muted { "muted" } else { "not muted" }.to_owned()));
    }
    OutputPage {
        kind: OutputKind::Speakers,
        chip: (
            if view::ha_power(&ha.state) == Some(true) {
                "ok"
            } else {
                "dim"
            },
            ha.state.clone(),
        ),
        sub: format!("Home Assistant {} · no direct probe", ha.entity),
        sources: vec![column],
        agreement: None,
        agreement_note: "Home Assistant is the only source",
        explain: None,
        attributes,
        cec: None,
        cec_la: None,
        warming: snapshot.warming,
        measures: dv::output_measures(snapshot, OutputKind::Speakers),
        drifts: None,
        drift_grace_s: None,
        drive: config.map_or_else(Vec::new, |config| {
            dv::drive_sentences(OutputKind::Speakers, config)
        }),
    }
}

/// The bridge: its drawer's status block, as a page.
struct BridgePage {
    snapshot: Snapshot,
}

pub(crate) async fn output(
    State(web): State<WebState>,
    ctx: Ctx,
    Path(name): Path<String>,
) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    let config = config(&web);
    let layout = Some(Layout::new(ctx, "devices", &snapshot));
    let (title, body) = if name == "bridge" {
        (
            "Bridge".to_owned(),
            Body::Bridge(Box::new(BridgePage { snapshot })),
        )
    } else {
        let kind = OutputKind::from_path(&name).ok_or(WebError::NotFound)?;
        let page = match kind {
            OutputKind::Tv => tv_page(&snapshot, config.as_ref()),
            OutputKind::Receiver => receiver_page(&snapshot, config.as_ref()),
            OutputKind::Speakers => speakers_page(&snapshot, config.as_ref()),
        };
        (kind.name().to_owned(), Body::Output(Box::new(page)))
    };
    render(&DevicePage {
        layout,
        title,
        body,
    })
}

struct EntryPage {
    la: u8,
    label: &'static str,
    identity: (&'static str, &'static str),
    rows: Vec<(&'static str, String)>,
    frames: Vec<FrameView>,
    warming: bool,
}

/// An unbound bus entry. Addresses with a home elsewhere redirect there: the bridge, a bound
/// role, the role a restored entry waits for, the receiver's audio-system entry.
pub(crate) async fn entry(
    State(web): State<WebState>,
    ctx: Ctx,
    Path(la): Path<String>,
) -> Result<Response, WebError> {
    let la: u8 = la.parse().map_err(|_| WebError::NotFound)?;
    let snapshot = web.app.snapshot();
    let entry = snapshot
        .bus
        .iter()
        .find(|entry| entry.logical_address == la)
        .ok_or(WebError::NotFound)?;
    if la == snapshot.bridge_logical_address {
        return Ok(Redirect::to("/devices/out/bridge").into_response());
    }
    if let Some(role) = &entry.matched_role {
        return Ok(Redirect::to(&format!("/devices/{}", dv::path_segment(role))).into_response());
    }
    let config = config(&web);
    if let Some(role) = config.as_ref().and_then(|config| {
        snapshot
            .roles
            .iter()
            .filter(|role| view::is_restored(role))
            .find(|role| {
                config.device.iter().any(|device| {
                    device.role == role.role
                        && dv::restored_entry(&snapshot, device)
                            .is_some_and(|waiting| waiting.logical_address == la)
                })
            })
    }) {
        return Ok(
            Redirect::to(&format!("/devices/{}", dv::path_segment(&role.role))).into_response(),
        );
    }
    if dv::audio_entry(&snapshot).is_some_and(|audio| audio.logical_address == la) {
        return Ok(Redirect::to("/devices/out/receiver").into_response());
    }

    let now = Utc::now();
    let page = EntryPage {
        la,
        label: dv::la_label(la),
        identity: dv::entry_identity(&snapshot, entry),
        rows: vec![
            ("Physical", view::opt(&entry.physical_address)),
            ("Device type", view::device_type(&entry.device_type)),
            ("Vendor", view::opt(&entry.vendor)),
            (
                "OSD name",
                entry
                    .osd
                    .as_deref()
                    .map_or_else(|| "—".to_owned(), |osd| format!("\"{osd}\"")),
            ),
            ("Power", view::bus_power(&entry.power).to_owned()),
            (
                "Last seen",
                if entry.verified {
                    view::ago(&entry.last_seen)
                } else {
                    "before restart (restored from the state file)".to_owned()
                },
            ),
            (
                "Confirmed",
                entry
                    .confirmed_at
                    .as_ref()
                    .map_or_else(|| "never".to_owned(), view::when_ms),
            ),
            (
                "Identification",
                if entry.identified {
                    "complete: every key the match rules use has been reported".to_owned()
                } else {
                    format!(
                        "incomplete · {} (backing off; at most one query per second on the bus)",
                        dv::next_query(entry, now)
                    )
                },
            ),
        ],
        frames: snapshot
            .recent_frames
            .iter()
            .rev()
            .filter(|frame| frame.from == la || frame.to == la)
            .take(ENTRY_FRAMES)
            .cloned()
            .collect(),
        warming: snapshot.warming,
    };
    Ok(render(&DevicePage {
        layout: Some(Layout::new(ctx, "devices", &snapshot)),
        title: format!("LA {la}"),
        body: Body::Entry(Box::new(page)),
    })?
    .into_response())
}
