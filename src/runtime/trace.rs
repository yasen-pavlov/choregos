//! Shared replay codec and best-effort, append-only runtime capture.
use crate::{
    cec::{Frame, bus::LearnedIdentity},
    state::{Command, ConsoleState, DirectPower},
};
use anyhow::Context;
use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc, watch},
};

#[derive(Clone, Debug)]
pub enum TraceEntry {
    Received {
        bridge: String,
        frame: Frame,
    },
    Lwt {
        bridge: String,
        online: bool,
    },
    Sent {
        bridge: String,
        to: u8,
        data: Vec<u8>,
        id: Option<String>,
    },
    Result {
        bridge: String,
        payload: Value,
    },
    AetherStatus {
        node: String,
        payload: Value,
    },
    AetherBus {
        node: String,
        payload: Value,
    },
    HaState {
        entity: String,
        state: String,
        attrs: Value,
        changed: Option<DateTime<Utc>>,
        updated: Option<DateTime<Utc>>,
    },
    TvPower {
        state: DirectPower,
        at: DateTime<Utc>,
    },
    TvDirectUnavailable,
    ReceiverPower {
        state: DirectPower,
        at: DateTime<Utc>,
        source_code: Option<String>,
    },
    ConsolePower {
        role: String,
        state: ConsoleState,
        at: DateTime<Utc>,
    },
    Command(Command),
    /// Full decision row with structured actions (not UI display strings).
    Decision(Value),
    Verdict(Value),
    Identities(Vec<LearnedIdentity>),
    Connection {
        mqtt: bool,
    },
}

#[derive(Deserialize, Serialize)]
struct Envelope {
    ts: DateTime<Utc>,
    topic: String,
    payload: Value,
}

pub fn line(ts: DateTime<Utc>, entry: &TraceEntry) -> String {
    let (topic, payload) = match entry {
        TraceEntry::Received { bridge, frame } => (
            format!("tele/{bridge}/SENSOR"),
            json!({"HdmiReceived":{"From":frame.from,"To":frame.to,"Data":frame.data()}}),
        ),
        TraceEntry::Lwt { bridge, online } => (
            format!("tele/{bridge}/LWT"),
            json!(if *online { "Online" } else { "Offline" }),
        ),
        TraceEntry::Sent {
            bridge,
            to,
            data,
            id,
        } => match id {
            Some(id) => (
                format!("aether/{bridge}/tx"),
                json!({"id":id,"to":to,"data":hex::encode_upper(data)}),
            ),
            None => (
                format!("cmnd/{bridge}/HdmiSend"),
                json!({"To":to,"Data":hex::encode_upper(data)}),
            ),
        },
        TraceEntry::Result { bridge, payload } => {
            (format!("stat/{bridge}/RESULT"), payload.clone())
        }
        TraceEntry::AetherStatus { node, payload } => {
            (format!("aether/{node}/status"), payload.clone())
        }
        TraceEntry::AetherBus { node, payload } => (format!("aether/{node}/bus"), payload.clone()),
        TraceEntry::HaState {
            entity,
            state,
            attrs,
            changed,
            updated,
        } => (
            "ha/state".into(),
            json!({"entity":entity,"state":state,"attrs":attrs,"changed":changed,"updated":updated}),
        ),
        TraceEntry::TvPower { state, at } => ("tv/state".into(), json!({"state":state,"at":at})),
        TraceEntry::TvDirectUnavailable => ("tv/unavailable".into(), json!({})),
        TraceEntry::ReceiverPower {
            state,
            at,
            source_code,
        } => (
            "receiver/state".into(),
            json!({"state":state,"at":at,"source_code":source_code}),
        ),
        TraceEntry::ConsolePower { role, state, at } => (
            "probe/state".into(),
            json!({"role":role,"state":state,"at":at}),
        ),
        TraceEntry::Command(command) => ("choregos/command".into(), json!(command)),
        TraceEntry::Decision(payload) => ("choregos/decision".into(), payload.clone()),
        TraceEntry::Verdict(payload) => ("choregos/verdict".into(), payload.clone()),
        TraceEntry::Identities(identities) => ("choregos/identities".into(), json!(identities)),
        TraceEntry::Connection { mqtt } => ("choregos/connection".into(), json!({"mqtt":mqtt})),
    };
    let mut text = json!({"ts":ts.to_rfc3339_opts(SecondsFormat::Millis, true),"topic":topic,"payload":payload}).to_string();
    text.push('\n');
    text
}

pub fn parse(line: &str) -> Option<(DateTime<Utc>, TraceEntry)> {
    parse_checked(line).ok().flatten()
}

pub fn parse_checked(line: &str) -> anyhow::Result<Option<(DateTime<Utc>, TraceEntry)>> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    let Envelope { ts, topic, payload } =
        serde_json::from_str(line).context("invalid trace envelope")?;
    let known = known_topic(&topic);
    let entry = parse_entry(&topic, payload, ts);
    anyhow::ensure!(
        !known || entry.is_some(),
        "invalid trace payload for {topic}"
    );
    Ok(entry.map(|entry| (ts, entry)))
}

fn known_topic(topic: &str) -> bool {
    if matches!(
        topic,
        "choregos/identities"
            | "choregos/command"
            | "choregos/connection"
            | "choregos/decision"
            | "choregos/verdict"
            | "ha/state"
            | "tv/state"
            | "tv/unavailable"
            | "receiver/state"
            | "probe/state"
    ) {
        return true;
    }
    let mut parts = topic.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some("tele"), Some(_), Some("SENSOR" | "LWT"), None)
            | (Some("cmnd"), Some(_), Some("HdmiSend"), None)
            | (Some("stat"), Some(_), Some("RESULT"), None)
            | (Some("aether"), Some(_), Some("status" | "bus" | "tx"), None)
    )
}

fn parse_entry(topic: &str, payload: Value, ts: DateTime<Utc>) -> Option<TraceEntry> {
    Some(match topic {
        "choregos/identities" => TraceEntry::Identities(serde_json::from_value(payload).ok()?),
        "choregos/command" => TraceEntry::Command(serde_json::from_value(payload).ok()?),
        "choregos/decision" => TraceEntry::Decision(payload),
        "choregos/verdict" => TraceEntry::Verdict(payload),
        "choregos/connection" => TraceEntry::Connection {
            mqtt: payload.get("mqtt")?.as_bool()?,
        },
        "ha/state" => TraceEntry::HaState {
            entity: payload.get("entity")?.as_str()?.into(),
            state: payload.get("state")?.as_str()?.into(),
            attrs: payload.get("attrs")?.clone(),
            changed: serde_json::from_value(payload.get("changed")?.clone()).ok()?,
            updated: match payload.get("updated") {
                Some(value) => serde_json::from_value::<Option<DateTime<Utc>>>(value.clone())
                    .ok()?
                    .or(Some(ts)),
                None => Some(ts),
            },
        },
        "tv/state" => TraceEntry::TvPower {
            state: serde_json::from_value(payload.get("state")?.clone()).ok()?,
            at: serde_json::from_value(payload.get("at")?.clone()).ok()?,
        },
        "tv/unavailable" if payload.is_object() => TraceEntry::TvDirectUnavailable,
        "receiver/state" => TraceEntry::ReceiverPower {
            state: serde_json::from_value(payload.get("state")?.clone()).ok()?,
            at: serde_json::from_value(payload.get("at")?.clone()).ok()?,
            source_code: payload
                .get("source_code")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "probe/state" => TraceEntry::ConsolePower {
            role: payload.get("role")?.as_str()?.into(),
            state: serde_json::from_value(payload.get("state")?.clone()).ok()?,
            at: serde_json::from_value(payload.get("at")?.clone()).ok()?,
        },
        _ => parse_mqtt(topic, payload)?,
    })
}

fn parse_mqtt(topic: &str, payload: Value) -> Option<TraceEntry> {
    let mut parts = topic.split('/');
    let prefix = parts.next()?;
    let bridge = parts.next()?.to_owned();
    let suffix = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some(match (prefix, suffix) {
        ("tele", "SENSOR") => TraceEntry::Received {
            bridge,
            frame: Frame::parse(&serde_json::to_vec(&payload).ok()?).ok()?,
        },
        ("tele", "LWT") => TraceEntry::Lwt {
            bridge,
            online: match payload.as_str()? {
                "Online" => true,
                "Offline" => false,
                _ => return None,
            },
        },
        ("cmnd", "HdmiSend") => TraceEntry::Sent {
            bridge,
            to: u8::try_from(payload.get("To")?.as_u64()?).ok()?,
            data: hex::decode(payload.get("Data")?.as_str()?).ok()?,
            id: None,
        },
        ("stat", "RESULT") if payload.get("HdmiSend").is_some() => {
            TraceEntry::Result { bridge, payload }
        }
        ("aether", "status") if payload.is_object() => TraceEntry::AetherStatus {
            node: bridge,
            payload,
        },
        ("aether", "bus") if payload.is_object() => TraceEntry::AetherBus {
            node: bridge,
            payload,
        },
        ("aether", "tx") => TraceEntry::Sent {
            bridge,
            to: u8::try_from(payload.get("to")?.as_u64()?).ok()?,
            data: hex::decode(payload.get("data")?.as_str()?).ok()?,
            id: Some(payload.get("id")?.as_str()?.into()),
        },
        _ => return None,
    })
}

/// The day file the writer appends `date`'s records to.
pub fn day_file(directory: &Path, date: NaiveDate) -> PathBuf {
    directory.join(format!("cec-bus.{date}.jsonl"))
}

/// The bounds of one window read: the entries it returns, the longest line it buffers, and
/// the bytes and lines it scans in total (pre-window, torn and unknown lines included).
#[derive(Clone, Copy, Debug)]
pub struct ReadBudget {
    pub entries: usize,
    pub line_bytes: usize,
    pub scan_bytes: u64,
    pub scan_lines: u64,
}

impl ReadBudget {
    /// The bus window's budget: 5000 entries, 1 MiB lines, 64 MiB or 2 M lines scanned.
    pub const WINDOW: Self = Self {
        entries: 5000,
        line_bytes: 1 << 20,
        scan_bytes: 64 << 20,
        scan_lines: 2_000_000,
    };
}

/// What a window read returned, and whether it stopped at its budget with input left
/// (more in-window entries than `entries`, or the scan budget spent before the window end).
#[derive(Debug, Default)]
pub struct WindowRead {
    pub entries: Vec<(DateTime<Utc>, TraceEntry)>,
    pub truncated: bool,
}

/// Reads one line of at most `max` bytes (newline included) into `line`, consuming at most
/// `allowance` bytes. Returns the bytes consumed and whether the line is whole; a longer
/// line is consumed to its end (within the allowance) without being kept. `None` at EOF.
fn bounded_line(
    reader: &mut impl std::io::BufRead,
    line: &mut Vec<u8>,
    max: usize,
    allowance: u64,
) -> std::io::Result<Option<(u64, bool)>> {
    line.clear();
    let mut consumed = 0u64;
    let mut whole = true;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((consumed > 0).then_some((consumed, whole)));
        }
        let room = usize::try_from(allowance - consumed).unwrap_or(usize::MAX);
        if room == 0 {
            return Ok(Some((consumed, false)));
        }
        let chunk = &available[..available.len().min(room)];
        let (chunk, done) = match chunk.iter().position(|byte| *byte == b'\n') {
            Some(end) => (&chunk[..=end], true),
            None => (chunk, false),
        };
        if whole && line.len() + chunk.len() <= max {
            line.extend_from_slice(chunk);
        } else {
            whole = false;
            line.clear();
        }
        let length = chunk.len();
        reader.consume(length);
        consumed += length as u64;
        if done {
            return Ok(Some((consumed, whole)));
        }
    }
}

/// Records with `from ≤ ts ≤ to` from the UTC day files the window spans, in file order and
/// at most `budget.entries` of them. Each file is scanned line by line: lines before `from`
/// are skipped on their timestamp alone, the scan of a file stops at the first line after
/// `to`, torn, unknown and overlong lines are ignored (an overlong one is never buffered
/// whole). A missing day file contributes nothing. Reading stops, flagged `truncated`, at
/// one entry over the limit or when the scan budget is spent with input left.
pub fn read_window(
    directory: &Path,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    budget: ReadBudget,
) -> std::io::Result<WindowRead> {
    let mut scan = WindowScan {
        budget,
        from,
        to,
        bytes: 0,
        lines: 0,
        line: Vec::new(),
        read: WindowRead::default(),
    };
    let mut date = from.date_naive();
    while date <= to.date_naive() {
        match std::fs::File::open(day_file(directory, date)) {
            Ok(file) => {
                if !scan.file(&mut std::io::BufReader::new(file))? {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let Some(next) = date.succ_opt() else {
            break;
        };
        date = next;
    }
    Ok(scan.read)
}

/// What one whole trace line means for a window read.
enum WindowLine {
    /// Torn, unknown, or before the window.
    Skip,
    /// After the window: the rest of the file is later still.
    After,
    Entry((DateTime<Utc>, TraceEntry)),
}

/// Lines before `from` are judged on their timestamp alone, without a full parse.
fn window_line(line: &[u8], from: DateTime<Utc>, to: DateTime<Utc>) -> WindowLine {
    #[derive(Deserialize)]
    struct Stamp {
        ts: DateTime<Utc>,
    }
    let Ok(text) = std::str::from_utf8(line) else {
        return WindowLine::Skip;
    };
    let Ok(Stamp { ts }) = serde_json::from_str(text) else {
        return WindowLine::Skip;
    };
    if ts > to {
        WindowLine::After
    } else if ts >= from
        && let Some(entry) = parse(text)
    {
        WindowLine::Entry(entry)
    } else {
        WindowLine::Skip
    }
}

/// The running state of one window read across its day files.
struct WindowScan {
    budget: ReadBudget,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    bytes: u64,
    lines: u64,
    line: Vec<u8>,
    read: WindowRead,
}

impl WindowScan {
    /// Scans one day file to its end, its first line after the window, or the budget.
    /// `false` when the whole read stops, flagged `truncated`.
    fn file(&mut self, reader: &mut impl std::io::BufRead) -> std::io::Result<bool> {
        loop {
            let allowance = self.budget.scan_bytes.saturating_sub(self.bytes);
            if self.lines >= self.budget.scan_lines || allowance == 0 {
                self.read.truncated = !reader.fill_buf()?.is_empty();
                return Ok(!self.read.truncated);
            }
            let Some((consumed, whole)) =
                bounded_line(reader, &mut self.line, self.budget.line_bytes, allowance)?
            else {
                return Ok(true);
            };
            self.bytes += consumed;
            self.lines += 1;
            if !whole {
                continue;
            }
            match window_line(&self.line, self.from, self.to) {
                WindowLine::Skip => {}
                WindowLine::After => return Ok(true),
                WindowLine::Entry(entry) => {
                    if self.read.entries.len() == self.budget.entries {
                        self.read.truncated = true;
                        return Ok(false);
                    }
                    self.read.entries.push(entry);
                }
            }
        }
    }
}

#[derive(Clone)]
struct Controls {
    keep_days: u32,
    at: DateTime<Utc>,
    identities: Arc<Vec<LearnedIdentity>>,
}

pub(super) struct TraceWriter {
    tx: mpsc::Sender<(DateTime<Utc>, TraceEntry)>,
    controls: watch::Sender<Controls>,
    task: tokio::task::JoinHandle<()>,
    dropped: AtomicBool,
}

impl TraceWriter {
    pub fn start(
        directory: PathBuf,
        keep_days: u32,
        ts: DateTime<Utc>,
        identities: Vec<LearnedIdentity>,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel(1024);
        let (controls, mut latest) = watch::channel(Controls {
            keep_days,
            at: ts,
            identities: Arc::new(identities),
        });
        let task = tokio::spawn(async move {
            let initial = latest.borrow_and_update().clone();
            let mut writer = Files {
                directory,
                keep_days: initial.keep_days,
                identities: initial.identities,
                open: None,
                prune_failed: false,
            };
            let mut failed = false;
            report(writer.prepare(initial.at).await, &mut failed);
            while let Some((ts, entry)) = rx.recv().await {
                {
                    let controls = latest.borrow_and_update();
                    if writer.keep_days != controls.keep_days {
                        writer.keep_days = controls.keep_days;
                        writer.open = None;
                    }
                    if !Arc::ptr_eq(&writer.identities, &controls.identities) {
                        writer.identities = controls.identities.clone();
                    }
                }
                report(writer.record(ts, &entry).await, &mut failed);
            }
        });
        Self {
            tx,
            controls,
            task,
            dropped: AtomicBool::new(false),
        }
    }
    pub fn record(&self, ts: DateTime<Utc>, entry: TraceEntry) {
        if self.controls.borrow().keep_days == 0 {
            return;
        }
        if self.tx.try_send((ts, entry)).is_ok() {
            self.dropped.store(false, Ordering::Relaxed);
        } else if !self.dropped.swap(true, Ordering::Relaxed) {
            tracing::warn!("dropping bus trace records: writer queue full or stopped");
        }
    }
    pub fn identities(&self, identities: Vec<LearnedIdentity>) {
        self.controls
            .send_modify(|controls| controls.identities = Arc::new(identities));
    }
    pub fn configure(&self, ts: DateTime<Utc>, days: u32) {
        self.controls.send_modify(|controls| {
            controls.at = ts;
            controls.keep_days = days;
        });
    }
    pub async fn flush(self) {
        drop(self.tx);
        if let Err(error) = self.task.await {
            tracing::warn!(%error, "trace writer stopped unexpectedly");
        }
    }
}
fn report(result: std::io::Result<()>, failed: &mut bool) {
    match result {
        Ok(()) => {
            *failed = false;
        }
        Err(error) => {
            if !*failed {
                tracing::warn!(%error, "cannot write bus trace; continuing without capture");
            }
            *failed = true;
        }
    }
}

struct Files {
    directory: PathBuf,
    keep_days: u32,
    identities: Arc<Vec<LearnedIdentity>>,
    open: Option<(NaiveDate, tokio::fs::File)>,
    prune_failed: bool,
}
impl Files {
    async fn prepare(&mut self, ts: DateTime<Utc>) -> std::io::Result<()> {
        if self.keep_days == 0 {
            return Ok(());
        }
        let date = ts.date_naive();
        if self
            .open
            .as_ref()
            .is_some_and(|(current, _)| *current == date)
        {
            return Ok(());
        }
        self.open = None;
        let pruned = prune(&self.directory, date, self.keep_days).await;
        self.open_after_prune(ts, pruned).await
    }

    async fn open_after_prune(
        &mut self,
        ts: DateTime<Utc>,
        pruned: std::io::Result<()>,
    ) -> std::io::Result<()> {
        let date = ts.date_naive();
        match pruned {
            Ok(()) => self.prune_failed = false,
            Err(error) => {
                if !self.prune_failed {
                    tracing::warn!(%error, "cannot prune bus traces; capture continues");
                }
                self.prune_failed = true;
            }
        }
        let path = self.directory.join(format!("cec-bus.{date}.jsonl"));
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        if file.metadata().await?.len() != 0 {
            file.write_all(b"\n").await?;
        }
        file.write_all(
            line(
                ts,
                &TraceEntry::Identities(self.identities.as_ref().clone()),
            )
            .as_bytes(),
        )
        .await?;
        file.flush().await?;
        self.open = Some((date, file));
        Ok(())
    }
    async fn record(&mut self, ts: DateTime<Utc>, entry: &TraceEntry) -> std::io::Result<()> {
        self.prepare(ts).await?;
        if let Some((_, file)) = &mut self.open {
            let result = async {
                file.write_all(line(ts, entry).as_bytes()).await?;
                file.flush().await
            }
            .await;
            if result.is_err() {
                self.open = None;
            }
            result
        } else {
            Ok(())
        }
    }
}
async fn prune(directory: &Path, date: NaiveDate, keep_days: u32) -> std::io::Result<()> {
    let cutoff = date - chrono::Duration::days(i64::from(keep_days));
    let mut entries = tokio::fs::read_dir(directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stamp) = name
            .strip_prefix("cec-bus.")
            .and_then(|s| s.strip_suffix(".jsonl"))
        else {
            continue;
        };
        if let Ok(day) = NaiveDate::parse_from_str(stamp, "%Y-%m-%d")
            && stamp == day.to_string()
            && day < cutoff
            && entry.file_type().await?.is_file()
        {
            tokio::fs::remove_file(entry.path()).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Mode;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }
    fn commands() -> Vec<Command> {
        vec![
            Command::AllOff,
            Command::SetMode(Mode::Active),
            Command::SetMode(Mode::Shadow),
            Command::Identify(4),
            Command::Forget(8),
            Command::SendRaw {
                to: 4,
                data: vec![0x36],
            },
            Command::Takeover("streamer".into()),
            Command::Standby("streamer".into()),
        ]
    }
    #[test]
    fn every_trace_kind_and_command_variant_round_trips() {
        let ts = at("2026-10-30T12:00:00.123Z");
        let mut entries = vec![
            TraceEntry::Connection { mqtt: true },
            TraceEntry::Connection { mqtt: false },
            TraceEntry::Received {
                bridge: "tasmota_cec".into(),
                frame: Frame::from_hex(4, 0, "04").unwrap(),
            },
            TraceEntry::Received {
                bridge: "tasmota_cec".into(),
                frame: Frame::from_hex(4, 0, "").unwrap(),
            },
            TraceEntry::Lwt {
                bridge: "tasmota_cec".into(),
                online: true,
            },
            TraceEntry::Lwt {
                bridge: "tasmota_cec".into(),
                online: false,
            },
            TraceEntry::Sent {
                bridge: "tasmota_cec".into(),
                to: 4,
                data: vec![0x36],
                id: None,
            },
            TraceEntry::Result {
                bridge: "tasmota_cec".into(),
                payload: json!({"HdmiSend":{"Ack":false}}),
            },
            TraceEntry::Result {
                bridge: "tasmota_cec".into(),
                payload: json!({"HdmiSend":"Done"}),
            },
            TraceEntry::HaState {
                entity: "media_player.tv".into(),
                state: "on".into(),
                attrs: json!({"volume_level":0.5}),
                changed: Some(ts),
                updated: Some(ts),
            },
            TraceEntry::HaState {
                entity: "media_player.receiver".into(),
                state: "unknown".into(),
                attrs: json!({}),
                changed: None,
                updated: Some(ts),
            },
            TraceEntry::Identities(vec![LearnedIdentity {
                la: 4,
                physical: Some(0x2100),
                vendor: Some(0x0010fa),
                osd: Some("Streamer".into()),
                device_type: Some(4),
                confirmed_at: ts,
            }]),
        ];
        entries.extend(commands().into_iter().map(TraceEntry::Command));
        for entry in entries {
            let encoded = line(ts, &entry);
            let (decoded_ts, decoded) = parse(&encoded).unwrap();
            assert_eq!(decoded_ts, ts);
            assert_eq!(encoded, line(decoded_ts, &decoded));
            assert!(encoded.contains(".123Z"));
        }
        assert!(
            parse(r#"{"ts":"2026-10-30T12:00:00Z","topic":"future/event","payload":{}}"#).is_none()
        );
    }

    #[test]
    fn old_ha_attribute_observations_fall_back_to_the_trace_timestamp() {
        let ts = at("2026-10-30T12:00:05Z");
        let changed = at("2026-10-30T12:00:00Z");
        for updated in [None, Some(json!("2026-10-30T12:00:04Z"))] {
            let mut payload = json!({"entity":"media_player.receiver","state":"on","attrs":{"source":"Console B"},"changed":changed});
            if let Some(updated) = &updated {
                payload["updated"] = updated.clone();
            }
            let text = json!({"ts":ts,"topic":"ha/state","payload":payload}).to_string();
            let (
                _,
                TraceEntry::HaState {
                    changed: actual_changed,
                    updated: actual_updated,
                    ..
                },
            ) = parse(&text).unwrap()
            else {
                panic!("HA state expected");
            };
            assert_eq!(actual_changed, Some(changed));
            assert_eq!(
                actual_updated,
                Some(if updated.is_some() {
                    at("2026-10-30T12:00:04Z")
                } else {
                    ts
                })
            );
        }
    }

    #[tokio::test]
    async fn writer_rolls_over_at_utc_midnight_and_prunes_only_old_trace_files() {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "cec-bus.2026-10-01.jsonl",
            "cec-bus.2026-10-28.jsonl",
            "cec-bus.invalid.jsonl",
            "other.2026-10-01.jsonl",
        ] {
            std::fs::write(directory.path().join(name), "").unwrap();
        }
        std::fs::create_dir(directory.path().join("cec-bus.2026-09-01.jsonl")).unwrap();
        let before = at("2026-10-30T23:59:59.999Z");
        let after = at("2026-10-31T00:00:00.001Z");
        let writer = TraceWriter::start(directory.path().into(), 2, before, vec![]);
        for ts in [before, after] {
            writer.record(
                ts,
                TraceEntry::Lwt {
                    bridge: "tasmota_cec".into(),
                    online: true,
                },
            );
        }
        writer.flush().await;
        for date in ["2026-10-30", "2026-10-31"] {
            let text =
                std::fs::read_to_string(directory.path().join(format!("cec-bus.{date}.jsonl")))
                    .unwrap();
            let mut entries = text.lines().map(|line| parse(line).unwrap().1);
            assert!(matches!(entries.next(), Some(TraceEntry::Identities(_))));
            assert!(matches!(
                entries.next(),
                Some(TraceEntry::Lwt { online: true, .. })
            ));
            assert!(entries.next().is_none());
        }
        assert!(!directory.path().join("cec-bus.2026-10-01.jsonl").exists());
        // At the first open the 28th is retained; midnight makes it older than two days.
        assert!(!directory.path().join("cec-bus.2026-10-28.jsonl").exists());
        assert!(directory.path().join("cec-bus.invalid.jsonl").exists());
        assert!(directory.path().join("other.2026-10-01.jsonl").exists());
        assert!(directory.path().join("cec-bus.2026-09-01.jsonl").is_dir());
    }

    #[tokio::test]
    async fn enabling_trace_midday_writes_identity_header_before_first_input() {
        let directory = tempfile::tempdir().unwrap();
        let start = at("2026-10-30T09:00:00Z");
        let enabled = at("2026-10-30T13:00:00Z");
        let identity = LearnedIdentity {
            la: 4,
            physical: Some(0x2100),
            vendor: Some(0x0010fa),
            osd: Some("Streamer".into()),
            device_type: Some(4),
            confirmed_at: start,
        };
        let writer = TraceWriter::start(directory.path().into(), 0, start, vec![]);
        writer.record(start, TraceEntry::Command(Command::Identify(8)));
        writer.identities(vec![identity.clone()]);
        writer.configure(enabled, 30);
        writer.record(enabled, TraceEntry::Command(Command::Identify(4)));
        writer.flush().await;
        let text =
            std::fs::read_to_string(directory.path().join("cec-bus.2026-10-30.jsonl")).unwrap();
        let mut lines = text.lines();
        let (ts, first) = parse(lines.next().unwrap()).unwrap();
        assert_eq!(ts, enabled);
        let TraceEntry::Identities(identities) = first else {
            panic!("line 1 must be the identities header");
        };
        assert_eq!(serde_json::to_value(identities).unwrap(), json!([identity]));
        assert!(matches!(
            parse(lines.next().unwrap()).unwrap().1,
            TraceEntry::Command(Command::Identify(4))
        ));
        assert!(lines.next().is_none());
    }

    #[tokio::test]
    async fn disabled_writer_does_not_create_or_prune_files_and_reload_can_enable_it() {
        let directory = tempfile::tempdir().unwrap();
        let ts = at("2026-10-30T12:00:00Z");
        let writer = TraceWriter::start(directory.path().into(), 0, ts, vec![]);
        writer.record(ts, TraceEntry::Command(Command::AllOff));
        writer.flush().await;
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let writer = TraceWriter::start(directory.path().into(), 0, ts, vec![]);
        writer.configure(ts, 30);
        writer.record(ts, TraceEntry::Command(Command::AllOff));
        writer.flush().await;
        let text =
            std::fs::read_to_string(directory.path().join("cec-bus.2026-10-30.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(matches!(
            parse(text.lines().nth(1).unwrap()).unwrap().1,
            TraceEntry::Command(Command::AllOff)
        ));
        let writer = TraceWriter::start(directory.path().into(), 30, ts, vec![]);
        writer.configure(ts, 0);
        writer.record(ts, TraceEntry::Command(Command::Identify(4)));
        writer.flush().await;
        assert_eq!(
            std::fs::read_to_string(directory.path().join("cec-bus.2026-10-30.jsonl")).unwrap(),
            text
        );
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[tokio::test]
    async fn prune_failure_does_not_prevent_capture_and_reopen_separates_torn_lines() {
        let directory = tempfile::tempdir().unwrap();
        let ts: DateTime<Utc> = "2026-10-30T12:00:00Z".parse().unwrap();
        let path = directory.path().join("cec-bus.2026-10-30.jsonl");
        std::fs::write(&path, r#"{"ts":"torn"#).unwrap();
        let mut files = Files {
            directory: directory.path().into(),
            keep_days: 30,
            identities: Arc::new(vec![]),
            open: None,
            prune_failed: false,
        };
        files
            .open_after_prune(
                ts,
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "test retention failure",
                )),
            )
            .await
            .unwrap();
        files
            .record(ts, &TraceEntry::Command(Command::AllOff))
            .await
            .unwrap();
        drop(files);
        let text = std::fs::read_to_string(path).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next().unwrap(), r#"{"ts":"torn"#);
        assert!(matches!(
            parse(lines.next().unwrap()).unwrap().1,
            TraceEntry::Identities(_)
        ));
        assert!(matches!(
            parse(lines.next().unwrap()).unwrap().1,
            TraceEntry::Command(Command::AllOff)
        ));
        assert!(lines.next().is_none());
    }

    #[tokio::test]
    async fn full_trace_queue_drops_without_blocking_and_recovers_after_space_is_available() {
        let (tx, mut rx) = mpsc::channel(1024);
        let (controls, _latest) = watch::channel(Controls {
            keep_days: 30,
            at: "2026-10-30T12:00:00Z".parse().unwrap(),
            identities: Arc::new(vec![]),
        });
        let writer = TraceWriter {
            tx,
            controls,
            task: tokio::spawn(async {}),
            dropped: AtomicBool::new(false),
        };
        let ts = "2026-10-30T12:00:00Z".parse().unwrap();
        for _ in 0..1024 {
            writer.record(ts, TraceEntry::Command(Command::AllOff));
        }
        assert_eq!(rx.len(), 1024);
        assert!(!writer.dropped.load(Ordering::Relaxed));
        writer.record(ts, TraceEntry::Command(Command::AllOff));
        writer.record(ts, TraceEntry::Command(Command::AllOff));
        assert!(writer.dropped.load(Ordering::Relaxed));
        assert_eq!(rx.len(), 1024);
        rx.try_recv().unwrap();
        writer.record(ts, TraceEntry::Command(Command::AllOff));
        assert!(!writer.dropped.load(Ordering::Relaxed));
        assert_eq!(rx.len(), 1024);
        writer.flush().await;
    }

    #[tokio::test]
    async fn full_record_queue_cannot_drop_retention_or_identity_controls() {
        let directory = tempfile::tempdir().unwrap();
        let ts = "2026-10-30T13:00:00Z".parse().unwrap();
        let writer = TraceWriter::start(directory.path().into(), 30, ts, vec![]);
        // This single-threaded test fills the queue before the writer can run.
        for _ in 0..1024 {
            writer.record(ts, TraceEntry::Command(Command::AllOff));
        }
        assert_eq!(writer.tx.capacity(), 0);
        writer.configure(ts, 0);
        assert_eq!(writer.controls.borrow().keep_days, 0);
        let identity = LearnedIdentity {
            la: 4,
            physical: Some(0x2100),
            vendor: Some(0x0010fa),
            osd: Some("Streamer".into()),
            device_type: Some(4),
            confirmed_at: ts,
        };
        writer.identities(vec![identity.clone()]);
        writer.configure(ts, 30);
        writer.flush().await;
        let text =
            std::fs::read_to_string(directory.path().join("cec-bus.2026-10-30.jsonl")).unwrap();
        let mut lines = text.lines();
        let TraceEntry::Identities(identities) = parse(lines.next().unwrap()).unwrap().1 else {
            panic!("missing identities header");
        };
        assert_eq!(serde_json::to_value(identities).unwrap(), json!([identity]));
        let records: Vec<_> = lines.map(|line| parse(line).unwrap().1).collect();
        assert_eq!(records.len(), 1024);
        assert!(
            records
                .iter()
                .all(|entry| matches!(entry, TraceEntry::Command(Command::AllOff)))
        );
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    #[test]
    fn native_raw_payloads_and_correlated_requests_roundtrip_without_topic_aliases() {
        let ts = "2026-11-01T00:00:00Z".parse().unwrap();
        for entry in [
            TraceEntry::AetherStatus {
                node: "cec-bridge".into(),
                payload: json!({"online":true,"v":1,"boot":3,"extra":"preserved"}),
            },
            TraceEntry::AetherBus {
                node: "cec-bridge".into(),
                payload: json!({"ev":"line","seq":4,"bus":"ok","extra":123}),
            },
            TraceEntry::Sent {
                bridge: "cec-bridge".into(),
                to: 4,
                data: vec![0x36],
                id: Some("s-1".into()),
            },
        ] {
            let text = line(ts, &entry);
            let (_, parsed) = parse_checked(&text).unwrap().unwrap();
            assert_eq!(line(ts, &parsed), text);
        }
        assert!(
            parse_checked(
                &json!({"ts":ts,"topic":"aether/cec-bridge/bus/extra","payload":{}}).to_string()
            )
            .unwrap()
            .is_none()
        );
        assert!(
            parse_checked(
                &json!({"ts":ts,"topic":"aether/cec-bridge/telemetry","payload":{}}).to_string()
            )
            .unwrap()
            .is_none()
        );
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    fn command(la: u8) -> TraceEntry {
        TraceEntry::Command(Command::Identify(la))
    }

    /// Writes `records` (timestamp, LA tag) as the day file of `date`, with a header and a
    /// torn line in the middle like a crashed writer leaves.
    fn day(directory: &Path, date: &str, records: &[(&str, u8)]) {
        let mut text = line(
            at(&format!("{date}T00:00:00Z")),
            &TraceEntry::Identities(vec![]),
        );
        for (index, (ts, la)) in records.iter().enumerate() {
            if index == 1 {
                text.push_str("{\"ts\":\"torn\n");
            }
            text.push_str(&line(at(ts), &command(*la)));
        }
        std::fs::write(
            day_file(directory, date.parse::<NaiveDate>().unwrap()),
            text,
        )
        .unwrap();
    }

    fn tags(entries: &[(DateTime<Utc>, TraceEntry)]) -> Vec<u8> {
        entries
            .iter()
            .map(|(_, entry)| match entry {
                TraceEntry::Command(Command::Identify(la)) => *la,
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    #[test]
    fn window_spans_midnight_across_two_day_files_inclusively() {
        let directory = tempfile::tempdir().unwrap();
        day(
            directory.path(),
            "2026-11-06",
            &[
                ("2026-11-06T23:59:00Z", 1),
                ("2026-11-06T23:59:50Z", 2),
                ("2026-11-06T23:59:59.999Z", 3),
            ],
        );
        day(
            directory.path(),
            "2026-11-07",
            &[
                ("2026-11-07T00:00:05Z", 4),
                ("2026-11-07T00:00:10Z", 5),
                ("2026-11-07T00:00:11Z", 6),
            ],
        );
        let entries = read_all(
            directory.path(),
            at("2026-11-06T23:59:50Z"),
            at("2026-11-07T00:00:10Z"),
            100,
        );
        // The second file's identity header (00:00:00) is inside the window too.
        assert!(matches!(entries[2].1, TraceEntry::Identities(_)));
        let records: Vec<_> = entries
            .into_iter()
            .filter(|(_, entry)| !matches!(entry, TraceEntry::Identities(_)))
            .collect();
        assert_eq!(tags(&records), [2, 3, 4, 5]);
        assert_eq!(records[0].0, at("2026-11-06T23:59:50Z"));
    }

    #[test]
    fn missing_day_files_read_as_an_empty_window() {
        let directory = tempfile::tempdir().unwrap();
        let window = |from, to| read_all(directory.path(), at(from), at(to), 100);
        assert!(window("2026-11-06T12:00:00Z", "2026-11-06T12:10:00Z").is_empty());
        // One of two days present: the window reads what exists.
        day(
            directory.path(),
            "2026-11-07",
            &[("2026-11-07T00:00:01Z", 7)],
        );
        let entries = window("2026-11-06T23:59:00Z", "2026-11-07T00:01:00Z");
        assert_eq!(entries.len(), 2, "header + one record");
        let absent = directory.path().join("absent");
        let from = at("2026-11-07T00:00:00Z");
        assert!(read_all(&absent, from, from, 10).is_empty());
    }

    #[test]
    fn limit_caps_the_entries_returned() {
        let directory = tempfile::tempdir().unwrap();
        day(
            directory.path(),
            "2026-11-06",
            &[
                ("2026-11-06T10:00:01Z", 1),
                ("2026-11-06T10:00:02Z", 2),
                ("2026-11-06T10:00:03Z", 3),
            ],
        );
        day(
            directory.path(),
            "2026-11-07",
            &[("2026-11-07T10:00:00Z", 4)],
        );
        let from = at("2026-11-06T10:00:00Z");
        let to = at("2026-11-07T11:00:00Z");
        let read = |limit| read_window(directory.path(), from, to, budget(limit)).unwrap();
        assert_eq!(tags(&read(2).entries), [1, 2]);
        assert!(read(2).truncated, "more in-window entries remain");
        assert!(read(0).entries.is_empty());
        let all = read(100);
        assert_eq!(
            all.entries.len(),
            5,
            "four records and the second day's header"
        );
        assert!(!all.truncated);
        // Exactly at the limit with nothing left in the window is complete.
        assert!(!read(5).truncated);
    }

    fn budget(entries: usize) -> ReadBudget {
        ReadBudget {
            entries,
            ..ReadBudget::WINDOW
        }
    }

    fn read_all(
        directory: &Path,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: usize,
    ) -> Vec<(DateTime<Utc>, TraceEntry)> {
        let read = read_window(directory, from, to, budget(limit)).unwrap();
        assert!(!read.truncated);
        read.entries
    }

    #[test]
    fn overlong_lines_are_skipped_without_buffering_them() {
        let directory = tempfile::tempdir().unwrap();
        let mut text = line(at("2026-11-07T10:00:01Z"), &command(1));
        // A torn writer's payload far over the line cap, valid JSON prefix and all.
        text.push_str("{\"ts\":\"2026-11-07T10:00:02Z\",\"topic\":\"x\",\"payload\":\"");
        text.push_str(&"A".repeat(4096));
        text.push_str("\"}\n");
        text.push_str(&line(at("2026-11-07T10:00:03Z"), &command(3)));
        std::fs::write(
            day_file(directory.path(), "2026-11-07".parse().unwrap()),
            text,
        )
        .unwrap();
        let read = read_window(
            directory.path(),
            at("2026-11-07T10:00:00Z"),
            at("2026-11-07T11:00:00Z"),
            ReadBudget {
                line_bytes: 1024,
                ..budget(100)
            },
        )
        .unwrap();
        assert_eq!(tags(&read.entries), [1, 3]);
        assert!(!read.truncated);

        let mut line = Vec::new();
        let mut reader = std::io::Cursor::new(format!("{}\nok\n", "B".repeat(100)));
        assert_eq!(
            bounded_line(&mut reader, &mut line, 10, u64::MAX).unwrap(),
            Some((101, false))
        );
        assert!(line.is_empty(), "the overlong line is not kept");
        assert_eq!(
            bounded_line(&mut reader, &mut line, 10, u64::MAX).unwrap(),
            Some((3, true))
        );
        assert_eq!(line, b"ok\n");
        assert_eq!(
            bounded_line(&mut reader, &mut line, 10, u64::MAX).unwrap(),
            None
        );
    }

    #[test]
    fn a_spent_scan_budget_is_flagged_not_silent() {
        let directory = tempfile::tempdir().unwrap();
        // Lots of pre-window lines, which never count against the entry limit.
        let early: Vec<(String, u8)> = (0..50)
            .map(|second| (format!("2026-11-07T09:00:{second:02}Z"), 0))
            .collect();
        let mut records: Vec<(&str, u8)> =
            early.iter().map(|(ts, la)| (ts.as_str(), *la)).collect();
        records.push(("2026-11-07T10:00:01Z", 9));
        day(directory.path(), "2026-11-07", &records);
        let (from, to) = (at("2026-11-07T10:00:00Z"), at("2026-11-07T11:00:00Z"));
        let read = |budget| read_window(directory.path(), from, to, budget).unwrap();

        let complete = read(budget(100));
        assert_eq!(
            (tags(&complete.entries), complete.truncated),
            (vec![9], false)
        );
        let by_lines = read(ReadBudget {
            scan_lines: 10,
            ..budget(100)
        });
        assert!(by_lines.entries.is_empty() && by_lines.truncated);
        let by_bytes = read(ReadBudget {
            scan_bytes: 500,
            ..budget(100)
        });
        assert!(by_bytes.entries.is_empty() && by_bytes.truncated);
        // A budget spent exactly at the end of the input is not a truncation.
        let size = std::fs::metadata(day_file(directory.path(), "2026-11-07".parse().unwrap()))
            .unwrap()
            .len();
        let exact = read(ReadBudget {
            scan_bytes: size,
            ..budget(100)
        });
        assert_eq!((tags(&exact.entries), exact.truncated), (vec![9], false));
    }
}
