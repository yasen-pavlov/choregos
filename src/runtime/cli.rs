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
    // Pre-scan only supplies a fallback; headers encountered before the first
    // in-window event override it with the most recent restored seed.
    'seed: for fixture in fixtures {
        use std::io::BufRead;
        for (index, line) in std::io::BufReader::new(std::fs::File::open(fixture)?)
            .lines()
            .enumerate()
        {
            let line = line.with_context(|| format!("{}:{}", fixture.display(), index + 1))?;
            if let Some(identities) = identity_header(&line) {
                replay.fallback_identities = Some(identities);
                break 'seed;
            }
        }
    }
    for fixture in fixtures {
        use std::io::BufRead;
        for (index, line) in std::io::BufReader::new(std::fs::File::open(fixture)?)
            .lines()
            .enumerate()
        {
            let location = || format!("{}:{}", fixture.display(), index + 1);
            let line = line.with_context(location)?;
            match replay.line(&line, output) {
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
    replay.finish(output)
}

pub fn replay(config: Config, text: &str, output: &mut impl Write) -> anyhow::Result<()> {
    let mut replay = Replay::new(config, None, None)?;
    replay.fallback_identities = text.lines().find_map(identity_header);
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

struct Replay {
    config: Option<Config>,
    engine: Option<Engine>,
    start: Instant,
    epoch: Option<chrono::DateTime<chrono::Utc>>,
    last: Option<(Instant, chrono::DateTime<chrono::Utc>)>,
    previous_ts: Option<chrono::DateTime<chrono::Utc>>,
    identities: Option<Vec<crate::cec::bus::LearnedIdentity>>,
    fallback_identities: Option<Vec<crate::cec::bus::LearnedIdentity>>,
    correlation: Correlation,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
}

impl Replay {
    fn new(
        config: Config,
        since: Option<chrono::DateTime<chrono::Utc>>,
        until: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            since.zip(until).is_none_or(|(a, b)| a <= b),
            "--since must not be after --until"
        );
        Ok(Self {
            config: Some(config),
            engine: None,
            start: Instant::now(),
            epoch: None,
            last: None,
            previous_ts: None,
            identities: None,
            fallback_identities: None,
            correlation: Correlation::default(),
            since,
            until,
        })
    }

    fn line(&mut self, line: &str, output: &mut impl Write) -> anyhow::Result<()> {
        let Some((timestamp, entry)) = trace::parse_checked(line)? else {
            return Ok(());
        };
        if let TraceEntry::Identities(identities) = entry {
            if self.engine.is_none() {
                self.identities = Some(identities);
            }
            return Ok(());
        }
        if self
            .previous_ts
            .is_some_and(|previous| timestamp < previous)
        {
            return Err(TraceOrderError.into());
        }
        self.previous_ts = Some(timestamp);
        if self.since.is_some_and(|since| timestamp < since)
            || self.until.is_some_and(|until| timestamp > until)
        {
            return Ok(());
        }
        let epoch = *self.epoch.get_or_insert(timestamp);
        let now = self.start + (timestamp - epoch).to_std()?;
        if self.engine.is_none() {
            self.engine = Some(Engine::new(
                self.config.take().expect("replay config available"),
                self.start,
                self.identities
                    .take()
                    .or_else(|| self.fallback_identities.take())
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
            let end = self
                .until
                .map_or(wall + chrono::Duration::seconds(5), |until| {
                    until.min(wall + chrono::Duration::seconds(5))
                });
            let duration = (end - wall).to_std()?;
            emit(
                self.engine.as_mut().expect("replay initialized"),
                Event::Tick,
                now + duration,
                end,
                output,
            )?;
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
        writeln!(output, "{}", serde_json::to_string(decision)?)?;
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
        emit(
            &mut engine,
            Event::Tick,
            start + Duration::from_millis(6004),
            epoch + chrono::Duration::milliseconds(6004),
            &mut live,
        )
        .unwrap();
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
            assert_eq!(engine.bridge_online, include_status && since.is_none());
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
