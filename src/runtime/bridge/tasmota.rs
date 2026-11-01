use super::Incoming;
use crate::{
    cec::Frame,
    engine::{Event, SendStatus},
    runtime::trace::TraceEntry,
    state::BridgeTelemetry,
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct OutstandingSends(VecDeque<(u8, Vec<u8>, Instant)>);
impl OutstandingSends {
    pub fn clear(&mut self) {
        self.0.clear();
    }
    pub fn record(&mut self, to: u8, data: Vec<u8>, now: Instant) {
        if self.0.len() == 64 {
            self.0.pop_front();
        }
        self.0.push_back((to, data, now));
    }
    pub fn result(&mut self, payload: &Value, now: Instant) -> Option<Event> {
        let result = payload.get("HdmiSend")?;
        while self
            .0
            .front()
            .is_some_and(|(_, _, at)| now.saturating_duration_since(*at) > Duration::from_secs(5))
        {
            self.0.pop_front();
        }
        let (to, data, _) = self.0.pop_front()?;
        let status = if result.get("Ack")?.as_bool()? {
            SendStatus::Ack
        } else {
            SendStatus::Nack
        };
        Some(Event::SendResult {
            to,
            data,
            status,
            attempts: None,
        })
    }
}

pub(super) fn decode(name: &str, topic: &str, payload: &[u8]) -> Option<Incoming> {
    let entry = if topic == format!("tele/{name}/STATE") {
        return telemetry(payload).map(Incoming::Telemetry);
    } else if topic == format!("tele/{name}/SENSOR") {
        TraceEntry::Received {
            bridge: name.into(),
            frame: Frame::parse(payload).ok()?,
        }
    } else if topic == format!("tele/{name}/LWT") {
        TraceEntry::Lwt {
            bridge: name.into(),
            online: match payload {
                b"Online" => true,
                b"Offline" => false,
                _ => return None,
            },
        }
    } else if topic == format!("stat/{name}/RESULT") {
        let payload: Value = serde_json::from_slice(payload).ok()?;
        payload.get("HdmiSend")?;
        TraceEntry::Result {
            bridge: name.into(),
            payload,
        }
    } else {
        return None;
    };
    Some(Incoming::Trace(entry))
}

fn telemetry(payload: &[u8]) -> Option<BridgeTelemetry> {
    fn integer(value: &Value) -> Option<i64> {
        if let Some(value) = value.as_i64() {
            return Some(value);
        }
        let value = value.as_f64()?;
        (value.is_finite()
            && value.fract() == 0.0
            && value >= i64::MIN as f64
            && value < -(i64::MIN as f64))
            .then_some(value as i64)
    }
    let value: Value = serde_json::from_slice(payload).ok()?;
    Some(BridgeTelemetry {
        signal_dbm: i32::try_from(integer(value.get("Wifi")?.get("Signal")?)?).ok()?,
        link_count: u32::try_from(integer(value.get("Wifi")?.get("LinkCount")?)?).ok()?,
        uptime_s: u64::try_from(integer(value.get("UptimeSec")?)?).ok()?,
        at: chrono::Utc::now(),
        stale_after_s: 900,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn telemetry_preserves_integral_floats_and_rejects_invalid_fields() {
        for payload in [
            serde_json::json!({"UptimeSec":282348,"Wifi":{"Signal":-50,"LinkCount":2}}),
            serde_json::json!({"UptimeSec":282348.0,"Wifi":{"Signal":-50.0,"LinkCount":2.0}}),
        ] {
            let t = telemetry(payload.to_string().as_bytes()).unwrap();
            assert_eq!(
                (t.signal_dbm, t.link_count, t.uptime_s, t.stale_after_s),
                (-50, 2, 282348, 900)
            );
        }
        for payload in [
            serde_json::json!({"UptimeSec":1,"Wifi":{"RSSI":100,"LinkCount":2}}),
            serde_json::json!({"UptimeSec":1,"Wifi":{"Signal":-50.5,"LinkCount":2}}),
            serde_json::json!({"UptimeSec":1,"Wifi":{"Signal":-50,"LinkCount":-1}}),
            serde_json::json!({"UptimeSec":-1,"Wifi":{"Signal":-50,"LinkCount":2}}),
            serde_json::json!({"UptimeSec":1,"Wifi":{"Signal":-50,"LinkCount":4294967296_u64}}),
        ] {
            assert!(telemetry(payload.to_string().as_bytes()).is_none());
        }
    }
    #[test]
    fn stock_replies_consume_entries_without_classifying_the_target() {
        let now = Instant::now();
        let mut pending = OutstandingSends::default();
        for value in [
            serde_json::json!("Done"),
            serde_json::json!("Sending failed"),
            serde_json::json!("Invalid JSON"),
            serde_json::json!({"Ack":true}),
            serde_json::json!({"Ack":false}),
        ] {
            pending.record(4, vec![0x36], now);
            let event = pending.result(&serde_json::json!({"HdmiSend":value}), now);
            assert_eq!(event.is_some(), value.is_object());
            assert!(pending.0.is_empty());
        }
    }
    #[test]
    fn fifo_expires_old_sends_bounds_memory_and_clears() {
        let now = Instant::now();
        let mut pending = OutstandingSends::default();
        for n in 0..65 {
            pending.record(4, vec![n], now);
        }
        let ack = serde_json::json!({"HdmiSend":{"Ack":true}});
        assert!(
            matches!(pending.result(&ack, now),Some(Event::SendResult { data, .. }) if data == [1])
        );
        assert!(
            pending
                .result(&ack, now + Duration::from_millis(5001))
                .is_none()
        );
        pending.record(4, vec![0x36], now);
        pending.clear();
        assert!(pending.result(&ack, now).is_none());
    }
}
