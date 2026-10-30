//! Shared replay codec and best-effort, append-only runtime capture.
use crate::{
    cec::{Frame, bus::LearnedIdentity},
    state::Command,
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
    },
    Result {
        bridge: String,
        payload: Value,
    },
    HaState {
        entity: String,
        state: String,
        attrs: Value,
        changed: Option<DateTime<Utc>>,
    },
    Command(Command),
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
        TraceEntry::Sent { bridge, to, data } => (
            format!("cmnd/{bridge}/HdmiSend"),
            json!({"To":to,"Data":hex::encode_upper(data)}),
        ),
        TraceEntry::Result { bridge, payload } => {
            (format!("stat/{bridge}/RESULT"), payload.clone())
        }
        TraceEntry::HaState {
            entity,
            state,
            attrs,
            changed,
        } => (
            "ha/state".into(),
            json!({"entity":entity,"state":state,"attrs":attrs,"changed":changed}),
        ),
        TraceEntry::Command(command) => ("choregos/command".into(), json!(command)),
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
    let entry = parse_entry(&topic, payload);
    anyhow::ensure!(
        !known || entry.is_some(),
        "invalid trace payload for {topic}"
    );
    Ok(entry.map(|entry| (ts, entry)))
}

fn known_topic(topic: &str) -> bool {
    if matches!(
        topic,
        "choregos/identities" | "choregos/command" | "choregos/connection" | "ha/state"
    ) {
        return true;
    }
    let mut parts = topic.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some("tele"), Some(_), Some("SENSOR" | "LWT"), None)
            | (Some("cmnd"), Some(_), Some("HdmiSend"), None)
            | (Some("stat"), Some(_), Some("RESULT"), None)
    )
}

fn parse_entry(topic: &str, payload: Value) -> Option<TraceEntry> {
    Some(match topic {
        "choregos/identities" => TraceEntry::Identities(serde_json::from_value(payload).ok()?),
        "choregos/command" => TraceEntry::Command(serde_json::from_value(payload).ok()?),
        "choregos/connection" => TraceEntry::Connection {
            mqtt: payload.get("mqtt")?.as_bool()?,
        },
        "ha/state" => TraceEntry::HaState {
            entity: payload.get("entity")?.as_str()?.into(),
            state: payload.get("state")?.as_str()?.into(),
            attrs: payload.get("attrs")?.clone(),
            changed: serde_json::from_value(payload.get("changed")?.clone()).ok()?,
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
        },
        ("stat", "RESULT") if payload.get("HdmiSend").is_some() => {
            TraceEntry::Result { bridge, payload }
        }
        _ => return None,
    })
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
            },
            TraceEntry::HaState {
                entity: "media_player.receiver".into(),
                state: "unknown".into(),
                attrs: json!({}),
                changed: None,
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
