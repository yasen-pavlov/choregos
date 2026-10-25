use super::{
    frame::{Frame, decode_osd},
    opcode::*,
};
use crate::{action::Action, state::BusPower};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct BusEntry {
    pub physical: Option<u16>,
    pub device_type: Option<u8>,
    pub vendor: Option<u32>,
    pub osd: Option<String>,
    pub power: BusPower,
    pub last_seen: Instant,
    pub identified: bool,
    last_identify: Option<Instant>,
}
impl BusEntry {
    fn new(now: Instant) -> Self {
        Self {
            physical: None,
            device_type: None,
            vendor: None,
            osd: None,
            power: BusPower::Unknown,
            last_seen: now,
            identified: false,
            last_identify: None,
        }
    }
}

pub struct BusModel {
    pub entries: BTreeMap<u8, BusEntry>,
    pub active_source: Option<u16>,
    pub bridge: u8,
    retry: Duration,
}
impl BusModel {
    pub fn new(bridge: u8, identify_retry_s: u64) -> Self {
        Self {
            entries: BTreeMap::new(),
            active_source: None,
            bridge,
            retry: Duration::from_secs(identify_retry_s),
        }
    }
    pub fn set_identify_retry(&mut self, seconds: u64) {
        self.retry = Duration::from_secs(seconds);
    }
    pub fn handle(&mut self, frame: &Frame, now: Instant) -> Vec<Action> {
        if frame.from >= 15 {
            return vec![];
        }
        if frame.opcode == Some(REPORT_PHYSICAL_ADDRESS) && frame.params.len() >= 3 {
            let physical = frame.physical();
            self.entries
                .retain(|&la, entry| la == frame.from || entry.physical != physical);
        }
        let entry = self
            .entries
            .entry(frame.from)
            .or_insert_with(|| BusEntry::new(now));
        if frame.opcode == Some(REPORT_PHYSICAL_ADDRESS)
            && frame.params.len() >= 3
            && entry.physical.is_some()
            && entry.physical != frame.physical()
        {
            if self.active_source == entry.physical {
                self.active_source = None;
            }
            *entry = BusEntry::new(now);
        }
        entry.last_seen = now;
        match frame.opcode {
            Some(REPORT_PHYSICAL_ADDRESS) if frame.params.len() >= 3 => {
                entry.physical = frame.physical();
                entry.device_type = Some(frame.params[2]);
            }
            Some(DEVICE_VENDOR_ID) if frame.params.len() == 3 => {
                entry.vendor = Some(u32::from_be_bytes([
                    0,
                    frame.params[0],
                    frame.params[1],
                    frame.params[2],
                ]));
            }
            Some(SET_OSD_NAME) if !frame.params.is_empty() => {
                entry.osd = Some(decode_osd(&frame.params));
            }
            Some(ACTIVE_SOURCE) if frame.params.len() == 2 && frame.to == 15 => {
                self.active_source = frame.physical();
                entry.power = BusPower::On;
            }
            Some(INACTIVE_SOURCE) if frame.params.len() == 2 && frame.to == self.bridge => {
                if self.active_source == frame.physical() {
                    self.active_source = None;
                }
                entry.power = BusPower::Standby;
            }
            Some(STANDBY)
                if frame.from != self.bridge && (frame.to == self.bridge || frame.to == 15) =>
            {
                entry.power = BusPower::Standby;
                if self.active_source == entry.physical {
                    self.active_source = None;
                }
            }
            Some(REPORT_POWER_STATUS) => {
                entry.power = match frame.params.first() {
                    Some(0) => BusPower::On,
                    Some(1) => BusPower::Standby,
                    Some(2 | 3) => BusPower::Transition,
                    _ => entry.power,
                };
            }
            Some(IMAGE_VIEW_ON | TEXT_VIEW_ON) if frame.to == self.bridge => {
                entry.power = BusPower::On;
            }
            _ => {}
        }
        entry.identified =
            entry.physical.is_some() && (entry.osd.is_some() || entry.vendor.is_some());
        if frame.from != self.bridge
            && !entry.identified
            && entry
                .last_identify
                .is_none_or(|t| now.duration_since(t) >= self.retry)
        {
            entry.last_identify = Some(now);
            vec![Action::Identify { la: frame.from }]
        } else {
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn address_replacement_and_identification_limit() {
        let now = Instant::now();
        let mut bus = BusModel::new(0, 10);
        let f = |s| Frame::from_hex(4, 15, s).unwrap();
        assert_eq!(bus.handle(&f("84210004"), now).len(), 1);
        assert!(
            bus.handle(&f("04"), now + Duration::from_secs(9))
                .is_empty()
        );
        assert_eq!(bus.handle(&f("04"), now + Duration::from_secs(10)).len(), 1);
        bus.handle(&f("870010FA"), now + Duration::from_secs(11));
        assert!(bus.entries[&4].identified);
        bus.handle(&f("84230004"), now + Duration::from_secs(12));
        assert_eq!(bus.entries[&4].vendor, None);
        assert!(!bus.entries[&4].identified);
    }
}
