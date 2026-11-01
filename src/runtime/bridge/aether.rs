use super::{Decoded, Incoming};
use crate::{
    cec::Frame,
    engine::{Event, SendStatus},
    runtime::trace::TraceEntry,
    state::BridgeTelemetry,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

type Pending = (u8, Vec<u8>, Instant);
const DEFAULT_TELEMETRY_S: u64 = 60;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Availability {
    Unknown,
    Offline,
    Online,
}

pub(super) struct Correlation {
    pending: HashMap<String, Pending>,
    boot: Option<u32>,
    last_seq: Option<u32>,
    availability: Availability,
    last_status_t_us: Option<u64>,
    pub telemetry_s: u64,
    warned_version: bool,
    warned_la: bool,
    warned_replies: bool,
}
impl Default for Correlation {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            boot: None,
            last_seq: None,
            availability: Availability::Unknown,
            last_status_t_us: None,
            telemetry_s: DEFAULT_TELEMETRY_S,
            warned_version: false,
            warned_la: false,
            warned_replies: false,
        }
    }
}
impl Correlation {
    /// A new MQTT session of ours, not of the board: keep its boot and sequence
    /// history so a QoS 1 redelivery around a broker restart still dedupes.
    pub fn reconnect(&mut self) {
        *self = Self {
            boot: self.boot,
            last_seq: self.last_seq,
            last_status_t_us: self.last_status_t_us,
            ..Self::default()
        };
    }
}
impl Correlation {
    pub fn record(&mut self, id: String, to: u8, data: Vec<u8>, now: Instant) {
        self.pending
            .retain(|_, (_, _, at)| now.saturating_duration_since(*at) < Duration::from_secs(5));
        if self.pending.len() >= 64
            && let Some(oldest) = self
                .pending
                .iter()
                .min_by_key(|(id, (_, _, at))| (*at, *id))
                .map(|(id, _)| id.clone())
        {
            self.pending.remove(&oldest);
        }
        self.pending.insert(id, (to, data, now));
    }
    pub fn status(&mut self, payload: &Value) -> Decoded {
        #[derive(Deserialize)]
        struct Status {
            online: bool,
            v: Option<u32>,
            boot: Option<u32>,
            bus: Option<String>,
            t_us: Option<u64>,
        }
        let Ok(status) = serde_json::from_value::<Status>(payload.clone()) else {
            return Decoded::default();
        };
        let mut decoded = Decoded::default();
        if !status.online {
            self.pending.clear();
            self.availability = Availability::Offline;
            decoded.events.push(Event::BridgeOffline);
            return decoded;
        }
        if status.v != Some(1) {
            if !self.warned_version {
                tracing::error!(version = ?status.v, "unsupported aether protocol version");
                self.warned_version = true;
            }
            if self.availability == Availability::Online {
                decoded.events.push(Event::BridgeOffline);
            }
            self.pending.clear();
            self.availability = Availability::Offline;
            return decoded;
        }
        let Some(boot) = status.boot else {
            return decoded;
        };
        let boot_changed = self.boot != Some(boot);
        let fresh_status = status
            .t_us
            .is_some_and(|t_us| self.last_status_t_us.is_none_or(|last| t_us > last));
        if boot_changed || self.availability != Availability::Online || fresh_status {
            if boot_changed && self.availability == Availability::Online {
                decoded.events.push(Event::BridgeOffline);
            }
            self.pending.clear();
            if boot_changed {
                self.last_seq = None;
                self.last_status_t_us = None;
            }
            self.availability = Availability::Online;
            self.boot = Some(boot);
            decoded.events.push(Event::BridgeOnline);
        }
        if let Some(t_us) = status.t_us {
            self.last_status_t_us = Some(self.last_status_t_us.map_or(t_us, |last| last.max(t_us)));
        }
        decoded.bus = valid_bus(status.bus.as_deref());
        decoded
    }
    pub fn bus(&mut self, payload: &Value, now: Instant) -> Decoded {
        let mut decoded = Decoded::default();
        if self.availability == Availability::Offline {
            return decoded;
        }
        let Some(seq) = payload
            .get("seq")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
        else {
            return decoded;
        };
        if let Some(previous) = self.last_seq {
            if seq <= previous {
                return decoded;
            }
            if seq - previous > 1 {
                tracing::warn!(previous, seq, "gap in aether bus stream");
            }
        }
        self.last_seq = Some(seq);
        match payload.get("ev").and_then(Value::as_str) {
            Some("rx") => {
                if let Some(frame) = frame(payload) {
                    if frame.to != 15 && frame.ack == Some(false) {
                        let header = Frame {
                            opcode: None,
                            params: vec![],
                            ..frame
                        };
                        decoded.raw_received = Some(frame);
                        decoded.events.push(Event::Frame(header));
                    } else {
                        decoded.events.push(Event::Frame(frame));
                    }
                }
            }
            Some("tx") => self.tx(payload, now, &mut decoded),
            Some("line") => {
                decoded.bus = valid_bus(payload.get("bus").and_then(Value::as_str));
                if payload.get("hpd").is_some() || payload.get("la").is_some() {
                    tracing::debug!(%payload, "aether line metadata");
                }
            }
            Some("err") => tracing::debug!(%payload, "aether receive error"),
            _ => tracing::debug!(%payload, "unknown aether bus event"),
        }
        decoded
    }
    fn tx(&mut self, payload: &Value, now: Instant, decoded: &mut Decoded) {
        let status = payload.get("status").and_then(Value::as_str);
        if payload.get("origin").and_then(Value::as_str) == Some("local") {
            let ack = match status {
                Some("ack") => Some(true),
                Some("nack") => Some(false),
                _ => None,
            };
            if let Some(mut frame) = frame(payload) {
                frame.ack = ack;
                decoded.local.push(frame);
            }
            return;
        }
        if payload.get("origin").and_then(Value::as_str) != Some("host") {
            return;
        }
        let Some(id) = payload.get("id").and_then(Value::as_str) else {
            if status == Some("rejected") {
                tracing::warn!(reason = ?payload.get("reason"), "aether rejected send");
            }
            return;
        };
        self.pending
            .retain(|_, (_, _, at)| now.saturating_duration_since(*at) < Duration::from_secs(5));
        let Some((to, data, _)) = self.pending.remove(id) else {
            tracing::debug!(id, "unknown or expired aether send outcome");
            return;
        };
        let status = match status {
            Some("ack") => SendStatus::Ack,
            Some("nack") => SendStatus::Nack,
            Some("arb_lost") => SendStatus::ArbLost,
            Some("line_error") => SendStatus::LineError,
            Some("busy") => SendStatus::Busy,
            Some("rejected") => {
                tracing::warn!(id, reason = ?payload.get("reason"), "aether rejected send");
                return;
            }
            _ => return,
        };
        let attempts = payload
            .get("attempts")
            .and_then(Value::as_u64)
            .and_then(|v| u8::try_from(v).ok());
        decoded.events.push(Event::SendResult {
            to,
            data,
            status,
            attempts,
        });
    }
    pub fn configure(&mut self, payload: &Value, logical_address: u8, fake_tv_replies: bool) {
        if payload
            .get("la")
            .is_some_and(|la| la.as_u64() != Some(u64::from(logical_address)))
            && !self.warned_la
        {
            tracing::warn!(expected = logical_address, actual = ?payload.get("la"), "aether logical address differs from configuration");
            self.warned_la = true;
        }
        if fake_tv_replies
            && payload
                .get("replies")
                .and_then(Value::as_array)
                .is_some_and(|r| !r.is_empty())
            && !self.warned_replies
        {
            tracing::warn!("aether local replies overlap with fake_tv.replies");
            self.warned_replies = true;
        }
        if let Some(period) = payload.get("telemetry_s").and_then(Value::as_u64) {
            self.telemetry_s = period;
        }
    }
}
fn valid_bus(bus: Option<&str>) -> Option<String> {
    bus.filter(|bus| matches!(*bus, "ok" | "stuck_low" | "no_pullup"))
        .map(str::to_owned)
}
fn frame(payload: &Value) -> Option<Frame> {
    let mut frame = Frame::from_hex(
        u8::try_from(payload.get("from")?.as_u64()?).ok()?,
        u8::try_from(payload.get("to")?.as_u64()?).ok()?,
        payload.get("data")?.as_str()?,
    )
    .ok()?;
    frame.ack = payload.get("ack").and_then(Value::as_bool);
    frame.t_us = payload.get("t_us").and_then(Value::as_u64);
    Some(frame)
}
pub(super) fn decode(node: &str, topic: &str, payload: &[u8]) -> Option<Incoming> {
    let mut parts = topic.split('/');
    if parts.next()? != "aether" || parts.next()? != node {
        return None;
    }
    let suffix = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let payload: Value = serde_json::from_slice(payload).ok()?;
    if !payload.is_object() {
        return None;
    }
    Some(match suffix {
        "status" => Incoming::Trace(TraceEntry::AetherStatus {
            node: node.into(),
            payload,
        }),
        "bus" => Incoming::Trace(TraceEntry::AetherBus {
            node: node.into(),
            payload,
        }),
        "cfg" => Incoming::Cfg(payload),
        "telemetry" => Incoming::Telemetry(BridgeTelemetry {
            signal_dbm: i32::try_from(payload.get("rssi")?.as_i64()?).ok()?,
            link_count: u32::try_from(payload.get("link_count")?.as_u64()?).ok()?,
            uptime_s: payload.get("t_us")?.as_u64()? / 1_000_000,
            at: chrono::Utc::now(),
            stale_after_s: DEFAULT_TELEMETRY_S * 3,
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn online(c: &mut Correlation, boot: u32) -> Decoded {
        c.status(&json!({"online":true,"v":1,"boot":boot,"bus":"ok","future":true}))
    }
    fn outcome(seq: u32, id: &str, status: &str) -> Value {
        json!({"ev":"tx","seq":seq,"t_us":123456,"id":id,"origin":"host","from":0,"to":4,"data":"36","status":status,"attempts":2})
    }
    #[test]
    fn status_tracks_online_offline_boot_version_and_reconnect() {
        let mut c = Correlation::default();
        assert!(matches!(
            &online(&mut c, 1).events[..],
            [Event::BridgeOnline]
        ));
        assert!(online(&mut c, 1).events.is_empty());
        assert!(matches!(
            &online(&mut c, 2).events[..],
            [Event::BridgeOffline, Event::BridgeOnline]
        ));
        assert!(matches!(
            &c.status(&json!({"online":false})).events[..],
            [Event::BridgeOffline]
        ));
        assert!(matches!(
            &online(&mut c, 2).events[..],
            [Event::BridgeOnline]
        ));
        assert!(matches!(
            &c.status(&json!({"online":true,"v":2,"boot":2})).events[..],
            [Event::BridgeOffline]
        ));
        assert!(c.warned_version);
        assert!(
            c.status(&json!({"online":true,"v":2,"boot":2}))
                .events
                .is_empty()
        );
        assert!(
            c.bus(
                &json!({"ev":"rx","seq":1,"from":4,"to":0,"data":"04"}),
                Instant::now()
            )
            .events
            .is_empty()
        );
        assert!(matches!(
            &online(&mut c, 2).events[..],
            [Event::BridgeOnline]
        ));
        let mut shared = super::super::Correlation::default();
        let status = TraceEntry::AetherStatus {
            node: "cec-bridge".into(),
            payload: json!({"online":true,"v":1,"boot":2}),
        };
        assert_eq!(shared.apply(&status, Instant::now()).events.len(), 1);
        shared.clear();
        assert_eq!(shared.apply(&status, Instant::now()).events.len(), 1);
    }
    #[test]
    fn rx_preserves_poll_ack_timestamp_and_deduplicates_every_kind() {
        let mut c = Correlation::default();
        online(&mut c, 1);
        let now = Instant::now();
        let rx = json!({"ev":"rx","seq":4,"t_us":456,"from":5,"to":5,"data":"","ack":false});
        assert!(matches!(
            &c.bus(&rx, now).events[..],
            [Event::Frame(Frame {
                opcode: None,
                ack: Some(false),
                t_us: Some(456),
                ..
            })]
        ));
        assert!(c.bus(&rx, now).events.is_empty());
        assert!(
            c.bus(&json!({"ev":"line","seq":3,"bus":"stuck_low"}), now)
                .bus
                .is_none()
        );
        assert_eq!(
            c.bus(&json!({"ev":"line","seq":6,"bus":"stuck_low"}), now)
                .bus
                .as_deref(),
            Some("stuck_low")
        );
        assert_eq!(c.last_seq, Some(6));
        online(&mut c, 2);
        assert!(!c.bus(&rx, now).events.is_empty());
    }
    #[test]
    fn host_results_use_recorded_id_not_reported_target_and_classify_all_statuses() {
        let mut c = Correlation::default();
        online(&mut c, 1);
        let now = Instant::now();
        for (i, (wire, expected)) in [
            ("ack", SendStatus::Ack),
            ("nack", SendStatus::Nack),
            ("arb_lost", SendStatus::ArbLost),
            ("line_error", SendStatus::LineError),
            ("busy", SendStatus::Busy),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("s-{i}");
            c.record(id.clone(), 8, vec![0x8f], now);
            let mut value = outcome(i as u32 + 1, &id, wire);
            if wire == "busy" {
                value.as_object_mut().unwrap().remove("attempts");
            }
            assert!(
                matches!(&c.bus(&value, now).events[..], [Event::SendResult { to: 8, data, status, attempts }] if data == &[0x8f] && *status == expected && *attempts == if wire == "busy" {None} else {Some(2)})
            );
        }
        c.record("reject".into(), 4, vec![0x36], now);
        let mut rejected = outcome(6, "reject", "rejected");
        rejected["reason"] = json!("no_la");
        assert!(c.bus(&rejected, now).events.is_empty());
        assert!(c.pending.is_empty());
        assert!(c.bus(&outcome(7, "manual", "ack"), now).events.is_empty());
    }
    #[test]
    fn pending_ids_expire_at_five_seconds_are_bounded_and_clear_on_boot_or_offline() {
        let mut c = Correlation::default();
        online(&mut c, 1);
        let now = Instant::now();
        c.record("expired".into(), 4, vec![0x36], now);
        assert!(
            c.bus(&outcome(1, "expired", "ack"), now + Duration::from_secs(5))
                .events
                .is_empty()
        );
        for i in 0..65 {
            c.record(
                format!("s-{i}"),
                4,
                vec![0x36],
                now + Duration::from_millis(i),
            );
        }
        assert_eq!(c.pending.len(), 64);
        assert!(!c.pending.contains_key("s-0"));
        online(&mut c, 2);
        assert!(c.pending.is_empty());
        c.record("offline".into(), 4, vec![0x36], now);
        c.status(&json!({"online":false}));
        assert!(c.pending.is_empty());
        assert!(c.bus(&outcome(2, "offline", "ack"), now).events.is_empty());
    }
    #[test]
    fn local_transmissions_are_ui_only_and_line_error_metadata_is_diagnostic() {
        let mut c = Correlation::default();
        online(&mut c, 1);
        let now = Instant::now();
        let local = c.bus(&json!({"ev":"tx","seq":1,"t_us":456,"origin":"local","from":0,"to":15,"data":"84000000","status":"ack","attempts":1}),now);
        assert!(local.events.is_empty());
        assert!(matches!(
            &local.local[..],
            [Frame {
                from: 0,
                to: 15,
                ack: Some(true),
                t_us: Some(456),
                ..
            }]
        ));
        assert_eq!(
            c.bus(&json!({"ev":"line","seq":2,"bus":"no_pullup"}), now)
                .bus
                .as_deref(),
            Some("no_pullup")
        );
        assert!(
            c.bus(&json!({"ev":"line","seq":3,"hpd":true,"la":4}), now)
                .bus
                .is_none()
        );
        assert!(
            c.bus(
                &json!({"ev":"err","seq":4,"kind":"overrun","dropped":4}),
                now
            )
            .events
            .is_empty()
        );
    }

    #[test]
    fn same_boot_reconnect_deduplicates_and_newer_status_resyncs_without_resetting_seq() {
        let mut c = Correlation::default();
        let now = Instant::now();
        let status = |boot, t_us| json!({"online":true,"v":1,"boot":boot,"t_us":t_us});
        let rx = json!({"ev":"rx","seq":9,"from":4,"to":0,"data":"4441","ack":true});
        assert!(matches!(
            &c.status(&status(1, 100)).events[..],
            [Event::BridgeOnline]
        ));
        assert_eq!(c.bus(&rx, now).events.len(), 1);
        c.record("old".into(), 4, vec![0x36], now);
        c.status(&json!({"online":false}));
        assert!(c.pending.is_empty());
        assert_eq!(c.boot, Some(1));
        assert_eq!(c.last_seq, Some(9));
        assert!(matches!(
            &c.status(&status(1, 200)).events[..],
            [Event::BridgeOnline]
        ));
        assert!(c.bus(&rx, now).events.is_empty());
        for t_us in [200, 199] {
            assert!(c.status(&status(1, t_us)).events.is_empty());
        }
        c.record("stale".into(), 4, vec![0x36], now);
        assert!(matches!(
            &c.status(&status(1, 201)).events[..],
            [Event::BridgeOnline]
        ));
        assert!(c.pending.is_empty());
        assert_eq!(c.last_seq, Some(9));
        assert!(c.status(&status(1, 200)).events.is_empty());
        assert!(matches!(
            &c.status(&status(2, 1)).events[..],
            [Event::BridgeOffline, Event::BridgeOnline]
        ));
        assert_eq!(c.last_seq, None);
        assert_eq!(c.bus(&rx, now).events.len(), 1);
    }

    #[test]
    fn our_own_reconnect_keeps_the_boards_sequence_history_but_goes_online_again() {
        let mut c = Correlation::default();
        let now = Instant::now();
        let status = json!({"online":true,"v":1,"boot":1,"t_us":100});
        let rx = json!({"ev":"rx","seq":100,"from":4,"to":5,"data":"4441","ack":true});
        c.status(&status);
        assert_eq!(c.bus(&rx, now).events.len(), 1);
        c.record("s-1".into(), 4, vec![0x36], now);
        c.reconnect();
        assert!(c.pending.is_empty());
        // Retained status redelivered with the same boot and t_us: online again.
        assert!(matches!(
            &c.status(&status).events[..],
            [Event::BridgeOnline]
        ));
        // The board's QoS 1 redelivery of seq 100 around the broker restart.
        assert!(c.bus(&rx, now).events.is_empty());
        assert_eq!(
            c.bus(
                &json!({"ev":"rx","seq":101,"from":4,"to":5,"data":"4441","ack":true}),
                now
            )
            .events
            .len(),
            1
        );
    }

    #[test]
    fn unknown_availability_accepts_frames_but_explicit_offline_blocks_them() {
        let mut c = Correlation::default();
        let now = Instant::now();
        let rx = json!({"ev":"rx","seq":1,"from":4,"to":15,"data":"822100","ack":false});
        assert!(matches!(
            &c.bus(&rx, now).events[..],
            [Event::Frame(Frame {
                opcode: Some(0x82),
                ..
            })]
        ));
        c.status(&json!({"online":false}));
        assert!(
            c.bus(&json!({"ev":"rx","seq":2,"from":4,"to":0,"data":"04"}), now)
                .events
                .is_empty()
        );
    }

    #[test]
    fn local_outcomes_only_report_ack_or_nack_when_observed() {
        let mut c = Correlation::default();
        for (seq, (status, expected)) in [
            ("ack", Some(true)),
            ("nack", Some(false)),
            ("arb_lost", None),
            ("line_error", None),
            ("busy", None),
            ("rejected", None),
            ("unknown", None),
        ]
        .into_iter()
        .enumerate()
        {
            let decoded = c.bus(&json!({"ev":"tx","seq":seq,"origin":"local","from":0,"to":4,"data":"9000","status":status,"ack":true}), Instant::now());
            assert!(decoded.events.is_empty());
            assert_eq!(decoded.local[0].ack, expected, "{status}");
        }
    }
    #[test]
    fn telemetry_and_effective_config_control_staleness_and_overlap_warnings() {
        let mut shared = super::super::Correlation::default();
        assert_eq!(shared.telemetry_stale_after_s(), 180);
        let config = crate::config::BridgeConfig::Aether {
            node: "cec-bridge".into(),
            logical_address: 0,
        };
        shared.configure(
            &json!({"la":4,"replies":["83"],"telemetry_s":30}),
            &config,
            true,
        );
        assert_eq!(shared.telemetry_stale_after_s(), 90);
        assert!(shared.aether.warned_la && shared.aether.warned_replies);
        shared.configure(
            &json!({"la":0,"replies":[],"telemetry_s":0}),
            &config,
            false,
        );
        assert_eq!(shared.telemetry_stale_after_s(), 0);
        shared.clear();
        assert!(!shared.aether.warned_la && !shared.aether.warned_replies);
        let Incoming::Telemetry(t) = decode(
            "cec-bridge",
            "aether/cec-bridge/telemetry",
            br#"{"t_us":60000123,"boot":3,"rssi":-58,"link_count":2,"rx":{"frames":1}}"#,
        )
        .unwrap() else {
            panic!("telemetry")
        };
        assert_eq!(
            (t.signal_dbm, t.link_count, t.uptime_s, t.stale_after_s),
            (-58, 2, 60, 180)
        );
        assert!(decode("cec-bridge", "aether/other/telemetry", b"{}").is_none());
        assert!(decode("cec-bridge", "aether/cec-bridge/bus/extra", b"{}").is_none());
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    use serde_json::json;

    // Diagnostics are state, not log text: tracing's callsite interest cache
    // makes per-test subscribers unreliable under parallel tests.
    #[test]
    fn duplicates_are_dropped_after_gaps_and_unsupported_versions_block_bus_events() {
        let mut c = Correlation::default();
        c.status(&json!({"online":true,"v":2,"boot":1}));
        assert!(c.warned_version && c.availability == Availability::Offline);
        assert!(
            c.bus(&json!({"ev":"line","seq":1,"bus":"ok"}), Instant::now())
                .bus
                .is_none(),
            "bus events are ignored on an unsupported version"
        );
        c = Correlation::default();
        c.status(&json!({"online":true,"v":1,"boot":1}));
        let now = Instant::now();
        let seen: Vec<bool> = [1, 4, 4, 5, 8, 8]
            .into_iter()
            .map(|seq| {
                c.bus(&json!({"ev":"line","seq":seq,"bus":"ok"}), now)
                    .bus
                    .is_some()
            })
            .collect();
        assert_eq!(seen, [true, true, false, true, true, false]);
        assert_eq!(c.last_seq, Some(8));
    }
}
