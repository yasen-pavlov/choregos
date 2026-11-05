use choregos::{
    action::{Action, Expectation, IdentifyQueries, Outcome, TimerKind, Trigger, Verdict, Via},
    cec::Frame,
    config::{Config, Takeover},
    engine::{Engine, Event},
    state::{BusPower, Command, Mode, RolePower, RoomIntent, RoomPower, TvState},
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

fn wall() -> DateTime<Utc> {
    "2026-10-25T12:34:18Z".parse().unwrap()
}
fn setup() -> (Engine, Instant) {
    let now = Instant::now();
    let mut engine = Engine::new(
        Config::parse(include_str!("../choregos.example.toml")).unwrap(),
        now,
        vec![],
        1,
    );
    engine.handle(Event::BridgeOnline, now, wall());
    (engine, now)
}
fn frame(engine: &mut Engine, now: Instant, from: u8, to: u8, data: &str) -> Vec<Action> {
    engine.handle(
        Event::Frame(Frame::from_hex(from, to, data).unwrap()),
        now,
        wall(),
    )
}
fn identify_atv(engine: &mut Engine, now: Instant) {
    frame(engine, now, 4, 15, "84210004");
    frame(engine, now, 4, 15, "870010FA");
    frame(engine, now, 4, 0, "474170706C65205456");
}
fn service(action: &Action, wanted: &str) -> bool {
    matches!(action, Action::HaCall { service, .. } if service == wanted)
}
fn ha(
    engine: &mut Engine,
    now: Instant,
    entity: String,
    state: &str,
    changed: Option<DateTime<Utc>>,
) {
    engine.handle(
        Event::HaState {
            entity,
            state: state.into(),
            attrs: BTreeMap::new(),
            changed,
            updated: None,
        },
        now,
        wall(),
    );
}

#[test]
fn capture_replays_power_release_and_both_volume_branches() {
    for sonos_playing in [false, true] {
        let (mut engine, start) = setup();
        // Capture starts with Kodi going off before its identifying replies. Seed the
        // pre-capture identity/on state explicitly; do not rewrite the real fixture.
        frame(&mut engine, start, 1, 15, "84220001");
        frame(&mut engine, start, 1, 0, "474B6F6469");
        frame(&mut engine, start, 1, 15, "822200");
        // Finish pre-capture takeover before collecting capture decisions.
        engine.handle(Event::Tick, start + Duration::from_secs(2), wall());
        let start = start + Duration::from_secs(3);
        let sonos = engine.config.sonos.entity.clone();
        ha(
            &mut engine,
            start,
            sonos,
            if sonos_playing { "playing" } else { "paused" },
            None,
        );
        let mut transitions = vec![];
        let mut effects = vec![];
        let mut reasons = vec![];
        let epoch: DateTime<Utc> = "2026-10-25T12:34:18Z".parse().unwrap();
        let mut last = start;
        let mut presses = 0;
        for line in include_str!("../fixtures/capture-streamer-wake.jsonl").lines() {
            let value: Value = serde_json::from_str(line).unwrap();
            let wall: DateTime<Utc> = value["ts"].as_str().unwrap().parse().unwrap();
            let now = start + (wall - epoch).to_std().unwrap();
            last = now;
            while let Some(deadline) = engine.next_deadline().filter(|d| *d < now) {
                effects.extend(engine.handle(
                    Event::Tick,
                    deadline,
                    epoch + chrono::Duration::from_std(deadline.duration_since(start)).unwrap(),
                ));
                reasons.extend(engine.decisions.iter().map(|d| d.reason.clone()));
            }
            let event = if value["topic"].as_str().unwrap().ends_with("/LWT") {
                Some(Event::BridgeOnline)
            } else {
                Frame::parse(&serde_json::to_vec(&value["payload"]).unwrap())
                    .ok()
                    .map(Event::Frame)
            };
            if let Some(event) = event {
                if matches!(
                    &event,
                    Event::Frame(Frame {
                        opcode: Some(0x44),
                        ..
                    })
                ) {
                    presses += 1;
                }
                effects.extend(engine.handle(event, now, wall));
                transitions.extend(
                    engine
                        .decisions
                        .iter()
                        .filter_map(|d| d.to.map(|to| (d.role.clone().unwrap(), to))),
                );
                reasons.extend(engine.decisions.iter().map(|d| d.reason.clone()));
            }
        }
        assert_eq!(
            transitions,
            [
                ("kodi".into(), RolePower::Off),
                ("streamer".into(), RolePower::On),
                ("streamer".into(), RolePower::Off)
            ]
        );
        assert!(reasons.iter().any(|r| r == "off_timer_expired"));
        let wake = effects.iter().position(|a| *a == Action::WakeTv).unwrap();
        let receiver_on = effects
            .iter()
            .position(|a| {
                service(a, "turn_on")
                    && matches!(a, Action::HaCall { domain, .. } if domain == "media_player")
            })
            .unwrap();
        let select = effects
            .iter()
            .position(|a| service(a, "select_source"))
            .unwrap();
        let preset = effects
            .iter()
            .position(|a| matches!(a, Action::HaCall { domain, .. } if domain == "script"))
            .unwrap();
        let off = effects
            .iter()
            .enumerate()
            .find(|(index, action)| *index > preset && **action == Action::TvOff)
            .map(|(index, _)| index)
            .unwrap();
        assert!(effects[..wake].contains(&Action::TvOff));
        assert!(wake < receiver_on && receiver_on < select && select < preset && preset < off);
        assert_eq!(effects.iter().filter(|a| **a == Action::TvOff).count(), 2);
        assert_eq!(effects.iter().filter(|a| service(a, "turn_off")).count(), 2); // Each release powers off the TV natively and the receiver through HA.
        assert!(effects.iter().filter(|a| service(a, "turn_off")).all(
            |a| matches!(a, Action::HaCall { entity, .. } if entity == &engine.config.receiver.entity)
        ));
        let volume_count = effects
            .iter()
            .filter(|a| service(a, "volume_up") || service(a, "volume_down"))
            .count();
        assert_eq!(volume_count, if sonos_playing { presses } else { 0 });
        // This capture ends with volume frames, not a second wake. Exercise the
        // wake-again transition with an explicit additional bus event.
        let wake_again = frame(&mut engine, last + Duration::from_secs(1), 4, 0, "04");
        assert!(wake_again.contains(&Action::WakeTv));
        assert_eq!(engine.roles["streamer"].power, RolePower::On);
    }
}

#[test]
fn takeover_is_ordered_idempotent_and_settled() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 1, 0, "474B6F6469");
    frame(&mut engine, now, 1, 15, "822200");
    let sonos = engine.config.sonos.entity.clone();
    ha(&mut engine, now, sonos, "playing", None);
    let actions = frame(&mut engine, now, 4, 0, "04");
    assert!(matches!(actions[0], Action::WakeTv));
    assert!(service(&actions[1], "media_stop")); // Kodi first
    assert!(service(&actions[2], "media_stop")); // Sonos next
    assert!(service(&actions[3], "turn_on"));
    assert!(frame(&mut engine, now, 4, 15, "822100").is_empty());
    assert!(
        engine
            .handle(Event::Tick, now + Duration::from_millis(1999), wall())
            .iter()
            .all(|action| matches!(action, Action::Identify { .. }))
    );
    let settled = engine.handle(Event::Tick, now + Duration::from_millis(2000), wall());
    assert!(service(&settled[0], "select_source"));
    assert!(service(&settled[1], "turn_on"));
}

fn takeover_from_streamer() -> (Engine, Instant, Duration) {
    takeover_from_streamer_in_mode(Mode::Active)
}

fn takeover_from_streamer_in_mode(mode: Mode) -> (Engine, Instant, Duration) {
    takeover_from_streamer_with_policy(mode, Takeover::CecStandby)
}

fn streamer_stop_actions(policy: Takeover) -> Vec<Action> {
    let data = if policy == Takeover::CecPauseStandby {
        vec![vec![0x41, 0x25], vec![0x36], vec![0x8f]]
    } else {
        vec![vec![0x36], vec![0x8f]]
    };
    data.into_iter()
        .map(|data| Action::CecSend { to: 4, data })
        .collect()
}

fn takeover_from_streamer_with_policy(mode: Mode, policy: Takeover) -> (Engine, Instant, Duration) {
    let (mut engine, now) = setup();
    std::sync::Arc::make_mut(&mut engine.config).device[0].takeover = policy;
    engine.handle(Event::Command(Command::SetMode(mode)), now, wall());
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 0, "04");
    let takeover = now + Duration::from_secs(1);
    let actions = frame(&mut engine, takeover, 8, 15, "84230004");
    let sent: Vec<_> = actions
        .into_iter()
        .filter(|action| matches!(action, Action::CecSend { .. }))
        .collect();
    assert_eq!(sent, streamer_stop_actions(policy));
    assert_eq!(engine.roles["streamer"].power, RolePower::Off);
    let settle = Duration::from_millis(engine.config.timing.takeover_settle_ms);
    (engine, takeover, settle)
}

fn assert_no_standby_decision(engine: &Engine, role: &str) {
    assert!(!engine.decisions.iter().any(|decision| {
        decision.role.as_deref() == Some(role) && decision.reason.starts_with("standby_")
    }));
}

fn standby_expectation<'a>(engine: &'a Engine, role: &str) -> &'a Expectation {
    (1..engine.next_decision_id)
        .rev()
        .filter_map(|id| engine.expectations(id))
        .flat_map(|expectations| expectations.iter())
        .find(|expectation| expectation.subject.strip_prefix("role:") == Some(role))
        .expect("standby expectation")
}

fn assert_standby_unknown(engine: &Engine, role: &str, absent: bool) {
    assert_eq!(
        standby_expectation(engine, role).outcome,
        Outcome::Unknown {
            observed: absent.then(|| "absent".into()),
        }
    );
}

#[test]
fn takeover_retries_unconfirmed_standby_in_order_then_expires() {
    let (mut engine, takeover, settle) = takeover_from_streamer();
    assert_eq!(engine.next_deadline(), Some(takeover + settle));
    let before = takeover + settle - Duration::from_millis(1);
    engine.handle(Event::Tick, before, wall());
    assert_no_standby_decision(&engine, "streamer");
    let expected = vec![
        Action::CecSend {
            to: 4,
            data: vec![0x36],
        },
        Action::CecSend {
            to: 4,
            data: vec![0x8f],
        },
    ];
    let retries = engine.config.timing.standby_retries;
    for attempt in 1..=retries {
        let now = takeover + settle * u32::from(attempt);
        let actions = engine.handle(Event::Tick, now, wall());
        assert_eq!(standby_expectation(&engine, "streamer").attempts, attempt);
        assert!(actions.windows(2).any(|pair| pair == expected));
        if attempt == 1 {
            assert_eq!(engine.next_deadline(), Some(now + settle));
        }
    }
    let exhausted = takeover + settle * (u32::from(retries) + 1);
    engine.handle(Event::Tick, exhausted, wall());
    assert_standby_unknown(&engine, "streamer", false);
    engine.handle(Event::Tick, exhausted + settle, wall());
    assert_no_standby_decision(&engine, "streamer");
}

fn stop_non_verifying_console(policy: Takeover, all_off: bool) -> (Engine, Instant, Duration) {
    let (mut engine, now) = setup();
    let config = std::sync::Arc::make_mut(&mut engine.config);
    config.mode = Mode::Active;
    let console = config
        .device
        .iter_mut()
        .find(|device| device.role == "console_a")
        .unwrap();
    console.takeover = policy;
    assert!(!console.verify_standby);
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 8, 15, "84230004");
    frame(&mut engine, now, 8, 15, "822300");
    assert_eq!(engine.roles["console_a"].power, RolePower::On);
    let takeover = now + Duration::from_secs(1);
    let (actions, reason) = if all_off {
        (
            engine.handle(Event::Command(Command::AllOff), takeover, wall()),
            "all_off",
        )
    } else {
        (frame(&mut engine, takeover, 4, 0, "04"), "taken_over")
    };
    let sent: Vec<_> = actions
        .into_iter()
        .filter(|action| matches!(action, Action::CecSend { to: 8, .. }))
        .collect();
    let mut expected = vec![];
    if policy == Takeover::CecPauseStandby {
        expected.push(Action::CecSend {
            to: 8,
            data: vec![0x41, 0x25],
        });
    }
    expected.extend([
        Action::CecSend {
            to: 8,
            data: vec![0x36],
        },
        Action::CecSend {
            to: 8,
            data: vec![0x8f],
        },
    ]);
    assert_eq!(sent, expected);
    assert_eq!(engine.roles["console_a"].power, RolePower::Off);
    assert!(engine.decisions.iter().any(|decision| {
        decision.role.as_deref() == Some("console_a") && decision.reason == reason
    }));
    assert_no_standby_decision(&engine, "console_a");
    let settle = Duration::from_millis(engine.config.timing.takeover_settle_ms);
    (engine, takeover, settle)
}

#[test]
fn disabled_standby_verification_sends_each_sequence_once_without_retry_or_expiry() {
    for policy in [Takeover::CecStandby, Takeover::CecPauseStandby] {
        let (mut engine, takeover, settle) = stop_non_verifying_console(policy, false);
        for data in [vec![0x41, 0x25], vec![0x36], vec![0x8f]] {
            engine.handle(
                Event::SendResult {
                    to: 8,
                    data,
                    status: choregos::engine::SendStatus::Nack,
                    attempts: None,
                },
                takeover + Duration::from_millis(1),
                wall(),
            );
            assert_no_standby_decision(&engine, "console_a");
        }
        for attempt in 1..=u32::from(engine.config.timing.standby_retries) + 2 {
            let actions = engine.handle(Event::Tick, takeover + settle * attempt, wall());
            assert!(
                !actions
                    .iter()
                    .any(|action| matches!(action, Action::CecSend { to: 8, .. }))
            );
            assert_no_standby_decision(&engine, "console_a");
            assert_eq!(engine.roles["console_a"].power, RolePower::Off);
        }
    }
}

#[test]
fn all_off_sends_non_verifying_console_sequence_once_without_retries() {
    for policy in [Takeover::CecStandby, Takeover::CecPauseStandby] {
        let (mut engine, stopped, settle) = stop_non_verifying_console(policy, true);
        for attempt in 1..=u32::from(engine.config.timing.standby_retries) + 2 {
            let actions = engine.handle(Event::Tick, stopped + settle * attempt, wall());
            assert!(
                !actions
                    .iter()
                    .any(|action| matches!(action, Action::CecSend { to: 8, .. }))
            );
            assert_no_standby_decision(&engine, "console_a");
            assert_eq!(engine.roles["console_a"].power, RolePower::Off);
        }
    }
}

fn exhaust_standby_with_results(results: &[(u8, u8, bool)]) -> Engine {
    let (mut engine, takeover, settle) = takeover_from_streamer();
    for attempt in 0..=engine.config.timing.standby_retries {
        let sent_at = takeover + settle * u32::from(attempt) + Duration::from_millis(1);
        for &(to, opcode, ack) in results {
            engine.handle(
                Event::SendResult {
                    to,
                    data: vec![opcode],
                    status: if ack {
                        choregos::engine::SendStatus::Ack
                    } else {
                        choregos::engine::SendStatus::Nack
                    },
                    attempts: None,
                },
                sent_at,
                wall(),
            );
            assert_no_standby_decision(&engine, "streamer");
        }
        engine.handle(
            Event::Tick,
            takeover + settle * (u32::from(attempt) + 1),
            wall(),
        );
        if attempt < engine.config.timing.standby_retries {
            assert_eq!(
                standby_expectation(&engine, "streamer").attempts,
                attempt + 1
            );
        }
    }
    engine
}

#[test]
fn all_nacked_standby_frames_record_target_absent_after_bounded_retries() {
    let engine = exhaust_standby_with_results(&[(4, 0x36, false), (4, 0x8f, false)]);
    assert_standby_unknown(&engine, "streamer", true);
    assert_eq!(engine.bus.entries[&4].power, BusPower::On);
}

#[test]
fn mixed_ack_standby_frames_without_power_report_remain_unconfirmed() {
    let engine = exhaust_standby_with_results(&[(4, 0x36, false), (4, 0x8f, true)]);
    assert_standby_unknown(&engine, "streamer", false);
}

#[test]
fn send_results_for_other_las_and_opcodes_do_not_count_as_standby_nacks() {
    let engine =
        exhaust_standby_with_results(&[(8, 0x36, false), (8, 0x8f, false), (4, 0x83, false)]);
    assert_standby_unknown(&engine, "streamer", false);
}

#[test]
fn missing_standby_send_results_do_not_imply_absence() {
    let engine = exhaust_standby_with_results(&[(4, 0x36, false)]);
    assert_standby_unknown(&engine, "streamer", false);
}

#[test]
fn repeated_standby_with_stale_nacks_still_records_target_absent() {
    let (mut engine, takeover, settle) = takeover_from_streamer();
    std::sync::Arc::make_mut(&mut engine.config)
        .timing
        .standby_retries = 0;
    let stopped = takeover + Duration::from_millis(1);
    engine.handle(
        Event::Command(Command::Standby("streamer".into())),
        stopped,
        wall(),
    );
    // Both the replaced verification's pair and the new pair finish after replacement.
    for opcode in [0x36, 0x8f, 0x36, 0x8f] {
        engine.handle(
            Event::SendResult {
                to: 4,
                data: vec![opcode],
                status: choregos::engine::SendStatus::Nack,
                attempts: None,
            },
            stopped,
            wall(),
        );
    }
    engine.handle(Event::Tick, stopped + settle, wall());
    assert_standby_unknown(&engine, "streamer", true);
}

#[test]
fn resync_nacks_during_verification_never_override_an_observed_ack() {
    // The extra 8F NACK belongs to resync, not to the pending standby pair.
    let engine =
        exhaust_standby_with_results(&[(4, 0x36, true), (4, 0x8f, false), (4, 0x8f, false)]);
    assert_standby_unknown(&engine, "streamer", false);
}

#[test]
fn raw_cec_data_accepts_fourteen_bytes_but_rejects_fifteen() {
    let (mut engine, now) = setup();
    for length in [14, 15] {
        let data = vec![0x44; length];
        let actions = engine.handle(
            Event::Command(Command::SendRaw {
                to: 4,
                data: data.clone(),
            }),
            now,
            wall(),
        );
        assert_eq!(
            actions.contains(&Action::CecSend { to: 4, data }),
            length == 14
        );
    }
}

#[test]
fn standby_and_transition_reports_cancel_takeover_verification() {
    for report in ["9001", "9002", "9003", "9D2100", "36"] {
        let (mut engine, takeover, settle) = takeover_from_streamer();
        frame(
            &mut engine,
            takeover + settle - Duration::from_millis(1),
            4,
            0,
            report,
        );
        assert!(matches!(
            engine.bus.entries[&4].power,
            BusPower::Standby | BusPower::Transition
        ));
        for interval in 1..=4 {
            let actions = engine.handle(Event::Tick, takeover + settle * interval, wall());
            assert!(!actions.contains(&Action::CecSend {
                to: 4,
                data: vec![0x36],
            }));
            assert_no_standby_decision(&engine, "streamer");
        }
    }
}

#[test]
fn rewoken_role_cancels_takeover_standby_verification() {
    let (mut engine, takeover, settle) = takeover_from_streamer();
    frame(
        &mut engine,
        takeover + settle - Duration::from_millis(1),
        4,
        0,
        "04",
    );
    assert_eq!(engine.roles["streamer"].power, RolePower::On);
    let actions = engine.handle(Event::Tick, takeover + settle, wall());
    assert!(!actions.contains(&Action::CecSend {
        to: 4,
        data: vec![0x36],
    }));
    assert_no_standby_decision(&engine, "streamer");
}

#[test]
fn pause_standby_config_parses() {
    let text = include_str!("../choregos.example.toml").replacen(
        "takeover = \"cec_standby\"",
        "takeover = \"cec_pause_standby\"",
        1,
    );
    let config = Config::parse(&text).unwrap();
    assert_eq!(config.device[0].takeover, Takeover::CecPauseStandby);
}

#[test]
fn pause_standby_takeover_retries_the_entire_directed_sequence() {
    let (mut engine, takeover, settle) =
        takeover_from_streamer_with_policy(Mode::Active, Takeover::CecPauseStandby);
    assert_eq!(engine.next_deadline(), Some(takeover + settle));
    let actions = engine.handle(Event::Tick, takeover + settle, wall());
    let sent: Vec<_> = actions
        .into_iter()
        .filter(|action| matches!(action, Action::CecSend { .. }))
        .collect();
    assert_eq!(sent, streamer_stop_actions(Takeover::CecPauseStandby));
    assert_eq!(standby_expectation(&engine, "streamer").attempts, 1);
}

#[test]
fn pause_standby_observed_standby_confirms_despite_nacked_trailing_query() {
    for report in ["9D2100", "9001"] {
        let (mut engine, takeover, settle) =
            takeover_from_streamer_with_policy(Mode::Active, Takeover::CecPauseStandby);
        engine.handle(
            Event::SendResult {
                to: 4,
                data: vec![0x36],
                status: choregos::engine::SendStatus::Ack,
                attempts: None,
            },
            takeover + Duration::from_millis(10),
            wall(),
        );
        frame(
            &mut engine,
            takeover + Duration::from_millis(20),
            4,
            0,
            report,
        );
        engine.handle(
            Event::SendResult {
                to: 4,
                data: vec![0x8f],
                status: choregos::engine::SendStatus::Nack,
                attempts: None,
            },
            takeover + Duration::from_millis(30),
            wall(),
        );
        assert_eq!(engine.bus.entries[&4].power, BusPower::Standby);
        for interval in 1..=4 {
            let actions = engine.handle(Event::Tick, takeover + settle * interval, wall());
            assert!(
                !actions
                    .iter()
                    .any(|action| matches!(action, Action::CecSend { to: 4, .. }))
            );
            assert_no_standby_decision(&engine, "streamer");
            assert_eq!(engine.roles["streamer"].power, RolePower::Off);
            assert_eq!(engine.roles["console_a"].power, RolePower::On);
            assert_eq!(engine.intent.active_role.as_deref(), Some("console_a"));
        }
    }
}

#[test]
fn pause_standby_rewake_signals_cancel_verification() {
    for (to, data) in [(0, "04"), (0, "0D"), (15, "822100")] {
        let (mut engine, takeover, settle) =
            takeover_from_streamer_with_policy(Mode::Active, Takeover::CecPauseStandby);
        frame(
            &mut engine,
            takeover + Duration::from_millis(100),
            4,
            to,
            data,
        );
        assert_eq!(engine.bus.entries[&4].power, BusPower::On);
        assert_eq!(engine.roles["streamer"].power, RolePower::On);
        assert_eq!(engine.intent.active_role.as_deref(), Some("streamer"));
        for interval in 1..=4 {
            let actions = engine.handle(Event::Tick, takeover + settle * interval, wall());
            assert!(
                !actions
                    .iter()
                    .any(|action| matches!(action, Action::CecSend { to: 4, .. }))
            );
            assert_no_standby_decision(&engine, "streamer");
        }
    }
}

#[test]
fn pause_standby_nack_accounting_counts_all_three_frames_per_round() {
    for missing in [None, Some(0x41), Some(0x36), Some(0x8f)] {
        let (mut engine, takeover, settle) =
            takeover_from_streamer_with_policy(Mode::Active, Takeover::CecPauseStandby);
        for round in 0..=engine.config.timing.standby_retries {
            let sent_at = takeover + settle * u32::from(round) + Duration::from_millis(1);
            for action in streamer_stop_actions(Takeover::CecPauseStandby) {
                let Action::CecSend { to, data } = action else {
                    unreachable!()
                };
                if Some(data[0]) != missing {
                    engine.handle(
                        Event::SendResult {
                            to,
                            data,
                            status: choregos::engine::SendStatus::Nack,
                            attempts: None,
                        },
                        sent_at,
                        wall(),
                    );
                    assert_no_standby_decision(&engine, "streamer");
                }
            }
            engine.handle(
                Event::Tick,
                takeover + settle * (u32::from(round) + 1),
                wall(),
            );
        }
        assert_standby_unknown(&engine, "streamer", missing.is_none());
    }
}

#[test]
fn pause_standby_acked_standby_without_power_report_remains_unconfirmed() {
    let (mut engine, takeover, settle) =
        takeover_from_streamer_with_policy(Mode::Active, Takeover::CecPauseStandby);
    for round in 0..=engine.config.timing.standby_retries {
        let sent_at = takeover + settle * u32::from(round) + Duration::from_millis(1);
        for action in streamer_stop_actions(Takeover::CecPauseStandby) {
            let Action::CecSend { to, data } = action else {
                unreachable!()
            };
            engine.handle(
                Event::SendResult {
                    to,
                    status: if data == [0x36] {
                        choregos::engine::SendStatus::Ack
                    } else {
                        choregos::engine::SendStatus::Nack
                    },
                    attempts: None,
                    data,
                },
                sent_at,
                wall(),
            );
        }
        // Even an extra late query NACK cannot override the Standby ACK.
        engine.handle(
            Event::SendResult {
                to: 4,
                data: vec![0x8f],
                status: choregos::engine::SendStatus::Nack,
                attempts: None,
            },
            sent_at,
            wall(),
        );
        engine.handle(
            Event::Tick,
            takeover + settle * (u32::from(round) + 1),
            wall(),
        );
    }
    assert_eq!(engine.bus.entries[&4].power, BusPower::On);
    assert_standby_unknown(&engine, "streamer", false);
}

#[test]
fn all_off_uses_the_same_standby_verification() {
    let (mut engine, now) = setup();
    engine.handle(Event::Command(Command::SetMode(Mode::Active)), now, wall());
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 0, "04");
    let stopped = now + Duration::from_secs(1);
    engine.handle(Event::Command(Command::AllOff), stopped, wall());
    assert_eq!(engine.roles["streamer"].power, RolePower::Off);
    let settle = Duration::from_millis(engine.config.timing.takeover_settle_ms);
    assert_eq!(engine.next_deadline(), Some(stopped + settle));
    let actions = engine.handle(Event::Tick, stopped + settle, wall());
    assert_eq!(
        actions,
        [
            Action::CecSend {
                to: 4,
                data: vec![0x36],
            },
            Action::CecSend {
                to: 4,
                data: vec![0x8f],
            },
        ]
    );
    assert_eq!(standby_expectation(&engine, "streamer").attempts, 1);
}

#[test]
fn bridge_offline_clears_pending_standby_verification() {
    let (mut engine, takeover, settle) = takeover_from_streamer();
    engine.handle(Event::BridgeOffline, takeover, wall());
    engine.handle(Event::Tick, takeover + settle, wall());
    assert_no_standby_decision(&engine, "streamer");
    engine.handle(Event::BridgeOnline, takeover + settle, wall());
    engine.handle(Event::Tick, takeover + settle * 3, wall());
    assert_no_standby_decision(&engine, "streamer");
}

#[test]
fn silent_role_woken_only_by_physical_address_is_not_assumed_in_standby() {
    let (mut engine, now) = setup();
    std::sync::Arc::make_mut(&mut engine.config)
        .device
        .iter_mut()
        .find(|device| device.role == "console_a")
        .unwrap()
        .verify_standby = true;
    engine.handle(Event::Command(Command::SetMode(Mode::Active)), now, wall());
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 8, 15, "84230004");
    assert_eq!(engine.roles["console_a"].power, RolePower::On);
    assert_eq!(engine.bus.entries[&8].power, BusPower::Unknown);
    let takeover = now + Duration::from_secs(1);
    frame(&mut engine, takeover, 4, 0, "04");
    let settle = Duration::from_millis(engine.config.timing.takeover_settle_ms);
    let actions = engine.handle(Event::Tick, takeover + settle, wall());
    assert!(actions.windows(2).any(|pair| {
        pair == [
            Action::CecSend {
                to: 8,
                data: vec![0x36],
            },
            Action::CecSend {
                to: 8,
                data: vec![0x8f],
            },
        ]
    }));
    assert_eq!(standby_expectation(&engine, "console_a").attempts, 1);
}

#[test]
fn standby_verification_cancels_blocked_and_remapped_roles() {
    for blocked in [false, true] {
        let (mut engine, takeover, settle) = takeover_from_streamer();
        if blocked {
            frame(&mut engine, takeover, 11, 15, "84240004");
            frame(&mut engine, takeover, 11, 15, "870010FA");
            assert!(engine.identity.blocked.contains("streamer"));
        } else {
            // A differing physical address removes the old vendor match.
            frame(&mut engine, takeover, 4, 15, "84250004");
            assert!(!engine.identity.roles.contains_key("streamer"));
        }
        let actions = engine.handle(Event::Tick, takeover + settle, wall());
        assert!(!actions.contains(&Action::CecSend {
            to: 4,
            data: vec![0x36],
        }));
        assert_no_standby_decision(&engine, "streamer");
    }
}

#[test]
fn zero_standby_retries_records_unconfirmed_without_resending() {
    let (mut engine, takeover, settle) = takeover_from_streamer();
    let config = std::sync::Arc::make_mut(&mut engine.config);
    config.timing.standby_retries = 0;
    engine.handle(
        Event::Command(Command::Standby("streamer".into())),
        takeover,
        wall(),
    );
    let actions = engine.handle(Event::Tick, takeover + settle, wall());
    assert!(!actions.contains(&Action::CecSend {
        to: 4,
        data: vec![0x36],
    }));
    assert_standby_unknown(&engine, "streamer", false);
}

#[test]
fn new_standby_replaces_pending_verification_for_same_address() {
    let (mut engine, takeover, settle) = takeover_from_streamer();
    let stopped = takeover + settle - Duration::from_millis(1);
    engine.handle(
        Event::Command(Command::Standby("streamer".into())),
        stopped,
        wall(),
    );
    engine.handle(Event::Tick, takeover + settle, wall());
    assert_no_standby_decision(&engine, "streamer");
    engine.handle(Event::Tick, stopped + settle, wall());
    assert_eq!(standby_expectation(&engine, "streamer").attempts, 1);
}

#[test]
fn verification_power_on_reply_never_takes_back_the_room_or_buffers_a_wake() {
    for resync in [false, true] {
        let (mut engine, takeover, settle) = takeover_from_streamer();
        let first_retry = takeover + settle;
        engine.handle(Event::Tick, first_retry, wall());
        if resync {
            engine.handle(
                Event::BridgeOnline,
                first_retry + Duration::from_millis(1),
                wall(),
            );
            assert!(engine.warming);
        }
        let actions = frame(
            &mut engine,
            first_retry + Duration::from_millis(2),
            4,
            0,
            "9000",
        );
        assert!(!engine.warming);
        assert_eq!(engine.bus.entries[&4].power, BusPower::On);
        assert_eq!(engine.roles["streamer"].power, RolePower::Off);
        assert_eq!(engine.roles["console_a"].power, RolePower::On);
        assert_eq!(engine.intent.active_role.as_deref(), Some("console_a"));
        assert!(!actions.contains(&Action::CecSend {
            to: 8,
            data: vec![0x36],
        }));
        // Resync defers missing-key identification by a second; drain that
        // read-only query before checking the next verification deadline.
        if resync {
            engine.handle(
                Event::Tick,
                first_retry + Duration::from_millis(1001),
                wall(),
            );
        }
        assert_eq!(engine.next_deadline(), Some(first_retry + settle));
        let actions = engine.handle(Event::Tick, first_retry + settle, wall());
        assert!(actions.contains(&Action::CecSend {
            to: 4,
            data: vec![0x36],
        }));
        assert_eq!(engine.roles["console_a"].power, RolePower::On);
        assert_eq!(standby_expectation(&engine, "streamer").attempts, 2);
    }
}

#[test]
fn shadow_takeover_does_not_register_standby_verification() {
    let (mut engine, takeover, settle) = takeover_from_streamer_in_mode(Mode::Shadow);
    for interval in 1..=4 {
        engine.handle(Event::Tick, takeover + settle * interval, wall());
        assert_no_standby_decision(&engine, "streamer");
    }
    engine.handle(
        Event::Command(Command::SetMode(Mode::Active)),
        takeover + settle * 4,
        wall(),
    );
    engine.handle(Event::Tick, takeover + settle * 5, wall());
    assert_no_standby_decision(&engine, "streamer");
}

#[test]
fn switching_to_shadow_clears_pending_standby_verification() {
    let (mut engine, takeover, settle) = takeover_from_streamer();
    engine.handle(
        Event::Command(Command::SetMode(Mode::Shadow)),
        takeover,
        wall(),
    );
    engine.handle(Event::Tick, takeover + settle, wall());
    assert_no_standby_decision(&engine, "streamer");
    engine.handle(
        Event::Command(Command::SetMode(Mode::Active)),
        takeover + settle,
        wall(),
    );
    engine.handle(Event::Tick, takeover + settle * 2, wall());
    assert_no_standby_decision(&engine, "streamer");
}

#[test]
fn config_reload_always_clears_pending_standby_verification() {
    for change in ["none", "policy", "identity", "shadow"] {
        let (mut engine, takeover, _) = takeover_from_streamer();
        let mut config = (*engine.config).clone();
        match change {
            "policy" => config.device[0].takeover = choregos::config::Takeover::None,
            "identity" => config.device[0].matches.vendor = Some("FFFFFF".into()),
            "shadow" => config.mode = Mode::Shadow,
            _ => {}
        }
        engine.handle(
            Event::ConfigReloaded(std::sync::Arc::new(config)),
            takeover,
            wall(),
        );
        engine.handle(Event::Tick, takeover + Duration::from_secs(3), wall());
        assert_no_standby_decision(&engine, "streamer");
    }
}

#[test]
fn off_timer_is_cancelled_by_wake() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 0, "04");
    frame(&mut engine, now + Duration::from_secs(1), 4, 15, "36");
    frame(&mut engine, now + Duration::from_secs(2), 4, 0, "04");
    assert!(
        !engine
            .handle(Event::Tick, now + Duration::from_secs(5), wall())
            .iter()
            .any(|a| *a == Action::TvOff || service(a, "turn_off"))
    );
}

#[test]
fn warming_blocks_power_until_reply_or_timeout() {
    let (mut engine, now) = setup();
    let startup = engine.handle(Event::BridgeOnline, now, wall());
    assert_eq!(
        startup
            .iter()
            .filter(|a| matches!(a, Action::Resync))
            .count(),
        1
    );
    assert!(
        engine
            .handle(
                Event::Command(Command::Takeover("streamer".into())),
                now,
                wall()
            )
            .is_empty()
    );
    assert!(engine.warming);
    engine.handle(Event::Tick, now + Duration::from_secs(3), wall());
    assert!(!engine.warming);
    assert!(
        engine
            .handle(
                Event::Command(Command::Takeover("streamer".into())),
                now + Duration::from_secs(3),
                wall()
            )
            .contains(&Action::WakeTv)
    );
    engine.handle(Event::BridgeOnline, now + Duration::from_secs(4), wall());
    frame(&mut engine, now + Duration::from_secs(4), 4, 0, "9000");
    assert!(!engine.warming);
}

#[test]
fn presence_misses_require_unanswered_polls_and_any_frame_resets() {
    let (mut engine, now) = setup();
    frame(&mut engine, now, 8, 15, "84230004");
    frame(&mut engine, now, 8, 15, "47436F6E736F6C652041");
    frame(&mut engine, now, 8, 15, "822300");
    let identity = engine.bus.entries[&8].clone();
    assert_eq!(identity.power, BusPower::On);
    assert_eq!(engine.roles["console_a"].power, RolePower::On);
    assert!(
        engine
            .handle(Event::Tick, now + Duration::from_secs(5), wall())
            .contains(&Action::PresencePoll { la: 8 })
    );
    engine.handle(Event::Tick, now + Duration::from_secs(10), wall());
    frame(&mut engine, now + Duration::from_secs(11), 8, 0, "");
    engine.handle(Event::Tick, now + Duration::from_secs(15), wall());
    engine.handle(Event::Tick, now + Duration::from_secs(20), wall());
    assert_eq!(engine.roles["console_a"].power, RolePower::On);
    assert_eq!(engine.bus.entries[&8].power, BusPower::On);
    engine.handle(Event::Tick, now + Duration::from_secs(25), wall());
    assert_eq!(engine.roles["console_a"].power, RolePower::Off);
    assert!(
        engine
            .decisions
            .iter()
            .any(|d| d.reason == "presence_timeout")
    );
    let entry = &engine.bus.entries[&8];
    assert_eq!(entry.power, BusPower::Unknown);
    assert_eq!(entry.physical, identity.physical);
    assert_eq!(entry.osd, identity.osd);
    assert_eq!(entry.vendor, identity.vendor);
    assert_eq!(entry.identified, identity.identified);
    assert_eq!(engine.identity.roles["console_a"], 8);
    assert!(
        !engine
            .handle(Event::Tick, now + Duration::from_secs(30), wall())
            .iter()
            .any(|a| matches!(a, Action::PresencePoll { .. }))
    );
    frame(&mut engine, now + Duration::from_secs(31), 8, 15, "822300");
    assert_eq!(engine.bus.entries[&8].power, BusPower::On);
    assert_eq!(engine.roles["console_a"].power, RolePower::On);
}

#[test]
fn conflict_blocks_power_and_resolves_after_address_replacement() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 8, 15, "84240004");
    frame(&mut engine, now, 8, 15, "870010FA");
    frame(&mut engine, now, 8, 0, "474170706C65205456");
    assert_eq!(engine.identity.conflicts.len(), 1);
    assert!(!frame(&mut engine, now, 4, 0, "04").contains(&Action::WakeTv));
    let resolved = frame(&mut engine, now, 8, 15, "84250004");
    assert!(engine.identity.conflicts.is_empty());
    assert!(resolved.contains(&Action::WakeTv));
    assert!(!frame(&mut engine, now, 4, 0, "04").contains(&Action::WakeTv));
}

#[test]
fn fake_tv_and_volume_mute_are_address_scoped() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    assert_eq!(
        frame(&mut engine, now, 4, 0, "8F"),
        vec![Action::CecSend {
            to: 4,
            data: vec![0x90, 0]
        }]
    );
    assert!(frame(&mut engine, now, 4, 5, "8F").is_empty());
    assert_eq!(
        frame(&mut engine, now, 4, 0, "9F"),
        vec![Action::CecSend {
            to: 4,
            data: vec![0x9e, 5]
        }]
    );
    let sonos = engine.config.sonos.entity.clone();
    ha(&mut engine, now, sonos, "playing", None);
    assert!(
        frame(&mut engine, now, 4, 5, "4443")
            .iter()
            .any(|a| service(a, "toggle"))
    );
    assert!(frame(&mut engine, now, 4, 1, "4441").is_empty());
    assert!(frame(&mut engine, now, 4, 0, "45").is_empty());
}

#[test]
fn preset_uses_injected_timezone_and_existing_receiver_source_is_preserved() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    let receiver = engine.config.receiver.entity.clone();
    let mut attrs = BTreeMap::new();
    attrs.insert("source".into(), Value::String("Streamer".into()));
    engine.handle(
        Event::HaState {
            entity: receiver,
            state: "on".into(),
            attrs,
            changed: None,
            updated: None,
        },
        now,
        wall(),
    );
    let tv = engine.config.tv.entity.clone();
    ha(&mut engine, now, tv, "on", None);
    let actions = frame(&mut engine, now, 4, 0, "04");
    assert!(!actions.contains(&Action::WakeTv));
    assert!(!actions.iter().any(|a| service(a, "turn_on")));
    let settled = engine.handle(Event::Tick, now + Duration::from_secs(2), wall());
    assert_eq!(settled.len(), 1);
    assert!(matches!(&settled[0], Action::HaCall { domain, entity, .. }
        if domain == "script" && entity == "script.listening_day"));
}

#[test]
fn on_signal_before_identity_is_retained_without_retriggering_on_chatter() {
    let (mut engine, now) = setup();
    assert!(!frame(&mut engine, now, 4, 0, "04").contains(&Action::WakeTv));
    identify_atv(&mut engine, now + Duration::from_millis(10));
    assert_eq!(engine.roles["streamer"].power, RolePower::On);
    assert!(frame(&mut engine, now + Duration::from_secs(1), 4, 0, "").is_empty());
}

#[test]
fn config_reload_reinterrogates_and_discards_removed_roles() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    let mut config = (*engine.config).clone();
    config.device.retain(|d| d.role != "streamer");
    let actions = engine.handle(
        Event::ConfigReloaded(std::sync::Arc::new(config)),
        now,
        wall(),
    );
    assert!(!engine.roles.contains_key("streamer"));
    assert!(engine.warming);
    assert!(actions.is_empty());
}

#[test]
fn replacing_a_logical_address_releases_its_previous_physical_role() {
    let (mut engine, now) = setup();
    frame(&mut engine, now, 8, 15, "84230004");
    assert_eq!(engine.roles["console_a"].power, RolePower::On);
    frame(&mut engine, now + Duration::from_secs(1), 8, 15, "84240004");
    assert_eq!(engine.roles["console_a"].power, RolePower::Off);
    assert!(engine.decisions.iter().any(|d| d.reason == "identity_lost"));
}

#[test]
fn bridge_resync_defers_due_polls_without_spinning_or_losing_roles() {
    let (mut engine, now) = setup();
    frame(&mut engine, now, 8, 15, "84230004");
    engine.handle(Event::BridgeOffline, now + Duration::from_secs(3), wall());
    assert!(!engine.bridge_online);
    engine.handle(Event::BridgeOnline, now + Duration::from_secs(4), wall());
    assert_eq!(engine.next_deadline(), Some(now + Duration::from_secs(5)));
    assert!(
        engine
            .handle(Event::Tick, now + Duration::from_secs(4), wall())
            .is_empty()
    );
    let identification = engine.handle(Event::Tick, now + Duration::from_secs(5), wall());
    assert!(
        identification
            .iter()
            .all(|action| matches!(action, Action::Identify { .. }))
    );
    assert!(!identification.is_empty());
    assert!(
        engine
            .handle(Event::Tick, now + Duration::from_secs(5), wall())
            .is_empty()
    );
    assert_eq!(engine.next_deadline(), Some(now + Duration::from_secs(7)));
    engine.handle(Event::Tick, now + Duration::from_secs(7), wall());
    assert!(!engine.warming);
    assert_eq!(engine.roles["console_a"].power, RolePower::On);
    assert!(
        engine
            .next_deadline()
            .is_some_and(|d| d > now + Duration::from_secs(7))
    );
}

#[test]
fn switching_to_shadow_keeps_pending_timer_but_suppresses_its_effects() {
    use choregos::{runtime::executor::suppressed, state::Mode};
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    engine.handle(Event::Command(Command::SetMode(Mode::Active)), now, wall());
    frame(&mut engine, now, 4, 0, "04");
    frame(&mut engine, now + Duration::from_secs(1), 4, 15, "36");
    engine.handle(
        Event::Command(Command::SetMode(Mode::Shadow)),
        now + Duration::from_secs(2),
        wall(),
    );
    let actions = engine.handle(Event::Tick, now + Duration::from_secs(4), wall());
    assert_eq!(actions.len(), 2);
    assert!(actions.iter().all(|a| suppressed(engine.mode(), a)));
    assert_eq!(
        actions,
        [
            Action::TvOff,
            Action::ha("media_player", "turn_off", &engine.config.receiver.entity)
        ]
    );
}

#[test]
fn taken_over_sources_cannot_hold_the_room_on_without_standby_reports() {
    let (mut engine, now) = setup();
    frame(&mut engine, now, 1, 0, "474B6F6469");
    frame(&mut engine, now, 1, 15, "822200");
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 0, "04");
    assert_eq!(engine.roles["kodi"].power, RolePower::Off);
    assert!(engine.decisions.iter().any(|d| d.reason == "taken_over"));
    frame(&mut engine, now + Duration::from_secs(1), 4, 0, "36");
    let actions = engine.handle(Event::Tick, now + Duration::from_secs(4), wall());
    assert_eq!(
        actions,
        [
            Action::Identify {
                la: 1,
                queries: IdentifyQueries {
                    physical: true,
                    vendor: true,
                    osd: false
                },
            },
            Action::TvOff,
            Action::ha("media_player", "turn_off", &engine.config.receiver.entity)
        ]
    );
}

#[test]
fn bridge_offline_pauses_presence_and_resync_resets_outstanding_misses() {
    let (mut engine, now) = setup();
    frame(&mut engine, now, 8, 15, "84230004");
    engine.handle(Event::Tick, now + Duration::from_secs(5), wall());
    engine.handle(Event::BridgeOffline, now + Duration::from_secs(6), wall());
    for second in [10, 15, 20, 25, 30] {
        assert!(
            engine
                .handle(Event::Tick, now + Duration::from_secs(second), wall())
                .iter()
                .all(|action| matches!(action, Action::Identify { .. }))
        );
        assert_eq!(engine.roles["console_a"].power, RolePower::On);
    }
    engine.handle(Event::BridgeOnline, now + Duration::from_secs(31), wall());
    frame(&mut engine, now + Duration::from_secs(31), 8, 0, "9000");
    engine.handle(Event::Tick, now + Duration::from_secs(35), wall());
    engine.handle(Event::Tick, now + Duration::from_secs(40), wall());
    assert_eq!(engine.roles["console_a"].power, RolePower::On);
}

#[test]
fn a_physical_device_reclaiming_another_la_evicts_the_old_identity() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 0, "04");
    frame(&mut engine, now + Duration::from_secs(1), 8, 15, "84210004");
    frame(&mut engine, now + Duration::from_secs(1), 8, 15, "870010FA");
    frame(
        &mut engine,
        now + Duration::from_secs(1),
        8,
        0,
        "474170706C65205456",
    );
    assert!(!engine.bus.entries.contains_key(&4));
    assert!(engine.identity.conflicts.is_empty());
    assert_eq!(engine.identity.roles["streamer"], 8);
    frame(&mut engine, now + Duration::from_secs(2), 8, 0, "04");
    frame(&mut engine, now + Duration::from_secs(3), 8, 0, "36");
    assert_eq!(
        engine.handle(Event::Tick, now + Duration::from_secs(6), wall()),
        [
            Action::TvOff,
            Action::ha("media_player", "turn_off", &engine.config.receiver.entity)
        ]
    );
}

#[test]
fn unrelated_conflicts_do_not_cancel_takeover_settle_or_release() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 0, "04");
    for (la, physical) in [(8, "84240004"), (11, "84250004")] {
        frame(&mut engine, now, la, 15, physical);
        frame(&mut engine, now, la, 15, "87080046");
    }
    assert!(engine.identity.blocked.contains("console_b"));
    let settled = engine.handle(Event::Tick, now + Duration::from_secs(2), wall());
    assert!(settled.iter().any(|a| service(a, "select_source")));
    frame(&mut engine, now + Duration::from_secs(3), 4, 0, "36");
    assert_eq!(
        engine.handle(Event::Tick, now + Duration::from_secs(6), wall()),
        [
            Action::Identify {
                la: 11,
                queries: IdentifyQueries {
                    physical: false,
                    vendor: false,
                    osd: true
                },
            },
            Action::TvOff,
            Action::ha("media_player", "turn_off", &engine.config.receiver.entity)
        ]
    );
}

#[test]
fn retained_online_defers_but_preserves_both_pending_timer_classes() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 0, "04");
    engine.handle(Event::BridgeOnline, now + Duration::from_secs(1), wall());
    let settle = engine.handle(Event::Tick, now + Duration::from_secs(4), wall());
    assert!(settle.iter().any(|a| service(a, "select_source")));
    assert!(
        settle
            .iter()
            .any(|a| matches!(a, Action::HaCall { domain, .. } if domain == "script"))
    );
    frame(&mut engine, now + Duration::from_secs(5), 4, 0, "36");
    engine.handle(Event::BridgeOnline, now + Duration::from_secs(6), wall());
    let off = engine.handle(Event::Tick, now + Duration::from_secs(9), wall());
    assert_eq!(
        off,
        [
            Action::TvOff,
            Action::ha("media_player", "turn_off", &engine.config.receiver.entity)
        ]
    );
}

#[test]
fn quick_rewake_ignores_ha_on_values_stale_from_before_our_off_actions() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    for entity in [
        engine.config.tv.entity.clone(),
        engine.config.receiver.entity.clone(),
    ] {
        ha(&mut engine, now, entity, "on", None);
    }
    frame(&mut engine, now, 4, 0, "04");
    frame(&mut engine, now + Duration::from_secs(1), 4, 0, "36");
    engine.handle(Event::Tick, now + Duration::from_secs(4), wall());
    assert_eq!(engine.intent.power, RoomPower::Off);
    assert_eq!(engine.ha[&engine.config.tv.entity].state, "on");
    assert_eq!(engine.ha[&engine.config.receiver.entity].state, "on");
    let wake = frame(&mut engine, now + Duration::from_millis(4001), 4, 0, "04");
    assert!(wake.contains(&Action::WakeTv));
    assert!(wake.iter().any(|a| service(a, "turn_on")));
}

#[test]
fn restart_with_room_on_uses_ha_cache_until_intent_is_known() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    for entity in [
        engine.config.tv.entity.clone(),
        engine.config.receiver.entity.clone(),
    ] {
        ha(&mut engine, now, entity, "on", None);
    }
    assert_eq!(engine.intent.power, RoomPower::Unknown);
    let actions = frame(&mut engine, now, 4, 0, "04");
    assert!(!actions.contains(&Action::WakeTv));
    assert!(!actions.iter().any(|action| service(action, "turn_on")));
    assert_eq!(engine.intent.power, RoomPower::On);
}

#[test]
fn fake_tv_never_answers_unregistered_senders_and_can_delegate_directed_replies() {
    let (mut engine, now) = setup();
    for query in ["8F", "9F", "46", "91"] {
        assert!(frame(&mut engine, now, 15, 0, query).is_empty());
    }
    identify_atv(&mut engine, now);
    std::sync::Arc::make_mut(&mut engine.config).fake_tv.replies = false;
    for query in ["8F", "9F", "46", "91"] {
        assert!(frame(&mut engine, now, 4, 0, query).is_empty());
    }
}

#[test]
fn fake_tv_never_answers_request_active_source_even_with_known_source() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 15, "822100");
    assert_eq!(engine.bus.active_source, Some(0x2100));
    for replies in [true, false] {
        std::sync::Arc::make_mut(&mut engine.config).fake_tv.replies = replies;
        let actions = frame(&mut engine, now, 1, 15, "85");
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::CecSend { .. }))
        );
        assert_eq!(engine.bus.active_source, Some(0x2100));
        assert_eq!(engine.roles["streamer"].power, RolePower::On);
        assert_eq!(engine.intent.active_role.as_deref(), Some("streamer"));
    }
}

#[test]
fn preset_observes_wall_clock_steps_independently_of_monotonic_time() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 0, "04");
    let stepped_wall = "2026-10-25T21:30:00Z".parse().unwrap();
    let actions = engine.handle(Event::Tick, now + Duration::from_secs(2), stepped_wall);
    assert!(actions.iter().any(
        |a| matches!(a, Action::HaCall { entity, .. } if entity == "script.listening_evening")
    ));
}

#[test]
fn manual_takeover_actions_are_recorded_once_not_as_a_duplicate_command_decision() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    engine.handle(
        Event::Command(Command::Takeover("streamer".into())),
        now,
        wall(),
    );
    assert_eq!(engine.decisions.len(), 1);
    assert_eq!(engine.decisions[0].reason, "manual_takeover");
}

#[test]
fn ha_changed_prefers_ha_then_stamps_transitions_but_not_attribute_updates() {
    let (mut engine, now) = setup();
    let entity = engine.config.tv.entity.clone();
    let provided = wall() - chrono::Duration::minutes(5);
    ha(&mut engine, now, entity.clone(), "on", Some(provided));
    assert_eq!(engine.ha[&entity].changed, Some(provided));

    ha(&mut engine, now, entity.clone(), "off", None);
    assert_eq!(engine.ha[&entity].changed, Some(wall()));

    let attrs = BTreeMap::from([("volume_level".into(), serde_json::json!(0.5))]);
    engine.handle(
        Event::HaState {
            entity: entity.clone(),
            state: "off".into(),
            attrs: attrs.clone(),
            changed: None,
            updated: None,
        },
        now,
        wall() + chrono::Duration::seconds(1),
    );
    assert_eq!(engine.ha[&entity].changed, Some(wall()));
    assert_eq!(engine.ha[&entity].attrs, attrs);

    ha(&mut engine, now, entity.clone(), "off", Some(provided));
    assert_eq!(engine.ha[&entity].changed, Some(provided));
}

#[test]
fn room_off_preserves_observed_ha_states_and_timestamps() {
    let (mut engine, now) = setup();
    frame(&mut engine, now, 8, 15, "84230004");
    let observed = wall() - chrono::Duration::minutes(5);
    for (entity, state) in [
        (engine.config.tv.entity.clone(), "on"),
        (engine.config.receiver.entity.clone(), "off"),
    ] {
        ha(&mut engine, now, entity, state, Some(observed));
    }
    frame(&mut engine, now, 8, 15, "36");
    let delay = Duration::from_secs(engine.config.timing.off_delay_s);
    let expired = wall() + chrono::Duration::from_std(delay).unwrap();
    engine.handle(Event::Tick, now + delay, expired);
    for (entity, state) in [
        (&engine.config.tv.entity, "on"),
        (&engine.config.receiver.entity, "off"),
    ] {
        assert_eq!(engine.ha[entity].state, state);
        assert_eq!(engine.ha[entity].changed, Some(observed));
    }
}

#[test]
fn missing_vendor_retries_on_ticks_and_releases_buffered_active_source() {
    let (mut engine, now) = setup();
    let now = now + Duration::from_secs(1);
    frame(&mut engine, now, 4, 15, "84210004");
    frame(&mut engine, now, 4, 0, "4753747265616D6572");
    frame(&mut engine, now, 4, 15, "822100");
    assert!(!engine.bus.entries[&4].identified);
    assert!(!engine.identity.roles.contains_key("streamer"));
    let retry = Duration::from_secs(engine.config.timing.identify_retry_s);
    let vendor_only = Action::Identify {
        la: 4,
        queries: IdentifyQueries {
            vendor: true,
            ..IdentifyQueries::default()
        },
    };
    for interval in 1..=3 {
        let due = now + retry * ((1 << interval) - 1);
        assert_eq!(engine.next_deadline(), Some(due));
        assert!(
            engine
                .handle(Event::Tick, due - Duration::from_millis(1), wall())
                .is_empty()
        );
        assert_eq!(
            engine.handle(Event::Tick, due, wall()).as_slice(),
            std::slice::from_ref(&vendor_only)
        );
    }
    let actions = frame(&mut engine, now + retry * 7, 4, 15, "870010FA");
    assert!(engine.bus.entries[&4].identified);
    assert_eq!(engine.identity.roles["streamer"], 4);
    assert_eq!(engine.roles["streamer"].power, RolePower::On);
    assert!(actions.contains(&Action::WakeTv));
    assert_eq!(engine.decisions[0].reason, "ActiveSource");
    assert!(
        !engine
            .handle(Event::Tick, now + retry * 8, wall())
            .iter()
            .any(|action| matches!(action, Action::Identify { .. }))
    );
}

#[test]
fn identification_requirements_follow_config_reload_without_optional_vendor_queries() {
    let now = Instant::now();
    let mut config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    config
        .device
        .retain(|device| device.matches.vendor.is_none());
    let mut engine = Engine::new(config.clone(), now, vec![], 1);
    engine.handle(Event::BridgeOnline, now, wall());
    frame(&mut engine, now, 1, 15, "84220001");
    frame(&mut engine, now, 1, 0, "474B6F6469");
    assert!(engine.bus.entries[&1].identified);
    let retry = Duration::from_secs(config.timing.identify_retry_s);
    assert!(engine.handle(Event::Tick, now + retry, wall()).is_empty());
    let with_vendor = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    engine.handle(
        Event::ConfigReloaded(std::sync::Arc::new(with_vendor)),
        now + retry,
        wall(),
    );
    assert!(!engine.bus.entries[&1].identified);
    assert_eq!(
        engine.handle(Event::Tick, now + retry, wall()),
        [Action::Identify {
            la: 1,
            queries: IdentifyQueries {
                vendor: true,
                ..IdentifyQueries::default()
            },
        }]
    );
    engine.handle(
        Event::ConfigReloaded(std::sync::Arc::new(config)),
        now + retry,
        wall(),
    );
    assert!(engine.bus.entries[&1].identified);
    assert!(
        !engine
            .handle(Event::Tick, now + retry * 2, wall())
            .iter()
            .any(|action| matches!(action, Action::Identify { .. }))
    );
}

#[test]
fn identify_queries_only_missing_keys_but_manual_identify_requests_all() {
    let (mut engine, now) = setup();
    let now = now + Duration::from_secs(1);
    assert_eq!(
        frame(&mut engine, now, 4, 15, "870010FA"),
        [Action::Identify {
            la: 4,
            queries: IdentifyQueries {
                physical: true,
                vendor: false,
                osd: true
            },
        }]
    );
    frame(&mut engine, now, 4, 0, "4753747265616D6572");
    let retry = Duration::from_secs(engine.config.timing.identify_retry_s);
    assert_eq!(
        engine.handle(Event::Tick, now + retry, wall()),
        [Action::Identify {
            la: 4,
            queries: IdentifyQueries {
                physical: true,
                ..IdentifyQueries::default()
            },
        }]
    );
    assert_eq!(
        engine.handle(
            Event::Command(Command::Identify(4)),
            now + retry + Duration::from_secs(1),
            wall(),
        ),
        [Action::Identify {
            la: 4,
            queries: IdentifyQueries::ALL
        },]
    );
    for la in [0, 15] {
        assert!(
            engine
                .handle(
                    Event::Command(Command::Identify(la)),
                    now + retry + Duration::from_secs(1),
                    wall(),
                )
                .is_empty()
        );
        frame(
            &mut engine,
            now + retry + Duration::from_secs(1),
            la,
            15,
            "870010FA",
        );
    }
    let actions = engine.handle(Event::Tick, now + retry * 2, wall());
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::Identify { la: 0 | 15, .. }))
    );
}

fn restored_identity(la: u8, physical: u16) -> choregos::cec::bus::LearnedIdentity {
    choregos::cec::bus::LearnedIdentity {
        la,
        physical: Some(physical),
        vendor: Some(0x0010fa),
        osd: Some("Streamer".into()),
        device_type: Some(4),
        confirmed_at: wall(),
    }
}

#[test]
fn forgetting_a_restored_entry_removes_its_identification_retries_in_both_modes() {
    for mode in [Mode::Shadow, Mode::Active] {
        let now = Instant::now();
        let mut config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
        config.mode = mode;
        let mut engine = Engine::new(config, now, vec![restored_identity(3, 0x2500)], 1);
        engine.handle(Event::BridgeOnline, now, wall());
        // Leave a full manual identify queued behind the resync send slot.
        assert!(
            engine
                .handle(Event::Command(Command::Identify(3)), now, wall())
                .is_empty()
        );
        assert!(!engine.bus.entries[&3].identified);
        let actions = engine.handle(Event::Command(Command::Forget(3)), now, wall());
        assert!(actions.is_empty());
        assert!(!engine.bus.entries.contains_key(&3));
        assert_eq!(engine.decisions.len(), 1);
        let decision = &engine.decisions[0];
        assert_eq!(decision.reason, "command_forget");
        assert_eq!(
            (decision.role.as_ref(), decision.from, decision.to),
            (None, None, None)
        );
        assert!(decision.actions.is_empty());
        for seconds in [1, 10, 30, 3600] {
            assert!(
                engine
                    .handle(Event::Tick, now + Duration::from_secs(seconds), wall())
                    .is_empty()
            );
        }
    }
}

#[test]
fn forgetting_an_on_role_unbinds_it_and_uses_identity_lost_release_semantics() {
    let (mut engine, now) = setup();
    frame(&mut engine, now, 8, 15, "84230004");
    assert_eq!(engine.roles["console_a"].power, RolePower::On);
    assert_eq!(engine.identity.roles["console_a"], 8);
    assert_eq!(engine.intent.active_role.as_deref(), Some("console_a"));
    let forgotten = now + Duration::from_secs(1);
    let actions = engine.handle(Event::Command(Command::Forget(8)), forgotten, wall());
    assert!(actions.is_empty());
    assert!(!engine.bus.entries.contains_key(&8));
    assert!(!engine.identity.roles.contains_key("console_a"));
    assert_eq!(engine.roles["console_a"].power, RolePower::Off);
    assert_eq!(
        engine.roles["console_a"].last_event.as_deref(),
        Some("identity_lost")
    );
    assert_eq!(engine.intent.active_role, None);
    assert_eq!(
        engine
            .decisions
            .iter()
            .map(|decision| (
                decision.role.as_deref(),
                decision.from,
                decision.to,
                decision.reason.as_str(),
                decision.actions.as_slice(),
            ))
            .collect::<Vec<_>>(),
        [
            (
                Some("console_a"),
                Some(RolePower::On),
                Some(RolePower::Off),
                "identity_lost",
                &[][..]
            ),
            (None, None, None, "command_forget", &[][..]),
        ]
    );
    let released = engine.handle(
        Event::Tick,
        forgotten + Duration::from_secs(engine.config.timing.off_delay_s),
        wall(),
    );
    assert!(released.contains(&Action::TvOff));
    assert!(released.iter().any(|action| service(action, "turn_off")));
}

#[test]
fn forgetting_a_conflicting_address_replays_the_new_bindings_latest_buffered_signal() {
    for (latest, expected) in [("04", RolePower::On), ("36", RolePower::Off)] {
        let (mut engine, now) = setup();
        identify_atv(&mut engine, now);
        frame(&mut engine, now, 11, 15, "84250004");
        frame(&mut engine, now, 11, 15, "870010FA");
        assert!(engine.identity.blocked.contains("streamer"));
        assert!(!engine.identity.roles.contains_key("streamer"));
        assert!(frame(&mut engine, now, 11, 0, "04").is_empty());
        assert!(frame(&mut engine, now, 11, 0, latest).is_empty());
        assert_eq!(engine.roles["streamer"].power, RolePower::Off);

        let actions = engine.handle(Event::Command(Command::Forget(4)), now, wall());
        assert!(!engine.identity.blocked.contains("streamer"));
        assert_eq!(engine.identity.roles["streamer"], 11);
        assert_eq!(engine.roles["streamer"].power, expected);
        assert_eq!(actions.contains(&Action::WakeTv), expected == RolePower::On);
        if expected == RolePower::On {
            assert_eq!(
                engine.roles["streamer"].last_event.as_deref(),
                Some("ImageViewOn")
            );
        }
        assert!(
            engine
                .decisions
                .iter()
                .any(|decision| decision.reason == "conflict_resolved")
        );
        let forgotten = engine.decisions.last().unwrap();
        assert_eq!(forgotten.reason, "command_forget");
        assert!(forgotten.actions.is_empty());
    }
}

#[test]
fn a_forgotten_address_is_learned_afresh_without_cached_keys_backoff_or_wake_signals() {
    let now = Instant::now();
    let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)], 1);
    engine.handle(Event::BridgeOnline, now, wall());
    // Buffer a wake under the unverified identity and build up retry backoff.
    frame(&mut engine, now, 4, 0, "04");
    for seconds in [10, 30] {
        engine.handle(Event::Tick, now + Duration::from_secs(seconds), wall());
    }
    let forgotten = now + Duration::from_secs(31);
    assert!(
        engine
            .handle(Event::Command(Command::Forget(4)), forgotten, wall())
            .is_empty()
    );
    // An ordinary poll is enough to rediscover the LA: no wake special case.
    assert_eq!(
        frame(&mut engine, forgotten, 4, 0, ""),
        [Action::Identify {
            la: 4,
            queries: IdentifyQueries::ALL
        }]
    );
    let entry = &engine.bus.entries[&4];
    assert_eq!(
        (entry.physical, entry.vendor, entry.osd.as_ref()),
        (None, None, None)
    );
    assert_eq!(entry.confirmed_at, None);
    assert_eq!(entry.power, BusPower::Unknown);
    assert_eq!(entry.last_seen, Some(forgotten));
    identify_atv(&mut engine, forgotten);
    assert_eq!(engine.identity.roles["streamer"], 4);
    assert_eq!(engine.roles["streamer"].power, RolePower::Off);
    assert_eq!(engine.bus.entries[&4].power, BusPower::Unknown);
    assert!(frame(&mut engine, forgotten, 4, 0, "04").contains(&Action::WakeTv));
}

#[test]
fn forgetting_an_address_cancels_its_pending_standby_verification() {
    let (mut engine, takeover, settle) = takeover_from_streamer();
    assert_eq!(engine.roles["streamer"].power, RolePower::Off);
    assert!(
        engine
            .handle(Event::Command(Command::Forget(4)), takeover, wall())
            .is_empty()
    );
    // Rebinding the same role at the same LA must not revive the old verification.
    identify_atv(&mut engine, takeover);
    assert_eq!(engine.identity.roles["streamer"], 4);
    for interval in 1..=4 {
        let actions = engine.handle(Event::Tick, takeover + settle * interval, wall());
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::CecSend { to: 4, .. }))
        );
        assert!(
            !engine
                .decisions
                .iter()
                .any(|decision| decision.reason.starts_with("standby_"))
        );
    }
}

#[test]
fn restored_identity_buffers_power_until_confirming_physical_report() {
    let now = Instant::now();
    let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)], 1);
    assert!(!engine.identity.roles.contains_key("streamer"));
    assert!(!engine.bus.entries[&4].identified);
    assert!(!engine.bus.entries[&4].verified);
    assert_eq!(engine.bus.entries[&4].last_seen, None);
    assert_eq!(engine.bus.entries[&4].power, BusPower::Unknown);
    let resync = engine.handle(Event::BridgeOnline, now, wall());
    assert!(resync.contains(&Action::Identify {
        la: 4,
        queries: IdentifyQueries {
            physical: true,
            ..IdentifyQueries::default()
        },
    }));
    assert!(!frame(&mut engine, now, 4, 15, "822100").contains(&Action::WakeTv));
    assert!(!engine.identity.roles.contains_key("streamer"));
    let actions = frame(&mut engine, now, 4, 15, "84210004");
    assert!(actions.contains(&Action::WakeTv));
    assert_eq!(engine.identity.roles["streamer"], 4);
    assert!(engine.bus.entries[&4].verified);
    assert!(engine.bus.entries[&4].identified);
    assert_eq!(engine.bus.entries[&4].last_seen, Some(now));
    assert_eq!(engine.bus.entries[&4].confirmed_at, Some(wall()));
}

#[test]
fn mismatching_active_source_replaces_restored_identity_and_old_signals() {
    let now = Instant::now();
    let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)], 1);
    engine.handle(Event::BridgeOnline, now, wall());
    frame(&mut engine, now, 4, 15, "822300");
    assert_eq!(engine.bus.entries[&4].physical, Some(0x2300));
    assert_eq!(engine.bus.entries[&4].vendor, None);
    assert_eq!(engine.bus.entries[&4].osd, None);
    assert!(!engine.identity.roles.contains_key("streamer"));
    assert_eq!(engine.identity.roles["console_a"], 4);
}

#[test]
fn restored_conflicts_wait_for_verification_and_duplicate_physical_is_skipped() {
    let now = Instant::now();
    let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    let mut engine = Engine::new(
        config,
        now,
        vec![
            restored_identity(4, 0x2100),
            restored_identity(8, 0x2500),
            restored_identity(11, 0x2100),
        ],
        1,
    );
    assert_eq!(engine.bus.entries.len(), 2);
    assert!(engine.identity.conflicts.is_empty());
    engine.handle(Event::BridgeOnline, now, wall());
    frame(&mut engine, now, 4, 15, "84210004");
    frame(&mut engine, now, 8, 15, "84250004");
    assert!(engine.identity.blocked.contains("streamer"));
    assert_eq!(engine.identity.conflicts.len(), 1);
    assert!(!frame(&mut engine, now, 4, 15, "822100").contains(&Action::WakeTv));
}

#[test]
fn vendor_change_discards_other_keys_and_requires_fresh_physical_confirmation() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 15, "822100");
    frame(&mut engine, now, 4, 15, "87080046");
    let entry = &engine.bus.entries[&4];
    assert_eq!(entry.physical, Some(0x2100));
    assert_eq!(entry.osd, None);
    assert_eq!(entry.device_type, None);
    assert!(!entry.verified);
    assert!(!entry.identified);
    assert!(!engine.identity.roles.contains_key("console_b"));
    let actions = frame(&mut engine, now, 4, 15, "84210004");
    assert_eq!(engine.identity.roles["console_b"], 4);
    assert_eq!(engine.roles["console_b"].power, RolePower::Off);
    assert!(!actions.contains(&Action::WakeTv));
}

#[test]
fn identification_backoff_caps_at_an_hour_resets_on_progress_and_pauses_offline() {
    let (mut engine, now) = setup();
    let now = now + Duration::from_secs(3);
    frame(&mut engine, now, 4, 15, "84210004");
    let mut due = now;
    for delay in [10, 20, 40, 80, 160, 320, 640, 1280, 2560, 3600, 3600] {
        due += Duration::from_secs(delay);
        assert_eq!(engine.next_deadline(), Some(due));
        assert!(
            engine
                .handle(Event::Tick, due - Duration::from_millis(1), wall())
                .is_empty()
        );
        assert_eq!(engine.handle(Event::Tick, due, wall()).len(), 1);
    }
    frame(&mut engine, due, 4, 0, "");
    assert_eq!(
        engine.next_deadline(),
        Some(due + Duration::from_secs(3600))
    );
    frame(&mut engine, due, 4, 0, "4753747265616D6572");
    assert_eq!(engine.next_deadline(), Some(due + Duration::from_secs(10)));
    engine.handle(Event::BridgeOffline, due, wall());
    assert_eq!(engine.next_deadline(), None);
    assert!(
        engine
            .handle(Event::Tick, due + Duration::from_secs(20), wall())
            .is_empty()
    );
    let online = due + Duration::from_secs(20);
    engine.handle(Event::BridgeOnline, online, wall());
    assert!(frame(&mut engine, online, 4, 0, "").is_empty());
    assert_eq!(
        engine.next_deadline(),
        Some(online + Duration::from_secs(1))
    );
    assert!(matches!(
        engine
            .handle(Event::Tick, online + Duration::from_secs(1), wall())
            .as_slice(),
        [Action::Identify { .. }]
    ));
}

#[test]
fn osd_change_clears_vendor_and_does_not_replay_previous_occupants_power_signal() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 15, "822100");
    frame(&mut engine, now, 4, 0, "474B6F6469");
    assert_eq!(engine.bus.entries[&4].vendor, None);
    assert!(!engine.bus.entries[&4].verified);
    assert!(!engine.identity.roles.contains_key("kodi"));
    frame(&mut engine, now, 4, 15, "84210004");
    assert_eq!(engine.identity.roles["kodi"], 4);
    assert_eq!(engine.roles["kodi"].power, RolePower::Off);
}

#[test]
fn manual_identification_defers_automatic_retry_from_the_manual_query_instant() {
    let (mut engine, now) = setup();
    frame(&mut engine, now, 4, 15, "84210004");
    engine.handle(
        Event::Command(Command::Identify(4)),
        now + Duration::from_secs(9),
        wall(),
    );
    assert!(
        engine
            .handle(Event::Tick, now + Duration::from_secs(10), wall())
            .is_empty()
    );
    assert_eq!(engine.next_deadline(), Some(now + Duration::from_secs(19)));
    assert!(matches!(
        engine
            .handle(Event::Tick, now + Duration::from_secs(19), wall())
            .as_slice(),
        [Action::Identify { la: 4, .. }]
    ));
}

#[test]
fn restored_osd_replies_do_not_restart_backoff_or_request_known_keys() {
    let now = Instant::now();
    let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)], 1);
    let pa_only = Action::Identify {
        la: 4,
        queries: IdentifyQueries {
            physical: true,
            ..IdentifyQueries::default()
        },
    };
    assert!(
        engine
            .handle(Event::BridgeOnline, now, wall())
            .contains(&pa_only)
    );
    assert!(engine.handle(Event::Tick, now, wall()).is_empty());
    let mut due = now;
    for delay in [10, 20, 40, 80, 160, 320, 640, 1280, 2560, 3600, 3600] {
        assert!(
            frame(
                &mut engine,
                due + Duration::from_millis(100),
                4,
                0,
                "4753747265616D6572",
            )
            .is_empty()
        );
        due += Duration::from_secs(delay);
        assert_eq!(engine.next_deadline(), Some(due));
        assert!(
            engine
                .handle(Event::Tick, due - Duration::from_millis(1), wall())
                .is_empty()
        );
        assert_eq!(
            engine.handle(Event::Tick, due, wall()).as_slice(),
            std::slice::from_ref(&pa_only)
        );
        assert!(!engine.bus.entries[&4].verified);
    }
}

#[test]
fn matching_restored_vendor_and_osd_verify_in_either_order_without_new_confirmation_time() {
    for replies in [
        [(15, "870010FA"), (0, "4753747265616D6572")],
        [(0, "4753747265616D6572"), (0, "870010FA")],
        [(5, "870010FA"), (5, "4753747265616D6572")],
        [(5, "4753747265616D6572"), (5, "870010FA")],
    ] {
        let now = Instant::now();
        let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
        let mut identity = restored_identity(4, 0x2100);
        identity.confirmed_at = wall() - chrono::Duration::days(1);
        let confirmed_at = identity.confirmed_at;
        let mut engine = Engine::new(config, now, vec![identity], 1);
        engine.handle(Event::BridgeOnline, now, wall());
        frame(&mut engine, now, 4, 0, "04");
        for (index, (to, data)) in replies.into_iter().enumerate() {
            let actions = frame(&mut engine, now, 4, to, data);
            if index == 0 {
                assert!(!engine.bus.entries[&4].verified);
                assert!(!engine.identity.roles.contains_key("streamer"));
                assert!(!actions.contains(&Action::WakeTv));
            } else {
                assert!(engine.bus.entries[&4].verified);
                assert!(engine.bus.entries[&4].identified);
                assert_eq!(engine.identity.roles["streamer"], 4);
                assert_eq!(engine.roles["streamer"].power, RolePower::On);
                assert!(actions.contains(&Action::WakeTv));
            }
        }
        assert_eq!(engine.bus.entries[&4].confirmed_at, Some(confirmed_at));
        for seconds in [1, 10, 3600] {
            assert!(
                !engine
                    .handle(Event::Tick, now + Duration::from_secs(seconds), wall())
                    .iter()
                    .any(|action| matches!(action, Action::Identify { .. }))
            );
        }
    }
}

#[test]
fn restored_vendor_change_clears_reconfirmation_and_requires_physical_report() {
    let now = Instant::now();
    let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)], 1);
    engine.handle(Event::BridgeOnline, now, wall());
    frame(&mut engine, now, 4, 0, "4753747265616D6572");
    frame(&mut engine, now, 4, 15, "87080046");
    let entry = &engine.bus.entries[&4];
    assert_eq!(entry.vendor, Some(0x080046));
    assert_eq!(entry.osd, None);
    assert_eq!(entry.physical, Some(0x2100));
    assert!(!entry.verified);
    assert!(!entry.identified);
    frame(&mut engine, now, 4, 15, "87080046");
    frame(&mut engine, now, 4, 0, "4753747265616D6572");
    assert!(!engine.bus.entries[&4].verified);
    assert!(!engine.identity.roles.contains_key("console_b"));
    frame(&mut engine, now, 4, 15, "84210004");
    assert!(engine.bus.entries[&4].verified);
    assert_eq!(engine.identity.roles["console_b"], 4);
}

#[test]
fn physical_verification_resets_backoff_for_still_missing_keys() {
    let (mut engine, now) = setup();
    let now = now + Duration::from_secs(3);
    frame(&mut engine, now, 4, 15, "870010FA");
    for seconds in [10, 30, 70] {
        engine.handle(Event::Tick, now + Duration::from_secs(seconds), wall());
    }
    let verified = now + Duration::from_secs(71);
    frame(&mut engine, verified, 4, 15, "84210004");
    assert_eq!(engine.next_deadline(), Some(now + Duration::from_secs(80)));
    assert_eq!(
        engine.handle(Event::Tick, now + Duration::from_secs(80), wall()),
        [Action::Identify {
            la: 4,
            queries: IdentifyQueries {
                osd: true,
                ..IdentifyQueries::default()
            },
        }]
    );
}

#[test]
fn simultaneous_identification_is_spread_one_address_per_second() {
    let (mut engine, now) = setup();
    let now = now + Duration::from_secs(3);
    for (index, la) in [1, 4, 8, 11].into_iter().enumerate() {
        let actions = frame(&mut engine, now, la, 0, "");
        if index == 0 {
            assert_eq!(
                actions,
                [Action::Identify {
                    la,
                    queries: IdentifyQueries::ALL,
                }]
            );
        } else {
            assert!(actions.is_empty());
        }
    }
    for (seconds, la) in [(1, 4), (2, 8), (3, 11)] {
        let due = now + Duration::from_secs(seconds);
        assert_eq!(engine.next_deadline(), Some(due));
        assert!(
            engine
                .handle(Event::Tick, due - Duration::from_millis(1), wall())
                .is_empty()
        );
        assert_eq!(
            engine.handle(Event::Tick, due, wall()),
            [Action::Identify {
                la,
                queries: IdentifyQueries::ALL,
            }]
        );
        assert!(frame(&mut engine, due, 11, 0, "").is_empty());
        assert!(engine.handle(Event::Tick, due, wall()).is_empty());
    }
    assert_eq!(engine.next_deadline(), Some(now + Duration::from_secs(10)));
    // The next retry wave retains the same spacing.
    for (seconds, la) in [(10, 1), (11, 4), (12, 8), (13, 11)] {
        let due = now + Duration::from_secs(seconds);
        assert_eq!(engine.next_deadline(), Some(due));
        assert!(matches!(
            engine.handle(Event::Tick, due, wall()).as_slice(),
            [Action::Identify { la: target, .. }] if *target == la
        ));
    }
}

#[test]
fn manual_identification_waits_for_the_shared_send_slot() {
    let (mut engine, now) = setup();
    let now = now + Duration::from_secs(3);
    frame(&mut engine, now, 1, 0, "");
    assert!(
        engine
            .handle(Event::Command(Command::Identify(4)), now, wall())
            .is_empty()
    );
    assert_eq!(engine.decisions.last().unwrap().reason, "command_identify");
    let due = now + Duration::from_secs(1);
    assert_eq!(engine.next_deadline(), Some(due));
    assert_eq!(
        engine.handle(Event::Tick, due, wall()),
        [Action::Identify {
            la: 4,
            queries: IdentifyQueries::ALL,
        }]
    );
    assert!(engine.handle(Event::Tick, due, wall()).is_empty());
}

#[test]
fn four_restored_addresses_due_together_wait_for_separate_ticks_after_resync() {
    let now = Instant::now();
    let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    let identities = [(1, 0x2200), (4, 0x2100), (8, 0x2300), (11, 0x2400)]
        .into_iter()
        .map(|(la, physical)| restored_identity(la, physical))
        .collect();
    let mut engine = Engine::new(config, now, identities, 1);
    engine.handle(Event::BridgeOnline, now, wall());
    assert!(engine.handle(Event::Tick, now, wall()).is_empty());
    assert!(
        engine
            .handle(Event::Tick, now + Duration::from_secs(1), wall())
            .is_empty()
    );
    engine.handle(Event::Tick, now + Duration::from_secs(3), wall());
    for (seconds, la) in [(10, 1), (11, 4), (12, 8), (13, 11)] {
        let due = now + Duration::from_secs(seconds);
        assert_eq!(engine.next_deadline(), Some(due));
        assert!(
            engine
                .handle(Event::Tick, due - Duration::from_millis(1), wall())
                .is_empty()
        );
        assert_eq!(
            engine.handle(Event::Tick, due, wall()),
            [Action::Identify {
                la,
                queries: IdentifyQueries {
                    physical: true,
                    ..IdentifyQueries::default()
                },
            }]
        );
        assert!(engine.handle(Event::Tick, due, wall()).is_empty());
    }
}

#[test]
fn incomplete_restore_queries_only_missing_required_keys_and_still_needs_physical_verification() {
    let now = Instant::now();
    let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    let mut identity = restored_identity(4, 0x2100);
    identity.osd = None;
    let mut engine = Engine::new(config, now, vec![identity], 1);
    engine.handle(Event::BridgeOnline, now, wall());
    assert_eq!(
        engine.handle(Event::Tick, now + Duration::from_secs(10), wall()),
        [Action::Identify {
            la: 4,
            queries: IdentifyQueries {
                physical: true,
                vendor: false,
                osd: true,
            },
        }]
    );
    for seconds in [11, 12] {
        let time = now + Duration::from_secs(seconds);
        frame(&mut engine, time, 4, 0, "4753747265616D6572");
        frame(&mut engine, time, 4, 15, "870010FA");
    }
    assert!(!engine.bus.entries[&4].verified);
    assert_eq!(
        engine.handle(Event::Tick, now + Duration::from_secs(20), wall()),
        [Action::Identify {
            la: 4,
            queries: IdentifyQueries {
                physical: true,
                ..IdentifyQueries::default()
            },
        }]
    );
}

#[test]
fn flapping_identity_keys_preserve_exponential_backoff_even_with_physical_reconfirmation() {
    for (first, second) in [
        ("474E616D652041", "474E616D652042"),
        ("870010FA", "87080046"),
        ("84210004", "84220004"),
    ] {
        for reconfirm_physical in [false, true] {
            let (mut engine, now) = setup();
            let now = now + Duration::from_secs(3);
            frame(&mut engine, now, 4, 15, "84210004");
            frame(&mut engine, now, 4, 0, first);
            let mut query_times = Vec::new();
            for seconds in 1..=150 {
                let time = now + Duration::from_secs(seconds);
                let data = if seconds % 2 == 0 { first } else { second };
                let mut actions = frame(&mut engine, time, 4, 0, data);
                if reconfirm_physical {
                    let physical = engine.bus.entries[&4].physical.unwrap();
                    actions.extend(frame(
                        &mut engine,
                        time,
                        4,
                        15,
                        &format!("84{physical:04X}04"),
                    ));
                }
                if !actions.is_empty() {
                    assert!(matches!(
                        actions.as_slice(),
                        [Action::Identify { la: 4, .. }]
                    ));
                    query_times.push(seconds);
                }
            }
            assert_eq!(query_times, [10, 30, 70, 150]);
            assert_eq!(engine.next_deadline(), Some(now + Duration::from_secs(310)));
        }
    }
}

#[test]
fn wake_frame_from_restored_identity_requeries_immediately() {
    let now = Instant::now();
    let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    let retry = Duration::from_secs(config.timing.identify_retry_s);
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)], 1);
    let pa_only = Action::Identify {
        la: 4,
        queries: IdentifyQueries {
            physical: true,
            ..IdentifyQueries::default()
        },
    };
    assert!(
        engine
            .handle(Event::BridgeOnline, now, wall())
            .contains(&pa_only)
    );
    let last = now + retry * 3;
    for time in [now + retry, last] {
        assert_eq!(
            engine.handle(Event::Tick, time, wall()).as_slice(),
            std::slice::from_ref(&pa_only)
        );
    }
    assert_eq!(engine.next_deadline(), Some(last + retry * 4));
    let wake = last + retry + Duration::from_secs(5);
    assert_eq!(frame(&mut engine, wake, 4, 0, "04"), [pa_only]);
    assert_eq!(engine.next_deadline(), Some(wake + retry));
    assert!(
        frame(
            &mut engine,
            wake + Duration::from_millis(300),
            4,
            15,
            "822100",
        )
        .is_empty()
    );
    frame(
        &mut engine,
        wake + Duration::from_secs(1),
        4,
        15,
        "84210004",
    );
    assert!(engine.bus.entries[&4].verified);
    assert!(engine.bus.entries[&4].identified);
    assert!(
        engine
            .handle(Event::Tick, wake + retry, wall())
            .iter()
            .all(|action| !matches!(action, Action::Identify { .. }))
    );
}

#[test]
fn wake_frame_within_base_interval_does_not_rearm() {
    let now = Instant::now();
    let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    let retry = Duration::from_secs(config.timing.identify_retry_s);
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)], 1);
    engine.handle(Event::BridgeOnline, now, wall());
    assert!(frame(&mut engine, now + Duration::from_secs(2), 4, 0, "04").is_empty());
    assert!(
        engine
            .handle(Event::Tick, now + Duration::from_secs(3), wall())
            .is_empty()
    );
    assert_eq!(engine.next_deadline(), Some(now + retry));
    assert_eq!(
        engine.handle(Event::Tick, now + retry, wall()),
        [Action::Identify {
            la: 4,
            queries: IdentifyQueries {
                physical: true,
                ..IdentifyQueries::default()
            },
        }]
    );
    assert_eq!(engine.next_deadline(), Some(now + retry * 3));
}

#[test]
fn wake_frames_from_identified_devices_never_query() {
    let (mut engine, now) = setup();
    frame(&mut engine, now, 4, 15, "84210004");
    frame(&mut engine, now, 4, 15, "870010FA");
    frame(&mut engine, now, 4, 0, "4753747265616D6572");
    assert!(engine.bus.entries[&4].identified);
    for (index, (to, data)) in [(0, "04"), (15, "822100"), (15, "85")]
        .into_iter()
        .enumerate()
    {
        let time = now + Duration::from_secs(20 * (index as u64 + 1));
        assert!(
            frame(&mut engine, time, 4, to, data)
                .iter()
                .all(|action| !matches!(action, Action::Identify { .. }))
        );
        assert!(
            engine
                .handle(Event::Tick, time + Duration::from_secs(10), wall())
                .iter()
                .all(|action| !matches!(action, Action::Identify { .. }))
        );
    }
}

#[test]
fn wake_rearming_respects_opcode_destination_and_active_source_length() {
    for (to, data, wakes) in [
        (0, "04", true),
        (0, "0D", true),
        (15, "822100", true),
        (15, "85", true),
        (15, "04", false),
        (5, "0D", false),
        (0, "822100", false),
        (15, "82210004", false),
        (0, "85", false),
        (15, "870010FA", false),
        (0, "4753747265616D6572", false),
        (0, "9000", false),
        (15, "36", false),
        (0, "9D2100", false),
    ] {
        let now = Instant::now();
        let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
        let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)], 1);
        engine.handle(Event::BridgeOnline, now, wall());
        for seconds in [10, 30] {
            engine.handle(Event::Tick, now + Duration::from_secs(seconds), wall());
        }
        let actions = frame(&mut engine, now + Duration::from_secs(45), 4, to, data);
        assert_eq!(
            actions
                .iter()
                .any(|action| matches!(action, Action::Identify { la: 4, .. })),
            wakes,
            "to={to}, data={data}",
        );
    }
}

#[test]
fn wake_rearming_after_physical_replacement_queries_missing_keys() {
    let now = Instant::now();
    let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)], 1);
    engine.handle(Event::BridgeOnline, now, wall());
    for seconds in [10, 30] {
        engine.handle(Event::Tick, now + Duration::from_secs(seconds), wall());
    }
    assert_eq!(
        frame(&mut engine, now + Duration::from_secs(45), 4, 15, "822500"),
        [Action::Identify {
            la: 4,
            queries: IdentifyQueries {
                physical: false,
                vendor: true,
                osd: true,
            },
        }]
    );
    assert_eq!(engine.bus.entries[&4].physical, Some(0x2500));
    assert!(!engine.bus.entries[&4].identified);
}

#[test]
fn wake_rearming_waits_for_shared_identification_gate() {
    let now = Instant::now();
    let config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)], 1);
    engine.handle(Event::BridgeOnline, now, wall());
    for seconds in [10, 30] {
        engine.handle(Event::Tick, now + Duration::from_secs(seconds), wall());
    }
    let other_query = now + Duration::from_millis(44_500);
    assert_eq!(
        engine.handle(Event::Command(Command::Identify(8)), other_query, wall()),
        [Action::Identify {
            la: 8,
            queries: IdentifyQueries::ALL,
        }]
    );
    assert!(frame(&mut engine, now + Duration::from_secs(45), 4, 0, "04").is_empty());
    let due = other_query + Duration::from_secs(1);
    assert_eq!(engine.next_deadline(), Some(due));
    assert_eq!(
        engine.handle(Event::Tick, due, wall()),
        [Action::Identify {
            la: 4,
            queries: IdentifyQueries {
                physical: true,
                ..IdentifyQueries::default()
            },
        }]
    );
}

#[test]
fn active_source_flapping_rearms_at_most_once_per_base_interval() {
    let (mut engine, now) = setup();
    let retry = Duration::from_secs(engine.config.timing.identify_retry_s);
    let start = now + Duration::from_secs(3);
    let mut query_times: Vec<_> = frame(&mut engine, start, 4, 15, "84210004")
        .into_iter()
        .filter(|action| matches!(action, Action::Identify { la: 4, .. }))
        .map(|_| start)
        .collect();
    for seconds in (3..=600).step_by(3) {
        let time = start + Duration::from_secs(seconds);
        while let Some(due) = engine.next_deadline().filter(|due| *due <= time) {
            query_times.extend(
                engine
                    .handle(Event::Tick, due, wall())
                    .into_iter()
                    .filter(|action| matches!(action, Action::Identify { la: 4, .. }))
                    .map(|_| due),
            );
        }
        let data = if seconds % 6 == 3 { "822500" } else { "822100" };
        query_times.extend(
            frame(&mut engine, time, 4, 15, data)
                .into_iter()
                .filter(|action| matches!(action, Action::Identify { la: 4, .. }))
                .map(|_| time),
        );
        assert!(!engine.bus.entries[&4].identified);
    }
    assert!(!query_times.is_empty());
    assert!(
        query_times
            .windows(2)
            .all(|pair| pair[1].duration_since(pair[0]) >= retry),
        "identification queries exceeded the base-interval bound: {query_times:?}",
    );
}

fn console_before_address_move() -> (Engine, Instant) {
    let (mut engine, now) = setup();
    let console = std::sync::Arc::make_mut(&mut engine.config)
        .device
        .iter_mut()
        .find(|device| device.role == "console_b")
        .unwrap();
    console.matches.osd = Some("Console B".into());
    let mut restored = restored_identity(11, 0x2300);
    restored.vendor = Some(0x0009bf);
    restored.osd = Some("Console A".into());
    engine.bus.restore(vec![restored], now);
    // A signal buffered for the previous destination occupant must not follow it.
    frame(&mut engine, now, 11, 0, "36");
    frame(&mut engine, now, 8, 15, "84240004");
    frame(&mut engine, now, 8, 15, "87080046");
    frame(&mut engine, now, 8, 0, "47436F6E736F6C652042");
    frame(&mut engine, now, 8, 0, "0D");
    assert_eq!(engine.roles["console_b"].power, RolePower::On);
    engine.handle(Event::Tick, now + Duration::from_secs(2), wall());
    (engine, now + Duration::from_secs(3))
}

fn assert_console_move_retained(engine: &Engine) {
    assert_eq!(engine.roles["console_b"].power, RolePower::On);
    assert_eq!(engine.identity.roles["console_b"], 11);
    assert!(!engine.bus.entries.contains_key(&8));
    assert!(engine.bus.entries[&11].verified);
    assert!(
        engine
            .decisions
            .iter()
            .all(|decision| { decision.reason != "identity_lost" && decision.to.is_none() }),
        "{:?}",
        engine.decisions
    );
}

#[test]
fn logical_address_move_keeps_role_on_without_off_timer() {
    let (mut engine, now) = console_before_address_move();
    assert_eq!(
        frame(&mut engine, now, 11, 15, "84240004"),
        vec![Action::Identify {
            la: 11,
            queries: IdentifyQueries::ALL
        }]
    );
    assert_console_move_retained(&engine);
    let entry = &engine.bus.entries[&11];
    assert_eq!(entry.vendor, Some(0x080046));
    assert_eq!(entry.osd.as_deref(), Some("Console B"));
    assert_eq!(entry.device_type, Some(4));
    assert_eq!(entry.power, BusPower::On);
    assert_eq!(entry.last_seen, Some(now));
    assert_eq!(entry.confirmed_at, Some(wall()));
    assert!(entry.identified);
    for data in ["87080046", "47436F6E736F6C652042"] {
        assert!(frame(&mut engine, now + Duration::from_secs(1), 11, 0, data).is_empty());
        assert_console_move_retained(&engine);
    }
    let after_delay = now + Duration::from_secs(engine.config.timing.off_delay_s + 1);
    assert!(engine.handle(Event::Tick, after_delay, wall()).is_empty());
    assert_console_move_retained(&engine);
    frame(&mut engine, after_delay, 11, 0, "36");
    assert_eq!(engine.roles["console_b"].power, RolePower::Off);
    let off = engine.handle(
        Event::Tick,
        after_delay + Duration::from_secs(engine.config.timing.off_delay_s),
        wall(),
    );
    assert_eq!(
        off,
        [
            Action::TvOff,
            Action::ha("media_player", "turn_off", &engine.config.receiver.entity)
        ]
    );
}

#[test]
fn logical_address_move_via_active_source_keeps_role() {
    let (mut engine, now) = console_before_address_move();
    // Active Source inherits existing verification without inventing a confirmation.
    let confirmed = engine.bus.entries[&8].confirmed_at;
    assert_eq!(
        frame(&mut engine, now, 11, 15, "822400"),
        vec![Action::Identify {
            la: 11,
            queries: IdentifyQueries::ALL
        }]
    );
    assert_console_move_retained(&engine);
    assert_eq!(engine.bus.entries[&11].confirmed_at, confirmed);
    assert_eq!(engine.bus.active_source, Some(0x2400));
    engine.handle(Event::Tick, now + Duration::from_millis(500), wall());
    assert_console_move_retained(&engine);
    frame(
        &mut engine,
        now + Duration::from_secs(1),
        11,
        15,
        "84240004",
    );
    assert_console_move_retained(&engine);
    let after_delay = now + Duration::from_secs(engine.config.timing.off_delay_s + 1);
    assert!(engine.handle(Event::Tick, after_delay, wall()).is_empty());
    assert_console_move_retained(&engine);
}

#[test]
fn same_physical_address_from_a_different_device_still_replaces() {
    for changed_key in ["870009BF", "47436F6E736F6C652041"] {
        let (mut engine, now) = console_before_address_move();
        frame(&mut engine, now, 11, 15, "84240004");
        frame(
            &mut engine,
            now + Duration::from_secs(1),
            11,
            0,
            changed_key,
        );
        let entry = &engine.bus.entries[&11];
        assert_eq!(entry.physical, Some(0x2400));
        assert!(!entry.verified);
        assert_eq!(entry.power, BusPower::Unknown);
        assert_eq!(entry.confirmed_at, None);
        assert!(entry.vendor.is_none() || entry.osd.is_none());
        assert!(!engine.identity.roles.contains_key("console_b"));
        assert_eq!(engine.roles["console_b"].power, RolePower::Off);
        assert!(
            engine
                .decisions
                .iter()
                .any(|decision| decision.reason == "identity_lost")
        );
    }
}

#[test]
fn logical_address_move_preserves_unmatched_buffered_wake() {
    let (mut engine, now) = setup();
    frame(&mut engine, now, 8, 15, "84220001");
    frame(&mut engine, now, 8, 0, "04");
    assert!(!engine.identity.roles.contains_key("kodi"));
    frame(
        &mut engine,
        now + Duration::from_secs(1),
        11,
        15,
        "84220001",
    );
    assert_eq!(engine.bus.entries[&11].power, BusPower::On);
    frame(
        &mut engine,
        now + Duration::from_secs(2),
        11,
        0,
        "474B6F6469",
    );
    assert_eq!(engine.identity.roles["kodi"], 11);
    assert_eq!(engine.roles["kodi"].power, RolePower::On);
}

#[test]
fn logical_address_move_from_restored_identity_waits_for_physical_verification() {
    for move_frame in ["84210004", "822100"] {
        let (mut engine, now) = setup();
        engine.bus.restore(vec![restored_identity(8, 0x2100)], now);
        frame(&mut engine, now, 8, 0, "04");
        frame(
            &mut engine,
            now + Duration::from_secs(1),
            11,
            15,
            move_frame,
        );
        assert!(!engine.bus.entries.contains_key(&8));
        assert_eq!(engine.bus.entries[&11].vendor, Some(0x0010fa));
        assert_eq!(engine.bus.entries[&11].osd.as_deref(), Some("Streamer"));
        if move_frame == "822100" {
            assert!(!engine.bus.entries[&11].verified);
            assert!(!engine.identity.roles.contains_key("streamer"));
            assert_eq!(engine.roles["streamer"].power, RolePower::Off);
            frame(
                &mut engine,
                now + Duration::from_secs(2),
                11,
                15,
                "84210004",
            );
        }
        assert!(engine.bus.entries[&11].verified);
        assert_eq!(engine.identity.roles["streamer"], 11);
        assert_eq!(engine.roles["streamer"].power, RolePower::On);
    }
}

#[test]
fn logical_address_move_retargets_pending_standby_without_replaying_wake() {
    let (mut engine, now) = console_before_address_move();
    engine.handle(Event::Command(Command::SetMode(Mode::Active)), now, wall());
    engine.handle(
        Event::Command(Command::Standby("console_b".into())),
        now,
        wall(),
    );
    let stopping_id = engine
        .decisions
        .iter()
        .find(|decision| decision.reason == "manual_standby")
        .unwrap()
        .id;
    assert_eq!(engine.decisions.last().unwrap().parent, None);
    assert_eq!(
        engine.decisions.last().unwrap().trigger,
        Trigger::Command {
            command: Command::Standby("console_b".into()),
        }
    );
    assert_eq!(engine.roles["console_b"].power, RolePower::Off);
    frame(
        &mut engine,
        now + Duration::from_secs(1),
        11,
        15,
        "84240004",
    );
    assert_eq!(engine.roles["console_b"].power, RolePower::Off);
    assert!(
        engine
            .decisions
            .iter()
            .all(|decision| decision.to.is_none())
    );
    // Late replies to the old address must not count against the new target.
    engine.handle(
        Event::SendResult {
            to: 8,
            data: vec![0x36],
            status: choregos::engine::SendStatus::Nack,
            attempts: None,
        },
        now + Duration::from_secs(1),
        wall(),
    );
    let retry_at = now + Duration::from_millis(engine.config.timing.takeover_settle_ms);
    let retry = engine.handle(Event::Tick, retry_at, wall());
    assert_eq!(
        retry,
        vec![
            Action::CecSend {
                to: 11,
                data: vec![0x36]
            },
            Action::CecSend {
                to: 11,
                data: vec![0x8f]
            },
        ]
    );
    assert_eq!(engine.expectations(stopping_id).unwrap()[0].attempts, 1);
    frame(&mut engine, retry_at, 11, 0, "9000");
    assert_eq!(engine.roles["console_b"].power, RolePower::Off);
    frame(&mut engine, retry_at, 11, 0, "9001");
    let after = retry_at + Duration::from_millis(engine.config.timing.takeover_settle_ms);
    let actions = engine.handle(Event::Tick, after, wall());
    assert_eq!(engine.verdicts[0].id, stopping_id);
    assert_eq!(engine.verdicts[0].verdict, Verdict::Achieved);
    assert!(matches!(
        engine.verdicts[0].expectations[0].outcome,
        Outcome::CecOnly { .. }
    ));
    assert!(
        actions
            .iter()
            .all(|action| !matches!(action, Action::CecSend { .. }))
    );
    assert_no_standby_decision(&engine, "console_b");
    assert!(engine.next_decision_id > stopping_id);
}

#[test]
fn logical_address_move_keeps_presence_polling_on_the_new_address() {
    let (mut engine, now) = setup();
    frame(&mut engine, now, 8, 15, "84230004");
    let interval = Duration::from_secs(engine.config.timing.presence_poll_s);
    engine.handle(Event::Tick, now + interval, wall());
    // The move is also a presence reply, clearing the old outstanding probe.
    frame(
        &mut engine,
        now + interval + Duration::from_secs(1),
        11,
        15,
        "84230004",
    );
    assert_eq!(engine.roles["console_a"].power, RolePower::On);
    let actions = engine.handle(Event::Tick, now + interval * 2, wall());
    assert!(actions.contains(&Action::PresencePoll { la: 11 }));
    assert!(!actions.contains(&Action::PresencePoll { la: 8 }));
    assert_eq!(engine.roles["console_a"].power, RolePower::On);
}

#[test]
fn moved_identity_is_corrected_when_the_new_address_reports_a_different_vendor() {
    let (mut engine, now) = console_before_address_move();
    // The new LA has not supplied any keys yet: keep the room on provisionally.
    let actions = frame(&mut engine, now, 4, 15, "84240004");
    assert_eq!(engine.identity.roles["console_b"], 4);
    assert_eq!(engine.roles["console_b"].power, RolePower::On);
    assert_eq!(
        actions,
        vec![Action::Identify {
            la: 4,
            queries: IdentifyQueries::ALL
        }]
    );
    assert!(engine.decisions.is_empty());
    let actions = frame(&mut engine, now + Duration::from_secs(1), 4, 15, "870010FA");
    assert!(actions.is_empty());
    assert!(!engine.identity.roles.contains_key("console_b"));
    assert!(!engine.identity.roles.contains_key("streamer"));
    assert!(!engine.bus.entries[&4].verified);
    assert_eq!(
        engine
            .decisions
            .iter()
            .map(|decision| (
                decision.role.as_deref(),
                decision.from,
                decision.to,
                decision.reason.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![(
            Some("console_b"),
            Some(RolePower::On),
            Some(RolePower::Off),
            "identity_lost"
        )]
    );
    frame(&mut engine, now + Duration::from_secs(2), 4, 15, "84240004");
    assert_eq!(engine.identity.roles["streamer"], 4);
    assert!(!engine.identity.roles.contains_key("console_b"));
    assert!(engine.decisions.is_empty());
}

#[test]
fn destination_identity_keys_prevent_inheritance_without_physical_replacement() {
    for key in ["870010FA", "4753747265616D6572"] {
        let (mut engine, now) = console_before_address_move();
        frame(&mut engine, now, 4, 15, key);
        frame(&mut engine, now + Duration::from_secs(1), 4, 15, "84240004");
        assert!(!engine.bus.entries.contains_key(&8));
        assert!(!engine.identity.roles.contains_key("console_b"));
        let entry = &engine.bus.entries[&4];
        assert!(entry.verified);
        assert_eq!(entry.power, BusPower::Unknown);
        if key == "870010FA" {
            assert_eq!(entry.vendor, Some(0x0010fa));
            assert_eq!(entry.osd, None);
            assert_eq!(engine.identity.roles["streamer"], 4);
        } else {
            assert_eq!(entry.osd.as_deref(), Some("Streamer"));
            assert_eq!(entry.vendor, None);
        }
        assert_eq!(
            engine
                .decisions
                .iter()
                .map(|decision| decision.reason.as_str())
                .collect::<Vec<_>>(),
            vec!["identity_lost"]
        );
    }
}

#[test]
fn retarget_after_last_standby_retry_clears_old_address_send_accounting() {
    for acked in [false, true] {
        let (mut engine, now) = console_before_address_move();
        engine.handle(Event::Command(Command::SetMode(Mode::Active)), now, wall());
        let settle = Duration::from_millis(engine.config.timing.takeover_settle_ms);
        let retries = engine.config.timing.standby_retries;
        engine.handle(
            Event::Command(Command::Standby("console_b".into())),
            now,
            wall(),
        );
        for round in 0..=retries {
            let sent_at = now + settle * u32::from(round);
            if round > 0 {
                engine.handle(Event::Tick, sent_at, wall());
            }
            for opcode in [0x36, 0x8f] {
                engine.handle(
                    Event::SendResult {
                        to: 8,
                        data: vec![opcode],
                        status: if acked {
                            choregos::engine::SendStatus::Ack
                        } else {
                            choregos::engine::SendStatus::Nack
                        },
                        attempts: None,
                    },
                    sent_at,
                    wall(),
                );
            }
        }
        let last_retry = now + settle * u32::from(retries);
        frame(
            &mut engine,
            last_retry + Duration::from_millis(1),
            11,
            15,
            "84240004",
        );
        engine.handle(Event::Tick, last_retry + settle, wall());
        assert_standby_unknown(&engine, "console_b", false);
    }
}

#[test]
fn console_address_hop_during_receiver_power_up_keeps_room_on() {
    let now = Instant::now();
    let mut config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    config
        .device
        .iter_mut()
        .find(|device| device.role == "console_b")
        .unwrap()
        .matches
        .osd = Some("Console B".into());
    let mut restored = restored_identity(11, 0x2300);
    restored.vendor = Some(0x0009bf);
    restored.osd = Some("Console A".into());
    let mut engine = Engine::new(config, now, vec![restored], 1);
    engine.handle(Event::BridgeOnline, now, wall());
    let mut reasons = Vec::new();
    for data in ["84240004", "87080046", "47436F6E736F6C652042"] {
        frame(&mut engine, now, 8, 15, data);
        reasons.extend(
            engine
                .decisions
                .iter()
                .map(|decision| decision.reason.clone()),
        );
    }
    let actions = frame(&mut engine, now, 8, 0, "0D");
    assert!(actions.contains(&Action::WakeTv));
    assert!(actions.iter().any(|action| service(action, "turn_on")));
    assert_eq!(engine.roles["console_b"].power, RolePower::On);
    reasons.extend(
        engine
            .decisions
            .iter()
            .map(|decision| decision.reason.clone()),
    );
    let settled_at = now + Duration::from_millis(engine.config.timing.takeover_settle_ms);
    engine.handle(Event::Tick, settled_at, wall());
    reasons.extend(
        engine
            .decisions
            .iter()
            .map(|decision| decision.reason.clone()),
    );
    let moved_at = settled_at + Duration::from_secs(1);
    frame(&mut engine, moved_at, 11, 15, "84240004");
    assert_console_move_retained(&engine);
    reasons.extend(
        engine
            .decisions
            .iter()
            .map(|decision| decision.reason.clone()),
    );
    let after_delay = moved_at + Duration::from_secs(engine.config.timing.off_delay_s + 1);
    let actions = engine.handle(Event::Tick, after_delay, wall());
    assert!(
        actions
            .iter()
            .all(|action| *action != Action::TvOff && !service(action, "turn_off"))
    );
    assert_console_move_retained(&engine);
    reasons.extend(
        engine
            .decisions
            .iter()
            .map(|decision| decision.reason.clone()),
    );
    for data in ["87080046", "47436F6E736F6C652042", "822400"] {
        frame(&mut engine, after_delay, 11, 15, data);
        assert_console_move_retained(&engine);
        reasons.extend(
            engine
                .decisions
                .iter()
                .map(|decision| decision.reason.clone()),
        );
    }
    assert_eq!(reasons, vec!["TextViewOn", "takeover_settled"]);
}

#[test]
fn retargeted_standby_does_not_keep_old_address_acknowledgements() {
    let (mut engine, now) = console_before_address_move();
    engine.handle(Event::Command(Command::SetMode(Mode::Active)), now, wall());
    engine.handle(
        Event::Command(Command::Standby("console_b".into())),
        now,
        wall(),
    );
    engine.handle(
        Event::SendResult {
            to: 8,
            data: vec![0x36],
            status: choregos::engine::SendStatus::Ack,
            attempts: None,
        },
        now,
        wall(),
    );
    frame(
        &mut engine,
        now + Duration::from_secs(1),
        11,
        15,
        "84240004",
    );
    let settle = Duration::from_millis(engine.config.timing.takeover_settle_ms);
    let retries = engine.config.timing.standby_retries;
    for round in 1..=retries {
        let sent_at = now + settle * u32::from(round);
        engine.handle(Event::Tick, sent_at, wall());
        for opcode in [0x36, 0x8f] {
            engine.handle(
                Event::SendResult {
                    to: 11,
                    data: vec![opcode],
                    status: choregos::engine::SendStatus::Nack,
                    attempts: None,
                },
                sent_at,
                wall(),
            );
        }
    }
    engine.handle(Event::Tick, now + settle * (u32::from(retries) + 1), wall());
    assert_standby_unknown(&engine, "console_b", true);
}

fn console_physical_change_config() -> Config {
    let mut config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    let console = config
        .device
        .iter_mut()
        .find(|device| device.role == "console_a")
        .unwrap();
    console.matches.physical = None;
    console.matches.vendor = Some("0009BF".into());
    console.matches.osd = Some("Console A".into());
    config
}

fn console_before_physical_change(mode: Mode) -> (Engine, Instant) {
    let now = Instant::now();
    let mut config = console_physical_change_config();
    config.mode = mode;
    let mut engine = Engine::new(config, now, vec![], 1);
    engine.handle(Event::BridgeOnline, now, wall());
    for data in ["84230004", "870009BF", "47436F6E736F6C652041", "822300"] {
        frame(&mut engine, now, 8, 15, data);
    }
    engine.handle(Event::Tick, now + Duration::from_secs(2), wall());
    assert_eq!(engine.identity.roles["console_a"], 8);
    assert_eq!(engine.roles["console_a"].power, RolePower::On);
    (engine, now + Duration::from_secs(3))
}

#[test]
fn physical_address_flap_during_receiver_power_up_keeps_role_on() {
    let now = Instant::now();
    let mut identity = restored_identity(8, 0x2300);
    identity.vendor = Some(0x0009bf);
    identity.osd = Some("Console A".into());
    let mut engine = Engine::new(console_physical_change_config(), now, vec![identity], 1);
    engine.handle(Event::BridgeOnline, now, wall());
    assert!(
        engine
            .roles
            .values()
            .all(|role| role.power == RolePower::Off)
    );
    let mut reasons = Vec::new();
    let mut actions = Vec::new();
    let mut collect = |engine: &mut Engine, effects: Vec<Action>| {
        assert!(
            effects
                .iter()
                .all(|action| *action != Action::TvOff && !service(action, "turn_off"))
        );
        actions.extend(effects);
        reasons.extend(
            engine
                .decisions
                .drain(..)
                .filter(|decision| decision.role.as_deref() == Some("console_a"))
                .map(|decision| decision.reason),
        );
        assert_eq!(engine.identity.roles["console_a"], 8);
        assert_eq!(engine.roles["console_a"].power, RolePower::On);
    };
    for (ms, data, to) in [
        (0, Some("84230004"), 15),
        (800, Some("47436F6E736F6C652041"), 5),
        (2_000, None, 0),
        (2_100, Some("870009BF"), 15),
        (2_700, Some("84100004"), 15),
        (3_050, Some("870009BF"), 15),
        (3_400, Some("47436F6E736F6C652041"), 0),
        (5_100, Some("84100004"), 15),
        (6_100, Some("84230004"), 15),
        (10_300, Some("870009BF"), 15),
        (10_400, Some("822300"), 15),
    ] {
        let at = now + Duration::from_millis(ms);
        while let Some(deadline) = engine.next_deadline().filter(|deadline| *deadline < at) {
            let effects = engine.handle(Event::Tick, deadline, wall());
            collect(&mut engine, effects);
        }
        let effects = if let Some(data) = data {
            frame(&mut engine, at, 8, to, data)
        } else {
            engine.handle(Event::Tick, at, wall())
        };
        collect(&mut engine, effects);
    }
    assert_eq!(reasons, ["ReportPhysicalAddress", "takeover_settled"]);
    assert_eq!(actions.iter().filter(|a| **a == Action::WakeTv).count(), 1);
    assert!(actions.iter().any(|action| service(action, "turn_on")));
    assert_eq!(engine.bus.entries[&8].physical, Some(0x2300));
    assert_eq!(engine.bus.active_source, Some(0x2300));
}

#[test]
fn physical_address_change_requeries_and_a_different_device_is_corrected() {
    for mode in [Mode::Shadow, Mode::Active] {
        for changed_key in ["870010FA", "4753747265616D6572"] {
            let (mut engine, now) = console_before_physical_change(mode);
            let actions = frame(&mut engine, now, 8, 15, "84100004");
            assert_eq!(engine.identity.roles["console_a"], 8);
            assert_eq!(engine.roles["console_a"].power, RolePower::On);
            assert_eq!(
                actions,
                vec![Action::Identify {
                    la: 8,
                    queries: IdentifyQueries::ALL
                }]
            );
            assert!(engine.decisions.is_empty());
            frame(
                &mut engine,
                now + Duration::from_secs(1),
                8,
                15,
                changed_key,
            );
            assert!(!engine.identity.roles.contains_key("console_a"));
            assert_eq!(engine.roles["console_a"].power, RolePower::Off);
            assert!(!engine.bus.entries[&8].verified);
            assert!(
                engine
                    .decisions
                    .iter()
                    .any(|decision| decision.reason == "identity_lost")
            );
        }
    }
}

#[test]
fn physical_address_change_unbinds_roles_matched_by_physical() {
    let (mut engine, now) = setup();
    for data in ["84230004", "870009BF", "47436F6E736F6C652041", "04"] {
        frame(&mut engine, now, 8, 0, data);
    }
    assert_eq!(engine.identity.roles["console_a"], 8);
    frame(&mut engine, now + Duration::from_secs(1), 8, 15, "84100004");
    assert!(!engine.identity.roles.contains_key("console_a"));
    assert_eq!(engine.roles["console_a"].power, RolePower::Off);
    assert_eq!(engine.bus.entries[&8].vendor, Some(0x0009bf));
    assert!(
        engine
            .decisions
            .iter()
            .any(|decision| decision.reason == "identity_lost")
    );
    frame(&mut engine, now + Duration::from_secs(2), 8, 15, "84230004");
    assert_eq!(engine.identity.roles["console_a"], 8);
    assert_eq!(engine.roles["console_a"].power, RolePower::On);
}

#[test]
fn physical_address_change_with_unknown_keys_still_replaces() {
    for (case, keys, verified) in [
        ("no keys", &[][..], true),
        ("vendor only", &["870009BF"][..], true),
        ("OSD only", &["47436F6E736F6C652041"][..], true),
        (
            "both keys, unverified",
            &["870009BF", "47436F6E736F6C652041"][..],
            false,
        ),
    ] {
        let (mut engine, now) = setup();
        frame(&mut engine, now, 8, 15, "84230004");
        for key in keys {
            frame(&mut engine, now, 8, 15, key);
        }
        engine.bus.entries.get_mut(&8).unwrap().verified = verified;
        frame(&mut engine, now, 8, 15, "822300");
        frame(&mut engine, now + Duration::from_secs(1), 8, 15, "84100001");
        let entry = &engine.bus.entries[&8];
        assert_eq!(entry.vendor, None, "{case}");
        assert_eq!(entry.osd, None, "{case}");
        assert_eq!(entry.physical, Some(0x1000));
        assert_eq!(entry.device_type, Some(1));
        assert_eq!(entry.power, BusPower::Unknown);
        assert_eq!(engine.bus.active_source, None);
        assert!(!engine.identity.roles.contains_key("console_a"));
    }
}

#[test]
fn physical_address_change_via_active_source_preserves_confirmation_and_keys() {
    let (mut engine, now) = console_before_physical_change(Mode::Shadow);
    let before = engine.bus.entries[&8].clone();
    let actions = engine.handle(
        Event::Frame(Frame::from_hex(8, 15, "821000").unwrap()),
        now,
        wall() + chrono::Duration::seconds(10),
    );
    let after = &engine.bus.entries[&8];
    assert_eq!(after.physical, Some(0x1000));
    assert_eq!(after.vendor, before.vendor);
    assert_eq!(after.osd, before.osd);
    assert_eq!(after.device_type, before.device_type);
    assert_eq!(after.confirmed_at, before.confirmed_at);
    assert_eq!(after.verified, before.verified);
    assert_eq!(after.power, BusPower::On);
    assert_eq!(after.last_seen, Some(now));
    assert_eq!(engine.bus.active_source, Some(0x1000));
    assert_eq!(engine.identity.roles["console_a"], 8);
    assert!(engine.decisions.is_empty());
    assert_eq!(
        actions,
        vec![Action::Identify {
            la: 8,
            queries: IdentifyQueries::ALL
        }]
    );
}

#[test]
fn physical_address_change_queues_full_query_behind_shared_slot() {
    let (mut engine, now) = console_before_physical_change(Mode::Shadow);
    engine.handle(Event::Command(Command::Identify(8)), now, wall());
    let changed_at = now + Duration::from_millis(100);
    let confirmed = wall() + chrono::Duration::seconds(10);
    let actions = engine.handle(
        Event::Frame(Frame::from_hex(8, 15, "84100001").unwrap()),
        changed_at,
        confirmed,
    );
    assert!(actions.is_empty());
    let entry = &engine.bus.entries[&8];
    assert_eq!(entry.power, BusPower::On);
    assert_eq!(entry.last_seen, Some(changed_at));
    assert_eq!(entry.confirmed_at, Some(confirmed));
    assert!(entry.verified);
    assert_eq!(engine.bus.active_source, Some(0x1000));
    assert_eq!(
        engine.handle(Event::Tick, now + Duration::from_secs(1), wall()),
        vec![Action::Identify {
            la: 8,
            queries: IdentifyQueries::ALL
        }]
    );
}

#[test]
fn physical_address_change_of_stopped_role_does_not_rewake_it() {
    let (mut engine, now) = console_before_address_move();
    engine.handle(Event::Command(Command::SetMode(Mode::Active)), now, wall());
    engine.handle(
        Event::Command(Command::Standby("console_b".into())),
        now,
        wall(),
    );
    frame(&mut engine, now + Duration::from_secs(1), 8, 15, "84100004");
    frame(&mut engine, now + Duration::from_secs(1), 8, 0, "9000");
    assert_eq!(engine.roles["console_b"].power, RolePower::Off);
    let retry_at = now + Duration::from_millis(engine.config.timing.takeover_settle_ms);
    let actions = engine.handle(Event::Tick, retry_at, wall());
    assert!(!engine.identity.roles.contains_key("console_b"));
    assert!(
        actions
            .iter()
            .all(|action| !matches!(action, Action::CecSend { .. }))
    );
    assert_no_standby_decision(&engine, "console_b");
}

#[test]
fn physical_address_change_defers_to_another_logical_address_holding_the_pa() {
    let (mut engine, now) = console_before_physical_change(Mode::Shadow);
    identify_atv(&mut engine, now);
    frame(&mut engine, now + Duration::from_secs(1), 8, 15, "84210004");
    assert!(!engine.bus.entries.contains_key(&4));
    let entry = &engine.bus.entries[&8];
    assert_eq!(entry.physical, Some(0x2100));
    assert_eq!(entry.vendor, Some(0x0010fa));
    assert_eq!(entry.osd.as_deref(), Some("Apple TV"));
    assert_eq!(engine.identity.roles["streamer"], 8);
    assert!(!engine.identity.roles.contains_key("console_a"));
    assert_eq!(engine.bus.active_source, None);
}

#[test]
fn physical_address_change_from_bridge_never_creates_a_bus_identity() {
    let (mut engine, now) = setup();
    for data in ["84250004", "870009BF", "47436F6E736F6C652041", "822500"] {
        frame(&mut engine, now, 0, 15, data);
    }
    frame(&mut engine, now + Duration::from_secs(1), 0, 15, "84100004");
    assert!(!engine.bus.entries.contains_key(&0));
    assert!(engine.identity.roles.is_empty());
    assert_eq!(engine.bus.active_source, None);
}

#[test]
fn different_device_claiming_a_known_address_does_not_wake_the_old_role() {
    for mode in [Mode::Active, Mode::Shadow] {
        let (mut engine, now) = console_before_physical_change(mode);
        frame(&mut engine, now, 8, 0, "36");
        assert_eq!(engine.roles["console_a"].power, RolePower::Off);
        assert_eq!(engine.identity.roles["console_a"], 8);
        assert!(engine.bus.entries[&8].verified);
        for (ms, data, to) in [
            (100, "84240004", 15),
            (200, "87080046", 15),
            (300, "47436F6E736F6C652042", 0),
            (400, "04", 0),
            (500, "822400", 15),
        ] {
            let effects = frame(&mut engine, now + Duration::from_millis(ms), 8, to, data);
            assert_eq!(engine.roles["console_a"].power, RolePower::Off);
            assert!(!engine.identity.roles.contains_key("console_a"));
            assert!(
                engine
                    .decisions
                    .iter()
                    .all(|decision| decision.role.as_deref() != Some("console_a"))
            );
            if ms <= 300 {
                assert!(
                    effects
                        .iter()
                        .all(|action| matches!(action, Action::Identify { .. }))
                );
            }
        }
        assert_eq!(engine.identity.roles["console_b"], 8);
        assert_eq!(engine.roles["console_b"].power, RolePower::On);
        assert_eq!(engine.bus.active_source, Some(0x2400));
    }
}

#[test]
fn native_transport_failures_neither_ack_nor_establish_target_absence() {
    use choregos::engine::SendStatus;
    for status in [SendStatus::ArbLost, SendStatus::LineError, SendStatus::Busy] {
        for later_nacks in [false, true] {
            let (mut engine, takeover, settle) =
                takeover_from_streamer_with_policy(Mode::Active, Takeover::CecStandby);
            for round in 0..=engine.config.timing.standby_retries {
                let now = takeover + settle * u32::from(round) + Duration::from_millis(1);
                for opcode in [0x36, 0x8f] {
                    engine.handle(
                        Event::SendResult {
                            to: 4,
                            data: vec![opcode],
                            status,
                            attempts: Some(5),
                        },
                        now,
                        wall(),
                    );
                    if later_nacks {
                        engine.handle(
                            Event::SendResult {
                                to: 4,
                                data: vec![opcode],
                                status: SendStatus::Nack,
                                attempts: Some(1),
                            },
                            now,
                            wall(),
                        );
                    }
                }
                engine.handle(
                    Event::Tick,
                    takeover + settle * (u32::from(round) + 1),
                    wall(),
                );
            }
            assert_standby_unknown(&engine, "streamer", later_nacks);
        }
    }
}

#[test]
fn raw_send_limit_is_specific_to_the_selected_bridge() {
    use choregos::config::BridgeConfig;
    for bridge in [
        BridgeConfig::Tasmota {
            topic: "cec".into(),
            logical_address: 0,
        },
        BridgeConfig::Aether {
            node: "cec-bridge".into(),
            logical_address: 0,
        },
    ] {
        let max = bridge.max_raw_bytes();
        let mut config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
        config.bridge = bridge;
        let now = Instant::now();
        let mut engine = Engine::new(config, now, vec![], 1);
        for len in [14, 15, 16] {
            let data = vec![0x44; len];
            let actions = engine.handle(
                Event::Command(Command::SendRaw {
                    to: 4,
                    data: data.clone(),
                }),
                now,
                wall(),
            );
            assert_eq!(
                actions,
                if len <= max {
                    vec![Action::CecSend { to: 4, data }]
                } else {
                    vec![]
                }
            );
        }
    }
}

#[test]
fn promiscuous_bridge_address_frames_never_learn_or_signal_a_role() {
    let (mut engine, now) = setup();
    for data in ["84210004", "870010FA", "4753747265616D6572", "822100", "04"] {
        assert!(frame(&mut engine, now, 0, 15, data).is_empty());
        assert!(engine.bus.entries.is_empty());
        assert!(engine.decisions.is_empty());
        assert!(engine.identity.roles.is_empty());
        assert!(engine.intent.active_role.is_none());
        assert!(engine.bus.active_source.is_none());
    }
    // Also exercise BusModel directly: callers other than the engine must not insert own-LA entries.
    let f = Frame::from_hex(0, 15, "84210004").unwrap();
    assert!(engine.bus.handle(&f, now, wall(), false).is_empty());
    assert!(engine.bus.entries.is_empty());
}

#[test]
fn real_native_identify_capture_replays_announced_devices_and_excludes_own_echoes() {
    use choregos::runtime::{
        bridge::Correlation,
        trace::{self, TraceEntry},
    };
    let mut config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
    config.bridge = choregos::config::BridgeConfig::Aether {
        node: "cec-bridge".into(),
        logical_address: 0,
    };
    let text = include_str!("../fixtures/aether-identify.jsonl");
    let mut output = vec![];
    choregos::runtime::cli::replay(config.clone(), text, &mut output).unwrap();
    let start = Instant::now();
    let mut engine = Engine::new(config, start, vec![], 1);
    let mut correlation = Correlation::default();
    let mut epoch = None;
    let mut outcomes = 0;
    for line in text.lines() {
        let (ts, entry) = trace::parse_checked(line).unwrap().unwrap();
        let epoch = *epoch.get_or_insert(ts);
        let now = start + (ts - epoch).to_std().unwrap();
        while let Some(deadline) = engine.next_deadline().filter(|d| *d < now) {
            engine.handle(Event::Tick, deadline, ts);
        }
        if let TraceEntry::Sent { to, data, id, .. } = entry {
            correlation.record_send(id, to, data, now);
        } else {
            for event in correlation.apply(&entry, now).events {
                if matches!(event, Event::SendResult { .. }) {
                    outcomes += 1;
                }
                engine.handle(event, now, ts);
            }
        }
        assert!(
            !engine.bus.entries.contains_key(&0),
            "own RX echo inserted bridge LA"
        );
    }
    assert_eq!(
        engine.bus.entries.keys().copied().collect::<Vec<_>>(),
        [1, 4, 5, 11]
    );
    assert!(engine.bus.entries[&4].identified);
    assert!(engine.bus.entries[&5].identified);
    assert_eq!(engine.bus.entries[&4].physical, Some(0x2100));
    assert_eq!(engine.bus.entries[&5].physical, Some(0x2000));
    assert_eq!(engine.bus.entries[&11].osd.as_deref(), Some("Console B"));
    // PA replies for 1/11 were timing errors in the actual capture: no invented PA or identification.
    assert!(engine.bus.entries[&1].physical.is_none());
    assert!(engine.bus.entries[&11].physical.is_none());
    assert!(!engine.bus.entries[&1].identified);
    assert!(!engine.bus.entries[&11].identified);
    assert_eq!(outcomes, 26);
}

#[test]
fn nack_cut_osd_reply_keeps_verified_identity_and_running_role() {
    use choregos::runtime::{bridge::Correlation, trace::TraceEntry};
    use serde_json::json;

    let (mut engine, start) = setup();
    frame(&mut engine, start, 1, 15, "84220001");
    frame(&mut engine, start, 1, 0, "474B6F6469");
    frame(&mut engine, start, 1, 15, "822200");
    assert_eq!(engine.identity.roles["kodi"], 1);
    assert_eq!(engine.roles["kodi"].power, RolePower::On);
    let now = start + Duration::from_secs(4);
    let entry = TraceEntry::AetherBus {
        node: "cec-bridge".into(),
        payload: json!({"ev":"rx","seq":1,"from":1,"to":0,"data":"474B6F","ack":false,"t_us":456}),
    };
    let decoded = Correlation::default().apply(&entry, now);
    let raw = decoded.raw_received.unwrap();
    assert_eq!(raw.data(), "474B6F");
    assert_eq!(raw.ack, Some(false));
    assert!(matches!(&decoded.events[..], [Event::Frame(Frame {
        opcode: None, params, ack: Some(false), t_us: Some(456), ..
    })] if params.is_empty()));
    for event in decoded.events {
        engine.handle(event, now, wall());
        assert!(!engine.decisions.iter().any(|d| d.reason == "identity_lost"));
    }
    engine.handle(Event::Tick, now + Duration::from_secs(4), wall());
    assert_eq!(engine.bus.entries[&1].osd.as_deref(), Some("Kodi"));
    assert!(engine.bus.entries[&1].verified);
    assert_eq!(engine.identity.roles["kodi"], 1);
    assert_eq!(engine.roles["kodi"].power, RolePower::On);
    assert_eq!(engine.bus.entries[&1].last_seen, Some(now));
    assert!(!engine.decisions.iter().any(|d| d.reason == "identity_lost"));
}

#[test]
fn decision_ids_are_monotonic_and_triggers_identify_inputs_and_due_timers() {
    let (mut engine, now) = setup();
    let mut ids = vec![];
    identify_atv(&mut engine, now);
    ids.extend(engine.decisions.iter().map(|decision| decision.id));
    frame(&mut engine, now, 4, 0, "04");
    let wake = engine
        .decisions
        .iter()
        .find(|decision| decision.to == Some(RolePower::On))
        .unwrap();
    assert_eq!(
        wake.trigger,
        Trigger::Frame {
            from: 4,
            to: 0,
            data: "04".into()
        }
    );
    let wake_id = wake.id;
    ids.extend(engine.decisions.iter().map(|decision| decision.id));
    let settle = now + Duration::from_millis(engine.config.timing.takeover_settle_ms);
    // A timer due during an unrelated command must not inherit its command trigger.
    engine.handle(
        Event::Command(Command::SetMode(Mode::Active)),
        settle,
        wall(),
    );
    let command = engine
        .decisions
        .iter()
        .find(|decision| decision.reason == "command_set_mode")
        .unwrap();
    assert_eq!(
        command.trigger,
        Trigger::Command {
            command: Command::SetMode(Mode::Active)
        }
    );
    assert_eq!(command.parent, None);
    let settled = engine
        .decisions
        .iter()
        .find(|decision| decision.reason == "takeover_settled")
        .unwrap();
    assert_eq!(
        settled.trigger,
        Trigger::Timer {
            name: TimerKind::TakeoverSettle
        }
    );
    assert_eq!(settled.parent, Some(wake_id));
    ids.extend(engine.decisions.iter().map(|decision| decision.id));
    frame(&mut engine, settle, 4, 0, "36");
    ids.extend(engine.decisions.iter().map(|decision| decision.id));
    engine.handle(
        Event::Tick,
        settle + Duration::from_secs(engine.config.timing.off_delay_s),
        wall(),
    );
    assert!(
        engine
            .decisions
            .iter()
            .any(|decision| decision.reason == "off_timer_expired"
                && decision.trigger
                    == Trigger::Timer {
                        name: TimerKind::RoomOff
                    })
    );
    ids.extend(engine.decisions.iter().map(|decision| decision.id));
    assert!(!ids.is_empty());
    assert_eq!(ids[0], 1);
    assert!(ids.windows(2).all(|pair| pair[1] == pair[0] + 1), "{ids:?}");
}

#[test]
fn standby_expectation_stays_on_the_wake_decision_across_retries() {
    let (mut engine, takeover, settle) = takeover_from_streamer();
    let wake = engine
        .decisions
        .iter()
        .find(|decision| decision.to == Some(RolePower::On))
        .unwrap();
    let parent = wake.id;
    assert!(
        wake.actions
            .iter()
            .any(|action| matches!(action, Action::CecSend { to: 4, data } if data == &[0x36]))
    );
    for round in 1..=u32::from(engine.config.timing.standby_retries) + 1 {
        engine.handle(Event::Tick, takeover + settle * round, wall());
        assert_no_standby_decision(&engine, "streamer");
        for followup in &engine.decisions {
            if followup.reason == "takeover_settled" {
                assert_eq!(followup.parent, Some(parent));
            }
        }
        assert_eq!(
            engine
                .expectations(parent)
                .unwrap()
                .iter()
                .find(|expectation| expectation.subject == "role:streamer")
                .unwrap()
                .attempts,
            round.min(u32::from(engine.config.timing.standby_retries)) as u8
        );
    }
    assert_standby_unknown(&engine, "streamer", false);
}

#[test]
fn mixed_all_off_attaches_every_dispatched_stop_to_the_command_decision() {
    let (mut engine, now) = console_before_address_move();
    engine.handle(Event::Command(Command::SetMode(Mode::Active)), now, wall());
    engine.handle(
        Event::Command(Command::Standby("console_b".into())),
        now,
        wall(),
    );
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 0, "04");
    assert_eq!(engine.roles["streamer"].power, RolePower::On);
    assert_eq!(engine.roles["console_b"].power, RolePower::Off);
    engine.handle(Event::Command(Command::AllOff), now, wall());
    let stop = engine
        .decisions
        .iter()
        .find(|decision| decision.reason == "all_off")
        .unwrap();
    assert_eq!(stop.role.as_deref(), Some("streamer"));
    let stopping_id = stop.id;
    let settle = Duration::from_millis(engine.config.timing.takeover_settle_ms);
    engine.handle(Event::Tick, now + settle, wall());
    let expectations = engine.expectations(stopping_id).unwrap();
    for role in ["streamer", "console_b"] {
        assert_eq!(
            expectations
                .iter()
                .find(|expectation| expectation.subject.strip_prefix("role:") == Some(role))
                .unwrap()
                .attempts,
            1
        );
    }
}

#[test]
fn warmup_boundary_preserves_command_trigger_and_does_not_claim_buffered_role_rows() {
    let (mut engine, start) = setup();
    let now = start + Duration::from_secs(3);
    let command = Command::SendRaw {
        to: 4,
        data: vec![0x36],
    };
    engine.handle(Event::Command(command.clone()), now, wall());
    let row = engine
        .decisions
        .iter()
        .find(|decision| decision.reason == "command_send_raw")
        .unwrap();
    assert_eq!(row.trigger, Trigger::Command { command });
    assert_eq!(row.parent, None);

    // A bound but Off target is stopped during the same handle as another role's
    // buffered wake. That resync row must not become the stopped target's parent.
    let (mut engine, start) = setup();
    engine.handle(
        Event::Command(Command::SetMode(Mode::Active)),
        start,
        wall(),
    );
    identify_atv(&mut engine, start);
    frame(&mut engine, start, 8, 15, "84240004");
    frame(&mut engine, start, 8, 15, "87080046");
    frame(&mut engine, start, 8, 0, "47436F6E736F6C652042");
    engine.handle(Event::BridgeOnline, start, wall());
    frame(&mut engine, start, 4, 0, "04");
    let now = start + Duration::from_secs(3);
    engine.handle(
        Event::Command(Command::Standby("console_b".into())),
        now,
        wall(),
    );
    assert!(
        engine
            .decisions
            .iter()
            .any(|decision| decision.reason == "resync_complete"
                && decision.role.as_deref() == Some("streamer")
                && decision.trigger
                    == Trigger::Timer {
                        name: TimerKind::Warmup
                    })
    );
    let command = engine
        .decisions
        .iter()
        .find(|decision| decision.reason == "command_standby")
        .unwrap();
    assert_eq!(
        command.actions,
        vec![
            Action::CecSend {
                to: 8,
                data: vec![0x36]
            },
            Action::CecSend {
                to: 8,
                data: vec![0x8f]
            }
        ]
    );
    assert!(
        command
            .expectations
            .iter()
            .any(|expectation| expectation.subject == "role:console_b")
    );
    let settle = Duration::from_millis(engine.config.timing.takeover_settle_ms);
    engine.handle(Event::Tick, now + settle, wall());
    assert_no_standby_decision(&engine, "console_b");

    // Early completion by an unrelated device's reply is still warmup, not
    // evidence that this reply itself woke the buffered role.
    let (mut engine, start) = setup();
    identify_atv(&mut engine, start);
    engine.handle(Event::BridgeOnline, start, wall());
    frame(&mut engine, start, 4, 0, "04");
    frame(
        &mut engine,
        start + Duration::from_millis(300),
        5,
        0,
        "9001",
    );
    let row = engine
        .decisions
        .iter()
        .find(|decision| {
            decision.reason == "resync_complete" && decision.role.as_deref() == Some("streamer")
        })
        .unwrap();
    assert_eq!(
        row.trigger,
        Trigger::Timer {
            name: TimerKind::Warmup
        }
    );
}

#[test]
fn external_tv_on_does_not_override_off_intent_or_suppress_wake() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    engine.handle(Event::Command(Command::AllOff), now, wall());
    let now = now + Duration::from_secs(engine.config.timing.off_delay_s);
    engine.handle(Event::Tick, now, wall());
    let tv = engine.config.tv.entity.clone();
    ha(&mut engine, now, tv, "on", None);
    assert_eq!(engine.intent.power, RoomPower::Off);
    assert!(frame(&mut engine, now, 4, 0, "04").contains(&Action::WakeTv));
}

#[test]
fn room_intent_tracks_wake_settle_release_and_all_off() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 0, "04");
    assert_eq!(engine.intent.power, RoomPower::On);
    assert_eq!(engine.intent.active_role.as_deref(), Some("streamer"));
    assert_eq!(engine.intent.receiver_source.as_deref(), Some("Streamer"));
    engine.handle(Event::Tick, now + Duration::from_secs(2), wall());
    assert_eq!(engine.intent.receiver_source.as_deref(), Some("Streamer"));
    frame(&mut engine, now + Duration::from_secs(3), 4, 0, "36");
    assert_eq!(engine.intent.power, RoomPower::On);
    assert_eq!(engine.intent.active_role, None);
    engine.handle(Event::Tick, now + Duration::from_secs(6), wall());
    assert_eq!(engine.intent, RoomIntent::off(wall()));
    frame(&mut engine, now + Duration::from_secs(7), 4, 0, "04");
    engine.handle(
        Event::Command(Command::AllOff),
        now + Duration::from_secs(8),
        wall(),
    );
    assert_eq!(engine.intent.power, RoomPower::On);
    assert_eq!(engine.intent.active_role, None);
    engine.handle(Event::Tick, now + Duration::from_secs(11), wall());
    assert_eq!(engine.intent, RoomIntent::off(wall()));
}

#[test]
fn direct_observations_update_only_their_truth_source() {
    let (mut engine, now) = setup();
    assert!(
        engine
            .handle(
                Event::TvPower {
                    state: TvState::On,
                    at: wall()
                },
                now,
                wall()
            )
            .is_empty()
    );
    assert!(engine.decisions.is_empty());
    assert_eq!(engine.tv_direct, Some((TvState::On, wall())));
    assert_eq!(engine.intent, RoomIntent::default());
    assert!(engine.ha.is_empty());
    let roles_before = engine.roles.len();
    for role in ["console_b", "unknown_role"] {
        assert!(
            engine
                .handle(
                    Event::ConsolePower {
                        role: role.into(),
                        state: choregos::state::ConsoleState::Awake,
                        at: wall(),
                    },
                    now,
                    wall()
                )
                .is_empty()
        );
        assert!(engine.decisions.is_empty());
        assert_eq!(engine.roles["console_b"].power, RolePower::Off);
        assert_eq!(
            engine.roles["console_b"].direct,
            Some((choregos::state::ConsoleState::Awake, wall()))
        );
        assert_eq!(engine.roles.len(), roles_before);
        assert!(!engine.roles.contains_key("unknown_role"));
        assert_eq!(engine.intent, RoomIntent::default());
        assert_eq!(engine.tv_direct, Some((TvState::On, wall())));
        assert!(engine.ha.is_empty());
    }
}

#[test]
fn repeated_rewake_requires_ha_on_observed_after_the_last_room_off() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    for entity in [
        engine.config.tv.entity.clone(),
        engine.config.receiver.entity.clone(),
    ] {
        ha(&mut engine, now, entity, "on", Some(wall()));
    }
    frame(&mut engine, now, 4, 0, "04");
    frame(&mut engine, now + Duration::from_secs(1), 4, 0, "36");
    let off_wall = wall() + chrono::Duration::seconds(4);
    engine.handle(Event::Tick, now + Duration::from_secs(4), off_wall);
    assert_eq!(engine.intent.off_at, Some(off_wall));
    let first_wake = frame(&mut engine, now + Duration::from_millis(4001), 4, 0, "04");
    assert!(first_wake.contains(&Action::WakeTv));
    frame(&mut engine, now + Duration::from_millis(4500), 4, 0, "36");
    let second_wake = frame(&mut engine, now + Duration::from_millis(5000), 4, 0, "04");
    assert!(second_wake.contains(&Action::WakeTv));
    assert!(second_wake.contains(&Action::ha(
        "media_player",
        "turn_on",
        &engine.config.receiver.entity
    )));
    assert_eq!(engine.intent.off_at, Some(off_wall));
    for (step, changed, redundant) in [
        (6, off_wall, true),
        (7, off_wall + chrono::Duration::milliseconds(1), false),
    ] {
        for entity in [
            engine.config.tv.entity.clone(),
            engine.config.receiver.entity.clone(),
        ] {
            ha(
                &mut engine,
                now + Duration::from_secs(step),
                entity,
                "on",
                Some(changed),
            );
        }
        frame(&mut engine, now + Duration::from_secs(step), 4, 0, "36");
        let actions = frame(
            &mut engine,
            now + Duration::from_secs(step) + Duration::from_millis(1),
            4,
            0,
            "04",
        );
        assert_eq!(actions.contains(&Action::WakeTv), redundant);
        assert_eq!(
            actions.contains(&Action::ha(
                "media_player",
                "turn_on",
                &engine.config.receiver.entity
            )),
            redundant
        );
    }
}

#[test]
fn all_off_cancelled_before_expiry_does_not_force_redundant_power_requests() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    for entity in [
        engine.config.tv.entity.clone(),
        engine.config.receiver.entity.clone(),
    ] {
        ha(&mut engine, now, entity, "on", None);
    }
    frame(&mut engine, now, 4, 0, "04");
    engine.handle(
        Event::Command(Command::AllOff),
        now + Duration::from_secs(1),
        wall(),
    );
    assert_eq!(engine.intent.power, RoomPower::On);
    assert_eq!(engine.intent.off_at, None);
    let wake = frame(&mut engine, now + Duration::from_secs(2), 4, 0, "04");
    assert!(!wake.contains(&Action::WakeTv));
    assert!(!wake.contains(&Action::ha(
        "media_player",
        "turn_on",
        &engine.config.receiver.entity
    )));
    assert!(
        !engine
            .handle(Event::Tick, now + Duration::from_secs(5), wall())
            .contains(&Action::TvOff)
    );
}

#[test]
fn takeover_intent_uses_the_new_source_immediately_and_reload_clears_it() {
    let (mut engine, now) = setup();
    identify_atv(&mut engine, now);
    frame(&mut engine, now, 4, 0, "04");
    frame(&mut engine, now, 8, 15, "84230004");
    assert_eq!(engine.intent.active_role.as_deref(), Some("console_a"));
    assert_eq!(engine.intent.receiver_source.as_deref(), Some("Console A"));
    let mut config = (*engine.config).clone();
    config.receiver.select_source = false;
    engine.handle(
        Event::ConfigReloaded(std::sync::Arc::new(config)),
        now,
        wall(),
    );
    assert_eq!(engine.intent.receiver_source, None);
    let mut config = (*engine.config).clone();
    config.bridge = choregos::config::BridgeConfig::Aether {
        node: "cec-bridge".into(),
        logical_address: 0,
    };
    engine.handle(
        Event::ConfigReloaded(std::sync::Arc::new(config)),
        now,
        wall(),
    );
    assert_eq!(engine.intent.active_role, None);
}

#[test]
fn config_reload_discards_all_direct_observations_without_a_bridge_change() {
    let (mut engine, now) = setup();
    engine.handle(
        Event::TvPower {
            state: TvState::On,
            at: wall(),
        },
        now,
        wall(),
    );
    for role in engine.roles.keys().cloned().collect::<Vec<_>>() {
        engine.handle(
            Event::ConsolePower {
                role,
                state: choregos::state::ConsoleState::Awake,
                at: wall(),
            },
            now,
            wall(),
        );
    }
    assert!(engine.roles.values().all(|role| role.direct.is_some()));
    engine.handle(Event::ConfigReloaded(engine.config.clone()), now, wall());
    assert_eq!(engine.tv_direct, None);
    assert!(engine.roles.values().all(|role| role.direct.is_none()));
}

fn verdict_event(engine: &mut Engine, start: Instant, ms: u64, event: Event) -> Vec<Action> {
    engine.handle(
        event,
        start + Duration::from_millis(ms),
        wall() + chrono::Duration::milliseconds(ms as i64),
    )
}

fn verdict_ha(engine: &mut Engine, start: Instant, ms: u64, entity: String, state: &str) {
    verdict_event(
        engine,
        start,
        ms,
        Event::HaState {
            entity,
            state: state.into(),
            attrs: BTreeMap::new(),
            changed: Some(wall() + chrono::Duration::milliseconds(ms as i64)),
            updated: None,
        },
    );
}

fn verdict_wake() -> (Engine, Instant, u64) {
    let (mut engine, start) = setup();
    identify_atv(&mut engine, start);
    verdict_event(
        &mut engine,
        start,
        0,
        Event::Command(Command::SetMode(Mode::Active)),
    );
    verdict_event(
        &mut engine,
        start,
        100,
        Event::Frame(Frame::from_hex(4, 0, "04").unwrap()),
    );
    let id = engine.decisions.last().unwrap().id;
    (engine, start, id)
}

#[test]
fn wake_verdict_achieves_with_fresh_ha_and_records_latency() {
    let (mut engine, start, id) = verdict_wake();
    let tv = engine.config.tv.entity.clone();
    let receiver = engine.config.receiver.entity.clone();
    // A retained pre-decision On is not evidence of this wake.
    verdict_event(
        &mut engine,
        start,
        200,
        Event::HaState {
            entity: tv.clone(),
            state: "on".into(),
            attrs: BTreeMap::new(),
            changed: Some(wall()),
            updated: None,
        },
    );
    assert_eq!(
        engine.expectations(id).unwrap()[0].outcome,
        Outcome::Pending
    );
    verdict_ha(&mut engine, start, 1000, receiver, "on");
    verdict_ha(&mut engine, start, 1500, tv, "on");
    let update = engine
        .verdicts
        .iter()
        .find(|update| update.id == id)
        .unwrap();
    assert_eq!(update.verdict, Verdict::Achieved);
    assert_eq!(
        update.expectations[0].outcome,
        Outcome::Met {
            via: Via::Ha,
            observed: "on".into(),
            latency_ms: 1400
        }
    );
    assert_eq!(
        update.expectations[1].outcome,
        Outcome::Met {
            via: Via::Ha,
            observed: "on".into(),
            latency_ms: 900
        }
    );
}

#[test]
fn wake_verdict_is_partial_when_receiver_never_reports_on() {
    let (mut engine, start, id) = verdict_wake();
    let tv = engine.config.tv.entity.clone();
    verdict_ha(&mut engine, start, 1000, tv, "on");
    verdict_event(&mut engine, start, 10100, Event::Tick);
    let update = engine
        .verdicts
        .iter()
        .find(|update| update.id == id)
        .unwrap();
    assert_eq!(update.verdict, Verdict::Partial);
    assert_eq!(
        update.expectations[1].outcome,
        Outcome::Unknown { observed: None }
    );
}

#[test]
fn direct_tv_observation_wins_before_ha_and_ha_only_configuration_excludes_it() {
    for direct in [true, false] {
        let (mut engine, start, id) = verdict_wake();
        if !direct {
            // Change the transport before creating a fresh wake, through a reload.
            let mut config = (*engine.config).clone();
            config.tv.off = choregos::config::TvOffConfig::Ha;
            verdict_event(
                &mut engine,
                start,
                200,
                Event::ConfigReloaded(std::sync::Arc::new(config)),
            );
            assert_eq!(
                engine.expectations(id).unwrap()[0].outcome,
                Outcome::Unknown { observed: None }
            );
            verdict_event(
                &mut engine,
                start,
                4000,
                Event::Frame(Frame::from_hex(4, 0, "36").unwrap()),
            );
            verdict_event(
                &mut engine,
                start,
                4100,
                Event::Frame(Frame::from_hex(4, 0, "04").unwrap()),
            );
            let expectation = &engine.decisions.last().unwrap().expectations[0];
            assert_eq!(expectation.via, vec![Via::Ha]);
            continue;
        }
        let receiver = engine.config.receiver.entity.clone();
        verdict_ha(&mut engine, start, 500, receiver, "on");
        verdict_event(
            &mut engine,
            start,
            600,
            Event::TvPower {
                state: TvState::On,
                at: wall() + chrono::Duration::milliseconds(600),
            },
        );
        let update = engine
            .verdicts
            .iter()
            .find(|update| update.id == id)
            .unwrap();
        assert_eq!(update.verdict, Verdict::Achieved);
        assert_eq!(
            update.expectations[0].outcome,
            Outcome::Met {
                via: Via::TvDirect,
                observed: "on".into(),
                latency_ms: 500
            }
        );
    }
}

#[test]
fn takeover_standby_uses_independent_console_evidence_or_honest_cec_only() {
    for direct in [true, false] {
        let (mut engine, start) = setup();
        let config = std::sync::Arc::make_mut(&mut engine.config);
        config.mode = Mode::Active;
        config.device[0].probe = Some(choregos::config::ProbeConfig::Ps5 {
            host: "console.example".into(),
            port: 9302,
        });
        identify_atv(&mut engine, start);
        verdict_event(
            &mut engine,
            start,
            100,
            Event::Frame(Frame::from_hex(4, 0, "04").unwrap()),
        );
        verdict_event(
            &mut engine,
            start,
            1000,
            Event::Frame(Frame::from_hex(8, 15, "84230004").unwrap()),
        );
        let id = engine.decisions.last().unwrap().id;
        let tv = engine.config.tv.entity.clone();
        let receiver = engine.config.receiver.entity.clone();
        verdict_ha(&mut engine, start, 1100, tv, "on");
        verdict_ha(&mut engine, start, 1200, receiver, "on");
        verdict_event(
            &mut engine,
            start,
            1300,
            Event::Frame(Frame::from_hex(4, 0, "9001").unwrap()),
        );
        // CEC cannot prematurely hide the independent probe's result.
        assert_eq!(
            standby_expectation(&engine, "streamer").outcome,
            Outcome::Pending
        );
        if direct {
            verdict_event(
                &mut engine,
                start,
                1400,
                Event::ConsolePower {
                    role: "streamer".into(),
                    state: choregos::state::ConsoleState::Standby,
                    at: wall() + chrono::Duration::milliseconds(1400),
                },
            );
        } else {
            let deadline = 1000 + engine.config.verify.probe_window_s * 1000;
            verdict_event(&mut engine, start, deadline, Event::Tick);
        }
        let update = engine
            .verdicts
            .iter()
            .find(|update| update.id == id)
            .unwrap();
        assert_eq!(update.verdict, Verdict::Achieved);
        let outcome = &update
            .expectations
            .iter()
            .find(|expectation| expectation.subject == "role:streamer")
            .unwrap()
            .outcome;
        if direct {
            assert!(matches!(outcome, Outcome::Met { via: Via::Ps5, .. }));
        } else {
            assert!(matches!(outcome, Outcome::CecOnly { observed } if observed == "standby"));
        }
    }
}

#[test]
fn displaced_role_reporting_on_misses_after_the_final_retry() {
    let (mut engine, takeover, settle) = takeover_from_streamer();
    for round in 1..=u32::from(engine.config.timing.standby_retries) + 1 {
        frame(
            &mut engine,
            takeover + settle * round - Duration::from_millis(1),
            4,
            0,
            "9000",
        );
        engine.handle(Event::Tick, takeover + settle * round, wall());
    }
    let expectation = standby_expectation(&engine, "streamer");
    assert_eq!(expectation.attempts, engine.config.timing.standby_retries);
    assert_eq!(
        expectation.outcome,
        Outcome::Missed {
            observed: "on".into()
        }
    );
}

#[test]
fn non_verifying_roles_without_probes_contribute_no_standby_expectations() {
    for report in [false, true] {
        let (mut engine, stopped, settle) = stop_non_verifying_console(Takeover::CecStandby, false);
        if report {
            frame(
                &mut engine,
                stopped + Duration::from_millis(1),
                8,
                0,
                "9001",
            );
        }
        engine.handle(Event::Tick, stopped + settle * 3, wall());
        assert!(
            (1..engine.next_decision_id)
                .filter_map(|id| engine.expectations(id))
                .flatten()
                .all(|expectation| expectation.subject != "role:console_a")
        );
    }
}

#[test]
fn room_off_verdict_reports_achieved_and_missed_without_changing_intent() {
    for reached in [true, false] {
        let (mut engine, start, _) = verdict_wake();
        verdict_event(
            &mut engine,
            start,
            200,
            Event::Frame(Frame::from_hex(4, 0, "36").unwrap()),
        );
        verdict_event(&mut engine, start, 3200, Event::Tick);
        let id = engine
            .decisions
            .iter()
            .find(|decision| decision.reason == "off_timer_expired")
            .unwrap()
            .id;
        let tv = engine.config.tv.entity.clone();
        let receiver = engine.config.receiver.entity.clone();
        let value = if reached { "off" } else { "on" };
        verdict_ha(&mut engine, start, 4000, tv, value);
        verdict_ha(&mut engine, start, 4100, receiver, value);
        if !reached {
            let deadline = 3200 + engine.config.verify.tv_s * 1000;
            verdict_event(&mut engine, start, deadline, Event::Tick);
        }
        let update = engine
            .verdicts
            .iter()
            .find(|update| update.id == id)
            .unwrap();
        assert_eq!(
            update.verdict,
            if reached {
                Verdict::Achieved
            } else {
                Verdict::Missed
            }
        );
        assert_eq!(engine.intent.power, RoomPower::Off);
    }
}

/// The receiver reports off 0.2–10 s after `turn_off` (its integration polls), so its
/// off expectation has its own, longer deadline than the 2 s turn-on.
#[test]
fn receiver_off_uses_its_own_deadline_longer_than_receiver_on() {
    let (mut engine, start, _) = verdict_wake();
    assert!(engine.config.verify.receiver_off_s > engine.config.verify.receiver_s);
    verdict_event(
        &mut engine,
        start,
        200,
        Event::Frame(Frame::from_hex(4, 0, "36").unwrap()),
    );
    verdict_event(&mut engine, start, 3200, Event::Tick);
    let id = engine
        .decisions
        .iter()
        .find(|decision| decision.reason == "off_timer_expired")
        .unwrap()
        .id;
    let tv = engine.config.tv.entity.clone();
    let receiver = engine.config.receiver.entity.clone();
    verdict_ha(&mut engine, start, 4000, tv, "off");
    // Past the on-deadline, inside the off-deadline: the live 10.17 s case.
    let late = 3200 + engine.config.verify.receiver_s * 1000 + 170;
    verdict_event(&mut engine, start, late - 10, Event::Tick);
    assert!(!engine.verdicts.iter().any(|update| update.id == id));
    verdict_ha(&mut engine, start, late, receiver, "off");
    let update = engine
        .verdicts
        .iter()
        .find(|update| update.id == id)
        .unwrap();
    assert_eq!(update.verdict, Verdict::Achieved);
}

fn room_off_after_confirmed_ha_on() -> (Engine, Instant, u64) {
    let (mut engine, start, _) = verdict_wake();
    std::sync::Arc::make_mut(&mut engine.config).tv.off = choregos::config::TvOffConfig::Webos {
        host: "tv.example".into(),
        port: 3001,
        key_env: "CHOREGOS_TV_KEY".into(),
    };
    let tv = engine.config.tv.entity.clone();
    let receiver = engine.config.receiver.entity.clone();
    verdict_ha(&mut engine, start, 2000, tv, "on");
    verdict_ha(&mut engine, start, 2100, receiver, "on");
    verdict_event(
        &mut engine,
        start,
        5000,
        Event::Frame(Frame::from_hex(4, 0, "36").unwrap()),
    );
    verdict_event(&mut engine, start, 8000, Event::Tick);
    let id = engine
        .decisions
        .iter()
        .find(|row| row.reason == "off_timer_expired")
        .unwrap()
        .id;
    (engine, start, id)
}

#[test]
fn quick_rewake_cannot_reuse_ha_on_older_than_pending_room_off() {
    let (mut engine, start, off_id) = room_off_after_confirmed_ha_on();
    let actions = verdict_event(
        &mut engine,
        start,
        20000,
        Event::Frame(Frame::from_hex(4, 0, "04").unwrap()),
    );
    assert!(actions.contains(&Action::WakeTv));
    let wake = engine
        .decisions
        .iter()
        .find(|row| row.to == Some(RolePower::On))
        .unwrap();
    assert_eq!(wake.expectations[0].outcome, Outcome::Pending);
    assert_eq!(
        engine.expectations(off_id).unwrap()[0].outcome,
        Outcome::Pending
    );
}

#[test]
fn quick_rewake_uses_newer_direct_off_instead_of_older_matching_ha_on() {
    let (mut engine, start, off_id) = room_off_after_confirmed_ha_on();
    verdict_event(
        &mut engine,
        start,
        9000,
        Event::TvPower {
            state: TvState::Off,
            at: wall() + chrono::Duration::milliseconds(9000),
        },
    );
    engine.settle_expectations(start + Duration::from_millis(30000));
    assert_eq!(
        engine
            .verdicts
            .iter()
            .find(|update| update.id == off_id)
            .unwrap()
            .verdict,
        Verdict::Partial
    );
    let actions = verdict_event(
        &mut engine,
        start,
        31000,
        Event::Frame(Frame::from_hex(4, 0, "04").unwrap()),
    );
    assert!(actions.contains(&Action::WakeTv));
    let wake = engine
        .decisions
        .iter()
        .find(|row| row.to == Some(RolePower::On))
        .unwrap();
    assert_eq!(wake.expectations[0].outcome, Outcome::Pending);
}

#[test]
fn quick_room_off_cannot_reuse_ha_off_older_than_pending_wake() {
    let (mut engine, start, wake_id) = verdict_wake();
    let tv = engine.config.tv.entity.clone();
    verdict_event(
        &mut engine,
        start,
        300,
        Event::HaState {
            entity: tv,
            state: "off".into(),
            attrs: BTreeMap::new(),
            changed: Some(wall() + chrono::Duration::milliseconds(50)),
            updated: None,
        },
    );
    verdict_event(
        &mut engine,
        start,
        5000,
        Event::Frame(Frame::from_hex(4, 0, "36").unwrap()),
    );
    verdict_event(&mut engine, start, 8000, Event::Tick);
    let off = engine
        .decisions
        .iter()
        .find(|row| row.reason == "off_timer_expired")
        .unwrap();
    assert_eq!(off.expectations[0].outcome, Outcome::Pending);
    assert_eq!(
        engine.expectations(wake_id).unwrap()[0].outcome,
        Outcome::Pending
    );
}

#[test]
fn all_off_keeps_creation_met_rows_when_attaching_an_already_off_role() {
    let (mut engine, start) = setup();
    let config = std::sync::Arc::make_mut(&mut engine.config);
    config.mode = Mode::Active;
    config.device[0].probe = Some(choregos::config::ProbeConfig::Ps5 {
        host: "console.example".into(),
        port: 9302,
    });
    identify_atv(&mut engine, start);
    frame(&mut engine, start, 8, 15, "84240004");
    frame(&mut engine, start, 8, 15, "87080046");
    frame(&mut engine, start, 8, 0, "47436F6E736F6C652042");
    verdict_event(
        &mut engine,
        start,
        100,
        Event::Frame(Frame::from_hex(4, 0, "04").unwrap()),
    );
    assert_eq!(engine.roles["streamer"].power, RolePower::On);
    assert_eq!(engine.roles["console_b"].power, RolePower::Off);
    verdict_event(
        &mut engine,
        start,
        500,
        Event::ConsolePower {
            role: "streamer".into(),
            state: choregos::state::ConsoleState::Standby,
            at: wall() + chrono::Duration::milliseconds(500),
        },
    );
    verdict_event(&mut engine, start, 1000, Event::Command(Command::AllOff));
    let decision = engine
        .decisions
        .iter()
        .find(|row| row.reason == "all_off")
        .unwrap();
    let id = decision.id;
    assert_eq!(decision.verdict, Verdict::Pending);
    assert_eq!(decision.expectations.len(), 2);
    let met = Outcome::Met {
        via: Via::Ps5,
        observed: "standby".into(),
        latency_ms: 0,
    };
    assert_eq!(engine.expectations(id).unwrap().len(), 2);
    assert_eq!(engine.expectations(id).unwrap()[0].outcome, met);
    let deadline_ms = 1000
        + engine.config.timing.takeover_settle_ms
            * (u64::from(engine.config.timing.standby_retries) + 1);
    engine.settle_expectations(start + Duration::from_millis(deadline_ms));
    let update = engine
        .verdicts
        .iter()
        .find(|update| update.id == id)
        .unwrap();
    assert_eq!(update.verdict, Verdict::Partial);
    assert_eq!(update.expectations.len(), 2);
    assert_eq!(update.expectations[0].outcome, met);
    assert_eq!(update.expectations[1].subject, "role:console_b");
    assert_eq!(
        update.expectations[1].outcome,
        Outcome::Unknown { observed: None }
    );
}

#[test]
fn room_off_accepts_only_recent_already_off_receiver_samples_at_creation() {
    let window_ms = choregos::config::VerifyConfig::default().receiver_off_s * 1000;
    for age_ms in [1200, window_ms, window_ms + 1] {
        let (mut engine, start, _) = verdict_wake();
        let tv = engine.config.tv.entity.clone();
        let receiver = engine.config.receiver.entity.clone();
        verdict_ha(&mut engine, start, 500, tv, "on");
        verdict_ha(&mut engine, start, 600, receiver, "on");
        verdict_event(
            &mut engine,
            start,
            37000,
            Event::Frame(Frame::from_hex(4, 0, "36").unwrap()),
        );
        let receiver = engine.config.receiver.entity.clone();
        verdict_event(
            &mut engine,
            start,
            39000,
            Event::HaState {
                entity: receiver,
                state: "off".into(),
                attrs: BTreeMap::new(),
                changed: Some(wall() + chrono::Duration::milliseconds((40000 - age_ms) as i64)),
                updated: Some(wall() + chrono::Duration::milliseconds(39000)),
            },
        );
        verdict_event(&mut engine, start, 40000, Event::Tick);
        let decision = engine
            .decisions
            .iter()
            .find(|decision| decision.reason == "off_timer_expired")
            .unwrap();
        let id = decision.id;
        let fresh = age_ms <= window_ms;
        let expected = if fresh {
            Outcome::Met {
                via: Via::Ha,
                observed: "off".into(),
                latency_ms: 0,
            }
        } else {
            Outcome::Pending
        };
        assert_eq!(decision.expectations[1].outcome, expected, "age {age_ms}");
        let tv = engine.config.tv.entity.clone();
        verdict_ha(&mut engine, start, 41000, tv, "off");
        if !fresh {
            assert_eq!(
                engine.expectations(id).unwrap()[1].outcome,
                Outcome::Pending
            );
            engine.settle_expectations(start + Duration::from_millis(40000 + window_ms + 1));
        }
        let update = engine
            .verdicts
            .iter()
            .find(|update| update.id == id)
            .unwrap();
        assert_eq!(
            update.verdict,
            if fresh {
                Verdict::Achieved
            } else {
                Verdict::Partial
            }
        );
        assert_eq!(
            update.expectations[1].outcome,
            if fresh {
                expected
            } else {
                Outcome::Unknown { observed: None }
            }
        );
    }
}

#[test]
fn room_off_already_satisfied_independent_sources_achieve_at_creation() {
    let (mut engine, start, _) = verdict_wake();
    std::sync::Arc::make_mut(&mut engine.config).tv.off = choregos::config::TvOffConfig::Webos {
        host: "tv.example".into(),
        port: 3001,
        key_env: "CHOREGOS_TV_KEY".into(),
    };
    verdict_event(
        &mut engine,
        start,
        200,
        Event::Frame(Frame::from_hex(4, 0, "36").unwrap()),
    );
    let receiver = engine.config.receiver.entity.clone();
    verdict_ha(&mut engine, start, 2000, receiver, "off");
    verdict_event(
        &mut engine,
        start,
        2100,
        Event::TvPower {
            state: TvState::Off,
            at: wall() + chrono::Duration::milliseconds(2100),
        },
    );
    verdict_event(&mut engine, start, 3200, Event::Tick);
    let decision = engine
        .decisions
        .iter()
        .find(|row| row.reason == "off_timer_expired")
        .unwrap();
    assert_eq!(decision.verdict, Verdict::Achieved);
    for (expectation, via) in decision.expectations.iter().zip([Via::TvDirect, Via::Ha]) {
        assert_eq!(
            expectation.outcome,
            Outcome::Met {
                via,
                observed: "off".into(),
                latency_ms: 0
            }
        );
    }
    assert!(
        engine
            .verdicts
            .iter()
            .all(|update| update.id != decision.id)
    );
}

#[test]
fn already_standby_probe_settles_an_attached_command_expectation_at_creation() {
    let (mut engine, start) = setup();
    let config = std::sync::Arc::make_mut(&mut engine.config);
    config.mode = Mode::Active;
    config.device[0].probe = Some(choregos::config::ProbeConfig::Ps5 {
        host: "console.example".into(),
        port: 9302,
    });
    identify_atv(&mut engine, start);
    verdict_event(
        &mut engine,
        start,
        100,
        Event::ConsolePower {
            role: "streamer".into(),
            state: choregos::state::ConsoleState::Standby,
            at: wall() + chrono::Duration::milliseconds(100),
        },
    );
    verdict_event(
        &mut engine,
        start,
        1000,
        Event::Command(Command::Standby("streamer".into())),
    );
    let decision = engine
        .decisions
        .iter()
        .find(|row| !row.expectations.is_empty())
        .unwrap();
    assert_eq!(decision.verdict, Verdict::Achieved);
    assert_eq!(
        decision.expectations[0].outcome,
        Outcome::Met {
            via: Via::Ps5,
            observed: "standby".into(),
            latency_ms: 0
        }
    );
}

#[test]
fn already_standby_cec_never_presettles_a_command_expectation() {
    let (mut engine, start) = setup();
    std::sync::Arc::make_mut(&mut engine.config).mode = Mode::Active;
    identify_atv(&mut engine, start);
    verdict_event(
        &mut engine,
        start,
        100,
        Event::Frame(Frame::from_hex(4, 0, "9001").unwrap()),
    );
    verdict_event(
        &mut engine,
        start,
        1000,
        Event::Command(Command::Standby("streamer".into())),
    );
    let decision = engine
        .decisions
        .iter()
        .find(|row| !row.expectations.is_empty())
        .unwrap();
    assert_eq!(decision.verdict, Verdict::Pending);
    assert_eq!(decision.expectations[0].via, vec![Via::Cec]);
    assert_eq!(decision.expectations[0].outcome, Outcome::Pending);
}

#[test]
fn source_selection_expectation_requires_a_new_ha_source_observation() {
    let (mut engine, start, wake) = verdict_wake();
    verdict_event(&mut engine, start, 2100, Event::Tick);
    let decision = engine
        .decisions
        .iter()
        .find(|decision| decision.reason == "takeover_settled")
        .unwrap();
    assert_eq!(decision.parent, Some(wake));
    let id = decision.id;
    let expectation = &decision.expectations[0];
    assert_eq!(expectation.field, choregos::action::Field::Source);
    let expected = expectation.expect.clone();
    let receiver = engine.config.receiver.entity.clone();
    verdict_event(
        &mut engine,
        start,
        2500,
        Event::HaState {
            entity: receiver,
            state: "on".into(),
            attrs: [("source".into(), serde_json::json!(expected))].into(),
            changed: Some(wall()),
            updated: Some(wall() + chrono::Duration::milliseconds(2500)),
        },
    );
    assert_eq!(
        engine
            .verdicts
            .iter()
            .find(|update| update.id == id)
            .unwrap()
            .verdict,
        Verdict::Achieved
    );
    let receiver_power = engine
        .expectations(wake)
        .unwrap()
        .iter()
        .find(|expectation| expectation.subject == "receiver")
        .unwrap();
    assert_eq!(
        receiver_power.outcome,
        Outcome::Pending,
        "attribute freshness is not power freshness"
    );
}

#[test]
fn bridge_offline_and_unrelated_reload_preserve_independent_expectations() {
    for cancellation in 0..3 {
        let (mut engine, start, id) = verdict_wake();
        let event = match cancellation {
            0 => Event::BridgeOffline,
            1 => Event::ConfigReloaded(engine.config.clone()),
            _ => Event::Command(Command::SetMode(Mode::Shadow)),
        };
        verdict_event(&mut engine, start, 500, event);
        if cancellation == 2 {
            let update = engine
                .verdicts
                .iter()
                .find(|update| update.id == id)
                .unwrap();
            assert_eq!(update.verdict, Verdict::Unknown);
            assert!(
                update
                    .expectations
                    .iter()
                    .all(|expectation| expectation.outcome == Outcome::Unknown { observed: None })
            );
            assert_eq!(engine.next_expectation_deadline(), None);
        } else {
            assert!(engine.verdicts.iter().all(|update| update.id != id));
            let tv = engine.config.tv.entity.clone();
            let receiver = engine.config.receiver.entity.clone();
            verdict_ha(&mut engine, start, 600, tv, "on");
            verdict_ha(&mut engine, start, 700, receiver, "on");
            assert_eq!(
                engine
                    .verdicts
                    .iter()
                    .find(|update| update.id == id)
                    .unwrap()
                    .verdict,
                Verdict::Achieved
            );
        }
    }
}

#[test]
fn shadow_rows_and_actionless_rows_have_no_observable_verdict() {
    let (mut engine, start) = setup();
    identify_atv(&mut engine, start);
    frame(&mut engine, start, 4, 0, "04");
    assert!(
        engine
            .decisions
            .iter()
            .all(|decision| decision.verdict == Verdict::None && decision.expectations.is_empty())
    );
    assert_eq!(engine.next_expectation_deadline(), None);
}

#[test]
fn an_unreachable_sample_replaces_the_sources_earlier_disagreement() {
    let (mut engine, start, id) = verdict_wake();
    let receiver = engine.config.receiver.entity.clone();
    verdict_ha(&mut engine, start, 500, receiver, "on");
    for (ms, state) in [(600, TvState::Off), (700, TvState::Unreachable)] {
        verdict_event(
            &mut engine,
            start,
            ms,
            Event::TvPower {
                state,
                at: wall() + chrono::Duration::milliseconds(ms as i64),
            },
        );
    }
    let deadline = 100 + engine.config.verify.tv_s * 1000;
    verdict_event(&mut engine, start, deadline, Event::Tick);
    let update = engine
        .verdicts
        .iter()
        .find(|update| update.id == id)
        .unwrap();
    assert_eq!(update.verdict, Verdict::Partial);
    assert_eq!(
        update.expectations[0].outcome,
        Outcome::Unknown { observed: None }
    );
}

#[test]
fn wake_power_expectations_exist_only_for_dispatched_power_actions() {
    for tv_on in [false, true] {
        for receiver_on in [false, true] {
            let (mut engine, start) = setup();
            identify_atv(&mut engine, start);
            verdict_event(
                &mut engine,
                start,
                0,
                Event::Command(Command::SetMode(Mode::Active)),
            );
            let tv = engine.config.tv.entity.clone();
            let receiver = engine.config.receiver.entity.clone();
            verdict_ha(&mut engine, start, 10, tv, if tv_on { "on" } else { "off" });
            verdict_ha(
                &mut engine,
                start,
                20,
                receiver,
                if receiver_on { "on" } else { "off" },
            );
            let actions = verdict_event(
                &mut engine,
                start,
                100,
                Event::Frame(Frame::from_hex(4, 0, "04").unwrap()),
            );
            let decision = engine.decisions.last().unwrap();
            assert_eq!(actions.contains(&Action::WakeTv), !tv_on);
            assert_eq!(
                decision
                    .expectations
                    .iter()
                    .any(|expectation| expectation.subject == "tv"),
                !tv_on
            );
            assert_eq!(
                decision
                    .expectations
                    .iter()
                    .any(|expectation| expectation.subject == "receiver"),
                !receiver_on
            );
            assert_eq!(
                decision.verdict,
                if tv_on && receiver_on {
                    Verdict::None
                } else {
                    Verdict::Pending
                }
            );
        }
    }
}

#[test]
fn opted_out_cec_standby_uses_only_a_configured_probe() {
    use choregos::{config::ProbeConfig, state::ConsoleState};
    for confirmed in [false, true] {
        let (mut engine, start) = setup();
        let config = std::sync::Arc::make_mut(&mut engine.config);
        config.mode = Mode::Active;
        let console = config
            .device
            .iter_mut()
            .find(|device| device.role == "console_a")
            .unwrap();
        assert!(!console.verify_standby);
        console.probe = Some(ProbeConfig::Ps5 {
            host: "console.example".into(),
            port: 9302,
        });
        identify_atv(&mut engine, start);
        verdict_event(
            &mut engine,
            start,
            100,
            Event::Frame(Frame::from_hex(8, 15, "84230004").unwrap()),
        );
        verdict_event(
            &mut engine,
            start,
            1000,
            Event::Frame(Frame::from_hex(4, 0, "04").unwrap()),
        );
        let id = engine.decisions.last().unwrap().id;
        assert_eq!(
            standby_expectation(&engine, "console_a").deadline_ms,
            engine.config.verify.probe_window_s * 1000
        );
        let tv = engine.config.tv.entity.clone();
        let receiver = engine.config.receiver.entity.clone();
        verdict_ha(&mut engine, start, 1100, tv, "on");
        verdict_ha(&mut engine, start, 1150, receiver, "on");
        for (ms, data) in [(1200, "9000"), (1250, "9001")] {
            verdict_event(
                &mut engine,
                start,
                ms,
                Event::Frame(Frame::from_hex(8, 0, data).unwrap()),
            );
            let expectation = standby_expectation(&engine, "console_a");
            assert_eq!(expectation.via, vec![Via::Ps5]);
            assert_eq!(
                expectation.outcome,
                Outcome::Pending,
                "CEC neither confirms nor refutes an opted-out role"
            );
        }
        for (ms, state) in [
            (1400, ConsoleState::Awake),
            (3400, ConsoleState::Unreachable),
            (5400, ConsoleState::Unreachable),
            (7400, ConsoleState::Unreachable),
            (9400, ConsoleState::Unreachable),
        ] {
            verdict_event(
                &mut engine,
                start,
                ms,
                Event::ConsolePower {
                    role: "console_a".into(),
                    state,
                    at: wall() + chrono::Duration::milliseconds(ms as i64),
                },
            );
            assert_eq!(
                standby_expectation(&engine, "console_a").outcome,
                Outcome::Pending
            );
        }
        if confirmed {
            verdict_event(
                &mut engine,
                start,
                11000,
                Event::ConsolePower {
                    role: "console_a".into(),
                    state: ConsoleState::Standby,
                    at: wall() + chrono::Duration::milliseconds(11000),
                },
            );
        } else {
            let deadline = 1000 + engine.config.verify.probe_window_s * 1000;
            let actions = verdict_event(&mut engine, start, deadline, Event::Tick);
            assert!(
                !actions
                    .iter()
                    .any(|action| matches!(action, Action::CecSend { to: 8, .. }))
            );
        }
        let update = engine
            .verdicts
            .iter()
            .find(|update| update.id == id)
            .unwrap();
        let expectation = update
            .expectations
            .iter()
            .find(|expectation| expectation.subject == "role:console_a")
            .unwrap();
        assert_eq!(expectation.attempts, 0);
        assert_eq!(
            update.verdict,
            if confirmed {
                Verdict::Achieved
            } else {
                Verdict::Partial
            }
        );
        if confirmed {
            assert_eq!(
                expectation.outcome,
                Outcome::Met {
                    via: Via::Ps5,
                    observed: "standby".into(),
                    latency_ms: 10000,
                }
            );
        } else {
            assert_eq!(expectation.outcome, Outcome::Unknown { observed: None });
        }
    }
}

#[test]
fn ha_tv_wake_with_reconnect_lag_is_within_the_default_observation_window() {
    let (mut engine, start, id) = verdict_wake();
    let receiver = engine.config.receiver.entity.clone();
    let tv = engine.config.tv.entity.clone();
    verdict_ha(&mut engine, start, 500, receiver, "on");
    verdict_ha(&mut engine, start, 33000, tv, "on");
    let update = engine
        .verdicts
        .iter()
        .find(|update| update.id == id)
        .unwrap();
    assert_eq!(update.verdict, Verdict::Achieved);
    assert_eq!(
        update.expectations[0].outcome,
        Outcome::Met {
            via: Via::Ha,
            observed: "on".into(),
            latency_ms: 32900
        }
    );
}

fn review_step(
    engine: &mut Engine,
    tag: &str,
    start: Instant,
    ms: u64,
    event: Event,
) -> Vec<String> {
    let actions = engine.handle(event, start + Duration::from_millis(ms), wall());
    let sends: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            Action::CecSend { to, data } => Some(format!("{to}:{}", hex::encode_upper(data))),
            _ => None,
        })
        .collect();
    println!("PROBE {tag} +{ms}ms sends={sends:?}");
    sends
}

#[test]
fn review_p12_standby_then_on_keeps_verification_suppression_and_retries() {
    let (mut engine, start, _) = takeover_from_streamer();
    for (ms, event) in [
        (300, Event::Frame(Frame::from_hex(4, 0, "9001").unwrap())),
        (600, Event::Frame(Frame::from_hex(4, 0, "9000").unwrap())),
        (2000, Event::Tick),
        (4000, Event::Tick),
        (6000, Event::Tick),
        (8000, Event::Tick),
    ] {
        let sends = review_step(&mut engine, "p12", start, ms, event);
        assert_eq!(
            sends,
            if [2000, 4000].contains(&ms) {
                vec!["4:36", "4:8F"]
            } else {
                vec![]
            }
        );
        assert_eq!(engine.roles["streamer"].power, RolePower::Off);
        assert_eq!(engine.roles["console_a"].power, RolePower::On);
    }
}

#[test]
fn review_p7_confirmed_verification_releases_suppression_at_its_check() {
    let (mut engine, start) = setup();
    let config = std::sync::Arc::make_mut(&mut engine.config);
    config.mode = Mode::Active;
    config.device[0].probe = Some(choregos::config::ProbeConfig::Ps5 {
        host: "console.example".into(),
        port: 9302,
    });
    identify_atv(&mut engine, start);
    for (ms, event) in [
        (100, Event::Frame(Frame::from_hex(4, 0, "04").unwrap())),
        (
            1000,
            Event::Frame(Frame::from_hex(8, 15, "84230004").unwrap()),
        ),
        (2300, Event::Frame(Frame::from_hex(4, 0, "9001").unwrap())),
        (3000, Event::Tick),
        (4000, Event::Frame(Frame::from_hex(4, 0, "9000").unwrap())),
        (5000, Event::Tick),
        (7000, Event::Tick),
        (9000, Event::Tick),
    ] {
        let sends = review_step(&mut engine, "p7", start, ms, event);
        let expected = match ms {
            1000 => vec!["4:36", "4:8F"],
            4000 => vec!["8:36", "8:8F"],
            _ => vec![],
        };
        assert_eq!(sends, expected);
        if ms >= 4000 {
            assert_eq!(engine.roles["streamer"].power, RolePower::On);
        }
    }
}

#[test]
fn review_p10_warming_delays_retries_without_bunching_or_cancelling_them() {
    let (mut engine, start, _) = takeover_from_streamer();
    assert!(review_step(&mut engine, "p10", start, 1500, Event::BridgeOnline).is_empty());
    for ms in [2000, 4000, 4500, 4501, 5000, 6000, 6500, 8000, 10000] {
        let sends = review_step(&mut engine, "p10", start, ms, Event::Tick);
        assert_eq!(
            sends,
            if [4500, 6500].contains(&ms) {
                vec!["4:36", "4:8F"]
            } else {
                vec![]
            }
        );
    }
}

#[test]
fn review_p9_ha_media_stop_does_not_invent_a_cec_standby_expectation() {
    let (mut engine, start) = setup();
    std::sync::Arc::make_mut(&mut engine.config).mode = Mode::Active;
    identify_atv(&mut engine, start);
    let tv = engine.config.tv.entity.clone();
    let receiver = engine.config.receiver.entity.clone();
    verdict_ha(&mut engine, start, 0, tv, "on");
    verdict_ha(&mut engine, start, 0, receiver, "on");
    frame(&mut engine, start, 1, 0, "474B6F6469");
    frame(
        &mut engine,
        start + Duration::from_millis(10),
        1,
        15,
        "822200",
    );
    let actions = frame(&mut engine, start + Duration::from_secs(1), 4, 0, "04");
    assert!(actions.iter().any(|action| service(action, "media_stop")));
    let wake = engine
        .decisions
        .iter()
        .find(|decision| decision.to == Some(RolePower::On))
        .unwrap();
    assert_eq!(wake.verdict, Verdict::None);
    assert!(wake.expectations.is_empty());
}

#[test]
fn review_p4r_all_off_ignores_unobservable_media_stop_roles() {
    let (mut engine, start) = console_before_address_move();
    engine.handle(
        Event::Command(Command::SetMode(Mode::Active)),
        start,
        wall(),
    );
    identify_atv(&mut engine, start);
    frame(&mut engine, start, 4, 0, "04");
    verdict_event(&mut engine, start, 500, Event::Command(Command::AllOff));
    let row = engine
        .decisions
        .iter()
        .find(|decision| decision.reason == "all_off")
        .unwrap();
    let id = row.id;
    assert_eq!(row.expectations.len(), 2);
    assert!(
        row.expectations
            .iter()
            .all(|expectation| expectation.subject != "role:kodi")
    );
    verdict_event(
        &mut engine,
        start,
        600,
        Event::Frame(Frame::from_hex(8, 0, "9001").unwrap()),
    );
    verdict_event(&mut engine, start, 2500, Event::Tick);
    verdict_event(
        &mut engine,
        start,
        2600,
        Event::Frame(Frame::from_hex(4, 0, "9001").unwrap()),
    );
    verdict_event(&mut engine, start, 4500, Event::Tick);
    let update = engine
        .verdicts
        .iter()
        .find(|update| update.id == id)
        .unwrap();
    assert_eq!(update.verdict, Verdict::Achieved);
    assert!(
        update
            .expectations
            .iter()
            .all(|expectation| matches!(expectation.outcome, Outcome::CecOnly { .. }))
    );
}

#[test]
fn review_p11_any_fresh_matching_source_wins_over_a_direct_disagreement() {
    for unreachable in [false, true] {
        let (mut engine, start, _) = verdict_wake();
        verdict_event(
            &mut engine,
            start,
            200,
            Event::Frame(Frame::from_hex(4, 0, "36").unwrap()),
        );
        verdict_event(&mut engine, start, 3200, Event::Tick);
        let id = engine
            .decisions
            .iter()
            .find(|decision| decision.reason == "off_timer_expired")
            .unwrap()
            .id;
        verdict_event(
            &mut engine,
            start,
            4000,
            Event::TvPower {
                state: TvState::On,
                at: wall() + chrono::Duration::milliseconds(4000),
            },
        );
        if unreachable {
            verdict_event(
                &mut engine,
                start,
                6000,
                Event::TvPower {
                    state: TvState::Unreachable,
                    at: wall() + chrono::Duration::milliseconds(6000),
                },
            );
        }
        let tv = engine.config.tv.entity.clone();
        let receiver = engine.config.receiver.entity.clone();
        verdict_ha(&mut engine, start, 8000, tv, "off");
        verdict_ha(&mut engine, start, 8100, receiver, "off");
        let update = engine
            .verdicts
            .iter()
            .find(|update| update.id == id)
            .unwrap();
        assert_eq!(update.verdict, Verdict::Achieved);
        assert_eq!(
            update.expectations[0].outcome,
            Outcome::Met {
                via: Via::Ha,
                observed: "off".into(),
                latency_ms: 4800
            }
        );
    }
}

#[test]
fn reload_cancels_only_expectations_whose_relevant_configuration_changed() {
    for relevant in [false, true] {
        let (mut engine, start, id) = verdict_wake();
        let mut config = (*engine.config).clone();
        if relevant {
            config.tv.entity = "media_player.replacement_tv".into();
        } else {
            config.trace.keep_days += 1;
        }
        verdict_event(
            &mut engine,
            start,
            500,
            Event::ConfigReloaded(std::sync::Arc::new(config)),
        );
        let tv = engine.config.tv.entity.clone();
        let receiver = engine.config.receiver.entity.clone();
        verdict_ha(&mut engine, start, 600, tv, "on");
        verdict_ha(&mut engine, start, 700, receiver, "on");
        let update = engine
            .verdicts
            .iter()
            .find(|update| update.id == id)
            .unwrap();
        assert_eq!(
            update.verdict,
            if relevant {
                Verdict::Partial
            } else {
                Verdict::Achieved
            }
        );
    }
}

#[test]
fn reload_of_the_other_receiver_deadline_preserves_a_pending_receiver_expectation() {
    // A pending receiver *on* expectation survives a `receiver_off_s` edit and vice versa.
    let (mut engine, start, id) = verdict_wake();
    let mut config = (*engine.config).clone();
    config.verify.receiver_off_s += 5;
    verdict_event(
        &mut engine,
        start,
        500,
        Event::ConfigReloaded(std::sync::Arc::new(config)),
    );
    let tv = engine.config.tv.entity.clone();
    let receiver = engine.config.receiver.entity.clone();
    verdict_ha(&mut engine, start, 600, tv, "on");
    verdict_ha(&mut engine, start, 700, receiver, "on");
    let update = engine
        .verdicts
        .iter()
        .find(|update| update.id == id)
        .unwrap();
    assert_eq!(update.verdict, Verdict::Achieved);

    let (mut engine, start, _) = verdict_wake();
    verdict_event(
        &mut engine,
        start,
        200,
        Event::Frame(Frame::from_hex(4, 0, "36").unwrap()),
    );
    verdict_event(&mut engine, start, 3200, Event::Tick);
    let id = engine
        .decisions
        .iter()
        .find(|decision| decision.reason == "off_timer_expired")
        .unwrap()
        .id;
    let mut config = (*engine.config).clone();
    config.verify.receiver_s += 5;
    verdict_event(
        &mut engine,
        start,
        3300,
        Event::ConfigReloaded(std::sync::Arc::new(config)),
    );
    let tv = engine.config.tv.entity.clone();
    let receiver = engine.config.receiver.entity.clone();
    verdict_ha(&mut engine, start, 4000, tv, "off");
    verdict_ha(&mut engine, start, 4100, receiver, "off");
    let update = engine
        .verdicts
        .iter()
        .find(|update| update.id == id)
        .unwrap();
    assert_eq!(update.verdict, Verdict::Achieved);
}

#[test]
fn bridge_offline_cancels_cec_only_but_preserves_independent_probe_expectations() {
    for probe in [false, true] {
        let (mut engine, start) = setup();
        let config = std::sync::Arc::make_mut(&mut engine.config);
        config.mode = Mode::Active;
        if probe {
            config.device[0].probe = Some(choregos::config::ProbeConfig::Ps5 {
                host: "console.example".into(),
                port: 9302,
            });
        }
        identify_atv(&mut engine, start);
        verdict_event(
            &mut engine,
            start,
            100,
            Event::Frame(Frame::from_hex(4, 0, "04").unwrap()),
        );
        verdict_event(
            &mut engine,
            start,
            1000,
            Event::Frame(Frame::from_hex(8, 15, "84230004").unwrap()),
        );
        let id = engine.decisions.last().unwrap().id;
        let tv = engine.config.tv.entity.clone();
        let receiver = engine.config.receiver.entity.clone();
        verdict_ha(&mut engine, start, 1100, tv, "on");
        verdict_ha(&mut engine, start, 1200, receiver, "on");
        verdict_event(&mut engine, start, 1300, Event::BridgeOffline);
        if probe {
            assert!(engine.verdicts.iter().all(|update| update.id != id));
            verdict_event(
                &mut engine,
                start,
                1400,
                Event::ConsolePower {
                    role: "streamer".into(),
                    state: choregos::state::ConsoleState::Standby,
                    at: wall() + chrono::Duration::milliseconds(1400),
                },
            );
        }
        let update = engine
            .verdicts
            .iter()
            .find(|update| update.id == id)
            .unwrap();
        assert_eq!(
            update.verdict,
            if probe {
                Verdict::Achieved
            } else {
                Verdict::Partial
            }
        );
    }
}

#[test]
fn bus_power_freshness_tracks_reports_not_identity_chatter_or_invalid_status() {
    let (mut engine, start) = setup();
    identify_atv(&mut engine, start);
    frame(&mut engine, start + Duration::from_secs(1), 4, 0, "04");
    assert_eq!(
        engine.bus.entries[&4].power_at,
        Some(start + Duration::from_secs(1))
    );
    frame(
        &mut engine,
        start + Duration::from_secs(2),
        4,
        15,
        "870010FA",
    );
    assert_eq!(
        engine.bus.entries[&4].power_at,
        Some(start + Duration::from_secs(1))
    );
    frame(&mut engine, start + Duration::from_secs(3), 4, 0, "9001");
    frame(
        &mut engine,
        start + Duration::from_millis(3500),
        4,
        0,
        "9007",
    );
    assert_eq!(
        engine.bus.entries[&4].power_at,
        Some(start + Duration::from_secs(3))
    );
    frame(&mut engine, start + Duration::from_secs(4), 4, 15, "822100");
    assert_eq!(engine.bus.entries[&4].power, BusPower::On);
    assert_eq!(
        engine.bus.entries[&4].power_at,
        Some(start + Duration::from_secs(4))
    );
}

#[test]
fn review_q7_transient_standby_does_not_confirm_a_device_still_on_at_exhaustion() {
    let (mut engine, start, settle) = takeover_from_streamer();
    for (ms, data) in [(300, "9001"), (600, "9000")] {
        frame(&mut engine, start + Duration::from_millis(ms), 4, 0, data);
        assert_eq!(
            standby_expectation(&engine, "streamer").outcome,
            Outcome::Pending
        );
    }
    for round in 1..=u32::from(engine.config.timing.standby_retries) + 1 {
        engine.handle(Event::Tick, start + settle * round, wall());
    }
    let expectation = standby_expectation(&engine, "streamer");
    assert_eq!(expectation.attempts, engine.config.timing.standby_retries);
    assert_eq!(
        expectation.outcome,
        Outcome::Missed {
            observed: "on".into()
        }
    );
}

#[test]
fn review_q6_only_checked_cec_confirmation_survives_a_later_user_wake() {
    for checked in [false, true] {
        let (mut engine, start) = setup();
        let config = std::sync::Arc::make_mut(&mut engine.config);
        config.mode = Mode::Active;
        if checked {
            config.timing.takeover_settle_ms = 1000;
        }
        config.device[0].probe = Some(choregos::config::ProbeConfig::Ps5 {
            host: "console.example".into(),
            port: 9302,
        });
        identify_atv(&mut engine, start);
        verdict_event(
            &mut engine,
            start,
            100,
            Event::Frame(Frame::from_hex(4, 0, "04").unwrap()),
        );
        verdict_event(
            &mut engine,
            start,
            1000,
            Event::Frame(Frame::from_hex(8, 15, "84230004").unwrap()),
        );
        let id = engine.decisions.last().unwrap().id;
        let tv = engine.config.tv.entity.clone();
        let receiver = engine.config.receiver.entity.clone();
        verdict_ha(&mut engine, start, 1100, tv, "on");
        verdict_ha(&mut engine, start, 1200, receiver, "on");
        verdict_event(
            &mut engine,
            start,
            2300,
            Event::Frame(Frame::from_hex(4, 0, "9001").unwrap()),
        );
        assert_eq!(
            standby_expectation(&engine, "streamer").outcome,
            Outcome::Pending
        );
        if checked {
            verdict_event(&mut engine, start, 2400, Event::Tick);
        }
        verdict_event(
            &mut engine,
            start,
            2600,
            Event::Frame(Frame::from_hex(4, 15, "822100").unwrap()),
        );
        let update = engine
            .verdicts
            .iter()
            .find(|update| update.id == id)
            .unwrap();
        let expectation = update
            .expectations
            .iter()
            .find(|expectation| expectation.subject == "role:streamer")
            .unwrap();
        if checked {
            assert_eq!(
                expectation.outcome,
                Outcome::CecOnly {
                    observed: "standby".into()
                }
            );
            assert_eq!(update.verdict, Verdict::Achieved);
        } else {
            assert_eq!(expectation.outcome, Outcome::Unknown { observed: None });
            assert_eq!(update.verdict, Verdict::Partial);
        }
        assert_eq!(engine.roles["streamer"].power, RolePower::On);
    }
}

fn probe_standby_verdict(cec: bool, probe_window_s: u64, settle_ms: u64) -> (Engine, Instant, u64) {
    let (mut engine, start) = setup();
    let config = std::sync::Arc::make_mut(&mut engine.config);
    config.mode = Mode::Active;
    config.verify.probe_window_s = probe_window_s;
    config.timing.takeover_settle_ms = settle_ms;
    config.device[0].verify_standby = cec;
    config.device[0].probe = Some(choregos::config::ProbeConfig::Ps5 {
        host: "console.example".into(),
        port: 9302,
    });
    identify_atv(&mut engine, start);
    verdict_event(
        &mut engine,
        start,
        100,
        Event::Frame(Frame::from_hex(4, 0, "04").unwrap()),
    );
    verdict_event(
        &mut engine,
        start,
        1000,
        Event::Frame(Frame::from_hex(8, 15, "84230004").unwrap()),
    );
    let id = engine.decisions.last().unwrap().id;
    let tv = engine.config.tv.entity.clone();
    let receiver = engine.config.receiver.entity.clone();
    verdict_ha(&mut engine, start, 1100, tv, "on");
    verdict_ha(&mut engine, start, 1200, receiver, "on");
    (engine, start, id)
}

#[test]
fn standby_deadline_covers_only_the_enabled_observation_windows() {
    for (cec, probe_window_s, settle_ms, expected_ms) in [
        (false, 17, 2000, 17000),
        (false, 5, 10000, 5000),
        (true, 17, 2000, 17000),
        (true, 5, 10000, 30000),
    ] {
        let (mut engine, start, id) = probe_standby_verdict(cec, probe_window_s, settle_ms);
        assert_eq!(
            standby_expectation(&engine, "streamer").deadline_ms,
            expected_ms
        );
        engine.settle_expectations(start + Duration::from_millis(1000 + expected_ms - 1));
        assert_eq!(
            standby_expectation(&engine, "streamer").outcome,
            Outcome::Pending
        );
        assert_eq!(
            engine.next_expectation_deadline(),
            Some(start + Duration::from_millis(1000 + expected_ms))
        );
        engine.settle_expectations(start + Duration::from_millis(1000 + expected_ms));
        assert_eq!(
            engine.expectations(id).unwrap().last().unwrap().outcome,
            Outcome::Unknown { observed: None }
        );
    }
}

#[test]
fn mixed_standby_probe_can_confirm_after_unchanged_cec_retries_exhaust() {
    let (mut engine, start, id) = probe_standby_verdict(true, 45, 2000);
    for ms in [3000, 5000, 7000] {
        verdict_event(
            &mut engine,
            start,
            ms - 1,
            Event::Frame(Frame::from_hex(4, 0, "9000").unwrap()),
        );
        let actions = verdict_event(&mut engine, start, ms, Event::Tick);
        let resends = actions
            .iter()
            .filter(|action| matches!(action, Action::CecSend { to: 4, data } if data == &[0x36]))
            .count();
        assert_eq!(resends, usize::from(ms != 7000));
        assert_eq!(
            standby_expectation(&engine, "streamer").outcome,
            Outcome::Pending
        );
    }
    assert_eq!(standby_expectation(&engine, "streamer").attempts, 2);
    verdict_event(
        &mut engine,
        start,
        11500,
        Event::ConsolePower {
            role: "streamer".into(),
            state: choregos::state::ConsoleState::Standby,
            at: wall() + chrono::Duration::milliseconds(11500),
        },
    );
    assert_eq!(
        engine.expectations(id).unwrap().last().unwrap().outcome,
        Outcome::Met {
            via: Via::Ps5,
            observed: "standby".into(),
            latency_ms: 10500,
        }
    );
}

#[test]
fn probe_deadline_uses_only_the_current_fresh_sample_for_disagreement() {
    use choregos::state::ConsoleState;
    for (initial_awake, latest, fresh, missed) in [
        (true, ConsoleState::Awake, true, true),
        (true, ConsoleState::Unreachable, true, false),
        (false, ConsoleState::Awake, false, false),
        (true, ConsoleState::Awake, false, false),
    ] {
        let (mut engine, start, id) = probe_standby_verdict(false, 17, 2000);
        if initial_awake {
            verdict_event(
                &mut engine,
                start,
                1400,
                Event::ConsolePower {
                    role: "streamer".into(),
                    state: ConsoleState::Awake,
                    at: wall() + chrono::Duration::milliseconds(1400),
                },
            );
        }
        verdict_event(
            &mut engine,
            start,
            2000,
            Event::ConsolePower {
                role: "streamer".into(),
                state: latest,
                at: wall() + chrono::Duration::milliseconds(if fresh { 2000 } else { 0 }),
            },
        );
        engine.settle_expectations(start + Duration::from_millis(18000));
        assert_eq!(
            engine.expectations(id).unwrap().last().unwrap().outcome,
            if missed {
                Outcome::Missed {
                    observed: "on".into(),
                }
            } else {
                Outcome::Unknown { observed: None }
            }
        );
    }
}

#[test]
fn unavailable_or_unknown_ha_replaces_its_earlier_disagreement() {
    for unavailable in ["unavailable", "unknown"] {
        let (mut engine, start, id) = verdict_wake();
        let receiver = engine.config.receiver.entity.clone();
        let tv = engine.config.tv.entity.clone();
        verdict_ha(&mut engine, start, 500, receiver, "on");
        verdict_ha(&mut engine, start, 600, tv.clone(), "off");
        verdict_ha(&mut engine, start, 700, tv, unavailable);
        let deadline = 100 + engine.config.verify.tv_s * 1000;
        verdict_event(&mut engine, start, deadline, Event::Tick);
        let update = engine
            .verdicts
            .iter()
            .find(|update| update.id == id)
            .unwrap();
        assert_eq!(update.verdict, Verdict::Partial);
        assert_eq!(
            update.expectations[0].outcome,
            Outcome::Unknown { observed: None }
        );
    }
}

#[test]
fn reload_cancels_probe_expectations_only_for_their_enabled_deadline_config() {
    for probe_window_changed in [false, true] {
        let (mut engine, start, id) = probe_standby_verdict(false, 17, 2000);
        let mut config = (*engine.config).clone();
        if probe_window_changed {
            config.verify.probe_window_s += 1;
        } else {
            config.timing.takeover_settle_ms += 1;
            config.timing.standby_retries += 1;
        }
        verdict_event(
            &mut engine,
            start,
            1300,
            Event::ConfigReloaded(std::sync::Arc::new(config)),
        );
        assert_eq!(
            engine.expectations(id).unwrap().last().unwrap().outcome,
            if probe_window_changed {
                Outcome::Unknown { observed: None }
            } else {
                Outcome::Pending
            }
        );
    }
}

#[test]
fn waking_a_role_cancels_its_earlier_probe_standby_expectation() {
    for cec in [false, true] {
        let (mut engine, start, id) = probe_standby_verdict(cec, 45, 2000);
        verdict_event(
            &mut engine,
            start,
            20000,
            Event::Frame(Frame::from_hex(4, 15, "822100").unwrap()),
        );
        assert_eq!(
            engine.expectations(id).unwrap().last().unwrap().outcome,
            Outcome::Unknown { observed: None }
        );
        verdict_event(
            &mut engine,
            start,
            22000,
            Event::ConsolePower {
                role: "streamer".into(),
                state: choregos::state::ConsoleState::Awake,
                at: wall() + chrono::Duration::milliseconds(22000),
            },
        );
        verdict_event(&mut engine, start, 46000, Event::Tick);
        assert!(engine.verdicts.iter().all(|update| update.id != id));
    }
}

#[test]
fn a_sample_arriving_after_the_probe_deadline_cannot_replace_deadline_evidence() {
    use choregos::state::ConsoleState;
    for tick_first in [false, true] {
        for latest in [ConsoleState::Awake, ConsoleState::Unreachable] {
            for late in [
                ConsoleState::Awake,
                ConsoleState::Standby,
                ConsoleState::Unreachable,
            ] {
                let (mut engine, start, id) = probe_standby_verdict(false, 17, 2000);
                verdict_event(
                    &mut engine,
                    start,
                    17000,
                    Event::ConsolePower {
                        role: "streamer".into(),
                        state: latest,
                        at: wall() + chrono::Duration::milliseconds(17000),
                    },
                );
                let expected = if latest == ConsoleState::Awake {
                    Outcome::Missed {
                        observed: "on".into(),
                    }
                } else {
                    Outcome::Unknown { observed: None }
                };
                if tick_first {
                    verdict_event(&mut engine, start, 18000, Event::Tick);
                    assert_eq!(
                        engine.expectations(id).unwrap().last().unwrap().outcome,
                        expected
                    );
                }
                verdict_event(
                    &mut engine,
                    start,
                    18050,
                    Event::ConsolePower {
                        role: "streamer".into(),
                        state: late,
                        at: wall() + chrono::Duration::milliseconds(18050),
                    },
                );
                if tick_first {
                    assert!(engine.verdicts.iter().all(|update| update.id != id));
                } else {
                    assert_eq!(
                        engine.expectations(id).unwrap().last().unwrap().outcome,
                        expected
                    );
                }
            }
        }
    }
}

#[test]
fn late_tv_or_ha_samples_cannot_replace_expired_disagreement() {
    for direct in [false, true] {
        let (mut engine, start, id) = verdict_wake();
        let receiver = engine.config.receiver.entity.clone();
        let tv = engine.config.tv.entity.clone();
        verdict_ha(&mut engine, start, 500, receiver, "on");
        let sample = |ms, state: TvState| {
            if direct {
                Event::TvPower {
                    state,
                    at: wall() + chrono::Duration::milliseconds(ms),
                }
            } else {
                Event::HaState {
                    entity: tv.clone(),
                    state: if state == TvState::On { "on" } else { "off" }.into(),
                    attrs: BTreeMap::new(),
                    changed: Some(wall() + chrono::Duration::milliseconds(ms)),
                    updated: None,
                }
            }
        };
        verdict_event(&mut engine, start, 44000, sample(44000, TvState::Off));
        verdict_event(&mut engine, start, 45150, sample(45150, TvState::On));
        assert_eq!(
            engine.expectations(id).unwrap()[0].outcome,
            Outcome::Missed {
                observed: "off".into(),
            }
        );
    }
}

#[test]
fn cancellation_preserves_checked_cec_evidence_while_waiting_for_the_probe() {
    for checked in [false, true] {
        for reload in [false, true] {
            let (mut engine, start, id) = probe_standby_verdict(true, 45, 2000);
            if checked {
                verdict_event(
                    &mut engine,
                    start,
                    2300,
                    Event::Frame(Frame::from_hex(4, 0, "9001").unwrap()),
                );
            }
            verdict_event(&mut engine, start, 3000, Event::Tick);
            assert_eq!(
                standby_expectation(&engine, "streamer").outcome,
                Outcome::Pending
            );
            let event = if reload {
                let mut config = (*engine.config).clone();
                config.verify.probe_window_s += 1;
                Event::ConfigReloaded(std::sync::Arc::new(config))
            } else {
                Event::Command(Command::SetMode(Mode::Shadow))
            };
            verdict_event(&mut engine, start, 4000, event);
            assert_eq!(
                engine.expectations(id).unwrap().last().unwrap().outcome,
                if checked {
                    Outcome::CecOnly {
                        observed: "standby".into(),
                    }
                } else {
                    Outcome::Unknown { observed: None }
                }
            );
        }
    }
}
