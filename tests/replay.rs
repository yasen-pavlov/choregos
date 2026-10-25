use choregos::{
    action::Action,
    cec::Frame,
    config::Config,
    engine::{Engine, Event},
    state::{BusPower, Command, RolePower},
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
            .find(|(index, action)| *index > preset && service(action, "turn_off"))
            .map(|(index, _)| index)
            .unwrap();
        assert!(effects[..wake].iter().any(|a| service(a, "turn_off")));
        assert!(wake < receiver_on && receiver_on < select && select < preset && preset < off);
        assert_eq!(effects.iter().filter(|a| service(a, "turn_off")).count(), 4); // Kodi release, then streamer release.
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
            .is_empty()
    );
    let settled = engine.handle(Event::Tick, now + Duration::from_millis(2000), wall());
    assert!(service(&settled[0], "select_source"));
    assert!(service(&settled[1], "turn_on"));
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
            .any(|a| service(a, "turn_off"))
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
    assert!(entry.identified);
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
    assert_eq!(engine.next_deadline(), Some(now + Duration::from_secs(7)));
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
    assert!(
        actions
            .iter()
            .all(|a| service(a, "turn_off") && suppressed(engine.mode(), a))
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
    assert_eq!(actions.iter().filter(|a| service(a, "turn_off")).count(), 2);
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
                .is_empty()
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
    assert!(
        engine
            .handle(Event::Tick, now + Duration::from_secs(6), wall())
            .iter()
            .any(|a| service(a, "turn_off"))
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
        engine
            .handle(Event::Tick, now + Duration::from_secs(6), wall())
            .iter()
            .filter(|a| service(a, "turn_off"))
            .count(),
        2
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
    assert_eq!(off.iter().filter(|a| service(a, "turn_off")).count(), 2);
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
    frame(&mut engine, now, 4, 15, "822100");
    assert!(
        frame(&mut engine, now, 4, 15, "85").contains(&Action::CecSend {
            to: 15,
            data: vec![0x82, 0x21, 0]
        })
    );
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
