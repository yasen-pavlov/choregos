//! Bridge codecs and the correlation used identically by live ingress and replay.
mod aether;
mod tasmota;

use super::trace::TraceEntry;
use crate::{cec::Frame, config::BridgeConfig, engine::Event, state::BridgeTelemetry};
use serde_json::Value;
use std::time::Instant;

#[derive(Default)]
pub struct Decoded {
    pub events: Vec<Event>,
    /// Full received bytes when the engine frame was reduced after a directed NACK.
    pub raw_received: Option<Frame>,
    pub local: Vec<Frame>,
    pub bus: Option<String>,
}

pub enum Incoming {
    Trace(TraceEntry),
    Telemetry(BridgeTelemetry),
    Cfg(Value),
}

#[derive(Default)]
pub struct Correlation {
    tasmota: tasmota::OutstandingSends,
    aether: aether::Correlation,
}
impl Correlation {
    /// A transport reconnect is a new observation session: pending sends and
    /// availability start over (the retained status is an `Online` again), the
    /// board's boot/sequence history is kept for deduplication.
    pub fn clear(&mut self) {
        self.tasmota.clear();
        self.aether.reconnect();
    }
    pub fn record_send(&mut self, id: Option<String>, to: u8, data: Vec<u8>, now: Instant) {
        match id {
            Some(id) => self.aether.record(id, to, data, now),
            None => self.tasmota.record(to, data, now),
        }
    }
    pub fn apply(&mut self, entry: &TraceEntry, now: Instant) -> Decoded {
        let mut decoded = Decoded::default();
        match entry {
            TraceEntry::Received { frame, .. } => decoded.events.push(Event::Frame(frame.clone())),
            TraceEntry::Lwt { online, .. } => {
                self.tasmota.clear();
                decoded.events.push(if *online {
                    Event::BridgeOnline
                } else {
                    Event::BridgeOffline
                });
            }
            TraceEntry::Result { payload, .. } => {
                decoded.events.extend(self.tasmota.result(payload, now))
            }
            TraceEntry::AetherStatus { payload, .. } => return self.aether.status(payload),
            TraceEntry::AetherBus { payload, .. } => return self.aether.bus(payload, now),
            _ => {}
        }
        decoded
    }
    pub fn configure(&mut self, payload: &Value, config: &BridgeConfig, fake_tv_replies: bool) {
        self.aether
            .configure(payload, config.logical_address(), fake_tv_replies);
    }
    pub fn telemetry_stale_after_s(&self) -> u64 {
        self.aether.telemetry_s.saturating_mul(3)
    }
}

pub fn subscriptions(config: &BridgeConfig) -> Vec<String> {
    match config {
        BridgeConfig::Tasmota { topic, .. } => [
            format!("tele/{topic}/SENSOR"),
            format!("tele/{topic}/LWT"),
            format!("tele/{topic}/STATE"),
            format!("stat/{topic}/RESULT"),
        ]
        .into(),
        BridgeConfig::Aether { node, .. } => ["status", "bus", "telemetry", "cfg"]
            .map(|suffix| format!("aether/{node}/{suffix}"))
            .into(),
    }
}

pub fn decode(config: &BridgeConfig, topic: &str, payload: &[u8]) -> Option<Incoming> {
    match config {
        BridgeConfig::Tasmota { topic: name, .. } => tasmota::decode(name, topic, payload),
        BridgeConfig::Aether { node, .. } => aether::decode(node, topic, payload),
    }
}

pub fn publish_send(
    config: &BridgeConfig,
    counter: u64,
    to: u8,
    data: &[u8],
) -> (String, String, Option<String>) {
    let data = hex::encode_upper(data);
    match config {
        BridgeConfig::Tasmota { topic, .. } => (
            format!("cmnd/{topic}/HdmiSend"),
            serde_json::json!({"To":to,"Data":data}).to_string(),
            None,
        ),
        BridgeConfig::Aether { node, .. } => {
            let id = format!("s-{counter}");
            (
                format!("aether/{node}/tx"),
                serde_json::json!({"id":id,"to":to,"data":data}).to_string(),
                Some(id),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bridge_availability_and_connection_changes_discard_stale_results() {
        let now = Instant::now();
        let mut c = Correlation::default();
        let result = TraceEntry::Result {
            bridge: "cec".into(),
            payload: json!({"HdmiSend":{"Ack":true}}),
        };
        for online in [false, true] {
            c.record_send(None, 4, vec![0x36], now);
            c.apply(
                &TraceEntry::Lwt {
                    bridge: "cec".into(),
                    online,
                },
                now,
            );
            assert!(c.apply(&result, now).events.is_empty());
        }
        c.record_send(None, 4, vec![0x36], now);
        c.clear();
        assert!(c.apply(&result, now).events.is_empty());
    }
}
