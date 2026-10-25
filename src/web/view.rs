//! Presentation helpers shared by templates: layout chrome and value formatting.

use chrono::{DateTime, Utc};

use super::auth::{Ctx, SessionUser};
use crate::state::{BusPower, FrameDirection, HaEntityView, Health, Mode, RolePower, Snapshot};

/// Chrome rendered by `base.html` around every authenticated page.
pub(crate) struct Layout {
    pub(crate) page: &'static str,
    pub(crate) csrf: String,
    pub(crate) user: SessionUser,
    pub(crate) mode: Mode,
    pub(crate) warming: bool,
    pub(crate) health: Health,
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
    timestamp.map_or_else(|| "never".to_owned(), |ts| ago_at(ts, Utc::now()))
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

pub(crate) fn opt<T: std::fmt::Display>(value: &Option<T>) -> String {
    value
        .as_ref()
        .map_or_else(|| "—".to_owned(), |value| value.to_string())
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
        assert_eq!(ago_at(at(59), now), "59s ago");
        assert_eq!(ago_at(at(60), now), "1m ago");
        assert_eq!(ago_at(at(3_599), now), "59m ago");
        assert_eq!(ago_at(at(3_600), now), "1h ago");
        assert_eq!(ago_at(at(86_400 * 3), now), "3d ago");
        assert_eq!(ago_at(at(-5), now), "0s ago");
    }
}
