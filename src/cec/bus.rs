use super::{
    frame::{Frame, decode_osd},
    opcode::{
        ACTIVE_SOURCE, DEVICE_VENDOR_ID, IMAGE_VIEW_ON, INACTIVE_SOURCE, REPORT_PHYSICAL_ADDRESS,
        REPORT_POWER_STATUS, SET_OSD_NAME, STANDBY, TEXT_VIEW_ON,
    },
};
use crate::{
    action::{Action, IdentifyQueries},
    state::BusPower,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct LearnedIdentity {
    pub la: u8,
    pub physical: Option<u16>,
    pub vendor: Option<u32>,
    pub osd: Option<String>,
    pub device_type: Option<u8>,
    pub confirmed_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct BusEntry {
    pub physical: Option<u16>,
    pub device_type: Option<u8>,
    pub vendor: Option<u32>,
    pub osd: Option<String>,
    pub power: BusPower,
    pub last_seen: Option<Instant>,
    pub verified: bool,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub identified: bool,
    last_identify: Option<Instant>,
    attempts: u8,
    identify_since: Instant,
    restore_reconfirmation: bool,
    vendor_reconfirmed: bool,
    osd_reconfirmed: bool,
    manual_pending: bool,
}
impl BusEntry {
    fn new(now: Instant) -> Self {
        Self {
            physical: None,
            device_type: None,
            vendor: None,
            osd: None,
            power: BusPower::Unknown,
            last_seen: None,
            verified: true,
            confirmed_at: None,
            identified: false,
            last_identify: None,
            attempts: 0,
            identify_since: now,
            restore_reconfirmation: false,
            vendor_reconfirmed: false,
            osd_reconfirmed: false,
            manual_pending: false,
        }
    }
}

pub struct BusModel {
    pub entries: BTreeMap<u8, BusEntry>,
    pub active_source: Option<u16>,
    pub bridge: u8,
    retry: Duration,
    required: IdentifyQueries,
    identify_not_before: Option<Instant>,
}
impl BusModel {
    pub fn new(bridge: u8, identify_retry_s: u64, required: IdentifyQueries) -> Self {
        Self {
            entries: BTreeMap::new(),
            active_source: None,
            bridge,
            retry: Duration::from_secs(identify_retry_s),
            required,
            identify_not_before: None,
        }
    }
    pub fn configure_identification(&mut self, seconds: u64, required: IdentifyQueries) {
        self.retry = Duration::from_secs(seconds);
        self.required = required;
        for entry in self.entries.values_mut() {
            entry.identified = Self::missing(required, entry).is_empty();
        }
    }

    pub fn restore(&mut self, identities: Vec<LearnedIdentity>, now: Instant) {
        for identity in identities {
            if identity.la >= 15
                || identity.la == self.bridge
                || identity.physical.is_some_and(|pa| {
                    self.entries
                        .values()
                        .any(|entry| entry.physical == Some(pa))
                })
            {
                continue;
            }
            let mut entry = BusEntry::new(now);
            entry.physical = identity.physical;
            entry.vendor = identity.vendor;
            entry.osd = identity.osd;
            entry.device_type = identity.device_type;
            entry.verified = false;
            entry.confirmed_at = Some(identity.confirmed_at);
            entry.restore_reconfirmation =
                entry.physical.is_some() && entry.vendor.is_some() && entry.osd.is_some();
            entry.identified = Self::missing(self.required, &entry).is_empty();
            self.entries.insert(identity.la, entry);
        }
    }

    fn missing(required: IdentifyQueries, entry: &BusEntry) -> IdentifyQueries {
        IdentifyQueries {
            physical: !entry.verified || entry.physical.is_none(),
            vendor: required.vendor && entry.vendor.is_none(),
            osd: required.osd && entry.osd.is_none(),
        }
    }

    fn delay(retry: Duration, attempts: u8) -> Duration {
        retry
            .saturating_mul(1_u32 << attempts.min(31))
            .min(Duration::from_secs(3600))
    }

    pub fn reset_identification(&mut self, now: Instant) {
        for entry in self.entries.values_mut() {
            entry.last_identify = None;
            entry.attempts = 0;
            entry.identify_since = now;
        }
        self.defer_identification(now);
    }

    fn defer_identification(&mut self, now: Instant) {
        self.identify_not_before = Some(now + Duration::from_secs(1));
    }

    pub fn manual_identify(&mut self, la: u8, now: Instant) -> Vec<Action> {
        let entry = self.entries.entry(la).or_insert_with(|| BusEntry::new(now));
        entry.manual_pending = true;
        self.identify(la, now).into_iter().collect()
    }
    pub fn replaces_identity(&self, frame: &Frame) -> bool {
        let Some(entry) = self.entries.get(&frame.from) else {
            return false;
        };
        match frame.opcode {
            Some(REPORT_PHYSICAL_ADDRESS) if frame.params.len() >= 3 => {
                entry.physical.is_some() && entry.physical != frame.physical()
            }
            Some(ACTIVE_SOURCE) if frame.params.len() == 2 && frame.to == 15 => {
                entry.physical.is_some() && entry.physical != frame.physical()
            }
            Some(DEVICE_VENDOR_ID) if frame.params.len() == 3 => entry
                .vendor
                .is_some_and(|vendor| vendor != Self::vendor(frame)),
            Some(SET_OSD_NAME) if !frame.params.is_empty() => entry
                .osd
                .as_ref()
                .is_some_and(|osd| *osd != decode_osd(&frame.params)),
            _ => false,
        }
    }

    fn vendor(frame: &Frame) -> u32 {
        u32::from_be_bytes([0, frame.params[0], frame.params[1], frame.params[2]])
    }
    fn entry_deadline(retry: Duration, entry: &BusEntry) -> Instant {
        if entry.manual_pending {
            entry.identify_since
        } else {
            entry.last_identify.map_or(entry.identify_since, |last| {
                last + Self::delay(retry, entry.attempts)
            })
        }
    }

    fn identify(&mut self, la: u8, now: Instant) -> Option<Action> {
        if self.identify_not_before.is_some_and(|next| now < next) {
            return None;
        }
        let entry = self.entries.get_mut(&la)?;
        let queries = if entry.manual_pending {
            IdentifyQueries::ALL
        } else {
            Self::missing(self.required, entry)
        };
        if queries.is_empty() || now < Self::entry_deadline(self.retry, entry) {
            return None;
        }
        if entry.manual_pending {
            entry.manual_pending = false;
        } else if entry.last_identify.is_some() {
            entry.attempts = entry.attempts.saturating_add(1);
        }
        entry.last_identify = Some(now);
        self.defer_identification(now);
        Some(Action::Identify { la, queries })
    }

    pub fn retry_identification(&mut self, now: Instant) -> Vec<Action> {
        let next = self
            .entries
            .iter()
            .filter(|(la, entry)| {
                **la < 15 && **la != self.bridge && (!entry.identified || entry.manual_pending)
            })
            .min_by_key(|(_, entry)| Self::entry_deadline(self.retry, entry))
            .map(|(&la, _)| la);
        next.and_then(|la| self.identify(la, now))
            .into_iter()
            .collect()
    }

    pub fn next_identify_deadline(&self) -> Option<Instant> {
        self.entries
            .iter()
            .filter(|(la, entry)| {
                **la < 15 && **la != self.bridge && (!entry.identified || entry.manual_pending)
            })
            .map(|(_, entry)| {
                let due = Self::entry_deadline(self.retry, entry);
                self.identify_not_before.map_or(due, |gate| due.max(gate))
            })
            .min()
    }
    pub fn handle(&mut self, frame: &Frame, now: Instant, wall: DateTime<Utc>) -> Vec<Action> {
        if frame.from >= 15 {
            return vec![];
        }
        let replaces = self.replaces_identity(frame);
        if (frame.opcode == Some(REPORT_PHYSICAL_ADDRESS) && frame.params.len() >= 3)
            || (frame.opcode == Some(ACTIVE_SOURCE) && frame.params.len() == 2 && frame.to == 15)
        {
            let physical = frame.physical();
            self.entries
                .retain(|&la, entry| la == frame.from || entry.physical != physical);
        }
        let entry = self
            .entries
            .entry(frame.from)
            .or_insert_with(|| BusEntry::new(now));
        if replaces {
            if self.active_source == entry.physical {
                self.active_source = None;
            }
            let physical = entry.physical;
            let key_changed = matches!(frame.opcode, Some(DEVICE_VENDOR_ID | SET_OSD_NAME));
            *entry = BusEntry::new(now);
            if key_changed {
                entry.physical = physical;
                entry.verified = false;
            }
        }
        entry.last_seen = Some(now);
        let learned_key = match frame.opcode {
            Some(REPORT_PHYSICAL_ADDRESS) if frame.params.len() >= 3 => {
                entry.physical.is_none() || !entry.verified
            }
            Some(DEVICE_VENDOR_ID) if frame.params.len() == 3 => entry.vendor.is_none(),
            Some(SET_OSD_NAME) if !frame.params.is_empty() => entry.osd.is_none(),
            _ => false,
        };
        match frame.opcode {
            Some(REPORT_PHYSICAL_ADDRESS) if frame.params.len() >= 3 => {
                entry.physical = frame.physical();
                entry.device_type = Some(frame.params[2]);
                entry.verified = true;
                entry.confirmed_at = Some(wall);
            }
            Some(DEVICE_VENDOR_ID) if frame.params.len() == 3 => {
                if entry.restore_reconfirmation
                    && entry.vendor == Some(Self::vendor(frame))
                    && (frame.to == self.bridge || frame.to == 15)
                {
                    entry.vendor_reconfirmed = true;
                }
                entry.vendor = Some(Self::vendor(frame));
            }
            Some(SET_OSD_NAME) if !frame.params.is_empty() => {
                let osd = decode_osd(&frame.params);
                if entry.restore_reconfirmation
                    && entry.osd.as_ref() == Some(&osd)
                    && frame.to == self.bridge
                {
                    entry.osd_reconfirmed = true;
                }
                entry.osd = Some(osd);
            }
            Some(ACTIVE_SOURCE) if frame.params.len() == 2 && frame.to == 15 => {
                if replaces {
                    entry.physical = frame.physical();
                }
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
        let reconfirmed = !entry.verified && entry.vendor_reconfirmed && entry.osd_reconfirmed;
        if reconfirmed {
            entry.verified = true;
        }
        if entry.verified {
            entry.restore_reconfirmation = false;
        }
        if learned_key || reconfirmed {
            entry.attempts = 0;
        }
        entry.identified = Self::missing(self.required, entry).is_empty();
        if frame.from != self.bridge {
            self.identify(frame.from, now).into_iter().collect()
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
        let mut bus = BusModel::new(
            0,
            10,
            IdentifyQueries {
                physical: true,
                vendor: true,
                osd: false,
            },
        );
        let f = |s| Frame::from_hex(4, 15, s).unwrap();
        assert_eq!(
            bus.handle(&f("84210004"), now, chrono::DateTime::UNIX_EPOCH)
                .len(),
            1
        );
        assert!(
            bus.handle(
                &f("04"),
                now + Duration::from_secs(9),
                chrono::DateTime::UNIX_EPOCH
            )
            .is_empty()
        );
        assert_eq!(
            bus.handle(
                &f("04"),
                now + Duration::from_secs(10),
                chrono::DateTime::UNIX_EPOCH
            )
            .len(),
            1
        );
        bus.handle(
            &f("870010FA"),
            now + Duration::from_secs(11),
            chrono::DateTime::UNIX_EPOCH,
        );
        assert!(bus.entries[&4].identified);
        bus.handle(
            &f("84230004"),
            now + Duration::from_secs(12),
            chrono::DateTime::UNIX_EPOCH,
        );
        assert_eq!(bus.entries[&4].vendor, None);
        assert!(!bus.entries[&4].identified);
    }
}
