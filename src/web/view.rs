//! Presentation helpers shared by templates: layout chrome and value formatting.

use chrono::{DateTime, Utc};

use super::auth::{Ctx, SessionUser};
use crate::state::{
    BridgeTelemetry, BusPower, FrameDirection, HaEntityView, Health, Mode, RolePower, Snapshot,
};

/// Bridge Wi-Fi signal below this (dBm) is flagged on the dashboard.
pub(crate) const WEAK_SIGNAL_DBM: i32 = -75;
/// The bridge publishes telemetry every `TelePeriod` (300 s by default); three missed
/// periods while it is online means the telemetry (or the link) is stuck.
pub(crate) const STALE_TELEMETRY: chrono::TimeDelta = chrono::TimeDelta::minutes(15);

/// Chrome rendered by `base.html` around every authenticated page.
pub(crate) struct Layout {
    pub(crate) page: &'static str,
    pub(crate) csrf: String,
    pub(crate) user: SessionUser,
    pub(crate) mode: Mode,
    pub(crate) warming: bool,
    pub(crate) health: Health,
    /// Header bridge badge tooltip suffix (`−50 dBm`) once telemetry exists.
    pub(crate) bridge_signal: Option<String>,
}

impl Layout {
    pub(crate) fn new(ctx: Ctx, page: &'static str, snapshot: &Snapshot) -> Self {
        Self {
            page,
            csrf: ctx.csrf,
            user: ctx.user,
            mode: snapshot.mode,
            warming: snapshot.warming,
            health: snapshot.connections.clone(),
            bridge_signal: snapshot
                .bridge_telemetry
                .as_ref()
                .map(|telemetry| dbm(&telemetry.signal_dbm)),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.mode == Mode::Active
    }
}

// Askama passes call arguments by reference, hence the `&` parameters below.

pub(crate) fn clock(timestamp: &DateTime<Utc>) -> String {
    timestamp.format("%H:%M:%S%.3f").to_string()
}

pub(crate) fn iso(timestamp: &DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(crate) fn ago(timestamp: &Option<DateTime<Utc>>) -> String {
    timestamp.as_ref().map_or_else(|| "never".to_owned(), since)
}

pub(crate) fn since(timestamp: &DateTime<Utc>) -> String {
    ago_at(*timestamp, Utc::now())
}

fn ago_at(timestamp: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - timestamp).num_seconds().max(0);
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// Compact uptime: the two most significant units (`3d 6h`, `6h 25m`), else one (`25m`, `48s`).
pub(crate) fn uptime(seconds: &u64) -> String {
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    match (days, hours, minutes) {
        (1.., ..) => format!("{days}d {hours}h"),
        (0, 1.., _) => format!("{hours}h {minutes}m"),
        (0, 0, 1..) => format!("{minutes}m"),
        (0, 0, 0) => format!("{seconds}s"),
    }
}

/// Signal strength with a typographic minus (`−50 dBm`).
pub(crate) fn dbm(signal: &i32) -> String {
    let magnitude = signal.unsigned_abs();
    if *signal < 0 {
        format!("−{magnitude} dBm")
    } else {
        format!("{magnitude} dBm")
    }
}

pub(crate) fn links(count: &u32) -> String {
    if *count == 1 {
        "1 link".to_owned()
    } else {
        format!("{count} links")
    }
}

/// Whether an online bridge's link looks degraded: weak signal or telemetry gone quiet.
pub(crate) fn bridge_degraded(snapshot: &Snapshot) -> bool {
    snapshot.bridge_online
        && snapshot
            .bridge_telemetry
            .as_ref()
            .is_some_and(|telemetry| telemetry_degraded_at(telemetry, Utc::now()))
}

fn telemetry_degraded_at(telemetry: &BridgeTelemetry, now: DateTime<Utc>) -> bool {
    telemetry.signal_dbm < WEAK_SIGNAL_DBM || now - telemetry.at > STALE_TELEMETRY
}

pub(crate) fn opt<T: std::fmt::Display>(value: &Option<T>) -> String {
    value
        .as_ref()
        .map_or_else(|| "—".to_owned(), ToString::to_string)
}

pub(crate) fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn opcode(opcode: &Option<u8>) -> String {
    opcode.map_or_else(|| "··".to_owned(), |opcode| format!("{opcode:02X}"))
}

pub(crate) fn direction(direction: &FrameDirection) -> &'static str {
    match direction {
        FrameDirection::Received => "rx",
        FrameDirection::Sent => "tx",
    }
}

pub(crate) fn role_power(power: &RolePower) -> &'static str {
    match power {
        RolePower::On => "on",
        RolePower::Off => "off",
    }
}

pub(crate) fn transition(from: &Option<RolePower>, to: &Option<RolePower>) -> String {
    match (from, to) {
        (None, None) => String::new(),
        (from, to) => format!(
            "{} → {}",
            from.as_ref().map_or("?", role_power),
            to.as_ref().map_or("?", role_power)
        ),
    }
}

pub(crate) fn bus_power(power: &BusPower) -> &'static str {
    match power {
        BusPower::Unknown => "unknown",
        BusPower::On => "on",
        BusPower::Standby => "standby",
        BusPower::Transition => "transition",
    }
}

/// CEC device type names (UI only).
pub(crate) fn device_type(device_type: &Option<u8>) -> String {
    match *device_type {
        None => "—".to_owned(),
        Some(0) => "tv".to_owned(),
        Some(1) => "recording".to_owned(),
        Some(3) => "tuner".to_owned(),
        Some(4) => "playback".to_owned(),
        Some(5) => "audio".to_owned(),
        Some(6) => "switch".to_owned(),
        Some(7) => "processor".to_owned(),
        Some(other) => format!("type {other}"),
    }
}

/// The most informative Home Assistant attribute for a media entity, as plain text.
pub(crate) fn ha_detail(entity: &HaEntityView) -> String {
    ["source", "media_title", "app_name"]
        .iter()
        .find_map(|key| entity.attributes.get(*key))
        .map_or_else(String::new, |value| match value {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn ago_buckets_and_clamps_future_timestamps() {
        let now = Utc.with_ymd_and_hms(2026, 10, 25, 12, 0, 0).unwrap();
        let at = |seconds| now - chrono::Duration::seconds(seconds);
        assert_eq!(ago_at(at(12), now), "12s ago");
        assert_eq!(ago_at(at(59), now), "59s ago");
        assert_eq!(ago_at(at(60), now), "1m ago");
        assert_eq!(ago_at(at(120), now), "2m ago");
        assert_eq!(ago_at(at(3_599), now), "59m ago");
        assert_eq!(ago_at(at(3_600), now), "1h ago");
        assert_eq!(ago_at(at(86_400 * 3), now), "3d ago");
        assert_eq!(ago_at(at(-5), now), "0s ago");
    }

    #[test]
    fn uptime_keeps_the_two_most_significant_units() {
        assert_eq!(uptime(&0), "0s");
        assert_eq!(uptime(&48), "48s");
        assert_eq!(uptime(&60), "1m");
        assert_eq!(uptime(&(25 * 60 + 59)), "25m");
        assert_eq!(uptime(&(6 * 3600 + 25 * 60 + 7)), "6h 25m");
        assert_eq!(uptime(&3600), "1h 0m");
        assert_eq!(uptime(&282_348), "3d 6h");
    }

    #[test]
    fn telemetry_is_degraded_by_weak_signal_or_age_past_the_thresholds() {
        let now = Utc.with_ymd_and_hms(2026, 10, 30, 12, 0, 0).unwrap();
        let telemetry = |signal_dbm, age_s| BridgeTelemetry {
            signal_dbm,
            link_count: 1,
            uptime_s: 60,
            at: now - chrono::Duration::seconds(age_s),
        };
        assert!(!telemetry_degraded_at(&telemetry(-75, 900), now));
        assert!(telemetry_degraded_at(&telemetry(-76, 0), now));
        assert!(telemetry_degraded_at(&telemetry(-50, 901), now));
    }
}
