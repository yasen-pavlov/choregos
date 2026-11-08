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
    /// The recorded send this bridge outcome consumed, whether or not it yields a
    /// `SendResult` (a stock Tasmota reply, an aether rejection). Display metadata only.
    pub result_for: Option<SendKey>,
    /// An aether host transmission that yields no `SendResult` (unknown or expired id, no
    /// id, rejected, unknown status), as it appeared on the wire. Display metadata only;
    /// boxed because it is rare and `Decoded` travels inside the runtime's input enum.
    pub host_tx: Option<Box<HostTx>>,
}

/// The identity the adapter correlates a send by: its Tasmota FIFO position (counted over
/// the correlation's lifetime, never reused) or its aether request id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SendKey {
    Fifo(u64),
    Id(String),
}

/// An aether `tx` with `origin:"host"`: the frame and its status word (`ack`, `rejected`…).
#[derive(Debug)]
pub struct HostTx {
    pub frame: Frame,
    pub status: String,
    pub attempts: Option<u8>,
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
    /// Records a send for correlation; returns the FIFO position of a Tasmota send (an aether
    /// send is keyed by its id).
    pub fn record_send(
        &mut self,
        id: Option<String>,
        to: u8,
        data: Vec<u8>,
        now: Instant,
    ) -> Option<u64> {
        match id {
            Some(id) => {
                self.aether.record(id, to, data, now);
                None
            }
            None => Some(self.tasmota.record(to, data, now)),
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
                if let Some((position, event)) = self.tasmota.result(payload, now) {
                    decoded.result_for = Some(SendKey::Fifo(position));
                    decoded.events.extend(event);
                }
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
    #[test]
    fn tasmota_results_name_the_fifo_position_they_consume() {
        let now = Instant::now();
        let mut c = Correlation::default();
        let result = |ack: Value| TraceEntry::Result {
            bridge: "cec".into(),
            payload: json!({ "HdmiSend": ack }),
        };
        let positions: Vec<_> = (0..3)
            .map(|_| c.record_send(None, 4, vec![0x8F], now))
            .collect();
        assert_eq!(positions, [Some(0), Some(1), Some(2)]);
        assert_eq!(c.record_send(Some("s-1".into()), 4, vec![0x8F], now), None);
        // A stock reply consumes the oldest send without an engine event.
        let stock = c.apply(&result(json!("Done")), now);
        assert!(stock.events.is_empty());
        assert_eq!(stock.result_for, Some(SendKey::Fifo(0)));
        for (ack, position) in [(false, 1), (true, 2)] {
            let decoded = c.apply(&result(json!({ "Ack": ack })), now);
            assert_eq!(decoded.result_for, Some(SendKey::Fifo(position)));
            assert!(matches!(&decoded.events[..], [Event::SendResult { .. }]));
        }
        assert!(
            c.apply(&result(json!({"Ack": true})), now)
                .result_for
                .is_none()
        );
    }
}
