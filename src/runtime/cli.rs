use crate::{
    config::{self, Config},
    engine::{Engine, Event},
    runtime::{
        bridge::Correlation,
        trace::{self, TraceEntry},
    },
};
use anyhow::Context;
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub async fn run(path: std::path::PathBuf) -> anyhow::Result<()> {
    let (config, text) = config::load(&path)?;
    super::run(config, text, path).await
}
pub fn check_config(path: &Path) -> anyhow::Result<String> {
    let (config, _) = config::load(path)?;
    Ok(config.redacted_text()?)
}
pub async fn healthz(url: &str) -> anyhow::Result<()> {
    let response = openidconnect::reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?
        .get(url)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("health endpoint unavailable"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "health endpoint returned an error"
    );
    let health: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    anyhow::ensure!(health.get("mode").is_some(), "invalid health response");
    Ok(())
}
pub fn replay_file(config: &Path, fixture: &Path, output: &mut impl Write) -> anyhow::Result<()> {
    replay_files(config, &[fixture.to_path_buf()], None, None, output)
}

pub fn replay_files(
    config: &Path,
    fixtures: &[PathBuf],
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    output: &mut impl Write,
) -> anyhow::Result<()> {
    replay_files_with_warnings(
        config,
        fixtures,
        since,
        until,
        output,
        &mut std::io::stderr(),
    )
}

fn replay_files_with_warnings(
    config: &Path,
    fixtures: &[PathBuf],
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    output: &mut impl Write,
    warnings: &mut impl Write,
) -> anyhow::Result<()> {
    let config = Config::parse(&std::fs::read_to_string(config)?)?;
    let mut replay = Replay::new(config, since, until)?;
    replay.window.fallback_header = first_header(fixtures)?;
    visit_files(fixtures, warnings, |line| replay.line(line, output))?;
    replay.finish(output)
}

fn first_header(fixtures: &[PathBuf]) -> anyhow::Result<Option<String>> {
    for fixture in fixtures {
        use std::io::BufRead;
        for line in std::io::BufReader::new(std::fs::File::open(fixture)?).lines() {
            let line = line?;
            if identity_header(&line).is_some() {
                return Ok(Some(line));
            }
        }
    }
    Ok(None)
}

fn visit_files(
    fixtures: &[PathBuf],
    warnings: &mut impl Write,
    mut visit: impl FnMut(&str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for fixture in fixtures {
        use std::io::BufRead;
        for (index, line) in std::io::BufReader::new(std::fs::File::open(fixture)?)
            .lines()
            .enumerate()
        {
            let location = || format!("{}:{}", fixture.display(), index + 1);
            let line = line.with_context(location)?;
            match visit(&line) {
                Ok(()) => {}
                Err(error) if serde_json::from_str::<serde::de::IgnoredAny>(&line).is_err() => {
                    writeln!(
                        warnings,
                        "warning: {}: skipping invalid JSON trace line: {error:#}",
                        location()
                    )?;
                }
                Err(error) if error.is::<TraceOrderError>() => {
                    anyhow::bail!(
                        "trace files must be given in chronological order ({})",
                        location()
                    );
                }
                Err(error) => return Err(error).with_context(location),
            }
        }
    }
    Ok(())
}

/// Extract an offline replay window, optionally anonymizing its string values.
pub fn extract_files(
    fixtures: &[PathBuf],
    since: chrono::DateTime<chrono::Utc>,
    until: chrono::DateTime<chrono::Utc>,
    renames: &[(String, String)],
    node: Option<&str>,
    output: &mut impl Write,
) -> anyhow::Result<()> {
    extract_with_warnings(
        fixtures,
        (since, until),
        renames,
        node,
        output,
        &mut std::io::stderr(),
    )
}

/// Atomically replace an extraction output only after the whole trace succeeds.
pub fn extract_to_path(
    fixtures: &[PathBuf],
    since: chrono::DateTime<chrono::Utc>,
    until: chrono::DateTime<chrono::Utc>,
    renames: &[(String, String)],
    node: Option<&str>,
    output: &Path,
) -> anyhow::Result<()> {
    TraceWindow::new(Some(since), Some(until))?;
    if let Some(node) = node {
        parse_node(node).map_err(anyhow::Error::msg)?;
    }
    let existing_output = match output.canonicalize() {
        Ok(path) => Some(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("resolving extraction output"),
    };
    for fixture in fixtures {
        let input = fixture
            .canonicalize()
            .with_context(|| format!("resolving {}", fixture.display()))?;
        anyhow::ensure!(
            existing_output.as_ref() != Some(&input),
            "output must not overwrite an input trace"
        );
    }
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(".choregos-extract-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> anyhow::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("creating temporary output in {}", parent.display()))?;
        let mut writer = std::io::BufWriter::new(file);
        extract_files(fixtures, since, until, renames, node, &mut writer)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        std::fs::rename(&temporary, output)
            .with_context(|| format!("writing {}", output.display()))?;
        if let Err(error) = std::fs::File::open(parent).and_then(|directory| directory.sync_all()) {
            tracing::warn!(error = %error, "extraction renamed but directory sync failed; crash durability is not guaranteed");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn extract_with_warnings(
    fixtures: &[PathBuf],
    bounds: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>),
    renames: &[(String, String)],
    node: Option<&str>,
    output: &mut impl Write,
    warnings: &mut impl Write,
) -> anyhow::Result<()> {
    let mut window = TraceWindow::new(Some(bounds.0), Some(bounds.1))?;
    window.fallback_header = first_header(fixtures)?;
    let mut pending_headers = Vec::new();
    let mut seeded = false;
    visit_files(fixtures, warnings, |line| {
        let Some(selected) = window.select(line)? else {
            return Ok(());
        };
        if !seeded {
            if matches!(selected.entry, Some(TraceEntry::Identities(_))) {
                pending_headers.push(line.to_owned());
                return Ok(());
            }
            write_extraction_seed(&window, &mut pending_headers, renames, node, output)?;
            seeded = true;
        }
        write_extracted(line, renames, node, output)
    })?;
    if !seeded {
        write_extraction_seed(&window, &mut pending_headers, renames, node, output)?;
    }
    Ok(())
}

fn write_extraction_seed(
    window: &TraceWindow,
    pending_headers: &mut Vec<String>,
    renames: &[(String, String)],
    node: Option<&str>,
    output: &mut impl Write,
) -> anyhow::Result<()> {
    if pending_headers.is_empty() {
        if let Some(header) = window.seed() {
            write_extracted(header, renames, node, output)?;
        }
    } else {
        for header in pending_headers.drain(..) {
            write_extracted(&header, renames, node, output)?;
        }
    }
    if let Some(availability) = &window.availability {
        write_extracted(availability, renames, node, output)?;
    }
    Ok(())
}

pub fn parse_rename(value: &str) -> Result<(String, String), String> {
    let (old, new) = value.split_once('=').ok_or("--rename requires OLD=NEW")?;
    if old.is_empty() {
        return Err("--rename OLD must not be empty".into());
    }
    Ok((old.into(), new.into()))
}

pub fn parse_node(value: &str) -> Result<String, String> {
    if value.is_empty() || value.contains(['/', '+', '#']) {
        return Err("--node must be non-empty and contain no '/', '+' or '#'".into());
    }
    Ok(value.to_owned())
}

fn write_extracted(
    line: &str,
    renames: &[(String, String)],
    node: Option<&str>,
    output: &mut impl Write,
) -> anyhow::Result<()> {
    if renames.is_empty() && node.is_none() {
        writeln!(output, "{line}")?;
        return Ok(());
    }
    let mut value: serde_json::Value = serde_json::from_str(line)?;
    rename_strings(&mut value, renames)?;
    if let Some(node) = node
        && let Some(topic) = value
            .get_mut("topic")
            .and_then(|topic| topic.as_str().map(str::to_owned))
    {
        let mut parts = topic.splitn(3, '/');
        if let (Some(prefix @ ("aether" | "tele" | "stat" | "cmnd")), Some(_), Some(suffix)) =
            (parts.next(), parts.next(), parts.next())
        {
            value["topic"] = format!("{prefix}/{node}/{suffix}").into();
        }
    }
    serde_json::to_writer(&mut *output, &value)?;
    writeln!(output)?;
    Ok(())
}

/// Renames inside the ASCII operand of a Set OSD Name (`47`) frame; `None` when the
/// operand is not decodable ASCII or no rename touched it.
fn rename_osd_name(data: &str, renames: &[(String, String)]) -> anyhow::Result<Option<String>> {
    let Ok(bytes) = hex::decode(&data[2..]) else {
        return Ok(None);
    };
    if !bytes.is_ascii() {
        return Ok(None);
    }
    let original = std::str::from_utf8(&bytes)?;
    let mut name = original.to_owned();
    for (old, new) in renames {
        name = name.replace(old, new);
    }
    if name == original {
        return Ok(None);
    }
    anyhow::ensure!(name.is_ascii(), "renamed Set OSD Name must be ASCII");
    anyhow::ensure!(
        name.len() < 15,
        "renamed Set OSD Name exceeds 15-byte frame limit"
    );
    Ok(Some(format!("47{}", hex::encode_upper(name))))
}

fn rename_text(text: &mut String, renames: &[(String, String)]) -> anyhow::Result<()> {
    // Some captures store an entire JSON payload inside a JSON string.
    if let Ok(mut nested @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) =
        serde_json::from_str(text)
    {
        let before = nested.clone();
        rename_strings(&mut nested, renames)?;
        if nested != before {
            *text = serde_json::to_string(&nested)?;
        }
        return Ok(());
    }
    for (old, new) in renames {
        *text = text.replace(old, new);
    }
    Ok(())
}

fn rename_strings(
    value: &mut serde_json::Value,
    renames: &[(String, String)],
) -> anyhow::Result<()> {
    match value {
        serde_json::Value::String(text) => rename_text(text, renames),
        serde_json::Value::Array(values) => values
            .iter_mut()
            .try_for_each(|value| rename_strings(value, renames)),
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                let osd_frame = matches!(key.as_str(), "data" | "Data")
                    && value.as_str().is_some_and(|data| data.starts_with("47"));
                if osd_frame {
                    if let Some(renamed) =
                        rename_osd_name(value.as_str().unwrap_or_default(), renames)?
                    {
                        *value = renamed.into();
                    }
                } else if key != "ts" {
                    rename_strings(value, renames)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub fn replay(config: Config, text: &str, output: &mut impl Write) -> anyhow::Result<()> {
    let mut replay = Replay::new(config, None, None)?;
    replay.window.fallback_header = text
        .lines()
        .find(|line| identity_header(line).is_some())
        .map(str::to_owned);
    for line in text.lines() {
        replay.line(line, output)?;
    }
    replay.finish(output)
}

fn identity_header(line: &str) -> Option<Vec<crate::cec::bus::LearnedIdentity>> {
    match trace::parse(line)?.1 {
        TraceEntry::Identities(identities) => Some(identities),
        _ => None,
    }
}

#[derive(Debug, thiserror::Error)]
#[error("trace files must be given in chronological order")]
struct TraceOrderError;

/// Shared inclusive window and identity-seed selection for replay and extraction.
struct TraceWindow {
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    previous_ts: Option<chrono::DateTime<chrono::Utc>>,
    header: Option<String>,
    fallback_header: Option<String>,
    availability: Option<String>,
    started: bool,
}

impl TraceWindow {
    fn new(
        since: Option<chrono::DateTime<chrono::Utc>>,
        until: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            since.zip(until).is_none_or(|(a, b)| a <= b),
            "--since must not be after --until"
        );
        Ok(Self {
            since,
            until,
            previous_ts: None,
            header: None,
            fallback_header: None,
            availability: None,
            started: false,
        })
    }

    fn select(&mut self, line: &str) -> anyhow::Result<Option<WindowEntry>> {
        if line.trim().is_empty() {
            return Ok(None);
        }
        let parsed = trace::parse_checked(line)?;
        // Unknown topics still belong to an extracted window.
        #[derive(serde::Deserialize)]
        struct Timestamp {
            ts: chrono::DateTime<chrono::Utc>,
        }
        let timestamp = match &parsed {
            Some((timestamp, _)) => *timestamp,
            None => serde_json::from_str::<Timestamp>(line)?.ts,
        };
        let entry = parsed.map(|(_, entry)| entry);
        let header = matches!(entry, Some(TraceEntry::Identities(_)));
        if header {
            if !self.started {
                self.header = Some(line.to_owned());
            }
        } else {
            if self
                .previous_ts
                .is_some_and(|previous| timestamp < previous)
            {
                return Err(TraceOrderError.into());
            }
            self.previous_ts = Some(timestamp);
        }
        if self.since.is_some_and(|since| timestamp < since)
            && matches!(
                entry,
                Some(TraceEntry::Lwt { .. } | TraceEntry::AetherStatus { .. })
            )
        {
            self.availability = Some(line.to_owned());
        }
        if self.since.is_some_and(|since| timestamp < since)
            || self.until.is_some_and(|until| timestamp > until)
        {
            return Ok(None);
        }
        if !header {
            self.started = true;
        }
        Ok(Some(WindowEntry { timestamp, entry }))
    }

    fn seed(&self) -> Option<&str> {
        self.header.as_deref().or(self.fallback_header.as_deref())
    }
}

struct WindowEntry {
    timestamp: chrono::DateTime<chrono::Utc>,
    entry: Option<TraceEntry>,
}

struct Replay {
    config: Option<Config>,
    engine: Option<Engine>,
    start: Instant,
    epoch: Option<chrono::DateTime<chrono::Utc>>,
    last: Option<(Instant, chrono::DateTime<chrono::Utc>)>,
    window: TraceWindow,
    correlation: Correlation,
}

impl Replay {
    fn new(
        config: Config,
        since: Option<chrono::DateTime<chrono::Utc>>,
        until: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<Self> {
        let window = TraceWindow::new(since, until)?;
        Ok(Self {
            config: Some(config),
            engine: None,
            start: Instant::now(),
            epoch: None,
            last: None,
            window,
            correlation: Correlation::default(),
        })
    }

    fn line(&mut self, line: &str, output: &mut impl Write) -> anyhow::Result<()> {
        let Some(WindowEntry { timestamp, entry }) = self.window.select(line)? else {
            return Ok(());
        };
        let Some(entry) = entry else {
            return Ok(());
        };
        if matches!(entry, TraceEntry::Identities(_)) {
            return Ok(());
        }
        if self.engine.is_none()
            && let Some(availability) = self.window.availability.take()
        {
            let (timestamp, entry) =
                trace::parse_checked(&availability)?.expect("validated availability");
            self.apply_entry(timestamp, entry, output)?;
        }
        self.apply_entry(timestamp, entry, output)
    }

    fn apply_entry(
        &mut self,
        timestamp: chrono::DateTime<chrono::Utc>,
        entry: TraceEntry,
        output: &mut impl Write,
    ) -> anyhow::Result<()> {
        let epoch = *self.epoch.get_or_insert(timestamp);
        let now = self.start + (timestamp - epoch).to_std()?;
        if self.engine.is_none() {
            self.engine = Some(Engine::new(
                self.config.take().expect("replay config available"),
                self.start,
                self.window
                    .seed()
                    .and_then(identity_header)
                    .unwrap_or_default(),
            ));
        }
        let engine = self.engine.as_mut().expect("replay initialized");
        while let Some(deadline) = engine.next_deadline().filter(|d| *d < now) {
            let wall = epoch + chrono::Duration::from_std(deadline.duration_since(self.start))?;
            emit(engine, Event::Tick, deadline, wall, output)?;
        }
        let events = match entry {
            TraceEntry::Received { .. }
            | TraceEntry::Lwt { .. }
            | TraceEntry::Result { .. }
            | TraceEntry::AetherStatus { .. }
            | TraceEntry::AetherBus { .. } => self.correlation.apply(&entry, now).events,
            TraceEntry::Sent { to, data, id, .. } => {
                self.correlation.record_send(id, to, data, now);
                vec![]
            }
            TraceEntry::HaState {
                entity,
                state,
                attrs,
                changed,
            } => vec![Event::HaState {
                entity,
                state,
                attrs: serde_json::from_value(attrs)?,
                changed,
            }],
            TraceEntry::Command(command) => vec![Event::Command(command)],
            TraceEntry::Connection { mqtt } => {
                self.correlation.clear();
                if mqtt {
                    vec![]
                } else {
                    vec![Event::BridgeOffline]
                }
            }
            TraceEntry::Identities(_) => unreachable!(),
        };
        for event in events {
            emit(engine, event, now, timestamp, output)?;
        }
        self.last = Some((now, timestamp));
        Ok(())
    }

    fn finish(&mut self, output: &mut impl Write) -> anyhow::Result<()> {
        if let Some((now, wall)) = self.last {
            let duration = Duration::from_secs(5);
            let engine = self.engine.as_mut().expect("replay initialized");
            while let Some(deadline) = engine.next_deadline().filter(|d| *d <= now + duration) {
                let timestamp = wall + chrono::Duration::from_std(deadline.duration_since(now))?;
                emit(engine, Event::Tick, deadline, timestamp, output)?;
            }
        }
        Ok(())
    }
}

fn emit(
    engine: &mut Engine,
    event: Event,
    now: Instant,
    wall: chrono::DateTime<chrono::Utc>,
    output: &mut impl Write,
) -> anyhow::Result<()> {
    engine.handle(event, now, wall);
    for decision in &engine.decisions {
        #[derive(serde::Serialize)]
        struct TimedDecision<'a> {
            at: chrono::DateTime<chrono::Utc>,
            #[serde(flatten)]
            decision: &'a crate::action::Decision,
        }
        serde_json::to_writer(&mut *output, &TimedDecision { at: wall, decision })?;
        writeln!(output)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture() -> (Config, Vec<String>) {
        use crate::{
            cec::{Frame, bus::LearnedIdentity},
            state::{Command, Mode},
        };
        use serde_json::json;
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config.mode = Mode::Active;
        config.timing.standby_retries = 0;
        config.timing.takeover_settle_ms = 100;
        let epoch: chrono::DateTime<chrono::Utc> = "2026-10-30T12:00:00Z".parse().unwrap();
        let bridge = config.bridge.name().to_owned();
        let entries = vec![
            (
                0,
                TraceEntry::Identities(vec![LearnedIdentity {
                    la: 4,
                    physical: Some(0x2100),
                    vendor: Some(0x0010fa),
                    osd: Some("Streamer".into()),
                    device_type: Some(4),
                    confirmed_at: epoch,
                }]),
            ),
            (
                0,
                TraceEntry::Lwt {
                    bridge: bridge.clone(),
                    online: true,
                },
            ),
            (
                0,
                TraceEntry::HaState {
                    entity: config.receiver.entity.clone(),
                    state: "on".into(),
                    attrs: json!({"source":"Streamer"}),
                    changed: Some(epoch),
                },
            ),
            (
                1000,
                TraceEntry::Received {
                    bridge: bridge.clone(),
                    frame: Frame::from_hex(4, 15, "84210004").unwrap(),
                },
            ),
            (
                1001,
                TraceEntry::Received {
                    bridge: bridge.clone(),
                    frame: Frame::from_hex(4, 15, "822100").unwrap(),
                },
            ),
            (
                2000,
                TraceEntry::Command(Command::Standby("streamer".into())),
            ),
            (
                2000,
                TraceEntry::Sent {
                    bridge: bridge.clone(),
                    to: 4,
                    data: vec![0x36],
                    id: None,
                },
            ),
            (
                2000,
                TraceEntry::Sent {
                    bridge: bridge.clone(),
                    to: 4,
                    data: vec![0x8f],
                    id: None,
                },
            ),
            (
                2001,
                TraceEntry::Result {
                    bridge: bridge.clone(),
                    payload: json!({"HdmiSend":{"Ack":false}}),
                },
            ),
            (
                2002,
                TraceEntry::Result {
                    bridge: bridge.clone(),
                    payload: json!({"HdmiSend":{"Ack":false}}),
                },
            ),
            (
                3000,
                TraceEntry::HaState {
                    entity: config.tv.entity.clone(),
                    state: "on".into(),
                    attrs: json!({}),
                    changed: None,
                },
            ),
        ];
        (
            config,
            entries
                .into_iter()
                .map(|(ms, entry)| trace::line(epoch + chrono::Duration::milliseconds(ms), &entry))
                .collect(),
        )
    }

    #[test]
    fn replay_correlates_recorded_sends_and_results_with_seeded_identity_ha_and_commands() {
        let (config, lines) = capture();
        let mut output = vec![];
        replay(config.clone(), &lines.concat(), &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("standby_target_absent"), "{text}");
        assert!(text.contains("takeover_settled"), "{text}");
        let without_results: String = lines
            .into_iter()
            .filter(|line| !line.contains("/RESULT"))
            .collect();
        let mut output = vec![];
        replay(config, &without_results, &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains("standby_target_absent"));
        assert!(text.contains("standby_unconfirmed"), "{text}");
    }

    #[test]
    fn replay_filters_frames_but_never_filters_the_identity_header() {
        let (config, mut lines) = capture();
        let epoch: chrono::DateTime<chrono::Utc> = "2026-10-30T12:00:00Z".parse().unwrap();
        lines.insert(
            1,
            trace::line(
                epoch,
                &TraceEntry::Received {
                    bridge: config.bridge.name().to_owned(),
                    frame: crate::cec::Frame::from_hex(4, 15, "87000001").unwrap(),
                },
            ),
        );
        let mut runner = Replay::new(
            config,
            Some(epoch + chrono::Duration::milliseconds(500)),
            Some(epoch + chrono::Duration::milliseconds(1500)),
        )
        .unwrap();
        let mut output = vec![];
        for line in lines {
            runner.line(&line, &mut output).unwrap();
        }
        runner.finish(&mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("streamer"), "{text}");
        assert!(text.contains("takeover_settled"), "{text}");
        assert!(!text.contains("command_standby"), "{text}");
        let engine = runner.engine.unwrap();
        assert_eq!(engine.active_role.as_deref(), Some("streamer"));
        assert_eq!(engine.bus.entries[&4].vendor, Some(0x0010fa));
    }

    #[test]
    fn replay_concatenates_multiple_files_in_argument_order() {
        let (config, lines) = capture();
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("choregos.toml");
        std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let first = directory.path().join("first.jsonl");
        let second = directory.path().join("second.jsonl");
        std::fs::write(&first, lines[..6].concat()).unwrap();
        std::fs::write(&second, lines[6..].concat()).unwrap();
        let mut multiple = vec![];
        replay_files(&config_path, &[first, second], None, None, &mut multiple).unwrap();
        let mut single = vec![];
        replay(config, &lines.concat(), &mut single).unwrap();
        assert_eq!(multiple, single);
        assert!(
            String::from_utf8(multiple)
                .unwrap()
                .contains("standby_target_absent")
        );
    }

    #[test]
    fn replay_uses_first_header_as_fallback_after_events_and_ignores_post_start_headers() {
        let (config, mut lines) = capture();
        // Both availability and a verifying PA frame precede the seed. Processing
        // the PA without that seed would leave the streamer's vendor/OSD missing.
        let header = lines.remove(0);
        lines.insert(3, header);
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("choregos.toml");
        std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let first = directory.path().join("first.jsonl");
        let second = directory.path().join("second.jsonl");
        std::fs::write(&first, lines[..6].concat()).unwrap();
        let later_header = trace::line(
            "2026-10-30T12:00:02Z".parse().unwrap(),
            &TraceEntry::Identities(vec![]),
        );
        std::fs::write(&second, later_header + &lines[6..].concat()).unwrap();
        let mut output = vec![];
        replay_files(&config_path, &[first, second], None, None, &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("takeover_settled"), "{text}");
        assert!(text.contains("standby_target_absent"), "{text}");
        let mut single = vec![];
        replay(config, &lines.concat(), &mut single).unwrap();
        assert_eq!(text, String::from_utf8(single).unwrap());
    }

    #[test]
    fn replay_uses_last_pre_window_header_instead_of_the_oldest_file_seed() {
        let (config, lines) = capture();
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("choregos.toml");
        std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let first = directory.path().join("old.jsonl");
        let second = directory.path().join("current.jsonl");
        std::fs::write(
            &first,
            trace::line(
                "2026-10-29T00:00:00Z".parse().unwrap(),
                &TraceEntry::Identities(vec![]),
            ),
        )
        .unwrap();
        let later_header = trace::line(
            "2026-10-30T12:00:02Z".parse().unwrap(),
            &TraceEntry::Identities(vec![]),
        );
        std::fs::write(
            &second,
            lines[..6].concat() + &later_header + &lines[6..].concat(),
        )
        .unwrap();
        let mut output = vec![];
        replay_files(
            &config_path,
            &[first, second],
            Some("2026-10-30T12:00:00.500Z".parse().unwrap()),
            None,
            &mut output,
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("takeover_settled"), "{text}");
        assert!(text.contains("standby_target_absent"), "{text}");
    }

    #[test]
    fn replay_discards_results_after_recorded_send_expiry() {
        let (mut config, mut lines) = capture();
        // Keep verification pending beyond the correlation expiry so that the
        // lack of an absent-target decision proves expiry, not an early timeout.
        config.timing.takeover_settle_ms = 10_000;
        for (index, ms) in [(8, 7001), (9, 7002)] {
            let (_, entry) = trace::parse(&lines[index]).unwrap();
            lines[index] = trace::line(
                "2026-10-30T12:00:00Z"
                    .parse::<chrono::DateTime<chrono::Utc>>()
                    .unwrap()
                    + chrono::Duration::milliseconds(ms),
                &entry,
            );
        }
        let (_, entry) = trace::parse(&lines[10]).unwrap();
        lines[10] = trace::line("2026-10-30T12:00:13Z".parse().unwrap(), &entry);
        let mut output = vec![];
        replay(config, &lines.concat(), &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains("standby_target_absent"), "{text}");
        assert!(text.contains("standby_unconfirmed"), "{text}");
    }

    #[test]
    fn replay_rejects_inverted_time_bounds_with_a_clear_error() {
        let config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        let since = "2026-10-30T12:00:01Z".parse().unwrap();
        let until = "2026-10-30T12:00:00Z".parse().unwrap();
        let error = Replay::new(config, Some(since), Some(until)).err().unwrap();
        assert_eq!(error.to_string(), "--since must not be after --until");
    }
    #[test]
    fn replay_outputs_actual_decisions_without_environment_or_network() {
        let config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        let mut output = vec![];
        replay(
            config,
            include_str!("../../fixtures/capture-streamer-wake.jsonl"),
            &mut output,
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("takeover_settled"));
        assert!(text.contains("off_timer_expired"));
        assert!(
            text.lines()
                .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
        );
    }
    #[test]
    fn replay_file_never_creates_or_changes_runtime_state() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("choregos.toml");
        let fixture = directory.path().join("capture.jsonl");
        let state = directory.path().join("choregos.state.json");
        std::fs::write(&config, include_str!("../../choregos.example.toml")).unwrap();
        std::fs::write(
            &fixture,
            include_str!("../../fixtures/capture-streamer-wake.jsonl"),
        )
        .unwrap();
        let mut output = vec![];
        replay_file(&config, &fixture, &mut output).unwrap();
        assert!(!state.exists());
        std::fs::write(&state, b"existing runtime state").unwrap();
        replay_file(&config, &fixture, &mut output).unwrap();
        assert_eq!(std::fs::read(&state).unwrap(), b"existing runtime state");
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("takeover_settled")
        );
    }
    #[tokio::test]
    async fn health_probe_accepts_health_json_and_rejects_invalid_responses() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new()
            .route(
                "/good",
                axum::routing::get(|| async { axum::Json(serde_json::json!({"mode":"shadow"})) }),
            )
            .route("/bad", axum::routing::get(|| async { "{}" }));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        healthz(&format!("http://{address}/good")).await.unwrap();
        assert!(healthz(&format!("http://{address}/bad")).await.is_err());
        assert!(healthz(&format!("http://{address}/missing")).await.is_err());
        server.abort();
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use crate::state::Command;
    use chrono::{DateTime, Utc};
    use serde_json::json;

    fn config() -> Config {
        Config::parse(include_str!("../../choregos.example.toml")).unwrap()
    }
    fn ts() -> DateTime<Utc> {
        "2026-10-30T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn recorded_all_off_during_replay_warming_is_not_revalidated() {
        let bridge = config().bridge.name().to_owned();
        let lines = trace::line(
            ts(),
            &TraceEntry::Lwt {
                bridge,
                online: true,
            },
        ) + &trace::line(
            ts() + chrono::Duration::seconds(2),
            &TraceEntry::Command(Command::AllOff),
        );
        let mut output = vec![];
        replay(config(), &lines, &mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn malformed_envelopes_fail_but_unknown_topics_and_blank_lines_are_ignored() {
        for line in [
            "{",
            r#"{"topic":"ha/state","payload":{}}"#,
            r#"{"ts":"garbage","topic":"ha/state","payload":{}}"#,
            r#"{"ts":"2026-10-30T12:00:00Z","payload":{}}"#,
            r#"{"ts":"2026-10-30T12:00:00Z","topic":"ha/state"}"#,
        ] {
            let error = replay(config(), line, &mut vec![]).unwrap_err();
            assert!(
                error.to_string().contains("invalid trace envelope"),
                "{error}"
            );
        }
        assert!(replay(config(), "\n", &mut vec![]).is_ok());
        assert!(
            replay(
                config(),
                &json!({"ts":ts(),"topic":"future/input","payload":null}).to_string(),
                &mut vec![]
            )
            .is_ok()
        );
        let invalid_command =
            json!({"ts":ts(),"topic":"choregos/command","payload":{"kind":"future_command"}});
        assert!(
            replay(config(), &invalid_command.to_string(), &mut vec![])
                .unwrap_err()
                .to_string()
                .contains("invalid trace payload")
        );
    }

    #[test]
    fn connection_loss_clears_recorded_sends_without_faking_a_bridge_lwt() {
        let mut runner = Replay::new(config(), None, None).unwrap();
        let bridge = config().bridge.name().to_owned();
        let mut output = vec![];
        for entry in [
            TraceEntry::Lwt {
                bridge: bridge.clone(),
                online: true,
            },
            TraceEntry::Sent {
                bridge: bridge.clone(),
                to: 4,
                data: vec![0x36],
                id: None,
            },
            TraceEntry::Connection { mqtt: false },
            TraceEntry::Result {
                bridge,
                payload: json!({"HdmiSend":{"Ack":false}}),
            },
            TraceEntry::Connection { mqtt: true },
        ] {
            runner
                .line(&trace::line(ts(), &entry), &mut output)
                .unwrap();
        }
        assert!(!runner.engine.unwrap().bridge_online);
        assert!(output.is_empty());
    }
}

#[cfg(test)]
mod file_review_tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("choregos.toml");
        std::fs::write(&config, include_str!("../../choregos.example.toml")).unwrap();
        (directory, config)
    }

    #[test]
    fn file_replay_warns_with_location_and_continues_after_a_torn_json_line() {
        let (directory, config_path) = setup();
        let fixture = directory.path().join("torn.jsonl");
        let capture = include_str!("../../fixtures/capture-streamer-wake.jsonl");
        let (first, rest) = capture.split_once('\n').unwrap();
        let header = trace::line(
            "2026-10-25T12:34:18Z".parse().unwrap(),
            &TraceEntry::Identities(vec![]),
        );
        let text = format!("{header}{first}\n{{\"ts\":\"torn\n{header}{rest}");
        std::fs::write(&fixture, &text).unwrap();
        let mut output = vec![];
        let mut warnings = vec![];
        replay_files_with_warnings(
            &config_path,
            std::slice::from_ref(&fixture),
            None,
            None,
            &mut output,
            &mut warnings,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("takeover_settled"));
        assert!(output.contains("off_timer_expired"));
        let warnings = String::from_utf8(warnings).unwrap();
        assert!(
            warnings.contains(&format!("{}:3", fixture.display())),
            "{warnings}"
        );
        assert!(
            warnings.contains("skipping invalid JSON trace line"),
            "{warnings}"
        );
        let config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        assert!(
            replay(config, &text, &mut vec![]).is_err(),
            "text fixtures remain strict"
        );
    }

    #[test]
    fn file_replay_keeps_malformed_known_payloads_and_valid_json_envelope_errors_fatal() {
        let (directory, config_path) = setup();
        let fixture = directory.path().join("invalid.jsonl");
        for text in [
            r#"{"ts":"2026-10-30T12:00:00Z","topic":"choregos/command","payload":{"kind":"send_raw","data":{"to":"bad"}}}"#,
            r#"{"topic":"ha/state","payload":{}}"#,
        ] {
            std::fs::write(&fixture, text).unwrap();
            let mut warnings = vec![];
            let error = replay_files_with_warnings(
                &config_path,
                std::slice::from_ref(&fixture),
                None,
                None,
                &mut vec![],
                &mut warnings,
            )
            .unwrap_err();
            let text = format!("{error:#}");
            assert!(text.contains(&format!("{}:1", fixture.display())), "{text}");
            assert!(text.contains("invalid trace"), "{text}");
            assert!(warnings.is_empty());
        }
    }

    #[test]
    fn out_of_order_files_fail_with_a_clear_filename_and_line_number() {
        let (directory, config_path) = setup();
        let later = directory.path().join("later.jsonl");
        let earlier = directory.path().join("earlier.jsonl");
        let entry = TraceEntry::Lwt {
            bridge: "tasmota_cec".into(),
            online: true,
        };
        std::fs::write(
            &later,
            trace::line("2026-10-31T00:00:00Z".parse().unwrap(), &entry),
        )
        .unwrap();
        std::fs::write(
            &earlier,
            trace::line("2026-10-30T00:00:00Z".parse().unwrap(), &entry),
        )
        .unwrap();
        let error = replay_files(
            &config_path,
            &[later, earlier.clone()],
            None,
            None,
            &mut vec![],
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "trace files must be given in chronological order ({}:1)",
                earlier.display()
            )
        );
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use crate::{
        config::BridgeConfig,
        runtime::bridge::{self, Incoming},
        state::{Command, Mode},
    };
    use serde_json::{Value, json};

    #[test]
    fn native_trace_replay_emits_the_same_decisions_as_live_decoding() {
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config.bridge = BridgeConfig::Aether {
            node: "cec-bridge".into(),
            logical_address: 0,
        };
        config.mode = Mode::Active;
        config.timing.standby_retries = 0;
        config.timing.takeover_settle_ms = 100;
        let rows = [
            (
                0,
                "status",
                json!({"online":true,"v":1,"boot":1,"bus":"ok"}),
            ),
            (
                1,
                "bus",
                json!({"ev":"rx","seq":1,"from":4,"to":15,"data":"84210004","ack":true,"t_us":1000}),
            ),
            (
                2,
                "bus",
                json!({"ev":"rx","seq":2,"from":4,"to":15,"data":"870010FA","ack":true,"t_us":2000}),
            ),
            (
                3,
                "bus",
                json!({"ev":"rx","seq":3,"from":4,"to":0,"data":"4753747265616D6572","ack":true,"t_us":3000}),
            ),
            (
                4,
                "bus",
                json!({"ev":"rx","seq":4,"from":4,"to":15,"data":"822100","ack":true,"t_us":4000}),
            ),
            (1000, "command", json!(Command::Standby("streamer".into()))),
            (1001, "tx", json!({"id":"s-1","to":4,"data":"36"})),
            (1002, "tx", json!({"id":"s-2","to":4,"data":"8F"})),
            (
                1003,
                "bus",
                json!({"ev":"tx","seq":5,"id":"s-1","origin":"host","status":"nack","attempts":2}),
            ),
            (
                1004,
                "bus",
                json!({"ev":"tx","seq":6,"id":"s-2","origin":"host","status":"nack","attempts":2}),
            ),
        ];
        let epoch: chrono::DateTime<chrono::Utc> = "2026-11-01T00:00:00Z".parse().unwrap();
        let start = Instant::now();
        let mut engine = Engine::new(config.clone(), start, vec![]);
        let mut correlation = Correlation::default();
        let mut live = vec![];
        let mut lines = String::new();
        for (ms, suffix, payload) in rows {
            let ts = epoch + chrono::Duration::milliseconds(ms);
            let now = start + Duration::from_millis(ms as u64);
            while let Some(deadline) = engine.next_deadline().filter(|d| *d < now) {
                let wall =
                    epoch + chrono::Duration::from_std(deadline.duration_since(start)).unwrap();
                emit(&mut engine, Event::Tick, deadline, wall, &mut live).unwrap();
            }
            let topic = if suffix == "command" {
                "choregos/command".into()
            } else {
                format!("aether/cec-bridge/{suffix}")
            };
            lines.push_str(&json!({"ts":ts,"topic":topic,"payload":payload}).to_string());
            lines.push('\n');
            if suffix == "tx" {
                correlation.record_send(
                    Some(payload["id"].as_str().unwrap().into()),
                    4,
                    hex::decode(payload["data"].as_str().unwrap()).unwrap(),
                    now,
                );
            } else if suffix == "command" {
                emit(
                    &mut engine,
                    Event::Command(serde_json::from_value(payload).unwrap()),
                    now,
                    ts,
                    &mut live,
                )
                .unwrap();
            } else {
                let Some(Incoming::Trace(entry)) = bridge::decode(
                    &config.bridge,
                    &topic,
                    &serde_json::to_vec(&payload).unwrap(),
                ) else {
                    panic!("live codec failed")
                };
                for event in correlation.apply(&entry, now).events {
                    emit(&mut engine, event, now, ts, &mut live).unwrap();
                }
            }
        }
        while let Some(deadline) = engine
            .next_deadline()
            .filter(|d| *d <= start + Duration::from_millis(6004))
        {
            let wall = epoch + chrono::Duration::from_std(deadline.duration_since(start)).unwrap();
            emit(&mut engine, Event::Tick, deadline, wall, &mut live).unwrap();
        }
        let mut replayed = vec![];
        replay(config, &lines, &mut replayed).unwrap();
        assert_eq!(replayed, live);
        assert!(
            String::from_utf8(live)
                .unwrap()
                .contains("standby_target_absent")
        );
    }

    #[test]
    fn native_replay_without_status_and_since_after_status_keep_frames_and_decisions() {
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config.bridge = BridgeConfig::Aether {
            node: "cec-bridge".into(),
            logical_address: 0,
        };
        let epoch: chrono::DateTime<chrono::Utc> = "2026-11-01T00:00:00Z".parse().unwrap();
        let status = json!({"ts":epoch,"topic":"aether/cec-bridge/status","payload":{"online":true,"v":1,"boot":1,"t_us":0}}).to_string();
        let frames: Vec<String> = [
            (15, "84210004"),
            (15, "870010FA"),
            (0, "4753747265616D6572"),
            (0, "04"),
            (15, "822100"),
        ].into_iter().enumerate().map(|(i, (to, data))| {
            json!({"ts":epoch + chrono::Duration::seconds(i as i64 + 1),
                "topic":"aether/cec-bridge/bus",
                "payload":{"ev":"rx","seq":i+1,"from":4,"to":to,"data":data,"ack":true,"t_us":(i+1)*1000}}).to_string()
        }).collect();
        let run = |include_status, since| {
            let mut replay = Replay::new(config.clone(), since, None).unwrap();
            let mut output = vec![];
            if include_status {
                replay.line(&status, &mut output).unwrap();
            }
            for line in &frames {
                replay.line(line, &mut output).unwrap();
            }
            replay.finish(&mut output).unwrap();
            let engine = replay.engine.as_ref().unwrap();
            assert_eq!(engine.bus.entries[&4].physical, Some(0x2100));
            assert_eq!(engine.roles["streamer"].power, crate::state::RolePower::On);
            assert_eq!(engine.bridge_online, include_status);
            let decisions: Vec<Value> = String::from_utf8(output)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert!(decisions.iter().any(|d| d["reason"] == "ImageViewOn"));
            decisions
        };
        let with_status = run(true, None);
        assert_eq!(run(false, None), with_status);
        assert_eq!(
            run(true, Some(epoch + chrono::Duration::seconds(1))),
            with_status
        );

        // The shared decoder delivers every frame regardless of the status prefix;
        // only the prefix itself adds an availability event.
        let now = Instant::now();
        let (_, status_entry) = trace::parse_checked(&status).unwrap().unwrap();
        let mut with = Correlation::default();
        assert!(matches!(
            &with.apply(&status_entry, now).events[..],
            [Event::BridgeOnline]
        ));
        let mut without = Correlation::default();
        for line in frames {
            let (_, entry) = trace::parse_checked(&line).unwrap().unwrap();
            let with_events = with.apply(&entry, now).events;
            let without_events = without.apply(&entry, now).events;
            assert!(matches!((&with_events[..], &without_events[..]),
                ([Event::Frame(a)], [Event::Frame(b)]) if a == b));
        }
    }
}

#[cfg(test)]
mod extraction_tests {
    use super::*;
    use serde_json::{Value, json};

    fn timestamp(seconds: i64) -> chrono::DateTime<chrono::Utc> {
        "2026-11-01T12:00:00Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap()
            + chrono::Duration::seconds(seconds)
    }

    fn row(seconds: i64, topic: &str, payload: Value) -> String {
        format!(
            "{}\n",
            json!({"ts":timestamp(seconds),"topic":topic,"payload":payload})
        )
    }

    #[test]
    fn extraction_preserves_inclusive_window_order_unknown_topics_and_restart_headers() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.jsonl");
        let second = directory.path().join("second.jsonl");
        let old = row(-20, "choregos/identities", json!([]));
        let seed = row(-10, "choregos/identities", json!([]));
        let initial = row(0, "future/input", json!({"unchanged":true}));
        let restart = row(1, "choregos/identities", json!([]));
        let last = row(2, "choregos/connection", json!({"mqtt":false}));
        std::fs::write(&first, old + &seed + &initial + "{\"ts\":\"torn\n").unwrap();
        std::fs::write(
            &second,
            restart.clone() + &last + &row(3, "future/input", Value::Null),
        )
        .unwrap();
        let mut output = vec![];
        let mut warnings = vec![];
        extract_with_warnings(
            &[first.clone(), second],
            (timestamp(0), timestamp(2)),
            &[],
            None,
            &mut output,
            &mut warnings,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            seed + &initial + &restart + &last
        );
        let warnings = String::from_utf8(warnings).unwrap();
        assert!(
            warnings.contains(&format!("{}:4", first.display())),
            "{warnings}"
        );
    }

    #[test]
    fn extraction_seeds_last_pre_window_availability_with_its_original_timestamp() {
        for (topic, old, latest) in [
            ("tele/bridge/LWT", json!("Offline"), json!("Online")),
            (
                "aether/bridge/status",
                json!({"online":false}),
                json!({"online":true,"v":1,"boot":1}),
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let fixture = directory.path().join("trace.jsonl");
            let header = row(-10, "choregos/identities", json!([]));
            let availability = row(-1, topic, latest.clone());
            let in_window = row(0, topic, latest);
            std::fs::write(
                &fixture,
                header.clone() + &row(-5, topic, old) + &availability + &in_window,
            )
            .unwrap();
            let mut output = vec![];
            extract_files(
                &[fixture],
                timestamp(0),
                timestamp(2),
                &[],
                None,
                &mut output,
            )
            .unwrap();
            assert_eq!(
                String::from_utf8(output).unwrap(),
                header + &availability + &in_window
            );
        }
    }

    #[test]
    fn extraction_falls_back_to_first_later_header_without_dropping_it() {
        let directory = tempfile::tempdir().unwrap();
        let fixture = directory.path().join("trace.jsonl");
        let event = row(0, "choregos/connection", json!({"mqtt":true}));
        let header = row(1, "choregos/identities", json!([]));
        std::fs::write(&fixture, event.clone() + &header).unwrap();
        let mut output = vec![];
        extract_files(
            &[fixture],
            timestamp(0),
            timestamp(2),
            &[],
            None,
            &mut output,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            header.clone() + &event + &header
        );
    }

    #[test]
    fn extraction_renames_nested_payloads_identities_osd_and_both_topic_kinds_in_order() {
        let renames = vec![
            ("Original".into(), "Middle".into()),
            ("Middle".into(), "Public".into()),
        ];
        for (topic, key) in [("aether/old/bus", "data"), ("tele/old/SENSOR", "Data")] {
            let original = json!({
                "ts":timestamp(0),
                "topic":topic,
                "payload":{"frame":{key:format!("47{}", hex::encode("Original"))},
                    "attrs":{"source":"Original","list":["Original"]},
                    "identities":[{"osd":"Original"}],
                    "serialized":json!({"osd":"Original","Data":format!("47{}",hex::encode("Original"))}).to_string()}
            });
            let mut output = vec![];
            write_extracted(&original.to_string(), &renames, Some("public"), &mut output).unwrap();
            let value: Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(value["ts"], original["ts"]);
            assert_eq!(value["topic"], topic.replace("/old/", "/public/"));
            assert_eq!(
                value["payload"]["frame"][key],
                format!("47{}", hex::encode_upper("Public"))
            );
            assert_eq!(value["payload"]["attrs"]["source"], "Public");
            assert_eq!(value["payload"]["attrs"]["list"][0], "Public");
            assert_eq!(value["payload"]["identities"][0]["osd"], "Public");
            let serialized: Value =
                serde_json::from_str(value["payload"]["serialized"].as_str().unwrap()).unwrap();
            assert_eq!(serialized["osd"], "Public");
            assert_eq!(
                serialized["Data"],
                format!("47{}", hex::encode_upper("Public"))
            );
        }
    }

    #[test]
    fn osd_renames_validate_only_changed_names_with_fifteen_byte_frame_limit() {
        let original = json!({"data":"4741"}).to_string();
        let mut output = vec![];
        write_extracted(
            &original,
            &[("A".into(), "12345678901234".into())],
            None,
            &mut output,
        )
        .unwrap();
        for replacement in ["123456789012345", "é"] {
            let error = write_extracted(
                &original,
                &[("A".into(), replacement.into())],
                None,
                &mut vec![],
            )
            .unwrap_err();
            assert!(error.to_string().contains(if replacement.is_ascii() {
                "15-byte"
            } else {
                "ASCII"
            }));
        }
        for data in ["474", "47FF", "47nothex", "47536869656c4420416e64726f6964"] {
            let original = json!({"data":data});
            let mut output = vec![];
            write_extracted(
                &original.to_string(),
                &[("Unrelated".into(), "X".into())],
                None,
                &mut output,
            )
            .unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), original);
        }
        assert!(parse_rename("missing").is_err());
        assert!(parse_rename("=empty").is_err());
        assert_eq!(
            parse_rename("old=new=tail").unwrap(),
            ("old".into(), "new=tail".into())
        );
    }

    #[test]
    fn extraction_avoids_duplicate_headers_and_replays_the_same_seed_clock_and_final_timers() {
        for native in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
            if native {
                config.bridge = crate::config::BridgeConfig::Aether {
                    node: "bridge".into(),
                    logical_address: 0,
                };
            }
            let availability_topic = if native {
                "aether/bridge/status"
            } else {
                "tele/tasmota_cec/LWT"
            };
            let availability_payload = if native {
                json!({"online":true,"v":1,"boot":1})
            } else {
                json!("Online")
            };
            let header = row(0, "choregos/identities", json!([]));
            let availability = row(-5, availability_topic, availability_payload);
            let mut text = row(-10, "choregos/identities", json!([])) + &availability + &header;
            for (index, data) in ["84210004", "870010FA", "4753747265616D6572", "04", "36"]
                .into_iter()
                .enumerate()
            {
                let to = if index < 2 { 15 } else { 0 };
                let payload = if native {
                    json!({"ev":"rx","seq":index+1,"from":4,"to":to,"data":data,"ack":true})
                } else {
                    json!({"HdmiReceived":{"From":4,"To":to,"Data":data}})
                };
                text.push_str(&row(
                    index as i64 + 1,
                    if native {
                        "aether/bridge/bus"
                    } else {
                        "tele/tasmota_cec/SENSOR"
                    },
                    payload,
                ));
            }
            let fixture = directory.path().join("full.jsonl");
            let config_path = directory.path().join("config.toml");
            std::fs::write(&fixture, text).unwrap();
            std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
            let mut extracted = vec![];
            extract_files(
                std::slice::from_ref(&fixture),
                timestamp(0),
                timestamp(6),
                &[],
                None,
                &mut extracted,
            )
            .unwrap();
            let extracted = String::from_utf8(extracted).unwrap();
            assert!(extracted.starts_with(&(header + &availability)));
            assert_eq!(
                extracted
                    .lines()
                    .filter(|line| line.contains("choregos/identities"))
                    .count(),
                1
            );
            let mut window_output = vec![];
            replay_files(
                &config_path,
                &[fixture],
                Some(timestamp(0)),
                Some(timestamp(6)),
                &mut window_output,
            )
            .unwrap();
            let mut extracted_output = vec![];
            replay(config, &extracted, &mut extracted_output).unwrap();
            assert_eq!(window_output, extracted_output);
            assert!(
                String::from_utf8(window_output)
                    .unwrap()
                    .contains("off_timer_expired")
            );
        }
    }

    #[test]
    fn atomic_extraction_preserves_existing_files_on_validation_and_midstream_errors() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.jsonl");
        let output = directory.path().join("output.jsonl");
        let valid = row(0, "choregos/connection", json!({"mqtt":true}));
        std::fs::write(&input, &valid).unwrap();
        std::fs::write(&output, "keep me").unwrap();
        let inputs = std::slice::from_ref(&input);
        assert!(extract_to_path(inputs, timestamp(2), timestamp(1), &[], None, &output).is_err());
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "keep me");
        assert!(
            extract_to_path(
                inputs,
                timestamp(0),
                timestamp(2),
                &[],
                None,
                &directory.path().join(".").join("input.jsonl")
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&input).unwrap(), valid);
        let alias = directory.path().join("alias.jsonl");
        std::os::unix::fs::symlink(&input, &alias).unwrap();
        assert!(extract_to_path(inputs, timestamp(0), timestamp(2), &[], None, &alias).is_err());
        assert_eq!(std::fs::read_to_string(&input).unwrap(), valid);
        std::fs::write(
            &input,
            valid.clone() + &row(1, "choregos/command", json!({"kind":"not_a_command"})),
        )
        .unwrap();
        assert!(extract_to_path(inputs, timestamp(0), timestamp(2), &[], None, &output).is_err());
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "keep me");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 3);
        std::fs::write(&input, &valid).unwrap();
        extract_to_path(inputs, timestamp(0), timestamp(2), &[], None, &output).unwrap();
        assert_eq!(std::fs::read_to_string(&output).unwrap(), valid);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 3);
    }

    #[test]
    fn node_parser_rejects_empty_segments_and_topic_wildcards() {
        for node in ["", "a/b", "a+", "#"] {
            assert!(parse_node(node).is_err());
        }
        assert_eq!(parse_node("cec-bridge").unwrap(), "cec-bridge");
    }

    #[test]
    fn replay_timestamps_input_decisions_and_final_timer_deadlines() {
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config.mode = crate::state::Mode::Active;
        let text: String = ["84210004", "870010FA", "4753747265616D6572", "04", "36"]
            .into_iter().enumerate().map(|(seconds, data)| {
                row(seconds as i64, "tele/tasmota_cec/SENSOR",
                    json!({"HdmiReceived":{"From":4,"To":if seconds < 2 {15} else {0},"Data":data}}))
            }).collect();
        let mut output = vec![];
        replay(config, &text, &mut output).unwrap();
        let decisions: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!decisions.is_empty());
        for decision in &decisions {
            assert!(
                decision["at"]
                    .as_str()
                    .unwrap()
                    .parse::<chrono::DateTime<chrono::Utc>>()
                    .is_ok()
            );
        }
        // The final input starts a three-second release timer.
        let release = decisions
            .iter()
            .find(|decision| decision["reason"] == "off_timer_expired")
            .unwrap();
        let standby = decisions
            .iter()
            .find(|decision| decision["reason"] == "Standby")
            .unwrap();
        let at = |value: &Value| {
            value["at"]
                .as_str()
                .unwrap()
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap()
        };
        assert_eq!(at(release) - at(standby), chrono::Duration::seconds(3));
        assert_eq!(at(standby), timestamp(4));
        assert_eq!(at(release), timestamp(7));
    }
}
