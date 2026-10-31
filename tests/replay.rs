use choregos::{
    action::{Action, IdentifyQueries},
    cec::Frame,
    config::{Config, Takeover},
    engine::{Engine, Event},
    state::{BusPower, Command, Mode, RolePower},
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
        let decision = engine
            .decisions
            .iter()
            .find(|decision| decision.reason == "standby_retry")
            .unwrap();
        assert_eq!(decision.role.as_deref(), Some("streamer"));
        assert_eq!(decision.actions, expected);
        assert!(actions.windows(2).any(|pair| pair == expected));
        if attempt == 1 {
            assert_eq!(engine.next_deadline(), Some(now + settle));
        }
    }
    let exhausted = takeover + settle * (u32::from(retries) + 1);
    engine.handle(Event::Tick, exhausted, wall());
    let decision = engine
        .decisions
        .iter()
        .find(|decision| decision.reason == "standby_unconfirmed")
        .unwrap();
    assert_eq!(decision.role.as_deref(), Some("streamer"));
    assert!(decision.actions.is_empty());
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
                    ack: false,
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
                    ack,
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
            let retry = engine
                .decisions
                .iter()
                .find(|decision| decision.reason == "standby_retry")
                .unwrap();
            assert_eq!(retry.actions.len(), 2);
        }
    }
    engine
}

#[test]
fn all_nacked_standby_frames_record_target_absent_after_bounded_retries() {
    let engine = exhaust_standby_with_results(&[(4, 0x36, false), (4, 0x8f, false)]);
    let decision = engine
        .decisions
        .iter()
        .find(|decision| decision.reason == "standby_target_absent")
        .unwrap();
    assert_eq!(decision.role.as_deref(), Some("streamer"));
    assert!(decision.actions.is_empty());
    assert_eq!(engine.bus.entries[&4].power, BusPower::On);
}

#[test]
fn mixed_ack_standby_frames_without_power_report_remain_unconfirmed() {
    let engine = exhaust_standby_with_results(&[(4, 0x36, false), (4, 0x8f, true)]);
    assert!(engine.decisions.iter().any(|decision| {
        decision.role.as_deref() == Some("streamer") && decision.reason == "standby_unconfirmed"
    }));
}

#[test]
fn send_results_for_other_las_and_opcodes_do_not_count_as_standby_nacks() {
    let engine =
        exhaust_standby_with_results(&[(8, 0x36, false), (8, 0x8f, false), (4, 0x83, false)]);
    assert!(engine.decisions.iter().any(|decision| {
        decision.role.as_deref() == Some("streamer") && decision.reason == "standby_unconfirmed"
    }));
}

#[test]
fn missing_standby_send_results_do_not_imply_absence() {
    let engine = exhaust_standby_with_results(&[(4, 0x36, false)]);
    assert!(engine.decisions.iter().any(|decision| {
        decision.role.as_deref() == Some("streamer") && decision.reason == "standby_unconfirmed"
    }));
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
                ack: false,
            },
            stopped,
            wall(),
        );
    }
    engine.handle(Event::Tick, stopped + settle, wall());
    assert!(engine.decisions.iter().any(|decision| {
        decision.role.as_deref() == Some("streamer") && decision.reason == "standby_target_absent"
    }));
}

#[test]
fn resync_nacks_during_verification_never_override_an_observed_ack() {
    // The extra 8F NACK belongs to resync, not to the pending standby pair.
    let engine =
        exhaust_standby_with_results(&[(4, 0x36, true), (4, 0x8f, false), (4, 0x8f, false)]);
    assert!(engine.decisions.iter().any(|decision| {
        decision.role.as_deref() == Some("streamer") && decision.reason == "standby_unconfirmed"
    }));
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
    let retry = engine
        .decisions
        .iter()
        .find(|decision| decision.reason == "standby_retry")
        .unwrap();
    assert_eq!(retry.role.as_deref(), Some("streamer"));
    assert_eq!(retry.actions, sent);
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
                ack: true,
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
                ack: false,
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
            assert_eq!(engine.active_role.as_deref(), Some("console_a"));
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
        assert_eq!(engine.active_role.as_deref(), Some("streamer"));
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
                            ack: false,
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
        let reason = if missing.is_none() {
            "standby_target_absent"
        } else {
            "standby_unconfirmed"
        };
        assert!(engine.decisions.iter().any(|decision| {
            decision.role.as_deref() == Some("streamer") && decision.reason == reason
        }));
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
                    ack: data == [0x36],
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
                ack: false,
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
    assert!(engine.decisions.iter().any(|decision| {
        decision.role.as_deref() == Some("streamer") && decision.reason == "standby_unconfirmed"
    }));
    assert!(
        !engine
            .decisions
            .iter()
            .any(|decision| { decision.reason == "standby_target_absent" })
    );
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
    assert_eq!(engine.decisions[0].reason, "standby_retry");
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
    assert!(engine.decisions.iter().any(|decision| {
        decision.role.as_deref() == Some("console_a") && decision.reason == "standby_retry"
    }));
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
    std::sync::Arc::make_mut(&mut engine.config)
        .timing
        .standby_retries = 0;
    let actions = engine.handle(Event::Tick, takeover + settle, wall());
    assert!(!actions.contains(&Action::CecSend {
        to: 4,
        data: vec![0x36],
    }));
    assert!(engine.decisions.iter().any(|decision| {
        decision.role.as_deref() == Some("streamer")
            && decision.reason == "standby_unconfirmed"
            && decision.actions.is_empty()
    }));
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
    assert_eq!(
        engine
            .decisions
            .iter()
            .filter(|decision| decision.reason == "standby_retry")
            .count(),
        1
    );
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
        assert_eq!(engine.active_role.as_deref(), Some("console_a"));
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
        assert!(engine.decisions.iter().any(|decision| {
            decision.role.as_deref() == Some("streamer") && decision.reason == "standby_retry"
        }));
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
    let wake = frame(&mut engine, now + Duration::from_millis(4001), 4, 0, "04");
    assert!(wake.contains(&Action::WakeTv));
    assert!(wake.iter().any(|a| service(a, "turn_on")));
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
        assert_eq!(engine.active_role.as_deref(), Some("streamer"));
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
fn optimistic_off_preserves_observed_ha_timestamps() {
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
    for entity in [&engine.config.tv.entity, &engine.config.receiver.entity] {
        assert_eq!(engine.ha[entity].state, "off");
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
    let mut engine = Engine::new(config.clone(), now, vec![]);
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
        let mut engine = Engine::new(config, now, vec![restored_identity(3, 0x2500)]);
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
    assert_eq!(engine.active_role.as_deref(), Some("console_a"));
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
    assert_eq!(engine.active_role, None);
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
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)]);
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
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)]);
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
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)]);
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
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)]);
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
        let mut engine = Engine::new(config, now, vec![identity]);
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
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)]);
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
    let mut engine = Engine::new(config, now, identities);
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
    let mut engine = Engine::new(config, now, vec![identity]);
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
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)]);
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
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)]);
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
        let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)]);
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
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)]);
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
    let mut engine = Engine::new(config, now, vec![restored_identity(4, 0x2100)]);
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
            ack: false,
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
    frame(&mut engine, retry_at, 11, 0, "9000");
    assert_eq!(engine.roles["console_b"].power, RolePower::Off);
    frame(&mut engine, retry_at, 11, 0, "9001");
    let after = retry_at + Duration::from_millis(engine.config.timing.takeover_settle_ms);
    let actions = engine.handle(Event::Tick, after, wall());
    assert!(
        actions
            .iter()
            .all(|action| !matches!(action, Action::CecSend { .. }))
    );
    assert_no_standby_decision(&engine, "console_b");
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
                        ack: acked,
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
        assert_eq!(
            engine
                .decisions
                .iter()
                .filter(|decision| decision.role.as_deref() == Some("console_b"))
                .map(|decision| decision.reason.as_str())
                .collect::<Vec<_>>(),
            vec!["standby_unconfirmed"]
        );
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
    let mut engine = Engine::new(config, now, vec![restored]);
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
            ack: true,
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
                    ack: false,
                },
                sent_at,
                wall(),
            );
        }
    }
    engine.handle(Event::Tick, now + settle * (u32::from(retries) + 1), wall());
    assert_eq!(
        engine
            .decisions
            .iter()
            .filter(|decision| decision.role.as_deref() == Some("console_b"))
            .map(|decision| decision.reason.as_str())
            .collect::<Vec<_>>(),
        vec!["standby_target_absent"]
    );
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
    let mut engine = Engine::new(config, now, vec![]);
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
    let mut engine = Engine::new(console_physical_change_config(), now, vec![identity]);
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
fn physical_address_change_from_bridge_still_replaces_its_keys() {
    let (mut engine, now) = setup();
    for data in ["84250004", "870009BF", "47436F6E736F6C652041", "822500"] {
        frame(&mut engine, now, 0, 15, data);
    }
    frame(&mut engine, now + Duration::from_secs(1), 0, 15, "84100004");
    assert_eq!(engine.bus.entries[&0].vendor, None);
    assert_eq!(engine.bus.entries[&0].osd, None);
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
