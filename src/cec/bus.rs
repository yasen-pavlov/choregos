use super::{
    frame::{Frame, decode_osd},
    opcode::{
        ACTIVE_SOURCE, DEVICE_VENDOR_ID, IMAGE_VIEW_ON, INACTIVE_SOURCE, REPORT_PHYSICAL_ADDRESS,
        REPORT_POWER_STATUS, REQUEST_ACTIVE_SOURCE, SET_OSD_NAME, STANDBY, TEXT_VIEW_ON,
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

    fn inherit_device(&mut self, moved: Self) {
        self.physical = moved.physical;
        self.vendor = moved.vendor;
        self.osd = moved.osd;
        self.device_type = moved.device_type;
        self.power = moved.power;
        self.verified = moved.verified;
        self.confirmed_at = moved.confirmed_at;
        // Scheduling stays LA-local, but inherited keys need fresh replies here.
        self.manual_pending = true;
        self.restore_reconfirmation = false;
        self.vendor_reconfirmed = false;
        self.osd_reconfirmed = false;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IdentityChange {
    None,
    Physical,
    Replace,
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

    pub fn reset_identification(&mut self, now: Instant) -> Vec<u8> {
        let mut unverified = Vec::new();
        for (&la, entry) in &mut self.entries {
            let query_physical = !entry.verified && la < 15 && la != self.bridge;
            entry.last_identify = query_physical.then_some(now);
            entry.attempts = 0;
            entry.identify_since = now;
            if query_physical {
                unverified.push(la);
            }
        }
        self.defer_identification(now);
        unverified
    }

    fn defer_identification(&mut self, now: Instant) {
        self.identify_not_before = Some(now + Duration::from_secs(1));
    }

    pub fn manual_identify(&mut self, la: u8, now: Instant) -> Vec<Action> {
        let entry = self.entries.entry(la).or_insert_with(|| BusEntry::new(now));
        entry.manual_pending = true;
        self.identify(la, now).into_iter().collect()
    }
    pub fn replaces_identity(&self, frame: &Frame, keep_running: bool) -> bool {
        self.identity_change(frame, keep_running) == IdentityChange::Replace
    }

    fn reports_physical(frame: &Frame) -> bool {
        match frame.opcode {
            Some(REPORT_PHYSICAL_ADDRESS) => frame.params.len() >= 3,
            Some(ACTIVE_SOURCE) => frame.params.len() == 2 && frame.to == 15,
            _ => false,
        }
    }

    fn physical_changed(&self, frame: &Frame) -> bool {
        Self::reports_physical(frame)
            && self
                .entries
                .get(&frame.from)
                .is_some_and(|entry| entry.physical.is_some() && entry.physical != frame.physical())
    }

    fn identity_change(&self, frame: &Frame, keep_running: bool) -> IdentityChange {
        let Some(entry) = self.entries.get(&frame.from) else {
            return IdentityChange::None;
        };
        // Address moves take precedence over continuity at the destination LA.
        if self.physical_changed(frame) {
            return if keep_running
                && frame.from != self.bridge
                && entry.verified
                && entry.vendor.is_some()
                && entry.osd.is_some()
                && self.physical_duplicate_from(frame).is_none()
            {
                IdentityChange::Physical
            } else {
                IdentityChange::Replace
            };
        }
        let replaces = match frame.opcode {
            Some(DEVICE_VENDOR_ID) if frame.params.len() == 3 => entry
                .vendor
                .is_some_and(|vendor| vendor != Self::vendor(frame)),
            Some(SET_OSD_NAME) if !frame.params.is_empty() => entry
                .osd
                .as_ref()
                .is_some_and(|osd| *osd != decode_osd(&frame.params)),
            _ => false,
        };
        if replaces {
            IdentityChange::Replace
        } else {
            IdentityChange::None
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
    fn physical_duplicate_from(&self, frame: &Frame) -> Option<u8> {
        if frame.from < 15 && Self::reports_physical(frame) {
            self.entries.iter().find_map(|(&la, entry)| {
                (la != frame.from && entry.physical == frame.physical()).then_some(la)
            })
        } else {
            None
        }
    }

    fn accepts_address_move(&self, frame: &Frame, previous_la: u8) -> bool {
        self.replaces_identity(frame, false)
            || self.entries.get(&frame.from).is_none_or(|destination| {
                let moved = &self.entries[&previous_la];
                (destination.vendor.is_none() || destination.vendor == moved.vendor)
                    && (destination.osd.is_none() || destination.osd == moved.osd)
            })
    }

    pub(crate) fn address_move_from(&self, frame: &Frame) -> Option<u8> {
        self.physical_duplicate_from(frame)
            .filter(|&la| self.accepts_address_move(frame, la))
    }

    fn evict_physical_duplicates(&mut self, frame: &Frame) -> Option<BusEntry> {
        let previous_la = self.physical_duplicate_from(frame)?;
        let inherit = self.accepts_address_move(frame, previous_la);
        self.entries.remove(&previous_la).filter(|_| inherit)
    }

    fn replace_entry(
        entry: &mut BusEntry,
        frame: &Frame,
        now: Instant,
        active_source: &mut Option<u16>,
    ) {
        if *active_source == entry.physical {
            *active_source = None;
        }
        let physical = entry.physical;
        let key_changed = matches!(frame.opcode, Some(DEVICE_VENDOR_ID | SET_OSD_NAME));
        let last_identify = entry.last_identify;
        let attempts = entry.attempts;
        *entry = BusEntry::new(now);
        entry.last_identify = last_identify;
        entry.attempts = attempts;
        if key_changed {
            entry.physical = physical;
            entry.verified = false;
        }
    }

    fn learned_key(entry: &BusEntry, frame: &Frame) -> bool {
        match frame.opcode {
            Some(REPORT_PHYSICAL_ADDRESS) if frame.params.len() >= 3 => entry.physical.is_none(),
            Some(DEVICE_VENDOR_ID) if frame.params.len() == 3 => entry.vendor.is_none(),
            Some(SET_OSD_NAME) if !frame.params.is_empty() => entry.osd.is_none(),
            _ => false,
        }
    }

    fn apply_report(
        entry: &mut BusEntry,
        frame: &Frame,
        wall: DateTime<Utc>,
        change: IdentityChange,
        bridge: u8,
        active_source: &mut Option<u16>,
    ) {
        match frame.opcode {
            Some(REPORT_PHYSICAL_ADDRESS) if frame.params.len() >= 3 => {
                entry.physical = frame.physical();
                entry.device_type = Some(frame.params[2]);
                entry.verified = true;
                entry.confirmed_at = Some(wall);
            }
            Some(DEVICE_VENDOR_ID) if frame.params.len() == 3 => {
                if entry.restore_reconfirmation && entry.vendor == Some(Self::vendor(frame)) {
                    entry.vendor_reconfirmed = true;
                }
                entry.vendor = Some(Self::vendor(frame));
            }
            Some(SET_OSD_NAME) if !frame.params.is_empty() => {
                let osd = decode_osd(&frame.params);
                if entry.restore_reconfirmation && entry.osd.as_ref() == Some(&osd) {
                    entry.osd_reconfirmed = true;
                }
                entry.osd = Some(osd);
            }
            Some(ACTIVE_SOURCE) if frame.params.len() == 2 && frame.to == 15 => {
                if change != IdentityChange::None {
                    entry.physical = frame.physical();
                }
                *active_source = frame.physical();
                entry.power = BusPower::On;
            }
            Some(INACTIVE_SOURCE) if frame.params.len() == 2 && frame.to == bridge => {
                if *active_source == frame.physical() {
                    *active_source = None;
                }
                entry.power = BusPower::Standby;
            }
            Some(STANDBY) if frame.from != bridge && (frame.to == bridge || frame.to == 15) => {
                entry.power = BusPower::Standby;
                if *active_source == entry.physical {
                    *active_source = None;
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
            Some(IMAGE_VIEW_ON | TEXT_VIEW_ON) if frame.to == bridge => {
                entry.power = BusPower::On;
            }
            _ => {}
        }
    }

    fn is_wake_frame(&self, frame: &Frame) -> bool {
        match frame.opcode {
            Some(IMAGE_VIEW_ON | TEXT_VIEW_ON) => frame.to == self.bridge,
            Some(ACTIVE_SOURCE) => frame.to == 15 && frame.params.len() == 2,
            Some(REQUEST_ACTIVE_SOURCE) => frame.to == 15,
            _ => false,
        }
    }

    pub fn handle(
        &mut self,
        frame: &Frame,
        now: Instant,
        wall: DateTime<Utc>,
        keep_running: bool,
    ) -> Vec<Action> {
        if frame.from >= 15 || frame.from == self.bridge {
            return vec![];
        }
        let change = self.identity_change(frame, keep_running);
        let replaces = change == IdentityChange::Replace;
        let wake = self.is_wake_frame(frame);
        let moved = self.evict_physical_duplicates(frame);
        let entry = self
            .entries
            .entry(frame.from)
            .or_insert_with(|| BusEntry::new(now));
        if replaces {
            Self::replace_entry(entry, frame, now, &mut self.active_source);
        }
        if change == IdentityChange::Physical {
            if self.active_source == entry.physical {
                self.active_source = frame.physical();
            }
            entry.manual_pending = true;
        }
        if let Some(moved) = moved {
            entry.inherit_device(moved);
        }
        entry.last_seen = Some(now);
        let learned_key = !replaces && Self::learned_key(entry, frame);
        // Restores retain confirmed_at; a PA retained after replacement does not.
        let restored_pa_confirmed = !replaces
            && !entry.verified
            && entry.confirmed_at.is_some()
            && frame.opcode == Some(REPORT_PHYSICAL_ADDRESS)
            && frame.params.len() >= 3;
        Self::apply_report(
            entry,
            frame,
            wall,
            change,
            self.bridge,
            &mut self.active_source,
        );
        let reconfirmed = !entry.verified && entry.vendor_reconfirmed && entry.osd_reconfirmed;
        if reconfirmed {
            entry.verified = true;
        }
        if entry.verified {
            entry.restore_reconfirmation = false;
        }
        if learned_key || restored_pa_confirmed || reconfirmed {
            entry.attempts = 0;
        }
        entry.identified = Self::missing(self.required, entry).is_empty();
        // Wake announcements get one fresh attempt per base interval, even after replacement.
        if wake
            && !entry.identified
            && entry.last_identify.is_none_or(|last| {
                now.saturating_duration_since(last) >= Self::delay(self.retry, 0)
            })
        {
            entry.attempts = 0;
            entry.last_identify = None;
        }
        self.identify(frame.from, now).into_iter().collect()
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
            bus.handle(&f("84210004"), now, chrono::DateTime::UNIX_EPOCH, false)
                .len(),
            1
        );
        assert!(
            bus.handle(
                &f("04"),
                now + Duration::from_secs(9),
                chrono::DateTime::UNIX_EPOCH,
                false,
            )
            .is_empty()
        );
        assert_eq!(
            bus.handle(
                &f("04"),
                now + Duration::from_secs(10),
                chrono::DateTime::UNIX_EPOCH,
                false,
            )
            .len(),
            1
        );
        bus.handle(
            &f("870010FA"),
            now + Duration::from_secs(11),
            chrono::DateTime::UNIX_EPOCH,
            false,
        );
        assert!(bus.entries[&4].identified);
        bus.handle(
            &f("84230004"),
            now + Duration::from_secs(12),
            chrono::DateTime::UNIX_EPOCH,
            false,
        );
        assert_eq!(bus.entries[&4].vendor, None);
        assert!(!bus.entries[&4].identified);
    }
}
