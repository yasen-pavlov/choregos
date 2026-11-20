//! Offline checks of deadline conversion, command validation and durable identity rows.
use super::{PersistedState, Runtime, cli, trace, update_snapshot, validate_command};
use crate::{
    cec::Frame,
    config::Config,
    engine::{Engine, Event},
    state::{AppState, Command, Mode, UiEvent},
};
use chrono::{DateTime, Utc};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

fn setup() -> (Engine, Instant, DateTime<Utc>) {
    let now = Instant::now();
    let wall = DateTime::<Utc>::UNIX_EPOCH;
    let config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
    let mut engine = Engine::new(config, now - Duration::from_secs(5), vec![], 1);
    engine.handle(Event::BridgeOnline, now - Duration::from_secs(5), wall);
    (engine, now, wall)
}

#[test]
fn snapshot_release_deadline_is_forward_remaining_time_and_saturates() {
    let (mut engine, now, wall) = setup();
    engine.handle(Event::Tick, now, wall);
    engine.handle(Event::Command(Command::AllOff), now, wall);
    let state = AppState::new((*engine.config).clone(), String::new());
    let mut snapshot = state.snapshot();
    update_snapshot(
        &mut snapshot,
        &engine,
        now + Duration::from_secs(1),
        wall + chrono::Duration::seconds(20),
    );
    assert_eq!(
        snapshot.off_deadline,
        Some(wall + chrono::Duration::seconds(22))
    );
    update_snapshot(&mut snapshot, &engine, now, DateTime::<Utc>::MAX_UTC);
    assert_eq!(snapshot.off_deadline, Some(DateTime::<Utc>::MAX_UTC));
    update_snapshot(&mut snapshot, &engine, now + Duration::from_secs(10), wall);
    assert_eq!(
        snapshot.off_deadline,
        Some(wall),
        "overdue deadlines clamp forward to zero"
    );
    engine.handle(Event::Tick, now + Duration::from_secs(10), wall);
    update_snapshot(&mut snapshot, &engine, now + Duration::from_secs(10), wall);
    assert_eq!(snapshot.off_deadline, None);
}

#[test]
fn resync_validation_rejects_offline_and_only_the_open_five_second_window() {
    let (mut engine, now, wall) = setup();
    assert!(validate_command(&Command::Resync, &engine, now).is_ok());
    engine.handle(Event::Command(Command::Resync), now, wall);
    let error =
        validate_command(&Command::Resync, &engine, now + Duration::from_millis(4900)).unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid command: a resync is already running"
    );
    let boundary = now + Duration::from_secs(5);
    assert!(validate_command(&Command::Resync, &engine, boundary).is_ok());
    assert_eq!(
        engine.handle(Event::Command(Command::Resync), boundary, wall),
        [crate::action::Action::Resync]
    );
    assert_eq!(engine.decisions[0].reason, "command_resync");
    engine.handle(
        Event::BridgeOffline,
        boundary + Duration::from_secs(1),
        wall,
    );
    let error =
        validate_command(&Command::Resync, &engine, boundary + Duration::from_secs(1)).unwrap_err();
    assert_eq!(error.to_string(), "invalid command: bridge is offline");
    assert_eq!(
        serde_json::to_value(Command::Resync).unwrap(),
        serde_json::json!({"kind":"resync"})
    );
}

#[tokio::test]
async fn identity_and_resync_rows_survive_ring_state_trace_and_extraction_in_shadow() {
    let (mut engine, now, wall) = setup();
    std::sync::Arc::make_mut(&mut engine.config).mode = Mode::Shadow;
    engine.handle(
        Event::Frame(Frame::from_hex(12, 15, "84600004").unwrap()),
        now,
        wall,
    );
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::new((*engine.config).clone(), String::new());
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let mut runtime = Runtime {
        snapshot: state.snapshot(),
        state,
        path: directory.path().join("choregos.toml"),
        tx,
        engine,
        adapters: None,
        last_states: String::new(),
        removed_roles: BTreeSet::new(),
        writer: None,
        trace: Some(trace::TraceWriter::start(
            directory.path().to_path_buf(),
            30,
            wall,
            vec![],
        )),
        last_identities: None,
        decisions_dirty: false,
    };
    runtime.engine.handle(
        Event::Frame(Frame::from_hex(12, 15, "84610004").unwrap()),
        now,
        wall,
    );
    let detail = runtime.engine.decisions[0].identity.clone().unwrap();
    let mut events = vec![];
    runtime.record_decisions(wall, &mut events);
    assert!(
        matches!(&events[..], [UiEvent::Decision(row)] if row.identity.as_ref() == Some(&detail) && !row.would)
    );
    runtime
        .engine
        .handle(Event::Command(Command::Resync), now, wall);
    runtime.record_decisions(wall, &mut events);
    assert_eq!(runtime.snapshot.decisions.len(), 2);
    assert!(runtime.snapshot.decisions.iter().all(|row| !row.would));
    assert_eq!(runtime.snapshot.decisions[1].reason, "command_resync");
    let saved = PersistedState::capture(&runtime.engine, &runtime.snapshot.decisions);
    let path = directory.path().join("choregos.state.json");
    tokio::fs::write(&path, serde_json::to_vec(&saved).unwrap())
        .await
        .unwrap();
    let loaded = PersistedState::load(&path, wall).await.unwrap();
    assert_eq!(loaded.decisions, saved.decisions);
    assert_eq!(loaded.decisions[0].identity.as_ref(), Some(&detail));
    runtime.trace.take().unwrap().flush().await;
    let trace_path = trace::day_file(directory.path(), wall.date_naive());
    let text = std::fs::read_to_string(&trace_path).unwrap();
    let decisions: Vec<_> = text
        .lines()
        .filter_map(|line| match trace::parse(line)?.1 {
            trace::TraceEntry::Decision(row) => Some(row),
            _ => None,
        })
        .collect();
    assert_eq!(decisions.len(), 2);
    assert_eq!(
        decisions[0]["identity"],
        serde_json::to_value(detail).unwrap()
    );
    assert_eq!(decisions[0]["would"], false);
    assert_eq!(decisions[1]["would"], false);
    let mut extracted = vec![];
    cli::extract_files(
        &[trace_path],
        wall,
        wall + chrono::Duration::seconds(1),
        &[],
        None,
        &mut extracted,
    )
    .unwrap();
    let extracted = String::from_utf8(extracted).unwrap();
    assert!(extracted.lines().filter_map(trace::parse).any(
        |(_, entry)| matches!(entry, trace::TraceEntry::Decision(row) if row == decisions[0])
    ));
}
